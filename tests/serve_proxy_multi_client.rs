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

/// The J3 piggyback content block(s) of a tool response, joined — empty when
/// the response carries none.
fn piggyback_of(v: &serde_json::Value) -> String {
    v["result"]["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b["text"].as_str())
                .filter(|t| t.starts_with("write receipts"))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
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

/// Release a lease row the way the documented operator override does, given the
/// `agent@host#pid` token from the row itself.
///
/// Release is holder-scoped, so the dead (or unwanted) holder's identity is
/// reconstructed from the row rather than invented.
fn release_row(db: &str, holder_token: &str) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let store = SqliteStore::connect(db).unwrap();
        let mut parts = holder_token.rsplitn(2, '#');
        let pid: u32 = parts.next().unwrap().parse().unwrap();
        let head = parts.next().unwrap();
        let (agent, host) = head.rsplit_once('@').unwrap();
        store
            .release_lease(
                &SessionId::from(SESSION),
                &lambo::store::LeaseHolder {
                    agent: lambo::types::AgentId::new(agent),
                    pid,
                    host: host.to_string(),
                    endpoint: None,
                },
            )
            .await
            .unwrap();
    });
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

    // **J3 through the proxy.** The ack now precedes the write, so it carries a
    // receipt id — and the receipt has to survive the hop. Nothing in
    // `src/mcp/proxy.rs` knows what a receipt is: the byte pipe forwards the
    // response line untouched, which is exactly why J3 needed no transport
    // change. This is the assertion that says so.
    let receipt = derived["result"]["structuredContent"]["receipt"]
        .as_str()
        .unwrap_or_else(|| {
            panic!(
                "the proxied ack must carry a receipt: {}",
                text_of(&derived)
            )
        })
        .to_string();
    assert!(
        receipt.starts_with("lwr1."),
        "the receipt must cross the hop verbatim, not re-rendered: {receipt}"
    );

    // A second write, so the piggyback has something to carry that is not the
    // receipt being asked about. The lane is per-agent FIFO with one consumer,
    // so this one settling means the first one already did — which is what
    // makes the assertions below deterministic rather than a race.
    let second = b.call(
        20,
        "lambo_derive",
        serde_json::json!({
            "agent_id": "agent-b",
            "concepts": [{"content": "a second proxied write, for the piggyback", "concept_type": "logic"}]
        }),
    );
    let receipt_two = second["result"]["structuredContent"]["receipt"]
        .as_str()
        .unwrap_or_else(|| panic!("the second ack must carry a receipt: {}", text_of(&second)))
        .to_string();

    // Waiting on it through the proxy is the opt-in synchrony, and it is what
    // makes the read below deterministic instead of a race against the
    // holder's background worker.
    let waited = b.call(
        30,
        "lambo_stats",
        serde_json::json!({"agent_id": "agent-b", "receipt": receipt_two, "wait_ms": 4000}),
    );
    assert_eq!(
        waited["result"]["structuredContent"]["receipt"]["state"]
            .as_str()
            .unwrap_or("<none>"),
        "applied",
        "a receipt waited on through the proxy must resolve: {}",
        text_of(&waited)
    );
    // The piggyback reached the right caller through the shared hub, over the
    // byte pipe, in a `content` block the model reads. Per-agent scoping is
    // what makes that true of one hub serving two clients; the proxy neither
    // knows nor needs to.
    //
    // Which response carries it is deliberately not asserted: the first write
    // settles on the holder's own schedule, so its piggyback rides whichever of
    // B's later responses comes after that. What must hold is that it arrives
    // exactly once, on one of them.
    let delivered = format!("{}\n{}", piggyback_of(&second), piggyback_of(&waited));
    assert!(
        delivered.contains(&receipt),
        "B must be handed the tagged piggyback for its own earlier receipt: {}\n{}",
        text_of(&second),
        text_of(&waited)
    );
    // **J3-R1-9:** and no response may restate the receipt the same call
    // answered explicitly. One response stating one write's outcome twice is
    // one write outcome a model reads twice.
    assert!(
        !piggyback_of(&waited).contains(&receipt_two),
        "the piggyback must not repeat the receipt this call already answered: {}",
        piggyback_of(&waited)
    );

    // The receipt carries what the ack could not — including the node id the
    // reserve below needs. Fetched by id, which is the surface that exists for
    // exactly this: the piggyback line is prose, the fetch is structured.
    let first = b.call(
        31,
        "lambo_stats",
        serde_json::json!({"agent_id": "agent-b", "receipt": receipt, "wait_ms": 0}),
    );
    // **J3-R2-9:** and `mark_delivered` REMOVED it rather than suppressing it.
    // The J3-R1-9 assertion above proves the just-answered receipt is absent
    // from the answering call's own piggyback, which an implementation that
    // merely skipped any receipt settled during the current call would satisfy
    // unchanged. This is the response where such an implementation would
    // resurface it: a later call, with `receipt_two` long settled and nothing
    // suppressing it.
    assert!(
        !piggyback_of(&first).contains(&receipt_two),
        "a receipt taken out of the queue by mark_delivered must never come back on a later \
         response: {}",
        piggyback_of(&first)
    );

    let receipt_node = first["result"]["structuredContent"]["receipt"]["created"][0]
        .as_str()
        .unwrap_or_else(|| {
            panic!(
                "an applied receipt lists what it created: {}",
                text_of(&first)
            )
        })
        .to_string();

    // Read-your-writes across the hop, for the client that wrote it.
    let recalled = b.call(
        3,
        "lambo_recall",
        serde_json::json!({"agent_id": "agent-b", "query": "crossed the proxy hop"}),
    );
    // Take-once: the piggyback already rode the `lambo_stats` response above
    // (the wait settled the receipt inside that call, and the piggyback is
    // attached after the body runs), so it must NOT be repeated here.
    assert!(
        !text_of(&recalled).contains(&receipt),
        "a delivered receipt must not be re-announced on every later response: {}",
        text_of(&recalled)
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
    // reason J1 gated J2.
    let hits = &recalled["result"]["structuredContent"]["hits"];
    let node_id = hits[0]["node_id"]
        .as_str()
        .unwrap_or_else(|| panic!("recall must return a node id: {}", text_of(&recalled)))
        .to_string();
    assert_eq!(
        node_id, receipt_node,
        "the node id on the receipt and the node id recall returns must be the same node"
    );

    // Two agents, two locks, through the hop. B takes the lock as `agent-b `...
    //
    // **The trailing space is the assertion** (J2-R1-16). J1 takes `agent_id`
    // untrimmed on purpose, because normalising would silently merge two
    // callers' locks — so "the caller's agent_id crosses verbatim" is only
    // *tested* by an id a forwarder rebuilding the arguments would change.
    // `agent-b` survives any normalisation and pins nothing; `agent-b ` does
    // not. The refusal below renders as "reserved by {holder} until {expiry}",
    // so a trimmed id shows up as ONE space before "until" where the verbatim
    // id gives two. That is what would go red if the proxy ever started
    // re-serializing arguments.
    let taken = b.call(
        4,
        "lambo_reserve",
        serde_json::json!({"agent_id": "agent-b ", "node_id": node_id, "ttl_seconds": 60}),
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
    assert!(
        contended.contains("agent-b  until"),
        "the holder's agent_id must have crossed the hop UNTRIMMED — one space before 'until' \
         means a forwarder normalised it, which J1 forbids: {contended}"
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
    release_row(&db, &a_holder);
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
        .expect("a file-backed store is shareable and derives an endpoint");
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

/// J2-R1-6: the lease re-read on the reconnect path, pinned at the mechanism.
///
/// The round-1 review mutation-proved this unpinned. With `dial()`
/// short-circuited to a bare `connect(&self.endpoint)` — no `read_lease`, none
/// of the three `proxyable` checks — **both** other tests in this file still
/// passed, because a SIGKILLed holder leaves its socket file behind and a
/// connect to it fails whether or not the row was read.
///
/// The discriminating case is a **live socket with no lease row behind it**, and
/// reaching it needs a window the other tests do not have: the proxy's writer
/// must be `None` (only then is `dial` called at all) *while* a live holder is
/// listening at the endpoint. So: kill the holder so the proxy drops its
/// connection, start a new holder so the socket answers again, and release that
/// holder's row before the proxy ever reconnects to it.
///
/// With the re-read, the proxy refuses honestly. Without it, the call succeeds —
/// forwarding into a process whose licence to write has been revoked.
///
/// This is also the corrected *reason* the row is re-read (see
/// `HubProxy::reconnect`): not because the address moved — it never does, it is
/// a pure function of session and store — but because the **row** is the
/// authority on whether there is a holder at all.
#[test]
fn a_live_endpoint_with_no_lease_row_is_refused_rather_than_dialled() {
    let (dir, cfg, db) = scratch("orphan");
    provision(&db);

    let mut a = Serve::spawn(&cfg, "agent-a");
    a.initialize(1);
    let a_holder = lease_row(&db).expect("A holds").holder;

    let mut b = Serve::spawn(&cfg, "agent-b");
    b.initialize(1);
    let ok = b.call(
        2,
        "lambo_recall",
        serde_json::json!({"agent_id": "agent-b", "query": "anything"}),
    );
    assert!(ok["error"].is_null(), "baseline call: {}", text_of(&ok));

    // Kill A so the proxy's hub connection ends and its writer goes to `None`.
    a.sigkill();
    let _ = a.child.wait();
    // Clear A's row so C can take the session now rather than in one TTL. B is
    // NOT called in between, so its writer stays `None` and the next call it
    // makes will go through `dial`.
    release_row(&db, &a_holder);

    // C binds the same endpoint — the address is a pure function of session and
    // store, so it is literally the same path, stale socket and all.
    let mut c = Serve::spawn(&cfg, "agent-c");
    c.initialize(1);
    let c_holder = lease_row(&db).expect("C holds").holder;
    assert!(c_holder.starts_with("agent-c@"), "{c_holder}");
    assert!(
        std::path::Path::new(
            &lease_row(&db)
                .unwrap()
                .endpoint
                .expect("C publishes an endpoint")
        )
        .exists(),
        "C's socket must be live — that is the whole point of this test"
    );

    // Now revoke C's licence without stopping C. Its heartbeat re-acquires every
    // LEASE_HEARTBEAT_INTERVAL (15s), and the call below is one round trip.
    release_row(&db, &c_holder);
    assert!(
        lease_row(&db).is_none(),
        "the row must be gone while the socket stays live"
    );

    let refused = b.call(
        3,
        "lambo_recall",
        serde_json::json!({"agent_id": "agent-b", "query": "a live socket with no lease row"}),
    );
    let refused = text_of(&refused);
    assert!(
        refused.contains("NOTHING WAS READ OR WRITTEN"),
        "a live endpoint with no lease row must be REFUSED, not dialled — the row is the \
         authority on who holds the session, not the socket: {refused}"
    );

    b.sigterm();
    c.sigterm();
    let _ = b.child.wait();
    let _ = c.child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

/// J2-R1-5 at the binary: a base directory too long for a socket address must
/// cost this process its *endpoint*, not its *start*.
///
/// Before the remediation `SessionEndpoint::for_store`'s length refusal
/// propagated out of `serve` and the process exited — on a machine that served
/// fine before J2, for a feature the operator never asked for. A long runtime
/// directory is not exotic: a deep per-user path, a container mount, a long
/// username.
///
/// Driven through `XDG_RUNTIME_DIR`, which after J2-L1 is the only environment
/// variable in the derivation — `TMPDIR` was removed from it precisely because
/// two client products disagreed about passing it through.
///
/// The observable degradation, both halves: the client's session works, and the
/// lease row's `endpoint` is NULL, which is the honest row for a holder nothing
/// can reach — a proxy reading it gets `HolderPublishedNoEndpoint` and refuses
/// rather than dialling.
#[test]
fn a_base_directory_too_long_for_a_socket_still_serves_its_own_client() {
    let (dir, cfg, db) = scratch("longtmp");
    provision(&db);

    // Long enough that `<dir>/lambo/<38-byte filename>` cannot fit the 104-byte
    // sun_path bound. Real, because a runtime directory has to be usable.
    let long_tmp = std::path::PathBuf::from(format!("/tmp/{}", "x".repeat(80)));
    std::fs::create_dir_all(&long_tmp).expect("a long but real runtime directory");

    let mut child = Command::new(env!("CARGO_BIN_EXE_lambo"))
        .args([
            "--config",
            cfg.to_str().unwrap(),
            "serve",
            "--session",
            SESSION,
            "--agent",
            "agent-long",
            "--transport",
            "stdio",
        ])
        .env("XDG_RUNTIME_DIR", &long_tmp)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn");
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
    let mut stdin = child.stdin.take().expect("stdin");
    stdin
        .write_all(
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"j2-test","version":"1"}}}
"#,
        )
        .expect("write initialize");
    stdin.flush().expect("flush");

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let answered = loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "a serve with an unusable endpoint path must still serve its own client — it used to \
             exit instead"
        );
        let Ok(line) = rx.recv_timeout(remaining) else {
            panic!("the serve produced no initialize response — it exited instead of degrading");
        };
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
            if v.get("id").and_then(serde_json::Value::as_u64) == Some(1) {
                break v;
            }
        }
    };
    assert!(
        answered["result"]["serverInfo"].is_object(),
        "the degraded serve must be a real server: {answered}"
    );

    // And it advertises nothing, honestly.
    let row = lease_row(&db).expect("the degraded serve still holds the lease");
    assert_eq!(
        row.endpoint, None,
        "a holder with no endpoint must publish NULL, not an address nothing is listening on"
    );

    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&long_tmp);
    let _ = std::fs::remove_dir_all(&dir);
}

