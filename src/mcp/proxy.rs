//! The proxy half of J2 — what a `lambo serve` does when it loses the lease.
//!
//! Spec §2.2 admits one writer per session, and before J2 the losers exited 1.
//! On a machine running two agent clients — each spawning its own `lambo serve`
//! per the documented stdio wiring — that turned a correct process-level lock
//! into an agent-level outage, in one client's case with no error reaching the
//! agent at all. **Agents never clash; serve processes do.**
//!
//! So a refused serve stays alive and forwards. The lease is untouched: no
//! weakening, no preemption, no token change. A proxy takes no lease and
//! presents no token; every durable write still happens inside the holder, under
//! the holder's token. **The proxy moves the call, not the write.**
//!
//! # It is a byte pipe, and that is the design
//!
//! The proxy does not implement the seven tools. It copies newline-delimited
//! JSON-RPC frames between its own client's stdio and the holder's session
//! endpoint, in both directions, without deserializing them. rmcp's stdio and
//! `AsyncRead + AsyncWrite` transports speak the same line-framed wire, so the
//! two ends are already compatible.
//!
//! Four consequences, each of which is why this shape was chosen over a
//! tool-level forwarder with a `LamboServer` backend enum:
//!
//! * **The caller's per-call `agent_id` crosses verbatim.** It is never parsed
//!   and never re-serialized, so J1's contract — the id is taken *untrimmed*,
//!   because normalising would silently merge two callers' locks — cannot be
//!   violated in transit. A forwarder that rebuilt the arguments would be
//!   exactly the place that regression would appear.
//! * **The tool surface cannot drift.** Schemas, descriptions, the server
//!   instructions and the protocol-version negotiation all come from the real
//!   holder. There is no second copy to keep in step.
//! * **Everything else forwards for free** — notifications, `ping`,
//!   cancellation, progress — because nothing is enumerated.
//! * **It is genuinely cheap.** No `store::load` replay, no in-RAM graph, no
//!   embedder. N clients cost one graph instead of N. The hop measured 0.31 to
//!   0.48 ms on the dogfood rig, under 1% of any call that embeds.
//!
//! What it costs is the ability to *promote* itself: the MCP session state lives
//! in the holder, so when a holder dies its clients' sessions die with it and
//! this process cannot take over serving a client that has already handshaken
//! elsewhere. See [`HubProxy::run`] for the invariant that follows from that,
//! and §J2 for the extension that would lift it.
//!
//! # stdio only, deliberately
//!
//! A refused `--transport http` serve still exits 1, exactly as before. Its
//! client-facing wire would be streamable HTTP, which is not line-framed, so the
//! pipe does not apply — and the outage J2 exists to fix is the stdio one, where
//! the client spawns the process itself and never chose a port. `lambo serve
//! --transport http` therefore keeps working exactly as-is.

use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::mcp::endpoint::SessionEndpoint;
use crate::store::lease::LeaseInfo;
use crate::types::LamboError;

/// JSON-RPC error code returned for a call the proxy could not forward.
///
/// In the implementation-defined `-32000..=-32099` server range, deliberately
/// not one of the reserved codes: this is not a bad request, a missing method or
/// an internal fault in the holder — it is *this process* being unable to reach
/// the holder at all, which is a distinct thing for a client to log.
const HUB_UNREACHABLE_CODE: i64 = -32001;

/// What a caller is told when the holder cannot be reached.
///
/// Written for the **model**, which is who reads a tool error, and under the
/// same N4 discipline as `mcp::server`'s `tool_err`: no socket path, no store
/// URL, no raw connect error, no internal lease state. Three things a calling
/// agent can act on — nothing happened, memory returns by itself, and it is
/// safe to carry on without memory (AGENTS.md's own rule is never to block on
/// memory).
const HUB_UNREACHABLE_MESSAGE: &str = "lambo: this client reaches memory through the process \
     that holds this session, and that process is not responding. NOTHING WAS READ OR WRITTEN. \
     Memory recovers on its own once a lambo serve holds the session again — the previous \
     holder's lease lapses within 45 seconds — so retry later. Do not block on memory: carry \
     on with the work and record it when memory answers again.";

