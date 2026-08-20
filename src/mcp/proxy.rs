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
///
/// **It means the call never left this process.** That is why it is a different
/// code from [`HUB_LOST_CODE`]: the two differ in whether a retry is safe, which
/// is the only thing a caller can act on.
const HUB_UNREACHABLE_CODE: i64 = -32001;

/// JSON-RPC error code returned for a call that **was** forwarded and then lost
/// with the holder (J2-R1-1).
///
/// A separate code from [`HUB_UNREACHABLE_CODE`] on purpose. The two situations
/// are indistinguishable in the logs and opposite in consequence: an
/// unreachable holder means the call did not happen and a bare retry is safe,
/// while a lost in-flight call means *nobody knows* whether it happened and a
/// bare retry of a write may duplicate it. Collapsing them into one code would
/// force every caller to guess, and the honest answer here is "unknown", not
/// "nothing".
const HUB_LOST_CODE: i64 = -32002;

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

/// What a caller is told when its call was already inside the holder when the
/// holder stopped answering (J2-R1-1).
///
/// Same N4 discipline as [`HUB_UNREACHABLE_MESSAGE`] — model-facing, no socket
/// path, no store URL, no errno — but deliberately **not** the same claim.
/// `HUB_UNREACHABLE_MESSAGE` says "NOTHING WAS READ OR WRITTEN" because the
/// frame never left this process. Here the frame did leave, and this process
/// cannot know what the holder did with it before it died: an embed that had
/// already committed, or one that had not. Telling a model "nothing happened"
/// in that state is a lie that costs a duplicate write, so the text says
/// *unknown* and gives the one instruction that resolves it — recall before
/// re-deriving.
const HUB_LOST_MESSAGE: &str = "lambo: this call had already been handed to the process that \
     holds this session when that process stopped answering, so its outcome is UNKNOWN. It may \
     have been applied or it may not — if it was a write, treat it as neither done nor undone; \
     if it was a read, you received nothing. Memory recovers on its own once a lambo serve holds \
     the session again, so retry later. When it answers, recall before re-deriving: repeating a \
     write that did land duplicates it, and repeating one that did not is the fix. Do not block \
     on memory: carry on with the work.";

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
/// until the deadline says otherwise (J2-R1-8). The two together bound how long
/// the pump can be deaf to SIGTERM inside one `client_rx` arm body at
/// 2 × `CONNECT_BUDGET`.
pub(crate) const CONNECT_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// Interval between connect attempts inside [`CONNECT_BUDGET`].
const CONNECT_RETRY: std::time::Duration = std::time::Duration::from_millis(100);

/// The largest single frame either peer may send, in bytes.
///
/// Both directions read newline-delimited frames into a buffer, and a peer that
/// never sends a newline would grow that buffer without bound — a broken client
/// or a hostile one can OOM this process (J2-R1-18). 8 MiB is far above any real
/// MCP frame (the largest thing that crosses is a `lambo_recall` result, tens of
/// KiB) and far below anything that threatens a machine, so a frame past it is a
/// defect rather than a big call, and is dropped as one.
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// How many frames the handshake replay will read before giving up on finding
/// the `initialize` response it is waiting for.
///
/// A count bound beside the time bound ([`CONNECT_BUDGET`]): a holder that
/// streams notifications at speed could otherwise fill memory inside the
/// deadline. Generous, because every frame before the response is legitimate
/// traffic that gets forwarded.
const MAX_REPLAY_FRAMES: usize = 64;

/// The `LEASE_TTL` figure quoted verbatim in [`HUB_UNREACHABLE_MESSAGE`].
///
/// Model-facing text cannot be `format!`ed into a `const`, so the number is
/// written out — and this assertion is what stops it becoming a lie if the TTL
/// ever moves (J2-R1-14). A build that changes `LEASE_TTL` fails here, at the
/// sentence that needs rewording.
const _: () = assert!(
    crate::store::lease::LEASE_TTL.as_secs() == 45,
    "HUB_UNREACHABLE_MESSAGE tells the model the previous holder's lease lapses \
     'within 45 seconds'. LEASE_TTL has changed, so that sentence is now false — \
     reword it and update this assertion."
);

