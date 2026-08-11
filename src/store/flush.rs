//! Write-behind flush task (T3.4) — spec §2.4–§2.5.
//!
//! A tokio task drains the graph's ordered mutation log ([`Graph::drain_log`])
//! into a [`GraphStore`] every `interval` — or earlier, once a pending batch
//! reaches `max_batch` — with bounded retry and a retained-batch fallback so a
//! store outage never drops mutations:
//!
//! * **Loss bound is observable.** [`FlushTask::stats`] reports `lag` (time
//!   since the last successful flush), `depth` (mutations not yet durable:
//!   the in-graph log plus the pending batch — in flight, backed off, or
//!   retained after exhausted retries) and `dead_lettered` (batches dropped
//!   for a deterministic constraint violation, STORE-4/D5). `lag` uses the
//!   tokio clock so it tracks paused time deterministically in tests.
//! * **Bounded attempts.** Every `store.flush` attempt is bounded by
//!   [`FLUSH_ATTEMPT_TIMEOUT`] (STORE-2): a hung store must not wedge the
//!   loop forever — the timeout maps into the retry path like any `Err`.
//! * **Never dropped (except dead letters).** After `retries` retries the
//!   batch is retained in the task's pending buffer and flushes before newly
//!   drained mutations on the next cycle (drained mutations are appended
//!   *after* whatever is already pending, preserving chronological order —
//!   the mod.rs contract). The in-memory graph is the primary tier (spec
//!   §2.1), so the session keeps accepting writes throughout an outage. A
//!   retained batch that exhausted its retries waits out
//!   [`RETAINED_BACKOFF`] before the next attempt (F3): a permanently failing
//!   store re-enters the retry sequence at most once per hold, not once per
//!   interval tick. The one exception (STORE-4/D5): a batch rejected with a
//!   deterministic constraint violation is logged and dropped — never
//!   retried, never retained — so it cannot poison the queue head.
//! * **Degradation.** If the pending depth exceeds `log_max` the session
//!   degrades to `durability="none"`: the task logs at ERROR, sets
//!   [`FlushTask::degraded`], and stops all store I/O. The mode is terminal
//!   for the task's lifetime. While degraded the task keeps draining the log
//!   but DROPS each drained batch (STORE-3, spec §2.3 "none = pure RAM") —
//!   post-degrade retention must not grow without bound.
//!
//! ## Lock discipline (spec §6.4)
//!
//! The graph WRITE lock is held only to drain the log; the READ lock only for
//! `log_len` / `session_id`. No graph lock is ever held across an `.await`.
//!
//! ## Timing model
//!
//! The task wakes on `interval` ticks (the tick-triggered flush) and, in
//! between, polls every [`POLL_QUANTUM`] so a batch that reaches `max_batch`
//! flushes *early* — before the interval elapses (spec §2.4 "forces an early
//! flush"). The graph is polled, not notified (there is no write channel on
//! [`Graph`]), so [`POLL_QUANTUM`] is the granularity of the early-flush
//! trigger and of the depth observation.
//!
//! There is no shutdown signal (v0.1 keeps it simple): the task runs until the
//! runtime drops it or the returned [`tokio::task::JoinHandle`] is aborted.

use parking_lot::{Mutex, RwLock};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use crate::graph::Graph;
use crate::store::GraphStore;
use crate::types::{MutationBatch, StoreError};

/// Poll cadence for the `max_batch` early-flush trigger and for keeping
/// `depth` fresh between interval ticks (see module docs).
const POLL_QUANTUM: Duration = Duration::from_millis(100);
/// Per-attempt bound on a single `store.flush` call (STORE-2): a hung store
/// must not wedge the flush loop forever. The timeout maps into the existing
/// retry path (same as any `Err`), so the loop still retries → retains →
/// degrades as designed. Mirrors the F2 `LOAD_SESSION_TIMEOUT` (load.rs) —
/// same 30s value, same naming convention.
const FLUSH_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(30);
/// Backoff base for flush retries; doubles per retry.
const BACKOFF_BASE: Duration = Duration::from_millis(100);
/// Cap on a single backoff sleep (retries are bounded by `FlushParams::retries`
/// anyway; the cap keeps a large `retries` config from sleeping for minutes).
const BACKOFF_CAP: Duration = Duration::from_secs(10);
/// Post-retry hold on a retained batch (F3): once `retries` are exhausted the
/// batch waits out this long before the **next** attempt re-enters the retry
/// sequence. Without it a permanently failing store would re-run the whole
/// retry sequence (attempts + warn) on every interval tick. Equal to the
/// per-attempt backoff cap; a documented multiple of the interval would work
/// too — the contract is "at least this long between exhausted-retry
/// sequences", not the exact value.
const RETAINED_BACKOFF: Duration = BACKOFF_CAP;

/// Flush tuning. Callers pass `Config::backend_flush_*` values; this task has
/// no `Config` dependency.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlushParams {
    pub interval: Duration,
    pub max_batch: usize,
    pub retries: u32,
    pub log_max: usize,
}

/// Observable durability loss bound (spec §2.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlushStats {
    /// Time since the last successful flush (tokio clock; 0 until the first
    /// successful flush after spawn).
    pub lag: Duration,
    /// Mutations not yet durable: in-graph log + pending batch (in flight,
    /// backed off, or retained after exhausted retries).
    pub depth: usize,
    /// Batches dropped as dead letters (STORE-4 / D5): a deterministic
    /// constraint violation is logged and dropped — never retried, never
    /// retained — so it cannot poison the queue head. Visible in stats by
    /// design ("drop-after-log, visible in stats").
    pub dead_lettered: u64,
}

/// Lock-light state shared between the running task and the caller's stats
/// handle. `last_success` is a `Mutex<Instant>` (held for nanoseconds inside
/// `stats`); `started`/`depth`/`degraded`/`dead_lettered` are atomics.
#[derive(Debug)]
struct Shared {
    /// Set by `spawn` (check-and-set): exactly one flush loop may run per task.
    /// Visible to `stats`/`degraded` without races.
    started: AtomicBool,
    last_success: Mutex<tokio::time::Instant>,
    depth: AtomicUsize,
    degraded: AtomicBool,
    /// STORE-4 / D5: dead-lettered-batch counter (deterministic constraint
    /// violations dropped after logging). Monotonic for the task's lifetime.
    dead_lettered: AtomicU64,
}

impl Shared {
    fn new() -> Self {
        Self {
            // `last_success` is a placeholder until `spawn` initializes it (the
            // lag contract starts at spawn, not construction — see `spawn`).
            started: AtomicBool::new(false),
            last_success: Mutex::new(tokio::time::Instant::now()),
            depth: AtomicUsize::new(0),
            degraded: AtomicBool::new(false),
            dead_lettered: AtomicU64::new(0),
        }
    }

    fn stats(&self) -> FlushStats {
        let now = tokio::time::Instant::now();
        FlushStats {
            lag: now.saturating_duration_since(*self.last_success.lock()),
            depth: self.depth.load(Ordering::Acquire),
            dead_lettered: self.dead_lettered.load(Ordering::Acquire),
        }
    }
}

/// Write-behind flush task (spec §2.4–§2.5).
///
/// Spawn with [`FlushTask::spawn`]; the returned handle is the only way to stop
/// it (abort). [`FlushTask::stats`] / [`FlushTask::degraded`] are callable from
/// any thread while the task runs.
pub struct FlushTask {
    graph: Arc<RwLock<Graph>>,
    store: Arc<dyn GraphStore>,
    params: FlushParams,
    shared: Arc<Shared>,
}

impl FlushTask {
    /// `params` typically come from `Config::backend_flush_*`.
    pub fn new(graph: Arc<RwLock<Graph>>, store: Arc<dyn GraphStore>, params: FlushParams) -> Self {
        Self {
            graph,
            store,
            params,
            shared: Arc::new(Shared::new()),
        }
    }

