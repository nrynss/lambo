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
//! There is **no `await`, no file I/O, and no lock held across anything that can
//! block** on the calling path — the one lock `append` takes is a
//! `parking_lot::Mutex` around the sender, held for the duration of a
//! non-blocking `try_send` and nothing else — and no allocation of the writer's
//! making. A full channel **drops the line** and bumps
//! [`LedgerCounters::dropped_channel_full`]. Writing happens on a dedicated OS
//! thread — not a Tokio worker — so a slow or hung filesystem cannot starve the
//! runtime of the worker that would otherwise run `Memory::close` on SIGTERM.
//!
//! Every failure mode is a *drop*, counted and visible in `lambo_stats`. The two
//! counters are separate because they are different operational facts: a full
//! channel means the writer is behind, an unwritten batch means the path is
//! broken, and an operator reading one total cannot tell those apart.
//!
//! | failure | behaviour |
//! | --- | --- |
//! | channel full (writer behind) | drop the line, `dropped_channel_full += 1` |
//! | path **cannot** be opened (`ENOENT`, `ENOTDIR`, `EACCES`) | one WARN from the writer thread, every batch drops, serve serves |
//! | path's `open` **blocks** (a FIFO with no reader, a hung mount) | the writer thread parks; serve starts, serves, and shuts down normally. Lines **queue** (visible immediately as `ledger_queued_lines`, `written` and both drop counters still `0`); once the queue fills they drop as channel-full; any still queued at shutdown are counted as `dropped_write_failed` by the abandoned-lines path |
//! | path unwritable mid-run | one WARN, `dropped_write_failed += batch`, keep dropping |
//! | shutdown drain exceeds [`SHUTDOWN_DRAIN`] | give up waiting, log, count the abandoned lines as `dropped_write_failed`, exit |
//!
//! "Cannot open" and "open blocks" are deliberately separate rows: the first is
//! a typo and is loud, the second is a filesystem condition no amount of
//! error-handling can turn into a return value. Both are survivable because
//! **nothing on the startup path opens the file** — the probe that makes a typo
//! loud runs as the writer thread's first act, which is precisely the thread the
//! OS-thread design exists to let park.
//!
//! There are **no silent caps**: both drop counters are reported next to
//! `written`, so silence in the file is always distinguishable from silence in
//! the traffic. A *queue* is not a drop, so it needs its own field — a parked
//! writer has dropped nothing yet, and reporting only drops made it look
//! identical to an idle one until the queue filled. [`LedgerCounters::queued`]
//! (`ledger_queued_lines`) closes that blind spot (I-R2-3).
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
/// hostage. Exceeding it loses at most **[`CHANNEL_CAPACITY`] lines plus one
/// in-flight batch** (the batch the writer is holding inside its `write`, itself
/// bounded by [`MAX_BATCH_BYTES`]) — and those lines are *counted*, as
/// `dropped_write_failed`, by [`Ledger::shutdown`] before it returns. The policy
/// is documented, bounded and visible rather than a surprise.
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
/// `dev-diary/lambo-for-mooshik/DOGFOOD-SETUP.md` **§2 "The pinned binary"** owns
/// the rig's build step and is the place that sets it.
pub const GIT_SHA: &str = match option_env!("LAMBO_GIT_SHA") {
    Some(sha) => sha,
    None => "unknown",
};

/// Written / dropped line counts, shared with the writer thread.
///
/// The dropped counts are the load-bearing ones: they are what `lambo_stats`
/// reports so that a gap in the ledger is never mistaken for a gap in the
/// traffic. They are **split by cause** because the two causes are different
/// operational facts and a single total cannot distinguish them — which is
/// exactly the position an earlier revision left an operator (and a test) in.
#[derive(Debug, Default)]
pub struct LedgerCounters {
    written: AtomicU64,
    /// Backpressure: `try_send` rejected the line because the writer is behind.
    channel_full: AtomicU64,
    /// Everything else: see [`LedgerCounters::dropped_write_failed`].
    write_failed: AtomicU64,
    /// Lines the channel accepted — written, still in flight, or abandoned.
    ///
    /// Not reported directly, but [`LedgerCounters::queued`] derives
    /// `lambo_stats`'s `ledger_queued_lines` from it, and [`Ledger::shutdown`]
    /// uses the same arithmetic to count the lines an abandoned writer was still
    /// holding instead of losing them silently.
    ///
    /// Note what this does **not** count: a `channel_full` drop was rejected by
    /// `try_send` and never accepted, so it never appears here. That is why both
    /// derivations subtract `write_failed` rather than the `dropped` total.
    accepted: AtomicU64,
}