/// One frame from a line-framed peer, or the reason there is not one.
///
/// # Why this exists rather than `AsyncBufReadExt::lines()`
///
/// `tokio::io::Lines` is wrong for a forwarding pipe in three ways, each of
/// which the round-1 review found as its own defect:
///
/// * **It invents a frame boundary.** A trailing line ending is optional, so
///   `Lines` yields an unterminated remainder as a line. A holder that dies
///   mid-write therefore had the half of a JSON object that reached the socket
///   delivered to the client's stdout *as a complete frame* (J2-R1-4). A byte
///   pipe copies frames without interpreting them, which is exactly why it must
///   not manufacture one the peer never wrote.
/// * **It grows without bound** (J2-R1-18).
/// * **It ends the stream on a decode error.** `while let Ok(Some(line))` treats
///   a single non-UTF-8 byte as end-of-input, so the client was told "proxy
///   client disconnected" for what was a bad frame (J2-R1-17).
///
/// So: a frame is complete or it is not, an over-long or non-UTF-8 frame is
/// dropped *and the stream resynchronises at the next newline*, and only a real
/// EOF ends the stream.
#[derive(Debug, PartialEq, Eq)]
enum Framed {
    /// A complete, newline-terminated, valid-UTF-8 frame, without its newline.
    Line(String),
    /// The peer stopped mid-frame: this many bytes arrived with no newline
    /// after them. Never forwarded — a torn JSON line is never valid to
    /// deliver — and always followed by end-of-stream.
    Torn(usize),
    /// A frame past [`MAX_FRAME_BYTES`], discarded through its newline. The
    /// stream is still usable.
    Oversize(usize),
    /// A complete frame that is not UTF-8, so it cannot be JSON-RPC. Discarded;
    /// the stream is still usable.
    NotUtf8(usize),
    /// The peer closed cleanly, on a frame boundary.
    Eof,
}

/// Read one [`Framed`] from a line-framed peer.
///
/// Bounded and resynchronising — see [`Framed`] for why neither property is
/// optional here.
async fn read_frame<R>(r: &mut R) -> std::io::Result<Framed>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut buf: Vec<u8> = Vec::new();
    // Set once the frame passes the cap: from then on bytes are counted and
    // thrown away rather than buffered, up to the newline that ends the frame.
    let mut over = 0usize;
    loop {
        let (consume, terminated) = {
            let available = r.fill_buf().await?;
            if available.is_empty() {
                return Ok(if over > 0 {
                    Framed::Oversize(buf.len() + over)
                } else if buf.is_empty() {
                    Framed::Eof
                } else {
                    Framed::Torn(buf.len())
                });
            }
            let (take, terminated) = match available.iter().position(|b| *b == b'\n') {
                Some(i) => (i, true),
                None => (available.len(), false),
            };
            if over > 0 || buf.len() + take > MAX_FRAME_BYTES {
                over += take;
            } else {
                buf.extend_from_slice(&available[..take]);
            }
            (take + usize::from(terminated), terminated)
        };
        r.consume(consume);
        if terminated {
            if over > 0 {
                return Ok(Framed::Oversize(buf.len() + over));
            }
            // `\r\n` is legal on the wire; `Lines` strips it, so this does too.
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            return Ok(match String::from_utf8(buf) {
                Ok(line) => Framed::Line(line),
                Err(e) => Framed::NotUtf8(e.into_bytes().len()),
            });
        }
    }
}

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
    /// [`proxyable`].
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
/// symmetry — [`HubProxy::dial`] runs the published directory through the same
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

/// The `id` of a client frame that is a **request** — the only kind of frame
/// this process may ever answer on the holder's behalf.
///
/// Three exclusions, each of which would corrupt the client's stream if it were
/// answered:
///
/// * a **notification** has no `id` (or a null one), so by JSON-RPC there is
///   nothing to answer and inventing a response would invent a frame;
/// * an unparseable line has no `id` to key a reply to, and the client is the
///   one that wrote it;
/// * a **response** — an `id` and *no* `method` — is the client's own answer to
///   a server-initiated request (`sampling/createMessage`, `roots/list`), and
///   that id belongs to the *holder*. Answering it would send the holder's own
///   request id back to the client as an error it never asked for (J2-R1-10).
///
/// So the `method` key is what separates "a call this process owes an answer to"
/// from "traffic that merely carries an id".
fn request_id(frame: &str) -> Option<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_str(frame).ok()?;
    // A request has a method. A response does not.
    value.get("method")?.as_str()?;
    let id = value.get("id")?;
    if id.is_null() {
        return None;
    }
    Some(id.clone())
}

/// The `id` a **holder frame** answers, when it is a response at all.
///
/// The mirror of [`request_id`], and it is what retires an in-flight id: a
/// `result` or an `error` keyed to an id the client is waiting on. A holder
/// frame with a `method` is a notification or a server-initiated request, not an
/// answer, so it retires nothing — a `notifications/progress` carrying the
/// original id must not be mistaken for the call completing.
fn response_id(frame: &str) -> Option<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_str(frame).ok()?;
    if value.get("method").is_some() {
        return None;
    }
    if value.get("result").is_none() && value.get("error").is_none() {
        return None;
    }
    let id = value.get("id")?;
    if id.is_null() {
        return None;
    }
    Some(id.clone())
}

/// One JSON-RPC error response, keyed to `id`.
fn error_frame(id: &serde_json::Value, code: i64, message: &str) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
    .to_string()
}

