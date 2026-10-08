//! Reaching the holder: whether a lease row can be proxied to at all
//! ([`proxyable`]), the private-directory check on the dial side, the
//! bounded connect, and the shutdown-raced dial that re-reads the row and
//! replays the handshake on every reconnect ([`HubProxy::dial_bounded`]).

use tokio::io::BufReader;

use super::handshake::Handshake;
use super::HubProxy;
use crate::mcp::endpoint::SessionEndpoint;
use crate::store::lease::LeaseInfo;
use crate::types::LamboError;

/// How long a connect to the holder is retried, and how long the handshake
/// replay waits for its answer, before the endpoint is treated as dead.
///
/// Covers the holder's own acquire→bind window (the endpoint's address is
/// published with the lease, the socket is bound microseconds later), plus a
/// generous margin for a loaded machine. Short, because the *other* wait — for
/// a dead holder's lease to lapse — is bounded by the TTL and handled by the
/// caller, not here.
///
/// [`Handshake::replay`] reuses it rather than defining a second budget: both
/// are "this holder is not answering the door", and a connection that lands in
/// the backlog of a stopped accept loop is indistinguishable from a slow one
/// until the deadline says otherwise (J2-R1-8).
///
/// **Neither of them bounds the arm body, and this docstring used to say they
/// did** — "the two together bound how long the pump can be deaf to SIGTERM
/// inside one `client_rx` arm body at 2 × `CONNECT_BUDGET`" (J2-R2-1). That
/// sentence was true about `connect` and `replay` and false about the arm body,
/// which also contains the store read that *starts* [`HubProxy::dial`]. The bound
/// that is true, and the reasoning behind the number, live at [`DIAL_BUDGET`];
/// read that constant, not this one, for what a SIGTERM costs.
///
/// One smaller number corrected in the same pass: `connect` below tests its
/// deadline only *after* a failed attempt, so its own worst case is
/// `CONNECT_BUDGET + CONNECT_RETRY` plus one attempt — about 2.1s, not 2.0s.
pub(crate) const CONNECT_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// Interval between connect attempts inside [`CONNECT_BUDGET`].
pub(super) const CONNECT_RETRY: std::time::Duration = std::time::Duration::from_millis(100);

