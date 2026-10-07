//! Issue #13: the embedder keep-warm is wired on the **holder** path of the
//! shipped `lambo serve`, and never on the proxy path.
//!
//! The unit tests in `src/embed/keep_warm.rs` pin the policy and the loop, and
//! `src/mcp/serve.rs` pins that a proxy releases its embedder. What neither
//! can see is whether the real binary spawns the task where the design says:
//! a holder with `keep_warm_secs = 1` must arm it and actually touch the
//! embedder, and a second serve on the same session, which becomes a proxy,
//! must do neither.
//!
//! Spawning, environment scrubbing and isolation come from `tests/common`
//! (#15): `lambo_command`, `RuntimeDir`, `ServeChild`, `ScratchDir`.
#![cfg(all(feature = "store-sqlite", feature = "embed-fixture", unix))]

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::Stdio;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use lambo::store::{GraphStore, SqliteStore};

mod common;
use common::{RuntimeDir, ScratchDir, ServeChild, RUNTIME_DIR_VAR};

const SESSION: &str = "issue13-keep-warm-wiring";
const ARMED: &str = "lambo serve: embedder keep-warm armed";
/// The loop's per-touch line (`tracing::debug!` in `keep_warm_loop`). Only
/// emitted at debug level, so the serve runs with `LOG_FILTER`.
const TOUCH: &str = "embedder keep-warm: touch";
/// `lambo_command` clears an ambient `RUST_LOG`; this is the one this test
/// wants: the serve's default info level plus the keep-warm debug line.
const LOG_FILTER: &str = "lambo=info,rmcp=warn,lambo::embed::keep_warm=debug";

/// A running `lambo serve` plus a channel of its stderr lines.
struct Serve {
    /// Held for its drop: kills and reaps the serve.
    _guard: ServeChild,
    stderr: mpsc::Receiver<String>,
}

impl Serve {
    fn spawn(cfg: &Path, runtime: &RuntimeDir, agent: &str) -> Self {
        let mut child = ServeChild::new(
            common::lambo_command()
                .env(RUNTIME_DIR_VAR, runtime.path())
                // After `lambo_command` cleared the ambient value.
                .env("RUST_LOG", LOG_FILTER)
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
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap_or_else(|e| panic!("spawn {agent}: {e}")),
        );
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
        Self {
            _guard: child,
            stderr: rx,
        }
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

#[test]
fn the_holder_arms_and_touches_the_keep_warm_and_a_proxy_does_not() {
    // Declaration order is drop order reversed: the serves are reaped before
    // their runtime dir and scratch files go.
    let scratch = ScratchDir::new("lb13kw");
    let runtime = RuntimeDir::new();
    let db = scratch.join("lambo.db");
    let cfg = scratch.join("lambo.toml");
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

    let holder = Serve::spawn(&cfg, &runtime, "agent-holder");
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
    // "Armed" is logged before the task is spawned, so it cannot prove the
    // spawn. An actual touch can: the first one lands one interval (1 s) after
    // arming, so 2.5 s is generous without being a flake budget.
    assert!(
        holder.wait_for(TOUCH, Duration::from_millis(2_500), &mut holder_log),
        "the holder armed the keep-warm but never touched the embedder within 2.5 s \
         (interval 1 s):\n{}",
        holder_log.join("\n")
    );

    let proxy = Serve::spawn(&cfg, &runtime, "agent-proxy");
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
        !proxy_log
            .iter()
            .any(|l| l.contains(ARMED) || l.contains(TOUCH)),
        "a proxy holds no embedder and must neither arm nor run a keep-warm:\n{}",
        proxy_log.join("\n")
    );

    drop(proxy);
    drop(holder);
}
