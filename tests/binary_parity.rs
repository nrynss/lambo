//! Binary+TOML parity (T8.7): every surface is driven through the *shipped
//! artifact* — the real `lambo` binary with a real `lambo.toml` — and asserts
//! the same outcomes the in-process tests assert.
//!
//! This is the process-level counterpart to the cargo tests. Where
//! `tests/t84_demo.rs` runs `demo::run_scenario` against an in-memory
//! `DemoOutcome`, `tests/cli_write_lease.rs` drives the CLI lease, and the
//! `src/mcp` / `src/cli/serve_web` unit tests hit the server in-process, each
//! surface here is spawned as `env!("CARGO_BIN_EXE_lambo")` with `--config
//! <toml>` and asserted over stdout / stdio / loopback HTTP.
//!
//! * The binary is resolved exactly like the existing process tests, via
//!   `CARGO_BIN_EXE_lambo` — Cargo injects the path of the binary built for
//!   the test profile, so a `--release` run exercises the release artifact
//!   with no extra build step.
//! * Each test uses a scratch `lambo.toml` (`[store] kind="sqlite"` +
//!   `[embedder] kind="fixture" dim=1024`) and a schema initialized with the
//!   binary's own idempotent `provision` verb — no live Cockroach cluster, no
//!   DSN.
//! * Only `std` subprocess/threads/TcpStream are used — no heavy new deps.
//!
//! Gated like the existing process tests (`tests/cli_write_lease.rs`), so it
//! runs under the `cargo test --features store-sqlite` row.
#![cfg(all(feature = "store-sqlite", feature = "embed-fixture", unix))]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Scratch config + binary helpers
// ---------------------------------------------------------------------------

struct Scratch {
    dir: std::path::PathBuf,
    config: std::path::PathBuf,
}

