//! #32 PR 4: the session registry, in-process.
//!
//! Two pinned sessions over one store, served through the real router and
//! guards on a loopback port, driven by a minimal streamable-HTTP MCP client:
//! a request reaches only its session; recall, inspect, saints, stats and GC
//! never cross sessions; `/mcp` is the default session; an unknown id is the
//! uniform 404; the MCP-session cap is process-wide. Then the lease-loss
//! policies, the shutdown's lease release, and sixteen dirty SQLite sessions
//! closing inside `SHUTDOWN_BUDGET`.
//!
//! Every endpoint socket lives in a short scratch directory: a registry that
//! derives endpoints is handed a store config whose derivation is pointed at
//! it, and the router tests use the in-memory store, for which no endpoint is
//! derived at all. Nothing here binds 7700.

use super::*;
use crate::embed::FixtureEmbedder;
use crate::mcp::serve::pinned::check_pinned;
use crate::mcp::serve::registry::{
    Acquired, LeaseLossPolicy, SessionAttacher, SessionRegistry, PINNED_RETRY,
};
use crate::mcp::serve::transport::session_router;
use crate::store::{GraphStore, MemoryStore, StoreConfig};
use crate::types::EmbeddingContract;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The marker one session writes; the other must never see it.
const MARKER_A: &str = "registry isolation marker alpha";
const MARKER_B: &str = "registry isolation marker beta";

fn backends_over(store: Box<dyn GraphStore>, config: crate::Config) -> ResolvedBackends {
    ResolvedBackends {
        store,
        embedder: Box::new(FixtureEmbedder::new()),
        store_cfg: StoreConfig::default(),
        embedder_cfg: Default::default(),
        embedding: EmbeddingContract {
            kind: "fixture".into(),
            model: None,
            dim: 1024,
        },
        allow_embedding_mismatch: false,
        config,
    }
}

/// A test config: a fast daemon tick, so a GC due on mutations runs within
/// the test, and the background flush out of the way.
fn fast_config(gc_interval: u64) -> crate::Config {
    crate::Config {
        daemon_tick_interval: Duration::from_millis(50),
        gc_interval,
        backend_flush_interval: Duration::from_secs(3_600),
        ..crate::Config::default()
    }
}

/// A `DetachSession` registry over `backends`, hosting `sessions`, with
/// every pinned session acquired and admitted. Held-elsewhere sessions are
/// marked held, as `serve_pinned` does.
async fn pinned_registry(
    sessions: &[&str],
    backends: ResolvedBackends,
    max_sessions: usize,
) -> Arc<SessionRegistry> {
    let registry = new_registry(sessions, backends, max_sessions);
    for id in sessions {
        attach_or_hold(&registry, id).await;
    }
    registry.mark_started();
    registry
}

/// Acquire pinned session `id`: admit it, or mark it held elsewhere.
async fn attach_or_hold(registry: &Arc<SessionRegistry>, id: &str) {
    match registry.acquire(id).await.expect("acquire") {
        Acquired::Attached(mem, endpoint) => {
            registry.admit(mem, endpoint);
        }
        Acquired::Held(held) => registry.mark_held(id, &held).await,
    }
}

/// A `DetachSession` (or, for one, `ExitProcess`) registry over `backends`
/// hosting `sessions`, nothing attached yet.
fn new_registry(
    sessions: &[&str],
    backends: ResolvedBackends,
    max_sessions: usize,
) -> Arc<SessionRegistry> {
    let opts = ServeOptions::new(sessions[0], "agent-a");
    let early = EarlyShutdown::unarmed();
    let store_cfg = backends.store_cfg.clone();
    let template = super::builder::serve_builder(
        &opts,
        backends,
        None,
        None,
        early.clone(),
        Some(crate::writeq::EmbedderCalibration::new()),
    );
    SessionRegistry::new(
        sessions.iter().map(|s| s.to_string()).collect(),
        Some(sessions[0].to_string()),
        LeaseLossPolicy::for_pinned(sessions.len()),
        Some(SessionAttacher {
            template,
            store_cfg,
            ledger: None,
            max_sessions,
            agent: "agent-a".into(),
        }),
        early,
    )
}

