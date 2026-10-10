//! #101: the line-framed transports' frame cap ([`CappedFrames`]).
//!
//! The proof that an oversized argument is refused before it is allocated
//! in full is the reader's own high-water mark: everything rmcp buffers or
//! parses comes through it, so a frame it never holds more than the cap of
//! is a frame nothing downstream holds more of either. The oversized frames
//! here are streamed (a fixed 8 KiB chunk written again and again), never
//! built, and are eight times the cap; without the cap the high-water mark
//! is the whole frame.

use std::sync::atomic::Ordering;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use crate::mcp::serve::frames::{CappedFrames, MAX_MCP_FRAME_BYTES};

/// How far past the cap an oversized frame runs.
const OVER: usize = 8 * MAX_MCP_FRAME_BYTES;

/// Write `prefix`, then `filler` repeated (in 8 KiB chunks) until at least
/// `body` bytes of it have gone, then `suffix`: an oversized frame that is
/// never held in memory on the writing side either.
async fn stream_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    prefix: &str,
    filler: &str,
    body: usize,
    suffix: &str,
) {
    let chunk = filler.repeat(8192 / filler.len());
    w.write_all(prefix.as_bytes()).await.expect("prefix");
    let mut sent = 0;
    while sent < body {
        w.write_all(chunk.as_bytes()).await.expect("filler");
        sent += chunk.len();
    }
    w.write_all(suffix.as_bytes()).await.expect("suffix");
}

/// Read everything `CappedFrames` hands on from `input` under `cap`.
async fn through(input: &[u8], cap: usize) -> Vec<u8> {
    let mut capped = CappedFrames::with_cap(input, "stdio", cap);
    let mut out = Vec::new();
    capped.read_to_end(&mut out).await.expect("read");
    out
}

/// Frames within the cap reach rmcp byte for byte: `\r\n`, an empty line,
/// a frame exactly at the cap, and an unterminated last frame (rmcp's
/// `read_until` parses one at end of stream, so it must still arrive).
#[tokio::test]
async fn frames_within_the_cap_pass_byte_for_byte() {
    let at_cap = "x".repeat(16);
    let input = format!("{{\"a\":1}}\n{{\"b\":2}}\r\n\n{at_cap}\n{{\"tail\":3}}");
    assert_eq!(through(input.as_bytes(), 16).await, input.as_bytes());
    // And through a reader that hands out one byte at a time, so every
    // partial state is crossed.
    let mut capped = CappedFrames::with_cap(input.as_bytes(), "stdio", 16);
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = capped.read(&mut byte).await.expect("read");
        if n == 0 {
            break;
        }
        out.push(byte[0]);
    }
    assert_eq!(out, input.as_bytes());
}

/// One byte past the cap is a discarded frame; the frames around it pass.
///
/// Mutation: drop the cap comparison in `poll_read` and the long frame is
/// handed on.
#[tokio::test]
async fn a_frame_one_byte_over_the_cap_is_dropped_and_the_stream_resynchronises() {
    let input = format!("first\n{}\nnext\n", "y".repeat(17));
    assert_eq!(through(input.as_bytes(), 16).await, b"first\nnext\n");
    // Unterminated at end of stream: dropped, and the stream simply ends.
    let input = format!("first\n{}", "y".repeat(17));
    assert_eq!(through(input.as_bytes(), 16).await, b"first\n");
}

/// The proof at the reader: an oversized frame streamed at 8x the real cap
/// is never buffered past the cap, is logged by size and never by content,
/// and the frame after it arrives intact.
///
/// Mutation: drop the cap comparison and the high-water mark is the whole
/// 32 MiB frame (and the frame is handed on).
#[tokio::test]
async fn an_oversized_frame_is_discarded_without_being_buffered() {
    let (logs, _guard) = crate::test_util::capture_logs(tracing::Level::WARN);
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let mut capped = CappedFrames::new(server, "stdio");
    let peak = capped.peak();
    let writer = tokio::spawn(async move {
        stream_frame(&mut client, "{\"values\":\"", "zqzq", OVER, "\"}\n").await;
        client.write_all(b"{\"after\":1}\n").await.expect("after");
    });
    let mut out = Vec::new();
    let mut lines = BufReader::new(&mut capped);
    lines.read_until(b'\n', &mut out).await.expect("read");
    writer.await.expect("writer");
    let peak = peak.load(Ordering::Relaxed);
    assert!(
        peak <= MAX_MCP_FRAME_BYTES,
        "the reader buffered {peak} bytes of a {OVER}-byte frame (cap {MAX_MCP_FRAME_BYTES})"
    );
    assert!(
        out == b"{\"after\":1}\n",
        "only the frame after it arrives; got {} bytes starting {:?}",
        out.len(),
        String::from_utf8_lossy(&out[..out.len().min(40)])
    );
    let logged = logs.contents();
    assert!(
        logged.contains("over the size cap was discarded unread"),
        "{logged}"
    );
    // Its size (prefix, filler, suffix) and the cap, as fields.
    let size = "{\"values\":\"".len() + OVER + "\"}".len();
    assert!(logged.contains(&size.to_string()), "{logged}");
    assert!(
        logged.contains(&MAX_MCP_FRAME_BYTES.to_string()),
        "{logged}"
    );
    // The filler is a marker that cannot occur in a timestamp, a field name
    // or the message (#101 review L1: `0.5` matched `…:30.5…` timestamps).
    //
    // Mutation: add the frame's first bytes to the WARN and this fails.
    assert!(!logged.contains("zq"), "the log never quotes the frame");
}

