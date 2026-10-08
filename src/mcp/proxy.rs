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
//!
//! # Its interface with `serve`
//!
//! `serve`'s role resolution reaches this module through `serve::hub` only,
//! and through six items: [`proxyable`] and [`NotProxyable::explain`] (the
//! refusal's wording when the holder cannot be proxied), `dial_dir` and
//! `connect` for the election's probe of the holder, then [`HubProxy::new`]
//! and [`HubProxy::run`] for the proxy role. Nothing else crosses.
//!
//! # Modules
//!
//! | module | holds |
//! |---|---|
//! | this file | [`HubProxy`] and the pump ([`HubProxy::run`]) |
//! | `dialing` | the budgets, [`proxyable`], the dial-side directory check, the connect, the shutdown-raced dial |
//! | `handshake` | the remembered handshake and its replay on reconnect |
//! | `forwarding` | request and response ids, the holder reader task, the framed write, the in-flight warning |
//! | `disconnect` | the honest errors: never-left-this-process and lost-with-the-holder |
//! | `framing` | the bounded, resynchronising frame reader |

use std::sync::Arc;

use tokio::io::BufReader;

use crate::mcp::endpoint::SessionEndpoint;
use crate::types::LamboError;

mod dialing;
mod disconnect;
mod forwarding;
mod framing;
mod handshake;

pub(crate) use dialing::{connect, dial_dir};
pub use dialing::{proxyable, NotProxyable};
pub use disconnect::unreachable_reply;
pub(crate) use forwarding::INFLIGHT_DEPTH_WARN;

use dialing::Dialled;
use disconnect::client_gone;
use forwarding::{request_id, response_id, FromHub, Step};
use framing::{read_frame, Framed, MAX_FRAME_BYTES};
use handshake::Handshake;

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
    /// J4-R1-2. The agent id of this proxying serve, written as the `agent`
    /// on its own `proxying` / `proxying_stopped` ledger lines so the proxying
    /// actor is never anonymized — a reader of the line sees exactly who is
    /// forwarding (and, on the proxy path, who was refused).
    agent: String,
    /// J4. An optional call ledger this proxy appends its own `lease` lines to
    /// (`proxying` at the first successful dial, `proxying_stopped` when the
    /// holder it forwards to stops answering). A proxy is alive and can write
    /// its own lines — see §J2's handoff. `None` for a serve run without
    /// `--ledger`.
    ledger: Option<Arc<crate::ledger::Ledger>>,
}