fn scratch(tag: &str) -> Scratch {
    let dir = std::env::temp_dir().join(format!(
        "lambo-binary-parity-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let sqlite = dir.join("parity.sqlite");
    let sqlite_str = sqlite.to_str().expect("utf-8").to_string();
    let config = dir.join("lambo.toml");
    std::fs::write(
        &config,
        format!(
            "[store]\nkind = \"sqlite\"\npath = \"{sqlite_str}\"\n\n[embedder]\nkind = \"fixture\"\ndim = 1024\n"
        ),
    )
    .expect("write config");
    Scratch { dir, config }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        // Panic-safe: the unique /tmp scratch dir (parity.sqlite + WAL/shm) is
        // owned by this guard, so an assertion failure mid-test can't leak it.
        // `remove_dir_all` is idempotent — a prior `cleanup(&s)` already removed
        // the dir, and this re-run simply no-ops on the missing path.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A `lambo` command with any ambient store/embedder/config env removed, so
/// the only configuration is the `--config <toml>` we pass (Level B).
fn lambo() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lambo"));
    for k in [
        "LAMBO_STORE",
        "LAMBO_EMBEDDER",
        "LAMBO_CONFIG",
        "LAMBO_COCKROACH_DSN",
        "DATABASE_URL",
        "LAMBO_SQLITE_PATH",
        "LAMBO_EMBED_DIM",
        "LAMBO_LLAMA_EMBED_URL",
        "LAMBO_LLAMA_MODEL",
    ] {
        cmd.env_remove(k);
    }
    cmd
}

struct Output {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

/// Run a subcommand to completion and capture (status, stdout, stderr).
fn run(cmd: &mut Command) -> Output {
    let out = cmd.output().expect("spawn lambo binary");
    Output {
        status: out.status,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// A writer `derive` on the given config. Writers acquire the single-writer
/// lease, write, and release it on exit.
fn derive(s: &Scratch, session: &str, agent: &str, content: &str) -> Output {
    run(lambo().args([
        "--config",
        s.config.to_str().unwrap(),
        "derive",
        "--session",
        session,
        "--agent",
        agent,
        "--content",
        content,
        "--kind",
        "entity",
    ]))
}

/// Initialize the sqlite schema with the binary's own idempotent `provision`
/// verb (the CLI's `derive`/`serve` do not auto-migrate).
fn provision(s: &Scratch) -> Output {
    run(lambo().args(["--config", s.config.to_str().unwrap(), "provision"]))
}

fn sigterm(pid: u32) {
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status();
}

fn cleanup(s: &Scratch) {
    let _ = std::fs::remove_dir_all(&s.dir);
}

// ---------------------------------------------------------------------------
// MCP stdio child (initialize/tools/list/tools/call over the real binary)
// ---------------------------------------------------------------------------

struct Mcp {
    guard: KillOnDrop,
    stdin: std::process::ChildStdin,
    rx: mpsc::Receiver<String>,
    _reader: std::thread::JoinHandle<()>,
}

fn spawn_serve_stdio(s: &Scratch, session: &str, agent: &str) -> Mcp {
    let mut cmd = lambo();
    cmd.args([
        "--config",
        s.config.to_str().unwrap(),
        "serve",
        "--session",
        session,
        "--agent",
        agent,
        "--transport",
        "stdio",
    ]);
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn serve (stdio)");
    let stdout = child.stdout.take().expect("serve stdout");
    let (tx, rx) = mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        let mut lines = BufReader::new(stdout).lines();
        while let Some(Ok(line)) = lines.next() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let stdin = child.stdin.take().expect("serve stdin");
    Mcp {
        guard: KillOnDrop { child: Some(child) },
        stdin,
        rx,
        _reader: reader,
    }
}

impl Mcp {
    fn send(&mut self, frame: &str) {
        self.stdin.write_all(frame.as_bytes()).expect("write frame");
        self.stdin.write_all(b"\n").expect("write newline");
        self.stdin.flush().expect("flush frame");
    }

    /// Read the first newline-delimited JSON-RPC frame whose id matches.
    fn read_response(&self, id: u64) -> String {
        let needle = format!("\"id\": {id}");
        let needle_compact = format!("\"id\":{id}");
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let line = self
                .rx
                .recv_timeout(remaining)
                .unwrap_or_else(|e| panic!("no JSON-RPC frame with id {id} within 20s: {e}"));
            if line.contains(&needle) || line.contains(&needle_compact) {
                return line;
            }
        }
    }

    fn shutdown(mut self) {
        // Reap the serve stdio child deterministically. The owning KillOnDrop
        // guard reaps it again on drop (a no-op once reaped), so a panicking
        // assertion cannot leak a live `lambo serve` holding the session lease.
        self.guard.reap();
        drop(self.stdin);
        let _ = self._reader.join();
    }
}

/// Drive `initialize` and assert the server answer completes the handshake.
fn initialize(mcp: &mut Mcp) {
    mcp.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"binary-parity","version":"1"}}}"#);
    let init = mcp.read_response(1);
    assert!(
        init.contains("\"serverInfo\""),
        "serve did not complete initialize: {init}"
    );
}

/// Pull the `"name":"lambo_…"` tool names out of a `tools/list` frame.
fn tool_names(frame: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = frame;
    while let Some(i) = rest.find("\"name\":\"lambo_") {
        let after = &rest[i + "\"name\":\"lambo_".len()..];
        let end = after.find('"').expect("closing quote after tool name");
        names.push(format!("lambo_{}", &after[..end]));
        rest = &after[end..];
    }
    names.sort();
    names
}

const SEVEN_TOOLS: &[&str] = &[
    "lambo_derive",
    "lambo_inspect",
    "lambo_recall",
    "lambo_record_action",
    "lambo_reserve",
    "lambo_saints",
    "lambo_stats",
];

// ---------------------------------------------------------------------------
// Minimal HTTP client (std TcpStream) for the loopback serve-web surface
// ---------------------------------------------------------------------------

struct HttpResponse {
    status: u16,
    body: String,
}

fn http(addr: &str, method: &str, path: &str) -> HttpResponse {
    let mut sock = TcpStream::connect(addr).expect("connect serve-web");
    sock.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    let req = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    sock.write_all(req.as_bytes()).expect("write request");
    sock.flush().expect("flush request");
    let mut raw = Vec::new();
    sock.read_to_end(&mut raw).expect("read response");
    let raw = String::from_utf8_lossy(&raw);
    let (head, body) = raw.split_once("\r\n\r\n").expect("http header/body split");
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .expect("status code")
        .parse()
        .expect("parse status");
    HttpResponse {
        status,
        body: body.to_string(),
    }
}

/// Like [`http`], but returns `None` on connection refusal — used while a
/// freshly spawned server is still binding its socket.
fn try_http(addr: &str, method: &str, path: &str) -> Option<HttpResponse> {
    match TcpStream::connect(addr) {
        Ok(_) => Some(http(addr, method, path)),
        Err(_) => None,
    }
}

fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .expect("bind ephemeral")
        .local_addr()
        .expect("local addr")
        .port()
}

