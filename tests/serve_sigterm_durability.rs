//! Test-gap (b): a **real subprocess** SIGTERM durability test.
//!
//! The unit tests pin that `serve()` *reaches* `Memory::close` on a shutdown
//! signal (`src/mcp/serve.rs`), but they run in-process against a synthetic
//! transport. This drives the shipped binary end to end: launch `lambo serve
//! --transport stdio` against a **durable** SQLite file, complete the MCP
//! handshake, record an action over the wire, send it `SIGTERM`, and — after the
//! process has exited — reopen the store in a fresh connection and assert the
//! recorded concept persisted. That is the whole durability contract T82-1 is
//! about: a signal must flush the write-behind tail, not drop it.
//!
//! SQLite (not MemoryStore) is deliberate: durability across a process boundary
//! can only be observed through a store that outlives the process. The test is
//! gated on `store-sqlite`, so the default `cargo test` gate (which does not
//! enable it) skips it; run it with `--features store-sqlite`.
#![cfg(all(feature = "store-sqlite", feature = "embed-fixture", unix))]

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use lambo::store::{GraphStore, SqliteStore};
use lambo::types::SessionId;

const SESSION: &str = "t8.2-sigterm-durability";
const ACTION: &str = "wrote src/durability_probe.rs under SIGTERM";
const PRODUCES: &str = "src/durability_probe.rs";

/// Send `SIGTERM` to `pid` via the `kill` binary — avoids pulling `libc`/`nix`
/// in as a dev-dependency just for one signal.
fn sigterm(pid: u32) {
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .expect("spawn kill");
    assert!(status.success(), "kill -TERM {pid} failed");
}

/// Read newline-delimited JSON-RPC frames from the child's stdout until one
/// carries `"id": <id>`, on a background thread so a hung server cannot wedge
/// the test forever. Returns the matching frame text.
fn read_response(reader_rx: &mpsc::Receiver<String>, id: u64) -> String {
    let needle = format!("\"id\": {id}");
    let needle_compact = format!("\"id\":{id}");
    loop {
        let line = reader_rx
            .recv_timeout(Duration::from_secs(20))
            .unwrap_or_else(|e| panic!("no JSON-RPC frame with id {id} within 20s: {e}"));
        if line.contains(&needle) || line.contains(&needle_compact) {
            return line;
        }
        // Otherwise it is a notification or a different id — keep reading.
    }
}

fn write_frame(stdin: &mut impl Write, frame: &str) {
    stdin.write_all(frame.as_bytes()).expect("write frame");
    stdin.write_all(b"\n").expect("write newline");
    stdin.flush().expect("flush frame");
}

#[test]
fn a_sigterm_flushes_the_recorded_action_to_the_durable_store() {
    // A scratch dir the test owns; the sqlite file lives inside it.
    let dir = std::env::temp_dir().join(format!(
        "lambo-sigterm-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let db_path = dir.join("durability.sqlite");
    let db_str = db_path.to_str().expect("utf-8 path").to_string();
    let cfg_path = dir.join("lambo.toml");
    std::fs::write(
        &cfg_path,
        format!(
            "[store]\nkind = \"sqlite\"\npath = \"{db_str}\"\n\n[embedder]\nkind = \"fixture\"\ndim = 1024\n"
        ),
    )
    .expect("write config");

    let rt = tokio::runtime::Runtime::new().expect("runtime");

    // Provision the schema, then drop the store so its connection is not holding
    // the file when the subprocess opens it.
    rt.block_on(async {
        let store = SqliteStore::connect(&db_str).expect("connect for provision");
        store.init_schema().await.expect("init_schema");
    });

    // Launch the shipped binary exactly as an MCP client would.
    let mut child = Command::new(env!("CARGO_BIN_EXE_lambo"))
        .args([
            "--config",
            cfg_path.to_str().unwrap(),
            "serve",
            "--session",
            SESSION,
            "--agent",
            "agent-a",
            "--transport",
            "stdio",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn lambo serve");
    let pid = child.id();

    // Pump stdout on a background thread.
    let stdout = child.stdout.take().expect("child stdout");
    let (tx, rx) = mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        let mut lines = BufReader::new(stdout).lines();
        while let Some(Ok(line)) = lines.next() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let mut stdin = child.stdin.take().expect("child stdin");

    // 1. initialize
    write_frame(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"sigterm-test","version":"1"}}}"#,
    );
    let init = read_response(&rx, 1);
    assert!(
        init.contains("\"serverInfo\""),
        "no initialize result: {init}"
    );

    // 2. initialized notification (no response)
    write_frame(
        &mut stdin,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    );

    // 3. lambo_record_action over the wire
    let call = format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"lambo_record_action","arguments":{{"agent_id":"agent-a","action":"{ACTION}","produces":["{PRODUCES}"]}}}}}}"#
    );
    write_frame(&mut stdin, &call);
    let acted = read_response(&rx, 2);
    assert!(
        acted.contains("\"isError\": false") || acted.contains("\"isError\":false"),
        "record_action was not accepted: {acted}"
    );

    // 4. SIGTERM while the tail is still only in the write-behind log.
    sigterm(pid);

    // 5. The process must exit on its own (the fix under test). Reap it on a
    //    thread — `child.wait()` both detects exit and clears the zombie — and
    //    bound the wait so a regression that hangs fails loudly instead of
    //    wedging the suite.
    let (wait_tx, wait_rx) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let _ = wait_tx.send(child.wait());
    });
    match wait_rx.recv_timeout(Duration::from_secs(15)) {
        Ok(status) => {
            let status = status.expect("wait on child");
            assert!(
                status.success(),
                "lambo serve must exit cleanly on SIGTERM (close() succeeded), got {status:?}"
            );
        }
        Err(_) => {
            // Force it dead so the reaper thread unblocks, then fail.
            let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
            let _ = waiter.join();
            let _ = reader.join();
            let _ = std::fs::remove_dir_all(&dir);
            panic!("lambo serve did not exit within 15s of SIGTERM — close() may not run");
        }
    }
    let _ = waiter.join();
    let _ = reader.join();
    drop(stdin);

    // 6. Reopen the store in a fresh connection and assert the tail is durable.
    let concepts = rt.block_on(async {
        let store = SqliteStore::connect(&db_str).expect("reconnect");
        store
            .load_session(&SessionId::from(SESSION))
            .await
            .expect("load_session after SIGTERM — the session row must exist, i.e. close() ran")
            .concepts
    });

    let contents: Vec<&str> = concepts.iter().map(|c| c.content.as_str()).collect();
    assert!(
        contents.contains(&ACTION),
        "the recorded action must have survived SIGTERM in the durable store; got {contents:?}"
    );
    assert!(
        contents.contains(&PRODUCES),
        "the produced-resource concept must have survived too; got {contents:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
