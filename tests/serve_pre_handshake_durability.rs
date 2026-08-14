//! Test-gap R2-a: a **real subprocess** SIGTERM-*before-handshake* durability
//! test.
//!
//! `serve_sigterm_durability.rs` proves a signal after `initialize` still
//! flushes the tail. This one covers the narrower, earlier window R2-a is about:
//! a signal that lands **after the session is attached but before the client has
//! sent `initialize`**. `Memory` is already live at that point (a clean run has
//! `mutations=1` to flush — the session-attach record), and before R2-a the
//! signal hit the default disposition and killed the process with `close()`
//! un-run, so that row never reached the store.
//!
//! The fix arms the shutdown signal in `serve()` *before* the transport handoff,
//! threaded through the pre-handshake window (`src/mcp/serve.rs`). Crucially,
//! this is only pinned at the `setup_or_shutdown` helper in isolation: R4 showed
//! that unwiring the serve-level arming (back to a bare `.serve(stdio()).await`)
//! reproduces the *exact* old bug — process killed by the signal, session row
//! `durable=0` — while every other test stays green. This test closes that hole:
//! it drives the shipped binary, waits for the "session attached" stderr line,
//! sends `SIGTERM` **before** any JSON-RPC, and asserts the process exits `0` and
//! the session row is durable in the reopened store.
//!
//! SQLite (not MemoryStore) is deliberate, and the test is gated on
//! `store-sqlite` exactly like its sibling: durability across a process boundary
//! can only be observed through a store that outlives the process.
#![cfg(all(feature = "store-sqlite", feature = "embed-fixture", unix))]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use lambo::store::{GraphStore, SqliteStore};
use lambo::types::SessionId;

const SESSION: &str = "t8.2-pre-handshake-durability";

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

#[test]
fn a_pre_handshake_sigterm_still_flushes_the_session_row() {
    // A scratch dir the test owns; the sqlite file lives inside it.
    let dir = std::env::temp_dir().join(format!(
        "lambo-pre-handshake-{}-{}",
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

    // Launch the shipped binary exactly as an MCP client would — but capture
    // stderr this time, because "session attached" (the pre-handshake sync
    // point) is a tracing line on stderr, and we must not send the signal until
    // the session is genuinely attached.
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
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn lambo serve");
    let pid = child.id();

    // Pump stderr on a background thread and forward each line, so we can both
    // wait for the sync point and echo diagnostics.
    let stderr = child.stderr.take().expect("child stderr");
    let (etx, erx) = mpsc::channel::<String>();
    let ereader = std::thread::spawn(move || {
        let mut lines = BufReader::new(stderr).lines();
        while let Some(Ok(line)) = lines.next() {
            eprintln!("[serve stderr] {line}");
            if etx.send(line).is_err() {
                break;
            }
        }
    });

    // Keep stdin/stdout open (piped) but send NOTHING: the whole point is that
    // no `initialize` frame is ever written, so the transport is still parked in
    // its pre-handshake window when the signal arrives.
    let _stdin = child.stdin.take().expect("child stdin");
    let stdout = child.stdout.take().expect("child stdout");
    let out_reader = std::thread::spawn(move || {
        // Drain stdout so the child never blocks on a full pipe; we assert
        // nothing about it here.
        let mut lines = BufReader::new(stdout).lines();
        while let Some(Ok(_line)) = lines.next() {}
    });

    // Wait for the session to actually attach before signalling — otherwise the
    // race is meaningless (we could kill before `Memory` exists, which proves
    // nothing about the pre-handshake window).
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut attached = false;
    while std::time::Instant::now() < deadline {
        match erx.recv_timeout(Duration::from_secs(20)) {
            Ok(line) if line.contains("session attached") => {
                attached = true;
                break;
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    assert!(
        attached,
        "never saw 'session attached' on stderr within 20s — cannot signal the pre-handshake window"
    );

    // SIGTERM in the pre-handshake window: session attached, no `initialize`
    // sent, transport not yet serving.
    sigterm(pid);

    // The process must exit cleanly on its own (the fix under test). Reap on a
    // thread and bound the wait so a regression that hangs fails loudly.
    let (wait_tx, wait_rx) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let _ = wait_tx.send(child.wait());
    });
    match wait_rx.recv_timeout(Duration::from_secs(15)) {
        Ok(status) => {
            let status = status.expect("wait on child");
            assert!(
                status.success(),
                "lambo serve must exit cleanly on a pre-handshake SIGTERM (close() ran), got {status:?}"
            );
        }
        Err(_) => {
            let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
            let _ = waiter.join();
            let _ = ereader.join();
            let _ = out_reader.join();
            let _ = std::fs::remove_dir_all(&dir);
            panic!(
                "lambo serve did not exit within 15s of a pre-handshake SIGTERM — close() may not run"
            );
        }
    }
    let _ = waiter.join();
    let _ = ereader.join();
    let _ = out_reader.join();

    // Reopen the store in a fresh connection: the session row must be durable.
    // With the bug (serve-level arming unwired) the pre-handshake signal kills
    // the process with close() un-run, so no row is written and `load_session`
    // errors. Its success here is the durability proof R2-a is about.
    rt.block_on(async {
        let store = SqliteStore::connect(&db_str).expect("reconnect");
        store.load_session(&SessionId::from(SESSION)).await.expect(
            "load_session after a pre-handshake SIGTERM — the session row must exist, \
                 i.e. close() ran and flushed the attach mutation",
        );
    });

    let _ = std::fs::remove_dir_all(&dir);
}