/// How long the initial connect to the holder is retried before the endpoint is
/// treated as dead.
///
/// Covers the holder's own acquire→bind window (the endpoint's address is
/// published with the lease, the socket is bound microseconds later), plus a
/// generous margin for a loaded machine. Short, because the *other* wait — for
/// a dead holder's lease to lapse — is bounded by the TTL and handled by the
/// caller, not here.
pub(crate) const CONNECT_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// Interval between connect attempts inside [`CONNECT_BUDGET`].
const CONNECT_RETRY: std::time::Duration = std::time::Duration::from_millis(100);

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
    /// The row's endpoint is not the one this build derives for this session and
    /// store. The holder is running a different endpoint scheme (a different
    /// lambo version, a different `XDG_RUNTIME_DIR`), so this process cannot
    /// know that the socket it would dial serves the graph it means.
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
                "That holder published the endpoint {published}, which is not the address this \
                 build derives for this session and store. It is running a different endpoint \
                 scheme — a different lambo version, or a different XDG_RUNTIME_DIR/TMPDIR — so \
                 forwarding there could reach a socket serving a different graph. Refusing to \
                 guess. Align the two processes' environments, or stop the other holder."
            ),
        }
    }
}

/// Decide whether the holder named by a lease row can be proxied to.
///
/// Pure: three checks, no I/O, so it is unit-testable and so the *reason* a
/// refusal happened is a value rather than a log line.
pub fn proxyable(
    row: &LeaseInfo,
    ours: &SessionEndpoint,
    our_host: &str,
) -> Result<(), NotProxyable> {
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
    if published != ours.published() {
        return Err(NotProxyable::EndpointIsNotOurs {
            published: published.to_string(),
        });
    }
    Ok(())
}

/// Does a `agent@host#pid` holder token name this host?
///
/// The agent id is caller-chosen and untrimmed (J1), so it can itself contain
/// `@` and `#`. The host is the segment between the **last** `@` and the last
/// `#`, which is exactly how `LeaseHolder::token` composes it.
fn holder_is_on_host(holder: &str, our_host: &str) -> bool {
    let Some(after_at) = holder.rsplit_once('@').map(|(_, rest)| rest) else {
        return false;
    };
    let host = match after_at.rsplit_once('#') {
        Some((host, _pid)) => host,
        None => after_at,
    };
    host == our_host
}

/// Synthesize the JSON-RPC error a client gets for a call the proxy could not
/// forward — or `None` when the frame needs no answer.
///
/// A request carries an `id` and gets an error keyed to it. A **notification**
/// has no `id`, so by JSON-RPC there is nothing to answer and inventing a
/// response would corrupt the stream: it is dropped. An unparseable line is also
/// dropped, for the same reason — with no `id` there is no honest reply to make,
/// and the client is the one that wrote it.
pub fn unreachable_reply(client_frame: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(client_frame).ok()?;
    let id = value.get("id")?;
    if id.is_null() {
        return None;
    }
    Some(
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": HUB_UNREACHABLE_CODE, "message": HUB_UNREACHABLE_MESSAGE },
        })
        .to_string(),
    )
}

/// Our own client's stdout failed — the pipe is gone, so there is nobody left
/// to serve. A proxy holds no tail and no lease, so this is an ordinary exit
/// condition rather than a durability event.
fn client_gone(e: std::io::Error) -> LamboError {
    LamboError::Config(format!("proxy client stdout: {e}"))
}

/// The two frames that make an MCP session, kept so a reconnect can rebuild one.
///
/// # Why the proxy has to remember them
///
/// An MCP session is stateful: a server answers `tools/call` only after the
/// client's `initialize`. That state lives in the **holder**, so when a holder
/// dies its clients' sessions die with it — and a proxy that simply dialled the
/// next holder would forward `tools/call` frames into a server that had never
/// seen an `initialize`, which does not answer them. Measured, not reasoned: the
/// first version of this module reconnected without a replay and the recovery
/// case in `serve_proxy_multi_client.rs` hung on exactly that.
///
/// So the proxy keeps the client's own handshake — the frames it already sent
/// once, verbatim — and replays them into each new connection, swallowing the
/// duplicate `initialize` response the client has already had.
///
/// **The residual risk, stated rather than hidden.** The client's view of
/// `serverInfo`, `capabilities` and the negotiated `protocolVersion` came from
/// the *old* holder. Two holders of the same binary answer identically, which is
/// every real case on one machine; two holders of different lambo versions could
/// differ, and the client would keep the older view. That is a narrower failure
/// than "memory is gone until you restart the client", which is the alternative.
///
/// **This is emphatically NOT promotion.** Replaying into another process's
/// server is not the same as becoming one. The proxy still takes no lease; see
/// [`HubProxy::run`].
#[derive(Default)]
struct Handshake {
    /// The client's `initialize` request, verbatim.
    initialize: Option<String>,
    /// Its `notifications/initialized`, verbatim.
    initialized: Option<String>,
}