impl LedgerCounters {
    /// Lines successfully appended to the file.
    pub fn written(&self) -> u64 {
        self.written.load(Ordering::Relaxed)
    }

    /// Lines the channel refused because the writer was behind — **backpressure**.
    ///
    /// The first row of this module's failure table, and the one a genuinely
    /// stalled filesystem produces. Non-zero here means the ledger is an
    /// undercount *and* that the writer could not keep up.
    pub fn dropped_channel_full(&self) -> u64 {
        self.channel_full.load(Ordering::Relaxed)
    }

    /// Lines that never reached the file for any reason other than backpressure:
    ///
    /// * a batch whose `write` (or its `open`) failed — the path is broken;
    /// * a batch abandoned when [`SHUTDOWN_DRAIN`] expired;
    /// * an [`Ledger::append`] after [`Ledger::shutdown`];
    /// * a `Value` that would not serialize (a caller bug, still only a drop).
    ///
    /// Non-zero here points at the *path or the process*, not at throughput.
    pub fn dropped_write_failed(&self) -> u64 {
        self.write_failed.load(Ordering::Relaxed)
    }

    /// Every dropped line, whatever the cause.
    ///
    /// Retained as the headline number (`lambo_stats`'s `ledger_dropped_lines`)
    /// so "is this ledger complete?" stays one field; the split answers "why".
    pub fn dropped(&self) -> u64 {
        self.dropped_channel_full() + self.dropped_write_failed()
    }

    /// Lines accepted but not yet on disk — the writer's queue depth.
    ///
    /// `lambo_stats`'s `ledger_queued_lines`, and the answer to a blind spot
    /// (I-R2-3): on a path whose `open` blocks — a reader-less FIFO, a hung mount
    /// — the writer parks *before its first write*, so `written`, and both drop
    /// counters, stay `0` until [`CHANNEL_CAPACITY`] lines have piled up. An
    /// operator watching the other keys reads that as "no traffic" when it is
    /// really "writer parked". This one moves on the very first call.
    ///
    /// Bounded by [`CHANNEL_CAPACITY`] plus one in-flight batch. Subtracts
    /// `write_failed` and **not** `dropped`, for the reason on
    /// [`LedgerCounters::accepted`]: a `channel_full` drop was never accepted, so
    /// subtracting it would understate the depth by exactly the backpressure
    /// count — and would drive this key back to `0` in the stalled-and-full case
    /// it exists to make visible.
    ///
    /// A gauge sampled from relaxed loads, so it can be momentarily inconsistent
    /// with its parts under concurrency; `saturating_sub` keeps it at worst `0`
    /// rather than wrapping.
    pub fn queued(&self) -> u64 {
        self.accepted
            .load(Ordering::Relaxed)
            .saturating_sub(self.written())
            .saturating_sub(self.dropped_write_failed())
    }
}