/// Serve `registry` behind the real guards on a loopback port.
async fn serve_router(registry: &Arc<SessionRegistry>, max_sessions: usize) -> SocketAddr {
    let guard = HttpGuard {
        auth: None,
        max_sessions,
        live: registry.clone(),
        rate: None,
    };
    let app = session_router(Arc::clone(registry))
        .layer(axum::middleware::from_fn_with_state(guard, guard_request));
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

/// One HTTP exchange, read to the end.
struct Reply {
    status: u16,
    head: String,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<String> {
        self.head.lines().find_map(|line| {
            let (k, v) = line.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case(name)
                .then(|| v.trim().to_string())
        })
    }

    /// The JSON-RPC message in the body: the `data:` line of an SSE body,
    /// else the body itself.
    fn message(&self) -> serde_json::Value {
        let json = self
            .body
            .lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .map(str::trim)
            .find(|l| l.starts_with('{'))
            .unwrap_or(self.body.trim());
        serde_json::from_str(json)
            .unwrap_or_else(|e| panic!("no JSON-RPC message ({e}) in {:?}", self.body))
    }
}

/// Undo HTTP/1.1 chunked transfer encoding.
fn dechunk(mut raw: &str) -> String {
    let mut out = String::new();
    loop {
        let Some((size, rest)) = raw.split_once("\r\n") else {
            return out;
        };
        let size = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
        if size == 0 || rest.len() < size {
            return out;
        }
        out.push_str(&rest[..size]);
        raw = rest[size..].strip_prefix("\r\n").unwrap_or(&rest[size..]);
    }
}

async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    mcp_session: Option<&str>,
    body: &str,
) -> Reply {
    let mut sock = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAccept: application/json, \
         text/event-stream\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n",
        body.len()
    );
    if let Some(id) = mcp_session {
        head.push_str(&format!("Mcp-Session-Id: {id}\r\n"));
    }
    head.push_str("\r\n");
    sock.write_all(head.as_bytes()).await.expect("write head");
    sock.write_all(body.as_bytes()).await.expect("write body");
    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), sock.read_to_end(&mut raw))
        .await
        .expect("the reply completes")
        .expect("read");
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status line: {text:?}"));
    let chunked = head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked");
    Reply {
        status,
        head: head.to_string(),
        body: if chunked {
            dechunk(body)
        } else {
            body.to_string()
        },
    }
}

/// Initialize an MCP session at `path`: its id and the `initialize` result.
async fn initialize(addr: SocketAddr, path: &str) -> (String, serde_json::Value) {
    let reply = http(
        addr,
        "POST",
        path,
        None,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"registry-test","version":"1"}}}"#,
    )
    .await;
    assert_eq!(reply.status, 200, "initialize at {path}: {}", reply.body);
    let id = reply.header("mcp-session-id").expect("an MCP session id");
    let result = reply.message()["result"].clone();
    let ack = http(
        addr,
        "POST",
        path,
        Some(&id),
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    )
    .await;
    assert!(ack.status < 300, "initialized: {} {}", ack.status, ack.body);
    (id, result)
}

/// Call a tool and return the `result` (or panic with the error).
async fn call(
    addr: SocketAddr,
    path: &str,
    mcp_session: &str,
    name: &str,
    args: serde_json::Value,
) -> serde_json::Value {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": {"name": name, "arguments": args},
    })
    .to_string();
    let reply = http(addr, "POST", path, Some(mcp_session), &body).await;
    assert_eq!(reply.status, 200, "{name} at {path}: {}", reply.body);
    let message = reply.message();
    assert!(message.get("error").is_none(), "{name}: {message}");
    message["result"].clone()
}

async fn derive(addr: SocketAddr, path: &str, sid: &str, contents: &[&str]) {
    let concepts: Vec<_> = contents
        .iter()
        .map(|c| serde_json::json!({"content": c, "concept_type": "entity"}))
        .collect();
    let out = call(
        addr,
        path,
        sid,
        "lambo_derive",
        serde_json::json!({"agent_id": "agent-a", "concepts": concepts}),
    )
    .await;
    let receipt = out["structuredContent"]["receipt"]
        .as_str()
        .expect("a receipt")
        .to_string();
    // Wait for the write to apply, so the reads below see it.
    call(
        addr,
        path,
        sid,
        "lambo_stats",
        serde_json::json!({"agent_id": "agent-a", "receipt": receipt, "wait_ms": 10_000}),
    )
    .await;
}

async fn stats(addr: SocketAddr, path: &str, sid: &str) -> serde_json::Value {
    call(
        addr,
        path,
        sid,
        "lambo_stats",
        serde_json::json!({"agent_id": "agent-a"}),
    )
    .await["structuredContent"]
        .clone()
}