    /// Spawn the interval loop and return its handle.
    ///
    /// Call `spawn` **exactly once** per `FlushTask` — a second call panics.
    /// Exactly one loop may run: two concurrent loops would each carry an
    /// independent `pending` buffer and could persist batches out of order
    /// (drain serialization alone does not order flushes).
    ///
    /// Takes `&self` rather than the pinned `self`: the task clones the
    /// graph/store/shared arcs, so the caller keeps this `FlushTask` as its
    /// stats handle — `spawn(self)` would consume the only path to
    /// [`FlushTask::stats`]. `last_success` is initialized here (not at
    /// construction), so `stats().lag` is 0 until the first successful flush
    /// after spawn even if the task was built long before it was spawned. The
    /// first flush happens one `interval` after spawn.
    pub fn spawn(&self) -> tokio::task::JoinHandle<()> {
        // Single-loop enforcement: check-and-set the shared `started` flag so a
        // second `spawn` panics before it can start another loop. The flag
        // lives in `Shared` (not task-local) so `stats`/`degraded` observe it
        // race-free.
        self.shared
            .started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .expect("FlushTask::spawn called twice — exactly one loop may run");
        *self.shared.last_success.lock() = tokio::time::Instant::now();

        let graph = self.graph.clone();
        let store = self.store.clone();
        let shared = self.shared.clone();
        let params = self.params; // Copy — do not capture `self` into the 'static task
        tokio::spawn(async move {
            FlushLoop {
                graph,
                store,
                params,
                shared,
                pending: MutationBatch::default(),
                retry_after: None,
            }
            .run()
            .await
        })
    }

    /// Current observable durability loss bound (spec §2.4: the bound is
    /// observable, not assumed).
    pub fn stats(&self) -> FlushStats {
        self.shared.stats()
    }

    /// `true` once the pending depth exceeded `log_max`: flushing has stopped
    /// (`durability="none"`). Terminal for the task's lifetime.
    pub fn degraded(&self) -> bool {
        self.shared.degraded.load(Ordering::Acquire)
    }
}

/// Polls `F` inside [`std::panic::catch_unwind`], turning a panic during any
/// poll into `Err(payload)`. std-only: used to keep a panicking store backend
/// from aborting the flush task (which would stop flushing permanently with
/// `degraded()==false` — silent durability loss). Dropping the future after a
/// caught panic is safe: the backend only ever holds a `&MutationBatch`, so the
/// loop's `pending` buffer cannot have been corrupted.
struct CatchUnwindPoll<F>(F);

impl<F: Future> Future for CatchUnwindPoll<F> {
    type Output = Result<F::Output, Box<dyn std::any::Any + Send>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: `self` is a pinned `CatchUnwindPoll` and `F` is its only
        // field, so `F` stays in place for as long as the outer pin does. The
        // closure only borrows the field for the poll — it never moves or
        // replaces it — so re-projecting the pin onto `F` is sound.
        let fut = unsafe { self.map_unchecked_mut(|s| &mut s.0) };
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fut.poll(cx))) {
            Ok(poll) => poll.map(Ok),
            Err(payload) => Poll::Ready(Err(payload)),
        }
    }
}

/// Best-effort human-readable message for a panic payload (for logging).
///
/// Takes `&Box<dyn Any + Send>` rather than `&(dyn Any + Send)`: a plain
/// `&boxed` coerces via the reflexive `Unsize` rule and would present the Box
/// itself (not its payload) as the trait object, breaking every downcast.
/// `Box::as_ref` is the explicit deref that reaches the payload.
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    let payload: &(dyn std::any::Any + Send) = payload.as_ref();
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        format!(
            "non-string panic payload ({})",
            std::any::type_name_of_val(payload)
        )
    }
}

/// Task-owned loop state (moved into the spawned task).
struct FlushLoop {
    graph: Arc<RwLock<Graph>>,
    store: Arc<dyn GraphStore>,
    params: FlushParams,
    shared: Arc<Shared>,
    /// Mutations not yet durable. Retained batches stay at the front; newly
    /// drained mutations are appended in chronological order — never re-sorted
    /// (mod.rs contract).
    pending: MutationBatch,
    /// Earliest time (tokio clock) the next flush attempt may run after a
    /// retained batch exhausted its retries (F3, `RETAINED_BACKOFF`). `None`
    /// when no batch is in the post-retry hold.
    retry_after: Option<tokio::time::Instant>,
}

impl FlushLoop {
    async fn run(mut self) {
        let mut interval = tokio::time::interval(self.params.interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // A fresh interval's first tick fires immediately; consume it so the
        // first real flush happens one interval after spawn.
        interval.tick().await;

        loop {
            tokio::select! {
                _ = interval.tick() => self.cycle(true).await,
                // Early-flush poll: catch a batch reaching max_batch before the tick.
                _ = tokio::time::sleep(POLL_QUANTUM) => self.cycle(false).await,
            }
        }
    }

    /// One flush cycle: drain the graph log, then flush the pending batch when
    /// this is an interval tick (`tick`) or the batch reached `max_batch`
    /// (whichever first).
    async fn cycle(&mut self, tick: bool) {
        let session = self.graph.read().session_id().clone();
        {
            // WRITE lock only for the drain; the guard dies before any I/O.
            let mut graph = self.graph.write();
            let drained = graph.drain_log();
            self.pending.mutations.extend(drained.mutations);
        }

        self.refresh_depth();
        if self.pending.is_empty() {
            return;
        }
        if self.shared.degraded.load(Ordering::Acquire) {
            // durability="none" (spec §2.3 — "none = pure RAM"): the graph is
            // the only tier, so no further store I/O. STORE-3: DROP the
            // drained batch instead of retaining it — post-degrade retention
            // used to grow without bound for the session's remaining life.
            // Depth is the in-graph log only.
            self.pending.mutations.clear();
            self.refresh_depth();
            return;
        }
        let depth = self.shared.depth.load(Ordering::Acquire);
        if depth > self.params.log_max {
            self.shared.degraded.store(true, Ordering::Release);
            tracing::error!(
                depth,
                log_max = self.params.log_max,
                session = %session,
                "FlushDegraded: mutation backlog {depth} exceeds backend_log_max {}; \
                 durability=\"none\", flushing stopped",
                self.params.log_max,
            );
            return;
        }

        // Post-retry hold (F3): a batch that exhausted its retries waits out
        // RETAINED_BACKOFF before the next attempt, so a permanently failing
        // store does not re-run the whole retry sequence on every interval
        // tick. New drains keep appending while we wait; the batch flushes
        // whole (order preserved) once the hold elapses.
        if let Some(deadline) = self.retry_after {
            if tokio::time::Instant::now() < deadline {
                return;
            }
            self.retry_after = None;
        }

        if !tick && self.pending.len() < self.params.max_batch {
            return; // early-flush poll: batch not full yet, wait for the tick
        }

        match self.flush_with_retry().await {
            Ok(()) => {
                self.pending.mutations.clear();
                self.retry_after = None;
                *self.shared.last_success.lock() = tokio::time::Instant::now();
                // Writes may have landed while we flushed; depth is the log only now.
                self.refresh_depth();
            }
            Err(StoreError::Constraint(code)) => {
                // STORE-4 / D5: a deterministic constraint violation can
                // never succeed on replay — retrying it would only poison the
                // queue head (head-of-line blocking). Dead-letter the WHOLE
                // batch: warn (constraint + batch summary), drop, count in
                // stats, session continues, never degrade for a dead-lettered
                // batch.
                let batch_len = self.pending.len();
                self.pending.mutations.clear();
                self.retry_after = None;
                self.shared.dead_lettered.fetch_add(1, Ordering::AcqRel);
                self.refresh_depth();
                tracing::warn!(
                    constraint = %code,
                    batch_len,
                    depth = self.shared.depth.load(Ordering::Acquire),
                    session = %session,
                    "FlushDeadLettered: deterministic constraint violation; batch of {batch_len} \
                     mutations dropped (dead-letter D5, drop-after-log), session continues",
                );
            }
            Err(err) => {
                // Retries exhausted: RETAIN the batch — it is still in
                // `self.pending`, never dropped, and flushes before new drains
                // on the next cycle. The session keeps working (spec §2.4).
                // F3: hold the next attempt until the post-retry backoff
                // elapses (see `cycle`'s gate above).
                self.retry_after = Some(tokio::time::Instant::now() + RETAINED_BACKOFF);
                self.refresh_depth();
                tracing::warn!(
                    error = %err,
                    depth = self.shared.depth.load(Ordering::Acquire),
                    session = %session,
                    "BackendFlushFailed: store flush failed after {} retries; batch retained \
                     (never dropped), next attempt in {:?}, session keeps working, \
                     durability is best-effort",
                    self.params.retries,
                    RETAINED_BACKOFF,
                );
            }
        }
    }

    /// Attempt `store.flush` with exponential backoff, up to `retries` retries
    /// after the initial attempt (total attempts = `retries` + 1). Every
    /// attempt is bounded by [`FLUSH_ATTEMPT_TIMEOUT`] (STORE-2): a hung
    /// store must not wedge the loop forever — the timeout maps into this
    /// retry path exactly like any other `Err` (retry → retain → degrade).
    /// A [`StoreError::Constraint`] (STORE-4) is deterministic and surfaces
    /// immediately with NO backoff: the caller dead-letters the batch.
    async fn flush_with_retry(&mut self) -> Result<(), StoreError> {
        let mut retries_used: u32 = 0;
        let mut backoff = BACKOFF_BASE;
        loop {
            // A panicking backend must not abort the spawned loop: the task
            // would die silently, stopping all flush bookkeeping with
            // `degraded()==false` and no events — durability loss worse than
            // the designed failure path. Poll the flush inside `catch_unwind`
            // and route a panic into the typed-error path below (same backoff
            // → retain/degrade handling), logging the payload.
            let attempt = async {
                match CatchUnwindPoll(async { self.store.flush(&self.pending).await }).await {
                    Ok(result) => result,
                    Err(payload) => {
                        let message = panic_message(&payload);
                        tracing::warn!(
                            panic = %message,
                            attempt = retries_used + 1,
                            max_retries = self.params.retries,
                            "BackendFlushPanic: store.flush panicked; treating as a failed flush \
                             attempt (backoff, then retain/degrade as usual)"
                        );
                        Err(StoreError::Backend(format!(
                            "store flush panicked: {message}"
                        )))
                    }
                }
            };
            // STORE-2: bound the whole attempt (panic containment included).
            // On timeout the in-flight future is dropped; the backend only
            // borrows `&self.pending`, so the pending buffer is intact.
            let result = match tokio::time::timeout(FLUSH_ATTEMPT_TIMEOUT, attempt).await {
                Ok(result) => result,
                Err(_elapsed) => Err(StoreError::Backend(format!(
                    "store flush timed out after {FLUSH_ATTEMPT_TIMEOUT:?}"
                ))),
            };
            match result {
                Ok(()) => return Ok(()),
                Err(err) => {
                    if !err.is_retryable() {
                        // STORE-4: deterministic constraint violation — no
                        // backoff, no retry; the caller dead-letters it.
                        return Err(err);
                    }
                    if retries_used >= self.params.retries {
                        return Err(err);
                    }
                    retries_used += 1;
                    tracing::debug!(
                        error = %err,
                        attempt = retries_used,
                        max_retries = self.params.retries,
                        backoff_ms = backoff.as_millis(),
                        "store flush failed; retrying with exponential backoff"
                    );
                    tokio::time::sleep(backoff).await;
                    // Writes that landed during the backoff are now part of depth.
                    self.refresh_depth();
                    backoff = (backoff * 2).min(BACKOFF_CAP);
                }
            }
        }
    }

    /// depth = pending batch + in-graph log (everything not yet durable).
    fn refresh_depth(&self) {
        let depth = self.pending.len() + self.graph.read().log_len();
        self.shared.depth.store(depth, Ordering::Release);
    }
}

#[cfg(all(test, feature = "store-memory"))]
mod tests {
    use super::*;
    use crate::store::memory::MemoryStore;
    use crate::store::Capabilities;
    use crate::types::{
        AgentId, CanonizationEvent, CanonizationStatus, Concept, ConceptType, GraphSnapshot,
        Interaction, InteractionSpan, NodeId, Scored, SessionId,
    };
    use async_trait::async_trait;
    use chrono::{DateTime, TimeZone, Utc};
    use std::future::Future;
    use std::io;
    use tracing_subscriber::fmt::MakeWriter;
    use uuid::Uuid;

