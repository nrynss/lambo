//! J2: a **real subprocess** two-client test — the committed version of the
//! 2026-08-19 dogfood probe.
//!
//! That probe is what created workstream J. Two independent agent clients on one
//! machine (Claude Code and pi), each spawning its own `lambo serve` per the
//! documented stdio wiring, met the single-writer lease: it admitted one and the
//! others exited 1, in one case with no error reaching the agent at all. The
//! lease was right; the wiring turned a correct process-level lock into an
//! agent-level outage. **Agents never clash; serve processes do.**
//!
//! This file drives the shipped binary the way those clients did — two `lambo
//! serve --transport stdio` processes, one SQLite file, one session, no client
//! configuration change — and asserts the J2 outcome instead of the defect.
//!
//! `serve_single_writer_lease.rs` is its sibling and still pins fail-closed
//! cross-process enforcement, on `--transport http` where a refusal is the
//! designed outcome. Between them both halves of "one writer, many clients" are
//! covered across a real process boundary.
//!
//! SQLite on a shared file is deliberate and required: two processes can only
//! contend through a store they actually share, and the endpoint is only derived
//! for a store a second process can see.
#![cfg(all(feature = "store-sqlite", feature = "embed-fixture", unix))]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use lambo::store::{GraphStore, SqliteStore};
use lambo::types::SessionId;

const SESSION: &str = "j2-proxy-multi-client";

/// One `lambo serve` subprocess plus the plumbing to speak JSON-RPC to it.
struct Serve {
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<String>,
    pid: u32,
}

impl Serve {
    /// Spawn a serve on stdio, with stdout piped so frames can be read back.
    fn spawn(cfg: &std::path::Path, agent: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_lambo"))
            .args([
                "--config",
                cfg.to_str().unwrap(),
                "serve",
                "--session",
                SESSION,
                "--agent",
                agent,
                "--transport",
                "stdio",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {agent}: {e}"));
        let pid = child.id();
        let stdout = child.stdout.take().expect("stdout");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut lines = BufReader::new(stdout).lines();
            while let Some(Ok(line)) = lines.next() {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stdin = child.stdin.take().expect("stdin");
        Self {
            child,
            stdin,
            rx,
            pid,
        }
    }

    fn send(&mut self, frame: &str) {
        self.stdin.write_all(frame.as_bytes()).expect("write frame");
        self.stdin.write_all(b"\n").expect("write newline");
        self.stdin.flush().expect("flush frame");
    }

    /// Read frames until the one carrying `id` arrives. Bounded so a hang fails
    /// loudly rather than stalling the suite.
    fn response(&self, id: u64, what: &str) -> serde_json::Value {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "no JSON-RPC frame with id {id} ({what}) — a proxy that HANGS instead of failing \
                 honestly looks exactly like this"
            );
            let line = self
                .rx
                .recv_timeout(remaining)
                .unwrap_or_else(|e| panic!("no frame with id {id} ({what}): {e}"));
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if v.get("id").and_then(serde_json::Value::as_u64) == Some(id) {
                return v;
            }
        }
    }

    /// The MCP handshake. Through a proxy this is the first proof of life: the
    /// `serverInfo` in the answer was written by the HOLDER, not by the process
    /// this test is talking to.
    fn initialize(&mut self, id: u64) -> serde_json::Value {
        self.send(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"initialize","params":{{"protocolVersion":"2025-06-18","capabilities":{{}},"clientInfo":{{"name":"j2-test","version":"1"}}}}}}"#
        ));
        let init = self.response(id, "initialize");
        assert!(
            init["result"]["serverInfo"].is_object(),
            "initialize must be answered by a real server: {init}"
        );
        self.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        init
    }

    fn call(&mut self, id: u64, tool: &str, args: serde_json::Value) -> serde_json::Value {
        let frame = serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": { "name": tool, "arguments": args },
        });
        self.send(&frame.to_string());
        self.response(id, tool)
    }

    fn sigterm(&self) {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(self.pid.to_string())
            .status();
    }

    fn sigkill(&self) {
        let _ = Command::new("kill")
            .arg("-KILL")
            .arg(self.pid.to_string())
            .status();
    }
}

