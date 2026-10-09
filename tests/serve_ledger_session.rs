//! #32 decision 15: every ledger line names its session.
//!
//! One `--ledger` file will carry many sessions once a serve hosts more than
//! one (#32 PR 4), so the `session` field goes on every line now, while a
//! serve still holds exactly one. `startup` and `lease` lines already carried
//! it; this pins the `call`, `completion` and `stats` (heartbeat) lines a real
//! `lambo serve` writes, driven across the process boundary.
//!
//! Gated on `store-sqlite,embed-fixture` like the other serve integration
//! tests; run with `--features store-sqlite,embed-fixture`.
#![cfg(all(feature = "store-sqlite", feature = "embed-fixture", unix))]

use std::io::{BufRead, BufReader, Write};
use std::process::Stdio;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use lambo::store::{GraphStore, SqliteStore};

mod common;
use common::{RuntimeDir, ScratchDir, ServeChild};

const SESSION: &str = "i32-ledger-session";

fn write_frame(stdin: &mut impl Write, frame: &str) {
    stdin.write_all(frame.as_bytes()).expect("write frame");
    stdin.write_all(b"\n").expect("write newline");
    stdin.flush().expect("flush frame");
}

fn read_response(rx: &mpsc::Receiver<String>, id: u64) -> serde_json::Value {
    let needle = format!("\"id\": {id}");
    let needle_compact = format!("\"id\":{id}");
    loop {
        let line = rx
            .recv_timeout(Duration::from_secs(30))
            .unwrap_or_else(|e| panic!("no frame with id {id} within 30s: {e}"));
        if line.contains(&needle) || line.contains(&needle_compact) {
            return serde_json::from_str(&line).expect("frame is JSON");
        }
    }
}

/// Every complete line of the ledger, parsed. The writer may be mid-batch
/// while this reads, so a last line without its newline is skipped (it is
/// read again on the next poll); any earlier line must parse.
fn read_ledger(path: &std::path::Path) -> Vec<serde_json::Value> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let complete = match text.rfind('\n') {
        Some(end) => &text[..=end],
        None => "",
    };
    complete
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("each complete ledger line is one JSON object"))
        .collect()
}

#[test]
fn call_completion_and_heartbeat_lines_carry_the_session() {
    let dir = ScratchDir::new("lambo-i32-ledger-session");
    let db = dir.join("ledger.sqlite");
    let ledger = dir.join("calls.jsonl");
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

    let runtime = RuntimeDir::new();
    let mut child = ServeChild::new(
        common::lambo_command()
            .env(common::RUNTIME_DIR_VAR, runtime.path())
            .args([
                "--config",
                cfg.to_str().unwrap(),
                "serve",
                "--session",
                SESSION,
                "--agent",
                "agent-a",
                "--transport",
                "stdio",
                "--ledger",
                ledger.to_str().unwrap(),
                "--ledger-heartbeat",
                "1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn serve"),
    );
    let stdout = child.stdout.take().expect("stdout");
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut stdin = child.stdin.take().expect("stdin");
    write_frame(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"i32-test","version":"1"}}}"#,
    );
    assert!(read_response(&rx, 1)["result"]["serverInfo"].is_object());
    write_frame(
        &mut stdin,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    );
    write_frame(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"lambo_derive","arguments":{"agent_id":"agent-a","concepts":[{"content":"every ledger line names its session","concept_type":"logic"}]}}}"#,
    );
    let ack = read_response(&rx, 2);
    let receipt = ack["result"]["structuredContent"]["receipt"]
        .as_str()
        .expect("derive acks with a receipt")
        .to_string();
    write_frame(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "lambo_stats", "arguments": {
                "agent_id": "agent-a", "receipt": receipt, "wait_ms": 5000,
            }},
        })
        .to_string(),
    );
    read_response(&rx, 3);

    let deadline = Instant::now() + Duration::from_secs(20);
    let lines = loop {
        let lines = read_ledger(&ledger);
        let has = |kind: &str| lines.iter().any(|l| l["kind"] == kind);
        if has("call") && has("completion") && has("stats") {
            break lines;
        }
        assert!(
            Instant::now() < deadline,
            "ledger never held call, completion and stats lines: {lines:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    };

    for line in &lines {
        assert_eq!(
            line["session"].as_str(),
            Some(SESSION),
            "every line names its session: {line}"
        );
    }

    drop(stdin);
    let _ = child.wait_with_output_within(Duration::from_secs(20));
}
