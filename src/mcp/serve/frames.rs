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
//! # The reply to a discarded frame
//!
//! A discarded frame was never parsed, but the reader has seen its first
//! `cap` bytes and keeps a small ring of its last ones, and that is where a
//! request's top-level `id` sits: before `params` (serde-built clients) or
//! after it (the TypeScript SDK). [`super::frame_id`] recovers it,
//! conservatively, and the caller gets a JSON-RPC `-32600` error, "request
//! too large (over 4 MiB)", keyed to that id, or to `null` when none can be
//! recovered with confidence. The connection stays open and the session
//! goes on serving. HTTP answers the same frame with `413`, so no transport
//! leaves the caller waiting for its own timeout. The frame is logged at
//! WARN with its size and the cap, never its content. An over-cap frame cut
//! off by end of input gets no reply: the client has stopped sending, and
//! rmcp is already shutting the transport down. A frame that ends with its
//! newline just before end of input is answered: the reader reports the end
//! only once the replies already queued are written (for at most 5 s, so a
//! client that has stopped reading too cannot keep the session alive).
//!
//! ## How the reply reaches the client
//!
//! rmcp owns the output stream: its transport writes each message as one
//! newline-terminated frame through a `FramedWrite` over the writer it is
//! given. The reply is written alongside it without interleaving inside one
//! of rmcp's frames by a frame-level lock ([`capped_transport`]):
//!
//! * the real writer sits in a shared `tokio::sync::Mutex`;
//! * rmcp is given a [`FrameWriter`], which takes the lock on the first byte
//!   of a frame and holds it until a write ends on the frame's newline (rmcp
//!   writes compact JSON, so a newline only ever ends a frame), and holds it
//!   across any operation the writer has not yet completed (`Pending`), so
//!   a reply never takes the one waker a tokio writer stores for rmcp;
//! * the reader queues each reply line on a small channel (waiting for room,
//!   never dropping one), and one task per transport takes the lock, writes
//!   the whole line and flushes.
//!
//! So the two writers alternate by whole frames, and rmcp's backpressure is
//! unchanged (a reply waits for the lock like any frame). This was chosen
//! over a wrapping rmcp `Transport` that sends the reply through rmcp's own
//! writer because rmcp serialises a missing id by omitting it, and JSON-RPC
//! 2.0 asks for `"id": null`.
//!
//! # Unterminated last lines differ from the proxy, on purpose
//!
//! An unterminated last frame within the cap is handed on here, because
//! rmcp's `read_until` parses one at end of input and this reader must not
//! change what rmcp sees. The proxy's reader (`crate::mcp::proxy`'s
//! `read_frame`) drops it as torn, because a byte pipe must not manufacture a
//! frame boundary. Both are deliberate; do not align one with the other.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{ready, Context, Poll};

use tokio::io::{AsyncBufRead, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf};
use tokio::sync::{mpsc, Mutex, OwnedMutexGuard};

use super::frame_id::{too_large_reply, IdProbe};
use super::http_guards::MAX_HTTP_BODY_BYTES;

/// The longest frame, in bytes without its newline, a line-framed MCP
/// transport accepts: the HTTP body cap (4 MiB), so a request over it is
/// refused on every transport.
///
/// This bounds the whole request, and it is not above every request the
/// tools' own limits allow. A `lambo_derive` at its limits (64 concepts and
/// 256 `parent_of` pairs, every string at 16 KiB) is about 9.5 MB of JSON,
/// and a `lambo_derive_image` with a full image and 256 pairs about
/// 11.2 MB; a client that escapes non-ASCII as `\uXXXX` makes either up to
/// three times larger. Such a batch reaches this cap before its count and
/// string limits, and is refused with a `-32600` reply (see the module
/// docs). The cap stays at 4 MiB because it also bounds the `Value` tree
/// rmcp builds from a frame (tens of MiB at 4 MiB); a large batch is split.
pub(crate) const MAX_MCP_FRAME_BYTES: usize = MAX_HTTP_BODY_BYTES as usize;