impl Handshake {
    /// Remember `frame` if it is part of the handshake. Called on every client
    /// frame; the two matches happen once each per session.
    fn observe(&mut self, frame: &str) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(frame) else {
            return;
        };
        match value.get("method").and_then(serde_json::Value::as_str) {
            Some("initialize") => self.initialize = Some(frame.to_string()),
            Some("notifications/initialized") => self.initialized = Some(frame.to_string()),
            _ => {}
        }
    }

    /// Rebuild the client's session on a freshly connected holder.
    ///
    /// The `initialize` response is read and **discarded** here, before the read
    /// half is handed to the pump's reader task, so the client never sees a
    /// second answer to an id it already has. Reading it through the same
    /// `BufReader` the task then owns is deliberate: a fresh reader could drop
    /// bytes the first one had already buffered.
    async fn replay(
        &self,
        read: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
        write: &mut tokio::net::unix::OwnedWriteHalf,
    ) -> std::io::Result<()> {
        let Some(initialize) = &self.initialize else {
            // The client has not handshaken yet, so there is nothing to rebuild
            // — its own `initialize` will flow through in a moment.
            return Ok(());
        };
        HubProxy::send(write, initialize).await?;
        let mut swallowed = String::new();
        if read.read_line(&mut swallowed).await? == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "holder closed the connection during handshake replay",
            ));
        }
        if let Some(initialized) = &self.initialized {
            HubProxy::send(write, initialized).await?;
        }
        Ok(())
    }
}

/// A line read from the holder, or the news that the connection ended.
enum FromHub {
    Frame(String),
    Closed,
}