/// Poll the server until a route answers or the deadline elapses. Tolerates
/// connection refusal while the server is still coming up.
fn wait_for(addr: &str, path: &str, seconds: u64) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if let Some(HttpResponse { status: 200, .. }) = try_http(addr, "GET", path) {
            return;
        }
        if Instant::now() >= deadline {
            let last = try_http(addr, "GET", path)
                .map(|r| format!("status {} {}", r.status, r.body))
                .unwrap_or_else(|| "connection refused".to_string());
            panic!("serve-web did not answer GET {path} within {seconds}s (last: {last})");
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// SIGTERM + reap a spawned serve-web process on drop, so a panicking
/// assertion cannot leak a live child that holds the harness open.
struct KillOnDrop {
    child: Option<Child>,
}
impl KillOnDrop {
    /// SIGTERM + reap the child if it is still owned; safe to call more than
    /// once (a no-op on later calls once the child has been taken/reaped).
    fn reap(&mut self) {
        if let Some(mut child) = self.child.take() {
            sigterm(child.id());
            let _ = child.wait();
        }
    }
}
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        self.reap();
    }
}

// ---------------------------------------------------------------------------
// 1. demo determinism: OUTCOME meets spec §13 and is byte-identical ×2
// ---------------------------------------------------------------------------

/// Extract the emitted `DemoOutcome::render` block from `lambo demo` stdout.
///
/// The narration (ACT banners, node ids, timings) is volatile; the *rendered
/// outcome* masks every volatile value (`<s>`, `<n>`, `<node>`) and is the
/// documented ×2 determinism bar. Its first line is the bare, un-indented
/// `scenario …` render line (everything before it is narration, indented).
fn outcome_block(stdout: &str) -> String {
    let lines: Vec<&str> = stdout.lines().collect();
    let start = lines
        .iter()
        .position(|l| l.starts_with("scenario") && !l.starts_with(' '))
        .expect("the demo stdout must carry the rendered OUTCOME block");
    lines[start..].join("\n")
}

#[test]
fn demo_outcome_meets_spec_13_and_is_identical_across_two_runs() {
    let s = scratch("demo");

    // Two invocations of the shipped binary against the same scratch store,
    // each minting a fresh session id (the binary's default — R3-1).
    let run1 = run(lambo().args([
        "--config",
        s.config.to_str().unwrap(),
        "demo",
        "--scenario",
        "rest-api",
    ]));
    let run2 = run(lambo().args([
        "--config",
        s.config.to_str().unwrap(),
        "demo",
        "--scenario",
        "rest-api",
    ]));

    assert_eq!(
        run1.status.code(),
        Some(0),
        "demo must exit 0; stderr=\n{}",
        run1.stderr
    );
    assert_eq!(
        run2.status.code(),
        Some(0),
        "demo (run 2) must exit 0; stderr=\n{}",
        run2.stderr
    );

    let o1 = outcome_block(&run1.stdout);
    let o2 = outcome_block(&run2.stdout);

    // ---- spec §13 headline counts (mirror t84_demo::assert_spec_13) ----
    assert!(o1.contains("scenario            rest-api"), "{o1}");
    assert!(o1.contains("interactions        12"), "{o1}");
    assert!(o1.contains("concepts            27"), "{o1}");
    assert!(o1.contains("edges               114"), "{o1}");
    assert!(o1.contains("canonization_events 5"), "{o1}");
    assert!(o1.contains("canonical           1"), "{o1}");

    // Step 2 outcome: the single canonical is `user schema` with a Stage-3
    // blast radius of 9.
    assert!(
        o1.contains("  user schema  blast_radius=9"),
        "the canonical line must name user schema with blast radius 9:\n{o1}"
    );
    // ...and it alone sits at Canonical (mirror of the `statuses` assertion).
    assert!(
        o1.contains("Canonical  user schema"),
        "user schema must be Canonical:\n{o1}"
    );

    // Step 3 recall context block: the canonical marker and the load-bearing
    // pillar line, verbatim (spec §13).
    assert!(
        o1.contains("user schema [Entity, canonical]"),
        "recall context is missing the canonical marker:\n{o1}"
    );
    assert!(
        o1.contains("⚑ Load-bearing pillar — 9 nodes depend on this. Modify with caution."),
        "recall context is missing the ⚑ load-bearing pillar line verbatim:\n{o1}"
    );

    // The ×2 bar: the OUTCOME blocks (volatile-masked) must be byte-identical.
    assert_eq!(
        o1, o2,
        "two binary runs of the same scenario must produce byte-identical OUTCOME blocks\n\
         ---- run 1 ----\n{o1}\n---- run 2 ----\n{o2}"
    );

    cleanup(&s);
}