/// What a frame buffer is shrunk back to after a large frame is handed on,
/// so a connection does not hold the cap's worth of memory for its life.
const RETAINED_FRAME_BYTES: usize = 64 * 1024;

/// How many replies may wait for the writer. Past it the reader waits for
/// room before reading on, as rmcp's own writes wait on a client that is
/// slow to read: no reply is dropped.
const REPLY_QUEUE: usize = 8;

/// How long end of input waits for replies already queued to be written
/// before it is reported. Bounded so a client that has stopped reading as
/// well as sending cannot keep the session from ending.
pub(super) const REPLY_DRAIN_LIMIT: std::time::Duration = std::time::Duration::from_secs(5);

/// A reply waiting for room on the reply queue.
type Reserving = Pin<
    Box<dyn Future<Output = Result<mpsc::OwnedPermit<String>, mpsc::error::SendError<()>>> + Send>,
>;

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
    /// `ready`, the complete frame being handed out.
    frame: Vec<u8>,
    /// How much of a `ready` frame has been handed out.
    handed: usize,
    ready: bool,
    /// Whether a `ready` frame's newline is still to be handed out. Kept as
    /// a flag, not pushed onto `frame`, so a frame exactly at the cap never
    /// grows its buffer past `cap + 1` bytes (#101 review L3).
    newline: bool,
    /// Non-zero while a frame over the cap is being discarded: its length
    /// so far. Its bytes are counted, never kept.
    over: usize,
    /// The id recovery for the frame being discarded.
    probe: Option<IdProbe>,
    /// Where the reply to a discarded frame goes; `None` for a reader with
    /// no writer to answer on (the tests of the reader alone).
    replies: Option<mpsc::Sender<String>>,
    /// The reply to the last discarded frame, while it waits for room on the
    /// queue; nothing more is read until it has it.
    pending: Option<(String, Reserving)>,
    /// The task that writes the replies, awaited at end of input so a reply
    /// already queued is written before rmcp sees the end and closes the
    /// transport (#101 review 2 L1).
    reply_task: Option<tokio::task::JoinHandle<()>>,
    /// When that wait gives up ([`REPLY_DRAIN_LIMIT`]).
    drain_deadline: Option<Pin<Box<tokio::time::Sleep>>>,
    /// The most bytes `frame` has held at once, and the most it has had
    /// room for, for the tests' proof that an oversized frame is never
    /// buffered.
    #[cfg(test)]
    peak: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    peak_capacity: Arc<std::sync::atomic::AtomicUsize>,
}

