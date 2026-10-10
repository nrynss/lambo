//! Moving frames between the client and the holder: which frames are
//! requests and which are answers ([`request_id`], [`response_id`]), the
//! holder's reader task ([`HubProxy::split_hub`]), the framed write
//! ([`HubProxy::send`]) and the in-flight depth warning.

use tokio::io::AsyncWriteExt;

use super::dialing::HubHalves;
use super::framing::{read_frame, Framed, MAX_FRAME_BYTES};
use super::HubProxy;

/// The in-flight depth past which the pump says so, once (J2-R2-7).
///
/// Not a cap: nothing is refused or dropped at this depth, and the list is
/// argued unbounded-by-construction at its declaration in [`HubProxy::run`]. This
/// is the number that makes that argument falsifiable — a request/response MCP
/// client has one call outstanding and a pipelining one a handful, so 64 is two
/// orders above the ceiling the argument claims and cannot fire on real traffic.
/// It reuses [`MAX_REPLAY_FRAMES`](super::handshake::MAX_REPLAY_FRAMES)'s order of magnitude for the same reason: past
/// it, the peer is doing something no legitimate client does.
pub(crate) const INFLIGHT_DEPTH_WARN: usize = 64;

/// Build-time invariant tying the two ceilings together, so neither can be
/// moved without the other being considered.
///
/// Asserted here, on the proxy side, because the proxy is the consumer whose
/// in-flight list a receipt wait occupies; the write queue is core and does
/// not depend on the transport (#27).
const _: () = assert!(
    crate::writeq::MAX_CONCURRENT_RECEIPT_WAITS * 2 <= INFLIGHT_DEPTH_WARN,
    "MAX_CONCURRENT_RECEIPT_WAITS must leave half of INFLIGHT_DEPTH_WARN for ordinary traffic — \
     a waiting lambo_stats(receipt=...) holds a proxy inflight slot, and answer_lost writes one un-raced \
     frame per slot (J2-R2-7, J2-R3-3)",
);

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
pub(super) fn request_id(frame: &str) -> Option<serde_json::Value> {
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
pub(super) fn response_id(frame: &str) -> Option<serde_json::Value> {
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

/// A line read from the holder, or the news that the connection ended.
pub(super) enum FromHub {
    Frame(String),
    Closed,
}

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
pub(super) enum Step {
    FromClient(Option<ClientInput>),
    FromHub(Option<(u64, FromHub)>),
}

/// What the proxy's stdin reader hands the pump.
pub(super) enum ClientInput {
    /// A frame to forward to the holder.
    Frame(String),
    /// The reply to a frame over the client cap, which was dropped here:
    /// written to the client, never forwarded (#101 review M2).
    TooLarge(String),
}

impl HubProxy {
    /// Hand a fresh hub connection to the pump: the read half becomes a task
    /// feeding `hub_tx`, the write half is returned for the pump to use.
    ///
    /// Takes the halves already split and already replayed-into, rather than a
    /// `UnixStream`, because the handshake replay has to read the swallowed
    /// `initialize` response through the *same* `BufReader` this task then owns.
    pub(super) fn split_hub(
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
                    Ok(Framed::Oversize { bytes, .. }) => tracing::warn!(
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
    pub(super) async fn send<W: AsyncWriteExt + Unpin>(
        w: &mut W,
        frame: &str,
    ) -> std::io::Result<()> {
        w.write_all(frame.as_bytes()).await?;
        w.write_all(b"\n").await?;
        w.flush().await
    }
}
