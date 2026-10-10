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

/// The real tool path: rmcp's stdio transport over [`CappedFrames`] (what
/// `capped_stdio` builds, with in-memory pipes in place of stdin and stdout)
/// in front of a `LamboServer` that serves all three fields.
#[cfg(all(feature = "store-memory", feature = "embed-fixture"))]
mod tool_path {
    use super::*;
    use crate::embed::{Embedder, FixtureEmbedder};
    use crate::mcp::server::LamboServer;
    use crate::memory::Memory;
    use crate::store::{GraphStore, MemoryStore};
    use crate::surface::image::{MAX_IMAGE_B64_LEN, MAX_VECTOR_VALUES};
    use crate::types::EmbeddingContract;
    use crate::Config;
    use rmcp::ServiceExt;
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
            let capped = CappedFrames::new(server_in, "stdio");
            let peak = capped.peak();
            let server = server().await;
            tokio::spawn(async move {
                if let Ok(running) = server.serve((capped, server_out)).await {
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

        /// Stream `field` at 8x the frame cap as request 10, then make two
        /// small calls, and assert the session answered both and never
        /// answered request 10: the frame was not parsed, so no tool saw it.
        async fn oversized_frame_goes_unanswered(&mut self, field: &str) {
            let (prefix, filler, suffix) = field_frame(field, 10);
            stream_frame(&mut self.to_server, &prefix, filler, OVER, &suffix).await;
            self.send(r#"{"jsonrpc":"2.0","id":11,"method":"tools/list"}"#)
                .await;
            let listed = self.response(11).await;
            assert!(listed.contains("lambo_derive_image"), "{field}: {listed}");
            self.send(r#"{"jsonrpc":"2.0","id":12,"method":"ping"}"#)
                .await;
            self.response(12).await;
            // The ping can overtake a tool call rmcp has spawned, so give a
            // parsed request 10 time to be answered before concluding it
            // was not.
            while let Ok(Ok(Some(line))) =
                tokio::time::timeout(Duration::from_secs(1), self.from_server.next_line()).await
            {
                self.seen.push(line);
            }
            if self.peak.is_some() {
                let peak = self.peak();
                assert!(
                    peak <= MAX_MCP_FRAME_BYTES,
                    "{field}: {peak} bytes of a {OVER}-byte frame were buffered"
                );
            }
            assert!(
                !self.seen.iter().any(|l| l.contains("\"id\":10")),
                "{field}: the oversized frame was answered, so it was parsed: {:?}",
                self.seen
                    .iter()
                    .map(|l| l.chars().take(200).collect::<String>())
                    .collect::<Vec<_>>()
            );
        }
    }

    /// Each capped field, streamed at 8x the frame cap through the real
    /// tool path: the frame is never buffered past the cap (so neither
    /// rmcp's line nor its `Value` tree nor the tool's `String`/`Vec<f32>`
    /// is ever built from it), it is never answered, and the session goes
    /// on to answer the next call.
    ///
    /// Mutations: drop the cap comparison in `CappedFrames::poll_read` and
    /// the high-water mark is the whole frame, which rmcp then parses and
    /// the tool answers by name.
    #[tokio::test]
    async fn an_oversized_field_is_refused_before_it_is_allocated() {
        for field in ["query_vector.values", "vector.values", "image.data"] {
            let mut client = Client::connect().await;
            client.oversized_frame_goes_unanswered(field).await;
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
    /// transport and gets the same cap: each oversized field goes unanswered
    /// there too, and the connection goes on serving.
    ///
    /// Mutation: serve the endpoint connection without `capped_endpoint`
    /// (`server.serve(stream)`, as before #101) and rmcp parses the frame
    /// and the tool answers request 10.
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
            client.oversized_frame_goes_unanswered(field).await;
        }
        hub.release(Some(&endpoint)).await;
        mem.close().await.expect("close");
    }
}