/// Synthesize the JSON-RPC error a client gets for a call the proxy could not
/// forward — or `None` when the frame needs no answer (see [`request_id`]).
///
/// This is the *never left this process* case: nothing was read or written.
pub fn unreachable_reply(client_frame: &str) -> Option<String> {
    request_id(client_frame)
        .map(|id| error_frame(&id, HUB_UNREACHABLE_CODE, HUB_UNREACHABLE_MESSAGE))
}

/// The error a client gets for a call that was forwarded and then lost with the
/// holder (J2-R1-1) — the *outcome unknown* case.
fn lost_reply(id: &serde_json::Value) -> String {
    error_frame(id, HUB_LOST_CODE, HUB_LOST_MESSAGE)
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
/// **A second residual, and it is a wider one (J2-R1-11).** These two frames are
/// not the whole of a session's client-side state. Anything else the client
/// *configured* on the old holder is silently lost on reconnect:
/// `logging/setLevel`, `notifications/roots/list_changed`, and any subscription
/// a future protocol revision adds. The new holder starts at its defaults and
/// the client is never told, because from the client's side nothing happened.
///
/// This is documented rather than fixed, deliberately. Recording "the small set
/// of idempotent session-configuring frames" means this module maintaining a
/// list of which MCP methods are session state — an enumeration of the protocol,
/// which is the one thing a byte pipe is chosen to avoid, and one that goes
/// stale silently on every protocol revision. The two frames replayed here are
/// not an arbitrary subset: `initialize` and `notifications/initialized` are the
/// only frames whose absence makes the *next* call fail, which is why they are
/// the ones measured and the ones replayed. Everything else degrades to a
/// default. If a future revision makes some other frame load-bearing in the same
/// way, this is the place that has to learn about it — and the way to notice is
/// that the reconnect stops working, not that a lint fires.
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
    ///
    /// # What is swallowed, and what is not (J2-R1-12)
    ///
    /// The response is found by **id**, not by position. Swallowing the first
    /// line back assumes the holder says nothing before answering; a holder that
    /// emits any notification first had that notification eaten and its actual
    /// `initialize` response forwarded to the client as a duplicate answer to an
    /// id the client already holds. So every frame before the matching response
    /// is returned to the caller to be forwarded, and only the response itself
    /// is dropped.
    ///
    /// # Bounded, because this runs inside the pump's arm body (J2-R1-8)
    ///
    /// `reconnect_and_replay` is awaited *in* the `client_rx` arm, not as a
    /// `select!` branch, so the shutdown branch cannot be polled while this
    /// runs. A `UnixStream::connect` succeeds as soon as the connection lands in
    /// the listener's backlog, so "accepted but never answered" needs no hostile
    /// peer — a holder whose accept loop is starved is enough — and an unbounded
    /// read here made the process deaf to SIGTERM as well as wedged. Bounded by
    /// [`CONNECT_BUDGET`] in time and [`MAX_REPLAY_FRAMES`] in count. Reusing the
    /// connect budget is deliberate: both are "this holder is not answering the
    /// door", and the *other* wait — for a dead holder's lease to lapse — is the
    /// TTL's job, not this function's.
    ///
    /// Returns the frames read before the response, in order, for the caller to
    /// forward to its client.
    async fn replay<R, W>(&self, read: &mut R, write: &mut W) -> std::io::Result<Vec<String>>
    where
        R: tokio::io::AsyncBufRead + Unpin,
        W: AsyncWriteExt + Unpin,
    {
        let Some(initialize) = &self.initialize else {
            // The client has not handshaken yet, so there is nothing to rebuild
            // — its own `initialize` will flow through in a moment.
            return Ok(Vec::new());
        };
        HubProxy::send(write, initialize).await?;
        let answered = match tokio::time::timeout(
            CONNECT_BUDGET,
            Self::swallow_response(read, request_id(initialize)),
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!(
                        "holder accepted the connection but did not answer the replayed \
                         initialize within {}s",
                        CONNECT_BUDGET.as_secs()
                    ),
                ))
            }
        };
        if let Some(initialized) = &self.initialized {
            HubProxy::send(write, initialized).await?;
        }
        Ok(answered)
    }

    /// Read until the frame answering `want` arrives, returning everything read
    /// before it. See [`Handshake::replay`] for the bounds and the reasons.
    async fn swallow_response<R>(
        read: &mut R,
        want: Option<serde_json::Value>,
    ) -> std::io::Result<Vec<String>>
    where
        R: tokio::io::AsyncBufRead + Unpin,
    {
        let mut before = Vec::new();
        for _ in 0..MAX_REPLAY_FRAMES {
            match read_frame(read).await? {
                Framed::Line(line) => {
                    // A recorded `initialize` with no id is malformed and cannot
                    // be matched, so fall back to the old positional rule rather
                    // than reading until the bound.
                    let matches = match (&want, response_id(&line)) {
                        (Some(want), Some(got)) => *want == got,
                        (None, _) => true,
                        _ => false,
                    };
                    if matches {
                        return Ok(before);
                    }
                    before.push(line);
                }
                Framed::Eof | Framed::Torn(_) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "holder closed the connection during handshake replay",
                    ))
                }
                Framed::Oversize(bytes) | Framed::NotUtf8(bytes) => {
                    tracing::warn!(
                        bytes,
                        "lambo serve: the holder sent an unusable frame during the handshake \
                         replay — dropping it and reading on"
                    );
                }
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "holder sent {MAX_REPLAY_FRAMES} frames without answering the replayed initialize"
            ),
        ))
    }
}