impl HubProxy {
    pub fn new(
        session: crate::types::SessionId,
        endpoint: SessionEndpoint,
        store: Arc<dyn crate::store::GraphStore>,
        our_host: String,
        agent: String,
        ledger: Option<Arc<crate::ledger::Ledger>>,
    ) -> Self {
        Self {
            session,
            endpoint,
            store,
            our_host,
            agent,
            ledger,
        }
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
    /// ## The invariant's other side: a fenced HOLDER exits (JE2E-4)
    ///
    /// The wedge invariant forbids a proxy from *becoming* the writer. Its dual
    /// is what a writer does when it stops being one, and since JE2E-4 the
    /// answer is symmetric: it winds down (`mcp::serve::wind_down`). The two are
    /// the same argument from opposite ends — a process may only serve the role
    /// its client handshook with, so a proxy may not promote itself into a
    /// holder and an ex-holder may not go on answering as one.
    ///
    /// It is also what makes the exit cheap rather than an outage, and the
    /// reason is *this* function: the client respawns its serve, the respawn
    /// loses the election to the real holder, and it arrives here as a proxy.
    /// That is the self-heal the "no in-process promotion" residual otherwise
    /// costs, arriving through the door J2 already built.
    ///
    /// What a dead holder gets instead: every forwarded call fails honestly and
    /// *bounded* — never hangs — and the next call re-reads the row, so the
    /// moment a new holder exists this proxy is working again with no restart.
    ///
    /// "Bounded" is three different numbers and the docstring used to round them
    /// all to "immediately" (J2-R2-1). Re-derived from the constants: a call
    /// already inside the dead holder is answered the moment its connection
    /// closes, which is microseconds of local work (2.6 ms end to end, measured
    /// at the two-client probe); a *new* call has to dial, and a dead holder
    /// whose socket file is still on disk refuses the connect, so that dial
    /// spends `CONNECT_BUDGET + CONNECT_RETRY` ≈ 2.1s retrying before it gives up
    /// (the probe measured ~2s for exactly this path); and the whole dial,
    /// including the lease-row read that opens it, is capped at `DIAL_BUDGET`.
    ///
    /// # Why the pump tracks in-flight ids (J2-R1-1)
    ///
    /// "Never hangs" is a promise about the call that matters most — the one
    /// already inside the holder when the holder died — and a byte pipe that
    /// only answers frames it *failed to write* does not keep it. A frame
    /// written successfully and then lost with its connection got no reply and
    /// no error, and the recovery path made that permanent rather than
    /// transient: the reconnect lives in the `client_rx` arm, so a client
    /// politely awaiting its response sends nothing, and a proxy waiting for a
    /// client byte reconnects to nothing. Two halves of one wedge, and the
    /// review that found it reproduced it from unmutated pump code.
    ///
    /// So the pump keeps every forwarded request id, tagged with the hub
    /// connection ("generation") it went out on, and retires it when a response
    /// answers it. When a connection ends, every id still outstanding **on that
    /// connection** is answered with `HUB_LOST_MESSAGE` — outcome *unknown*,
    /// not "nothing happened", because this process genuinely cannot tell. The
    /// client then has its answer, sends its next request, and that request
    /// drives the reconnect exactly as before. The wedge closes at both halves.
    ///
    /// **Nothing is retried, deliberately.** A retry would need this process to
    /// know which calls are idempotent, which means parsing `params.name` and
    /// knowing what the seven tools do — the tool-level understanding the byte
    /// pipe exists *not* to have, and the thing that keeps `agent_id` crossing
    /// verbatim and the tool surface from drifting. It would not even be cheap:
    /// the reconnect can only succeed once a new holder exists, which is up to
    /// one `LEASE_TTL` plus the election slack away, so "retry the read" means
    /// holding the caller's call open for the better part of a minute — the
    /// exact hang J2 exists to remove, reintroduced for the calls least in need
    /// of it. An honest error in milliseconds lets the model decide, which is
    /// what AGENTS.md's "never block on memory" asks for, and the error text
    /// tells it the one thing it needs to decide safely: recall before
    /// re-deriving.
    pub async fn run(
        &self,
        shutdown: impl std::future::Future<Output = ()>,
    ) -> Result<(), LamboError> {
        // Pinned HERE, before the first dial, and not at the loop (J2-R2-1). The
        // first dial runs the same unbudgeted lease-row read as every later one,
        // so arming only at the loop left a startup window in which this process
        // was deaf to SIGTERM for as long as the store took — the pre-handshake
        // shape of the very defect the I rounds closed at `serve`.
        tokio::pin!(shutdown);

        // The first connection needs no replay: the client has sent nothing
        // yet, so its own `initialize` will be the first frame through.
        let mut handshake = Handshake::default();
        let (first, _no_preamble, dialled) =
            match self.dial_bounded(&handshake, shutdown.as_mut()).await {
                Dialled::Hub(halves, before, dialled) => (halves, before, dialled),
                Dialled::Failed(e) => return Err(e),
                Dialled::ShutdownRequested => {
                    // Nothing to unwind and nothing to answer: no lease, no tail, no
                    // client byte exchanged, and the stdin task below is not spawned
                    // yet. This is the clean exit the `serve` proxy branch expects.
                    tracing::info!(
                        "lambo serve: shutdown signal during the proxy's first dial — exiting \
                     without opening a connection to the session holder"
                    );
                    return Ok(());
                }
            };
        // J2-R2-4: `dialled`, not `endpoint`. The old line logged
        // `self.endpoint` — this process's own derivation — which under J2-L1
        // half (2) is by construction not necessarily the socket that was
        // connected to, so an operator greping the headline line for the socket
        // (which J2-R1-19's doc paragraph tells them to do) was sent to a file
        // that does not exist. Both are logged, named for what they are: the
        // truth is `dialled`, and `derived` is what makes the divergence visible
        // without reading the earlier directory-differs line.
        tracing::info!(
            session = %self.session,
            dialled = %dialled.display(),
            derived = %self.endpoint.path().display(),
            "lambo serve: proxying to the session holder (this process takes no lease and holds \
             no graph; every write happens in the holder, under the holder's fencing token)"
        );
        // J4: a proxying serve is alive and books its own line — "proxying to
        // holder <X>". This is the artifact §J2's handoff points at.
        if let Some(ledger) = &self.ledger {
            ledger.append(&crate::ledger::lease_line(
                "proxying",
                "loser",
                &self.session.to_string(),
                &self.agent,
                &dialled.display().to_string(),
                None,
            ));
        }

        // stdin is read on its own task: a blocking read must not stop this loop
        // from noticing that the holder went away.
        let (client_tx, mut client_rx) = tokio::sync::mpsc::channel::<String>(64);
        let client_reader = tokio::spawn(async move {
            let mut stdin = BufReader::new(tokio::io::stdin());
            loop {
                match read_frame(&mut stdin).await {
                    Ok(Framed::Line(line)) => {
                        if client_tx.send(line).await.is_err() {
                            break;
                        }
                    }
                    Ok(Framed::Eof) => break,
                    Ok(Framed::Torn(bytes)) => {
                        // The client stopped mid-frame. Never forwarded: the
                        // holder would reject it anyway, and a byte pipe must
                        // not manufacture a frame boundary (J2-R1-4).
                        tracing::warn!(
                            bytes,
                            "lambo serve: the proxy's client stopped mid-frame — dropping the \
                             unterminated remainder rather than forwarding a torn JSON line"
                        );
                        break;
                    }
                    Ok(Framed::Oversize(bytes)) => tracing::warn!(
                        bytes,
                        cap = MAX_FRAME_BYTES,
                        "lambo serve: the proxy's client sent a frame over the size cap — dropped \
                         (no reply is possible: the frame was discarded before any id could be \
                         read from it)"
                    ),
                    Ok(Framed::NotUtf8(bytes)) => tracing::warn!(
                        bytes,
                        "lambo serve: the proxy's client sent a frame that is not UTF-8, so it \
                         cannot be JSON-RPC — dropped, and this stream is still live (J2-R1-17)"
                    ),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "lambo serve: the proxy's client stdin failed"
                        );
                        break;
                    }
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
        // Every request forwarded and not yet answered, tagged with the hub
        // connection it went out on. This list is what makes "never hangs" true
        // for the call in flight at the holder's death — see this function's
        // docs and [`HubProxy::answer_lost`].
        //
        // # Why it is neither capped nor indexed (J2-R2-7)
        //
        // It grows one entry per forwarded request and shrinks on the response or
        // on `Closed`, and both the growth and the O(n) `position` scan below are
        // bounded by the same real quantity: **the client's own in-flight
        // window**. The client here is the local, trusted party that spawned this
        // process; a request/response MCP client has one outstanding call, and
        // even a pipelining one has a handful, so n is a handful and the scan is
        // faster than a map would be. `MAX_FRAME_BYTES` was added for the
        // analogous unbounded case (J2-R1-18) and is *not* the same shape: that
        // one grew on bytes a peer chose to send with no reply expected, so a
        // single broken frame could OOM the process, whereas an entry here costs
        // a request the client is still waiting on.
        //
        // A cap was considered and declined for a specific reason: answering the
        // oldest id early to make room would let the holder's real answer arrive
        // afterwards, match nothing, and be forwarded as a **second** response to
        // an id the client has already been given an error for — a protocol
        // violation manufactured to fix a growth that has no real cause. Tearing
        // the connection down instead would put a new failure mode into the very
        // path J2-R1-1 exists to make reliable.
        //
        // What is here instead is the observability the argument depends on: the
        // WARN below fires once the list passes a depth no real client explains,
        // so if the ceiling ever stops holding it is a log line rather than a
        // slow leak. **J3 is the reason that matters**: receipts lengthen how long
        // an entry can stay outstanding, so this ceiling should be re-derived
        // there rather than inherited.
        let mut inflight: Vec<(u64, serde_json::Value)> = Vec::new();
        let mut inflight_warned = false;

        loop {
            // Shutdown first and unconditionally; the two traffic directions
            // then compete on equal terms. See [`Step`].
            let step = tokio::select! {
                biased;
                () = &mut shutdown => {
                    tracing::info!("lambo serve: shutdown signal — closing the proxy");
                    break;
                }
                step = async {
                    tokio::select! {
                        frame = client_rx.recv() => Step::FromClient(frame),
                        event = hub_rx.recv() => Step::FromHub(event),
                    }
                } => step,
            };
            match step {
                Step::FromClient(frame) => {
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
                        // first call after it appears. Raced against shutdown and
                        // capped at `DIAL_BUDGET` (J2-R2-1): this is the await the
                        // old "2 × CONNECT_BUDGET" bound was wrong about, because
                        // `dial` starts with a store read that no budget covered.
                        match self.dial_bounded(&handshake, shutdown.as_mut()).await {
                            Dialled::Hub(halves, before, dialled) => {
                                generation += 1;
                                writer = Self::split_hub(halves, generation, &hub_tx);
                                tracing::info!(
                                    generation,
                                    dialled = %dialled.display(),
                                    derived = %self.endpoint.path().display(),
                                    "lambo serve: proxy reconnected to the current session holder"
                                );
                                // JE2E-3: this closes the degradation episode
                                // the `proxying_stopped` line opened. Without
                                // it, two consecutive `proxying_stopped` lines
                                // are ambiguous — an operator cannot tell one
                                // long outage from two short ones — and the
                                // recovery, which is the thing J2 bought, has
                                // no artifact at all. Same event and same line
                                // as the first dial's: this proxy is forwarding
                                // to a holder again, and `holder` names which.
                                if let Some(ledger) = &self.ledger {
                                    ledger.append(&crate::ledger::lease_line(
                                        "proxying",
                                        "loser",
                                        &self.session.to_string(),
                                        &self.agent,
                                        &dialled.display().to_string(),
                                        Some(serde_json::json!({ "generation": generation })),
                                    ));
                                }
                                // Whatever the new holder said before answering
                                // the replayed handshake is the client's traffic,
                                // not ours to eat (J2-R1-12).
                                for frame in before {
                                    Self::send(&mut stdout, &frame).await.map_err(client_gone)?;
                                }
                            }
                            // JE2E-3: deliberately no ledger line. The episode
                            // this dial is failing inside was already booked by
                            // the `proxying_stopped` line at the `Closed` that
                            // started it, and a line here would be one per
                            // retry — N lines for one outage, with no way to
                            // tell them from N outages.
                            Dialled::Failed(e) => tracing::warn!(
                                error = %e,
                                "lambo serve: proxy cannot reach a session holder — failing this \
                                 call honestly"
                            ),
                            Dialled::ShutdownRequested => {
                                // The frame that triggered this dial goes
                                // unanswered, exactly as any frame in flight at a
                                // SIGTERM does. Answering it would mean writing to
                                // a client whose process is being torn down while
                                // the signal waits.
                                tracing::info!(
                                    "lambo serve: shutdown signal while dialling the session \
                                     holder — closing the proxy"
                                );
                                break;
                            }
                        }
                    }
                    let sent = match writer.as_mut() {
                        Some(w) => Self::send(w, &frame).await.is_ok(),
                        None => false,
                    };
                    if sent {
                        // Now this process owes the client an answer even if the
                        // holder never gives one (J2-R1-1). Recorded AFTER the
                        // write, because a frame that failed to write is
                        // answered below instead — and recorded against the
                        // generation it went out on, so the connection that
                        // loses it is the one that answers for it.
                        if let Some(id) = request_id(&frame) {
                            inflight.push((generation, id));
                            if inflight.len() > INFLIGHT_DEPTH_WARN && !inflight_warned {
                                inflight_warned = true;
                                tracing::warn!(
                                    outstanding = inflight.len(),
                                    threshold = INFLIGHT_DEPTH_WARN,
                                    "lambo serve: the proxy is holding more unanswered forwarded \
                                     requests than any real client explains — the holder may be \
                                     accepting frames without answering them. Every one of them \
                                     is still owed an answer and will get one when this \
                                     connection ends (J2-R2-7)"
                                );
                            }
                        }
                    } else {
                        writer = None;
                        if let Some(reply) = unreachable_reply(&frame) {
                            Self::send(&mut stdout, &reply).await.map_err(client_gone)?;
                        }
                    }
                }
                Step::FromHub(None) => {
                    // Unreachable while this pump holds `hub_tx`, and a `break`
                    // rather than an `unwrap` because "the hub channel closed"
                    // is an exit condition, not a panic.
                    tracing::warn!("lambo serve: the proxy's hub channel closed");
                    break;
                }
                Step::FromHub(Some((event_generation, event))) => {
                    match event {
                        FromHub::Frame(frame) => {
                            // A response retires the id it answers, from ANY
                            // generation. A late answer from a connection this
                            // pump has already replaced is still the holder's
                            // answer to a call the client is waiting on, and
                            // dropping it on the generation filter was the
                            // second half of J2-R1-1 — the id it answered was
                            // then never answered at all.
                            let answers = response_id(&frame).and_then(|id| {
                                inflight.iter().position(|(_, waiting)| *waiting == id)
                            });
                            if let Some(i) = answers {
                                inflight.remove(i);
                                Self::send(&mut stdout, &frame).await.map_err(client_gone)?;
                                continue;
                            }
                            if event_generation != generation {
                                // Not an answer anyone is waiting for, from a
                                // connection we no longer talk to: a
                                // notification, or a server-initiated request
                                // whose reply would go into a socket that is
                                // gone. Dropping it is honest; forwarding it
                                // would invite the client to answer nobody.
                                tracing::warn!(
                                    generation = event_generation,
                                    current = generation,
                                    "lambo serve: dropped a frame from a superseded holder \
                                     connection — it answers nothing this client is waiting for"
                                );
                                continue;
                            }
                            Self::send(&mut stdout, &frame).await.map_err(client_gone)?
                        }
                        FromHub::Closed => {
                            // Answer what this connection owes BEFORE deciding
                            // what its ending means for the pump: those ids are
                            // owed an answer whether or not the connection was
                            // still the current one.
                            let lost =
                                Self::answer_lost(&mut stdout, &mut inflight, event_generation)
                                    .await?;
                            if lost > 0 {
                                tracing::warn!(
                                    generation = event_generation,
                                    lost,
                                    "lambo serve: the session holder closed the connection with \
                                     calls still in flight — each was answered with an honest \
                                     'outcome unknown' error, because this process cannot know \
                                     whether the holder applied them before it died"
                                );
                            }
                            if event_generation == generation {
                                // J4 / **JE2E-3: the artifact is booked HERE,
                                // on the current connection ending, not inside
                                // `lost > 0`.**
                                //
                                // The shape chosen, stated where it is appended:
                                // **one line per degradation episode**, and the
                                // episode is "this proxy has no holder to
                                // forward to". It begins exactly when the
                                // current connection ends and it ends at the
                                // `proxying` line the reconnect books, so the
                                // pair brackets the window in which this
                                // client had no memory.
                                //
                                // The rejected alternatives are both "per
                                // retry": booking on each failed re-dial writes
                                // one line per call the client makes into a
                                // dead hub, and booking on any generation's
                                // `Closed` writes a second line for a
                                // connection already superseded — a
                                // *superseded* connection ending is the tail of
                                // an episode already booked, not a new one.
                                //
                                // `lost` stays as detail and is now often 0,
                                // which is the whole finding: the commonest
                                // degraded shape is a holder dying while the
                                // proxy is IDLE (most of a session is between
                                // calls), and under `lost > 0` that booked
                                // nothing at all — leaving "why did this agent
                                // have no memory" unanswerable from artifacts
                                // on the very path J4 exists to answer it on.
                                if let Some(ledger) = &self.ledger {
                                    ledger.append(&crate::ledger::lease_line(
                                        "proxying_stopped",
                                        "loser",
                                        &self.session.to_string(),
                                        &self.agent,
                                        &self.endpoint.path().display().to_string(),
                                        Some(serde_json::json!({ "lost": lost })),
                                    ));
                                }
                                tracing::warn!(
                                    generation = event_generation,
                                    "lambo serve: the session holder closed the connection — the \
                                     next call will re-read the lease and try the current holder"
                                );
                                writer = None;
                            }
                        }
                    }
                }
            }
        }
        client_reader.abort();
        Ok(())
    }
}

#[cfg(test)]
mod tests;
