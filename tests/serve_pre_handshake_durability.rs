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
//! it drives the shipped binary, waits for a "session attached" stderr line,
//! sends `SIGTERM` **before** any JSON-RPC, and asserts the process exits `0` and
//! the session row is durable in the reopened store.
//!
//! "A" line, not "the": two stderr lines contain that substring, and the wait
//! deliberately fires on the earlier memory-level one so the signal lands in the
//! window that opens at lease acquisition. See the comment at the matcher
//! (I-R2-2) — the looseness is load-bearing and must not be tightened.
//!
//! SQLite (not MemoryStore) is deliberate, and the test is gated on
//! `store-sqlite` exactly like its sibling: durability across a process boundary
//! can only be observed through a store that outlives the process.
//!
//! ## The second case, added by J2
//!
//! J2 gave `serve` a branch this file could not previously reach: a serve that
//! loses the single-writer lease becomes a **proxy** rather than exiting 1. The
//! J0 review flagged the hazard before J2 was written — a refused serve never
//! reaches the holder path, so proxy work would run above the shutdown-arming
//! point, I-R2-1's hole through a new door — and flagged that this test could
//! not see it either: **its loose `"session attached"` matcher never fires for a
//! proxy**, which does not attach a session and never logs that line. Anchoring
//! the new case on it would have produced a test that passed without ever
//! signalling anything.
//!
//! So the proxy case has its own sync point, `"proxying to the session holder"`,
//! a line only the proxy emits, and it asserts the property that actually
//! matters on that branch. A proxy holds no lease, no write-behind tail and no
//! graph, so a signal to it is **not** a durability event and must not become
//! one: it must exit cleanly, and the holder it was forwarding to must be
//! untouched and still durable afterwards. That is the pair — clean proxy exit,
//! intact holder — that a naive proxy branch would break.
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
            // This matcher is LOOSE ON PURPOSE — do not tighten it (I-R2-2).
            // Two stderr lines contain "session attached": the memory-level
            // "Memory session attached (daemon + flush + canonization running)",
            // emitted from inside `build_memory` right after the single-writer
            // lease is taken, and the later serve-level "lambo serve: session
            // attached". Substring-matching fires on the FIRST, so the SIGTERM
            // lands in the wider window that starts at lease acquisition rather
            // than the narrow one after the arming — which is precisely how
            // I-R2-1 was caught: I had moved `LamboServer::new` above the
            // shutdown arming, and this test went red in CI because it was
            // probing the real window instead of the intended one.
            //
            // Anchoring on the serve-level line would green CI while leaving the
            // product hole open. The looseness IS the coverage: it tests the
            // property the invariant comment in `serve()` claims, not the
            // property the test author had in mind.
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