/// The text of a tool result, whether it came back as a result or an error.
fn text_of(v: &serde_json::Value) -> String {
    v.to_string()
}

/// A scratch dir plus a `lambo.toml` pointing at a SQLite file inside it.
fn scratch(tag: &str) -> (std::path::PathBuf, std::path::PathBuf, String) {
    let dir = std::env::temp_dir().join(format!(
        "lambo-j2-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let db = dir.join("lambo.db");
    let cfg = dir.join("lambo.toml");
    std::fs::write(
        &cfg,
        format!(
            "[store]\nkind = \"sqlite\"\npath = \"{}\"\n\n[embedder]\nkind = \"fixture\"\ndim = 1024\n",
            db.display()
        ),
    )
    .expect("write config");
    (dir, cfg, db.display().to_string())
}

fn provision(db: &str) {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(async {
        let store = SqliteStore::connect(db).expect("connect");
        store.init_schema().await.expect("init schema");
    });
}

fn lease_row(db: &str) -> Option<lambo::store::LeaseInfo> {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(async {
        let store = SqliteStore::connect(db).expect("connect");
        store
            .read_lease(&SessionId::from(SESSION))
            .await
            .expect("read lease")
    })
}

/// Done-when, three boxes at once: a refused `serve` starts as a proxy instead
/// of exiting 1 and every tool call including writes succeeds through it; a
/// write through the proxy is visible to that client's next recall
/// (read-your-writes across the hop); and two clients wired over stdio, with no
/// configuration change, both fully work.
///
/// It is also the committed form of the 2026-08-19 probe's second half: two
/// agents hold **distinct** soft locks through the hop, and the loser of a
/// contended lock is told who holds it.
#[test]
fn two_clients_over_stdio_both_work_through_one_hub() {
    let (dir, cfg, db) = scratch("hub");
    provision(&db);

    // A starts first and therefore becomes the hub.
    let mut a = Serve::spawn(&cfg, "agent-a");
    a.initialize(1);

    // The lease row proves A holds it AND published where it can be reached —
    // the two halves J2 added to `session_leases`.
    let row = lease_row(&db).expect("A must hold the lease");
    assert!(
        row.holder.starts_with("agent-a@"),
        "A must be the holder: {}",
        row.holder
    );
    let a_holder = row.holder.clone();
    let endpoint = row
        .endpoint
        .clone()
        .expect("a serve holder must publish its endpoint");
    assert!(
        std::path::Path::new(&endpoint).exists(),
        "the published endpoint must be a socket that exists: {endpoint}"
    );

    // B is the process that used to exit 1. Same session, same store, no
    // configuration change — exactly the wiring that broke.
    let mut b = Serve::spawn(&cfg, "agent-b");
    b.initialize(1);

    // A write through the proxy. It is durable in the HOLDER, under the
    // holder's fencing token; the proxy moved the call, not the write.
    let derived = b.call(
        2,
        "lambo_derive",
        serde_json::json!({
            "agent_id": "agent-b",
            "concepts": [{"content": "a write that crossed the proxy hop", "concept_type": "logic"}]
        }),
    );
    assert!(
        derived["error"].is_null(),
        "a write through the proxy must succeed: {}",
        text_of(&derived)
    );

    // Read-your-writes across the hop, for the client that wrote it.
    let recalled = b.call(
        3,
        "lambo_recall",
        serde_json::json!({"agent_id": "agent-b", "query": "crossed the proxy hop"}),
    );
    assert!(
        text_of(&recalled).contains("crossed the proxy hop"),
        "B must see its own write through the proxy: {}",
        text_of(&recalled)
    );

    // ONE graph, not two: the holder's own client sees the proxied client's
    // write. This is the "N clients cost one graph" claim, tested.
    let a_sees = a.call(
        2,
        "lambo_recall",
        serde_json::json!({"agent_id": "agent-a", "query": "crossed the proxy hop"}),
    );
    assert!(
        text_of(&a_sees).contains("crossed the proxy hop"),
        "the hub's own client must see the proxied client's write: {}",
        text_of(&a_sees)
    );

    // The per-call `agent_id` crossed the hop VERBATIM — J1's contract, and the
    // reason J1 gated J2. A byte pipe cannot normalise it, and this is what
    // would go red if the proxy ever started re-serializing arguments.
    let hits = &recalled["result"]["structuredContent"]["hits"];
    let node_id = hits[0]["node_id"]
        .as_str()
        .unwrap_or_else(|| panic!("recall must return a node id: {}", text_of(&recalled)))
        .to_string();

    // Two agents, two locks, through the hop. B takes the lock as agent-b...
    let taken = b.call(
        4,
        "lambo_reserve",
        serde_json::json!({"agent_id": "agent-b", "node_id": node_id, "ttl_seconds": 60}),
    );
    assert!(
        taken["error"].is_null(),
        "agent-b must take the soft lock through the proxy: {}",
        text_of(&taken)
    );
    // ...and A, a different agent on a different process, is refused and told
    // who holds it. Before J1 this was refused for the wrong reason (a foreign
    // agent id); before J2 agent-b had no server to ask at all.
    let contended = a.call(
        3,
        "lambo_reserve",
        serde_json::json!({"agent_id": "agent-a", "node_id": node_id, "ttl_seconds": 60}),
    );
    let contended = text_of(&contended);
    assert!(
        contended.contains("agent-b"),
        "the loser of a contended lock must be told who holds it: {contended}"
    );

    // The wedge invariant, at the end of a fully working session: the proxy
    // took NO lease. One holder, still A, still the same fencing token.
    let row = lease_row(&db).expect("the lease must still be held");
    assert_eq!(
        row.holder, a_holder,
        "a proxy must never take the lease — it cannot serve a client if it does"
    );
    assert_eq!(row.token, 1, "no takeover happened, so no token was minted");

    b.sigterm();
    a.sigterm();
    let _ = b.child.wait();
    let _ = a.child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Done-when: killing the holder uncleanly leaves proxies **failing honestly
/// rather than hanging**, and the wedge invariant holds while it happens — a
/// proxy that cannot serve its own client must never take the lease.
///
/// Then the recovery half: the proxy re-**reads** the lease row on the next
/// call, so a brand-new holder at a brand-new endpoint is picked up with no
/// proxy restart.
#[test]
fn a_dead_holder_leaves_the_proxy_honest_and_the_lease_unclaimed() {
    let (dir, cfg, db) = scratch("dead");
    provision(&db);

    let mut a = Serve::spawn(&cfg, "agent-a");
    a.initialize(1);
    let a_holder = lease_row(&db).expect("A holds").holder;

    let mut b = Serve::spawn(&cfg, "agent-b");
    b.initialize(1);
    // Prove the hop works before breaking it, so a failure below is the kill
    // and not a broken proxy.
    let ok = b.call(
        2,
        "lambo_recall",
        serde_json::json!({"agent_id": "agent-b", "query": "anything"}),
    );
    assert!(ok["error"].is_null(), "baseline call: {}", text_of(&ok));

    // SIGKILL: no close, no lease release, no socket cleanup — the unclean
    // death the TTL exists for.
    a.sigkill();
    let _ = a.child.wait();

    // The honest failure. `response` is bounded, so a proxy that hangs fails
    // this test rather than stalling the suite.
    let refused = b.call(
        3,
        "lambo_recall",
        serde_json::json!({"agent_id": "agent-b", "query": "after the holder died"}),
    );
    let refused = text_of(&refused);
    assert!(
        refused.contains("NOTHING WAS READ OR WRITTEN"),
        "the caller must be told nothing happened: {refused}"
    );
    assert!(
        refused.contains("Do not block on memory"),
        "and told it is safe to carry on: {refused}"
    );
    assert!(
        !refused.contains(".sock"),
        "N4: no socket path may reach the model: {refused}"
    );

    // THE WEDGE INVARIANT. B is still running and still cannot serve a client
    // whose session died with A, so it must not have taken the lease — a
    // heartbeating holder that answers nothing wedges the whole machine, which
    // is strictly worse than the exit-1 J2 replaced.
    let row = lease_row(&db).expect("A's row outlives A until the TTL");
    assert_eq!(
        row.holder, a_holder,
        "the proxy must NOT have claimed the lease: {}",
        row.holder
    );
    assert!(
        !row.holder.starts_with("agent-b@"),
        "the proxy must never appear as the holder: {}",
        row.holder
    );

    // Recovery. Clear the dead holder's row the way an operator would (the
    // documented override) so a new holder can start now rather than in 45s,
    // then let C become the hub.
    {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let store = SqliteStore::connect(&db).unwrap();
            store
                .release_lease(&SessionId::from(SESSION), &{
                    // Release is holder-scoped, so reconstruct the dead holder's
                    // identity from the row rather than inventing one.
                    let mut parts = a_holder.rsplitn(2, '#');
                    let pid: u32 = parts.next().unwrap().parse().unwrap();
                    let head = parts.next().unwrap();
                    let (agent, host) = head.rsplit_once('@').unwrap();
                    lambo::store::LeaseHolder {
                        agent: lambo::types::AgentId::new(agent),
                        pid,
                        host: host.to_string(),
                        endpoint: None,
                    }
                })
                .await
                .unwrap();
        });
    }
    let mut c = Serve::spawn(&cfg, "agent-c");
    c.initialize(1);
    let new_row = lease_row(&db).expect("C holds now");
    assert!(
        new_row.holder.starts_with("agent-c@"),
        "C must be the new holder: {}",
        new_row.holder
    );

    // B was never restarted. Its next call re-reads the lease row, finds C, and
    // works — which is the whole reason the address is never cached.
    let healed = b.call(
        4,
        "lambo_recall",
        serde_json::json!({"agent_id": "agent-b", "query": "after a new holder arrived"}),
    );
    assert!(
        healed["error"].is_null(),
        "the proxy must recover onto the new holder without restarting: {}",
        text_of(&healed)
    );

    b.sigterm();
    c.sigterm();
    let _ = b.child.wait();
    let _ = c.child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

/// J2-R1-1, the reviewer's exact scenario: a call is **already inside the
/// holder** when the holder stops answering.
///
/// This is the case the round-1 review reproduced from unmutated pump code and
/// that the shipped pump wedged on permanently. The frame had been written
/// successfully, so nothing on the write-failure path fired; the holder's
/// `Closed` only logged and cleared the writer; and the reconnect lived in the
/// client arm, so a client politely awaiting its response never sent the byte
/// that would have triggered one. No answer, no error, no recovery — for a
/// client with no per-call timeout, forever.
///
/// **The holder here is the test itself, not a `lambo serve`, and that is the
/// point.** The window this pins is the one between "the holder has the frame"
/// and "the holder has answered it" — hundreds of milliseconds on a real write,
/// but not a window a test can *aim* at by racing a signal against a real
/// server. A hand-written holder that reads the call and then drops the
/// connection lands in it every time. Everything else is real: a real `lambo
/// serve` subprocess, a real lease row it loses to, all three `proxyable`
/// checks passing against a real socket, and the client's own frames crossing
/// the byte pipe.
///
/// What must arrive: a JSON-RPC error keyed to the **in-flight id**, promptly,
/// saying the outcome is unknown. Not "nothing happened" — the frame reached the
/// holder, and a model told nothing happened would re-derive a write that may
/// already have landed.
#[test]
fn a_call_in_flight_when_the_holder_dies_is_answered_rather_than_lost() {
    use std::io::Write as _;
    use std::os::unix::net::UnixListener;

    let (dir, cfg, db) = scratch("inflight");
    provision(&db);

    // The endpoint the spawned serve will derive for this session and store.
    // Derived here through the same public function it uses, so the socket the
    // fake holder binds is the one the proxy's `proxyable` check demands.
    let store_cfg = lambo::store::StoreConfig {
        kind: lambo::store::StoreKind::Sqlite,
        path: Some(db.clone()),
        ..lambo::store::StoreConfig::default()
    };
    let endpoint = lambo::mcp::SessionEndpoint::for_store(SESSION, &store_cfg)
        .expect("a file-backed store derives an endpoint")
        .expect("a file-backed store is shareable");
    let sock = endpoint.path().to_path_buf();
    let _ = std::fs::remove_file(&sock);

    // Take the lease as the fake holder, publishing that endpoint — the row the
    // spawned serve loses to, and the row it checks before dialling.
    let holder =
        lambo::store::LeaseHolder::for_this_process(&lambo::types::AgentId::new("fake-holder"))
            .reachable_at(endpoint.published());
    {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let store = SqliteStore::connect(&db).unwrap();
            let outcome = store
                .acquire_lease(&SessionId::from(SESSION), &holder, Duration::from_secs(45))
                .await
                .unwrap();
            assert!(
                matches!(outcome, lambo::store::LeaseOutcome::Acquired(_)),
                "the fake holder must actually hold the lease: {outcome:?}"
            );
        });
    }

    // Bind before spawning: `resolve_role` probes the endpoint with a real
    // connect before it commits to proxying, so an unbound socket would make the
    // serve wait out the TTL instead of becoming a proxy.
    let listener = UnixListener::bind(&sock).expect("bind the fake holder's endpoint");
    let holder_thread = std::thread::spawn(move || {
        // Connection 1 is `resolve_role`'s probe, which closes immediately.
        // Connection 2 is the pump's. Accept until one of them hands us a call.
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let mut w = stream.try_clone().expect("clone the holder side");
            let mut reader = std::io::BufReader::new(stream);
            let mut killed = false;
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                    continue;
                };
                match v.get("method").and_then(serde_json::Value::as_str) {
                    Some("initialize") => {
                        // Answer the handshake, so the proxy's client reaches the
                        // state that matters: a live session with a call to make.
                        let reply = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": v.get("id").cloned().unwrap_or(serde_json::Value::Null),
                            "result": {
                                "protocolVersion": "2025-06-18",
                                "capabilities": {},
                                "serverInfo": {"name": "j2-fake-holder", "version": "1"},
                            },
                        });
                        writeln!(w, "{reply}").expect("answer initialize");
                        w.flush().expect("flush initialize");
                    }
                    Some("notifications/initialized") => {}
                    Some(_) => {
                        // A REQUEST IS NOW IN FLIGHT. Die without answering it —
                        // exactly what a holder SIGKILLed mid-embed does.
                        killed = true;
                        break;
                    }
                    None => {}
                }
            }
            drop(w);
            drop(reader);
            if killed {
                // Stop listening too: the holder is gone, not merely quiet.
                break;
            }
        }
    });

    let mut b = Serve::spawn(&cfg, "agent-b");
    b.initialize(1);

    // The call that will be lost. Sent, not `call`ed, so the assertion below can
    // time how long the answer took.
    let started = std::time::Instant::now();
    b.send(
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {
                "name": "lambo_derive",
                "arguments": {
                    "agent_id": "agent-b",
                    "concepts": [{"content": "a write lost with its holder", "concept_type": "logic"}],
                },
            },
        })
        .to_string(),
    );

    // THE ASSERTION J2 EXISTS FOR. `response` is bounded, so the pre-fix wedge
    // fails here rather than stalling the suite.
    let answer = b.response(2, "a call in flight when the holder died");
    let elapsed = started.elapsed();
    assert!(
        !answer["error"].is_null(),
        "an in-flight call lost with its holder must come back as an error: {answer}"
    );
    let msg = answer["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    // Write-uncertainty, not the "nothing happened" claim: this frame reached
    // the holder, so a model must not be told it did not.
    assert!(
        msg.contains("UNKNOWN"),
        "the caller must be told the outcome is unknown, not that nothing happened: {msg}"
    );
    assert!(
        !msg.contains("NOTHING WAS READ OR WRITTEN"),
        "the never-forwarded text must NOT be reused for a forwarded call: {msg}"
    );
    assert!(
        msg.contains("recall before re-deriving"),
        "and told how to resolve the uncertainty safely: {msg}"
    );
    // N4: a tool error reaches the model, so no path and no store URL.
    assert!(!msg.contains(".sock"), "N4: no socket path: {msg}");
    assert!(!msg.contains("://"), "N4: no store URL: {msg}");
    // Promptly. The pre-fix behaviour was an answer that never came; an answer
    // that only arrives at the harness bound is the same defect wearing a
    // timeout.
    assert!(
        elapsed < Duration::from_secs(10),
        "the error must arrive when the connection drops, not at some timeout: {elapsed:?}"
    );

    b.sigterm();
    let _ = b.child.wait();
    let _ = holder_thread.join();
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_dir_all(&dir);
}