/// A line read from the holder, or the news that the connection ended.
enum FromHub {
    Frame(String),
    Closed,
}

/// The two halves of a hub connection, already split and already replayed into.
type HubHalves = (
    BufReader<tokio::net::unix::OwnedReadHalf>,
    tokio::net::unix::OwnedWriteHalf,
);

/// Which side of the pipe spoke, as one value.
///
/// The pump's `select!` polls shutdown first and unconditionally, then these two
/// in **random** order. Keeping `biased` over all three made the client arm
/// starve the hub arm under a client that streams notifications continuously —
/// self-limiting for a request/response client, not for a streaming one
/// (J2-R1-21). `tokio::select!` has no per-arm bias, so the fix is to nest: an
/// outer biased `select!` guarantees shutdown wins, and an inner unbiased one
/// gives the two traffic directions equal footing. The inner arms only *receive*
/// — every await that could be cut short lives in the outer arm body — so the
/// nesting costs no cancellation safety.
enum Step {
    FromClient(Option<String>),
    FromHub(Option<(u64, FromHub)>),
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
    async fn reconnect(&self) -> Result<tokio::net::UnixStream, LamboError> {
        self.dial().await
    }

    /// [`HubProxy::reconnect`], then rebuild the client's MCP session on the new
    /// connection ([`Handshake`]).
    ///
    /// Also returns any frames the holder emitted *before* answering the
    /// replayed `initialize` — legitimate traffic that the caller must forward
    /// to its client rather than swallow (J2-R1-12).
    async fn reconnect_and_replay(
        &self,
        handshake: &Handshake,
    ) -> Result<(HubHalves, Vec<String>), LamboError> {
        let (read, mut write) = self.reconnect().await?.into_split();
        let mut read = BufReader::new(read);
        let before = handshake.replay(&mut read, &mut write).await.map_err(|e| {
            LamboError::Conflict(format!("holder rejected the session handshake: {e}"))
        })?;
        Ok(((read, write), before))
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
        connect(&address)
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
    /// connection** is answered with [`HUB_LOST_MESSAGE`] — outcome *unknown*,
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
    /// of it. An honest error in microseconds lets the model decide, which is
    /// what AGENTS.md's "never block on memory" asks for, and the error text
    /// tells it the one thing it needs to decide safely: recall before
    /// re-deriving.
    pub async fn run(
        &self,
        shutdown: impl std::future::Future<Output = ()>,
    ) -> Result<(), LamboError> {
        // The first connection needs no replay: the client has sent nothing
        // yet, so its own `initialize` will be the first frame through.
        let mut handshake = Handshake::default();
        let (first, _no_preamble) = self.reconnect_and_replay(&handshake).await?;
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
        let mut inflight: Vec<(u64, serde_json::Value)> = Vec::new();

        tokio::pin!(shutdown);
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
                        // first call after it appears.
                        match self.reconnect_and_replay(&handshake).await {
                            Ok((halves, before)) => {
                                generation += 1;
                                writer = Self::split_hub(halves, generation, &hub_tx);
                                tracing::info!(
                                    generation,
                                    "lambo serve: proxy reconnected to the current session holder"
                                );
                                // Whatever the new holder said before answering
                                // the replayed handshake is the client's traffic,
                                // not ours to eat (J2-R1-12).
                                for frame in before {
                                    Self::send(&mut stdout, &frame).await.map_err(client_gone)?;
                                }
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
                    if sent {
                        // Now this process owes the client an answer even if the
                        // holder never gives one (J2-R1-1). Recorded AFTER the
                        // write, because a frame that failed to write is
                        // answered below instead — and recorded against the
                        // generation it went out on, so the connection that
                        // loses it is the one that answers for it.
                        if let Some(id) = request_id(&frame) {
                            inflight.push((generation, id));
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
                Step::FromHub(Some((gen, event))) => {
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
                            if gen != generation {
                                // Not an answer anyone is waiting for, from a
                                // connection we no longer talk to: a
                                // notification, or a server-initiated request
                                // whose reply would go into a socket that is
                                // gone. Dropping it is honest; forwarding it
                                // would invite the client to answer nobody.
                                tracing::warn!(
                                    generation = gen,
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
                            let lost = Self::answer_lost(&mut stdout, &mut inflight, gen).await?;
                            if lost > 0 {
                                tracing::warn!(
                                    generation = gen,
                                    lost,
                                    "lambo serve: the session holder closed the connection with \
                                     calls still in flight — each was answered with an honest \
                                     'outcome unknown' error, because this process cannot know \
                                     whether the holder applied them before it died"
                                );
                            }
                            if gen == generation {
                                tracing::warn!(
                                    generation = gen,
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

    /// Answer every request still outstanding on `generation`, then forget it.
    ///
    /// This is the mechanism behind "never hangs" for the one call that used to
    /// hang forever (J2-R1-1). It runs when a hub connection ends — for the
    /// current connection and for a superseded one alike, because a client
    /// waiting on an id does not care which connection carried it.
    ///
    /// The reply is [`HUB_LOST_MESSAGE`], not [`HUB_UNREACHABLE_MESSAGE`]: these
    /// frames *were* written to the holder, so "nothing was read or written" is
    /// false for them and a model that believed it would re-derive a write that
    /// may already have landed.
    ///
    /// Returns how many were answered, so the caller can log the count without
    /// logging when there is nothing to say.
    async fn answer_lost<W: AsyncWriteExt + Unpin>(
        client: &mut W,
        inflight: &mut Vec<(u64, serde_json::Value)>,
        generation: u64,
    ) -> Result<usize, LamboError> {
        let mut lost = Vec::new();
        inflight.retain(|(gen, id)| {
            if *gen == generation {
                lost.push(id.clone());
                false
            } else {
                true
            }
        });
        for id in &lost {
            Self::send(client, &lost_reply(id))
                .await
                .map_err(client_gone)?;
        }
        Ok(lost.len())
    }

    /// Hand a fresh hub connection to the pump: the read half becomes a task
    /// feeding `hub_tx`, the write half is returned for the pump to use.
    ///
    /// Takes the halves already split and already replayed-into, rather than a
    /// `UnixStream`, because the handshake replay has to read the swallowed
    /// `initialize` response through the *same* `BufReader` this task then owns.
    fn split_hub(
        halves: HubHalves,
        generation: u64,
        hub_tx: &tokio::sync::mpsc::Sender<(u64, FromHub)>,
    ) -> Option<tokio::net::unix::OwnedWriteHalf> {
        let (mut read, write) = halves;
        let tx = hub_tx.clone();
        tokio::spawn(async move {
            loop {
                match read_frame(&mut read).await {
                    Ok(Framed::Line(line)) => {
                        if tx.send((generation, FromHub::Frame(line))).await.is_err() {
                            return;
                        }
                    }
                    Ok(Framed::Eof) => break,
                    Ok(Framed::Torn(bytes)) => {
                        // J2-R1-4, and this is the direction where it mattered:
                        // the half of a JSON object that reached the socket
                        // before the holder died used to be delivered to the
                        // client's stdout as a complete frame. A torn JSON line
                        // is never valid to deliver.
                        tracing::warn!(
                            generation,
                            bytes,
                            "lambo serve: the session holder died mid-frame — dropping the \
                             unterminated remainder rather than forwarding truncated JSON to the \
                             client (the calls it left in flight are answered honestly below)"
                        );
                        break;
                    }
                    Ok(Framed::Oversize(bytes)) => tracing::warn!(
                        generation,
                        bytes,
                        cap = MAX_FRAME_BYTES,
                        "lambo serve: the session holder sent a frame over the size cap — dropped"
                    ),
                    Ok(Framed::NotUtf8(bytes)) => tracing::warn!(
                        generation,
                        bytes,
                        "lambo serve: the session holder sent a frame that is not UTF-8, so it \
                         cannot be JSON-RPC — dropped, and this connection is still live \
                         (J2-R1-17)"
                    ),
                    Err(e) => {
                        tracing::warn!(
                            generation,
                            error = %e,
                            "lambo serve: reading from the session holder failed"
                        );
                        break;
                    }
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
        SessionEndpoint::for_store("s", &store).expect("a file-backed store is shareable")
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
        assert_eq!(
            proxyable(
                &row("a@this-host#4213", Some(&ours.published())),
                &ours,
                "this-host"
            )
            .expect("our own derivation is proxyable"),
            ours.path().to_path_buf(),
            "the address to dial is the published one, which here equals ours"
        );
    }

    /// J2-L1, measured live: `cursor-agent` scrubs `TMPDIR` from the environment
    /// of the MCP server it spawns and `opencode` passes macOS's per-user
    /// `TMPDIR` through, so before this the two products' serves derived two
    /// **directories** for one session on one store and the loser refused to
    /// forward — cross-client memory silently absent on default wiring.
    ///
    /// The directory may differ. The **name** may not: it is a hash of the
    /// session and the canonicalized store identity, so a match means the same
    /// session on the same store and the directory decides only reachability.
    #[test]
    fn a_holder_publishing_the_same_address_in_another_directory_is_proxyable() {
        let ours = ours();
        let name = ours
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        // The two directories the live probe actually produced, in shape.
        for dir in [
            "/var/folders/q1/4dwfdvt563ng8lwybj8bry_c0000gn/T/lambo-501",
            "/run/user/501/lambo",
        ] {
            let published = format!("{dir}/{name}");
            let address = proxyable(&row("a@this-host#1", Some(&published)), &ours, "this-host")
                .unwrap_or_else(|e| {
                    panic!("a matching address name must be proxyable: {}", e.explain())
                });
            assert_eq!(
                address,
                std::path::PathBuf::from(&published),
                "the address to DIAL is the holder's published path, not this process's \
                 derivation — that is the whole fix"
            );
            assert_ne!(
                address,
                ours.path(),
                "and this test is only meaningful because the two differ"
            );
        }
    }

    /// The other side of the same decision: a differing **name** is the real
    /// different-graph case and stays a refusal, because the name is the only
    /// thing carrying identity.
    #[test]
    fn a_holder_publishing_a_different_address_name_is_not_proxyable() {
        let ours = ours();
        let dir = ours.path().parent().unwrap().display().to_string();
        for published in [
            // Same directory, another session or another store.
            format!("{dir}/other-0000000000000000.sock"),
            // Our own name with the hash altered by one nibble.
            {
                let name = ours
                    .path()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_string();
                format!("{dir}/{}", name.replacen(".sock", "x.sock", 1))
            },
            // No name at all.
            "/".to_string(),
            String::new(),
        ] {
            let err = proxyable(&row("a@this-host#1", Some(&published)), &ours, "this-host")
                .expect_err("a different address name must be refused");
            assert!(
                matches!(err, NotProxyable::EndpointIsNotOurs { .. }),
                "{published:?} gave {err:?}"
            );
        }
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
    ///
    /// Driven rather than asserted (J2-R1-15): the previous version of this test
    /// never called `replay`, it checked that `Handshake::default()` has two
    /// `None`s. What matters is that **no bytes reach the holder**, which is a
    /// property of `replay`, not of the struct.
    #[tokio::test]
    async fn a_handshake_that_never_happened_replays_nothing() {
        let h = Handshake::default();
        let mut read = BufReader::new(tokio::io::empty());
        let mut wrote: Vec<u8> = Vec::new();
        let before = h.replay(&mut read, &mut wrote).await.expect("a no-op");
        assert!(
            wrote.is_empty(),
            "an un-handshaken client must send nothing on reconnect, not an invented initialize"
        );
        assert!(before.is_empty());
        // And it must not have tried to read an answer either: `empty()` is at
        // EOF, so a swallow would have failed rather than returned Ok.
    }

    /// The replay finds its answer by **id**, not by position (J2-R1-12).
    ///
    /// A holder that says anything before answering used to have that frame
    /// swallowed and its actual `initialize` response forwarded to the client as
    /// a duplicate answer to an id the client already holds. The preamble is now
    /// returned for the caller to forward, and only the response is dropped.
    #[tokio::test]
    async fn the_replay_swallows_the_initialize_response_and_forwards_what_came_before_it() {
        let mut h = Handshake::default();
        h.observe(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);
        h.observe(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        let holder = concat!(
            r#"{"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info"}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":1,"result":{"serverInfo":{"name":"h"}}}"#,
            "\n",
        );
        let mut read = BufReader::new(std::io::Cursor::new(holder.as_bytes().to_vec()));
        let mut wrote: Vec<u8> = Vec::new();
        let before = h.replay(&mut read, &mut wrote).await.expect("replayed");
        assert_eq!(
            before.len(),
            1,
            "the notification must be forwarded: {before:?}"
        );
        assert!(before[0].contains("notifications/message"));
        // Both recorded frames went out, byte-identical and in order.
        let sent = String::from_utf8(wrote).unwrap();
        let sent: Vec<&str> = sent.lines().collect();
        assert_eq!(
            sent,
            vec![
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            ]
        );
    }

    /// J2-R1-8: a holder that accepts and never answers must not park the pump.
    ///
    /// `UnixStream::connect` succeeds as soon as the connection lands in the
    /// listener's backlog, so this needs no hostile peer — a holder whose accept
    /// loop is starved is enough. The unbounded `read_line` this replaces made
    /// the process deaf to SIGTERM too, because the replay is awaited inside a
    /// `select!` arm **body**.
    ///
    /// Time is paused, so the 2s budget costs the suite nothing: the runtime
    /// auto-advances the clock the moment the read is the only pending work,
    /// which is exactly the state a never-answering holder produces.
    #[tokio::test(start_paused = true)]
    async fn a_holder_that_never_answers_the_replay_is_given_up_on() {
        let mut h = Handshake::default();
        h.observe(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);
        // The far half is held open and silent for the whole call.
        let (ours, _theirs) = tokio::io::duplex(4096);
        let mut read = BufReader::new(ours);
        let mut wrote: Vec<u8> = Vec::new();
        let err = h
            .replay(&mut read, &mut wrote)
            .await
            .expect_err("a silent holder must not be waited on forever");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut, "{err}");
        assert!(
            err.to_string()
                .contains("did not answer the replayed initialize"),
            "the operator needs to know which holder behaviour this was: {err}"
        );
    }

    /// J2-R1-4, the finding that mattered most of the three framing ones: a
    /// holder that dies mid-write must not have the half-object that reached the
    /// socket delivered to the client as a frame.
    #[tokio::test]
    async fn a_torn_final_frame_is_dropped_not_forwarded() {
        let torn = br#"{"jsonrpc":"2.0","id":1,"resu"#.to_vec();
        let mut read = BufReader::new(std::io::Cursor::new(torn));
        assert_eq!(read_frame(&mut read).await.unwrap(), Framed::Torn(29));
        // And then end-of-stream, so the pump reports Closed exactly once.
        assert_eq!(read_frame(&mut read).await.unwrap(), Framed::Eof);
    }

    /// A complete frame, `\r\n` framing, and an empty line all behave; an EOF on
    /// a frame boundary is an EOF and nothing else.
    #[tokio::test]
    async fn a_complete_frame_is_read_without_its_newline() {
        let bytes = b"{\"a\":1}\n{\"b\":2}\r\n\n".to_vec();
        let mut read = BufReader::new(std::io::Cursor::new(bytes));
        assert_eq!(
            read_frame(&mut read).await.unwrap(),
            Framed::Line(r#"{"a":1}"#.to_string())
        );
        assert_eq!(
            read_frame(&mut read).await.unwrap(),
            Framed::Line(r#"{"b":2}"#.to_string()),
            "a CRLF-framed peer must not leave a stray carriage return in the frame"
        );
        assert_eq!(
            read_frame(&mut read).await.unwrap(),
            Framed::Line(String::new())
        );
        assert_eq!(read_frame(&mut read).await.unwrap(), Framed::Eof);
    }

    /// J2-R1-17: one bad byte used to end the pump, and the client was told
    /// "proxy client disconnected" for what was a decode failure. The frame is
    /// dropped and the **stream survives**.
    #[tokio::test]
    async fn a_non_utf8_frame_is_dropped_and_the_stream_survives() {
        let mut bytes = b"{\"a\":\"".to_vec();
        bytes.push(0xff);
        bytes.extend_from_slice(b"\"}\n{\"b\":2}\n");
        let mut read = BufReader::new(std::io::Cursor::new(bytes));
        assert!(matches!(
            read_frame(&mut read).await.unwrap(),
            Framed::NotUtf8(_)
        ));
        assert_eq!(
            read_frame(&mut read).await.unwrap(),
            Framed::Line(r#"{"b":2}"#.to_string()),
            "the next frame must still arrive: a bad frame is not end-of-input"
        );
    }

    /// J2-R1-18: neither direction may grow a buffer without bound. The
    /// over-long frame is discarded **through its newline**, so the stream
    /// resynchronises instead of splitting the oversize frame into garbage
    /// frames.
    #[tokio::test]
    async fn an_oversize_frame_is_dropped_and_the_stream_resynchronises() {
        let mut bytes = vec![b'x'; MAX_FRAME_BYTES + 10];
        bytes.push(b'\n');
        bytes.extend_from_slice(b"{\"after\":1}\n");
        let mut read = BufReader::new(std::io::Cursor::new(bytes));
        match read_frame(&mut read).await.unwrap() {
            Framed::Oversize(bytes) => assert_eq!(bytes, MAX_FRAME_BYTES + 10),
            other => panic!("expected Oversize, got {other:?}"),
        }
        assert_eq!(
            read_frame(&mut read).await.unwrap(),
            Framed::Line(r#"{"after":1}"#.to_string())
        );
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
    ///
    /// A **client response** is the third case (J2-R1-10): it carries an id and
    /// no method, but that id was minted by the *holder* for a server-initiated
    /// request (`sampling/createMessage`, `roots/list`). Answering it would send
    /// the holder's own request id back to the client as an error the client
    /// never asked for.
    #[test]
    fn a_notification_and_a_broken_frame_are_not_answered() {
        assert!(
            unreachable_reply(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .is_none()
        );
        assert!(unreachable_reply(r#"{"jsonrpc":"2.0","id":null,"method":"x"}"#).is_none());
        assert!(unreachable_reply("not json at all").is_none());
        assert!(unreachable_reply("").is_none());
        // The client's own answer to a server-initiated request: an id, no
        // method. Not ours to answer.
        assert!(
            unreachable_reply(r#"{"jsonrpc":"2.0","id":11,"result":{"model":"x"}}"#).is_none(),
            "a client RESPONSE must not be answered — that id belongs to the holder"
        );
        assert!(unreachable_reply(
            r#"{"jsonrpc":"2.0","id":11,"error":{"code":-1,"message":"no"}}"#
        )
        .is_none());
    }

    /// What retires an in-flight id, and what must not.
    ///
    /// The pump answers every id still outstanding when a hub connection ends
    /// (J2-R1-1), so a frame wrongly treated as an answer means a call the
    /// client is still waiting on gets no error — the original defect, one step
    /// removed. A `notifications/progress` echoes the request's id and is
    /// emphatically not an answer.
    #[test]
    fn only_a_response_retires_an_in_flight_id() {
        assert_eq!(
            response_id(r#"{"jsonrpc":"2.0","id":4,"result":{"content":[]}}"#),
            Some(serde_json::json!(4))
        );
        assert_eq!(
            response_id(r#"{"jsonrpc":"2.0","id":"c-4","error":{"code":-1,"message":"x"}}"#),
            Some(serde_json::json!("c-4"))
        );
        // Progress on a call that is still running: same id, not an answer.
        assert_eq!(
            response_id(
                r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"id":4},"id":4}"#
            ),
            None,
            "a notification carrying the id is not the call completing"
        );
        // A server-initiated request the holder sends mid-call.
        assert_eq!(
            response_id(r#"{"jsonrpc":"2.0","id":4,"method":"roots/list"}"#),
            None
        );
        // Neither result nor error: not a response at all.
        assert_eq!(response_id(r#"{"jsonrpc":"2.0","id":4}"#), None);
        assert_eq!(response_id("not json"), None);
    }

    /// The two failures a proxy can have are opposite in consequence, so they
    /// must not share their text or their code (J2-R1-1).
    ///
    /// "Nothing was read or written" is true of a frame that never left this
    /// process and **false** of one that reached the holder. A model told the
    /// wrong one re-derives a write that may already have landed.
    #[test]
    fn a_lost_in_flight_call_is_told_unknown_not_nothing() {
        let lost = lost_reply(&serde_json::json!(4));
        let v: serde_json::Value = serde_json::from_str(&lost).unwrap();
        assert_eq!(v["id"], 4);
        assert_eq!(v["error"]["code"], HUB_LOST_CODE);
        assert_ne!(
            HUB_LOST_CODE, HUB_UNREACHABLE_CODE,
            "a caller must be able to tell 'did not happen' from 'unknown'"
        );
        let msg = v["error"]["message"].as_str().unwrap();
        assert!(msg.contains("UNKNOWN"), "{msg}");
        assert!(
            !msg.contains("NOTHING WAS READ OR WRITTEN"),
            "the never-forwarded claim must not be reused here: {msg}"
        );
        // The one instruction that resolves the uncertainty safely.
        assert!(msg.contains("recall before re-deriving"), "{msg}");
        assert!(msg.contains("Do not block on memory"), "{msg}");
        // N4, same as the unreachable text: no path, no store URL.
        assert!(!msg.contains('/'), "no socket path may leak: {msg}");
        assert!(!msg.contains("://"), "no store URL may leak: {msg}");
    }

    /// `answer_lost` is per-connection: a reconnect does not retire the ids the
    /// *previous* connection is still on the hook for, and answering a
    /// generation twice would send the client two errors for one id.
    #[tokio::test]
    async fn lost_calls_are_answered_per_connection_and_only_once() {
        let mut out: Vec<u8> = Vec::new();
        let mut inflight = vec![
            (0, serde_json::json!(1)),
            (0, serde_json::json!("two")),
            (1, serde_json::json!(3)),
        ];
        let answered = HubProxy::answer_lost(&mut out, &mut inflight, 0)
            .await
            .unwrap();
        assert_eq!(answered, 2);
        assert_eq!(
            inflight,
            vec![(1, serde_json::json!(3))],
            "the surviving connection's call is still outstanding"
        );
        let text = String::from_utf8(out.clone()).unwrap();
        let ids: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()["id"].clone())
            .collect();
        assert_eq!(ids, vec![serde_json::json!(1), serde_json::json!("two")]);
        // Draining the same generation again answers nothing: one error per id.
        out.clear();
        assert_eq!(
            HubProxy::answer_lost(&mut out, &mut inflight, 0)
                .await
                .unwrap(),
            0
        );
        assert!(out.is_empty());
    }
}