/// #101 review L3: a frame exactly at the cap is handed on without the
/// buffer growing past the cap (pushing its newline used to double a full
/// 4 MiB allocation to 8 MiB), and a large frame's buffer is given back
/// once it has been handed on, so the connection does not keep it.
///
/// Mutations: push the newline onto the frame again, or extend without
/// `grow`, and the peak allocation is twice the cap; drop the `shrink_to`
/// and the retained capacity is the cap.
#[tokio::test]
async fn a_frame_at_the_cap_never_grows_the_buffer_past_it() {
    for cap in [16, MAX_MCP_FRAME_BYTES] {
        let at_cap = "x".repeat(cap);
        let input = format!("{at_cap}\n{{\"after\":1}}\n");
        let mut capped = CappedFrames::with_cap(input.as_bytes(), "stdio", cap);
        let peak = capped.peak_capacity();
        let mut out = Vec::new();
        let mut lines = BufReader::new(&mut capped);
        lines.read_until(b'\n', &mut out).await.expect("read");
        assert!(out.len() == cap + 1, "the frame at the cap arrives whole");
        out.clear();
        lines.read_until(b'\n', &mut out).await.expect("read");
        assert_eq!(out, b"{\"after\":1}\n");
        drop(lines);
        let peak = peak.load(Ordering::Relaxed);
        assert!(
            peak <= cap + 1,
            "cap {cap}: the frame buffer grew to {peak} bytes"
        );
        if cap > 64 * 1024 {
            assert!(
                capped.frame_capacity() <= 64 * 1024,
                "cap {cap}: {} bytes kept after the frame was handed on",
                capped.frame_capacity()
            );
        }
    }
}

/// #101 review M2: the reply to a discarded frame is written between the
/// server's own frames, never inside one, however the two writers' bytes
/// arrive. The server side writes its frames 7 bytes at a time and yields
/// between writes, through a pipe small enough that writes are partial,
/// while the reader discards a run of oversized requests; every line that
/// comes out must be one whole JSON message.
///
/// Mutation: release the frame lock after every write in
/// `FrameWriter::poll_write` (not only at a newline) and a reply lands
/// inside a server frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reply_never_lands_inside_a_frame_the_server_is_writing() {
    const FRAMES: usize = 20;
    let (mut client_in, server_in) = tokio::io::duplex(64 * 1024);
    let (server_out, client_out) = tokio::io::duplex(512);
    let (mut reader, mut writer) =
        crate::mcp::serve::frames::capped_transport_with_cap(server_in, server_out, "stdio", 64);
    let server_writes = tokio::spawn(async move {
        for i in 0..FRAMES {
            let frame = format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":{i},\"result\":\"{}\"}}\n",
                "r".repeat(2000)
            );
            for chunk in frame.as_bytes().chunks(7) {
                writer.write_all(chunk).await.expect("write");
                tokio::task::yield_now().await;
            }
            writer.flush().await.expect("flush");
        }
        writer
    });
    let client_writes = tokio::spawn(async move {
        for i in 0..FRAMES {
            let frame = format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":\"o{i}\",\"method\":\"x\",\"params\":\"{}\"}}\n",
                "p".repeat(300)
            );
            client_in.write_all(frame.as_bytes()).await.expect("send");
        }
    });
    let reads = tokio::spawn(async move {
        let mut sink = Vec::new();
        reader.read_to_end(&mut sink).await.expect("read");
        assert!(sink.is_empty(), "every request was over the cap");
        reader
    });
    let mut lines = BufReader::new(client_out).lines();
    let (mut results, mut replies) = (0, 0);
    while results + replies < 2 * FRAMES {
        let line = tokio::time::timeout(std::time::Duration::from_secs(20), lines.next_line())
            .await
            .expect("output within 20 s")
            .expect("read")
            .expect("not closed");
        let v: serde_json::Value = serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("a frame was torn ({e}): {line:.120}"));
        if v.get("result").is_some() {
            results += 1;
        } else {
            assert_eq!(v["error"]["code"], -32600, "{line:.120}");
            assert!(
                v["id"].as_str().is_some_and(|id| id.starts_with('o')),
                "{v}"
            );
            replies += 1;
        }
    }
    client_writes.await.expect("client");
    drop(server_writes.await.expect("server"));
    drop(reads.await.expect("reader"));
}

