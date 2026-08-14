//! T8.6: a **real subprocess** cross-process single-writer test.
//!
//! The in-store tests pin the lease logic in-process; this drives the shipped
//! binary end to end across a process boundary — the whole point of promoting
//! spec §2.2 from advisory (a process-local log) to store-enforced. Two
//! `lambo serve` processes are pointed at ONE SQLite file for ONE session:
//! process A attaches (completes the MCP handshake, so its lease is provably
//! held), then process B is launched on the same session and must **fail
//! closed** — exit non-zero, naming the current holder — rather than open a
//! second diverging writer.
//!
//! SQLite on a shared file is deliberate: cross-process enforcement can only be
//! observed through a store two processes actually share. Gated on
//! `store-sqlite`, so the default `cargo test` gate skips it; run with
//! `--features store-sqlite`.
#![cfg(all(feature = "store-sqlite", feature = "embed-fixture", unix))]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use lambo::store::{GraphStore, SqliteStore};

const SESSION: &str = "t8.6-single-writer";

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
            .unwrap_or_else(|e| panic!("no JSON-RPC frame with id {id} within 20s: {e}"));
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

fn serve_cmd(cfg_path: &std::path::Path, agent: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lambo"));
    cmd.args([
        "--config",
        cfg_path.to_str().unwrap(),
        "serve",
        "--session",
        SESSION,
        "--agent",
        agent,
        "--transport",
        "stdio",
    ]);
    cmd
}

#[test]
fn a_second_process_on_one_session_is_refused_by_the_lease() {
    let dir = std::env::temp_dir().join(format!(
        "lambo-lease-proc-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let db_path = dir.join("lease.sqlite");
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
    // Provision the schema (creates session_leases), then drop the connection.
    rt.block_on(async {
        let store = SqliteStore::connect(&db_str).expect("connect for provision");
        store.init_schema().await.expect("init_schema");
    });

    // Process A: attaches and holds the lease.
    let mut a: Child = serve_cmd(&cfg_path, "agent-a")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn A");
    let a_pid = a.id();

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

    // Handshake so we KNOW A's build ran and the lease is held (acquire happens
    // in serve() before the transport handshake).
    write_frame(
        &mut a_stdin,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"lease-test","version":"1"}}}"#,
    );
    let init = read_response(&rx, 1);
    assert!(
        init.contains("\"serverInfo\""),
        "A did not complete initialize: {init}"
    );

    // Process B: same session, same store file, different agent. It must fail
    // closed at build time — before it ever serves.
    let b = serve_cmd(&cfg_path, "agent-b")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn B");

    // B exits on its own (no handshake needed); bound the wait so a regression
    // that lets B open a second writer fails loudly instead of hanging.
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
            panic!("process B did not exit within 20s — it may have opened a second writer");
        }
    };

    // The core assertion: B was refused.
    assert!(
        !b_out.status.success(),
        "the second process must fail closed (non-zero exit); got {:?}",
        b_out.status
    );
    let b_stderr = String::from_utf8_lossy(&b_out.stderr);
    assert!(
        b_stderr.contains("single-writer"),
        "B's failure must name the single-writer lease; stderr was:\n{b_stderr}"
    );
    assert!(
        b_stderr.contains("agent-a"),
        "B's failure must name the current holder (agent-a); stderr was:\n{b_stderr}"
    );

    // Tear A down.
    sigterm(a_pid);
    let _ = a.wait();
    drop(a_stdin);
    let _ = reader.join();
    let _ = std::fs::remove_dir_all(&dir);
}