    fn ts(minutes: i64) -> DateTime<Utc> {
        let base = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
        base + chrono::Duration::minutes(minutes)
    }

    fn sid() -> SessionId {
        SessionId::from("flush-test-session")
    }

    fn interaction(id: u64, prev: Option<NodeId>, at_min: i64) -> Interaction {
        Interaction {
            id: NodeId(Uuid::from_u64_pair(1, id)),
            session_id: sid(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: Some(format!("prompt {id}")),
            previous_id: prev,
            created_at: ts(at_min),
        }
    }

    fn concept(id: u64, origin: NodeId, content: &str) -> Concept {
        Concept {
            id: NodeId(Uuid::from_u64_pair(2, id)),
            session_id: sid(),
            content: content.into(),
            canonical_key: content.into(),
            concept_type: ConceptType::Entity,
            origin_interaction: origin,
            origin_agent: AgentId::from("agent-a"),
            created_at: ts(0),
            access_count: 0,
            last_accessed: None,
            gc_survived: 0,
            canonization_status: CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: None,
            embedding: None,
            chunk_group_id: None,
        }
    }

    fn new_graph() -> Arc<RwLock<Graph>> {
        Arc::new(RwLock::new(Graph::new(sid())))
    }

    /// First interaction: 1 mutation. Every concept: 2 mutations
    /// (UpsertNode + Derives UpsertEdge).
    fn add_interaction(g: &Arc<RwLock<Graph>>, id: u64, prev: Option<NodeId>) -> NodeId {
        let i = interaction(id, prev, 0);
        let nid = i.id;
        g.write().insert_interaction(i).unwrap();
        nid
    }

    fn add_concept(g: &Arc<RwLock<Graph>>, id: u64, origin: NodeId) -> NodeId {
        let c = concept(id, origin, &format!("concept {id}"));
        let cid = c.id;
        g.write().insert_concept(c, origin).unwrap();
        cid
    }

    /// Yield until the task has been polled past its spawn-time first tick so
    /// its interval/backoff timers are armed at the paused-clock origin.
    async fn let_task_arm() {
        tokio::task::yield_now().await;
    }

    /// Poll the runtime until `cond` (with a bounded number of yields).
    ///
    /// The condition must be **initially false** — `tokio::time::advance` fires
    /// timers but only runs the woken task once the test yields, so a condition
    /// that is already true in the pre-poll state would return before the flush
    /// task has done its work.
    async fn wait_until(mut cond: impl FnMut() -> bool) {
        for _ in 0..1_000 {
            if cond() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("condition not met within 1000 yields");
    }

    /// Like [`wait_until`] for async conditions (e.g. store contents).
    async fn wait_until_async<F, Fut>(mut cond: F)
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = bool>,
    {
        for _ in 0..1_000 {
            if cond().await {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("condition not met within 1000 yields");
    }

    fn params(interval: Duration, max_batch: usize, retries: u32, log_max: usize) -> FlushParams {
        FlushParams {
            interval,
            max_batch,
            retries,
            log_max,
        }
    }

    /// Install a thread-local default that registers tracing callsites as
    /// `always`-interested while dropping every event.
    ///
    /// `tracing` caches each callsite's `Interest` process-wide at first
    /// registration; with no default subscriber that interest is `never` and
    /// the shared `BackendFlushFailed` warn callsite (in `cycle`) becomes
    /// permanently disabled for *every* test — including
    /// [`degrades_past_log_max_and_stops_flushing`], which asserts that event
    /// through a capturing subscriber. Any flush test that can reach that warn
    /// without its own subscriber must install this guard so the callsite can
    /// never be poisoned. `TRACE` keeps the filter from returning `never`; the
    /// sink writer keeps the events silent.
    fn keep_callsites_enabled() -> tracing::subscriber::DefaultGuard {
        tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::TRACE)
                .with_writer(std::io::sink)
                .finish(),
        )
    }

    /// `GraphStore` mock: fails the first `fail_next(n)` flush calls (or
    /// `fail_forever`), then delegates to an inner store. Records flush-call
    /// count and per-call batch sizes for assertions.
    struct FlakyStore {
        inner: Arc<dyn GraphStore>,
        flush_calls: AtomicUsize,
        fail_remaining: AtomicUsize,
        fail_always: AtomicBool,
        batch_sizes: Mutex<Vec<usize>>,
    }

    impl FlakyStore {
        fn new(inner: Arc<dyn GraphStore>) -> Self {
            Self {
                inner,
                flush_calls: AtomicUsize::new(0),
                fail_remaining: AtomicUsize::new(0),
                fail_always: AtomicBool::new(false),
                batch_sizes: Mutex::new(Vec::new()),
            }
        }

        fn fail_next(&self, n: usize) {
            self.fail_remaining.store(n, Ordering::SeqCst);
        }

        fn fail_forever(&self) {
            self.fail_always.store(true, Ordering::SeqCst);
        }

        fn flush_calls(&self) -> usize {
            self.flush_calls.load(Ordering::SeqCst)
        }

        fn batch_sizes(&self) -> Vec<usize> {
            self.batch_sizes.lock().clone()
        }

        fn should_fail(&self) -> bool {
            if self.fail_always.load(Ordering::SeqCst) {
                return true;
            }
            self.fail_remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                    (n > 0).then(|| n - 1)
                })
                .is_ok()
        }
    }