impl<R: AsyncRead + Unpin> CappedFrames<R> {
    /// Cap `inner` at [`MAX_MCP_FRAME_BYTES`], with no writer to answer a
    /// discarded frame on. `transport` names it in the log line.
    #[cfg(test)]
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
            newline: false,
            over: 0,
            probe: None,
            replies: None,
            pending: None,
            reply_task: None,
            drain_deadline: None,
            #[cfg(test)]
            peak: Arc::default(),
            #[cfg(test)]
            peak_capacity: Arc::default(),
        }
    }

    /// The most bytes this reader has buffered for one frame, shared so a
    /// test can read it after the reader has moved into a transport.
    #[cfg(test)]
    pub(crate) fn peak(&self) -> Arc<std::sync::atomic::AtomicUsize> {
        Arc::clone(&self.peak)
    }

    /// The largest the frame buffer's allocation has been.
    #[cfg(test)]
    pub(crate) fn peak_capacity(&self) -> Arc<std::sync::atomic::AtomicUsize> {
        Arc::clone(&self.peak_capacity)
    }

    /// The frame buffer's allocation now.
    #[cfg(test)]
    pub(crate) fn frame_capacity(&self) -> usize {
        self.frame.capacity()
    }

    #[cfg(test)]
    fn note_peak(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        self.peak.fetch_max(self.frame.len(), Relaxed);
        self.peak_capacity.fetch_max(self.frame.capacity(), Relaxed);
    }

    fn discarded(&mut self, terminated: bool) {
        let probe = self.probe.take();
        let id = probe.as_ref().and_then(IdProbe::finish);
        // A frame cut off by end of input is not answered: the client has
        // stopped sending and rmcp is shutting the transport down.
        let replying = terminated && self.replies.is_some();
        if replying && let Some(replies) = self.replies.clone() {
            self.pending = Some((
                too_large_reply(id.as_ref()),
                Box::pin(replies.reserve_owned()),
            ));
        }
        tracing::warn!(
            transport = self.transport,
            bytes = self.over,
            cap = self.cap,
            terminated,
            replying,
            id_recovered = id.is_some(),
            "lambo serve: a client frame over the size cap was discarded unread (`replying`: \
             whether the client is sent a request-too-large error)"
        );
        self.over = 0;
    }

    /// Queue the pending reply once there is room for it.
    fn poll_pending(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if let Some((reply, reserving)) = self.pending.as_mut() {
            // A closed queue means the reply task has gone, with the output.
            if let Ok(permit) = ready!(reserving.as_mut().poll(cx)) {
                permit.send(std::mem::take(reply));
            }
            self.pending = None;
        }
        Poll::Ready(())
    }

    /// At end of input: close the reply queue and wait, for at most
    /// [`REPLY_DRAIN_LIMIT`], until the reply task has written and flushed
    /// every reply already on it. Without this, rmcp could see the end and
    /// shut the writer down (or the process exit) before the reply to a
    /// frame that ended just before it was written.
    fn poll_drained(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        // Nothing more is read, so nothing more is queued; the task ends
        // once it has written what is on the queue.
        self.replies = None;
        if let Some(task) = self.reply_task.as_mut() {
            // The task is polled first, and the deadline made only if it is
            // still running: a task that has already ended (its writer
            // failed, or it drained the queue) needs no timer.
            if Pin::new(task).poll(cx).is_pending() {
                let deadline = self
                    .drain_deadline
                    .get_or_insert_with(|| Box::pin(tokio::time::sleep(REPLY_DRAIN_LIMIT)));
                if deadline.as_mut().poll(cx).is_pending() {
                    return Poll::Pending;
                }
            }
            self.reply_task = None;
            self.drain_deadline = None;
        }
        Poll::Ready(())
    }
}

/// Grow `frame` for `more` bytes, doubling, but never past `cap` while the
/// frame fits it: a frame exactly at the cap has a buffer of the cap, not of
/// the next power of two (#101 review L3).
fn grow(frame: &mut Vec<u8>, cap: usize, more: usize) {
    let need = frame.len() + more;
    if need > frame.capacity() {
        let target = (frame.capacity() * 2).max(need).min(cap.max(need));
        frame.reserve_exact(target - frame.len());
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
            ready!(this.poll_pending(cx));
            if this.ready {
                let rest = &this.frame[this.handed..];
                let n = rest.len().min(buf.remaining());
                buf.put_slice(&rest[..n]);
                this.handed += n;
                if this.handed == this.frame.len() && this.newline && buf.remaining() > 0 {
                    buf.put_slice(b"\n");
                    this.newline = false;
                }
                if this.handed == this.frame.len() && !this.newline {
                    this.frame.clear();
                    this.frame.shrink_to(RETAINED_FRAME_BYTES);
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
                ready!(this.poll_drained(cx));
                return Poll::Ready(Ok(()));
            }
            let (take, terminated) = match available.iter().position(|b| *b == b'\n') {
                Some(i) => (i, true),
                None => (available.len(), false),
            };
            if this.over > 0 {
                this.over = this.over.saturating_add(take);
                if let Some(probe) = this.probe.as_mut() {
                    probe.feed(&available[..take]);
                }
            } else if this.frame.len() + take > this.cap {
                // Past the cap: keep its head for the id, count the rest.
                let chunk = &available[..take];
                let room = this.cap - this.frame.len();
                grow(&mut this.frame, this.cap, room);
                this.frame.extend_from_slice(&chunk[..room]);
                let mut probe = IdProbe::new(&this.frame);
                probe.feed(&chunk[room..]);
                this.over = this.frame.len() + chunk.len() - room;
                this.probe = Some(probe);
                #[cfg(test)]
                this.note_peak();
                this.frame.clear();
                this.frame.shrink_to(RETAINED_FRAME_BYTES);
            } else {
                grow(&mut this.frame, this.cap, take);
                this.frame.extend_from_slice(&available[..take]);
                #[cfg(test)]
                this.note_peak();
            }
            Pin::new(&mut this.inner).consume(take + usize::from(terminated));
            if terminated {
                if this.over > 0 {
                    this.discarded(true);
                } else {
                    this.newline = true;
                    this.ready = true;
                }
            }
        }
    }
}

