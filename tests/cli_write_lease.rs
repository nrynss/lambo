//! T8.3: CLI write verbs acquire/refuse the T8.6 lease, and readers stay
//! lease-free while a serve owns the session.
//!
//! Follows `tests/serve_single_writer_lease.rs`: handshake so we know the
//! lease is held, then drive the shipped binary. Gated `store-sqlite` + unix.
#![cfg(all(feature = "store-sqlite", feature = "embed-fixture", unix))]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use lambo::store::{GraphStore, SqliteStore};

const SESSION: &str = "t8.3-cli-lease";

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

fn lambo() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lambo"));
    for k in [
        "LAMBO_STORE",
        "LAMBO_EMBEDDER",
        "LAMBO_CONFIG",
        "LAMBO_COCKROACH_DSN",
        "DATABASE_URL",
        "LAMBO_SQLITE_PATH",
        "LAMBO_EMBED_DIM",
        "LAMBO_LLAMA_EMBED_URL",
        "LAMBO_LLAMA_MODEL",
    ] {
        cmd.env_remove(k);
    }
    cmd
}

fn serve_cmd(cfg_path: &std::path::Path, agent: &str) -> Command {
    let mut cmd = lambo();
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

fn scratch() -> (std::path::PathBuf, std::path::PathBuf, String) {
    let dir = std::env::temp_dir().join(format!(
        "lambo-cli-lease-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("scratch");
    let db_path = dir.join("lease.sqlite");
    let db_str = db_path.to_str().expect("utf-8").to_string();
    let cfg_path = dir.join("lambo.toml");
    std::fs::write(
        &cfg_path,
        format!(
            "[store]\nkind = \"sqlite\"\npath = \"{db_str}\"\n\n[embedder]\nkind = \"fixture\"\ndim = 1024\n"
        ),
    )
    .expect("write config");
    (dir, cfg_path, db_str)
}

fn derive_cmd(cfg: &std::path::Path, agent: &str) -> Command {
    let mut cmd = lambo();
    cmd.args([
        "--config",
        cfg.to_str().unwrap(),
        "derive",
        "--session",
        SESSION,
        "--agent",
        agent,
        "--content",
        "user schema",
        "--kind",
        "entity",
    ]);
    cmd
}

#[test]
fn derive_succeeds_with_no_serve_and_fails_closed_while_serve_holds() {
    let (dir, cfg, db_str) = scratch();
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(async {
        let store = SqliteStore::connect(&db_str).expect("connect");
        store.init_schema().await.expect("init_schema");
    });

    // No serve: derive acquires the lease, writes, releases.
    let free = derive_cmd(&cfg, "agent-free")
        .output()
        .expect("derive with no serve");
    assert!(
        free.status.success(),
        "derive with no serve must succeed; stderr=\n{}",
        String::from_utf8_lossy(&free.stderr)
    );

    // Serve holds the session.
    let mut a: Child = serve_cmd(&cfg, "agent-a")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn serve");
    let a_pid = a.id();
    let a_stdout = a.stdout.take().expect("stdout");
    let (tx, rx) = mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        let mut lines = BufReader::new(a_stdout).lines();
        while let Some(Ok(line)) = lines.next() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut a_stdin = a.stdin.take().expect("stdin");
    write_frame(
        &mut a_stdin,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"cli-lease","version":"1"}}}"#,
    );
    let init = read_response(&rx, 1);
    assert!(
        init.contains("\"serverInfo\""),
        "serve did not complete initialize: {init}"
    );

    // Writer must fail closed, naming the holder.
    let refused = derive_cmd(&cfg, "agent-b")
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .output()
        .expect("derive while serve holds");
    assert!(
        !refused.status.success(),
        "derive while serve holds must fail closed; stdout=\n{}",
        String::from_utf8_lossy(&refused.stdout)
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("single-writer"),
        "must name the single-writer lease; stderr=\n{stderr}"
    );
    assert!(
        stderr.contains("agent-a"),
        "must name the holder; stderr=\n{stderr}"
    );

    // Readers must still succeed (spec §2.2).
    for (name, args) in [
        ("saints", vec!["saints", "--session", SESSION]),
        ("stats", vec!["stats", "--session", SESSION]),
        (
            "recall",
            vec!["recall", "--session", SESSION, "--query", "user schema"],
        ),
        (
            "inspect",
            vec!["inspect", "--session", SESSION, "--focus", "user schema"],
        ),
    ] {
        let mut cmd = lambo();
        cmd.arg("--config").arg(cfg.to_str().unwrap());
        cmd.args(&args);
        let out = cmd.output().unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(
            out.status.success(),
            "{name} is a reader and must succeed while serve holds; stderr=\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    sigterm(a_pid);
    let _ = a.wait();
    drop(a_stdin);
    let _ = reader.join();
    let _ = std::fs::remove_dir_all(&dir);
}