// ---------------------------------------------------------------------------
// 2. CLI write + lease: a writer succeeds free, and fails closed under serve
// ---------------------------------------------------------------------------

#[test]
fn derive_writes_then_a_second_writer_fails_closed_under_serve() {
    let s = scratch("lease");
    const SESSION: &str = "t87-binary-lease";

    let prov = provision(&s);
    assert!(
        prov.status.success(),
        "provision must init the sqlite schema; stderr=\n{}",
        prov.stderr
    );

    // No serve: the first writer acquires the lease, writes, releases.
    let first = derive(&s, SESSION, "agent-free", "user schema");
    assert!(
        first.status.success(),
        "derive with no serve must succeed; stderr=\n{}",
        first.stderr
    );

    // Property (mirror cli_write_lease): a successful CLI write always releases
    // the session lease, so a second free writer can acquire again.
    let again = derive(&s, SESSION, "agent-lease-release-probe", "auth middleware");
    assert!(
        again.status.success(),
        "lease must be released after the first derive so a second writer can acquire; stderr=\n{}",
        again.stderr
    );

    // Serve holds the session (writer lease) over stdio; handshake so we know
    // the lease is held.
    let mut mcp = spawn_serve_stdio(&s, SESSION, "agent-a");
    initialize(&mut mcp);

    // A writer must fail closed, naming the holder.
    let refused = derive(&s, SESSION, "agent-b", "session store");
    assert!(
        !refused.status.success(),
        "derive while serve holds must fail closed; stdout=\n{}",
        refused.stdout
    );
    let stderr = &refused.stderr;
    assert!(
        stderr.contains("single-writer"),
        "must name the single-writer lease; stderr=\n{stderr}"
    );
    assert!(
        stderr.contains("agent-a"),
        "must name the holder (agent-a); stderr=\n{stderr}"
    );

    // Readers must still succeed while serve holds the lease (spec §2.2).
    for (name, args) in [
        ("saints", vec!["saints", "--session", SESSION]),
        ("stats", vec!["stats", "--session", SESSION]),
        (
            "recall",
            vec!["recall", "--session", SESSION, "--query", "user schema"],
        ),
        (
            "inspect",
            vec!["inspect", "--session", SESSION, "--focus", "user schema"],
        ),
    ] {
        let mut cmd = lambo();
        cmd.arg("--config").arg(s.config.to_str().unwrap());
        cmd.args(&args);
        let out = run(&mut cmd);
        assert!(
            out.status.success(),
            "{name} is a reader and must succeed while serve holds; stderr=\n{}",
            out.stderr
        );
    }

    mcp.shutdown();
    cleanup(&s);
}

// ---------------------------------------------------------------------------
// 3. serve-web: live data over loopback HTTP, and read-only (405)
// ---------------------------------------------------------------------------

