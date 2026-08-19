//! I1/I2 — the serve call ledger: one JSONL line per MCP tool call.
//!
//! Lambo is strong on **state** observability (`canonization_events` audits
//! every promotion, `lambo_stats` reports health, the store answers any
//! post-hoc SQL question) and was blind to **flow**: nothing recorded the calls
//! agents actually make, so "agent X recalled, then derived, and a blast-radius
//! warning rendered" existed only in the conversation that received it. This
//! module is the serve-side record of that. See
//! `dev-diary/lambo-for-mooshik/I-observability.md`.
//!
//! # The two rules that shape every line of this file
//!
//! **1. Observability must never take down memory.** A tool call hands its line
//! to a bounded [`std::sync::mpsc::sync_channel`] with `try_send` and returns.
//! There is no `await`, no lock, no file I/O and no allocation of the writer's
//! making on the calling path; a full channel **drops the line** and bumps
//! [`LedgerCounters::dropped`]. Writing happens on a dedicated OS thread — not a
//! Tokio worker — so a slow or hung filesystem cannot starve the runtime of the
//! worker that would otherwise run `Memory::close` on SIGTERM.
//!
//! Every failure mode is a *drop*, counted and visible in `lambo_stats`:
//!
//! | failure | behaviour |
//! | --- | --- |
//! | channel full (writer behind) | drop the line, `dropped += 1` |
//! | path unopenable at startup | one WARN, every line drops, serve still starts |
//! | path unwritable mid-run | one WARN, `dropped += batch`, keep dropping |
//! | shutdown drain exceeds [`SHUTDOWN_DRAIN`] | give up waiting, log, exit |
//!
//! There are **no silent caps**: `dropped` is reported next to `written`, so
//! silence in the file is always distinguishable from silence in the traffic.
//!
//! **2. The ledger is not the store.** No replay semantics, nothing on the serve
//! path ever *reads* it, and the only schema promise is "one JSON object per
//! line, carrying a `v` field". Rotation is the operator's problem
//! (`logrotate`, or just `mv`): the writer reopens the path in append mode for
//! every batch, so a moved-away file is simply recreated on the next line and no
//! `SIGHUP` handling is needed.
//!
//! # Hygiene
//!
//! Concept text and recall queries already live in the store, so the ledger may
//! carry them — and it inherits the store's rules: the file lives **outside the
//! repo** (`~/lambo-dogfood/`), reaches `evidence/` only through the curated
//! export path, and never carries Endor-internal content.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// Schema version stamped on every line as `v`.
///
/// Bump when a field changes meaning or disappears. Adding a field does **not**
/// need a bump: consumers are documented to ignore unknown keys (the analysis
/// kit in `scripts/observability/` does).
pub const LINE_VERSION: u32 = 1;

/// Lines that may sit between the tool calls and the writer thread.
///
/// Sized for a burst, not a backlog. The writer drains everything available per
/// wakeup and does one `write` per batch, so it only falls behind on a genuinely
/// stalled filesystem — exactly the case where dropping is the right answer.
pub const CHANNEL_CAPACITY: usize = 1024;

/// Longest a batch may grow before the writer stops draining and writes.
///
/// Bounds the writer's own buffer so a flood cannot turn into unbounded RSS.
const MAX_BATCH_BYTES: usize = 1 << 20;

/// How long [`Ledger::shutdown`] waits for the writer to finish.
///
/// Bounded on purpose: an unresponsive filesystem must not hold process exit
/// hostage. Exceeding it loses at most [`CHANNEL_CAPACITY`] lines, which is the
/// documented policy rather than a surprise.
pub const SHUTDOWN_DRAIN: Duration = Duration::from_millis(500);

