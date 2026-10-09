//! #32 PR 4: one `lambo serve --transport http` holding two pinned sessions,
//! across the process boundary.
//!
//! * each session is reached at `/mcp/s/{session}`, and `/mcp` is the first
//!   `--session`;
//! * per-session fencing: a CLI writer is refused on either session, and a
//!   second hub pinning the same sessions starts with both held elsewhere and
//!   answers 503 for them;
//! * a stdio `serve --session b` proxies into the hub's session-b endpoint;
//! * SIGTERM releases every lease: both rows read back `lambo:released`
//!   with their fencing tokens kept.
//!
//! Every spawned serve gets `XDG_RUNTIME_DIR` pointed at the test's own
//! [`RuntimeDir`] (#15), and the HTTP ports are ephemeral, never 7700.
//! Gated on `store-sqlite,embed-fixture` like the other serve integration
//! tests; run with `--features store-sqlite,embed-fixture`.
#![cfg(all(feature = "store-sqlite", feature = "embed-fixture", unix))]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use lambo::store::{GraphStore, SqliteStore};
use lambo::types::SessionId;

mod common;
use common::{RuntimeDir, ScratchDir, ServeChild, RUNTIME_DIR_VAR};

const A: &str = "i32-multi-a";
const B: &str = "i32-multi-b";

fn scratch() -> (ScratchDir, std::path::PathBuf, String) {
    let dir = ScratchDir::new("lambo-i32-multi");
    let db = dir.join("multi.sqlite");
    let cfg = dir.join("lambo.toml");
    std::fs::write(
        &cfg,
        format!(
            "[store]\nkind = \"sqlite\"\npath = \"{}\"\n\n[embedder]\nkind = \"fixture\"\ndim = 1024\n",
            db.display()
        ),
    )
    .expect("write config");
    let db = db.display().to_string();
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(async {
        let store = SqliteStore::connect(&db).expect("connect");
        store.init_schema().await.expect("init_schema");
    });
    (dir, cfg, db)
}

fn lease_row(db: &str, session: &str) -> Option<lambo::store::LeaseInfo> {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(async {
        let store = SqliteStore::connect(db).expect("connect");
        store
            .read_lease(&SessionId::from(session))
            .await
            .expect("read lease")
    })
}

/// An ephemeral loopback port (never 7700).
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 0");
    listener.local_addr().expect("addr").port()
}

/// A spawned HTTP hub, its stderr collected line by line.
struct Hub {
    child: ServeChild,
    lines: mpsc::Receiver<String>,
    seen: Vec<String>,
    addr: SocketAddr,
}

impl Hub {
    fn spawn(cfg: &std::path::Path, runtime: &RuntimeDir, agent: &str) -> Self {
        let port = free_port();
        let mut child = ServeChild::new(
            common::lambo_command()
                .env(RUNTIME_DIR_VAR, runtime.path())
                .args([
                    "--config",
                    cfg.to_str().unwrap(),
                    "serve",
                    "--transport",
                    "http",
                    "--port",
                    &port.to_string(),
                    "--session",
                    A,
                    "--session",
                    B,
                    "--agent",
                    agent,
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn hub"),
        );
        let stderr = child.stderr.take().expect("stderr");
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            lines,
            seen: Vec::new(),
            addr: SocketAddr::from(([127, 0, 0, 1], port)),
        }
    }

    /// Wait for `count` stderr lines containing `needle`.
    fn wait_for(&mut self, needle: &str, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.seen.iter().filter(|l| l.contains(needle)).count() < count {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => self.seen.push(line),
                Err(e) => panic!(
                    "no {needle:?} x{count} ({e}); stderr so far:\n{}",
                    self.seen.join("\n")
                ),
            }
        }
    }

    fn sigterm_and_wait(mut self) -> (std::process::ExitStatus, Vec<String>) {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(self.child.id().to_string())
            .status();
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                break status;
            }
            assert!(Instant::now() < deadline, "hub did not exit on SIGTERM");
            std::thread::sleep(Duration::from_millis(50));
        };
        while let Ok(line) = self.lines.recv_timeout(Duration::from_millis(500)) {
            self.seen.push(line);
        }
        (status, self.seen)
    }
}

/// One HTTP POST, read to the end: (status, headers, body).
fn post(
    addr: SocketAddr,
    path: &str,
    mcp_session: Option<&str>,
    body: &str,
) -> (u16, String, String) {
    let mut sock = TcpStream::connect(addr).expect("connect");
    sock.set_read_timeout(Some(Duration::from_secs(20))).ok();
    let mut head = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nAccept: application/json, text/event-stream\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if let Some(id) = mcp_session {
        head.push_str(&format!("Mcp-Session-Id: {id}\r\n"));
    }
    head.push_str("\r\n");
    sock.write_all(head.as_bytes()).expect("write");
    sock.write_all(body.as_bytes()).expect("write body");
    let mut raw = Vec::new();
    let _ = sock.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status line: {text:?}"));
    (status, head.to_string(), body.to_string())
}

const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"i32-multi","version":"1"}}}"#;

/// `initialize` at `path`; the body names the session in its instructions.
fn initialize_names(addr: SocketAddr, path: &str, session: &str) {
    let (status, _, body) = post(addr, path, None, INITIALIZE);
    assert_eq!(status, 200, "initialize at {path}: {body}");
    assert!(
        body.contains(&format!("session '{session}'")),
        "{path} must reach {session}: {body}"
    );
}