/// J2: a SIGTERM to a **proxy** in its own pre-handshake window.
///
/// The holder attaches and holds the lease. A second serve is launched on the
/// same session and store, loses the lease, and becomes a proxy; its client
/// sends NOTHING, so it is parked exactly where the case above parks a holder.
/// SIGTERM the proxy.
///
/// Two assertions, and both are about the branch rather than about durability:
/// the proxy exits **cleanly** (a signal on that branch is handled, not fatal),
/// and the holder is **untouched** — still live, still the lease holder, and its
/// own tail still durable when it closes. A proxy that had taken a lease, or
/// held a tail, or unlinked the holder's socket on its way out, fails one of
/// them.
#[test]
fn a_pre_handshake_sigterm_to_a_proxy_exits_cleanly_and_leaves_the_holder_intact() {
    let dir = std::env::temp_dir().join(format!(
        "lambo-pre-handshake-proxy-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let db_str = dir
        .join("durability.sqlite")
        .to_str()
        .expect("utf-8 path")
        .to_string();
    let cfg_path = dir.join("lambo.toml");
    std::fs::write(
        &cfg_path,
        format!(
            "[store]\nkind = \"sqlite\"\npath = \"{db_str}\"\n\n[embedder]\nkind = \"fixture\"\ndim = 1024\n"
        ),
    )
    .expect("write config");

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(async {
        let store = SqliteStore::connect(&db_str).expect("connect for provision");
        store.init_schema().await.expect("init_schema");
    });

    let spawn = |agent: &str| {
        Command::new(env!("CARGO_BIN_EXE_lambo"))
            .args([
                "--config",
                cfg_path.to_str().unwrap(),
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
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn lambo serve")
    };

    /// Pump a child's stderr onto a channel so a sync line can be waited for.
    fn watch(child: &mut std::process::Child) -> mpsc::Receiver<String> {
        let stderr = child.stderr.take().expect("child stderr");
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            let mut lines = BufReader::new(stderr).lines();
            while let Some(Ok(line)) = lines.next() {
                eprintln!("[serve stderr] {line}");
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        rx
    }

    fn wait_for(rx: &mpsc::Receiver<String>, needle: &str) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(25);
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_secs(25)) {
                Ok(line) if line.contains(needle) => return true,
                Ok(_) => continue,
                Err(_) => return false,
            }
        }
        false
    }

    // The holder. Wait for the serve-level attach so the lease is provably held
    // before the second process races for it.
    let mut holder = spawn("agent-a");
    let holder_pid = holder.id();
    let holder_err = watch(&mut holder);
    let holder_out = holder.stdout.take().expect("holder stdout");
    let holder_drain = std::thread::spawn(move || {
        let mut lines = BufReader::new(holder_out).lines();
        while let Some(Ok(_)) = lines.next() {}
    });
    assert!(
        wait_for(&holder_err, "lambo serve: session attached"),
        "the holder never attached; cannot set up the proxy case"
    );

    // The proxy. Its client sends nothing at all — same pre-handshake parking as
    // the case above, on the other branch.
    let mut proxy = spawn("agent-b");
    let proxy_pid = proxy.id();
    let proxy_err = watch(&mut proxy);
    let proxy_out = proxy.stdout.take().expect("proxy stdout");
    let proxy_drain = std::thread::spawn(move || {
        let mut lines = BufReader::new(proxy_out).lines();
        while let Some(Ok(_)) = lines.next() {}
    });
    // The sync point that exists BECAUSE "session attached" cannot serve here:
    // a proxy attaches no session and never logs that line (J0 round 1).
    assert!(
        wait_for(&proxy_err, "proxying to the session holder"),
        "the second serve never became a proxy — it may have exited 1 as it did before J2"
    );

    sigterm(proxy_pid);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(proxy.wait());
    });
    match rx.recv_timeout(Duration::from_secs(15)) {
        Ok(status) => assert!(
            status.expect("wait on proxy").success(),
            "a SIGTERM to a proxy must be handled, not fatal"
        ),
        Err(_) => {
            let _ = Command::new("kill")
                .arg("-9")
                .arg(proxy_pid.to_string())
                .status();
            sigterm(holder_pid);
            let _ = std::fs::remove_dir_all(&dir);
            panic!("the proxy did not exit within 15s of a SIGTERM — its shutdown is not wired");
        }
    }

    // The holder is untouched: it still holds the lease, with its endpoint still
    // published. A proxy that took a lease on its way through, or unlinked the
    // holder's socket on exit, fails here.
    let row = rt
        .block_on(async {
            let store = SqliteStore::connect(&db_str).expect("reconnect");
            store.read_lease(&SessionId::from(SESSION)).await
        })
        .expect("read lease")
        .expect("the holder's lease row must still exist");
    assert!(
        row.holder.starts_with("agent-a@"),
        "the proxy must not have become the holder: {}",
        row.holder
    );
    let endpoint = row
        .endpoint
        .expect("the holder still publishes its endpoint");
    assert!(
        std::path::Path::new(&endpoint).exists(),
        "the proxy must not unlink the holder's socket on its way out: {endpoint}"
    );

    // And the holder's own tail is still durable when IT is asked to close.
    sigterm(holder_pid);
    let (htx, hrx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = htx.send(holder.wait());
    });
    match hrx.recv_timeout(Duration::from_secs(20)) {
        Ok(status) => assert!(
            status.expect("wait on holder").success(),
            "the holder must still close cleanly after a proxy came and went"
        ),
        Err(_) => {
            let _ = Command::new("kill")
                .arg("-9")
                .arg(holder_pid.to_string())
                .status();
            let _ = std::fs::remove_dir_all(&dir);
            panic!("the holder did not exit within 20s — a proxy's lifecycle damaged it");
        }
    }
    let _ = holder_drain.join();
    let _ = proxy_drain.join();
    rt.block_on(async {
        let store = SqliteStore::connect(&db_str).expect("reconnect");
        store
            .load_session(&SessionId::from(SESSION))
            .await
            .expect("the holder's session row must be durable");
    });
    let _ = std::fs::remove_dir_all(&dir);
}
