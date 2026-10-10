//! #32 PR 7: the in-serve erase across the process boundary, on SQLite.
//!
//! * A two-session HTTP hub: a session written over MCP (and flushed) is
//!   erased through `POST /admin/s/{s}/erase` by an operator credential;
//!   every table of the shipped DDL then holds no row of it but the
//!   tombstone, MCP requests for it are refused with 410, the other session
//!   keeps serving, and after SIGTERM nothing of it came back.
//! * A one-session hub (the dogfood rig's shape plus an operator
//!   credential scoped to the one session, so nothing attaches on demand,
//!   #32 PR 6): erasing its only session answers 200 and the process
//!   then exits on its own; a restart on the erased session refuses.
//!
//! Tokens are built at runtime. Every spawned serve gets `XDG_RUNTIME_DIR`
//! pointed at the test's own [`RuntimeDir`]; ports are ephemeral, never
//! 7700.
#![cfg(all(feature = "store-sqlite", feature = "embed-fixture", unix))]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use lambo::store::{GraphStore, SqliteStore};

mod common;
use common::{RuntimeDir, ScratchDir, ServeChild, RUNTIME_DIR_VAR};

const A: &str = "i32g-erase-a";
const B: &str = "i32g-erase-b";
const OPS_ENV: &str = "LAMBO_TEST_32G_OPS";
const AGENT_ENV: &str = "LAMBO_TEST_32G_AGENT";

fn token(label: &str) -> String {
    ["fake", label, "i32g", "value"].join("-")
}