/// The chosen wall-clock bound on one whole dial — the lease-row read, the
/// connect and the handshake replay together (J2-R2-1).
///
/// # Why a third budget, when connect and replay already have one
///
/// [`HubProxy::dial`] **begins** with `store.read_lease`, and nothing in this
/// module bounded it. What bounded it was whatever the store adapter happens to
/// be tuned for, which is not a number anybody chose for a proxy's shutdown
/// latency:
///
/// * **sqlite** — `busy_timeout` 8s inside the statement (set in
///   `SqliteStore::connect`, pinned by `file_backed_wal_and_busy_timeout_applied`)
///   on a pool of `max_connections(1)`, so a concurrent flush holding that one
///   connection makes the read wait at the *pool* first — and neither adapter
///   overrides sqlx's default `acquire_timeout`, which is **30s** (sqlx 0.8.6,
///   `PoolOptions::default`). Worst case ≈ 38s.
/// * **cockroach** — `statement_timeout` 20s per statement
///   (`store::pg::pool::STATEMENT_TIMEOUT`), behind the same 30s pool acquire, which
///   for a lazily-created pool includes the TCP connect and the auth handshake.
///   Worst case ≈ 50s.
///
/// So the real pre-J2-R2 bound on arm-body SIGTERM deafness was not four seconds
/// but tens of seconds, chosen by a store's *flush* tuning. That is the same
/// defect class three I rounds were spent closing, and it is not something a
/// docstring can fix by describing it more precisely — hence a code change here
/// rather than the doc-precision the review prescribed.
///
/// # What is chosen instead — two answers, two questions
///
/// * **Deafness** is answered by *racing*, not by a budget. The dial is polled
///   against the shutdown future ([`HubProxy::dial_bounded`]), so a SIGTERM
///   arriving mid-dial is honoured at the next poll whatever the store is doing.
///   There is no store timeout left in the deafness path for a constant to
///   under-state.
/// * **The client's wait** is what this constant bounds. A proxy's client is
///   blocked on the very call that triggered the dial, and a store wedged at the
///   pool must not turn that into a 38-second silence: past `DIAL_BUDGET` the
///   dial is abandoned and the call is answered with
///   [`HUB_UNREACHABLE_MESSAGE`](super::disconnect::HUB_UNREACHABLE_MESSAGE) — "nothing happened, retry later", which is
///   exactly what AGENTS.md's "never block on memory" asks for.
///
/// **6s, chosen from both directions.** It sits *above* the budgets the dial's
/// own steps already carry — `CONNECT_BUDGET + CONNECT_RETRY` for the connect and
/// `CONNECT_BUDGET` for the replay, ≈4.1s, asserted below — so a
/// healthy-but-slow holder is never cut off by the outer cap and each inner step
/// still raises its own, better-attributed error. And it sits *below* the
/// smallest store-emergent bound listed above (sqlite's 8s `busy_timeout`), so
/// the number an operator reads here is the number that actually governs. A
/// store contended for longer than that costs one honest error and a retry on
/// the next call, because the row is re-read on every dial.
///
/// # What is still unbounded in the arm body, stated
///
/// The frame writes that share it — six `Self::send` sites writing to the
/// holder or to the client's stdout (J2-R3-3: six, not the two first counted) —
/// are neither raced nor budgeted, and that is the J2-R1-8 deviation's real
/// argument rather than an oversight: a write abandoned mid-frame delivers a
/// torn JSON line, which this pipe may never do (see [`Framed::Torn`](super::framing::Framed::Torn)). Each is
/// bounded by its peer draining the socket. That is a *different shape* from
/// the store read this constant replaced — a peer that never reads is itself
/// already wedged, whereas a row read stuck behind a flush at the pool wedged a
/// **healthy** proxy talking to a **healthy** holder. One of the six is
/// `answer_lost`, whose burst length is the `inflight` list whose cap J2-R2-7
/// declined — the two residuals are coupled; J3's receipt ceiling bounds both.
/// Carried as a named residual in §J2 rather than fixed here, because abandoning
/// a client-facing write is a behaviour decision of its own.
pub(super) const DIAL_BUDGET: std::time::Duration = std::time::Duration::from_secs(6);

/// Build-time invariant: [`DIAL_BUDGET`] must not undercut the budgets of the
/// steps it wraps. If it did, the outer cap rather than the inner step would
/// decide a slow holder's fate, and the inner step is the one that knows whether
/// the holder failed to answer the door or failed to answer the handshake.
const _: () = assert!(
    DIAL_BUDGET.as_millis() > 2 * CONNECT_BUDGET.as_millis() + CONNECT_RETRY.as_millis(),
    "DIAL_BUDGET must exceed the connect budget (CONNECT_BUDGET + CONNECT_RETRY) plus the \
     replay budget (CONNECT_BUDGET) it wraps, or the outer cap decides instead of the inner \
     step — and the inner step is the one with the accurate error message."
);

/// Why a refused serve cannot proxy to the holder it lost to.
///
/// Each variant is a *refusal to guess*. A proxy that dialled anyway would at
/// best fail obscurely and at worst forward writes into the wrong graph.
#[derive(Debug, PartialEq, Eq)]
pub enum NotProxyable {
    /// The holder published no endpoint. Either a pre-J2 row, or — far more
    /// often — the holder is not a `serve` at all but a CLI writer holding the
    /// lease for the length of one verb. Nothing is listening; wait it out.
    HolderPublishedNoEndpoint,
    /// The holder is on another machine. `session_leases.endpoint` is a path on
    /// the *holder's* filesystem and `session_leases.holder` carries the host,
    /// so a same-shaped path here would be a different socket — or nothing.
    HolderIsOnAnotherHost { holder: String },
    /// The row's endpoint does not carry the address *identity* this build
    /// derives for this session and store — the hashed file name differs, so the
    /// holder is answering for a different `(session, store)` pair or is running
    /// a different endpoint scheme altogether. This process cannot know that the
    /// socket it would dial serves the graph it means.
    ///
    /// The **directory** differing is *not* this case (J2-L1) — see
    /// [`proxyable`]. A published path that is not **absolute** is (J2-R2-6): the
    /// column holds a path on the holder's filesystem, and a bare or
    /// `./`-relative spelling names nothing there.
    EndpointIsNotOurs { published: String },
}

