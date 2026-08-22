//! J4: lease conflicts leave an artifact (dev-diary/lambo-for-mooshik/
//! J-multi-client.md §J4).
//!
//! Before J4, a serve that LOSES the lease exits before it can open a ledger,
//! so the most common multi-agent failure — one serve refused by another's
//! single-writer lease — was structurally invisible to I1: the refuser had no
//! artifact, and the holder never learned it had turned someone away. This file
//! drives the shipped binary across real process boundaries and asserts the
//! artifact exists **from both sides**.
//!
//! The setup mirrors `serve_single_writer_lease.rs` (which still pins
//! fail-closed enforcement): two `lambo serve` processes share one SQLite file
//! and one session, and both pass the SAME `--ledger` path, so each side's
//! J4 lines land in one file that this test reads back. The loser is launched
//! on `--transport http` — the one transport where a refusal is still terminal
//! (an http serve has no line-framed client wire to proxy over, so after J2 the
//! refusal is the designed outcome there, exactly as §J2 records).
//!
//! Gated on `store-sqlite,embed-fixture` like its sibling, so the default
//! `cargo test` gate skips it; run with `--features store-sqlite,embed-fixture`.
#![cfg(all(feature = "store-sqlite", feature = "embed-fixture", unix))]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use lambo::store::{GraphStore, SqliteStore};

const SESSION: &str = "j4-lease-conflict";

fn sigterm(pid: u32) {
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status();
}

fn read_response(reader_rx: &mpsc::Receiver<String>, id: u64) -> String {
    let needle = format!("\"id\": {id}");
    let needle_compact = format!("\"id\":{id}");
    loop {
        let line = reader_rx
            .recv_timeout(Duration::from_secs(20))
            .unwrap_or_else(|e| panic!("no frame with id {id} within 20s: {e}"));
        if line.contains(&needle) || line.contains(&needle_compact) {
            return line;
        }
    }
}

fn write_frame(stdin: &mut impl Write, frame: &str) {
    stdin.write_all(frame.as_bytes()).expect("write frame");
    stdin.write_all(b"\n").expect("write newline");
    stdin.flush().expect("flush frame");
}