/// Connect to the holder's endpoint, retrying inside [`CONNECT_BUDGET`].
///
/// The retry exists for the holder's own acquire→bind window: the endpoint's
/// address is published by the acquire and the socket is bound a moment later,
/// so a proxy that raced in between would otherwise see `ECONNREFUSED` on a
/// perfectly healthy session.
pub(crate) async fn connect(
    endpoint: &SessionEndpoint,
) -> Result<tokio::net::UnixStream, std::io::Error> {
    let deadline = tokio::time::Instant::now() + CONNECT_BUDGET;
    loop {
        match tokio::net::UnixStream::connect(endpoint.path()).await {
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

/// Forward this process's stdio to the session holder, for as long as its client
/// is there.
pub struct HubProxy {
    session: crate::types::SessionId,
    /// The address this build derives. Checked against the row's every time the
    /// row is re-read, so a holder that changed scheme is refused rather than
    /// dialled.
    endpoint: SessionEndpoint,
    /// The store the failed attach handed back — read-only here, and only ever
    /// for `read_lease`.
    store: Arc<dyn crate::store::GraphStore>,
    our_host: String,
}

impl HubProxy {
    pub fn new(
        session: crate::types::SessionId,
        endpoint: SessionEndpoint,
        store: Arc<dyn crate::store::GraphStore>,
        our_host: String,
    ) -> Self {
        Self {
            session,
            endpoint,
            store,
            our_host,
        }
    }

    /// Re-read the lease row and reconnect to whoever holds it **now**.
    ///
    /// The row is re-read on **every** attempt and the address is never cached:
    /// a new holder is a new endpoint, and the whole point of recovering after a
    /// holder dies is to pick up its successor without this process restarting.
    ///
    /// **This function reads the lease and must never acquire it.** See
    /// [`HubProxy::run`].
    async fn reconnect(&self) -> Result<tokio::net::UnixStream, LamboError> {
        self.dial().await
    }

    /// [`HubProxy::reconnect`], then rebuild the client's MCP session on the new
    /// connection ([`Handshake`]).
    async fn reconnect_and_replay(
        &self,
        handshake: &Handshake,
    ) -> Result<
        (
            BufReader<tokio::net::unix::OwnedReadHalf>,
            tokio::net::unix::OwnedWriteHalf,
        ),
        LamboError,
    > {
        let (read, mut write) = self.reconnect().await?.into_split();
        let mut read = BufReader::new(read);
        handshake.replay(&mut read, &mut write).await.map_err(|e| {
            LamboError::Conflict(format!("holder rejected the session handshake: {e}"))
        })?;
        Ok((read, write))
    }

    async fn dial(&self) -> Result<tokio::net::UnixStream, LamboError> {
        let row = self
            .store
            .read_lease(&self.session)
            .await
            .map_err(LamboError::Store)?
            .ok_or_else(|| {
                LamboError::Conflict(format!(
                    "session {} has no lease holder to forward to",
                    self.session
                ))
            })?;
        proxyable(&row, &self.endpoint, &self.our_host)
            .map_err(|why| LamboError::Conflict(why.explain()))?;
        connect(&self.endpoint)
            .await
            .map_err(|e| LamboError::Conflict(format!("holder endpoint not reachable: {e}")))
    }

    /// Pump frames between this process's client and the session holder until
    /// the client goes away or a shutdown signal arrives.
    ///
    /// # The invariant this function must not violate
    ///
    /// **A proxy never acquires the lease.** It reads the row to find the
    /// current holder and nothing more. The temptation is obvious and wrong: on
    /// a holder's death this process could win the lapsed lease and "become the
    /// hub". It cannot serve its own client if it does — the client's MCP
    /// session was established with the dead holder and this process has no way
    /// to replay that handshake — so it would sit there holding and
    /// *heartbeating* a session it cannot serve, wedging every process on the
    /// machine for as long as it lived. That is strictly worse than the exit-1
    /// J2 exists to replace.
    ///
    /// Acquisition and promotion are therefore **one decision, not two**: while
    /// there is no promotion machinery, acquisition is forbidden. `serve` does
    /// re-acquire, but only *before* this function is entered — before any
    /// client byte has been exchanged, when winning the lease is safe because
    /// this process can then be a real holder.
    ///
    /// What a dead holder gets instead: every forwarded call fails honestly and
    /// immediately (never hangs), and the next call re-reads the row, so the
    /// moment a new holder exists this proxy is working again with no restart.
    pub async fn run(
        &self,
        shutdown: impl std::future::Future<Output = ()>,
    ) -> Result<(), LamboError> {
        // The first connection needs no replay: the client has sent nothing
        // yet, so its own `initialize` will be the first frame through.
        let mut handshake = Handshake::default();
        let first = self.reconnect_and_replay(&handshake).await?;
        tracing::info!(
            session = %self.session,
            endpoint = %self.endpoint.path().display(),
            "lambo serve: proxying to the session holder (this process takes no lease and holds \
             no graph; every write happens in the holder, under the holder's fencing token)"
        );

        // stdin is read on its own task: a blocking read must not stop this loop
        // from noticing that the holder went away.
        let (client_tx, mut client_rx) = tokio::sync::mpsc::channel::<String>(64);
        let client_reader = tokio::spawn(async move {
            let mut lines = BufReader::new(tokio::io::stdin()).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if client_tx.send(line).await.is_err() {
                    break;
                }
            }
        });

        let mut stdout = tokio::io::stdout();
        // One channel across every hub connection, tagged with the generation it
        // came from, so a `Closed` from a connection we already replaced cannot
        // tear down its successor.
        let (hub_tx, mut hub_rx) = tokio::sync::mpsc::channel::<(u64, FromHub)>(64);
        let mut generation: u64 = 0;
        let mut writer = Self::split_hub(first, generation, &hub_tx);

        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                biased;
                () = &mut shutdown => {
                    tracing::info!("lambo serve: shutdown signal — closing the proxy");
                    break;
                }
                frame = client_rx.recv() => {
                    let Some(frame) = frame else {
                        // Our own client disconnected. That is a clean exit: a
                        // proxy exists for exactly one client.
                        tracing::info!("lambo serve: proxy client disconnected");
                        break;
                    };
                    // Recorded BEFORE forwarding, so a reconnect triggered by
                    // this very frame already has the handshake to replay.
                    handshake.observe(&frame);
                    if writer.is_none() {
                        // Reconnect on the call, not on a timer: the row is
                        // re-read here, so a new holder is picked up by the
                        // first call after it appears.
                        match self.reconnect_and_replay(&handshake).await {
                            Ok(halves) => {
                                generation += 1;
                                writer = Self::split_hub(halves, generation, &hub_tx);
                                tracing::info!(
                                    generation,
                                    "lambo serve: proxy reconnected to the current session holder"
                                );
                            }
                            Err(e) => tracing::warn!(
                                error = %e,
                                "lambo serve: proxy cannot reach a session holder — failing this \
                                 call honestly"
                            ),
                        }
                    }
                    let sent = match writer.as_mut() {
                        Some(w) => Self::send(w, &frame).await.is_ok(),
                        None => false,
                    };
                    if !sent {
                        writer = None;
                        if let Some(reply) = unreachable_reply(&frame) {
                            Self::send(&mut stdout, &reply).await.map_err(client_gone)?;
                        }
                    }
                }
                Some((gen, event)) = hub_rx.recv() => {
                    if gen != generation {
                        continue;
                    }
                    match event {
                        FromHub::Frame(frame) => {
                            Self::send(&mut stdout, &frame).await.map_err(client_gone)?
                        }
                        FromHub::Closed => {
                            tracing::warn!(
                                generation = gen,
                                "lambo serve: the session holder closed the connection — the next \
                                 call will re-read the lease and try the current holder"
                            );
                            writer = None;
                        }
                    }
                }
            }
        }
        client_reader.abort();
        Ok(())
    }

    /// Hand a fresh hub connection to the pump: the read half becomes a task
    /// feeding `hub_tx`, the write half is returned for the pump to use.
    ///
    /// Takes the halves already split and already replayed-into, rather than a
    /// `UnixStream`, because the handshake replay has to read the swallowed
    /// `initialize` response through the *same* `BufReader` this task then owns.
    fn split_hub(
        halves: (
            BufReader<tokio::net::unix::OwnedReadHalf>,
            tokio::net::unix::OwnedWriteHalf,
        ),
        generation: u64,
        hub_tx: &tokio::sync::mpsc::Sender<(u64, FromHub)>,
    ) -> Option<tokio::net::unix::OwnedWriteHalf> {
        let (read, write) = halves;
        let tx = hub_tx.clone();
        tokio::spawn(async move {
            let mut lines = read.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if tx.send((generation, FromHub::Frame(line))).await.is_err() {
                    return;
                }
            }
            let _ = tx.send((generation, FromHub::Closed)).await;
        });
        Some(write)
    }

    /// Write one frame plus its newline, then flush.
    ///
    /// The flush is not optional: this is a line-framed protocol on a pipe, and
    /// a buffered frame is a call that never arrives.
    async fn send<W: AsyncWriteExt + Unpin>(w: &mut W, frame: &str) -> std::io::Result<()> {
        w.write_all(frame.as_bytes()).await?;
        w.write_all(b"\n").await?;
        w.flush().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::StoreConfig;
    use chrono::Utc;

    fn row(holder: &str, endpoint: Option<&str>) -> LeaseInfo {
        LeaseInfo {
            holder: holder.to_string(),
            token: 1,
            acquired_at: Utc::now(),
            expires_at: Utc::now(),
            endpoint: endpoint.map(str::to_string),
        }
    }

    fn ours() -> SessionEndpoint {
        let store = StoreConfig {
            kind: crate::store::StoreKind::Sqlite,
            path: Some("/a.db".into()),
            ..StoreConfig::default()
        };
        SessionEndpoint::for_store("s", &store)
            .unwrap()
            .expect("a file-backed store is shareable")
    }

    #[test]
    fn a_holder_that_published_no_endpoint_is_not_proxyable() {
        let ours = ours();
        let err = proxyable(&row("a@this-host#1", None), &ours, "this-host").unwrap_err();
        assert_eq!(err, NotProxyable::HolderPublishedNoEndpoint);
        // The explanation must name the common cause — a CLI verb holding the
        // lease briefly — because "wait" is the right action and "debug your
        // socket" is not.
        assert!(err.explain().contains("CLI verb"), "{}", err.explain());
    }

    #[test]
    fn a_holder_on_another_host_is_not_proxyable() {
        let ours = ours();
        let err = proxyable(
            &row("a@other-host#1", Some(&ours.published())),
            &ours,
            "this-host",
        )
        .unwrap_err();
        assert_eq!(
            err,
            NotProxyable::HolderIsOnAnotherHost {
                holder: "a@other-host#1".into()
            }
        );
        // A socket path is meaningless off-host even when it happens to match,
        // so the host is checked BEFORE the path — this row's path is ours.
        assert!(err.explain().contains("other-host"));
    }

    #[test]
    fn an_endpoint_this_build_does_not_derive_is_not_proxyable() {
        let ours = ours();
        let err = proxyable(
            &row("a@this-host#1", Some("/run/somewhere-else/x.sock")),
            &ours,
            "this-host",
        )
        .unwrap_err();
        assert!(matches!(err, NotProxyable::EndpointIsNotOurs { .. }));
        assert!(
            err.explain().contains("different graph"),
            "the refusal must say what forwarding anyway would risk: {}",
            err.explain()
        );
    }

    #[test]
    fn a_local_holder_publishing_our_endpoint_is_proxyable() {
        let ours = ours();
        assert!(proxyable(
            &row("a@this-host#4213", Some(&ours.published())),
            &ours,
            "this-host"
        )
        .is_ok());
    }

    /// J1 takes `agent_id` untrimmed and unnormalised, so an agent may name
    /// itself `weird@host#9`. The host is the segment between the LAST `@` and
    /// the last `#`, exactly as `LeaseHolder::token` composes it — otherwise a
    /// self-chosen id could make a remote holder look local.
    #[test]
    fn an_agent_id_containing_at_and_hash_does_not_confuse_the_host_check() {
        assert!(holder_is_on_host("a@b#c@this-host#4213", "this-host"));
        assert!(!holder_is_on_host(
            "a@this-host#1@other-host#2",
            "this-host"
        ));
        // A malformed token names no host and must not pass.
        assert!(!holder_is_on_host("no-at-sign", "this-host"));
    }

    /// The handshake record keeps the client's own frames **verbatim** — the
    /// replay must be byte-identical, because a rebuilt initialize that differs
    /// from the one the client sent negotiates a different session than the one
    /// the client believes it has.
    #[test]
    fn the_handshake_records_the_clients_own_frames_and_nothing_else() {
        let mut h = Handshake::default();
        let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#;
        let inited = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        h.observe(init);
        h.observe(inited);
        // A tool call is traffic, not session state; recording it would replay a
        // call the client already made.
        h.observe(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"lambo_recall"}}"#,
        );
        h.observe("not json");
        assert_eq!(h.initialize.as_deref(), Some(init));
        assert_eq!(h.initialized.as_deref(), Some(inited));
    }

    /// Before the client has handshaken there is nothing to rebuild, and the
    /// replay must be a no-op rather than an invented `initialize` — the
    /// client's own is about to arrive.
    #[test]
    fn a_handshake_that_never_happened_replays_nothing() {
        let h = Handshake::default();
        assert!(h.initialize.is_none());
        // `replay` returns Ok without touching the connection in this state;
        // pinned here as the precondition, since exercising it needs a socket
        // pair and the integration test drives the connected path.
        assert!(h.initialized.is_none());
    }

    #[test]
    fn a_request_gets_an_honest_error_keyed_to_its_id() {
        let reply = unreachable_reply(
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"lambo_derive"}}"#,
        )
        .expect("a request must be answered");
        let v: serde_json::Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(v["id"], 7);
        assert_eq!(v["error"]["code"], HUB_UNREACHABLE_CODE);
        let msg = v["error"]["message"].as_str().unwrap();
        // What the model needs: nothing happened, it recovers, do not block.
        assert!(msg.contains("NOTHING WAS READ OR WRITTEN"), "{msg}");
        assert!(msg.contains("retry later"), "{msg}");
        assert!(msg.contains("Do not block on memory"), "{msg}");
        // N4: no path, no store URL, no raw errno text.
        assert!(!msg.contains('/'), "no socket path may leak: {msg}");
        assert!(!msg.contains("://"), "no store URL may leak: {msg}");
    }

    /// A string id is legal JSON-RPC and must be echoed as a string, not
    /// coerced — a client matches its own id byte for byte.
    #[test]
    fn a_string_id_is_echoed_unchanged() {
        let reply =
            unreachable_reply(r#"{"jsonrpc":"2.0","id":"call-9","method":"tools/list"}"#).unwrap();
        let v: serde_json::Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(v["id"], "call-9");
    }

    /// A notification has no id, so JSON-RPC has nothing to answer and
    /// inventing a response would corrupt the client's stream.
    #[test]
    fn a_notification_and_a_broken_frame_are_not_answered() {
        assert!(
            unreachable_reply(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .is_none()
        );
        assert!(unreachable_reply(r#"{"jsonrpc":"2.0","id":null,"method":"x"}"#).is_none());
        assert!(unreachable_reply("not json at all").is_none());
        assert!(unreachable_reply("").is_none());
    }
}