/// The write half rmcp is given: the shared writer, locked a frame at a
/// time. See the module docs.
///
/// The lock is held while a frame is part-written (`mid_frame`), and while
/// an operation the writer returned `Pending` for is still to complete. It
/// is let go once an operation completes at a frame boundary.
///
/// * Not on `Pending`, even at a boundary: the writer has stored rmcp's
///   waker, and tokio's writers keep exactly one (`Stdout`'s blocking
///   operation, a socket's writer slot, `DuplexStream`'s `write_waker`). A
///   reply polled on the writer meanwhile replaces it, and rmcp's send task,
///   which holds rmcp's own write mutex, is never woken again: every later
///   message on the transport stalls (#101 review 2 H1). The cost is that a
///   reply waits for the operation rmcp already has in flight.
/// * Not inferred from "the lock is held": `mid_frame` is set only by
///   writes, so a flush that completes at a boundary lets the lock go and
///   the reply does not wait for rmcp's next frame (the bug 71830a68 fixed).
///
/// If rmcp drops a send mid-frame and never writes to this writer again,
/// the lock stays held until the `FrameWriter` itself is dropped, which
/// releases it. rmcp abandons a send only when its service is already
/// ending (its send tasks are aborted at shutdown) or wedged, and it drops
/// the transport, and this writer with it, when the service ends. Until
/// then a reply waits, and end of input waits for it at most
/// [`REPLY_DRAIN_LIMIT`].
///
/// A writer dropped mid-frame leaves that frame's bytes unterminated on the
/// output. It marks the output torn (a flag shared with the reply task)
/// before it lets the lock go, and the reply task then writes a newline
/// ahead of its next reply, so the cut frame ends as one bad line of its
/// own and the reply arrives as a whole, parseable line (#101 review 3
/// L-A), rather than glued onto the cut bytes.
pub(crate) struct FrameWriter<W> {
    shared: Arc<Mutex<W>>,
    guard: Option<OwnedMutexGuard<W>>,
    locking: Option<Pin<Box<dyn Future<Output = OwnedMutexGuard<W>> + Send>>>,
    /// Bytes of a frame have been written and its newline has not.
    mid_frame: bool,
    /// Set when this writer is dropped mid-frame: the next reply starts
    /// with a newline to end the cut frame.
    torn: Arc<AtomicBool>,
}

impl<W> Drop for FrameWriter<W> {
    fn drop(&mut self) {
        // Runs before `guard` is dropped, so the reply task, which can only
        // write once it has the lock, sees the flag.
        if self.mid_frame {
            self.torn.store(true, Ordering::Release);
        }
    }
}