/// One `lambo serve` subprocess plus a reader for its stdout.
fn spawn_serve(
    cfg: &std::path::Path,
    agent: &str,
    transport: &str,
    ledger: &std::path::Path,
) -> Child {
    let child = Command::new(env!("CARGO_BIN_EXE_lambo"))
        .args([
            "--config",
            cfg.to_str().unwrap(),
            "serve",
            "--session",
            SESSION,
            "--agent",
            agent,
            "--transport",
            transport,
            "--ledger",
            ledger.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn serve");
    child
}

/// Every JSON object in `path`, one per line, preserving order.
fn read_ledger(path: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap_or_else(|_| String::new())
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("each ledger line is one JSON object"))
        .collect()
}

/// Poll `path` until a predicate over its lines holds, or fail after `budget`.
fn wait_ledger<F: Fn(&[serde_json::Value]) -> bool>(
    path: &std::path::Path,
    what: &str,
    budget: Duration,
    pred: F,
) -> Vec<serde_json::Value> {
    let deadline = Instant::now() + budget;
    loop {
        let lines = read_ledger(path);
        if pred(&lines) {
            return lines;
        }
        if Instant::now() >= deadline {
            panic!("ledger never produced {what}; lines so far: {lines:?}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The J4 Done-when checkbox: **a refused lease acquisition appears in the
/// ledger from both sides.** Also pins the pre-lease startup line (deliverable
/// a): the refused http loser must have written a `startup` line before it was
/// refused, and the incumbent must have recorded the refused takeover it turned
/// away. FAILS on pre-J4 code, where neither half existed.
#[test]
fn refused_acquire_appears_in_the_ledger_from_both_sides() {
    let dir = std::env::temp_dir().join(format!(
        "lambo-j4-bothsides-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let db = dir.join("lease.sqlite");
    let ledger = dir.join("shared.jsonl");
    let cfg = dir.join("lambo.toml");
    std::fs::write(
        &cfg,
        format!(
            "[store]\nkind = \"sqlite\"\npath = \"{}\"\n\n[embedder]\nkind = \"fixture\"\ndim = 1024\n",
            db.display()
        ),
    )
    .expect("write config");

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(async {
        let store = SqliteStore::connect(db.to_str().unwrap()).expect("connect for provision");
        store.init_schema().await.expect("init_schema");
    });

    // Holder A: acquires and holds the lease, with the shared ledger attached.
    let a = spawn_serve(&cfg, "agent-a", "stdio", &ledger);
    let a_pid = a.id();
    let mut a: Child = a;
    let a_stdout = a.stdout.take().expect("A stdout");
    let (tx, rx) = mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        let mut lines = BufReader::new(a_stdout).lines();
        while let Some(Ok(line)) = lines.next() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut a_stdin = a.stdin.take().expect("A stdin");
    write_frame(
        &mut a_stdin,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"j4-test","version":"1"}}}"#,
    );
    assert!(
        read_response(&rx, 1).contains("\"serverInfo\""),
        "A did not become the holder"
    );

    // Let A's refusal-recorder task reach its poll loop before B is refused.
    std::thread::sleep(Duration::from_millis(800));

    // Loser B: same session, same store, http transport (refusal is terminal
    // there). It must write its OWN startup + refused lines, persist the refusal
    // to the store, and exit non-zero.
    let b = spawn_serve(&cfg, "agent-b", "http", &ledger);
    let (out_tx, out_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = out_tx.send(b.wait_with_output());
    });
    let b_out = match out_rx.recv_timeout(Duration::from_secs(20)) {
        Ok(r) => r.expect("wait B"),
        Err(_) => {
            sigterm(a_pid);
            let _ = a.wait();
            let _ = reader.join();
            let _ = std::fs::remove_dir_all(&dir);
            panic!("B did not exit within 20s — it may have become a second writer");
        }
    };
    assert!(
        !b_out.status.success(),
        "the http loser must fail closed (refused by the lease)"
    );
    let b_stderr = String::from_utf8_lossy(&b_out.stderr);
    assert!(
        b_stderr.contains("already held by another writer"),
        "B must be refused by the lease itself; stderr: {b_stderr}"
    );

    // The loser side is guaranteed present once B has written it; the holder
    // side arrives within a poll interval or two.
    let lines = wait_ledger(
        &ledger,
        "both sides of the refusal",
        Duration::from_secs(5),
        |ls| {
            let loser = ls
                .iter()
                .any(|l| l["kind"] == "lease" && l["event"] == "refused" && l["side"] == "loser");
            let holder = ls.iter().any(|l| {
                l["kind"] == "lease" && l["event"] == "refused_takeover" && l["side"] == "holder"
            });
            loser && holder
        },
    );

    let loser_line = lines
        .iter()
        .find(|l| l["kind"] == "lease" && l["event"] == "refused")
        .expect("loser line");
    assert_eq!(
        loser_line["agent_id"], "agent-b",
        "loser line is the loser's"
    );
    let loser_holder = loser_line["holder"].as_str().expect("holder token");
    assert!(
        loser_holder.starts_with("agent-a@"),
        "loser line names the incumbent; got {loser_holder}"
    );

    let holder_line = lines
        .iter()
        .find(|l| l["kind"] == "lease" && l["event"] == "refused_takeover")
        .expect("holder line");
    assert_eq!(
        holder_line["agent_id"], "agent-a",
        "holder line is the holder's"
    );
    let holder_refused = holder_line["holder"].as_str().expect("holder token");
    assert!(
        holder_refused.starts_with("agent-b@"),
        "holder line names the refused loser; got {holder_refused}"
    );

    // Pre-lease startup line (deliverable a): the REFUSED process wrote its
    // intent to acquire before it was refused — the line exists even though B
    // exited.
    assert!(
        lines
            .iter()
            .any(|l| l["kind"] == "startup" && l["agent_id"] == "agent-b"),
        "the refused loser must have written its pre-lease startup line"
    );
    assert!(
        lines
            .iter()
            .any(|l| l["kind"] == "startup" && l["agent_id"] == "agent-a"),
        "the holder wrote its own startup line"
    );

    // Clean up: A shuts down on SIGTERM and releases the lease.
    sigterm(a_pid);
    let _ = a.wait();
    drop(a_stdin);
    let _ = reader.join();
    let _ = std::fs::remove_dir_all(&dir);
}

/// J2→J4 handoff: a proxying serve is alive and books its own lines — at least
/// a `proxying` line when it starts forwarding. (Will not pass on pre-J4 code,
/// where "a proxy books no ledger lines of its own".)
#[test]
fn a_proxying_serve_writes_a_proxying_line() {
    let dir = std::env::temp_dir().join(format!(
        "lambo-j4-proxy-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let db = dir.join("lease.sqlite");
    let ledger = dir.join("proxy.jsonl");
    let cfg = dir.join("lambo.toml");
    std::fs::write(
        &cfg,
        format!(
            "[store]\nkind = \"sqlite\"\npath = \"{}\"\n\n[embedder]\nkind = \"fixture\"\ndim = 1024\n",
            db.display()
        ),
    )
    .expect("write config");

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(async {
        let store = SqliteStore::connect(db.to_str().unwrap()).expect("connect");
        store.init_schema().await.expect("init_schema");
    });

    // Holder A on stdio.
    let a = spawn_serve(&cfg, "agent-a", "stdio", &ledger);
    let a_pid = a.id();
    let mut a: Child = a;
    let a_stdout = a.stdout.take().expect("A stdout");
    let (tx, rx) = mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        let mut lines = BufReader::new(a_stdout).lines();
        while let Some(Ok(line)) = lines.next() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut a_stdin = a.stdin.take().expect("A stdin");
    write_frame(
        &mut a_stdin,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"j4-test","version":"1"}}}"#,
    );
    assert!(read_response(&rx, 1).contains("\"serverInfo\""), "A holder");

    // Proxy B: stdio, refused, but proxying is viable → becomes a proxy. It
    // writes its own `proxying` line before any frame flows.
    let mut b = spawn_serve(&cfg, "agent-b", "stdio", &ledger);
    let b_pid = b.id();

    // J4 — the proxy path must also satisfy "from both sides" (finding
    // J4-R1-1): B was refused the acquisition even though it degrades to a
    // proxy, so the incumbent must learn it was contended. And the proxying
    // line must name the real agent (finding J4-R1-2), never the literal
    // "proxy".
    let lines = wait_ledger(
        &ledger,
        "proxy line with the real agent and the refusal from both sides",
        Duration::from_secs(5),
        |ls| {
            let proxying = ls.iter().any(|l| {
                l["kind"] == "lease" && l["event"] == "proxying" && l["agent_id"] == "agent-b"
            });
            let loser = ls
                .iter()
                .any(|l| l["kind"] == "lease" && l["event"] == "refused" && l["side"] == "loser");
            let holder = ls.iter().any(|l| {
                l["kind"] == "lease" && l["event"] == "refused_takeover" && l["side"] == "holder"
            });
            proxying && loser && holder
        },
    );

    // J4-R1-2: the proxying line carries the real agent.
    let px = lines
        .iter()
        .find(|l| l["kind"] == "lease" && l["event"] == "proxying")
        .expect("proxying line");
    assert_eq!(px["side"], "loser");
    assert_eq!(
        px["agent_id"], "agent-b",
        "the proxying line must carry the real agent, not the literal 'proxy'"
    );

    // J4-R1-1: the proxy-degrading loser's refusal reaches the ledger on BOTH
    // sides — B's own refused line, and A's refused_takeover (the incumbent
    // learned it was contended, through the store, from a separate process).
    let loser = lines
        .iter()
        .find(|l| l["kind"] == "lease" && l["event"] == "refused")
        .expect("loser refused line on the proxy path");
    assert_eq!(loser["side"], "loser");
    assert_eq!(loser["agent_id"], "agent-b");
    assert!(
        loser["holder"]
            .as_str()
            .unwrap_or_default()
            .starts_with("agent-a@"),
        "loser names the incumbent holder; got {:?}",
        loser["holder"]
    );
    let holder = lines
        .iter()
        .find(|l| l["kind"] == "lease" && l["event"] == "refused_takeover")
        .expect("holder refused_takeover line on the proxy path");
    assert_eq!(holder["side"], "holder");
    assert_eq!(holder["agent_id"], "agent-a");
    assert!(
        holder["holder"]
            .as_str()
            .unwrap_or_default()
            .starts_with("agent-b@"),
        "holder names the refused loser; got {:?}",
        holder["holder"]
    );

    sigterm(a_pid);
    sigterm(b_pid);
    let _ = a.wait();
    let _ = b.wait();
    let _ = reader.join();
    let _ = std::fs::remove_dir_all(&dir);
}