/// A writer whose first flush is not ready at once, as tokio's `Stdout`
/// often is not (its writes run on the blocking pool). It wakes the task
/// itself, standing in for the blocking operation finishing; that is why
/// it cannot catch a lost waker, which
/// `a_reply_does_not_take_the_waker_of_a_server_write_parked_at_a_boundary`
/// covers with a real writer.
struct SlowFirstFlush {
    inner: tokio::io::DuplexStream,
    flushes: usize,
}

impl AsyncWrite for SlowFirstFlush {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.flushes += 1;
        if self.flushes == 1 {
            cx.waker().wake_by_ref();
            return std::task::Poll::Pending;
        }
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// A flush that is pending at a frame boundary keeps the frame lock only
/// until it completes, not afterwards: the reply to a discarded frame goes
/// out while the server writes nothing more. (Found by the stdio binary
/// test, where the reply sometimes waited for rmcp's next frame.)
///
/// Mutation: keep the lock after the flush completes at a boundary (treat
/// a held lock as mid-frame, as the first version did) and the reply never
/// arrives.
#[tokio::test]
async fn a_pending_flush_does_not_keep_the_reply_waiting() {
    let input = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"x\",\"params\":\"{}\"}}\n",
        "p".repeat(64)
    );
    let (server_out, client_out) = tokio::io::duplex(4096);
    let out = SlowFirstFlush {
        inner: server_out,
        flushes: 0,
    };
    let (mut reader, mut writer) =
        crate::mcp::serve::frames::capped_transport_with_cap(input.as_bytes(), out, "stdio", 16);
    writer.write_all(b"{\"a\":1}\n").await.expect("write");
    writer.flush().await.expect("flush");
    let mut sink = Vec::new();
    reader.read_to_end(&mut sink).await.expect("read");
    let mut lines = BufReader::new(client_out).lines();
    let first = lines.next_line().await.expect("read").expect("a frame");
    assert_eq!(first, "{\"a\":1}");
    let reply = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
        .await
        .expect("the reply arrives while the server writes nothing more")
        .expect("read")
        .expect("a frame");
    assert!(
        reply.contains("-32600") && reply.contains("\"id\":1"),
        "{reply}"
    );
    drop(writer);
}

/// #101 review 2 H1: a server write that is waiting on a full writer at a
/// frame boundary keeps the frame lock until it completes, so a reply
/// cannot poll the writer meanwhile and take the one waker it stores.
///
/// The output pipe is 64 bytes. The first frame fills it exactly, so the
/// server's next frame parks at the boundary with its waker stored in the
/// pipe; then a reply is queued and the reply task tries the writer too.
/// Once the client reads, the server's write must be woken and finish.
/// tokio's writers (`DuplexStream`, a socket's write half, `Stdout`) all
/// keep a single write waker, and `DuplexStream` is used here as it is,
/// with no test writer that wakes itself.
///
/// Mutation: let the lock go on `Pending` at a boundary in
/// `FrameWriter::with_lock` (drop `polled.is_ready()`) and the reply task
/// replaces the server's waker: the server's write is never woken and
/// this fails at its 5 s timeout, every run.
#[tokio::test]
async fn a_reply_does_not_take_the_waker_of_a_server_write_parked_at_a_boundary() {
    let (mut client_in, server_in) = tokio::io::duplex(64 * 1024);
    let (server_out, client_out) = tokio::io::duplex(64);
    let (mut reader, mut writer) =
        crate::mcp::serve::frames::capped_transport_with_cap(server_in, server_out, "stdio", 16);
    let first = format!("{{\"a\":\"{}\"}}", "x".repeat(55));
    assert_eq!(first.len() + 1, 64, "the first frame fills the pipe");
    writer
        .write_all(format!("{first}\n").as_bytes())
        .await
        .expect("write");
    writer.flush().await.expect("flush");
    // The next frame parks: the pipe is full, at a frame boundary.
    let server = tokio::spawn(async move {
        writer.write_all(b"{\"b\":2}\n").await.expect("write");
        writer.flush().await.expect("flush");
        writer
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    // A reply is queued, and the reply task goes for the writer.
    let request = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"x\",\"params\":\"{}\"}}\n",
        "p".repeat(64)
    );
    client_in.write_all(request.as_bytes()).await.expect("send");
    drop(client_in);
    let reads = tokio::spawn(async move {
        let mut sink = Vec::new();
        reader.read_to_end(&mut sink).await.expect("read");
        assert!(sink.is_empty(), "the request was over the cap");
        reader
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    // The client reads.
    let drain = tokio::spawn(async move {
        let mut lines = BufReader::new(client_out).lines();
        let mut got = Vec::new();
        while got.len() < 3 {
            match tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line()).await {
                Ok(Ok(Some(line))) => got.push(line),
                _ => break,
            }
        }
        got
    });
    let writer = tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .expect("the server's parked write is woken once the client reads (a reply took its waker)")
        .expect("server");
    let got = drain.await.expect("drain");
    assert_eq!(got.len(), 3, "{got:?}");
    assert_eq!(got[0], first);
    assert_eq!(
        got[1], "{\"b\":2}",
        "the parked frame goes before the reply"
    );
    assert!(
        got[2].contains("-32600") && got[2].contains("\"id\":7"),
        "{}",
        got[2]
    );
    drop(reads.await.expect("reader"));
    drop(writer);
}

/// #101 review 2 L1: a complete over-cap frame followed at once by end of
/// input is still answered. The reader reports the end only once the reply
/// is written, so the writer shut down straight after it (as rmcp closes
/// the transport at end of input) cannot cut the reply off.
///
/// Mutation: report end of input without `poll_drained` and the shutdown
/// runs before the reply task writes; the reply is lost.
#[tokio::test]
async fn a_reply_queued_just_before_end_of_input_is_written_before_the_end() {
    let input = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"x\",\"params\":\"{}\"}}\n",
        "p".repeat(64)
    );
    let (server_out, mut client_out) = tokio::io::duplex(4096);
    let (mut reader, mut writer) = crate::mcp::serve::frames::capped_transport_with_cap(
        input.as_bytes(),
        server_out,
        "stdio",
        16,
    );
    let mut sink = Vec::new();
    reader.read_to_end(&mut sink).await.expect("read");
    assert!(sink.is_empty(), "the request was over the cap");
    writer.shutdown().await.expect("shutdown");
    let mut out = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client_out.read_to_end(&mut out),
    )
    .await
    .expect("the output ends")
    .expect("read");
    let out = String::from_utf8(out).expect("utf-8");
    assert!(
        out.contains("-32600") && out.contains("\"id\":3") && out.ends_with('\n'),
        "{out:?}"
    );
    drop(reader);
}