/// A stdio serve, its stdout frames collected.
struct Stdio1 {
    child: ServeChild,
    stdin: std::process::ChildStdin,
    rx: mpsc::Receiver<String>,
}

impl Stdio1 {
    fn spawn(cfg: &std::path::Path, runtime: &RuntimeDir, session: &str) -> Self {
        let mut child = ServeChild::new(
            common::lambo_command()
                .env(RUNTIME_DIR_VAR, runtime.path())
                .args([
                    "--config",
                    cfg.to_str().unwrap(),
                    "serve",
                    "--session",
                    session,
                    "--agent",
                    "agent-stdio",
                    "--transport",
                    "stdio",
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("spawn stdio serve"),
        );
        let stdout = child.stdout.take().expect("stdout");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stdin = child.stdin.take().expect("stdin");
        Self { child, stdin, rx }
    }

    fn send(&mut self, frame: &str) {
        self.stdin.write_all(frame.as_bytes()).expect("write");
        self.stdin.write_all(b"\n").expect("newline");
        self.stdin.flush().expect("flush");
    }

    fn response(&self, id: u64) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self
                .rx
                .recv_timeout(left)
                .unwrap_or_else(|e| panic!("no frame with id {id}: {e}"));
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if v.get("id").and_then(serde_json::Value::as_u64) == Some(id) {
                return v;
            }
        }
    }
}

#[test]
fn one_hub_serves_two_pinned_sessions_fenced_proxied_and_released() {
    let (_dir, cfg, db) = scratch();
    let runtime = RuntimeDir::new();

    let mut hub = Hub::spawn(&cfg, &runtime, "agent-hub");
    hub.wait_for("mcp http: listening on /mcp", 1);
    hub.wait_for("lambo serve: session attached", 2);

    // Routing: each path reaches its own session; /mcp is the first --session.
    initialize_names(hub.addr, &format!("/mcp/s/{A}"), A);
    initialize_names(hub.addr, &format!("/mcp/s/{B}"), B);
    initialize_names(hub.addr, "/mcp", A);
    let (status, _, _) = post(hub.addr, "/mcp/s/i32-not-hosted", None, INITIALIZE);
    assert_eq!(status, 404);

    let tokens: Vec<u64> = [A, B]
        .iter()
        .map(|s| {
            let row = lease_row(&db, s).expect("the hub holds a lease row");
            assert!(row.holder.contains("agent-hub"), "{s}: {}", row.holder);
            row.token
        })
        .collect();

    // Per-session fencing, CLI writer: refused on either session.
    for session in [A, B] {
        let out = common::lambo_command()
            .env(RUNTIME_DIR_VAR, runtime.path())
            .args([
                "--config",
                cfg.to_str().unwrap(),
                "derive",
                "--session",
                session,
                "--agent",
                "agent-cli",
                "--content",
                "cli write",
                "--kind",
                "entity",
            ])
            .output()
            .expect("run derive");
        assert!(!out.status.success(), "{session}: a CLI writer got in");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("single-writer"), "{session}: {stderr}");
    }

    // Per-session fencing, a second hub: both sessions held elsewhere, 503.
    let mut second = Hub::spawn(&cfg, &runtime, "agent-second");
    second.wait_for("a pinned session is held by another writer", 2);
    second.wait_for("mcp http: listening on /mcp", 1);
    for session in [A, B] {
        let (status, head, body) =
            post(second.addr, &format!("/mcp/s/{session}"), None, INITIALIZE);
        assert_eq!(status, 503, "{session}: {body}");
        assert!(head.to_ascii_lowercase().contains("retry-after:"), "{head}");
    }
    let (status, _) = second.sigterm_and_wait();
    assert!(status.success(), "the second hub exits cleanly: {status:?}");

    // Stdio `serve --session b` proxies into the hub's session-b endpoint.
    let mut proxy = Stdio1::spawn(&cfg, &runtime, B);
    proxy.send(INITIALIZE);
    let init = proxy.response(1);
    assert!(
        init.to_string().contains(&format!("session '{B}'")),
        "the proxy reached the hub's {B}: {init}"
    );
    proxy.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    proxy.send(
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"lambo_stats","arguments":{"agent_id":"agent-stdio"}}}"#,
    );
    let stats = proxy.response(2);
    assert_eq!(
        stats["result"]["structuredContent"]["session"], B,
        "{stats}"
    );
    drop(proxy.stdin);
    let _ = proxy
        .child
        .wait_with_output_within(Duration::from_secs(30))
        .expect("the proxy exits on stdin EOF");

    // The holders' rows are still the hub's.
    for (session, token) in [A, B].iter().zip(&tokens) {
        let row = lease_row(&db, session).expect("row");
        assert!(
            row.holder.contains("agent-hub"),
            "{session}: {}",
            row.holder
        );
        assert_eq!(row.token, *token, "{session}");
    }

    // SIGTERM releases every lease, tokens kept.
    let (status, lines) = hub.sigterm_and_wait();
    assert!(
        status.success(),
        "hub exit: {status:?}\n{}",
        lines.join("\n")
    );
    assert!(
        lines.iter().any(|l| l.contains("shutdown finished")),
        "{}",
        lines.join("\n")
    );
    for (session, token) in [A, B].iter().zip(&tokens) {
        let row = lease_row(&db, session).expect("the row is kept");
        assert_eq!(
            row.holder,
            lambo::store::lease::RELEASED_HOLDER,
            "{session}"
        );
        assert_eq!(row.token, *token, "{session}: the token is kept");
    }
}