/// A scratch SQLite store and a `lambo.toml` over it with two credentials:
/// `agents` (both sessions, no flags) and `operator` (`"*"`, erase, admin).
fn scratch() -> (ScratchDir, std::path::PathBuf, std::path::PathBuf) {
    scratch_scoped(&format!(r#"["{A}", "{B}"]"#), r#"["*"]"#)
}

/// [`scratch`] with each credential's `sessions` given. A serve whose
/// credentials reach no session past the pinned ones attaches nothing on
/// demand (#32 PR 6), so pinning only `A` with both scoped to `["A"]` is
/// a one-session serve, which exits when its session is erased.
fn scratch_scoped(
    agents: &str,
    operator: &str,
) -> (ScratchDir, std::path::PathBuf, std::path::PathBuf) {
    let dir = ScratchDir::new("lambo-i32g");
    let db = dir.join("erase.sqlite");
    let cfg = dir.join("lambo.toml");
    std::fs::write(
        &cfg,
        format!(
            "[store]\nkind = \"sqlite\"\npath = \"{}\"\n\n[embedder]\nkind = \"fixture\"\ndim = 1024\n\n\
             [[serve.credential]]\nname = \"agents\"\ntoken_env = \"{AGENT_ENV}\"\nsessions = {agents}\n\n\
             [[serve.credential]]\nname = \"operator\"\ntoken_env = \"{OPS_ENV}\"\nsessions = {operator}\nerase = true\nadmin = true\n",
            db.display()
        ),
    )
    .expect("write config");
    runtime().block_on(async {
        let store = SqliteStore::connect(db.to_str().unwrap()).expect("connect");
        store.init_schema().await.expect("init_schema");
    });
    (dir, cfg, db)
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().expect("runtime")
}

fn serve_command(cfg: &std::path::Path, runtime: &RuntimeDir, sessions: &[&str]) -> Command {
    let mut cmd = common::lambo_command();
    cmd.env(RUNTIME_DIR_VAR, runtime.path())
        .env_remove("RUST_LOG")
        .env_remove("LAMBO_AUTH_TOKEN")
        .env(OPS_ENV, token("ops"))
        .env(AGENT_ENV, token("agents"))
        .args([
            "--config",
            cfg.to_str().unwrap(),
            "serve",
            "--transport",
            "http",
        ])
        .args(["--port", "0", "--agent", "agent-i32g"]);
    for s in sessions {
        cmd.args(["--session", s]);
    }
    cmd
}

/// A spawned HTTP hub, its stderr collected line by line.
struct Hub {
    child: ServeChild,
    lines: mpsc::Receiver<String>,
    seen: Vec<String>,
    addr: SocketAddr,
}

impl Hub {
    fn spawn(mut cmd: Command) -> Self {
        let mut child = ServeChild::new(
            cmd.stdin(Stdio::null())
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
        let mut hub = Self {
            child,
            lines,
            seen: Vec::new(),
            addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        };
        const LISTENING: &str = "mcp http: listening on /mcp";
        let deadline = Instant::now() + Duration::from_secs(60);
        while !hub.seen.iter().any(|l| l.contains(LISTENING)) {
            let left = deadline.saturating_duration_since(Instant::now());
            match hub.lines.recv_timeout(left) {
                Ok(line) => hub.seen.push(line),
                Err(e) => panic!("no listening line ({e}):\n{}", hub.seen.join("\n")),
            }
        }
        let line = hub.seen.iter().find(|l| l.contains(LISTENING)).unwrap();
        let at = line.find("127.0.0.1:").expect("a loopback address");
        let port: String = line[at + "127.0.0.1:".len()..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        hub.addr = SocketAddr::from(([127, 0, 0, 1], port.parse().expect("a port")));
        hub
    }

    /// Wait (up to 30 s) for the process to exit on its own.
    fn exits_on_its_own(mut self) -> (Option<i32>, Vec<String>) {
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "the hub did not exit:\n{}",
                self.seen.join("\n")
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        while let Ok(line) = self.lines.recv_timeout(Duration::from_millis(300)) {
            self.seen.push(line);
        }
        (status.code(), self.seen)
    }

    /// SIGTERM, then wait for the exit.
    fn stop(self) -> (Option<i32>, Vec<String>) {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(self.child.id().to_string())
            .status();
        self.exits_on_its_own()
    }
}

/// One HTTP/1.1 exchange: (status, raw response).
fn exchange(
    addr: SocketAddr,
    method: &str,
    path: &str,
    auth: &str,
    mcp: Option<&str>,
    body: &str,
) -> (u16, String) {
    let mut sock = TcpStream::connect(addr).expect("connect");
    sock.set_read_timeout(Some(Duration::from_secs(30))).ok();
    let mcp = mcp
        .map(|id| format!("Mcp-Session-Id: {id}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAccept: application/json, text/event-stream\r\n\
         Content-Type: application/json\r\nAuthorization: Bearer {auth}\r\n{mcp}Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    sock.write_all(request.as_bytes()).expect("write");
    let mut raw = Vec::new();
    let _ = sock.read_to_end(&mut raw);
    let raw = String::from_utf8_lossy(&raw).into_owned();
    let status = raw
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status line: {raw:?}"));
    (status, raw)
}

const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"i32g","version":"1"}}}"#;

/// Open an MCP session on `path`; its id.
fn initialize(addr: SocketAddr, path: &str, auth: &str) -> String {
    let (status, raw) = exchange(addr, "POST", path, auth, None, INITIALIZE);
    assert_eq!(status, 200, "{raw}");
    let id = raw
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("mcp-session-id")
                .then(|| v.trim().to_string())
        })
        .expect("an MCP session id");
    let (status, raw) = exchange(
        addr,
        "POST",
        path,
        auth,
        Some(&id),
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    );
    assert!(status < 300, "{raw}");
    id
}

/// `tools/call` `name` with `args`; the raw response.
fn call(addr: SocketAddr, path: &str, auth: &str, mcp: &str, name: &str, args: &str) -> String {
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{{"name":"{name}","arguments":{args}}}}}"#
    );
    let (status, raw) = exchange(addr, "POST", path, auth, Some(mcp), &body);
    assert_eq!(status, 200, "{raw}");
    assert!(!raw.contains(r#""error""#), "{raw}");
    raw
}

/// Derive `content` into the session at `path` and wait for it to apply.
fn derive(addr: SocketAddr, path: &str, auth: &str, mcp: &str, content: &str) {
    let raw = call(
        addr,
        path,
        auth,
        mcp,
        "lambo_derive",
        &format!(
            r#"{{"agent_id":"agent-i32g","concepts":[{{"content":"{content}","concept_type":"entity"}}]}}"#
        ),
    );
    let at = raw.find(r#""receipt":""#).expect("a receipt") + r#""receipt":""#.len();
    let receipt: String = raw[at..].chars().take_while(|c| *c != '"').collect();
    call(
        addr,
        path,
        auth,
        mcp,
        "lambo_stats",
        &format!(r#"{{"agent_id":"agent-i32g","receipt":"{receipt}","wait_ms":10000}}"#),
    );
}

/// Rows per table of the shipped DDL for `session`; the lease row's holder.
fn census(db: &std::path::Path, session: &str) -> (Vec<(String, i64)>, Option<String>) {
    runtime().block_on(async {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&format!("sqlite://{}", db.display()))
            .await
            .expect("open the store");
        let ddl = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/migrations/sqlite/001_init.sql"
        ));
        let mut rows = Vec::new();
        for table in lambo::store::tables_in_ddl(ddl) {
            let n: i64 = sqlx::query_scalar(&format!(
                "SELECT COUNT(*) FROM {table} WHERE session_id = ?1"
            ))
            .bind(session)
            .fetch_one(&pool)
            .await
            .unwrap_or_else(|e| panic!("count {table}: {e}"));
            rows.push((table.to_string(), n));
        }
        let holder: Option<String> =
            sqlx::query_scalar("SELECT holder FROM session_leases WHERE session_id = ?1")
                .bind(session)
                .fetch_optional(&pool)
                .await
                .expect("read the lease row");
        pool.close().await;
        (rows, holder)
    })
}

/// Only the tombstone is left of `session`.
fn assert_only_the_tombstone(db: &std::path::Path, session: &str) {
    let (rows, holder) = census(db, session);
    for (table, n) in rows {
        let want = i64::from(table == "session_leases");
        assert_eq!(n, want, "{session}: {table}");
    }
    assert_eq!(holder.as_deref(), Some(lambo::store::erase::ERASED_HOLDER));
}