/// #101 review 2 L2: a frame rmcp abandons part-written keeps the frame
/// lock (nothing else can tell it will not be finished), and dropping the
/// `FrameWriter`, as rmcp does with its transport when its service ends,
/// lets the lock go, so the queued reply is written.
///
/// Mutation: keep the guard alive past the writer (leak it on drop) and
/// the reply never arrives.
#[tokio::test]
async fn dropping_the_writer_mid_frame_lets_the_reply_through() {
    let input = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"x\",\"params\":\"{}\"}}\n",
        "p".repeat(64)
    );
    let (server_out, client_out) = tokio::io::duplex(4096);
    let (mut reader, mut writer) = crate::mcp::serve::frames::capped_transport_with_cap(
        std::io::Cursor::new(input.into_bytes()),
        server_out,
        "stdio",
        16,
    );
    writer.write_all(b"{\"cut\":").await.expect("write");
    let reads = tokio::spawn(async move {
        let mut sink = Vec::new();
        reader.read_to_end(&mut sink).await.expect("read");
        reader
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(!reads.is_finished(), "the reply waits for the frame lock");
    drop(writer);
    let mut lines = BufReader::new(client_out).lines();
    let line = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
        .await
        .expect("the reply is written once the writer is dropped")
        .expect("read")
        .expect("a line");
    // The abandoned bytes come first: the reply follows them, unsplit.
    let reply = line.strip_prefix("{\"cut\":").expect("the cut frame first");
    assert!(
        reply.contains("-32600") && reply.contains("\"id\":4"),
        "{reply}"
    );
    drop(reads.await.expect("reader"));
}

/// An over-cap frame cut off by end of input gets no reply: the client has
/// stopped sending, and its transport is shutting down.
///
/// Mutation: reply in `discarded` whatever `terminated` is, queued at once
/// (`try_send`) rather than on the next read, and a reply comes out.
#[tokio::test]
async fn an_oversized_frame_cut_off_by_end_of_input_gets_no_reply() {
    let input = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"x\",\"params\":\"{}",
        "p".repeat(300)
    );
    let (server_out, mut client_out) = tokio::io::duplex(4096);
    let (mut reader, writer) = crate::mcp::serve::frames::capped_transport_with_cap(
        input.as_bytes(),
        server_out,
        "stdio",
        64,
    );
    let mut sink = Vec::new();
    reader.read_to_end(&mut sink).await.expect("read");
    assert!(sink.is_empty());
    drop(reader);
    drop(writer);
    let mut out = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client_out.read_to_end(&mut out),
    )
    .await
    .expect("the reply task ends with its reader")
    .expect("read");
    assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));
}