#[test]
fn serve_web_serves_live_data_over_http_and_stays_read_only() {
    let s = scratch("serve-web");
    const SESSION: &str = "t87-binary-serveweb";

    let prov = provision(&s);
    assert!(prov.status.success(), "provision; stderr=\n{}", prov.stderr);

    // Seed the session with a writer derive (writer acquires/releases the lease).
    let seeded = derive(&s, SESSION, "agent-a", "user schema");
    assert!(
        seeded.status.success(),
        "seed derive; stderr=\n{}",
        seeded.stderr
    );

    let port = free_port();
    let addr = format!("127.0.0.1:{port}");

    let child = lambo()
        .args([
            "--config",
            s.config.to_str().unwrap(),
            "serve-web",
            "--session",
            SESSION,
            "--port",
            &port.to_string(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn serve-web");
    // Terminate the server on drop, so a panicking assertion cannot leak a
    // live child (its open stderr would hold the test harness open to timeout).
    let _guard = KillOnDrop { child: Some(child) };

    // Server is up when the read-only stats endpoint answers.
    wait_for(&addr, "/api/stats", 30);

    // Live data after the derive, over loopback HTTP. /api/stats counts the
    // session; a reader mode; real node/edge/concept counts.
    let stats = http(&addr, "GET", "/api/stats");
    assert_eq!(stats.status, 200, "{}", stats.body);
    assert!(
        stats.body.contains(&format!("\"session\":\"{SESSION}\"")),
        "/api/stats must report the session: {}",
        stats.body
    );
    assert!(
        stats.body.contains("\"mode\":\"reader\""),
        "/api/stats must report a reader process: {}",
        stats.body
    );
    assert!(
        stats.body.contains("\"nodes\":")
            && stats.body.contains("\"edges\":")
            && stats.body.contains("\"concepts\":"),
        "/api/stats must carry real node/edge/concept counts: {}",
        stats.body
    );

    // /api/recall returns the live context block for the seeded concept.
    let recall = http(&addr, "GET", "/api/recall?q=user%20schema");
    assert_eq!(recall.status, 200, "{}", recall.body);
    assert!(
        recall.body.contains("user schema"),
        "/api/recall must return a context block naming the concept: {}",
        recall.body
    );

    // /api/events tails the canonization feed (may legitimately be empty for a
    // non-canonized session — it must still be a well-formed count payload).
    let events = http(&addr, "GET", "/api/events");
    assert_eq!(events.status, 200, "{}", events.body);
    assert!(
        events.body.contains("\"total\""),
        "/api/events must carry a total count: {}",
        events.body
    );

    // Read-only: a mutating verb on any route is Method Not Allowed.
    let pulse = http(&addr, "POST", "/api/pulse");
    assert_eq!(
        pulse.status, 405,
        "POST /api/pulse must be Method Not Allowed — serve-web is a read-only window: {}",
        pulse.body
    );

    // Clean shutdown: SIGTERM + reap via the guard (drives the graceful drain),
    // then remove the scratch dir.
    drop(_guard);
    cleanup(&s);
}

// ---------------------------------------------------------------------------
// 4. MCP stdio: exactly seven tools, and a client timestamp is refused (F18)
// ---------------------------------------------------------------------------

#[test]
fn mcp_stdio_publishes_exactly_seven_tools_and_refuses_a_client_timestamp() {
    let s = scratch("mcp");
    const SESSION: &str = "t87-binary-mcp";

    let prov = provision(&s);
    assert!(prov.status.success(), "provision; stderr=\n{}", prov.stderr);

    let mut mcp = spawn_serve_stdio(&s, SESSION, "agent-a");
    initialize(&mut mcp);

    // tools/list -> exactly the seven spec §6.2 tools (mirror server.rs
    // the_router_publishes_exactly_the_seven_spec_tools) over the wire.
    mcp.send(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
    let listed = mcp.read_response(2);
    assert!(
        !listed.contains("\"error\""),
        "tools/list must succeed, not error: {listed}"
    );
    assert_eq!(
        tool_names(&listed),
        SEVEN_TOOLS,
        "spec §6.2 names exactly these seven tools; got: {listed}"
    );

    // F18 — a client-supplied timestamp is an unknown field and is refused.
    // `DeriveParams` is `deny_unknown_fields`; rmcp deserializes through
    // `Parameters<T>` and surfaces the serde failure as a readable tool error
    // (mirror of the module's F18 pinned tests, over the wire).
    mcp.send(
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"lambo_derive","arguments":{"agent_id":"agent-a","concepts":[{"content":"user schema","concept_type":"entity"}],"timestamp":"2020-01-01T00:00:00Z"}}}"#,
    );
    let refused = mcp.read_response(3);
    assert!(
        refused.contains("\"isError\":true"),
        "a client-supplied timestamp must be refused; resp=\n{refused}"
    );
    assert!(
        refused.contains("unknown field") && refused.contains("timestamp"),
        "the refusal must name the unknown timestamp field (F18); resp=\n{refused}"
    );

    // The session is still healthy: a clean derive (no timestamp) succeeds.
    mcp.send(
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"lambo_derive","arguments":{"agent_id":"agent-a","concepts":[{"content":"user schema","concept_type":"entity"}]}}}"#,
    );
    let ok = mcp.read_response(4);
    assert!(
        ok.contains("\"isError\":false"),
        "a clean derive must still succeed after the refusal; resp=\n{ok}"
    );

    mcp.shutdown();
    cleanup(&s);
}
