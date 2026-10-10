//! Rebuilding the client's MCP session on a freshly dialled holder: the two
//! handshake frames the proxy remembers and replays ([`Handshake`]).

use tokio::io::AsyncWriteExt;

use super::dialing::CONNECT_BUDGET;
use super::forwarding::{request_id, response_id};
use super::framing::{read_frame, Framed};
use super::HubProxy;

/// How many frames the handshake replay will read before giving up on finding
/// the `initialize` response it is waiting for.
///
/// A count bound beside the time bound ([`CONNECT_BUDGET`]): a holder that
/// streams notifications at speed could otherwise fill memory inside the
/// deadline. Generous, because every frame before the response is legitimate
/// traffic that gets forwarded.
pub(super) const MAX_REPLAY_FRAMES: usize = 64;

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
pub(super) struct Handshake {
    /// The client's `initialize` request, verbatim.
    pub(super) initialize: Option<String>,
    /// Its `notifications/initialized`, verbatim.
    pub(super) initialized: Option<String>,
}

impl Handshake {
    /// Remember `frame` if it is part of the handshake. Called on every client
    /// frame; the two matches happen once each per session.
    pub(super) fn observe(&mut self, frame: &str) {
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
    /// `select!` branch. A `UnixStream::connect` succeeds as soon as the
    /// connection lands in the listener's backlog, so "accepted but never
    /// answered" needs no hostile peer — a holder whose accept loop is starved is
    /// enough — and an unbounded read here made the process deaf to SIGTERM as
    /// well as wedged. Bounded by [`CONNECT_BUDGET`] in time and
    /// [`MAX_REPLAY_FRAMES`] in count. Reusing the connect budget is deliberate:
    /// both are "this holder is not answering the door", and the *other* wait —
    /// for a dead holder's lease to lapse — is the TTL's job, not this
    /// function's.
    ///
    /// This paragraph used to add "so the shutdown branch cannot be polled while
    /// this runs". That is no longer true, and it was the premise under which the
    /// arm body's deafness had to be bounded by budgets alone (J2-R2-1): the
    /// whole dial, this replay included, is now polled *against* the shutdown
    /// future by [`HubProxy::dial_bounded`], and capped by [`DIAL_BUDGET`](super::dialing::DIAL_BUDGET). The
    /// budget here still earns its keep — it is what distinguishes "the holder
    /// did not answer the handshake" from "the dial ran out of time" in the
    /// operator's log — but it is no longer the only thing standing between a
    /// silent holder and an unkillable process.
    ///
    /// Returns the frames read before the response, in order, for the caller to
    /// forward to its client.
    pub(super) async fn replay<R, W>(
        &self,
        read: &mut R,
        write: &mut W,
    ) -> std::io::Result<Vec<String>>
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
    pub(super) async fn swallow_response<R>(
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
                Framed::Oversize { bytes, .. } | Framed::NotUtf8(bytes) => {
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