/// The crate version this binary was built from.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The git sha this binary was built from, or `"unknown"`.
///
/// Deliberately an `option_env!` and not a `build.rs`: the heartbeat needs to
/// answer "which pinned binary produced this stretch of ledger", and a build
/// script that shells out to `git` on every consumer's `cargo install` is more
/// machinery than that question is worth. Set it at build time to get a real
/// value —
///
/// ```sh
/// LAMBO_GIT_SHA=$(git rev-parse --short HEAD) cargo build --release
/// ```
///
/// — and note the consequence of *not* setting it: two builds of the same crate
/// version both report `"unknown"`, so a dogfood upgrade event shows as a sha
/// change only if the rig sets the variable when it builds the pinned binary.
/// `DOGFOOD-SETUP.md`'s build step is the place that must do it.
pub const GIT_SHA: &str = match option_env!("LAMBO_GIT_SHA") {
    Some(sha) => sha,
    None => "unknown",
};

/// Written / dropped line counts, shared with the writer thread.
///
/// `dropped` is the load-bearing one: it is what `lambo_stats` reports so that
/// a gap in the ledger is never mistaken for a gap in the traffic.
#[derive(Debug, Default)]
pub struct LedgerCounters {
    written: AtomicU64,
    dropped: AtomicU64,
}

impl LedgerCounters {
    /// Lines successfully appended to the file.
    pub fn written(&self) -> u64 {
        self.written.load(Ordering::Relaxed)
    }