#[test]
fn an_attached_session_is_erased_over_the_admin_route_and_stays_erased() {
    let (_dir, cfg, db) = scratch();
    let rt = RuntimeDir::new();
    let hub = Hub::spawn(serve_command(&cfg, &rt, &[A, B]));
    let addr = hub.addr;
    let (agents, ops) = (token("agents"), token("ops"));
    let path_a = format!("/mcp/s/{A}");
    let path_b = format!("/mcp/s/{B}");

    let mcp_a = initialize(addr, &path_a, &agents);
    derive(addr, &path_a, &agents, &mcp_a, "a fact about a user");
    let mcp_b = initialize(addr, &path_b, &agents);
    derive(addr, &path_b, &agents, &mcp_b, "a fact that stays");
    // The write-behind flush makes A durable before the erase.
    let deadline = Instant::now() + Duration::from_secs(20);
    while census(&db, A)
        .0
        .iter()
        .any(|(t, n)| t == "concepts" && *n == 0)
    {
        assert!(Instant::now() < deadline, "A never flushed");
        std::thread::sleep(Duration::from_millis(100));
    }
    // One more write, likely still in RAM when the erase lands.
    derive(addr, &path_a, &agents, &mcp_a, "a later fact about a user");

    // The agents credential has no `erase`: the unrouted 404.
    let (status, _) = exchange(
        addr,
        "POST",
        &format!("/admin/s/{A}/erase"),
        &agents,
        None,
        &format!(r#"{{"confirm":"{A}"}}"#),
    );
    assert_eq!(status, 404);

    let (status, raw) = exchange(
        addr,
        "POST",
        &format!("/admin/s/{A}/erase"),
        &ops,
        None,
        &format!(r#"{{"confirm":"{A}"}}"#),
    );
    assert_eq!(status, 200, "{raw}");
    assert!(raw.contains(r#""already_absent":false"#), "{raw}");
    assert_only_the_tombstone(&db, A);

    // Refused from now on; B serves on.
    let (status, raw) = exchange(addr, "POST", &path_a, &agents, None, INITIALIZE);
    assert_eq!(status, 410, "{raw}");
    call(
        addr,
        &path_b,
        &agents,
        &mcp_b,
        "lambo_stats",
        r#"{"agent_id":"agent-i32g"}"#,
    );
    let (status, raw) = exchange(addr, "GET", "/admin/sessions", &ops, None, "");
    assert_eq!(status, 200, "{raw}");
    assert!(
        raw.contains(&format!(r#""session":"{A}","state":"erased""#)),
        "{raw}"
    );

    // A repeat is already absent.
    let (status, raw) = exchange(
        addr,
        "POST",
        &format!("/admin/s/{A}/erase"),
        &ops,
        None,
        &format!(r#"{{"confirm":"{A}"}}"#),
    );
    assert_eq!(status, 200, "{raw}");
    assert!(raw.contains(r#""already_absent":true"#), "{raw}");

    let (_, lines) = hub.stop();
    let all = lines.join("\n");
    assert!(
        !all.contains(&ops) && !all.contains(&agents),
        "a token reached the log"
    );
    // Nothing came back through the shutdown's closes, and B's data is
    // intact.
    assert_only_the_tombstone(&db, A);
    assert!(
        census(&db, B)
            .0
            .iter()
            .any(|(t, n)| t == "concepts" && *n > 0),
        "the other session keeps its rows"
    );
}

#[test]
fn erasing_the_only_session_answers_then_ends_the_process_and_a_restart_refuses() {
    let only_a = format!(r#"["{A}"]"#);
    let (_dir, cfg, db) = scratch_scoped(&only_a, &only_a);
    let rt = RuntimeDir::new();
    let hub = Hub::spawn(serve_command(&cfg, &rt, &[A]));
    let addr = hub.addr;
    let (agents, ops) = (token("agents"), token("ops"));
    let mcp = initialize(addr, "/mcp", &agents);
    derive(addr, "/mcp", &agents, &mcp, "a fact about a user");

    let (status, raw) = exchange(
        addr,
        "POST",
        &format!("/admin/s/{A}/erase"),
        &ops,
        None,
        &format!(r#"{{"confirm":"{A}"}}"#),
    );
    assert_eq!(status, 200, "{raw}");
    let (_, lines) = hub.exits_on_its_own();
    assert!(
        lines
            .iter()
            .any(|l| l.contains("this session was erased; exiting")),
        "{}",
        lines.join("\n")
    );
    assert_only_the_tombstone(&db, A);

    // A restart on the erased session refuses to start and creates nothing.
    let rt = RuntimeDir::new();
    let out = ServeChild::new(
        serve_command(&cfg, &rt, &[A])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn"),
    )
    .wait_with_output_within(Duration::from_secs(60))
    .expect("the refusal is prompt")
    .expect("wait");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(out.status.code(), Some(0), "{stderr}");
    assert!(stderr.contains("was erased"), "{stderr}");
    assert_only_the_tombstone(&db, A);
}