impl NotProxyable {
    /// The operator-facing explanation, appended to the lease refusal.
    ///
    /// Operator-facing rather than model-facing: this is a startup failure on
    /// stderr, not a tool result, so it may name hosts and paths — the model
    /// never sees it.
    pub fn explain(&self) -> String {
        match self {
            Self::HolderPublishedNoEndpoint => "That holder published no endpoint, so there is \
                 nothing to forward tool calls to — it is not a 'lambo serve' but a writer \
                 holding the lease for the length of one command (a CLI verb), or a process from \
                 a lambo older than the endpoint column. Retry once it finishes."
                .to_string(),
            Self::HolderIsOnAnotherHost { holder } => format!(
                "That holder is on another host ({holder}), and its endpoint is a socket path on \
                 that machine's filesystem, not this one's. A local proxy cannot reach it; use \
                 --transport http against the holder, or run this session's writer here."
            ),
            Self::EndpointIsNotOurs { published } => format!(
                "That holder published the endpoint {published}, whose name does not carry the \
                 address identity this build derives for this session and store. Only the \
                 directory may differ between two clients; the name is a hash of the session \
                 and the store, so a different name means a different session, a different \
                 store, or a lambo whose endpoint scheme is not this one — and forwarding \
                 there could reach a socket serving a different graph. Refusing to guess. Run \
                 both processes from the same lambo build against the same store, or stop the \
                 other holder."
            ),
        }
    }
}

/// Decide whether the holder named by a lease row can be proxied to, and return
/// **the path to dial**.
///
/// Pure: three checks, no I/O, so it is unit-testable and so the *reason* a
/// refusal happened is a value rather than a log line.
///
/// # Why the directory may differ but the name may not (J2-L1)
///
/// This used to require the published endpoint to equal this process's own
/// derivation, byte for byte. The live two-client probe showed that is too
/// strict in the one configuration J2 exists for: `cursor-agent` scrubs `TMPDIR`
/// from the environment of the MCP server it spawns and `opencode` passes
/// macOS's per-user `TMPDIR` through, so the two products' serves derived two
/// **directories** for one session on one store. The loser refused to forward,
/// waited out its election budget, and the client reported no tools —
/// cross-client memory silently absent on unmodified default wiring.
///
/// The address's **file name** is what carries identity: a cosmetic session
/// prefix plus 16 hex of FNV-1a over the session id and the *canonicalized*
/// store identity (J2-R1-2 is what makes that half trustworthy — before it, the
/// hash covered a store's spelling). So a matching name means the same session
/// on the same store, and the directory only decides *reachability*. Trusting
/// the published directory is therefore benign by construction, while a
/// differing name is the real different-graph case and is still refused.
///
/// **The trust boundary is unchanged: it is the store.** The published path is
/// store data, so a writer who could forge it could already write graph content
/// the model reads, which is strictly more power. The one thing added on top is
/// symmetry — `HubProxy::dial` runs the published directory through the same
/// private-directory check `bind` runs, so a directory this process would refuse
/// to place a socket in is one it refuses to reach a socket in.
pub fn proxyable(
    row: &LeaseInfo,
    ours: &SessionEndpoint,
    our_host: &str,
) -> Result<std::path::PathBuf, NotProxyable> {
    let Some(published) = row.endpoint.as_deref() else {
        return Err(NotProxyable::HolderPublishedNoEndpoint);
    };
    // `holder` is `agent@host#pid` (see `LeaseHolder::token`). The host is what
    // makes the path meaningful, so it is checked before the path.
    if !holder_is_on_host(&row.holder, our_host) {
        return Err(NotProxyable::HolderIsOnAnotherHost {
            holder: row.holder.clone(),
        });
    }
    let published = std::path::Path::new(published);
    // J2-R2-6. Only the **name** is compared below, so a row publishing the bare
    // `sess-<hash>.sock`, or `./sess-<hash>.sock`, matched and was returned as a
    // path to dial. `dial_dir` then took `address.parent()` — `Some("")` for a
    // bare name, which `assert_private_dir` reported as "endpoint directory
    // could not be inspected", with an empty path, in an operator-facing
    // message; and `.` for the relative spelling, which is *this* process's cwd
    // and could pass the private-directory check on its own merits. Neither is a
    // reachable endpoint on the holder's filesystem, which is what the column
    // means, so both are the same refusal as a name that does not match.
    //
    // The trust boundary is still the store (see this function's doc): a writer
    // who can forge the row can already write graph content. This is the
    // directory check not being handed a relative path, not a new defence.
    if !published.is_absolute() {
        return Err(NotProxyable::EndpointIsNotOurs {
            published: published.display().to_string(),
        });
    }
    // The name, not the whole path. An empty or directory-only published value
    // has no name and cannot match.
    let ours_name = ours.path().file_name();
    if published.file_name().is_none() || published.file_name() != ours_name {
        return Err(NotProxyable::EndpointIsNotOurs {
            published: published.display().to_string(),
        });
    }
    Ok(published.to_path_buf())
}

