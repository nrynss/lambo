//! Issue #13: the embedder keep-warm is wired on the **holder** path of the
//! shipped `lambo serve`, and never on the proxy path.
//!
//! The unit tests in `src/embed/keep_warm.rs` pin the policy and the loop, and
//! `src/mcp/serve.rs` pins that a proxy releases its embedder. What neither
//! can see is whether the real binary spawns the task where the design says:
//! a holder with `keep_warm_secs = 1` must log "lambo serve: embedder
//! keep-warm armed", and a second serve on the same session, which becomes a
//! proxy, must not.
//!
//! Self-contained on purpose: the scratch directory, the per-test
//! `XDG_RUNTIME_DIR` and the kill-and-reap child guard are local to this file.
//! Once the #15 test-isolation helpers (`tests/common`) land, this should
//! switch to them.
#![cfg(all(feature = "store-sqlite", feature = "embed-fixture", unix))]

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use lambo::store::{GraphStore, SqliteStore};

const SESSION: &str = "issue13-keep-warm-wiring";
const ARMED: &str = "lambo serve: embedder keep-warm armed";

/// A scratch directory under `/tmp` (short enough for the endpoint socket
/// address), removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let dir = PathBuf::from(format!("/tmp/lb13kw-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(dir.join("run")).expect("scratch dir");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A `lambo serve` child that is killed and reaped on drop, so a failing
/// assertion never leaks a serve holding a lease or a socket.
struct ServeChild {
    child: Child,
    stderr: mpsc::Receiver<String>,
}

impl ServeChild {
    fn spawn(cfg: &Path, runtime_dir: &Path, agent: &str) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_lambo"));
        // Ambient LAMBO_* overrides (an operator's shell, a dogfood rig) must
        // not reach this serve: the env overlay beats the file, so an inherited
        // LAMBO_EMBED_KEEP_WARM_SECS=0 would switch off what is under test.
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("LAMBO_") {
                cmd.env_remove(&key);
            }
        }
        let mut child = cmd
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
            .env("XDG_RUNTIME_DIR", runtime_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {agent}: {e}"));
        let stderr = child.stderr.take().expect("stderr");
        let (tx, rx) = mpsc::channel();
        let tag = agent.to_string();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                eprintln!("[{tag} stderr] {line}");
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self { child, stderr: rx }
    }

    /// Collect stderr lines into `seen` until one contains `needle` (`true`)
    /// or `budget` passes (`false`).
    fn wait_for(&self, needle: &str, budget: Duration, seen: &mut Vec<String>) -> bool {
        let deadline = Instant::now() + budget;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            match self.stderr.recv_timeout(left) {
                Ok(line) => {
                    let hit = line.contains(needle);
                    seen.push(line);
                    if hit {
                        return true;
                    }
                }
                Err(_) => return false,
            }
        }
    }

    /// Everything that arrives on stderr within `window`.
    fn drain_for(&self, window: Duration, seen: &mut Vec<String>) {
        let deadline = Instant::now() + window;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.stderr.recv_timeout(left) {
                Ok(line) => seen.push(line),
                Err(_) => return,
            }
        }
    }
}

impl Drop for ServeChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn the_holder_arms_the_keep_warm_and_a_proxy_does_not() {
    let scratch = Scratch::new();
    let db = scratch.path().join("lambo.db");
    let cfg = scratch.path().join("lambo.toml");
    // The fixture embedder is off under the auto rule (no weights), so the
    // explicit `keep_warm_secs = 1` is what opts it in: the wiring under test,
    // not the auto policy (that is unit-tested).
    std::fs::write(
        &cfg,
        format!(
            "[store]\nkind = \"sqlite\"\npath = \"{}\"\n\n[embedder]\nkind = \"fixture\"\n\
             dim = 1024\nkeep_warm_secs = 1\n",
            db.display()
        ),
    )
    .expect("write config");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        SqliteStore::connect(db.to_str().unwrap())
            .expect("connect")
            .init_schema()
            .await
            .expect("provision");
    });
    let runtime_dir = scratch.path().join("run");

    let holder = ServeChild::spawn(&cfg, &runtime_dir, "agent-holder");
    let mut holder_log = Vec::new();
    assert!(
        holder.wait_for(
            "lambo serve: session attached",
            Duration::from_secs(25),
            &mut holder_log
        ),
        "the holder never attached:\n{}",
        holder_log.join("\n")
    );
    assert!(
        holder_log.iter().any(|l| l.contains(ARMED)),
        "the holder must arm the keep-warm (it is logged before the serve-level attach \
         line):\n{}",
        holder_log.join("\n")
    );

    let proxy = ServeChild::spawn(&cfg, &runtime_dir, "agent-proxy");
    let mut proxy_log = Vec::new();
    assert!(
        proxy.wait_for(
            "proxying to the session holder",
            Duration::from_secs(25),
            &mut proxy_log
        ),
        "the second serve never became a proxy:\n{}",
        proxy_log.join("\n")
    );
    // Longer than the 1 s interval, so a proxy-side task armed late would
    // have had time to log and touch.
    proxy.drain_for(Duration::from_millis(1_500), &mut proxy_log);
    assert!(
        !proxy_log.iter().any(|l| l.contains(ARMED)),
        "a proxy holds no embedder and must not arm a keep-warm:\n{}",
        proxy_log.join("\n")
    );

    drop(proxy);
    drop(holder);
}