/// How a batch reaches durable storage.
///
/// Production always uses [`write_batch`]. The indirection exists for one
/// reason: the channel-full row of this module's failure table is only reachable
/// when the writer is *inside* a write that has not returned, and no black-box
/// test can hold a real filesystem there deterministically. Injecting the sink
/// lets a test park the writer on demand and prove backpressure drops rather
/// than blocking the caller.
type BatchSink = Arc<dyn Fn(&Path, &[u8]) -> std::io::Result<()> + Send + Sync>;

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
    /// Start the ledger's writer thread for `path`.
    ///
    /// **Never fails the server, and performs no I/O of its own.** Opening,
    /// probing and writing all happen on the writer thread, so neither an
    /// unopenable path (a typo: one WARN, every batch dropped) nor a path whose
    /// `open` *blocks* (a FIFO with no reader, a hung mount: the writer parks)
    /// can delay or wedge the caller. That distinction is the whole reason the
    /// probe is not here: `serve` calls this **after** the single-writer lease
    /// is taken and — since I-R2-1 — **after** the SIGTERM handler is armed
    /// (`shutdown_signal()` is the first statement once `build_memory` returns;
    /// this call is the next one). What that ordering would change, were the
    /// probe ever moved back here, is the hazard *class*: it would be an
    /// **availability** hazard rather than the durability one it used to be. A
    /// blocking `open` here would wedge `serve` between the lease and the
    /// transport, holding the session in a process that never serves, and it
    /// could not be shut down either — the pinned shutdown future is never
    /// polled while the main task sits in the blocking syscall, so
    /// `Memory::close` never runs. Before I-R2-1 the same block lost the tail
    /// instead, the signal hitting its default disposition and killing the
    /// process outright. Either way it is observability taking down memory
    /// through the flag that turns observability on.
    ///
    /// The operator still learns about a typo at startup rather than from an
    /// empty file a day later — the writer thread probes as its first act, with
    /// no tool call needed to provoke it.
    pub fn open(path: impl Into<PathBuf>) -> Arc<Self> {
        Self::open_with_sink(path, Arc::new(write_batch))
    }

    /// [`Ledger::open`], with the durable-write step injected. See [`BatchSink`].
    fn open_with_sink(path: impl Into<PathBuf>, sink: BatchSink) -> Arc<Self> {
        let path = path.into();
        let counters = Arc::new(LedgerCounters::default());

        let (tx, rx) = sync_channel::<Vec<u8>>(CHANNEL_CAPACITY);
        let writer = std::thread::Builder::new()
            .name("lambo-ledger".to_string())
            .spawn({
                let path = path.clone();
                let counters = Arc::clone(&counters);
                move || writer_loop(&path, rx, &counters, &sink)
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
    /// contiguous `write`); handing it over is a `try_send`. A full channel bumps
    /// `dropped_channel_full`; a departed writer or a post-shutdown call bumps
    /// `dropped_write_failed`.
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
                self.counters.write_failed.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        bytes.push(b'\n');

        let guard = self.tx.lock();
        let Some(tx) = guard.as_ref() else {
            // Post-shutdown call. Counted, not silent.
            self.counters.write_failed.fetch_add(1, Ordering::Relaxed);
            return;
        };
        match tx.try_send(bytes) {
            Ok(()) => {
                self.counters.accepted.fetch_add(1, Ordering::Relaxed);
            }
            // The writer is behind. This is the failure table's first row and
            // the one a stalled filesystem produces; counted on its own so an
            // operator (and a test) can tell it from a broken path.
            Err(TrySendError::Full(_)) => {
                self.counters.channel_full.fetch_add(1, Ordering::Relaxed);
            }
            // The writer thread is gone (it panicked, or the OS refused to
            // spawn it). Not backpressure — the line had nowhere to go.
            Err(TrySendError::Disconnected(_)) => {
                self.counters.write_failed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Stop accepting lines and give the writer up to [`SHUTDOWN_DRAIN`] to
    /// flush what it already holds.
    ///
    /// Idempotent. Bounded: a writer stuck on a hung filesystem is abandoned
    /// with a log line rather than allowed to hold process exit. The lines it
    /// was still holding are **counted as drops before this returns** — the
    /// sender is already gone, so `accepted - written - write_failed` is exactly
    /// what the abandoned writer had in hand plus whatever is still queued,
    /// bounded by [`CHANNEL_CAPACITY`] plus one [`MAX_BATCH_BYTES`] batch.
    pub fn shutdown(&self) {
        // Dropping the sender is what tells the writer loop to finish.
        drop(self.tx.lock().take());
        let Some(handle) = self.writer.lock().take() else {
            return;
        };
        let deadline = Instant::now() + SHUTDOWN_DRAIN;
        while !handle.is_finished() {
            if Instant::now() >= deadline {
                // No sender remains, so `accepted` can no longer move: the
                // arithmetic below is a settled count, not a sample.
                let abandoned = self
                    .counters
                    .accepted
                    .load(Ordering::Relaxed)
                    .saturating_sub(self.counters.written())
                    .saturating_sub(self.counters.dropped_write_failed());
                self.counters
                    .write_failed
                    .fetch_add(abandoned, Ordering::Relaxed);
                tracing::warn!(
                    target: "lambo::ledger",
                    abandoned,
                    dropped = self.counters.dropped(),
                    written = self.counters.written(),
                    "ledger: writer did not drain within the shutdown budget; abandoning it. \
                     The lines it still held are counted in ledger_dropped_lines — bounded by \
                     the channel capacity plus one batch, and never silent."
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

/// The writer thread: probe once, then drain, write, repeat until every sender
/// is gone.
///
/// **The probe lives here, not in [`Ledger::open`].** It is the same check it
/// always was — an `open` in append mode, dropped immediately, so a typo in
/// `--ledger` is loud at startup instead of showing up as an empty file a day
/// later — but running it on this thread is what makes the loudness free. An
/// `open` that cannot succeed warns; an `open` that *blocks* parks this thread
/// and nothing else, which is exactly the failure the OS-thread design exists to
/// absorb. On the runtime's main task the same call wedged `serve` between the
/// lease and the SIGTERM handler.
fn writer_loop(path: &Path, rx: Receiver<Vec<u8>>, counters: &LedgerCounters, sink: &BatchSink) {
    // One WARN for the whole run, however many writes fail (I1: "logs its own
    // failure once"). A recovered write re-arms it, so an operator who fixes the
    // path and breaks it again is told twice — which is information, not noise.
    let warned = AtomicBool::new(false);

    if let Err(err) = open_for_append(path) {
        tracing::warn!(
            target: "lambo::ledger",
            path = %path.display(),
            error = %err,
            "lambo serve: the call ledger path could not be opened; every line will be \
             DROPPED and counted in lambo_stats (ledger_dropped_lines). Serving continues — \
             observability never takes down memory."
        );
        // Already told. The first failing batch must not repeat it.
        warned.store(true, Ordering::Relaxed);
    }

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

        match sink(path, &batch) {
            Ok(()) => {
                counters.written.fetch_add(lines, Ordering::Relaxed);
                warned.store(false, Ordering::Relaxed);
            }
            Err(err) => {
                counters.write_failed.fetch_add(lines, Ordering::Relaxed);
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
        assert_eq!(
            ledger.counters().dropped_write_failed(),
            10,
            "an unopenable path is a broken path, not backpressure"
        );
        assert_eq!(
            ledger.counters().dropped_channel_full(),
            0,
            "the channel never filled: the writer drains it and fails on the write"
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

    /// **The failure table's first row, actually exercised.** A previous
    /// revision named a test for this and measured the unopenable-path arm
    /// instead: `writer_loop` calls `rx.recv()` first, so a receiver is never
    /// parked by a path it cannot open, and the channel never filled once in
    /// 3072 lines. Creating real backpressure needs the writer held *inside* its
    /// write, which is what the injected [`BatchSink`] is for.
    #[test]
    fn a_full_channel_drops_rather_than_blocking_the_caller() {
        let dir = temp_dir("channel-full");
        let path = dir.join("calls.jsonl");

        // The sink parks on first entry and stays parked until released, so the
        // writer holds exactly one batch and drains nothing.
        let entered = Arc::new(AtomicU64::new(0));
        let released = Arc::new((parking_lot::Mutex::new(false), parking_lot::Condvar::new()));
        let ledger = {
            let entered = Arc::clone(&entered);
            let released = Arc::clone(&released);
            Ledger::open_with_sink(
                &path,
                Arc::new(move |p: &Path, batch: &[u8]| {
                    entered.fetch_add(1, Ordering::Relaxed);
                    let (lock, cv) = &*released;
                    let mut open = lock.lock();
                    while !*open {
                        cv.wait(&mut open);
                    }
                    // Released: write for real, so the file is still the file.
                    write_batch(p, batch)
                }),
            )
        };

        // One line to get the writer into the sink, where it parks.
        ledger.append(&call_line("lambo_derive", "a", "ok", None, 1, None));
        assert!(
            until(Duration::from_secs(10), || entered.load(Ordering::Relaxed)
                >= 1),
            "the writer must be inside the sink before backpressure can be built"
        );

        // The channel now takes CHANNEL_CAPACITY lines and refuses the rest.
        let overflow = 64u64;
        let burst = CHANNEL_CAPACITY as u64 + overflow;
        let started = Instant::now();
        for _ in 0..burst {
            ledger.append(&call_line("lambo_derive", "a", "ok", None, 1, None));
        }
        let elapsed = started.elapsed();

        assert!(
            ledger.counters().dropped_channel_full() > 0,
            "a full channel must drop: dropped_channel_full={} of a {burst}-line burst",
            ledger.counters().dropped_channel_full()
        );
        assert_eq!(
            ledger.counters().dropped_channel_full(),
            overflow,
            "exactly the lines past the capacity are dropped as backpressure"
        );
        assert_eq!(
            ledger.counters().dropped_write_failed(),
            0,
            "nothing failed to write — this arm is backpressure, not a broken path"
        );
        // `append` returned for every line while the writer was parked. The
        // budget is deliberately loose: the assertion is "did not block", not a
        // throughput measurement.
        assert!(
            elapsed < Duration::from_secs(5),
            "append must not block on a full channel; {burst} appends took {elapsed:?}"
        );

        // I-R2-3: the queue depth is visible while the writer is parked, and it
        // does NOT collapse just because backpressure is also being counted.
        // `accepted - written - write_failed` = the batch in the sink plus the
        // full channel; subtracting the `dropped` total instead would understate
        // it by exactly `overflow`.
        assert_eq!(
            ledger.counters().queued(),
            1 + CHANNEL_CAPACITY as u64,
            "a parked writer's queue depth is the in-flight batch plus the full \
             channel; written={} channel_full={} write_failed={}",
            ledger.counters().written(),
            ledger.counters().dropped_channel_full(),
            ledger.counters().dropped_write_failed()
        );

        // Release and let it drain, so the shutdown path is the normal one.
        {
            let (lock, cv) = &*released;
            *lock.lock() = true;
            cv.notify_all();
        }
        let want = 1 + CHANNEL_CAPACITY as u64;
        assert!(
            until(Duration::from_secs(20), || ledger.counters().written()
                == want),
            "the accepted lines land once the writer is released: written={} of {want}",
            ledger.counters().written()
        );
        assert_eq!(ledger.counters().queued(), 0, "a drained queue reads zero");
        ledger.shutdown();
        assert_eq!(
            ledger.counters().dropped(),
            overflow,
            "the total is the backpressure drops and nothing else"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// **I-R1-3.** `Ledger::open` must not perform the ledger's first `open`.
    ///
    /// A FIFO with no reader is the cheapest real path whose `open(2)` blocks
    /// forever, and it stands in for the hung mount that motivates the rule:
    /// `serve` calls `Ledger::open` after the single-writer lease is taken, so an
    /// `open` on that path would hold the lease in a process that never serves.
    ///
    /// Since I-R2-1 the SIGTERM handler is armed *before* this call, which
    /// changes what such a process does on a signal without rescuing it: the
    /// handler is installed, so SIGTERM no longer kills it by the default
    /// disposition — but the pinned shutdown future is never polled while the
    /// main task blocks inside the `open`, so `Memory::close` never runs either.
    /// It simply hangs, lease held, until something kills it harder. The rule
    /// stands on its own anyway: a `serve` that hangs before it serves is a
    /// failure however its eventual death arrives, and the guarantee here is
    /// that `Ledger::open` performs no I/O at all.
    ///
    /// Guarded by a timeout rather than asserted structurally, and skipped
    /// rather than failed where `mkfifo` does not exist.
    #[test]
    fn opening_a_ledger_does_not_block_even_when_the_paths_open_blocks() {
        let dir = temp_dir("fifo");
        let path = dir.join("calls.jsonl");
        let made = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !made {
            // No FIFO available (no `mkfifo`, or a filesystem that refuses one).
            // Nothing to assert; do not fail a platform this claim is not about.
            std::fs::remove_dir_all(&dir).ok();
            return;
        }

        let (done_tx, done_rx) = sync_channel::<()>(1);
        let probe = path.clone();
        std::thread::spawn(move || {
            let ledger = Ledger::open(&probe);
            // `open` returned. Appending must also return — the writer is parked
            // in its own `open`, so these are channel-full drops, not blocks.
            for _ in 0..10 {
                ledger.append(&call_line("lambo_stats", "a", "ok", None, 1, None));
            }
            let _ = done_tx.send(());
            // Deliberately leaked: this ledger's writer thread is blocked in
            // `open(2)` on a reader-less FIFO and cannot be joined. It costs one
            // parked thread for the rest of the test binary's life, which is the
            // point — the process still exits.
            std::mem::forget(ledger);
        });

        assert!(
            done_rx.recv_timeout(Duration::from_secs(10)).is_ok(),
            "Ledger::open (and append) must return promptly on a path whose open blocks"
        );
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
        assert_eq!(
            ledger.counters().dropped_write_failed(),
            1,
            "a post-shutdown append had nowhere to go; that is not backpressure"
        );
        assert_eq!(ledger.counters().dropped_channel_full(), 0);
        assert_eq!(ledger.counters().written(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