/// The real tool path: rmcp's stdio transport over [`CappedFrames`] (what
/// `capped_stdio` builds, with in-memory pipes in place of stdin and stdout)
/// in front of a `LamboServer` that serves all three fields.
#[cfg(all(feature = "store-memory", feature = "embed-fixture"))]
mod tool_path {
    use super::*;
    use crate::embed::{Embedder, FixtureEmbedder};
    use crate::mcp::serve::frame_id::{TOO_LARGE_CODE, TOO_LARGE_MESSAGE};
    use crate::mcp::serve::frames::capped_transport;
    use crate::mcp::server::LamboServer;
    use crate::memory::Memory;
    use crate::store::{GraphStore, MemoryStore};
    use crate::surface::image::{MAX_IMAGE_B64_LEN, MAX_VECTOR_VALUES};
    use crate::types::EmbeddingContract;
    use crate::Config;
    use rmcp::ServiceExt;
    use serde_json::json;
    use std::sync::Arc;
    use std::time::Duration;

    const CONTRACT: &str = r#"{"kind":"fixture","dim":1024}"#;

    /// The prefix and suffix around a `values` array or `data` string for
    /// each of the three capped fields, as `tools/call` frames with `id`.
    fn field_frame(field: &str, id: u64) -> (String, &'static str, String) {
        let call = |tool: &str, args: &str| {
            format!(
                r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{tool}","arguments":{{"agent_id":"agent-a",{args}"#
            )
        };
        match field {
            "query_vector.values" => (
                call(
                    "lambo_recall",
                    &format!(r#""query_vector":{{"contract":{CONTRACT},"values":["#),
                ),
                "0.5,",
                "0.5]}}}}\n".to_owned(),
            ),
            "vector.values" => (
                call(
                    "lambo_derive_image",
                    &format!(
                        r#""caption":"c","concept_type":"resource","vector":{{"contract":{CONTRACT},"values":["#
                    ),
                ),
                "0.5,",
                "0.5]}}}}\n".to_owned(),
            ),
            "image.data" => (
                call(
                    "lambo_derive_image",
                    r#""caption":"c","concept_type":"resource","image":{"mime":"image/png","data":""#,
                ),
                "QUJD",
                "\"}}}}\n".to_owned(),
            ),
            other => panic!("no frame for {other}"),
        }
    }

    /// A server that reaches every field's cap: client vectors accepted, an
    /// image embedder, a store that searches vectors.
    async fn server() -> LamboServer {
        let store: Arc<dyn GraphStore> = Arc::new(crate::test_util::VectorSearchable(Arc::new(
            MemoryStore::new(),
        )));
        let mem = Memory::builder()
            .session("frames-tool-path")
            .agent("agent-a")
            .config(Config {
                accept_client_vectors: true,
                ..Config::default()
            })
            .flush_interval(Duration::from_secs(3_600))
            .store(store)
            .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
            .embedding_contract(EmbeddingContract {
                kind: "fixture".into(),
                model: None,
                dim: 1024,
            })
            .build()
            .await
            .expect("build");
        LamboServer::new(Arc::new(mem))
    }

    type Writer = Box<dyn tokio::io::AsyncWrite + Unpin + Send>;
    type Reader = Box<dyn tokio::io::AsyncRead + Unpin + Send>;

    struct Client {
        to_server: Writer,
        from_server: tokio::io::Lines<BufReader<Reader>>,
        /// Every frame the server has sent so far.
        seen: Vec<String>,
        /// The capped reader's high-water mark, where the test can reach it.
        peak: Option<Arc<std::sync::atomic::AtomicUsize>>,
    }

    impl Client {
        /// Serve a fresh server over capped in-memory stdio and complete the
        /// MCP handshake.
        async fn connect() -> Self {
            let (to_server, server_in) = tokio::io::duplex(64 * 1024);
            let (server_out, from_server) = tokio::io::duplex(64 * 1024);
            let (capped, out) = capped_transport(server_in, server_out, "stdio");
            let peak = capped.peak();
            let server = server().await;
            tokio::spawn(async move {
                if let Ok(running) = server.serve((capped, out)).await {
                    let _ = running.waiting().await;
                }
            });
            Self::handshake(Box::new(to_server), Box::new(from_server), Some(peak)).await
        }

        /// Dial a session endpoint the hub serves and complete the MCP
        /// handshake. The hub builds its own capped reader, so there is no
        /// high-water mark to read here.
        #[cfg(unix)]
        async fn dial(endpoint: &crate::mcp::SessionEndpoint) -> Self {
            let stream = tokio::net::UnixStream::connect(endpoint.path())
                .await
                .expect("dial the endpoint");
            let (read, write) = stream.into_split();
            Self::handshake(Box::new(write), Box::new(read), None).await
        }

        async fn handshake(
            to_server: Writer,
            from_server: Reader,
            peak: Option<Arc<std::sync::atomic::AtomicUsize>>,
        ) -> Self {
            let mut client = Self {
                to_server,
                from_server: BufReader::new(from_server).lines(),
                seen: Vec::new(),
                peak,
            };
            client
                .send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"frames","version":"1"}}}"#)
                .await;
            client.response(1).await;
            client
                .send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .await;
            client
        }

        async fn send(&mut self, frame: &str) {
            self.to_server
                .write_all(frame.as_bytes())
                .await
                .expect("send");
            self.to_server.write_all(b"\n").await.expect("newline");
        }

        /// The response to request `id`, recording every frame read on the
        /// way.
        async fn response(&mut self, id: u64) -> String {
            let needle = format!("\"id\":{id}");
            loop {
                let line =
                    tokio::time::timeout(Duration::from_secs(60), self.from_server.next_line())
                        .await
                        .unwrap_or_else(|_| panic!("no response to {id} within 60 s"))
                        .expect("read")
                        .unwrap_or_else(|| panic!("the server closed before answering {id}"));
                self.seen.push(line.clone());
                if line.contains(&needle) {
                    return line;
                }
            }
        }

        fn peak(&self) -> usize {
            self.peak
                .as_ref()
                .expect("a capped reader this test built")
                .load(Ordering::Relaxed)
        }

        /// Stream `field` at 8x the frame cap as request 10 and assert it is
        /// refused with a request-too-large error keyed to 10.
        async fn oversized_field_is_refused(&mut self, field: &str) {
            let (prefix, filler, suffix) = field_frame(field, 10);
            self.oversized_frame_is_refused(field, &prefix, filler, &suffix, &json!(10), 11)
                .await;
        }

        /// Stream a frame of `prefix`, `filler` to 8x the frame cap, and
        /// `suffix`, then make two small calls (`next` and `next + 1`), and
        /// assert: exactly one request-too-large error came back, keyed to
        /// `id` (or `null`); nothing else answered the frame (it was not
        /// parsed, so no tool saw it); the session answered both calls; and
        /// the reader never held more than the cap of it.
        async fn oversized_frame_is_refused(
            &mut self,
            case: &str,
            prefix: &str,
            filler: &str,
            suffix: &str,
            id: &serde_json::Value,
            next: u64,
        ) {
            self.seen.clear();
            stream_frame(&mut self.to_server, prefix, filler, OVER, suffix).await;
            self.send(&format!(
                r#"{{"jsonrpc":"2.0","id":{next},"method":"tools/list"}}"#
            ))
            .await;
            let listed = self.response(next).await;
            assert!(listed.contains("lambo_derive_image"), "{case}: {listed}");
            self.send(&format!(
                r#"{{"jsonrpc":"2.0","id":{},"method":"ping"}}"#,
                next + 1
            ))
            .await;
            self.response(next + 1).await;
            // The ping can overtake a tool call rmcp has spawned, so give a
            // parsed frame time to be answered before concluding it was not.
            while let Ok(Ok(Some(line))) =
                tokio::time::timeout(Duration::from_secs(1), self.from_server.next_line()).await
            {
                self.seen.push(line);
            }
            if self.peak.is_some() {
                let peak = self.peak();
                assert!(
                    peak <= MAX_MCP_FRAME_BYTES,
                    "{case}: {peak} bytes of a {OVER}-byte frame were buffered"
                );
            }
            let frames: Vec<serde_json::Value> = self
                .seen
                .iter()
                .map(|l| serde_json::from_str(l).expect("every frame out is JSON"))
                .collect();
            let refusals: Vec<&serde_json::Value> = frames
                .iter()
                .filter(|f| f["error"]["code"] == json!(TOO_LARGE_CODE))
                .collect();
            let short = || {
                self.seen
                    .iter()
                    .map(|l| l.chars().take(200).collect::<String>())
                    .collect::<Vec<_>>()
            };
            assert!(
                refusals.len() == 1,
                "{case}: expected one request-too-large reply: {:?}",
                short()
            );
            let refusal = refusals[0];
            assert!(
                refusal.get("id") == Some(id)
                    && refusal["error"]["message"] == json!(TOO_LARGE_MESSAGE),
                "{case}: the reply must be keyed to {id}: {refusal}"
            );
            if !id.is_null() {
                assert!(
                    frames.iter().filter(|f| f.get("id") == Some(id)).count() == 1,
                    "{case}: the oversized frame was answered, so it was parsed: {:?}",
                    short()
                );
            }
        }
    }

    /// The shapes of id an oversized request can carry, with the id the
    /// reply must be keyed to: before `params` (serde clients), after it
    /// (the TypeScript SDK), none, a string, and one only inside nested
    /// objects, which must not be taken for the request's.
    pub(super) fn id_shapes() -> Vec<(&'static str, String, String, serde_json::Value)> {
        const ARGS: &str = r#""name":"lambo_derive_image","arguments":{"agent_id":"agent-a","caption":"c","concept_type":"resource","image":{"mime":"image/png","data":""#;
        vec![
            (
                "id before params",
                format!(r#"{{"jsonrpc":"2.0","id":40,"method":"tools/call","params":{{{ARGS}"#),
                "\"}}}}\n".to_owned(),
                json!(40),
            ),
            (
                "id after params",
                format!(r#"{{"method":"tools/call","params":{{{ARGS}"#),
                "\"}}},\"jsonrpc\":\"2.0\",\"id\":41}\n".to_owned(),
                json!(41),
            ),
            (
                "no id",
                format!(r#"{{"jsonrpc":"2.0","method":"tools/call","params":{{{ARGS}"#),
                "\"}}}}\n".to_owned(),
                serde_json::Value::Null,
            ),
            (
                "string id",
                format!(
                    r#"{{"jsonrpc":"2.0","id":"big-42","method":"tools/call","params":{{{ARGS}"#
                ),
                "\"}}}}\n".to_owned(),
                json!("big-42"),
            ),
            (
                "id only nested",
                format!(r#"{{"jsonrpc":"2.0","method":"tools/call","params":{{"id":43,{ARGS}"#),
                "\"},\"id\":44},\"id\":45}}\n".to_owned(),
                serde_json::Value::Null,
            ),
        ]
    }

    /// Each capped field, streamed at 8x the frame cap through the real
    /// tool path: the frame is never buffered past the cap (so neither
    /// rmcp's line nor its `Value` tree nor the tool's `String`/`Vec<f32>`
    /// is ever built from it), it is refused with a request-too-large error
    /// keyed to its id rather than answered by the tool, and the session
    /// goes on to answer the next call.
    ///
    /// Mutations: drop the cap comparison in `CappedFrames::poll_read` and
    /// the high-water mark is the whole frame, which rmcp then parses and
    /// the tool answers by name; drop the reply in `discarded` and no
    /// request-too-large error arrives.
    #[tokio::test]
    async fn an_oversized_field_is_refused_before_it_is_allocated() {
        for field in ["query_vector.values", "vector.values", "image.data"] {
            let mut client = Client::connect().await;
            client.oversized_field_is_refused(field).await;
        }
    }

    /// #101 review M2 over stdio: an oversized request is answered with
    /// `-32600` keyed to the id recovered from its head or tail, or to
    /// `null`, never to an id nested inside it; every case on one
    /// connection, which keeps serving after each.
    ///
    /// Mutations: scan only the head (drop the tail ring) and "id after
    /// params" is answered with `null`; let the head scan descend into
    /// `params` and "id only nested" is answered with 43. (The frame lock
    /// that keeps the reply out of rmcp's frames is proved by
    /// `a_reply_never_lands_inside_a_frame_the_server_is_writing`.)
    #[tokio::test]
    async fn an_oversized_request_is_answered_with_its_id_over_stdio() {
        let mut client = Client::connect().await;
        for (n, (case, prefix, suffix, id)) in (50u64..).step_by(2).zip(id_shapes()) {
            client
                .oversized_frame_is_refused(case, &prefix, "QUJD", &suffix, &id, n)
                .await;
        }
    }

    /// Under the frame cap nothing changes: a field over its own cap is
    /// still refused by the tool, as a tool error naming the cap, with
    /// today's message and without echoing the value.
    #[tokio::test]
    async fn a_field_over_its_cap_in_a_frame_under_the_cap_is_refused_as_today() {
        let mut client = Client::connect().await;
        let values = vec!["0.5"; MAX_VECTOR_VALUES + 1].join(",");
        let data = "A".repeat(MAX_IMAGE_B64_LEN + 4);
        let cases = [
            (
                "query_vector.values",
                values.as_str(),
                format!(
                    "query_vector.values has {} components, over the limit of {MAX_VECTOR_VALUES}",
                    MAX_VECTOR_VALUES + 1
                ),
            ),
            (
                "vector.values",
                values.as_str(),
                format!(
                    "vector.values has {} components, over the limit of {MAX_VECTOR_VALUES}",
                    MAX_VECTOR_VALUES + 1
                ),
            ),
            (
                "image.data",
                data.as_str(),
                format!(
                    "image.data is {} characters, over the {MAX_IMAGE_B64_LEN}-character limit",
                    MAX_IMAGE_B64_LEN + 4
                ),
            ),
        ];
        for (id, (field, body, expected)) in (20u64..).zip(cases) {
            let (prefix, _, suffix) = field_frame(field, id);
            // The body replaces the trailing element the suffix closes with.
            let suffix = suffix
                .strip_prefix("0.5")
                .or_else(|| suffix.strip_prefix(""))
                .expect("suffix");
            let frame = format!("{prefix}{body}{suffix}");
            assert!(
                frame.len() < MAX_MCP_FRAME_BYTES,
                "{field}: under the frame cap"
            );
            client.send(frame.trim_end()).await;
            let answer = client.response(id).await;
            assert!(
                answer.contains("\"isError\":true"),
                "{field}: {answer:.300}"
            );
            assert!(answer.contains(&expected), "{field}: {answer:.300}");
            assert!(answer.len() < 4096, "{field}: the refusal echoes nothing");
        }
    }

    /// The session endpoint (the J2 hub socket) is the same rmcp line
    /// transport and gets the same cap: each oversized field is refused
    /// there too with a request-too-large error keyed to its id, as is every
    /// id shape, and the connection goes on serving.
    ///
    /// Mutations: serve the endpoint connection without `capped_endpoint`
    /// (`server.serve(stream)`, as before #101) and rmcp parses the frame
    /// and the tool answers request 10; give `capped_endpoint` a reader with
    /// no reply channel and no request-too-large error arrives.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_endpoint_discards_an_oversized_frame_too() {
        let dir = crate::test_util::ScratchDir::short("fc");
        let store = crate::store::StoreConfig {
            kind: crate::store::StoreKind::Sqlite,
            path: Some(dir.join("s.db").to_str().expect("utf-8").into()),
            ..crate::store::StoreConfig::default()
        };
        let endpoint =
            crate::mcp::SessionEndpoint::resolve_in(&dir.join("run"), "frames-hub", &store)
                .expect("endpoint fits");
        let server = server().await;
        let mem = Arc::clone(server.memory());
        let hub = crate::mcp::serve::hub::bind_hub(Some(&endpoint), &server, 4);
        for field in ["query_vector.values", "vector.values", "image.data"] {
            let mut client = Client::dial(&endpoint).await;
            client.oversized_field_is_refused(field).await;
        }
        // #101 review M2: every id shape, on one endpoint connection.
        let mut client = Client::dial(&endpoint).await;
        for (n, (case, prefix, suffix, id)) in (50u64..).step_by(2).zip(id_shapes()) {
            client
                .oversized_frame_is_refused(case, &prefix, "QUJD", &suffix, &id, n)
                .await;
        }
        drop(client);
        hub.release(Some(&endpoint)).await;
        mem.close().await.expect("close");
    }

    /// #101 review 2 H1 end to end: a reply queued while the endpoint
    /// socket is backed up does not strand rmcp's output.
    ///
    /// The client sends pings and reads nothing until the socket is full.
    /// rmcp's answers are small, and a small write to a full Unix stream
    /// socket is refused whole, so rmcp parks at a frame boundary with its
    /// waker in the socket's one writer slot. An oversized request then
    /// queues a reply. Once the client reads, every ping, the reply and a
    /// last ping must all be answered.
    ///
    /// Mutation: let the frame lock go on `Pending` at a boundary in
    /// `FrameWriter::with_lock` and the reply task takes rmcp's waker: the
    /// last ping is never answered.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_reply_queued_while_the_endpoint_is_backed_up_does_not_stall_it() {
        const PINGS: u64 = 5_000;
        let dir = crate::test_util::ScratchDir::short("fb");
        let store = crate::store::StoreConfig {
            kind: crate::store::StoreKind::Sqlite,
            path: Some(dir.join("s.db").to_str().expect("utf-8").into()),
            ..crate::store::StoreConfig::default()
        };
        let endpoint =
            crate::mcp::SessionEndpoint::resolve_in(&dir.join("run"), "frames-backed", &store)
                .expect("endpoint fits");
        let server = server().await;
        let mem = Arc::clone(server.memory());
        let hub = crate::mcp::serve::hub::bind_hub(Some(&endpoint), &server, 4);
        let mut client = Client::dial(&endpoint).await;
        for id in 1_000..1_000 + PINGS {
            client
                .send(&format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"ping"}}"#))
                .await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        let (prefix, filler, suffix) = field_frame("image.data", 10);
        tokio::time::timeout(
            Duration::from_secs(60),
            stream_frame(&mut client.to_server, &prefix, filler, OVER, &suffix),
        )
        .await
        .expect("the server reads on while its output is backed up");
        client
            .send(r#"{"jsonrpc":"2.0","id":99,"method":"ping"}"#)
            .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let (mut pongs, mut refused, mut last) = (0, 0, false);
        while !(last && refused == 1 && pongs == PINGS) {
            let line =
                tokio::time::timeout(Duration::from_secs(20), client.from_server.next_line())
                    .await
                    .unwrap_or_else(|_| {
                        panic!(
                            "output stalled: {pongs} of {PINGS} pings, {refused} replies, \
                         last ping answered: {last}"
                        )
                    })
                    .expect("read")
                    .expect("not closed");
            let v: serde_json::Value = serde_json::from_str(&line).expect("whole frames");
            if v["error"]["code"] == json!(TOO_LARGE_CODE) {
                assert_eq!(v["id"], json!(10), "{line}");
                refused += 1;
            } else if v["id"] == json!(99) {
                last = true;
            } else {
                pongs += 1;
            }
        }
        drop(client);
        hub.release(Some(&endpoint)).await;
        mem.close().await.expect("close");
    }
}
