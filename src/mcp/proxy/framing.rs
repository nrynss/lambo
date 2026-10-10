//! Reading one newline-delimited JSON-RPC frame from either peer, bounded
//! and resynchronising ([`read_frame`], [`Framed`]).

use tokio::io::AsyncBufReadExt;

/// The largest single frame either peer may send, in bytes.
///
/// Both directions read newline-delimited frames into a buffer, and a peer that
/// never sends a newline would grow that buffer without bound — a broken client
/// or a hostile one can OOM this process (J2-R1-18). 8 MiB is far above any real
/// MCP frame (the largest thing that crosses is a `lambo_recall` result, tens of
/// KiB) and far below anything that threatens a machine, so a frame past it is a
/// defect rather than a big call, and is dropped as one.
pub(super) const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// The largest frame the proxy forwards from its client to the holder: the
/// holder's own request cap ([`crate::mcp::serve::MAX_MCP_FRAME_BYTES`],
/// #101), which discards anything longer unread. Dropping it here instead
/// spares forwarding up to [`MAX_FRAME_BYTES`] the holder will throw away,
/// and leaves no forwarded request in the in-flight list that can never be
/// answered. Responses from the holder keep the wider [`MAX_FRAME_BYTES`].
pub(super) const MAX_CLIENT_FRAME_BYTES: usize = crate::mcp::serve::MAX_MCP_FRAME_BYTES;

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
pub(super) enum Framed {
    /// A complete, newline-terminated, valid-UTF-8 frame, without its newline.
    Line(String),
    /// The peer stopped mid-frame: this many bytes arrived with no newline
    /// after them. Never forwarded — a torn JSON line is never valid to
    /// deliver — and always followed by end-of-stream.
    Torn(usize),
    /// A frame past the cap ([`MAX_FRAME_BYTES`] unless the reader was given
    /// another), discarded through its newline. The
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
pub(super) async fn read_frame<R>(r: &mut R) -> std::io::Result<Framed>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    read_frame_within(r, MAX_FRAME_BYTES).await
}

/// [`read_frame`] with a frame cap of `cap` bytes instead of
/// [`MAX_FRAME_BYTES`].
pub(super) async fn read_frame_within<R>(r: &mut R, cap: usize) -> std::io::Result<Framed>
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
            if over > 0 || buf.len() + take > cap {
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