/// Does a `agent@host#pid` holder token name this host?
///
/// The agent id is caller-chosen and untrimmed (J1), so it can itself contain
/// `@` and `#`. The host is the segment between the **last** `@` and the last
/// `#`, which is exactly how `LeaseHolder::token` composes it.
pub(super) fn holder_is_on_host(holder: &str, our_host: &str) -> bool {
    let Some(after_at) = holder.rsplit_once('@').map(|(_, rest)| rest) else {
        return false;
    };
    let host = match after_at.rsplit_once('#') {
        Some((host, _pid)) => host,
        None => after_at,
    };
    host == our_host
}

/// The two halves of a hub connection, already split and already replayed into.
pub(super) type HubHalves = (
    BufReader<tokio::net::unix::OwnedReadHalf>,
    tokio::net::unix::OwnedWriteHalf,
);

/// What one bounded, shutdown-raced dial produced (J2-R2-1).
///
/// Three outcomes rather than a `Result`, because the third one is not a failure
/// and must not be reported as one: a dial cut short by the shutdown signal means
/// *stop*, not *answer this call honestly and carry on*. Collapsing it into the
/// error arm would have the pump log "cannot reach a session holder" on a clean
/// SIGTERM and then try the next frame.
pub(super) enum Dialled {
    /// A live connection, already replayed into; whatever the holder emitted
    /// before answering the replayed `initialize` (J2-R1-12); and **the address
    /// that was actually dialled**, which under J2-L1 need not be the one this
    /// process derives (J2-R2-4).
    Hub(HubHalves, Vec<String>, std::path::PathBuf),
    /// No connection. Carries the operator-facing reason; the caller answers its
    /// own client with [`HUB_UNREACHABLE_MESSAGE`](super::disconnect::HUB_UNREACHABLE_MESSAGE).
    Failed(LamboError),
    /// The shutdown future completed while the dial was in flight. The caller
    /// must stop.
    ShutdownRequested,
}

/// Refuse to dial into a directory this process would refuse to bind in.
///
/// The symmetric half of J2-L1: a proxy may now dial the directory the *holder*
/// published rather than only its own derivation, so the private-directory check
/// has to run on the dial side too. Same three checks, same messages, one
/// action word apart.
pub(crate) fn dial_dir(address: &std::path::Path) -> Result<(), LamboError> {
    let dir = address.parent().ok_or_else(|| {
        LamboError::Conflict(format!(
            "the holder published the endpoint {}, which has no parent directory",
            address.display()
        ))
    })?;
    crate::mcp::endpoint::assert_private_dir(dir, "forward to the session holder")
}