    #[async_trait]
    impl GraphStore for FlakyStore {
        async fn init_schema(&self) -> Result<(), StoreError> {
            self.inner.init_schema().await
        }

        fn capabilities(&self) -> Capabilities {
            self.inner.capabilities()
        }

        async fn flush(&self, batch: &MutationBatch) -> Result<(), StoreError> {
            self.flush_calls.fetch_add(1, Ordering::SeqCst);
            self.batch_sizes.lock().push(batch.len());
            if self.should_fail() {
                Err(StoreError::Backend(
                    "simulated flush failure (FlakyStore)".into(),
                ))
            } else {
                self.inner.flush(batch).await
            }
        }

        async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
            self.inner.load_session(session).await
        }

        async fn keyword_candidates(
            &self,
            session: &SessionId,
            tokens: &[String],
            limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.inner.keyword_candidates(session, tokens, limit).await
        }

        async fn vector_candidates(
            &self,
            session: &SessionId,
            embedding: &[f32],
            limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.inner
                .vector_candidates(session, embedding, limit)
                .await
        }

        async fn blast_radius(
            &self,
            session: &SessionId,
            node: NodeId,
            min_edge_age: Duration,
        ) -> Result<u64, StoreError> {
            self.inner.blast_radius(session, node, min_edge_age).await
        }

        async fn interaction_span(
            &self,
            session: &SessionId,
            node: NodeId,
            min_age: Duration,
        ) -> Result<InteractionSpan, StoreError> {
            self.inner.interaction_span(session, node, min_age).await
        }

        async fn record_canonization(&self, event: &CanonizationEvent) -> Result<(), StoreError> {
            self.inner.record_canonization(event).await
        }
    }

    /// `GraphStore` mock: PANICS (does not return `Err`) on the first
    /// `panic_next(n)` flush calls (or `panic_forever`), then delegates to an
    /// inner store. Mirrors [`FlakyStore`]'s call/batch bookkeeping but
    /// exercises the `catch_unwind` containment path: a raw panic must not
    /// abort the spawned flush loop.
    struct PanicStore {
        inner: Arc<dyn GraphStore>,
        flush_calls: AtomicUsize,
        panic_remaining: AtomicUsize,
        panic_always: AtomicBool,
        batch_sizes: Mutex<Vec<usize>>,
    }

    impl PanicStore {
        fn new(inner: Arc<dyn GraphStore>) -> Self {
            Self {
                inner,
                flush_calls: AtomicUsize::new(0),
                panic_remaining: AtomicUsize::new(0),
                panic_always: AtomicBool::new(false),
                batch_sizes: Mutex::new(Vec::new()),
            }
        }

        fn panic_next(&self, n: usize) {
            self.panic_remaining.store(n, Ordering::SeqCst);
        }

        fn panic_forever(&self) {
            self.panic_always.store(true, Ordering::SeqCst);
        }

        fn flush_calls(&self) -> usize {
            self.flush_calls.load(Ordering::SeqCst)
        }

        fn batch_sizes(&self) -> Vec<usize> {
            self.batch_sizes.lock().clone()
        }