impl<W: AsyncWrite + Unpin + Send + 'static> FrameWriter<W> {
    fn new(shared: Arc<Mutex<W>>, torn: Arc<AtomicBool>) -> Self {
        Self {
            shared,
            guard: None,
            locking: None,
            mid_frame: false,
            torn,
        }
    }

    /// Run `op` on the writer under the lock, waiting for the lock if a
    /// reply has it, and let the lock go afterwards unless a frame is
    /// part-written.
    fn with_lock<T>(
        &mut self,
        cx: &mut Context<'_>,
        op: impl FnOnce(Pin<&mut W>, &mut Context<'_>) -> Poll<std::io::Result<T>>,
        after: impl FnOnce(&T) -> Option<bool>,
    ) -> Poll<std::io::Result<T>> {
        if self.guard.is_none() {
            let shared = &self.shared;
            let locking = self
                .locking
                .get_or_insert_with(|| Box::pin(Arc::clone(shared).lock_owned()));
            let guard = ready!(locking.as_mut().poll(cx));
            self.locking = None;
            self.guard = Some(guard);
        }
        let Some(guard) = self.guard.as_mut() else {
            unreachable!("the lock is held here");
        };
        let polled = op(Pin::new(&mut **guard), cx);
        match &polled {
            Poll::Ready(Ok(v)) => {
                if let Some(mid_frame) = after(v) {
                    self.mid_frame = mid_frame;
                }
            }
            // A failed writer has no frame left to protect.
            Poll::Ready(Err(_)) => self.mid_frame = false,
            Poll::Pending => {}
        }
        // Let go only once the operation is done. A writer that returned
        // `Pending` has stored this task's waker, and tokio's writers store
        // one: a reply polled on it now would replace that waker, and
        // rmcp's send would never be woken (#101 review 2 H1).
        if !self.mid_frame && polled.is_ready() {
            self.guard = None;
        }
        polled
    }
}

impl<W: AsyncWrite + Unpin + Send + 'static> AsyncWrite for FrameWriter<W> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.get_mut().with_lock(
            cx,
            |w, cx| w.poll_write(cx, buf),
            // A write that ends on a newline ends the frame.
            |n| (*n > 0).then(|| buf[*n - 1] != b'\n'),
        )
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.get_mut()
            .with_lock(cx, |w, cx| w.poll_flush(cx), |()| None)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.get_mut()
            .with_lock(cx, |w, cx| w.poll_shutdown(cx), |()| Some(false))
    }
}

/// A line-framed transport for rmcp with the frame cap in front of `read`
/// and the reply to a discarded frame written to `write` between rmcp's own
/// frames (see the module docs). Spawns the reply task, which ends when the
/// reader is dropped or reaches end of input; at end of input the reader
/// waits for it to write the replies already queued.
pub(crate) fn capped_transport<R, W>(
    read: R,
    write: W,
    transport: &'static str,
) -> (CappedFrames<R>, FrameWriter<W>)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    capped_transport_with_cap(read, write, transport, MAX_MCP_FRAME_BYTES)
}

/// [`capped_transport`] with a frame cap of `cap` bytes.
pub(crate) fn capped_transport_with_cap<R, W>(
    read: R,
    write: W,
    transport: &'static str,
    cap: usize,
) -> (CappedFrames<R>, FrameWriter<W>)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let shared = Arc::new(Mutex::new(write));
    let (tx, mut rx) = mpsc::channel::<String>(REPLY_QUEUE);
    let out = Arc::clone(&shared);
    let torn = Arc::new(AtomicBool::new(false));
    let torn_out = Arc::clone(&torn);
    let reply_task = tokio::spawn(async move {
        while let Some(reply) = rx.recv().await {
            let mut w = out.lock().await;
            let written = async {
                // rmcp's writer was dropped mid-frame: end the cut frame
                // so the reply is a line of its own.
                if torn_out.swap(false, Ordering::Acquire) {
                    w.write_all(b"\n").await?;
                }
                w.write_all(reply.as_bytes()).await?;
                w.write_all(b"\n").await?;
                w.flush().await
            }
            .await;
            if let Err(e) = written {
                tracing::debug!(
                    transport,
                    error = %e,
                    "lambo serve: could not write the reply to an oversized frame"
                );
                break;
            }
        }
    });
    let mut reader = CappedFrames::with_cap(read, transport, cap);
    reader.replies = Some(tx);
    reader.reply_task = Some(reply_task);
    (reader, FrameWriter::new(shared, torn))
}