/// Connect to the holder's endpoint, retrying inside [`CONNECT_BUDGET`].
///
/// The retry exists for the holder's own acquire→bind window: the endpoint's
/// address is published by the acquire and the socket is bound a moment later,
/// so a proxy that raced in between would otherwise see `ECONNREFUSED` on a
/// perfectly healthy session.
pub(crate) async fn connect(
    path: &std::path::Path,
) -> Result<tokio::net::UnixStream, std::io::Error> {
    let deadline = tokio::time::Instant::now() + CONNECT_BUDGET;
    loop {
        match tokio::net::UnixStream::connect(path).await {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(e);
                }
                tokio::time::sleep(CONNECT_RETRY).await;
            }
        }
    }
}

impl HubProxy {
    /// Re-read the lease row and reconnect to whoever holds it **now**.
    ///
    /// # Why the row is re-read, and why it is NOT about the address (J2-R1-6)
    ///
    /// This used to say the address is never cached "because a new holder is a
    /// new endpoint". That is false, and the false half was load-bearing: the
    /// endpoint is a **pure function** of `(session, store identity)`
    /// ([`SessionEndpoint::resolve`]), so *every* holder of a given session on a
    /// given store binds the **same** path — which is precisely why `bind` needs
    /// a stale-socket branch at all. Caching the address would cost nothing.
    ///
    /// What changes between holders is the **row**: whether there is a holder,
    /// which host it is on, and whether it published an endpoint at all. So the
    /// re-read is about *liveness and honest errors*, and the two outcomes it
    /// buys are the ones a cached address could never produce:
    ///
    /// * the row is **gone** (a clean release, or an expired lease swept away) —
    ///   the honest answer is "there is no holder", and a cached address would
    ///   instead dial a dead socket and report a connect error, or worse dial a
    ///   *live* socket belonging to a process that no longer holds the session;
    /// * the row names a holder this process must **refuse** to forward to — a
    ///   CLI verb with no endpoint, another host, a different endpoint scheme —
    ///   each of which [`proxyable`] turns into a reason rather than a guess.
    ///
    /// The recovery property is real and is still what the integration test
    /// pins; it just does not come from the address moving. It comes from the
    /// row naming a holder that is *alive*.
    ///
    /// **This function reads the lease and must never acquire it.** See
    /// [`HubProxy::run`].
    pub(super) async fn reconnect(
        &self,
    ) -> Result<(tokio::net::UnixStream, std::path::PathBuf), LamboError> {
        self.dial().await
    }

    /// [`HubProxy::reconnect`], then rebuild the client's MCP session on the new
    /// connection ([`Handshake`]).
    ///
    /// Also returns any frames the holder emitted *before* answering the
    /// replayed `initialize` — legitimate traffic that the caller must forward
    /// to its client rather than swallow (J2-R1-12).
    pub(super) async fn reconnect_and_replay(
        &self,
        handshake: &Handshake,
    ) -> Result<(HubHalves, Vec<String>, std::path::PathBuf), LamboError> {
        let (stream, dialled) = self.reconnect().await?;
        let (read, mut write) = stream.into_split();
        let mut read = BufReader::new(read);
        let before = handshake.replay(&mut read, &mut write).await.map_err(|e| {
            LamboError::Conflict(format!("holder rejected the session handshake: {e}"))
        })?;
        Ok(((read, write), before, dialled))
    }