        fn should_panic(&self) -> bool {
            if self.panic_always.load(Ordering::SeqCst) {
                return true;
            }
            self.panic_remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                    (n > 0).then(|| n - 1)
                })
                .is_ok()
        }
    }

    #[async_trait]
    impl GraphStore for PanicStore {
        async fn init_schema(&self) -> Result<(), StoreError> {
            self.inner.init_schema().await
        }

        fn capabilities(&self) -> Capabilities {
            self.inner.capabilities()
        }

        async fn flush(&self, batch: &MutationBatch) -> Result<(), StoreError> {
            self.flush_calls.fetch_add(1, Ordering::SeqCst);
            self.batch_sizes.lock().push(batch.len());
            if self.should_panic() {
                panic!("simulated flush panic (PanicStore)");
            } else {
                self.inner.flush(batch).await
            }
        }

        async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
            self.inner.load_session(session).await
        }

        async fn keyword_candidates(
            &self,
            session: &SessionId,
            tokens: &[String],
            limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.inner.keyword_candidates(session, tokens, limit).await
        }

        async fn vector_candidates(
            &self,
            session: &SessionId,
            embedding: &[f32],
            limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.inner
                .vector_candidates(session, embedding, limit)
                .await
        }

        async fn blast_radius(
            &self,
            session: &SessionId,
            node: NodeId,
            min_edge_age: Duration,
        ) -> Result<u64, StoreError> {
            self.inner.blast_radius(session, node, min_edge_age).await
        }

        async fn interaction_span(
            &self,
            session: &SessionId,
            node: NodeId,
            min_age: Duration,
        ) -> Result<InteractionSpan, StoreError> {
            self.inner.interaction_span(session, node, min_age).await
        }

        async fn record_canonization(&self, event: &CanonizationEvent) -> Result<(), StoreError> {
            self.inner.record_canonization(event).await
        }
    }

    /// `GraphStore` mock: its flush HANGS (never resolves) for the first
    /// `hang_next(n)` calls, then delegates to an inner store. STORE-2
    /// harness — mirrors the F2 `HangingStore` (load.rs) for the flush path:
    /// a hung flush must be bounded by [`FLUSH_ATTEMPT_TIMEOUT`], never wedge
    /// the loop.
    struct HungStore {
        inner: Arc<dyn GraphStore>,
        flush_calls: AtomicUsize,
        hang_remaining: AtomicUsize,
        hang_always: AtomicBool,
        batch_sizes: Mutex<Vec<usize>>,
    }

    impl HungStore {
        fn new(inner: Arc<dyn GraphStore>) -> Self {
            Self {
                inner,
                flush_calls: AtomicUsize::new(0),
                hang_remaining: AtomicUsize::new(0),
                hang_always: AtomicBool::new(false),
                batch_sizes: Mutex::new(Vec::new()),
            }
        }

        fn hang_next(&self, n: usize) {
            self.hang_remaining.store(n, Ordering::SeqCst);
        }

        fn flush_calls(&self) -> usize {
            self.flush_calls.load(Ordering::SeqCst)
        }

        fn batch_sizes(&self) -> Vec<usize> {
            self.batch_sizes.lock().clone()
        }

        fn should_hang(&self) -> bool {
            if self.hang_always.load(Ordering::SeqCst) {
                return true;
            }
            self.hang_remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                    (n > 0).then(|| n - 1)
                })
                .is_ok()
        }
    }

    #[async_trait]
    impl GraphStore for HungStore {
        async fn init_schema(&self) -> Result<(), StoreError> {
            self.inner.init_schema().await
        }

        fn capabilities(&self) -> Capabilities {
            self.inner.capabilities()
        }

        async fn flush(&self, batch: &MutationBatch) -> Result<(), StoreError> {
            self.flush_calls.fetch_add(1, Ordering::SeqCst);
            self.batch_sizes.lock().push(batch.len());
            if self.should_hang() {
                // Never resolve — the flush-loop attempt timeout must bound
                // this (STORE-2); the pending future is dropped on timeout.
                std::future::pending().await
            } else {
                self.inner.flush(batch).await
            }
        }

        async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
            self.inner.load_session(session).await
        }

        async fn keyword_candidates(
            &self,
            session: &SessionId,
            tokens: &[String],
            limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.inner.keyword_candidates(session, tokens, limit).await
        }

        async fn vector_candidates(
            &self,
            session: &SessionId,
            embedding: &[f32],
            limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.inner
                .vector_candidates(session, embedding, limit)
                .await
        }

        async fn blast_radius(
            &self,
            session: &SessionId,
            node: NodeId,
            min_edge_age: Duration,
        ) -> Result<u64, StoreError> {
            self.inner.blast_radius(session, node, min_edge_age).await
        }

        async fn interaction_span(
            &self,
            session: &SessionId,
            node: NodeId,
            min_age: Duration,
        ) -> Result<InteractionSpan, StoreError> {
            self.inner.interaction_span(session, node, min_age).await
        }

        async fn record_canonization(&self, event: &CanonizationEvent) -> Result<(), StoreError> {
            self.inner.record_canonization(event).await
        }
    }

    /// `GraphStore` mock: flush ALWAYS fails with a deterministic constraint
    /// violation (STORE-4). Records call/batch bookkeeping like
    /// [`FlakyStore`].
    struct ConstraintStore {
        inner: Arc<dyn GraphStore>,
        flush_calls: AtomicUsize,
        batch_sizes: Mutex<Vec<usize>>,
    }

    impl ConstraintStore {
        fn new(inner: Arc<dyn GraphStore>) -> Self {
            Self {
                inner,
                flush_calls: AtomicUsize::new(0),
                batch_sizes: Mutex::new(Vec::new()),
            }
        }

        fn flush_calls(&self) -> usize {
            self.flush_calls.load(Ordering::SeqCst)
        }
    }
    #[async_trait]
    impl GraphStore for ConstraintStore {
        async fn init_schema(&self) -> Result<(), StoreError> {
            self.inner.init_schema().await
        }

        fn capabilities(&self) -> Capabilities {
            self.inner.capabilities()
        }

        async fn flush(&self, batch: &MutationBatch) -> Result<(), StoreError> {
            self.flush_calls.fetch_add(1, Ordering::SeqCst);
            self.batch_sizes.lock().push(batch.len());
            Err(StoreError::Constraint("23505".into()))
        }

        async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
            self.inner.load_session(session).await
        }

        async fn keyword_candidates(
            &self,
            session: &SessionId,
            tokens: &[String],
            limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.inner.keyword_candidates(session, tokens, limit).await
        }

        async fn vector_candidates(
            &self,
            session: &SessionId,
            embedding: &[f32],
            limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.inner
                .vector_candidates(session, embedding, limit)
                .await
        }

        async fn blast_radius(
            &self,
            session: &SessionId,
            node: NodeId,
            min_edge_age: Duration,
        ) -> Result<u64, StoreError> {
            self.inner.blast_radius(session, node, min_edge_age).await
        }

        async fn interaction_span(
            &self,
            session: &SessionId,
            node: NodeId,
            min_age: Duration,
        ) -> Result<InteractionSpan, StoreError> {
            self.inner.interaction_span(session, node, min_age).await
        }

        async fn record_canonization(&self, event: &CanonizationEvent) -> Result<(), StoreError> {
            self.inner.record_canonization(event).await
        }
    }

    /// Capturing writer for asserting on emitted tracing events.
    #[derive(Clone)]
    struct BufWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for BufWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for BufWriter {
        type Writer = BufWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn flushes_on_interval_tick() {
        let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let graph = new_graph();
        let task = FlushTask::new(
            graph.clone(),
            store.clone(),
            params(Duration::from_secs(1), 100, 3, 1_000),
        );
        let _handle = task.spawn();
        let_task_arm().await;

        assert_eq!(task.stats().depth, 0);

        let iid = add_interaction(&graph, 1, None);
        add_concept(&graph, 1, iid); // 3 mutations total
        assert_eq!(graph.read().log_len(), 3);

        // First poll drains the log into the pending batch (still below max_batch).
        tokio::time::advance(Duration::from_millis(100)).await;
        wait_until(|| graph.read().log_len() == 0).await;
        assert_eq!(task.stats().depth, 3);
        assert!(
            store.load_session(&sid()).await.is_err(),
            "nothing flushed yet"
        );

        // Interval tick delivers the batch.
        tokio::time::advance(Duration::from_millis(900)).await;
        wait_until(|| task.stats().depth == 0).await;

        let snap = store.load_session(&sid()).await.unwrap();
        assert_eq!(snap.interactions.len(), 1);
        assert_eq!(snap.concepts.len(), 1);
        assert_eq!(snap.edges.len(), 1); // Derives edge
        assert_eq!(task.stats().depth, 0);
        assert!(
            task.stats().lag < Duration::from_millis(50),
            "lag reset after success"
        );
        assert!(!task.degraded());
    }

    #[tokio::test(start_paused = true)]
    async fn max_batch_forces_early_flush() {
        let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let graph = new_graph();
        let task = FlushTask::new(
            graph.clone(),
            store.clone(),
            params(Duration::from_secs(1), 3, 0, 1_000),
        );
        let _handle = task.spawn();
        let_task_arm().await;

        // 3 mutations == max_batch; flushed at the first poll, well before the 1s tick.
        let iid = add_interaction(&graph, 1, None);
        add_concept(&graph, 1, iid);
        tokio::time::advance(Duration::from_millis(100)).await;
        wait_until_async(|| async { store.load_session(&sid()).await.is_ok() }).await;

        let snap = store.load_session(&sid()).await.unwrap();
        assert_eq!(snap.interactions.len(), 1);
        assert_eq!(snap.concepts.len(), 1);
        assert_eq!(task.stats().depth, 0);

        // A burst larger than max_batch is delivered whole — max_batch is a
        // trigger, not a cap.
        let _iid2 = add_interaction(&graph, 2, Some(iid)); // UpsertNode + Temporal edge
        add_concept(&graph, 2, _iid2); // UpsertNode + Derives edge
        assert_eq!(graph.read().log_len(), 4);
        tokio::time::advance(Duration::from_millis(100)).await;
        wait_until_async(|| async {
            store.load_session(&sid()).await.is_ok()
                && store.load_session(&sid()).await.unwrap().interactions.len() == 2
        })
        .await;

        let snap = store.load_session(&sid()).await.unwrap();
        assert_eq!(snap.interactions.len(), 2);
        assert_eq!(snap.concepts.len(), 2);
        assert_eq!(snap.edges.len(), 3); // Derives x2 + Temporal
    }

    #[tokio::test(start_paused = true)]
    async fn failing_then_recovering_store_keeps_session_alive() {
        let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let store = Arc::new(FlakyStore::new(inner));
        let graph = new_graph();
        let task = FlushTask::new(
            graph.clone(),
            store.clone(),
            params(Duration::from_secs(1), 100, 3, 1_000),
        );
        let _handle = task.spawn();
        let_task_arm().await;

        store.fail_next(3);
        let iid = add_interaction(&graph, 1, None);
        add_concept(&graph, 1, iid); // 3 mutations

        // Tick: first attempt fails; backoff 100ms begins.
        tokio::time::advance(Duration::from_secs(1)).await;
        wait_until(|| store.flush_calls() >= 1).await;
        assert_eq!(store.flush_calls(), 1);
        assert_eq!(task.stats().depth, 3);
        assert!(
            store.load_session(&sid()).await.is_err(),
            "nothing landed yet"
        );

        // Session uninterrupted: the graph accepts writes while the store is down.
        let _iid2 = add_interaction(&graph, 2, Some(iid));
        assert_eq!(graph.read().log_len(), 2);
        assert!(!task.degraded());

        // Backoff retries: 100ms, 200ms, 400ms; attempts 2 and 3 fail, attempt 4 lands.
        tokio::time::advance(Duration::from_millis(100)).await;
        wait_until(|| store.flush_calls() >= 2).await;
        assert_eq!(store.flush_calls(), 2);
        // depth = retained batch (3) + writes during the outage (2).
        assert_eq!(task.stats().depth, 5);

        tokio::time::advance(Duration::from_millis(200)).await;
        wait_until(|| store.flush_calls() >= 3).await;
        assert_eq!(store.flush_calls(), 3);

        tokio::time::advance(Duration::from_millis(400)).await;
        wait_until(|| store.flush_calls() >= 4).await;
        assert_eq!(store.flush_calls(), 4);

        // Original batch landed; only the outage-period writes remain pending.
        let snap = store.load_session(&sid()).await.unwrap();
        assert_eq!(snap.interactions.len(), 1);
        assert_eq!(snap.concepts.len(), 1);
        assert_eq!(task.stats().depth, 2);
        assert!(
            task.stats().lag < Duration::from_millis(50),
            "lag reset on success"
        );

        // Next tick catches the session up completely.
        tokio::time::advance(Duration::from_secs(1)).await;
        wait_until(|| task.stats().depth == 0).await;
        let snap = store.load_session(&sid()).await.unwrap();
        assert_eq!(snap.interactions.len(), 2);
        assert_eq!(snap.concepts.len(), 1);
        assert_eq!(snap.edges.len(), 2); // Derives + Temporal
        assert_eq!(store.batch_sizes(), vec![3, 3, 3, 3, 2]);
    }

    #[tokio::test(start_paused = true)]
    async fn degrades_past_log_max_and_stops_flushing() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sub = tracing_subscriber::fmt()
            .with_writer(BufWriter(buf.clone()))
            .with_max_level(tracing::Level::WARN)
            .finish();
        let _guard = tracing::subscriber::set_default(sub);

        let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let store = Arc::new(FlakyStore::new(inner));
        let graph = new_graph();
        let task = FlushTask::new(
            graph.clone(),
            store.clone(),
            params(Duration::from_secs(1), 100, 1, 4),
        );
        let _handle = task.spawn();
        let_task_arm().await;

        store.fail_forever();
        let iid = add_interaction(&graph, 1, None);
        add_concept(&graph, 1, iid); // 3 mutations

        // Tick: attempt 1 fails, one retry scheduled.
        tokio::time::advance(Duration::from_secs(1)).await;
        wait_until(|| store.flush_calls() >= 1).await;
        assert_eq!(store.flush_calls(), 1);
        assert!(!task.degraded());

        // Retry fails too; retries exhausted -> batch retained, warning raised.
        tokio::time::advance(Duration::from_millis(100)).await;
        wait_until(|| store.flush_calls() >= 2).await;
        assert_eq!(store.flush_calls(), 2);
        assert_eq!(task.stats().depth, 3);
        assert!(!task.degraded());

        // More writes push pending past log_max=4: degrade, stop flushing.
        add_concept(&graph, 2, iid); // 2 more mutations -> pending 5
        tokio::time::advance(Duration::from_secs(1)).await;
        wait_until(|| task.degraded()).await;
        assert!(task.degraded());
        assert_eq!(store.flush_calls(), 2, "no flush attempt once degraded");

        // STORE-3: post-degrade the drained batch is DROPPED (spec §2.3
        // "none = pure RAM") — pending is cleared every cycle, so depth
        // tracks the in-graph log only (which drains to zero). The old
        // contract retained every mutation for the session's remaining life
        // (unbounded RAM growth).
        tokio::time::advance(Duration::from_millis(100)).await;
        wait_until(|| task.stats().depth == 0).await;
        assert_eq!(task.stats().depth, 0, "pending cleared post-degrade");
        assert_eq!(graph.read().log_len(), 0, "log drained post-degrade");

        // Still degrading, still no I/O; new writes drain and drop every
        // cycle — depth never accumulates the backlog.
        add_concept(&graph, 3, iid);
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(store.flush_calls(), 2);
        wait_until(|| task.stats().depth == 0).await;
        assert_eq!(task.stats().depth, 0);
        assert!(task.degraded());

        let out = String::from_utf8(buf.lock().clone()).unwrap();
        assert!(out.contains("BackendFlushFailed"), "warn missing: {out}");
        assert!(out.contains("FlushDegraded"), "error missing: {out}");
        assert!(out.contains("ERROR"), "error level missing: {out}");
    }

    #[tokio::test(start_paused = true)]
    async fn stats_depth_and_lag_across_success_and_failure() {
        // This test reaches the shared BackendFlushFailed warn; keep its
        // callsite from registering `never` (see `keep_callsites_enabled`).
        let _callsites = keep_callsites_enabled();

        let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let store = Arc::new(FlakyStore::new(inner));
        let graph = new_graph();
        let task = FlushTask::new(
            graph.clone(),
            store.clone(),
            params(Duration::from_secs(1), 100, 1, 1_000),
        );
        let _handle = task.spawn();
        let_task_arm().await;

        assert_eq!(
            task.stats(),
            FlushStats {
                lag: Duration::ZERO,
                depth: 0,
                dead_lettered: 0,
            }
        );

        store.fail_next(2);
        let iid = add_interaction(&graph, 1, None);
        add_concept(&graph, 1, iid); // 3 mutations

        // Poll drains the log; depth visible, lag grows with virtual time.
        tokio::time::advance(Duration::from_millis(100)).await;
        wait_until(|| task.stats().depth == 3).await;
        assert_eq!(task.stats().lag, Duration::from_millis(100));

        // Tick: attempt 1 fails (no success yet).
        tokio::time::advance(Duration::from_millis(900)).await;
        wait_until(|| store.flush_calls() >= 1).await;
        assert_eq!(task.stats().lag, Duration::from_secs(1));
        assert_eq!(task.stats().depth, 3);

        // Retry fails; batch retained.
        tokio::time::advance(Duration::from_millis(100)).await;
        wait_until(|| store.flush_calls() >= 2).await;
        assert_eq!(task.stats().lag, Duration::from_millis(1_100));
        assert_eq!(task.stats().depth, 3);

        // New writes while the batch is retained: depth = log + retained.
        add_concept(&graph, 2, iid); // 2 more mutations
        tokio::time::advance(Duration::from_millis(100)).await;
        wait_until(|| task.stats().depth == 5).await;
        assert_eq!(task.stats().depth, 5);
        assert_eq!(task.stats().lag, Duration::from_millis(1_200));

        // Store recovers: the retained batch held for RETAINED_BACKOFF (F3 —
        // the next tick after retry_after elapses re-enters the sequence and
        // lands the whole pending batch); lag resets.
        tokio::time::advance(Duration::from_secs(10)).await;
        wait_until(|| store.flush_calls() >= 3).await;
        wait_until(|| task.stats().depth == 0).await;
        assert_eq!(task.stats().depth, 0);
        assert!(
            task.stats().lag < Duration::from_millis(50),
            "lag reset after success"
        );

        let snap = store.load_session(&sid()).await.unwrap();
        assert_eq!(snap.interactions.len(), 1);
        assert_eq!(snap.concepts.len(), 2);
        assert_eq!(snap.edges.len(), 2); // Derives x2

        // Idle: lag grows again, depth stays 0.
        tokio::time::advance(Duration::from_millis(500)).await;
        assert_eq!(task.stats().depth, 0);
        assert_eq!(task.stats().lag, Duration::from_millis(500));
    }

    #[tokio::test(start_paused = true)]
    async fn retained_batch_and_new_drains_flush_together_in_order() {
        // This test reaches the shared BackendFlushFailed warn; keep its
        // callsite from registering `never` (see `keep_callsites_enabled`).
        let _callsites = keep_callsites_enabled();

        let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let store = Arc::new(FlakyStore::new(inner));
        let graph = new_graph();
        let task = FlushTask::new(
            graph.clone(),
            store.clone(),
            params(Duration::from_secs(1), 100, 1, 1_000),
        );
        let _handle = task.spawn();
        let_task_arm().await;

        store.fail_next(2); // attempts 1 and 2 fail -> batch retained
        let iid = add_interaction(&graph, 1, None);
        add_concept(&graph, 1, iid); // 3 mutations

        tokio::time::advance(Duration::from_secs(1)).await;
        wait_until(|| store.flush_calls() >= 1).await; // attempt 1 fails
        tokio::time::advance(Duration::from_millis(100)).await;
        wait_until(|| store.flush_calls() >= 2).await; // retry fails -> retained
        assert_eq!(task.stats().depth, 3);

        // New writes land while the batch is retained.
        let _iid2 = add_interaction(&graph, 2, Some(iid)); // 2 mutations
        assert_eq!(graph.read().log_len(), 2);

        // After the post-retry hold elapses (F3), the next tick re-enters the
        // sequence: drained(2) appended AFTER retained(3) -> one ordered batch
        // of 5. (11s from the retain point: 10s hold + 1s to the next tick.)
        tokio::time::advance(Duration::from_secs(11)).await;
        wait_until(|| task.stats().depth == 0).await;
        assert_eq!(store.batch_sizes(), vec![3, 3, 5]);

        let snap = store.load_session(&sid()).await.unwrap();
        assert_eq!(snap.interactions.len(), 2);
        assert_eq!(snap.concepts.len(), 1);
        assert_eq!(snap.edges.len(), 2); // Derives + Temporal
        assert_eq!(task.stats().depth, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn retained_batch_does_not_retry_every_tick() {
        // F3: after retries are exhausted the retained batch holds for
        // RETAINED_BACKOFF (10s) before the next attempt — a permanently
        // failing store must not re-run the whole retry sequence (attempts +
        // warn) on every interval tick. Attempts stay flat while the hold is
        // active; exactly one new attempt happens once it elapses.
        // This test reaches the shared BackendFlushFailed warn; keep its
        // callsite from registering `never` (see `keep_callsites_enabled`).
        let _callsites = keep_callsites_enabled();

        let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let store = Arc::new(FlakyStore::new(inner));
        let graph = new_graph();
        let task = FlushTask::new(
            graph.clone(),
            store.clone(),
            params(Duration::from_secs(1), 100, 1, 1_000),
        );
        let _handle = task.spawn();
        let_task_arm().await;

        store.fail_forever();
        let iid = add_interaction(&graph, 1, None);
        add_concept(&graph, 1, iid); // 3 mutations

        // Tick: attempt 1 fails; the retry at +100ms fails too -> retries
        // exhausted, batch retained, hold armed (retry_after = 1.1s + 10s).
        tokio::time::advance(Duration::from_secs(1)).await;
        wait_until(|| store.flush_calls() >= 1).await;
        tokio::time::advance(Duration::from_millis(100)).await;
        wait_until(|| store.flush_calls() >= 2).await;
        assert_eq!(store.flush_calls(), 2);
        assert_eq!(task.stats().depth, 3, "retained batch still pending");

        // Interval ticks keep firing, but attempts stay flat while the hold
        // is active (ticks at 2.0 .. 11.0 are all before retry_after 11.1).
        tokio::time::advance(Duration::from_secs(3)).await; // -> 4.1
        assert_eq!(store.flush_calls(), 2, "no attempt while in the hold");
        assert_eq!(task.stats().depth, 3, "batch retained through the hold");

        tokio::time::advance(Duration::from_secs(6)).await; // -> 10.1
        assert_eq!(
            store.flush_calls(),
            2,
            "still flat just before the hold elapses"
        );

        // The hold elapses at 11.1; the tick at 12.1 re-enters the retry
        // sequence with exactly one new attempt, then arms a fresh hold.
        tokio::time::advance(Duration::from_secs(2)).await; // -> 12.1
        wait_until(|| store.flush_calls() >= 3).await;
        assert_eq!(
            store.flush_calls(),
            3,
            "exactly one new attempt after the hold"
        );
        assert_eq!(
            task.stats().depth,
            3,
            "still retained (store still failing)"
        );

        // The new hold keeps attempts flat again.
        tokio::time::advance(Duration::from_secs(1)).await; // -> 13.1
        assert_eq!(store.flush_calls(), 3, "flat again during the second hold");
        assert_eq!(task.stats().depth, 3);
        assert!(!task.degraded(), "below log_max: degrade must not trigger");
    }

    #[tokio::test(start_paused = true)]
    async fn panicking_backend_is_contained_batch_retained_then_lands() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sub = tracing_subscriber::fmt()
            .with_writer(BufWriter(buf.clone()))
            .with_max_level(tracing::Level::WARN)
            .finish();
        let _guard = tracing::subscriber::set_default(sub);

        let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let store = Arc::new(PanicStore::new(inner));
        let graph = new_graph();
        let task = FlushTask::new(
            graph.clone(),
            store.clone(),
            params(Duration::from_secs(1), 100, 1, 1_000),
        );
        let handle = task.spawn();
        let_task_arm().await;

        store.panic_next(2);
        let iid = add_interaction(&graph, 1, None);
        add_concept(&graph, 1, iid); // 3 mutations

        // Tick: attempt 1 panics (caught) -> backoff 100ms begins. The spawned
        // loop must survive: an uncontained panic would abort it, finishing
        // the JoinHandle with `degraded()==false`.
        tokio::time::advance(Duration::from_secs(1)).await;
        wait_until(|| store.flush_calls() >= 1).await;
        assert_eq!(store.flush_calls(), 1);
        assert!(!handle.is_finished(), "flush loop aborted on backend panic");
        assert!(!task.degraded());
        assert_eq!(task.stats().depth, 3);
        assert!(
            store.load_session(&sid()).await.is_err(),
            "nothing landed yet"
        );

        // Retry panics too; retries exhausted -> batch RETAINED (never dropped).
        tokio::time::advance(Duration::from_millis(100)).await;
        wait_until(|| store.flush_calls() >= 2).await;
        assert_eq!(store.flush_calls(), 2);
        assert!(!handle.is_finished(), "flush loop aborted on backend panic");
        assert!(!task.degraded());
        assert_eq!(task.stats().depth, 3, "retained batch still pending");

        // Store stops panicking: once the post-retry hold elapses (F3), the
        // next tick lands the retained batch whole.
        tokio::time::advance(Duration::from_secs(11)).await;
        wait_until(|| task.stats().depth == 0).await;
        assert!(!handle.is_finished());
        assert_eq!(store.batch_sizes(), vec![3, 3, 3]);
        let snap = store.load_session(&sid()).await.unwrap();
        assert_eq!(snap.interactions.len(), 1);
        assert_eq!(snap.concepts.len(), 1);
        assert_eq!(snap.edges.len(), 1); // Derives edge
        assert!(
            task.stats().lag < Duration::from_millis(50),
            "lag reset after success"
        );

        // Each caught panic logged its payload via the BackendFlushPanic warn.
        let out = String::from_utf8(buf.lock().clone()).unwrap();
        assert_eq!(
            out.matches("BackendFlushPanic").count(),
            2,
            "warn missing: {out}"
        );
        assert!(
            out.contains("simulated flush panic (PanicStore)"),
            "panic payload missing: {out}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_panics_lead_to_degrade_past_log_max() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sub = tracing_subscriber::fmt()
            .with_writer(BufWriter(buf.clone()))
            .with_max_level(tracing::Level::WARN)
            .finish();
        let _guard = tracing::subscriber::set_default(sub);

        let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let store = Arc::new(PanicStore::new(inner));
        let graph = new_graph();
        let task = FlushTask::new(
            graph.clone(),
            store.clone(),
            params(Duration::from_secs(1), 100, 1, 4),
        );
        let handle = task.spawn();
        let_task_arm().await;

        store.panic_forever();
        let iid = add_interaction(&graph, 1, None);
        add_concept(&graph, 1, iid); // 3 mutations

        // Panicking attempts exhaust retries; batch retained below log_max.
        tokio::time::advance(Duration::from_secs(1)).await;
        wait_until(|| store.flush_calls() >= 1).await;
        tokio::time::advance(Duration::from_millis(100)).await;
        wait_until(|| store.flush_calls() >= 2).await;
        assert_eq!(task.stats().depth, 3);
        assert!(!task.degraded());
        assert!(!handle.is_finished());

        // More writes push pending past log_max=4 -> designed degrade path
        // (same as a failing backend): durability="none", I/O stops.
        add_concept(&graph, 2, iid); // 2 more mutations -> pending 5
        tokio::time::advance(Duration::from_secs(1)).await;
        wait_until(|| task.degraded()).await;
        assert!(task.degraded());
        // STORE-3: post-degrade the drained batch is dropped — depth tracks
        // the in-graph log only (drains to zero), never the retained backlog.
        tokio::time::advance(Duration::from_millis(100)).await;
        wait_until(|| task.stats().depth == 0).await;
        assert_eq!(task.stats().depth, 0, "pending cleared post-degrade");
        assert_eq!(store.flush_calls(), 2, "no flush attempt once degraded");
        assert!(!handle.is_finished(), "degrade must not abort the loop");

        // Every caught panic logged BackendFlushPanic; degrade logged too.
        let out = String::from_utf8(buf.lock().clone()).unwrap();
        assert_eq!(
            out.matches("BackendFlushPanic").count(),
            2,
            "warn missing: {out}"
        );
        assert!(
            out.contains("simulated flush panic (PanicStore)"),
            "panic payload missing: {out}"
        );
        assert!(out.contains("FlushDegraded"), "error missing: {out}");
    }

    #[tokio::test(start_paused = true)]
    async fn hung_store_flush_times_out_never_wedges_the_loop() {
        // STORE-2: a store whose flush NEVER resolves must not wedge the
        // flush loop. The per-attempt FLUSH_ATTEMPT_TIMEOUT ends the attempt;
        // the timeout maps into the existing retry path (backoff → retry →
        // retain), the session keeps working, and the loop never dies, never
        // degrades. Reaches the shared BackendFlushFailed warn; keep its
        // callsite from registering `never`.
        let _callsites = keep_callsites_enabled();

        let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let store = Arc::new(HungStore::new(inner));
        let graph = new_graph();
        let task = FlushTask::new(
            graph.clone(),
            store.clone(),
            params(Duration::from_secs(1), 100, 1, 1_000),
        );
        let handle = task.spawn();
        let_task_arm().await;

        store.hang_next(2); // attempts 1 and 2 hang; attempt 3 delegates
        let iid = add_interaction(&graph, 1, None);
        add_concept(&graph, 1, iid); // 3 mutations

        // Tick at 1.0: attempt 1 hangs. The timeout (not the store) ends the
        // attempt; the loop stays alive and un-degraded.
        tokio::time::advance(Duration::from_secs(1)).await;
        wait_until(|| store.flush_calls() >= 1).await;
        assert_eq!(store.flush_calls(), 1);
        assert!(!handle.is_finished(), "flush loop wedged on a hung store");
        assert!(!task.degraded());
        assert_eq!(task.stats().depth, 3);

        // Attempt 1 times out at 1s + FLUSH_ATTEMPT_TIMEOUT; backoff
        // (BACKOFF_BASE) then attempt 2 hangs the same way. Advance past the
        // deadline in small steps: the paused-clock harness has a small,
        // deterministic scheduling lag between a timer's deadline and the
        // flush task observing it, so a single exact advance is not enough.
        tokio::time::advance(FLUSH_ATTEMPT_TIMEOUT).await;
        for _ in 0..30 {
            if store.flush_calls() >= 2 {
                break;
            }
            tokio::time::advance(Duration::from_millis(100)).await;
        }
        assert_eq!(store.flush_calls(), 2);
        assert!(!handle.is_finished(), "flush loop wedged on a hung store");
        assert!(!task.degraded());

        // Attempt 2 times out; retries exhausted -> batch RETAINED (never
        // dropped), post-retry hold armed (F3). Still no degrade, loop alive.
        tokio::time::advance(FLUSH_ATTEMPT_TIMEOUT).await;
        tokio::time::advance(Duration::from_secs(1)).await; // absorb lag; retain settles
        assert_eq!(
            store.flush_calls(),
            2,
            "hold armed: no attempt during the hold"
        );
        assert_eq!(task.stats().depth, 3, "retained batch still pending");
        assert!(!handle.is_finished());
        assert!(!task.degraded());

        // The session keeps accepting writes while the store is hung.
        let _iid2 = add_interaction(&graph, 2, Some(iid)); // 2 more mutations
        tokio::time::advance(Duration::from_secs(1)).await;
        wait_until(|| task.stats().depth == 5).await;
        assert_eq!(task.stats().depth, 5);
        assert!(!task.degraded());

        // Hold elapses; the next tick re-enters the sequence and the
        // recovered store lands the whole pending batch in order.
        tokio::time::advance(Duration::from_secs(11)).await;
        wait_until(|| task.stats().depth == 0).await;
        assert_eq!(store.batch_sizes(), vec![3, 3, 5]);
        let snap = store.load_session(&sid()).await.unwrap();
        assert_eq!(snap.interactions.len(), 2);
        assert_eq!(snap.concepts.len(), 1);
        assert_eq!(snap.edges.len(), 2); // Derives + Temporal
        assert!(!handle.is_finished());
        assert!(!task.degraded());
    }

    #[tokio::test(start_paused = true)]
    async fn constraint_violation_dead_letters_the_batch() {
        // STORE-4 / D5: a deterministic constraint violation is dead-lettered
        // — logged and dropped (visible in stats), NOT retried, NOT retained,
        // NOT degraded. The session continues and the next batch flushes
        // fresh (no head-of-line poisoning).
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sub = tracing_subscriber::fmt()
            .with_writer(BufWriter(buf.clone()))
            .with_max_level(tracing::Level::WARN)
            .finish();
        let _guard = tracing::subscriber::set_default(sub);

        let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let store = Arc::new(ConstraintStore::new(inner));
        let graph = new_graph();
        let task = FlushTask::new(
            graph.clone(),
            store.clone(),
            params(Duration::from_secs(1), 100, 3, 1_000),
        );
        let handle = task.spawn();
        let_task_arm().await;

        let iid = add_interaction(&graph, 1, None);
        add_concept(&graph, 1, iid); // 3 mutations

        // Tick: the constraint surfaces with NO backoff (retries=3 are
        // configured but must not be consumed) and the batch is dropped.
        tokio::time::advance(Duration::from_secs(1)).await;
        wait_until(|| task.stats().dead_lettered == 1).await;
        assert_eq!(store.flush_calls(), 1, "constraint must not be retried");
        assert_eq!(task.stats().dead_lettered, 1);
        assert_eq!(task.stats().depth, 0, "dead-lettered batch cleared");
        assert!(!task.degraded(), "dead-letter must not degrade the session");
        assert!(!handle.is_finished());

        // Session continues: the next write flushes (and fails) fresh — the
        // dead letter did not poison the queue.
        add_interaction(&graph, 2, Some(iid)); // 2 mutations
        tokio::time::advance(Duration::from_secs(1)).await;
        wait_until(|| task.stats().dead_lettered == 2).await;
        assert_eq!(store.flush_calls(), 2);
        assert_eq!(task.stats().depth, 0);
        assert!(!task.degraded());

        let out = String::from_utf8(buf.lock().clone()).unwrap();
        assert!(out.contains("FlushDeadLettered"), "warn missing: {out}");
        assert!(out.contains("23505"), "constraint code missing: {out}");
    }

    #[tokio::test(start_paused = true)]
    async fn transient_failures_keep_the_retain_path_untouched() {
        // STORE-4 contrast: transient (Backend) failures keep the EXISTING
        // retain path — retried, then retained — and never touch
        // `dead_lettered`. Only constraint violations dead-letter.
        let _callsites = keep_callsites_enabled();

        let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let store = Arc::new(FlakyStore::new(inner));
        let graph = new_graph();
        let task = FlushTask::new(
            graph.clone(),
            store.clone(),
            params(Duration::from_secs(1), 100, 1, 1_000),
        );
        let _handle = task.spawn();
        let_task_arm().await;

        store.fail_forever();
        let iid = add_interaction(&graph, 1, None);
        add_concept(&graph, 1, iid); // 3 mutations

        tokio::time::advance(Duration::from_secs(1)).await;
        wait_until(|| store.flush_calls() >= 1).await;
        tokio::time::advance(Duration::from_millis(100)).await;
        wait_until(|| store.flush_calls() >= 2).await;
        assert_eq!(task.stats().depth, 3, "transient failure retains the batch");
        assert_eq!(
            task.stats().dead_lettered,
            0,
            "transient failure is not a dead letter"
        );
        assert!(!task.degraded());
    }
}