/// J2-L1, at the binary: **two client products derive two endpoint directories
/// for one session on one store, and forwarding must still work.**
///
/// Measured live against `bbac803`: `cursor-agent` scrubs `TMPDIR` from the
/// environment of the MCP server it spawns (so the derivation fell through to
/// `/tmp/lambo`) while `opencode` passes macOS's per-user `TMPDIR` through (so it
/// derived `$TMPDIR/lambo`). Same binary, same store, same session, two
/// addresses. The losing serve compared the row's published endpoint against its
/// own derivation, refused ("it is running a different endpoint scheme"), waited
/// out its budget, and the client reported no tools at all — the exact outage J2
/// exists to remove, arriving through the environment on unmodified default
/// wiring.
///
/// Two changes answer it. The `TMPDIR` rung is gone, so that specific
/// divergence cannot happen; and `proxyable` now compares the **address name**
/// rather than the whole path, because the name is a hash of the session and the
/// canonicalized store identity while the directory decides only reachability.
/// This test pins the second, which is the general one: a holder publishing our
/// address name in a directory we would never derive is dialled, not refused.
#[test]
fn a_holder_reachable_only_at_its_own_directory_is_still_forwarded_to() {
    use std::io::Write as _;
    use std::os::unix::fs::DirBuilderExt as _;
    use std::os::unix::net::UnixListener;

    let (dir, cfg, db) = scratch("crossdir");
    provision(&db);

    let store_cfg = lambo::store::StoreConfig {
        kind: lambo::store::StoreKind::Sqlite,
        path: Some(db.clone()),
        ..lambo::store::StoreConfig::default()
    };
    let ours = lambo::mcp::SessionEndpoint::for_store(SESSION, &store_cfg)
        .expect("a file-backed store is shareable");
    let name = ours.path().file_name().unwrap().to_owned();

    // A directory the spawned serve will NOT derive — standing in for the other
    // client product's inherited environment. Private and self-owned, because
    // the dial side runs the same check the bind side does.
    let elsewhere = std::path::PathBuf::from(format!("/tmp/lbx{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&elsewhere);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&elsewhere)
        .expect("a private directory the serve would not derive");
    let sock = elsewhere.join(&name);
    assert_ne!(
        sock.as_path(),
        ours.path(),
        "this test is only meaningful if the two directories differ"
    );

    let holder =
        lambo::store::LeaseHolder::for_this_process(&lambo::types::AgentId::new("other-product"))
            .reachable_at(sock.to_string_lossy().into_owned());
    {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let store = SqliteStore::connect(&db).unwrap();
            let outcome = store
                .acquire_lease(&SessionId::from(SESSION), &holder, Duration::from_secs(45))
                .await
                .unwrap();
            assert!(matches!(outcome, lambo::store::LeaseOutcome::Acquired(_)));
        });
    }

    let listener = UnixListener::bind(&sock).expect("bind the other product's endpoint");
    let holder_thread = std::thread::spawn(move || {
        let mut served = 0usize;
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let mut w = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
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
                if v.get("method").and_then(serde_json::Value::as_str) == Some("initialize") {
                    let reply = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": v.get("id").cloned().unwrap_or(serde_json::Value::Null),
                        "result": {
                            "protocolVersion": "2025-06-18",
                            "capabilities": {},
                            "serverInfo": {"name": "other-product-holder", "version": "1"},
                        },
                    });
                    writeln!(w, "{reply}").unwrap();
                    w.flush().unwrap();
                    served += 1;
                }
            }
            if served > 0 {
                break;
            }
        }
    });

    // The losing serve. Before J2-L1 this refused and waited out its budget.
    let mut b = Serve::spawn(&cfg, "this-product");
    let init = b.initialize(1);
    assert_eq!(
        init["result"]["serverInfo"]["name"], "other-product-holder",
        "the handshake must have been answered by the HOLDER, through a directory this \
         process would never have derived: {init}"
    );

    b.sigterm();
    let _ = b.child.wait();
    let _ = holder_thread.join();
    let _ = std::fs::remove_dir_all(&elsewhere);
    let _ = std::fs::remove_dir_all(&dir);
}