/// Two pinned sessions, each reached only at its own path; every read tool
/// and GC stay inside the session the request addressed.
#[tokio::test]
async fn two_pinned_sessions_are_served_concurrently_and_never_cross() {
    let store: Box<dyn GraphStore> = Box::new(MemoryStore::new());
    // GC is due after 20 mutations: session a takes many more, b far fewer.
    let registry = pinned_registry(
        &["reg-a", "reg-b"],
        backends_over(store, fast_config(20)),
        32,
    )
    .await;
    let addr = serve_router(&registry, 32).await;

    let (a, init_a) = initialize(addr, "/mcp/s/reg-a").await;
    let (b, init_b) = initialize(addr, "/mcp/s/reg-b").await;
    assert!(
        init_a["instructions"]
            .as_str()
            .unwrap()
            .contains("session 'reg-a'"),
        "{init_a}"
    );
    assert!(
        init_b["instructions"]
            .as_str()
            .unwrap()
            .contains("session 'reg-b'"),
        "{init_b}"
    );

    // Both sessions written, concurrently.
    let many: Vec<String> = (0..12).map(|i| format!("{MARKER_A} {i}")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    tokio::join!(
        derive(addr, "/mcp/s/reg-a", &a, &many),
        derive(addr, "/mcp/s/reg-b", &b, &[MARKER_B]),
    );

    // GC is forced in a: wait for its sweep.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let s = stats(addr, "/mcp/s/reg-a", &a).await;
        if !s["gc"]["last_sweep"].is_null() {
            break;
        }
        assert!(Instant::now() < deadline, "a never swept: {s}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Stats: each session counts only its own concepts; b never swept.
    let sa = stats(addr, "/mcp/s/reg-a", &a).await;
    let sb = stats(addr, "/mcp/s/reg-b", &b).await;
    assert_eq!(sa["session"], "reg-a");
    assert_eq!(sb["session"], "reg-b");
    assert_eq!(sb["concept_count"], 1, "{sb}");
    assert!(sa["concept_count"].as_u64().unwrap() >= 12, "{sa}");
    assert!(sb["gc"]["last_sweep"].is_null(), "a's GC reached b: {sb}");

    // Recall, inspect and saints answer from the addressed session only.
    for (path, sid, own, other) in [
        ("/mcp/s/reg-a", &a, MARKER_A, MARKER_B),
        ("/mcp/s/reg-b", &b, MARKER_B, MARKER_A),
    ] {
        let recalled = call(
            addr,
            path,
            sid,
            "lambo_recall",
            serde_json::json!({"agent_id": "agent-a", "query": "registry isolation marker"}),
        )
        .await
        .to_string();
        assert!(
            recalled.contains(own),
            "{path} recall lost its own: {recalled}"
        );
        assert!(
            !recalled.contains(other),
            "{path} recall crossed: {recalled}"
        );

        let miss = call(
            addr,
            path,
            sid,
            "lambo_inspect",
            serde_json::json!({"agent_id": "agent-a", "focus": other}),
        )
        .await;
        assert_eq!(miss["isError"], true, "{path} inspected the other: {miss}");

        let saints = call(
            addr,
            path,
            sid,
            "lambo_saints",
            serde_json::json!({"agent_id": "agent-a"}),
        )
        .await
        .to_string();
        assert!(!saints.contains(other), "{path} saints crossed: {saints}");
    }

    // An MCP session minted by a is unknown to b.
    let crossed = http(
        addr,
        "POST",
        "/mcp/s/reg-b",
        Some(&a),
        r#"{"jsonrpc":"2.0","id":9,"method":"tools/list"}"#,
    )
    .await;
    assert!(
        crossed.status >= 400,
        "b served a's MCP session: {} {}",
        crossed.status,
        crossed.body
    );

    for session in registry.close_set().await {
        session.mem.close().await.expect("close");
    }
}

/// `/mcp` is the default session; an unknown, malformed or percent-encoded
/// id is the uniform 404 on every method.
#[tokio::test]
async fn the_bare_path_is_the_default_and_unknown_ids_are_the_uniform_404() {
    let store: Box<dyn GraphStore> = Box::new(MemoryStore::new());
    let registry = pinned_registry(
        &["reg-default", "reg-other"],
        backends_over(store, fast_config(1_000)),
        32,
    )
    .await;
    let addr = serve_router(&registry, 32).await;

    let (_, init) = initialize(addr, "/mcp").await;
    assert!(
        init["instructions"]
            .as_str()
            .unwrap()
            .contains("session 'reg-default'"),
        "{init}"
    );

    let unrouted = http(addr, "GET", "/not/routed", None, "").await;
    assert_eq!(unrouted.status, 404);
    for method in ["GET", "POST", "DELETE"] {
        for path in [
            "/mcp/s/reg-unknown",
            "/mcp/s/.hidden",
            "/mcp/s/reg%2Ddefault",
            "/mcp/s/a%2Fb",
        ] {
            let reply = http(addr, method, path, None, "").await;
            assert_eq!(reply.status, 404, "{method} {path}: {}", reply.body);
            assert_eq!(reply.body, unrouted.body, "{method} {path}");
        }
    }

    for session in registry.close_set().await {
        session.mem.close().await.expect("close");
    }
}

/// `--max-sessions` counts the MCP sessions of every attached session.
#[tokio::test]
async fn the_mcp_session_cap_is_process_wide() {
    let store: Box<dyn GraphStore> = Box::new(MemoryStore::new());
    let registry = pinned_registry(
        &["reg-cap-a", "reg-cap-b"],
        backends_over(store, fast_config(1_000)),
        32,
    )
    .await;
    let addr = serve_router(&registry, 1).await;

    initialize(addr, "/mcp/s/reg-cap-a").await;
    let refused = http(
        addr,
        "POST",
        "/mcp/s/reg-cap-b",
        None,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#,
    )
    .await;
    assert_eq!(refused.status, 503, "{}", refused.body);
    assert!(
        refused.body.contains("concurrent-session cap"),
        "{}",
        refused.body
    );

    for session in registry.close_set().await {
        session.mem.close().await.expect("close");
    }
}

/// The policy is derived from the pinned count, in one place.
#[test]
fn one_pinned_session_exits_on_lease_loss_and_more_detach() {
    assert_eq!(LeaseLossPolicy::for_pinned(1), LeaseLossPolicy::ExitProcess);
    assert_eq!(
        LeaseLossPolicy::for_pinned(2),
        LeaseLossPolicy::DetachSession
    );
    assert_eq!(
        LeaseLossPolicy::for_pinned(16),
        LeaseLossPolicy::DetachSession
    );
}

/// Under `DetachSession`, a lost lease detaches that session only: its slot
/// answers 503 with `Retry-After`, its close refuses to flush, and the other
/// session keeps serving.
#[tokio::test]
async fn a_lost_lease_detaches_one_session_and_the_other_keeps_serving() {
    let (logs, _guard) = crate::test_util::capture_logs(tracing::Level::INFO);
    let store: Box<dyn GraphStore> = Box::new(MemoryStore::new());
    let registry = pinned_registry(
        &["reg-loss-a", "reg-loss-b"],
        backends_over(store, fast_config(1_000)),
        32,
    )
    .await;
    let addr = serve_router(&registry, 32).await;
    let (b, _) = initialize(addr, "/mcp/s/reg-loss-b").await;

    let lost = registry
        .attached()
        .into_iter()
        .find(|s| s.id().as_str() == "reg-loss-a")
        .expect("a attached");
    lost.mem.simulate_lease_loss_to("another-writer@host#9");
    drop(lost);

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if logs
            .lines()
            .iter()
            .any(|l| l.contains("session detach finished") && l.contains("reg-loss-a"))
        {
            break;
        }
        assert!(Instant::now() < deadline, "no detach: {:?}", logs.lines());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let refused = http(addr, "POST", "/mcp/s/reg-loss-a", None, "{}").await;
    assert_eq!(refused.status, 503, "{}", refused.body);
    assert!(refused.header("retry-after").is_some(), "{}", refused.head);
    assert_eq!(registry.attached().len(), 1);
    assert_eq!(registry.attached()[0].id().as_str(), "reg-loss-b");
    // The detached session's outcome is the honest, named tail-lost line.
    assert!(
        logs.lines()
            .iter()
            .any(|l| l.contains("final flush failed") && l.contains("reg-loss-a")),
        "{:?}",
        logs.lines()
    );

    // b still serves, end to end.
    derive(addr, "/mcp/s/reg-loss-b", &b, &[MARKER_B]).await;
    assert_eq!(
        stats(addr, "/mcp/s/reg-loss-b", &b).await["concept_count"],
        1
    );

    for session in registry.close_set().await {
        session.mem.close().await.expect("close");
    }
}

/// The shutdown's close over a registry releases every lease: each row
/// reads back `lambo:released` and keeps its fencing token (#23).
#[tokio::test]
async fn closing_the_registry_releases_every_lease_and_keeps_the_tokens() {
    let registry = pinned_registry(
        &["reg-rel-a", "reg-rel-b"],
        backends_over(Box::new(MemoryStore::new()), fast_config(1_000)),
        32,
    )
    .await;
    // The one store every session shares.
    let store = Arc::clone(registry.attached()[0].mem.store());
    let mut tokens = Vec::new();
    for id in ["reg-rel-a", "reg-rel-b"] {
        let row = store
            .read_lease(&crate::types::SessionId::new(id))
            .await
            .expect("read")
            .expect("a lease row");
        tokens.push(row.token);
    }

    let sessions = registry.close_set().await;
    assert_eq!(sessions.len(), 2);
    let closing: Vec<_> = sessions.iter().map(|s| s.closing()).collect();
    let closed = close_sessions(
        &closing,
        &EarlyShutdown::unarmed(),
        &ShutdownProgress::new(),
    )
    .await;
    closed.report().expect("every tail durable");

    for (id, token) in ["reg-rel-a", "reg-rel-b"].into_iter().zip(tokens) {
        let row = store
            .read_lease(&crate::types::SessionId::new(id))
            .await
            .expect("read")
            .expect("the row is kept");
        assert_eq!(row.holder, crate::store::lease::RELEASED_HOLDER, "{id}");
        assert_eq!(row.token, token, "{id}: the token is kept");
    }
    // No attach starts once the set is taken.
    assert!(registry.close_set().await.is_empty());
}

/// The CLI's pinned plan: stdio owns one; HTTP pins the ordered union, the
/// default is pinned, and the cap holds after the union.
#[test]
fn the_pinned_plan_follows_the_cli_and_the_serve_table() {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let none = crate::config::ServeConfig::default();
    let table = crate::config::ServeConfig {
        sessions: s(&["b", "c"]),
        ..Default::default()
    };

    let one = pin_sessions(&s(&["a"]), &table, Transport::Stdio).expect("stdio one");
    assert_eq!(one.sessions, s(&["a"]), "stdio ignores [serve] sessions");
    assert!(pin_sessions(&s(&[]), &table, Transport::Stdio).is_err());
    assert!(pin_sessions(&s(&["a", "b"]), &none, Transport::Stdio).is_err());

    let http = pin_sessions(&s(&["a", "c"]), &table, Transport::Http).expect("http");
    assert_eq!(http.sessions, s(&["a", "c", "b"]));
    assert_eq!(http.default, "a");
    let from_table = pin_sessions(&s(&[]), &table, Transport::Http).expect("table only");
    assert_eq!(from_table.default, "b");
    assert!(pin_sessions(&s(&[]), &none, Transport::Http).is_err());

    let defaulted = crate::config::ServeConfig {
        default_session: Some("c".into()),
        ..table.clone()
    };
    assert_eq!(
        pin_sessions(&s(&[]), &defaulted, Transport::Http)
            .expect("default")
            .default,
        "c"
    );
    let unpinned_default = crate::config::ServeConfig {
        default_session: Some("z".into()),
        ..table.clone()
    };
    let err = pin_sessions(&s(&[]), &unpinned_default, Transport::Http)
        .expect_err("an unpinned default is refused")
        .to_string();
    assert!(err.contains("default_session"), "{err}");

    let capped = crate::config::ServeConfig {
        max_attached: Some(2),
        ..table
    };
    let err = pin_sessions(&s(&["a"]), &capped, Transport::Http)
        .expect_err("three pinned over a cap of two")
        .to_string();
    assert!(err.contains("max_attached"), "{err}");
}

/// `serve`'s own check: one loosely named session is fine; with two, each
/// must be addressable; stdio pins one; the default must be pinned.
#[test]
fn serve_checks_the_sessions_it_is_handed() {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    check_pinned("has space", &s(&["has space"]), Transport::Http).expect("one loose name");
    let err = check_pinned("ok", &s(&["ok", "has space"]), Transport::Http)
        .expect_err("two need the strict charset")
        .to_string();
    assert!(err.contains("cannot be addressed by URL"), "{err}");
    assert!(check_pinned("a", &s(&["a", "b"]), Transport::Stdio).is_err());
    assert!(check_pinned("z", &s(&["a", "b"]), Transport::Http).is_err());
    assert!(check_pinned("a", &s(&["a", "a"]), Transport::Http).is_err());
}

/// Design §3.5 / R2: sixteen dirty sessions sharing one SQLite connection
/// close inside `SHUTDOWN_BUDGET`, every tail durable.
#[cfg(feature = "store-sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sixteen_dirty_sqlite_sessions_close_inside_the_shutdown_budget() {
    use crate::store::SqliteStore;
    let dir = crate::test_util::ScratchDir::new("lambo-reg16");
    let path = dir.join("sixteen.sqlite");
    let store = SqliteStore::connect(path.to_str().expect("utf-8")).expect("connect");
    store.init_schema().await.expect("init_schema");
    let ids: Vec<String> = (0..16).map(|i| format!("reg-16-{i:02}")).collect();
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let registry = pinned_registry(
        &refs,
        backends_over(Box::new(store), fast_config(100_000)),
        32,
    )
    .await;
    let sessions = registry.attached();
    assert_eq!(sessions.len(), 16, "every pinned session attached");

    // Dirty every session: writes applied to the graph, nothing flushed (the
    // background flush interval is an hour).
    for session in &sessions {
        for i in 0..8 {
            let text = format!("dirty write {i} in {}", session.id());
            let action = crate::graph::action::Action {
                event_time: None,
                action: &text,
                produces: &[],
                modifies: &[],
                depends_on: &[],
            };
            session.mem.record_action(&action).expect("write");
        }
        assert!(
            session.mem.stats().log_depth > 0,
            "{} is dirty",
            session.id()
        );
    }
    drop(sessions);

    let started = Instant::now();
    let set = registry.close_set().await;
    let closing: Vec<_> = set.iter().map(|s| s.closing()).collect();
    let closed = close_sessions(
        &closing,
        &EarlyShutdown::unarmed(),
        &ShutdownProgress::new(),
    )
    .await;
    let elapsed = started.elapsed();
    closed.report().expect("every tail durable");
    assert!(
        elapsed < SHUTDOWN_BUDGET,
        "16 SQLite closes took {elapsed:?}, over SHUTDOWN_BUDGET {SHUTDOWN_BUDGET:?}"
    );

    // Durable: a fresh reader sees every session's writes.
    let reader = SqliteStore::connect(path.to_str().expect("utf-8")).expect("reconnect");
    for id in &ids {
        let loaded =
            crate::store::load::load_session_async(&reader, &crate::types::SessionId::new(id))
                .await
                .expect("load");
        assert!(!loaded.graph.is_empty(), "{id}: tail not durable");
    }
}

/// A pinned session another writer holds at startup is served as 503 and
/// taken back in the background once that writer releases it (design §3.2:
/// no election wait on the startup or request path).
#[tokio::test]
async fn a_pinned_session_held_elsewhere_is_re_elected_in_the_background() {
    let registry = new_registry(
        &["reg-held-a", "reg-held-b"],
        backends_over(Box::new(MemoryStore::new()), fast_config(1_000)),
        32,
    );
    attach_or_hold(&registry, "reg-held-a").await;
    // Another writer takes b on the same store before the registry tries.
    let other = crate::memory::Memory::builder()
        .session("reg-held-b")
        .agent("another-writer")
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::clone(registry.attached()[0].mem.store()))
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn crate::embed::Embedder>)
        .embedding_contract(EmbeddingContract {
            kind: "fixture".into(),
            model: None,
            dim: 1024,
        })
        .build()
        .await
        .expect("the other writer attaches");
    attach_or_hold(&registry, "reg-held-b").await;
    registry.mark_started();
    registry.spawn_retry_loop();
    let addr = serve_router(&registry, 32).await;

    assert_eq!(registry.attached().len(), 1, "only a attached");
    let held = http(addr, "POST", "/mcp/s/reg-held-b", None, "{}").await;
    assert_eq!(held.status, 503, "{}", held.body);
    assert!(held.header("retry-after").is_some(), "{}", held.head);

    other.close().await.expect("the other writer releases b");
    let deadline = Instant::now() + PINNED_RETRY * 3;
    while registry.attached().len() < 2 {
        assert!(Instant::now() < deadline, "b was never re-elected");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    initialize(addr, "/mcp/s/reg-held-b").await;

    for session in registry.close_set().await {
        session.mem.close().await.expect("close");
    }
}

/// The real multi-session serve, in-process (#32 review M1/M2).
mod pinned_serve;
