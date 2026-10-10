//! #101: a pin, not a fix. An oversized argument over streamable HTTP is
//! refused before the body is read in full, through the serve's own router,
//! guards and the session's rmcp service (built with `session::http_config`).
//! This held before #101 too, because rmcp's default ceiling is the same
//! 4 MiB; the test guards that it keeps holding, now that `http_config`
//! sets the ceiling explicitly. That the explicit setting is Lambo's own
//! constant is pinned directly by
//! `transport::rmcp_still_mints_through_create_session_with_the_same_ceiling`
//! (#101 review L4).
//!
//! Inside an MCP session the guard leaves the body to rmcp, which streams
//! it and answers `413` once more than the ceiling has arrived, before
//! parsing any of it. The body here is a `tools/call` whose
//! `query_vector.values` runs to eight times the ceiling, sent chunked (no
//! `Content-Length` to refuse up front) and written while the reply is
//! read: the `413` arrives while most of it is still unsent, so it was
//! never buffered, parsed into a `Value`, or handed to the tool.

use super::*;
use crate::mcp::serve::frames::MAX_MCP_FRAME_BYTES;

/// How far past the ceiling the body runs.
const OVER: usize = 8 * MAX_MCP_FRAME_BYTES;

/// A pin of HTTP behaviour that predates #101 (see the module docs).
///
/// Mutation: raise the ceiling in `http_config`
/// (`with_max_request_body_bytes(usize::MAX)`) and rmcp reads the whole
/// body, parses it and answers 200 with the tool's refusal. (Dropping the
/// explicit setting does not fail this test: rmcp's default is the same.)
#[tokio::test]
async fn an_oversized_tool_call_body_is_still_refused_before_it_is_read() {
    let store: Box<dyn GraphStore> = Box::new(MemoryStore::new());
    let registry = pinned_registry(
        &["oversized-body"],
        backends_over(store, crate::Config::default()),
        4,
    )
    .await;
    let addr = serve_router(&registry, 4).await;
    let (sid, _) = initialize(addr, "/mcp").await;

    let sock = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let (mut read, mut write) = sock.into_split();
    let sent = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let writer = {
        let sent = Arc::clone(&sent);
        let sid = sid.clone();
        tokio::spawn(async move {
            let head = format!(
                "POST /mcp HTTP/1.1\r\nHost: localhost\r\nAccept: application/json, \
                 text/event-stream\r\nContent-Type: application/json\r\nMcp-Session-Id: \
                 {sid}\r\nTransfer-Encoding: chunked\r\n\r\n"
            );
            let prefix = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"lambo_recall","arguments":{"agent_id":"agent-a","query_vector":{"contract":{"kind":"fixture","dim":1024},"values":["#;
            let filler = "0.5,".repeat(2048);
            let chunk = |data: &str| format!("{:x}\r\n{data}\r\n", data.len());
            let mut out = head + &chunk(prefix);
            // The server closes once it has refused; a failed write ends
            // the body early, which is the point.
            if write.write_all(out.as_bytes()).await.is_err() {
                return;
            }
            out = chunk(&filler);
            while sent.load(std::sync::atomic::Ordering::Relaxed) < OVER {
                if write.write_all(out.as_bytes()).await.is_err() {
                    return;
                }
                sent.fetch_add(filler.len(), std::sync::atomic::Ordering::Relaxed);
            }
            let _ = write
                .write_all(format!("{}0\r\n\r\n", chunk("0.5]}}}}")).as_bytes())
                .await;
        })
    };

    // Read until the status line is in, then note how much body had gone.
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    let sent_at_reply = loop {
        let n = tokio::time::timeout(Duration::from_secs(60), read.read(&mut buf))
            .await
            .expect("a reply within 60 s")
            .expect("read");
        assert!(
            n > 0,
            "closed without a reply, {} body bytes sent",
            sent.load(std::sync::atomic::Ordering::Relaxed)
        );
        raw.extend_from_slice(&buf[..n]);
        if raw.windows(2).any(|w| w == b"\r\n") {
            break sent.load(std::sync::atomic::Ordering::Relaxed);
        }
    };
    let reply = String::from_utf8_lossy(&raw).to_string();
    assert!(
        reply.starts_with("HTTP/1.1 413"),
        "an oversized body is refused by size: {:.300}",
        reply
    );
    assert!(
        sent_at_reply < OVER,
        "the refusal came only after the whole {OVER}-byte body was sent ({sent_at_reply})"
    );
    writer.abort();

    // The MCP session is untouched by the refusal: it still answers.
    let stats = call(
        addr,
        "/mcp",
        &sid,
        "lambo_stats",
        serde_json::json!({"agent_id": "agent-a"}),
    )
    .await;
    assert_eq!(stats["isError"], serde_json::json!(false), "{stats}");

    for session in registry.close_set().await {
        session.mem.close().await.expect("close");
    }
}