    /// [`HubProxy::reconnect_and_replay`], raced against the shutdown future and
    /// capped at [`DIAL_BUDGET`] (J2-R2-1).
    ///
    /// This is the whole of the J2-R2-1 fix, and it is deliberately the *only*
    /// await in a `client_rx` arm body that is raced. The dial is the one arm-body
    /// await that can be abandoned at any instant without consequence: the
    /// connection it is building belongs to nobody yet, so dropping it mid-replay
    /// leaves a torn `initialize` in a socket that is closed in the same
    /// statement — the holder's per-connection reader sees a bad frame and an EOF
    /// on a connection it never served, and no graph state is involved. The frame
    /// writes in the same arm body do not have that property (a torn frame
    /// reaches a *real* peer mid-conversation), which is why they stay
    /// un-raced — see [`DIAL_BUDGET`]'s last section.
    ///
    /// `biased`, shutdown first, for the same reason the pump's own `select!` is
    /// (J2-R1-21): a store that answers instantly on every poll must not be able
    /// to starve the signal.
    pub(super) async fn dial_bounded(
        &self,
        handshake: &Handshake,
        shutdown: std::pin::Pin<&mut impl std::future::Future<Output = ()>>,
    ) -> Dialled {
        tokio::select! {
            biased;
            () = shutdown => Dialled::ShutdownRequested,
            outcome = tokio::time::timeout(DIAL_BUDGET, self.reconnect_and_replay(handshake)) => {
                match outcome {
                    Ok(Ok((halves, before, dialled))) => Dialled::Hub(halves, before, dialled),
                    Ok(Err(e)) => Dialled::Failed(e),
                    // The unbudgeted term was the lease-row read; name it, because
                    // "the store did not answer" and "the holder did not answer"
                    // send an operator to different places.
                    Err(_) => Dialled::Failed(LamboError::Conflict(format!(
                        "no connection to the session holder within {}s — the lease row read, \
                         the connect and the handshake replay together did not finish inside \
                         the dial budget (a store blocked at its connection pool looks like \
                         this)",
                        DIAL_BUDGET.as_secs()
                    ))),
                }
            }
        }
    }

    /// Read the row, check it is ours to forward to, and connect.
    ///
    /// **The row read is the term no budget in this module covers**, and that is
    /// the whole of J2-R2-1: `read_lease` is bounded only by the store adapter's
    /// own tuning (sqlite's `busy_timeout` and cockroach's `statement_timeout`,
    /// both behind sqlx's 30s default pool acquire — the arithmetic is at
    /// [`DIAL_BUDGET`]), which is a number chosen for a *flush* and inherited
    /// here by accident. Callers therefore reach this function through
    /// [`HubProxy::dial_bounded`], never directly, so that the store's number is
    /// never the one that decides how long this process ignores a SIGTERM.
    pub(super) async fn dial(
        &self,
    ) -> Result<(tokio::net::UnixStream, std::path::PathBuf), LamboError> {
        let row = self
            .store
            .read_lease(&self.session)
            .await
            .map_err(LamboError::Store)?
            // A released row (#23 review H2: a release keeps the row and its
            // fencing token) names no holder, exactly like no row at all.
            .filter(|row| !crate::store::lease::is_released_holder(&row.holder))
            .ok_or_else(|| {
                LamboError::Conflict(format!(
                    "session {} has no lease holder to forward to",
                    self.session
                ))
            })?;
        let address = proxyable(&row, &self.endpoint, &self.our_host)
            .map_err(|why| LamboError::Conflict(why.explain()))?;
        dial_dir(&address)?;
        if address != self.endpoint.path() {
            // J2-L1. Logged at INFO, not WARN: it is the expected shape when two
            // client products pass different environment to their serve, and the
            // name matching is what makes it benign. An operator asking "why is
            // the socket not where I expected" needs to see it.
            tracing::info!(
                published = %address.display(),
                derived = %self.endpoint.path().display(),
                "lambo serve: the holder's endpoint directory differs from this process's — \
                 forwarding to the published path, because the address name (a hash of the \
                 session and the store) matches, so this is the same session on the same store \
                 reached through a different environment"
            );
        }
        let stream = connect(&address)
            .await
            .map_err(|e| LamboError::Conflict(format!("holder endpoint not reachable: {e}")))?;
        // The address goes back with the stream so the pump's log lines can name
        // what was dialled rather than what was derived (J2-R2-4).
        Ok((stream, address))
    }
}