    /// Lines never appended — channel full, or a write that failed.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// A handle onto the append-only call ledger.
///
/// Cheap to clone-by-`Arc` and safe to share: [`Ledger::append`] takes `&self`,
/// never blocks, and never fails.
#[derive(Debug)]
pub struct Ledger {
    path: PathBuf,
    /// `None` once [`Ledger::shutdown`] has run — the writer thread ends when
    /// the last sender drops, so shutdown must be able to drop this one.
    tx: parking_lot::Mutex<Option<SyncSender<Vec<u8>>>>,
    counters: Arc<LedgerCounters>,
    /// Joined by [`Ledger::shutdown`], bounded by [`SHUTDOWN_DRAIN`].
    writer: parking_lot::Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Ledger {
    /// Open (or create) the ledger at `path` and start its writer thread.
    ///
    /// **Never fails.** An unopenable path logs one WARN naming the path and the
    /// error and returns a ledger that counts every line as dropped: a typo in
    /// `--ledger` must not stop a memory server from serving memory. The startup
    /// probe exists so the operator learns about the typo at startup rather than
    /// from an empty file a day later.
    pub fn open(path: impl Into<PathBuf>) -> Arc<Self> {
        let path = path.into();
        let counters = Arc::new(LedgerCounters::default());

        // Probe once, up front, so a bad path is loud at startup. The writer
        // reopens per batch regardless (rotation-friendly), so this handle is
        // dropped immediately — it is a check, not the writer's file.
        if let Err(err) = open_for_append(&path) {
            tracing::warn!(
                target: "lambo::ledger",
                path = %path.display(),
                error = %err,
                "lambo serve: the call ledger path could not be opened; every line will be \
                 DROPPED and counted in lambo_stats (ledger_dropped_lines). Serving continues — \
                 observability never takes down memory."
            );
        }

        let (tx, rx) = sync_channel::<Vec<u8>>(CHANNEL_CAPACITY);
        let writer = std::thread::Builder::new()
            .name("lambo-ledger".to_string())
            .spawn({
                let path = path.clone();
                let counters = Arc::clone(&counters);
                move || writer_loop(&path, rx, &counters)
            });

        let writer = match writer {
            Ok(handle) => Some(handle),
            Err(err) => {
                // The OS refused a thread. Same policy as an unopenable path:
                // count drops, keep serving.
                tracing::warn!(
                    target: "lambo::ledger",
                    error = %err,
                    "lambo serve: could not spawn the ledger writer thread; every line will be \
                     DROPPED and counted in lambo_stats"
                );
                None
            }
        };

        Arc::new(Self {
            path,
            tx: parking_lot::Mutex::new(Some(tx)),
            counters,
            writer: parking_lot::Mutex::new(writer),
        })
    }

    /// The path lines are appended to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Written / dropped counts, for `lambo_stats` and the heartbeat.
    pub fn counters(&self) -> &Arc<LedgerCounters> {
        &self.counters
    }

    /// Append one line. **Never blocks, never fails, never awaits.**
    ///
    /// Serialization happens on the calling thread (it is a few hundred bytes of
    /// `serde_json`, and doing it here keeps the writer thread's batch a single
    /// contiguous `write`); handing it over is a `try_send`. A full channel or a
    /// departed writer drops the line and bumps `dropped`.
    pub fn append(&self, line: &Value) {
        let mut bytes = match serde_json::to_vec(line) {
            Ok(b) => b,
            Err(err) => {
                // A `Value` that will not serialize is a bug in the caller, not
                // an operational condition — but it is still only a dropped
                // line, never a failed tool call.
                tracing::debug!(
                    target: "lambo::ledger",
                    error = %err,
                    "ledger: line could not be serialized; dropped"
                );
                self.counters.dropped.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        bytes.push(b'\n');

        let guard = self.tx.lock();
        let Some(tx) = guard.as_ref() else {
            // Post-shutdown call. Counted, not silent.
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        match tx.try_send(bytes) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Stop accepting lines and give the writer up to [`SHUTDOWN_DRAIN`] to
    /// flush what it already holds.
    ///
    /// Idempotent. Bounded: a writer stuck on a hung filesystem is abandoned
    /// with a log line rather than allowed to hold process exit.
    pub fn shutdown(&self) {
        // Dropping the sender is what tells the writer loop to finish.
        drop(self.tx.lock().take());
        let Some(handle) = self.writer.lock().take() else {
            return;
        };
        let deadline = Instant::now() + SHUTDOWN_DRAIN;
        while !handle.is_finished() {
            if Instant::now() >= deadline {
                tracing::warn!(
                    target: "lambo::ledger",
                    dropped = self.counters.dropped(),
                    written = self.counters.written(),
                    "ledger: writer did not drain within the shutdown budget; abandoning it \
                     (buffered lines are lost — counted policy, not a surprise)"
                );
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        // Finished: the join is immediate and cannot block.
        let _ = handle.join();
    }
}

/// Open `path` for appending, creating it if absent.
///
/// Reopened per batch rather than held: that is what makes `logrotate` (or a
/// bare `mv`) work with no signal handling, and what makes "the path became
/// unwritable mid-run" a condition this module can actually observe.
fn open_for_append(path: &Path) -> std::io::Result<std::fs::File> {
    OpenOptions::new().create(true).append(true).open(path)
}

/// The writer thread: drain, write, repeat until every sender is gone.
fn writer_loop(path: &Path, rx: Receiver<Vec<u8>>, counters: &LedgerCounters) {
    // One WARN for the whole run, however many writes fail (I1: "logs its own
    // failure once"). A recovered write re-arms it, so an operator who fixes the
    // path and breaks it again is told twice — which is information, not noise.
    let warned = AtomicBool::new(false);

    while let Ok(first) = rx.recv() {
        let mut batch = first;
        let mut lines = 1u64;
        // Drain whatever else is already queued into the same write.
        while batch.len() < MAX_BATCH_BYTES {
            match rx.try_recv() {
                Ok(next) => {
                    batch.extend_from_slice(&next);
                    lines += 1;
                }
                Err(_) => break,
            }
        }

        match write_batch(path, &batch) {
            Ok(()) => {
                counters.written.fetch_add(lines, Ordering::Relaxed);
                warned.store(false, Ordering::Relaxed);
            }
            Err(err) => {
                counters.dropped.fetch_add(lines, Ordering::Relaxed);
                if !warned.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        target: "lambo::ledger",
                        path = %path.display(),
                        error = %err,
                        lines,
                        "ledger: append failed; these lines are DROPPED and counted in \
                         lambo_stats (ledger_dropped_lines). Further failures are silent until \
                         one write succeeds. Tool calls are unaffected."
                    );
                }
            }
        }
    }
}

/// One `open` + one `write_all` + one `flush` per batch.
fn write_batch(path: &Path, batch: &[u8]) -> std::io::Result<()> {
    let mut file = open_for_append(path)?;
    file.write_all(batch)?;
    file.flush()
}

// ---------------------------------------------------------------------------
// Line builders
// ---------------------------------------------------------------------------

/// The common head every line carries: `v`, a **server** timestamp, and `kind`.
///
/// The timestamp is stamped here and nowhere else. F18's rule for the graph — no
/// client ever supplies a time — holds for the ledger too, for the same reason:
/// a ledger whose ordering a client can influence cannot answer the
/// recall-first question.
fn head(kind: &str) -> Value {
    json!({
        "v": LINE_VERSION,
        "ts": chrono::Utc::now().to_rfc3339(),
        "kind": kind,
    })
}

/// A `call` line: one MCP tool call.
///
/// `facts` are the per-tool payload facts (see `scripts/observability/README.md`
/// for the per-tool shape) and are merged into the line at the top level.
pub fn call_line(
    tool: &str,
    agent_id: &str,
    outcome: &str,
    error_kind: Option<&str>,
    duration_us: u64,
    facts: Option<Value>,
) -> Value {
    let mut line = head("call");
    let obj = line.as_object_mut().expect("head is an object");
    obj.insert("tool".into(), json!(tool));
    obj.insert("agent_id".into(), json!(agent_id));
    obj.insert("outcome".into(), json!(outcome));
    if let Some(kind) = error_kind {
        obj.insert("error_kind".into(), json!(kind));
    }
    obj.insert("duration_us".into(), json!(duration_us));
    if let Some(Value::Object(facts)) = facts {
        for (k, v) in facts {
            obj.insert(k, v);
        }
    }
    line
}

/// A `stats` heartbeat line (I2): the `lambo_stats` payload plus uptime and the
/// binary's identity.
pub fn stats_line(stats: Value, uptime: Duration) -> Value {
    let mut line = head("stats");
    let obj = line.as_object_mut().expect("head is an object");
    obj.insert("uptime_secs".into(), json!(uptime.as_secs()));
    obj.insert("version".into(), json!(VERSION));
    obj.insert("git_sha".into(), json!(GIT_SHA));
    obj.insert("stats".into(), stats);
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ledger under a temp dir, plus the dir (kept alive by the caller).
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lambo-ledger-{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// Spin until `f` holds or the budget expires. The writer is a real thread,
    /// so tests wait on its effect rather than sleeping a guessed interval.
    fn until(budget: Duration, mut f: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        f()
    }

    #[test]
    fn ledger_lines_round_trip_as_one_json_object_per_line() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("calls.jsonl");
        let ledger = Ledger::open(&path);
        for i in 0..50 {
            ledger.append(&call_line(
                "lambo_derive",
                "agent-a",
                "ok",
                None,
                100 + i,
                Some(json!({"created": i, "matched": 0})),
            ));
        }
        ledger.shutdown();

        let text = std::fs::read_to_string(&path).expect("ledger file");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 50, "one line per append: {text}");
        for line in lines {
            let v: Value = serde_json::from_str(line).expect("every line parses as JSON");
            assert_eq!(v["v"], json!(LINE_VERSION));
            assert_eq!(v["kind"], json!("call"));
            assert_eq!(v["tool"], json!("lambo_derive"));
            assert_eq!(v["agent_id"], json!("agent-a"));
            assert!(v["ts"].as_str().is_some(), "server timestamp present");
        }
        assert_eq!(ledger.counters().dropped(), 0);
        assert_eq!(ledger.counters().written(), 50);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unopenable_path_drops_every_line_and_never_panics() {
        // A path whose parent is a *file*, so no `create` can succeed.
        let dir = temp_dir("unopenable");
        let blocker = dir.join("not-a-dir");
        std::fs::write(&blocker, b"x").expect("blocker");
        let path = blocker.join("calls.jsonl");

        let ledger = Ledger::open(&path);
        for _ in 0..10 {
            ledger.append(&call_line("lambo_stats", "a", "ok", None, 1, None));
        }
        assert!(
            until(Duration::from_secs(5), || ledger.counters().dropped() == 10),
            "every line drops when the path cannot be opened, dropped={}",
            ledger.counters().dropped()
        );
        assert_eq!(ledger.counters().written(), 0);
        ledger.shutdown();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_path_that_becomes_unwritable_mid_run_drops_and_keeps_counting() {
        let dir = temp_dir("mid-run");
        let path = dir.join("calls.jsonl");
        let ledger = Ledger::open(&path);

        ledger.append(&call_line("lambo_recall", "a", "ok", None, 1, None));
        assert!(
            until(Duration::from_secs(5), || ledger.counters().written() == 1),
            "the first line lands"
        );

        // Remove the whole directory: the writer reopens per batch, so the next
        // batch's `open` fails. Chosen over `chmod` because it fails the same
        // way for root, which some CI containers are.
        std::fs::remove_dir_all(&dir).expect("remove the ledger's directory");

        for _ in 0..5 {
            ledger.append(&call_line("lambo_recall", "a", "ok", None, 1, None));
        }
        assert!(
            until(Duration::from_secs(5), || ledger.counters().dropped() == 5),
            "post-failure lines are dropped and counted, dropped={}",
            ledger.counters().dropped()
        );
        assert_eq!(
            ledger.counters().written(),
            1,
            "the one pre-failure line stays written"
        );
        ledger.shutdown();
    }

    #[test]
    fn a_full_channel_drops_rather_than_blocking_the_caller() {
        // No writer thread can drain a channel whose receiver is parked on a
        // path it cannot open, so push well past the capacity and assert the
        // calls all returned (the test itself completing IS the assertion that
        // `append` never blocked) with the overflow counted.
        let dir = temp_dir("full");
        let blocker = dir.join("not-a-dir");
        std::fs::write(&blocker, b"x").expect("blocker");
        let ledger = Ledger::open(blocker.join("calls.jsonl"));

        let n = CHANNEL_CAPACITY as u64 * 3;
        for _ in 0..n {
            ledger.append(&call_line("lambo_derive", "a", "ok", None, 1, None));
        }
        assert!(
            until(Duration::from_secs(10), || ledger.counters().dropped() == n),
            "every line is accounted for as a drop: dropped={} of {n}",
            ledger.counters().dropped()
        );
        ledger.shutdown();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_stats_line_carries_the_version_and_the_sha() {
        let line = stats_line(json!({"node_count": 3}), Duration::from_secs(90));
        assert_eq!(line["kind"], json!("stats"));
        assert_eq!(line["v"], json!(LINE_VERSION));
        assert_eq!(line["uptime_secs"], json!(90));
        assert_eq!(line["version"], json!(VERSION));
        assert_eq!(
            line["git_sha"],
            json!(GIT_SHA),
            "the sha field is always present, 'unknown' when unset at build time"
        );
        assert_eq!(line["stats"]["node_count"], json!(3));
    }

    #[test]
    fn a_call_line_merges_per_tool_facts_at_the_top_level() {
        let line = call_line(
            "lambo_derive",
            "agent-x",
            "error",
            Some("invalid params"),
            42,
            Some(json!({"created": 0, "matched": 0})),
        );
        assert_eq!(line["outcome"], json!("error"));
        assert_eq!(line["error_kind"], json!("invalid params"));
        assert_eq!(line["duration_us"], json!(42));
        assert_eq!(line["created"], json!(0));
        assert_eq!(line["matched"], json!(0));
    }

    #[test]
    fn appending_after_shutdown_is_counted_not_silent() {
        let dir = temp_dir("post-shutdown");
        let path = dir.join("calls.jsonl");
        let ledger = Ledger::open(&path);
        ledger.append(&call_line("lambo_stats", "a", "ok", None, 1, None));
        ledger.shutdown();
        ledger.shutdown(); // idempotent
        ledger.append(&call_line("lambo_stats", "a", "ok", None, 1, None));
        assert_eq!(ledger.counters().dropped(), 1);
        assert_eq!(ledger.counters().written(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