/// J2-L2, at the binary: a serve that **cannot** win inside a client's patience
/// must say so at once, not spend that patience discovering it.
///
/// Measured live: the election waited `LEASE_TTL + ELECTION_SLACK` = 50s, and
/// `opencode` 1.18.18 declared the server failed at 31.96s — after which the
/// model had no lambo tools at all. A recoverable wait became a total outage.
///
/// The budget is now a client-tolerance number (20s), and nothing waits blindly:
/// the row says when the holder's lease expires, so a lease with a full TTL left
/// is refused immediately with the seconds named. Here the holder is a CLI-shaped
/// writer — it publishes no endpoint, so it is not proxyable and the only route
/// to the session is its lease lapsing, 45s away.
#[test]
fn a_holder_whose_lease_outlasts_the_client_budget_is_refused_at_once() {
    let (dir, cfg, db) = scratch("budget");
    provision(&db);

    // A holder with no endpoint: exactly what a `lambo derive` holding the lease
    // for the length of one verb looks like in the row.
    let holder =
        lambo::store::LeaseHolder::for_this_process(&lambo::types::AgentId::new("cli-writer"));
    {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let store = SqliteStore::connect(&db).unwrap();
            store
                .acquire_lease(&SessionId::from(SESSION), &holder, Duration::from_secs(45))
                .await
                .unwrap();
        });
    }

    let started = std::time::Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_lambo"))
        .args([
            "--config",
            cfg.to_str().unwrap(),
            "serve",
            "--session",
            SESSION,
            "--agent",
            "late-arrival",
            "--transport",
            "stdio",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn");
    let elapsed = started.elapsed();

    assert!(!out.status.success(), "it must refuse, not serve");
    // Fast. The pre-fix behaviour was 50s; the client observed here gave up at 32s.
    assert!(
        elapsed < Duration::from_secs(10),
        "the refusal must not spend the client's startup budget to reach the same answer: \
         {elapsed:?}"
    );
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        err.contains("does not lapse for"),
        "the refusal must name how long the wait would be: {err}"
    );
    assert!(
        err.contains("NO TOOLS"),
        "and why waiting would be worse — a client that gives up reports no tools: {err}"
    );
    // J2-R2-3, the narrow half. This holder published no endpoint, so it is a
    // CLI verb that really is alive and really is refreshing its lease — the
    // clause is TRUE here and must survive. The correction applies only where the
    // probe found the endpoint refusing connections; see the test below.
    assert!(
        err.contains("is still refreshing it"),
        "a live-but-unforwardable holder is still refreshing its lease, and the refusal must \
         keep saying so: {err}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// J2-R2-3, at the binary: a holder whose endpoint refuses connections must not
/// be described as one that "is still refreshing" its lease.
///
/// The refusal is composed from two sources — the memory-level lease message,
/// which knows only the row, and the election's own endpoint probe, which knows
/// *now*. J2-L2 is what first put them in one paragraph, and it put the row's
/// (stale, optimistic) half first: an operator who read the opening sentence went
/// hunting for a live process that had been `kill -9`'d seconds earlier. The
/// probe wins, because a refused connect is newer evidence than a lease row that
/// may be a whole `LEASE_HEARTBEAT_INTERVAL` old.
#[test]
fn a_holder_whose_endpoint_refuses_is_not_described_as_still_refreshing() {
    use std::os::unix::fs::DirBuilderExt as _;

    let (dir, cfg, db) = scratch("deadendpoint");
    provision(&db);

    let store_cfg = lambo::store::StoreConfig {
        kind: lambo::store::StoreKind::Sqlite,
        path: Some(db.clone()),
        ..lambo::store::StoreConfig::default()
    };
    let ours = lambo::mcp::SessionEndpoint::for_store(SESSION, &store_cfg)
        .expect("a file-backed store is shareable");
    let name = ours.path().file_name().unwrap().to_owned();

    // A private, self-owned directory so the dial-side directory check passes and
    // the probe gets as far as the connect — which is the outcome under test.
    // Nothing is ever bound at the path, which is exactly what an abruptly dead
    // holder leaves behind once its socket has been cleaned up.
    let gone = std::path::PathBuf::from(format!("/tmp/lbdead{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&gone);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&gone)
        .expect("a private directory for an endpoint that is not there");
    let sock = gone.join(&name);

    let holder =
        lambo::store::LeaseHolder::for_this_process(&lambo::types::AgentId::new("dead-holder"))
            .reachable_at(sock.to_string_lossy().into_owned());
    {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let store = SqliteStore::connect(&db).unwrap();
            let outcome = store
                .acquire_lease(&SessionId::from(SESSION), &holder, Duration::from_secs(45))
                .await
                .unwrap();
            assert!(matches!(outcome, lambo::store::LeaseOutcome::Acquired(_)));
        });
    }

    let out = Command::new(env!("CARGO_BIN_EXE_lambo"))
        .args([
            "--config",
            cfg.to_str().unwrap(),
            "serve",
            "--session",
            SESSION,
            "--agent",
            "late-arrival",
            "--transport",
            "stdio",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn");

    assert!(!out.status.success(), "it must refuse, not serve");
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        err.contains("not accepting connections"),
        "this test is only meaningful if the probe reached the connect: {err}"
    );
    assert!(
        !err.contains("is still refreshing it"),
        "the refusal contradicted itself: the holder is not refreshing anything, and the \
         message's own next clause says so: {err}"
    );
    assert!(
        err.contains("most likely died"),
        "and the operator needs the conclusion the probe supports: {err}"
    );

    let _ = std::fs::remove_dir_all(&gone);
    let _ = std::fs::remove_dir_all(&dir);
}
