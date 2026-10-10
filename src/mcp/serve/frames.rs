//! The frame cap of the line-framed MCP transports (#101): stdio and the
//! session endpoint's socket connections.
//!
//! # Why the cap is here and not on the fields
//!
//! rmcp 3.1.2 reads a line-framed transport with `read_until(b'\n')` into a
//! buffer with no limit, then parses the whole line into a JSON-RPC message
//! whose `tools/call` arguments are a `serde_json::Map<String, Value>`. Only
//! after that does a tool's `Parameters<T>` deserialize Lambo's own types,
//! from that `Value`. So by the time any Lambo code (or any Lambo
//! `deserialize_with`) sees `image.data` or a `values` array, the whole of it
//! is already in memory twice over: the line, and the `Value` tree (an owned
//! `String`; a `Vec<Value>` of 32 bytes per number). The tool's own caps
//! (`surface::image::MAX_IMAGE_B64_LEN`, `MAX_VECTOR_VALUES`) run before
//! decoding and embedding, but cannot run before that allocation.
//!
//! The one place a frame can be refused before it is buffered is the read
//! itself. [`CappedFrames`] sits between the byte stream and rmcp's
//! unchanged transport: it assembles each line itself, at most
//! [`MAX_MCP_FRAME_BYTES`] of it, and hands rmcp only complete lines within
//! the cap, byte for byte. A longer line is counted and thrown away through
//! its newline, never buffered past the cap, and the stream resynchronises
//! at the next line, as the proxy's own reader does
//! (`crate::mcp::proxy`'s `read_frame`).
//!
//! The cap is the HTTP transport's body cap, so the three transports refuse
//! the same frames. Over HTTP, rmcp streams a body and answers `413` once
//! more than that has arrived, before parsing any of it; a session opener's
//! body is read by Lambo's guard under the same ceiling.
//!
//! # No reply to a discarded frame
//!
//! A discarded frame was never parsed, so there is no request id to answer.
//! It is logged at WARN with its size and the cap, never its content, and
//! the caller's request goes unanswered: the same policy rmcp applies to a
//! line that is not JSON, and the proxy to its own oversized frames.

use std::pin::Pin;
use std::task::{ready, Context, Poll};

use tokio::io::{AsyncBufRead, AsyncRead, BufReader, ReadBuf};

use super::http_guards::MAX_HTTP_BODY_BYTES;

/// The longest frame, in bytes without its newline, a line-framed MCP
/// transport accepts: the HTTP body cap (4 MiB), well above the largest
/// legitimate request (an image at `MAX_IMAGE_B64_LEN`, 2,796,204 base64
/// characters, plus a caption and the JSON envelope, about 2.9 MB).
pub(crate) const MAX_MCP_FRAME_BYTES: usize = MAX_HTTP_BODY_BYTES as usize;

/// A byte stream of newline-delimited frames, with every frame longer than a
/// cap removed before anything downstream buffers it. See the module docs.
///
/// Cancellation-safe like any `poll_read`: every partial state (the frame
/// being assembled, the count of a frame being discarded, the rest of a
/// frame being handed out) lives in the struct, so a dropped read resumes
/// where it stopped.
pub(crate) struct CappedFrames<R> {
    inner: BufReader<R>,
    cap: usize,
    /// Which transport this is, for the log line: `stdio` or `endpoint`.
    transport: &'static str,
    /// The frame being assembled (never more than `cap` bytes), or, once
    /// `ready`, the complete frame and its newline being handed out.
    frame: Vec<u8>,
    /// How much of a `ready` frame has been handed out.
    handed: usize,
    ready: bool,
    /// Non-zero while a frame over the cap is being discarded: its length
    /// so far. Its bytes are counted, never kept.
    over: usize,
    /// The most bytes `frame` has held at once, for the tests' proof that an
    /// oversized frame is never buffered.
    #[cfg(test)]
    peak: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl<R: AsyncRead + Unpin> CappedFrames<R> {
    /// Cap `inner` at [`MAX_MCP_FRAME_BYTES`]. `transport` names it in the
    /// log line for a discarded frame.
    pub(crate) fn new(inner: R, transport: &'static str) -> Self {
        Self::with_cap(inner, transport, MAX_MCP_FRAME_BYTES)
    }

    pub(crate) fn with_cap(inner: R, transport: &'static str, cap: usize) -> Self {
        Self {
            inner: BufReader::new(inner),
            cap,
            transport,
            frame: Vec::new(),
            handed: 0,
            ready: false,
            over: 0,
            #[cfg(test)]
            peak: std::sync::Arc::default(),
        }
    }

    /// The most bytes this reader has buffered for one frame, shared so a
    /// test can read it after the reader has moved into a transport.
    #[cfg(test)]
    pub(crate) fn peak(&self) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
        std::sync::Arc::clone(&self.peak)
    }

    fn discarded(&mut self, terminated: bool) {
        tracing::warn!(
            transport = self.transport,
            bytes = self.over,
            cap = self.cap,
            terminated,
            "lambo serve: a client frame over the size cap was discarded unread (no reply is \
             possible: no request id was read from it)"
        );
        self.over = 0;
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for CappedFrames<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        loop {
            if this.ready {
                let rest = &this.frame[this.handed..];
                let n = rest.len().min(buf.remaining());
                buf.put_slice(&rest[..n]);
                this.handed += n;
                if this.handed == this.frame.len() {
                    this.frame.clear();
                    this.handed = 0;
                    this.ready = false;
                }
                return Poll::Ready(Ok(()));
            }
            let available = ready!(Pin::new(&mut this.inner).poll_fill_buf(cx))?;
            if available.is_empty() {
                // End of stream. An unterminated last frame within the cap is
                // handed on as it is: rmcp's `read_until` parses one, so
                // dropping it here would change what a client gets.
                if this.over > 0 {
                    this.discarded(false);
                } else if !this.frame.is_empty() {
                    this.ready = true;
                    continue;
                }
                return Poll::Ready(Ok(()));
            }
            let (take, terminated) = match available.iter().position(|b| *b == b'\n') {
                Some(i) => (i, true),
                None => (available.len(), false),
            };
            if this.over > 0 {
                this.over = this.over.saturating_add(take);
            } else if this.frame.len() + take > this.cap {
                // Past the cap: count it and drop what was kept of it.
                this.over = this.frame.len() + take;
                this.frame.clear();
            } else {
                this.frame.extend_from_slice(&available[..take]);
                #[cfg(test)]
                this.peak
                    .fetch_max(this.frame.len(), std::sync::atomic::Ordering::Relaxed);
            }
            Pin::new(&mut this.inner).consume(take + usize::from(terminated));
            if terminated {
                if this.over > 0 {
                    this.discarded(true);
                } else {
                    this.frame.push(b'\n');
                    this.ready = true;
                }
            }
        }
    }
}
