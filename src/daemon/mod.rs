//! Daemon task skeleton + composite scoring (P4, T4.1) + event transport
//! wiring (P4, T4.6).
//!
//! A tokio task that polls [`Graph::epoch`] on a tick interval. Each cycle:
//! rescore the session's concepts **when the epoch changed** (spec §9, spec
//! §2.5 warm-up note) and *always* run the daemon detectors — conflict,
//! drift, stale, and high-risk modification — against the current graph.
//! Detector hits are published on the §6.1 broadcast event channel
//! ([`events`]) on condition **transition** (a condition that enters the
//! detected set is emitted once; exit just stops emitting), the daemon-owned
//! hot list is kept equal to the cycle's fresh hits, and GC runs
//! periodically every `gc_interval` mutations.
//!
//! Detection runs on **every** tick, not only on epoch change: an idle
//! session still ages toward staleness — a concept untouched for longer than
//! the stale window fires `DaemonEvent::Stale` purely because time passed
//! (spec §9's background-daemon semantics; T4.6 finding 1).
//!
//! ## Wake seam (COH-5, 2026-08-12)
//!
//! There is **no mutation-notify channel and no T3.5 rescore signal** — both
//! were explicitly deferred. The loop is driven by the tick interval plus an
//! explicit [`Notify`] wake that tests use to trigger a cycle immediately;
//! the production notify seam lands with T8.1.
//!
//! ## Lock discipline (spec §6.4 — non-negotiable)
//!
//! The graph lock is **never held across an `.await`**. Each cycle: take the
//! lock, run the synchronous detection/rescore/GC work, release, then await
//! the next tick/wake. `parking_lot` guards are `!Send`, so the compiler
//! enforces this inside `tokio::spawn`. Lock order is always graph → hot
//! list (never the reverse).
//!
//! ## Stopping
//!
//! [`Daemon::spawn`] returns the `JoinHandle`; aborting it stops the loop (a
//! graceful stop is a P8 concern per the COH-6 note).
pub mod conflict;
pub mod drift;
pub mod events;
pub mod gc;
pub mod hotlist;
pub mod score;

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, Notify};

use crate::config::{Config, RecallWeights, ScoringWeights};
use crate::daemon::hotlist::{Condition, HotList};
use crate::graph::index::InvertedIndex;
use crate::graph::Graph;
use crate::recall::cache::{CacheKey, RecallCache};
use crate::recall::{assemble, candidates, expand};
use crate::types::{DaemonEvent, NodeId, RecallQuery, RecallResult, Scored, SessionId};

/// Default daemon poll interval (XP-7).
///
/// The spec fixes no value; 1s matches `backend_flush_interval` so the daemon
/// and the write-behind loop age state on the same beat, and it bounds the
/// window in which a hot-list entry can lag the graph to one second. Mirrored
/// by [`Config::daemon_tick_interval`], which is what P8 threads in.
pub const DAEMON_TICK_INTERVAL: Duration = Duration::from_secs(1);

/// The daemon's score table — epoch of the graph state it was computed from,
/// plus the score-descending ranked list of concept scores.
///
/// Daemon-owned: the rescore loop replaces it wholesale each cycle. T4.2+
/// reads it; never mutated from outside.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScoreTable {
    /// [`Graph::epoch`] the scores were computed from.
    pub epoch: u64,
    /// Score-descending (id-ascending tie-break) concept scores.
    pub ranked: Vec<Scored<NodeId>>,
}

/// The daemon cycle's `now` source (T4.6 finding-1 regression seam).
///
/// Production uses [`Utc::now`]; tests swap in a controllable clock
/// ([`Daemon::with_clock`]) so an idle session can be aged past a detector
/// window (e.g. staleness) without waiting on the wall clock.
/// The epoch-stable recall pipeline artifact: phase-1 candidates plus the
/// phase-2 expansion. Cached as a unit; assembly and rendering re-run on
/// every call so time-sensitive output (hot-list `seconds_ago`,
/// reservations, liveness) is never frozen by a cache hit (spec §9
/// "conditions re-validated on each recall()"; P5 phase-close finding).
#[derive(Clone)]
pub struct RecallPipeline {
    phase1: Vec<Scored<NodeId>>,
    expanded: expand::ExpandedSet,
}

pub type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// Background scorer + detector + event publisher (T4.1 skeleton, T4.6
/// wiring). Spawn with [`Daemon::spawn`].
pub struct Daemon {
    graph: Arc<RwLock<Graph>>,
    weights: ScoringWeights,
    tick: Duration,
    wake: Arc<Notify>,
    scores: Arc<RwLock<ScoreTable>>,
    hot: Arc<RwLock<HotList>>,
    events: events::EventSender,
    /// The owner's inverted index, when it gave the daemon one. GC mirrors
    /// collections into it via [`gc::sync_index`] (XP-5); `None` means the
    /// owner is doing that itself.
    index: Option<Arc<RwLock<InvertedIndex>>>,
    /// The most recent [`gc::GcOutcome`], for [`Daemon::last_gc`] (XP-5).
    last_gc: Arc<RwLock<Option<gc::GcOutcome>>>,
    /// Completed cycles, for [`Daemon::cycles`] (XP-6).
    cycles: Arc<AtomicU64>,
    params: CycleParams,
    clock: Clock,
    started: AtomicBool,
}

/// Daemon loop tuning (T4.6).
///
/// [`CycleParams::default`] is defined as `From<&Config::default()>` — the
/// shared knobs (hot_list_max, conflict_recency_window, drift_threshold,
/// gc_interval, max_canonical_nodes) have exactly one source of truth, so a
/// default can no longer drift between here and `Config` (XP-7; they were
/// duplicated as literals). The T4.6-specific knobs Config does not carry
/// (staleness / high-risk windows, event capacity, GC bump chunk) come from
/// their module consts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CycleParams {
    /// `conflict_recency_window` (spec §9).
    pub conflict_window: Duration,
    /// `drift_threshold` (spec §9).
    pub drift_threshold: usize,
    /// Untouched-for-this-long ⇒ `DaemonEvent::Stale` ([`events::STALE_WINDOW`]).
    pub stale_window: Duration,
    /// Fresh write to a high-value node ⇒ `DaemonEvent::HighRisk`.
    pub high_risk_window: Duration,
    /// Hot-list bound (`hot_list_max`, spec §9).
    pub hot_list_max: usize,
    /// GC runs every this many session mutations (`gc_interval`, spec §9).
    pub gc_interval: u64,
    /// Canonical budget ceiling GC records (`max_canonical_nodes`, spec §10).
    pub max_canonical_nodes: usize,
    /// Broadcast capacity (see [`events::EVENT_CAPACITY`]).
    pub event_capacity: usize,
    /// Survivor `gc_survived` bumps applied per cycle
    /// ([`gc::GC_SURVIVOR_BUMP_CHUNK`], CONC-6/XP-10).
    pub gc_survivor_bump_chunk: usize,
}

impl From<&Config> for CycleParams {
    /// Derive the loop's tuning from the session config (XP-7). P8 calls this
    /// instead of hand-copying fields; the knobs `Config` does not carry keep
    /// their module defaults.
    fn from(config: &Config) -> Self {
        Self {
            conflict_window: config.conflict_recency_window,
            drift_threshold: config.drift_threshold,
            stale_window: events::STALE_WINDOW,
            high_risk_window: events::HIGH_RISK_WRITE_WINDOW,
            hot_list_max: config.hot_list_max,
            gc_interval: config.gc_interval,
            max_canonical_nodes: config.max_canonical_nodes,
            event_capacity: events::EVENT_CAPACITY,
            gc_survivor_bump_chunk: gc::GC_SURVIVOR_BUMP_CHUNK,
        }
    }
}

impl Default for CycleParams {
    fn default() -> Self {
        Self::from(&Config::default())
    }
}

impl Daemon {
    /// `tick` is the rescore poll interval; tests pass a long tick and drive
    /// cycles with [`Daemon::wake`].
    pub fn new(graph: Arc<RwLock<Graph>>, weights: ScoringWeights, tick: Duration) -> Self {
        Self::with_params(graph, weights, tick, CycleParams::default())
    }

    /// `new` with explicit loop tuning (tests; P8 wires `Config` here).
    pub fn with_params(
        graph: Arc<RwLock<Graph>>,
        weights: ScoringWeights,
        tick: Duration,
        params: CycleParams,
    ) -> Self {
        assert!(
            params.event_capacity > 0,
            "event_capacity must be > 0 (broadcast channels cannot be empty)"
        );
        // `new`/`with_params` construct the struct literal below; the clock
        // defaults to the wall clock — `with_clock` swaps it (tests).
        let (sender, _) = events::event_channel_with_capacity(params.event_capacity);
        Self {
            graph,
            weights,
            tick,
            wake: Arc::new(Notify::new()),
            scores: Arc::new(RwLock::new(ScoreTable::default())),
            hot: Arc::new(RwLock::new(HotList::with_max(params.hot_list_max))),
            events: sender,
            index: None,
            last_gc: Arc::new(RwLock::new(None)),
            cycles: Arc::new(AtomicU64::new(0)),
            params,
            clock: Arc::new(Utc::now),
            started: AtomicBool::new(false),
        }
    }

    /// Build the loop's tuning from a [`Config`] (XP-7): `tick` comes from
    /// `daemon_tick_interval`, the rest from [`CycleParams::from`]. P8's entry
    /// point — nothing has to re-derive a default.
    pub fn from_config(graph: Arc<RwLock<Graph>>, config: &Config) -> Self {
        Self::with_params(
            graph,
            config.scoring,
            config.daemon_tick_interval,
            CycleParams::from(config),
        )
    }

    /// Use `clock` as the cycle's `now` source instead of the wall clock
    /// (tests drive a controllable clock; see [`Clock`]).
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// Give the daemon the owner's inverted index so GC mirrors its collections
    /// into it ([`gc::sync_index`], spec §9 step 4 — XP-5).
    ///
    /// The index is owner-side by the P3 contract (`src/graph/mod.rs`), so
    /// `gc::run(&mut Graph, …)` structurally cannot reach it; without this the
    /// hook had no production caller and a GC'd concept stayed searchable in the
    /// index until the owner happened to notice. An owner that mirrors GC
    /// itself — reading [`Daemon::last_gc`] — simply does not call this.
    pub fn with_index(mut self, index: Arc<RwLock<InvertedIndex>>) -> Self {
        self.index = Some(index);
        self
    }

    /// Spawn the daemon loop and return its handle (abort = stop).
    ///
    /// Call `spawn` **exactly once** per `Daemon` — a second call panics
    /// (single-loop enforcement, mirroring `FlushTask::spawn`). Takes `&self`
    /// so the caller keeps this handle for [`Daemon::wake`] /
    /// [`Daemon::scores`] / [`Daemon::events`] while the task runs.
    pub fn spawn(&self) -> tokio::task::JoinHandle<()> {
        self.started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .expect("Daemon::spawn called twice — exactly one loop may run");
        let graph = self.graph.clone();
        let wake = self.wake.clone();
        let scores = self.scores.clone();
        let hot = self.hot.clone();
        let sender = self.events.clone();
        let weights = self.weights;
        let tick = self.tick;
        let params = self.params;
        let clock = self.clock.clone();
        let state = LoopState {
            graph,
            wake,
            scores,
            hot,
            sender,
            clock,
            index: self.index.clone(),
            last_gc: self.last_gc.clone(),
            cycles: self.cycles.clone(),
        };
        tokio::spawn(async move {
            run_loop(state, weights, tick, params).await;
        })
    }

    /// Wake the loop for an immediate cycle (tests; later the T8.1 seam).
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    /// Snapshot of the daemon-owned score table.
    pub fn scores(&self) -> ScoreTable {
        self.scores.read().clone()
    }

    /// Three-phase recall (spec §8; P5).
    ///
    /// Store I/O happens in [`crate::recall::candidates::gather`] BEFORE any
    /// lock: the vector leg is async and must not run while the graph lock is
    /// held. The pipeline then runs under the documented lock order
    /// (graph read -> hot write). The daemon's inverted index must be
    /// installed via [`Daemon::with_index`]; without it recall returns an
    /// empty hit list with a warning (P8 wires the owner's index).
    ///
    /// `cache` is session-scoped: spec §8's key carries no session id, so the
    /// caller owns one [`RecallCache`] per session and hands it over by
    /// `&mut` (the cache has no interior synchronization). The cache stores
    /// the epoch-stable [`RecallPipeline`]; phase-3 assembly, hot-list
    /// re-validation and context rendering run on EVERY call with the
    /// caller's current `now`, so warning lines are always fresh.
    ///
    /// `embedding` is the query embedding when an embedder is configured;
    /// `None` degrades to the keyword + recent-interactions legs (spec §3.2).
    /// A store error during `gather` degrades to an empty vector leg with a
    /// warning rather than failing the read.
    pub async fn recall(
        &self,
        session: &SessionId,
        query: RecallQuery,
        store: &dyn crate::store::GraphStore,
        embedding: Option<&[f32]>,
        weights: RecallWeights,
        cache: &mut RecallCache<RecallPipeline>,
    ) -> RecallResult {
        // Cache probe needs only the epoch: a brief graph read, no gather.
        let key = CacheKey::new(
            &query.query,
            query.top_k,
            query.traversal_depth,
            self.graph.read().epoch(),
        );
        let pipeline = match cache.get(&key) {
            Some(pipeline) => pipeline.clone(),
            None => {
                // Gather BEFORE the graph lock: vector_candidates is store I/O.
                let input = match candidates::gather(store, session, embedding, query.top_k).await {
                    Ok(input) => input,
                    Err(err) => {
                        tracing::warn!(
                            target: "lambo::recall",
                            "phase-1 gather degraded: {err}"
                        );
                        candidates::Phase1Input::default()
                    }
                };

                // Scores snapshot before the locks (its own lock, no ordering
                // constraint), then graph read -> index read.
                let scores = self.scores.read().clone();
                let graph = self.graph.read();
                let index = self.index.as_ref().map(|i| i.read());

                // Re-read the epoch under the graph lock: if a mutation landed
                // between the probe and here, the insert key below reflects
                // it, so the cache can never serve this compute across an
                // epoch boundary.
                let key = CacheKey::new(
                    &query.query,
                    query.top_k,
                    query.traversal_depth,
                    graph.epoch(),
                );

                let phase1 = match index.as_deref() {
                    Some(index) => {
                        candidates::candidates(&graph, index, input, &query.query, query.top_k)
                    }
                    // The missing-index warning is re-emitted on every call
                    // below (it is time-invariant within an epoch).
                    None => Vec::new(),
                };
                let expanded = expand::expand(&graph, phase1.clone(), query.traversal_depth);
                let pipeline = RecallPipeline { phase1, expanded };

                // Never cache a compute whose daemon scores lag the graph
                // epoch (the rescore is epoch-gated, up to one tick late): the
                // next call after the rescore must recompute against the fresh
                // table (P5 phase-close finding).
                if scores.epoch == graph.epoch() {
                    cache.insert(key, pipeline.clone());
                }
                pipeline
            }
        };

        // Fresh per call: current scores, hot-list re-validation, reservations
        // and rendering — none of it cached (spec §9).
        let scores = self.scores.read().clone();
        let graph = self.graph.read();
        let mut hot = self.hot.write();
        let now = (self.clock)();
        let mut result = assemble::assemble(
            &graph,
            &pipeline.expanded,
            &pipeline.phase1,
            &scores,
            &mut hot,
            &query,
            weights,
            now,
            assemble::default_token_count,
        );
        let no_index = self.index.is_none();
        if no_index {
            result.warnings.push(
                "recall: no inverted index installed (Daemon::with_index) - keyword leg unavailable"
                    .to_string(),
            );
        }
        result
    }

    /// Cycles completed since [`Daemon::spawn`] (XP-6).
    ///
    /// A cycle increments this only after it finishes, so a test can assert
    /// "a full cycle ran and published nothing" instead of sleeping and hoping.
    /// A cycle that panicked (CONC-4) does not count.
    pub fn cycles(&self) -> u64 {
        self.cycles.load(Ordering::Acquire)
    }

    /// The most recent GC run's [`gc::GcOutcome`], or `None` before the first
    /// run (XP-5).
    ///
    /// This is T6.4's canonical-budget signal — `canonical_count`,
    /// `canonical_over_budget` and the ceiling it was checked against — plus
    /// `concepts_collected` for an owner mirroring the index itself, the
    /// advisory `warnings`, and `epoch_after` for T5.4's cache. Everything but
    /// `epoch_after` was previously dropped on the floor inside `run_loop`.
    pub fn last_gc(&self) -> Option<gc::GcOutcome> {
        self.last_gc.read().clone()
    }

    /// Subscribe to the daemon's event channel (spec §6.1). The receiver
    /// sees every `DaemonEvent` published after subscription; P8's
    /// `mem.events()` delegates here.
    ///
    /// A dropped receiver is not an error and a lagging receiver never blocks
    /// the daemon — it misses messages (`RecvError::Lagged`) and re-syncs to
    /// the newest retained window.
    ///
    /// ## Subscribe **before** [`Daemon::spawn`] (CONC-3)
    ///
    /// The loop's first cycle is the warm-up (spec §2.5), and it runs
    /// immediately — on a resumed session it detects and publishes the whole
    /// condition set the reload restored, including the planted demo
    /// `Conflict`. `broadcast` delivers only what is sent *after* a receiver
    /// subscribes, so a subscriber created after `spawn` races the warm-up and
    /// normally loses: emission is on transition, so nothing re-publishes for
    /// its benefit on the next cycle. The re-arm path (CONC-2) republishes a
    /// still-held condition only once the ring has wrapped past it, which is
    /// not a delivery guarantee for a late subscriber.
    ///
    /// P8 must therefore call `events()` **before** `spawn()`. Pinned by
    /// `daemon::tests::late_subscriber_misses_the_warm_up_condition_set`.
    pub fn events(&self) -> broadcast::Receiver<DaemonEvent> {
        self.events.subscribe()
    }

    /// The daemon's event **sender** (spec §6.1's single channel), for
    /// non-daemon publishers — P6's canonization evaluator calls
    /// [`events::emit_canonized`] with it (XP-4).
    ///
    /// Every clone feeds the same ring, so `Canonized` events reach the same
    /// [`Daemon::events`] subscribers as the daemon's own detector events, and
    /// the daemon retains its own handle so a dropped clone never closes the
    /// channel. Publish with [`events::EventSender::send`] (or
    /// [`events::emit_canonized`]) and nothing else is required of the caller.
    ///
    /// ## Why this is not a `broadcast::Sender` (NEW-3)
    ///
    /// It used to be. Every publisher must advance the channel's shared
    /// publication counter, because that counter is how the loop's re-arm path
    /// (CONC-2) knows a held condition's event has been pushed out of the ring.
    /// A raw `Sender` clone advanced the ring without advancing the counter, so
    /// an external publisher could evict a held `Conflict` **permanently**: 300
    /// external `Canonized` sends against a continuously-held `Conflict` and 601
    /// daemon cycles delivered zero `Conflict` events.
    pub fn event_sender(&self) -> events::EventSender {
        self.events.clone()
    }

    /// Handle to the daemon-owned hot list (recall, T5.3, reads it; tests
    /// assert maintenance here). The loop holds the graph lock while
    /// updating it, so consumers must never take the graph lock while
    /// holding this one.
    pub fn hot_list(&self) -> Arc<RwLock<HotList>> {
        self.hot.clone()
    }
}

/// The daemon's shared state, moved into the spawned loop task. One handle
/// per daemon (built in [`Daemon::spawn`]).
struct LoopState {
    graph: Arc<RwLock<Graph>>,
    wake: Arc<Notify>,
    scores: Arc<RwLock<ScoreTable>>,
    hot: Arc<RwLock<HotList>>,
    sender: events::EventSender,
    clock: Clock,
    index: Option<Arc<RwLock<InvertedIndex>>>,
    last_gc: Arc<RwLock<Option<gc::GcOutcome>>>,
    cycles: Arc<AtomicU64>,
}

/// The detected condition set for one cycle — `(condition, node)` pairs.
///
/// Both the emit-on-transition diff (finding 3) and the hot-list sync
/// (finding 2) key on this set: an event fires when a pair *enters* the set,
/// and an entry stays on the hot list only while its pair is in it.
fn condition_set(
    conflict: &[conflict::ConflictHit],
    drift: &[drift::DriftHit],
    stale: &[events::StaleHit],
    high_risk: &[events::HighRiskHit],
) -> HashSet<(Condition, NodeId)> {
    let mut set =
        HashSet::with_capacity(conflict.len() + drift.len() + stale.len() + high_risk.len());
    set.extend(conflict.iter().map(|h| (Condition::Conflict, h.node)));
    set.extend(drift.iter().map(|h| (Condition::Drift, h.node)));
    set.extend(stale.iter().map(|h| (Condition::StaleSession, h.node)));
    set.extend(
        high_risk
            .iter()
            .map(|h| (Condition::HighRiskModification, h.node)),
    );
    set
}

/// The daemon loop (rescore + detection + event publish + periodic GC).
///
/// First cycle runs immediately (the warm-up; spec §2.5), then on every tick
/// or wake. All work runs under brief synchronous lock scopes (never across
/// an `.await` — the select is the only suspension point):
///
/// 1. **Rescore — epoch-gated** (T4.1). Only when the graph epoch changed.
///    Detection below runs on *every* cycle, so an idle session still ages
///    into staleness (spec §9 background-daemon semantics; T4.6 finding 1).
/// 2. **Detect + hot-list sync + publish — every cycle** (T4.6). Run the
///    four detectors against the current graph; the hot list is set equal to
///    this cycle's fresh hits ([`HotList::retain_conditions`] drops entries
///    whose `(condition, node)` is no longer detected — no captured-`now`
///    predicate, no ghosts; finding 2). Events are **emit-on-transition**: a
///    `DaemonEvent` fires when a `(condition, node)` *enters* the detected
///    set, so a persisting condition is published once, not once per cycle —
///    a 256-capacity channel is never flooded with duplicates (finding 3).
///    Exit = stop emitting (`DaemonEvent` has no resolved variant — frozen
///    §6.1 enum). Hot-list entries still refresh per cycle.
///
///    **Re-arm (CONC-2).** Emit-on-transition alone loses an event
///    permanently: the transition is recorded whether or not any consumer
///    received it, so an event evicted from the ring while its condition
///    still holds is never re-published — and the demo's `Conflict` is
///    exactly such an event. So each held pair remembers the emission count
///    at its last publish, and once `event_capacity` further events have been
///    published that event can no longer be in the retained window.
///
///    That count is the **channel's**, shared by every publisher
///    ([`events::EventSender`], NEW-3), not a loop-private tally: anyone's send
///    advances the ring, so a publisher outside the loop — P6's
///    `emit_canonized` — must advance the same counter or its sends evict a
///    held condition invisibly.
///
///    The policy is deliberately minimal: **at most one re-arm per cycle**,
///    the pair whose last emission is oldest. Re-arming every eligible pair at
///    once would rebuild the same burst that evicts events in the first place;
///    one per cycle cannot itself overflow the ring, and always picking the
///    oldest gives round-robin coverage of a held set of any size. It is also
///    deliberately *conservative* — it re-arms on possible eviction, not on an
///    observed `Lagged`, which `broadcast` gives the sender no way to see.
///
///    The guarantee is **liveness, not exactly-once** — and it rests on the
///    counting above: as long as every publisher goes through
///    [`events::EventSender`], a still-held condition is eventually
///    re-published, and a duplicate advisory event is harmless (§6.1 has no
///    resolved variant to reconcile against). What is ruled out is the
///    permanent loss. An uncounted send would break the guarantee, not merely
///    delay it, which is why no raw `broadcast::Sender` is handed out (NEW-3).
///
///    **Order.** Publication is highest-severity-first — Conflict, HighRisk,
///    Drift, then the single session Stale ([`Condition::severity`]) — so a
///    consumer draining a burst in order sees the most actionable event
///    first. Ring-eviction protection is re-arm's job, not ordering's.
/// 3. Run GC once the mutation counter crosses `gc_interval` (spec §9). The
///    counter spans the graph's whole lifetime; GC resets it to its own
///    `epoch_after` so the next interval measures session mutations only.
///    Detection runs before GC: events reflect what the session's writes
///    did, GC is housekeeping after.
///
///    **Scoring stays inside the write guard (CONC-1, decided).** GC's step-2
///    rescore is deliberately *not* hoisted to a read snapshot. Step 2 must
///    score post-step-1 state, so a hoist means write-guard step 1, release,
///    score, re-acquire for steps 2–3 — a TOCTOU window in which a concurrent
///    `record_action` can add edges to a node already marked for collection,
///    and two write guards where the review verified one ("GC's mutations and
///    epoch bumps happen under one write guard"). The measured 272ms guard was
///    root-caused to `incident_edges` being a full edge scan, which the same
///    finding fixes: with the adjacency index the rescore is `O(nodes ×
///    degree)`, so atomicity is kept and the hold shrinks by the same factor.
///    Revisit only if a profile at scale says otherwise.
///
///    Survivor bumps are chunked (CONC-6/XP-10): step 5 applies at most
///    `gc_survivor_bump_chunk` per cycle and the loop drains the remainder on
///    following cycles, so one sweep can no longer enqueue twenty flush batches
///    from inside the guard. GC does not re-run while a drain is outstanding.
///
///    **A sweep must not fund the next one (NEW-2).** Survivor bumps are
///    mutations, so they advance the epoch; crediting them as *session*
///    mutations made GC self-sustaining on an idle session whenever
///    `survivors >= gc_interval + chunk` — every drain paid for the next
///    sweep, `gc_survived` climbed with no writes at all (crossing
///    canonization Stage 1's `>= 3` gate by idling), and the epoch ran away
///    from T5.4's recall cache. `epoch_after` covers only the bumps applied
///    *inside* `gc::run`; the deferred tail lands on later cycles, so each
///    drain advances `last_gc_epoch` by exactly what it appended. The elapsed
///    measure then counts session writes only, and an idle session reaches a
///    fixed point: bumps drain once, no further sweep. The current cycle's
///    `epoch` snapshot predates its own drain, so a drain cycle understates
///    elapsed by that chunk and the next cycle measures exactly — GC can be
///    one cycle late, never early.
///
/// ## Panic containment (CONC-4)
///
/// The cycle body is synchronous, so it is run inside `catch_unwind`: a panic
/// anywhere in scoring, detection, publication or GC is logged and the loop
/// continues to the next tick. Without this a single panic killed the task
/// silently and the process ran on with no scoring, no events and no GC for its
/// whole lifetime — flush.rs made the same argument for the write-behind loop
/// (`CatchUnwindPoll`) and reached the same conclusion.
///
/// Why continuing is sound:
///
/// * `parking_lot` guards release during unwind and the locks do not poison, so
///   the graph and hot list stay usable.
/// * The graph may be left **partially mutated** (a panic mid-GC-sweep). Every
///   mutation already applied is already in the append-only mutation log, so the
///   graph and the store stay consistent with each other; the next cycle
///   re-derives scores, hot list and detector hits from graph state, holding
///   nothing over.
/// * [`CycleState`] may be partially updated. The worst case is one duplicate
///   or one missed condition transition, and the re-arm path re-publishes a
///   still-held condition anyway.
async fn run_loop(state: LoopState, weights: ScoringWeights, tick: Duration, params: CycleParams) {
    let mut interval = tokio::time::interval(tick);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut cycle_state = CycleState::default();
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = state.wake.notified() => {}
        }
        // CONC-4: contain a panic in the cycle body — log it, keep the loop.
        let contained = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_cycle(&state, &weights, &params, &mut cycle_state);
        }));
        if let Err(payload) = contained {
            tracing::error!(
                target: "lambo::daemon",
                panic = %crate::store::flush::panic_message(&payload),
                "DaemonCyclePanic: daemon cycle panicked; the loop continues with the \
                 next tick (see run_loop's panic-containment note)"
            );
        }
    }
}

/// The loop's carry-over state between cycles.
#[derive(Default)]
struct CycleState {
    /// `None` → the first cycle always rescores (warm-up), then epoch-gated.
    last_epoch: Option<u64>,
    /// GC watermark: the epoch the next `gc_interval` is measured from. Set to
    /// `GcOutcome::epoch_after` when a sweep runs, then advanced by every
    /// deferred-bump drain so GC's own mutations are never credited as session
    /// mutations (NEW-2 — see `run_loop`'s step 3).
    last_gc_epoch: u64,
    /// Emit-on-transition (finding 3) + re-arm (CONC-2): every currently-held
    /// `(condition, node)` maps to the channel's publication index at its last
    /// emission ([`events::EventSender::send`]'s return). A pair absent from the
    /// map is entering the set and publishes; a pair whose stamp is
    /// `event_capacity` publications old has been pushed out of the broadcast
    /// ring and publishes again.
    ///
    /// The count lives on the channel, not here (NEW-3): it must include **every**
    /// publisher's sends, since anyone's send advances the ring. Only ever compared
    /// as a difference against these stamps, so wrap-around is not a concern (u64).
    armed: HashMap<(Condition, NodeId), u64>,
    /// Survivor bumps the last GC run deferred (CONC-6/XP-10). Drained a chunk
    /// per cycle; GC does not re-run until it is empty.
    gc_pending: Vec<NodeId>,
}

/// One cycle: rescore, detect, publish, GC. Fully synchronous — no `.await`, so
/// the graph lock is structurally incapable of spanning a suspension point
/// (spec §6.4) and the whole body fits inside one `catch_unwind` (CONC-4).
fn run_cycle(
    state: &LoopState,
    weights: &ScoringWeights,
    params: &CycleParams,
    cs: &mut CycleState,
) {
    let LoopState {
        graph,
        scores,
        hot,
        sender,
        clock,
        index,
        last_gc,
        cycles,
        ..
    } = state;
    // Brief lock: read epoch, release.
    let epoch = graph.read().epoch();
    let now = clock();

    // 1. Rescore — only when the epoch changed (finding 1: detection is
    //    NOT epoch-gated; an idle session must age into staleness).
    if cs.last_epoch != Some(epoch) {
        cs.last_epoch = Some(epoch);
        let ranked = {
            let g = graph.read();
            score::rescore(&g, weights)
        };
        *scores.write() = ScoreTable { epoch, ranked };
    }

    // 2. Detect + hot-list sync + publish. Lock order: graph read → hot
    //    write; every call here is synchronous.
    let (conflict_hits, drift_hits, stale_hits, high_risk_hits, fresh) = {
        let g = graph.read();
        let mut h = hot.write();
        let conflict_hits = conflict::insert_conflicts(&mut h, &g, params.conflict_window, now);
        let drift_hits = drift::record(&mut h, &g, params.drift_threshold);
        let stale_hits = events::insert_stale(&mut h, &g, params.stale_window, now);
        let high_risk_hits = events::insert_high_risk(&mut h, &g, params.high_risk_window, now);
        // Hot list = this cycle's fresh hits (finding 2): drop entries
        // whose (condition, node) is no longer detected — a HighRisk
        // entry whose 30s window elapsed ages out here, not in a frozen
        // captured-`now` predicate. Also removes the old
        // O(hot_len × full-graph scan) per-cycle revalidation.
        let fresh = condition_set(&conflict_hits, &drift_hits, &stale_hits, &high_risk_hits);
        h.retain_conditions(&fresh);
        (conflict_hits, drift_hits, stale_hits, high_risk_hits, fresh)
    };

    // Publish — fire-and-forget (spec §6.1): zero receivers → the event
    // is discarded; lagged receivers skip it. The daemon never blocks.
    // A pair that left the detected set is disarmed (exit = stop emitting).
    cs.armed.retain(|pair, _| fresh.contains(pair));

    // Pass 1 — pairs that ENTERED the set, highest severity first, so a
    // consumer draining a burst in order sees the Conflict before the
    // session Stale.
    for hit in &conflict_hits {
        if let Entry::Vacant(slot) = cs.armed.entry((Condition::Conflict, hit.node)) {
            slot.insert(events::emit(sender, events::conflict_event(hit)));
        }
    }
    for hit in &high_risk_hits {
        if let Entry::Vacant(slot) = cs.armed.entry((Condition::HighRiskModification, hit.node)) {
            slot.insert(events::emit(
                sender,
                events::high_risk_event(hit.node, hit.reason.clone()),
            ));
        }
    }
    for hit in &drift_hits {
        if let Entry::Vacant(slot) = cs.armed.entry((Condition::Drift, hit.node)) {
            slot.insert(events::emit(sender, events::drift_event(hit)));
        }
    }
    for hit in &stale_hits {
        if let Entry::Vacant(slot) = cs.armed.entry((Condition::StaleSession, hit.node)) {
            slot.insert(events::emit(
                sender,
                events::stale_event(hit.node, hit.seconds_inactive),
            ));
        }
    }

    // Pass 2 — re-arm ONE held pair (CONC-2). Stamps are unique, so
    // "oldest stamp" is a total order: the pair whose event has been out of
    // the retained window longest goes first, and re-publishing it moves it
    // to the back of the queue. One per cycle is what keeps re-arm from
    // recreating the very burst it exists to repair.
    if let Some((pair, stamp)) = cs
        .armed
        .iter()
        .min_by_key(|(_, stamp)| **stamp)
        .map(|(pair, stamp)| (*pair, *stamp))
    {
        // NEW-3: the count is the channel's, so an external publisher's sends
        // are measured too — they evict from the same ring.
        if sender.emitted_total() - stamp >= params.event_capacity as u64 {
            let event = match pair.0 {
                Condition::Conflict => conflict_hits
                    .iter()
                    .find(|h| h.node == pair.1)
                    .map(events::conflict_event),
                Condition::HighRiskModification => high_risk_hits
                    .iter()
                    .find(|h| h.node == pair.1)
                    .map(|h| events::high_risk_event(h.node, h.reason.clone())),
                Condition::Drift => drift_hits
                    .iter()
                    .find(|h| h.node == pair.1)
                    .map(events::drift_event),
                Condition::StaleSession => stale_hits
                    .iter()
                    .find(|h| h.node == pair.1)
                    .map(|h| events::stale_event(h.node, h.seconds_inactive)),
            };
            // `armed` is kept equal to `fresh`, so the hit is always found.
            if let Some(event) = event {
                cs.armed.insert(pair, events::emit(sender, event));
            }
        }
    }

    // 3a. Deferred survivor bumps from the last GC run (CONC-6/XP-10) —
    //     one chunk per cycle, and always to empty before the next run, so
    //     no concept ever carries two outstanding bumps.
    if !cs.gc_pending.is_empty() {
        let applied = {
            let mut g = graph.write();
            gc::drain_survivor_bumps(&mut g, &mut cs.gc_pending, params.gc_survivor_bump_chunk)
        };
        // NEW-2: a drain's own mutations must not be credited as session
        // mutations toward the next `gc_interval`. `bump_gc_survived` appends
        // exactly one `UpsertNode` per applied bump and the epoch bumps once
        // per appended mutation, so advancing the watermark by `applied`
        // cancels GC's own writes out of 3b's measure exactly.
        cs.last_gc_epoch = cs.last_gc_epoch.saturating_add(applied as u64);
    }

    // 3b. Periodic GC (spec §9): every `gc_interval` session mutations.
    if cs.gc_pending.is_empty() && epoch.saturating_sub(cs.last_gc_epoch) >= params.gc_interval {
        let outcome = {
            let mut g = graph.write();
            let outcome = gc::run(
                &mut g,
                gc::GcParams {
                    now,
                    // ALGO-4: GC's eviction ranking uses the session's own
                    // weights, not a second hardcoded default.
                    weights: *weights,
                    max_canonical_nodes: params.max_canonical_nodes,
                    max_survivor_bumps: params.gc_survivor_bump_chunk,
                    ..Default::default()
                },
            );
            // Spec §9 step 4 (XP-5): mirror collections into the owner's
            // index when it gave us one. Held WITH the graph lock so
            // recall's (graph, index) read pair sees an atomic publication
            // (P5 phase-close finding); lock order stays graph -> index.
            if let Some(index) = index.as_ref() {
                gc::sync_index(&outcome, &mut index.write());
            }
            outcome
        };
        // XP-5: the tier had zero logging, so the advisory
        // `max_concept_nodes` warning and the canonical-budget signal were
        // unobservable. Both ride `GcOutcome::warnings`.
        for warning in &outcome.warnings {
            tracing::warn!(target: "lambo::daemon::gc", "{warning}");
        }
        tracing::debug!(
            target: "lambo::daemon::gc",
            edges_removed = outcome.edges_removed.len(),
            concepts_collected = outcome.concepts_collected.len(),
            survivors = outcome.survivors.len(),
            survivors_deferred = outcome.survivors_pending.len(),
            canonical_count = outcome.canonical_count,
            canonical_over_budget = outcome.canonical_over_budget,
            epoch_after = outcome.epoch_after,
            "GC sweep complete"
        );
        cs.last_gc_epoch = outcome.epoch_after;
        cs.gc_pending = outcome.survivors_pending.clone();
        *last_gc.write() = Some(outcome);
    }

    // Last statement: a panicking cycle (CONC-4) does not count as completed,
    // so `Daemon::cycles` is a witness that the whole body ran (XP-6).
    cycles.fetch_add(1, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Graph;
    use crate::types::{
        AgentId, CanonizationStatus, Concept, ConceptType, Edge, EdgeType, Interaction, SessionId,
    };
    use chrono::{TimeZone, Utc};
    use uuid::Uuid;

    fn ts(m: i64) -> chrono::DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000 + m * 60, 0).unwrap()
    }

    fn sid() -> SessionId {
        SessionId::from("t4.1-daemon")
    }

    fn interaction(id: u64) -> Interaction {
        Interaction {
            id: NodeId(Uuid::from_u64_pair(0, id)),
            session_id: sid(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: Some("p".into()),
            previous_id: None,
            created_at: ts(0),
        }
    }

    fn concept(id: u64, origin: NodeId, content: &str) -> Concept {
        Concept {
            id: NodeId(Uuid::from_u64_pair(1, id)),
            session_id: sid(),
            content: content.into(),
            canonical_key: content.to_string(),
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

    /// A locked graph with one interaction and one concept (epoch 3:
    /// interaction node + concept node + Derives edge).
    fn locked_graph_with_one_concept() -> (Arc<RwLock<Graph>>, NodeId) {
        let mut g = Graph::new(sid());
        let i = interaction(1);
        let iid = i.id;
        g.insert_interaction(i).unwrap();
        let c = concept(1, iid, "user schema");
        let cid = c.id;
        g.insert_concept(c, iid).unwrap();
        (Arc::new(RwLock::new(g)), cid)
    }

    /// Wake the daemon and wait for the woken cycle to COMPLETE (XP-6).
    ///
    /// Every negative assertion in this suite ("nothing was published") uses
    /// this instead of sleeping: `Daemon::cycles` only advances after a full
    /// cycle body ran, so the assertion cannot pass vacuously because the cycle
    /// had not started yet. Under `start_paused` the wait is virtual-time, so it
    /// is also free.
    async fn wake_and_settle(daemon: &Daemon) {
        let before = daemon.cycles();
        daemon.wake();
        wait_until(|| daemon.cycles() > before).await;
    }

    /// Poll `cond` until true or a 2s timeout elapses (test helper).
    async fn wait_until(cond: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !cond() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("condition not met within 2s");
    }

    #[tokio::test(start_paused = true)]
    async fn epoch_change_triggers_rescore_via_wake() {
        // Tick of 1h so only the explicit wake drives cycles.
        let (graph, cid) = locked_graph_with_one_concept();
        let epoch0 = graph.read().epoch();
        assert_eq!(epoch0, 3);

        let daemon = Daemon::new(
            graph.clone(),
            ScoringWeights::default(),
            Duration::from_secs(3600),
        );
        let handle = daemon.spawn();

        // Warm-up rescore on the first cycle.
        wait_until(|| daemon.scores().epoch == epoch0).await;
        let warm = daemon.scores();
        assert_eq!(warm.ranked.len(), 1);
        assert_eq!(warm.ranked[0].item, cid);

        // Mutate the graph: add a second concept → epoch bumps.
        let c2 = {
            let iid = match graph.read().node(cid).unwrap() {
                crate::types::Node::Concept(c) => c.origin_interaction,
                _ => unreachable!(),
            };
            let c = concept(2, iid, "auth middleware");
            let id = c.id;
            graph.write().insert_concept(c, iid).unwrap();
            id
        };
        let epoch1 = graph.read().epoch();
        assert!(epoch1 > epoch0);

        // Explicit wake must trigger a rescore without waiting for the tick.
        daemon.wake();
        wait_until(|| daemon.scores().epoch == epoch1).await;
        let after = daemon.scores();
        assert_eq!(after.epoch, epoch1);
        assert_eq!(after.ranked.len(), 2);
        assert!(after.ranked.iter().any(|s| s.item == c2));

        handle.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn no_epoch_change_does_not_rescore() {
        let (graph, _) = locked_graph_with_one_concept();
        let epoch0 = graph.read().epoch();

        let daemon = Daemon::new(
            graph.clone(),
            ScoringWeights::default(),
            Duration::from_secs(3600),
        );
        let handle = daemon.spawn();
        wait_until(|| daemon.scores().epoch == epoch0).await;
        let before = daemon.scores();

        // Wake with no mutation: only the RESCORE is epoch-gated (finding 1)
        // — detection still runs, but with no condition transitions nothing
        // is published and the score table stays byte-identical.
        wake_and_settle(&daemon).await;
        let after = daemon.scores();
        assert_eq!(before, after, "no epoch change must not rescore");

        handle.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn cycle_completes_without_deadlock() {
        // Lock-discipline smoke: a cycle that takes the read lock, rescorees,
        // releases, then awaits must complete — never hold the lock across
        // .await (a violation would deadlock the write side below).
        let (graph, cid) = locked_graph_with_one_concept();
        let epoch0 = graph.read().epoch();

        let daemon = Daemon::new(
            graph.clone(),
            ScoringWeights::default(),
            Duration::from_millis(10),
        );
        let handle = daemon.spawn();
        wait_until(|| daemon.scores().epoch == epoch0).await;

        // Writer: grab the write lock, mutate, release — repeatedly while the
        // daemon runs. If the daemon held the read lock across .await, the
        // writer would starve and the timeout below would fire.
        let iid = match graph.read().node(cid).unwrap() {
            crate::types::Node::Concept(c) => c.origin_interaction,
            _ => unreachable!(),
        };
        for n in 2..=10u64 {
            let c = concept(n, iid, &format!("concept {n}"));
            graph.write().insert_concept(c, iid).unwrap();
            daemon.wake();
            wait_until(|| daemon.scores().epoch == graph.read().epoch()).await;
        }
        let final_table = daemon.scores();
        assert_eq!(final_table.ranked.len(), 10);
        for w in final_table.ranked.windows(2) {
            assert!(w[0].score >= w[1].score, "ranked list must stay sorted");
        }

        handle.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn abort_stops_the_loop() {
        let (graph, _) = locked_graph_with_one_concept();
        let daemon = Daemon::new(
            graph.clone(),
            ScoringWeights::default(),
            Duration::from_millis(10),
        );
        let handle = daemon.spawn();
        wait_until(|| daemon.scores().epoch == graph.read().epoch()).await;
        handle.abort();
        // Abort is our stop mechanism at this stage (graceful stop = P8).
        assert!(handle.await.is_err(), "aborted task must not complete Ok");
    }

    #[tokio::test(start_paused = true)]
    #[should_panic(expected = "spawn called twice")]
    async fn spawn_twice_panics() {
        let (graph, _) = locked_graph_with_one_concept();
        let daemon = Daemon::new(graph, ScoringWeights::default(), Duration::from_secs(3600));
        let first = daemon.spawn();
        std::mem::drop(first);
        // Second spawn must panic (single-loop guard), before any future exists.
        daemon.spawn();
    }
    // ------------------------------------------------------------------
    // T4.6 — event transport wiring: detectors publish, hot list maintains,
    // GC runs on the interval, receivers never block the loop.
    // ------------------------------------------------------------------

    fn wall_ts(secs_ago: i64) -> chrono::DateTime<Utc> {
        Utc::now() - chrono::Duration::seconds(secs_ago)
    }

    fn interaction_at(
        id: u64,
        prev: Option<u64>,
        agent: &str,
        at: chrono::DateTime<Utc>,
    ) -> Interaction {
        Interaction {
            id: NodeId(Uuid::from_u64_pair(0, id)),
            session_id: sid(),
            agent_id: AgentId::from(agent),
            prompt_text: Some("p".into()),
            previous_id: prev.map(|p| NodeId(Uuid::from_u64_pair(0, p))),
            created_at: at,
        }
    }

    fn concept_at(
        id: u64,
        origin: NodeId,
        agent: &str,
        content: &str,
        at: chrono::DateTime<Utc>,
    ) -> Concept {
        Concept {
            id: NodeId(Uuid::from_u64_pair(1, id)),
            session_id: sid(),
            content: content.into(),
            canonical_key: content.to_string(),
            concept_type: ConceptType::Entity,
            origin_interaction: origin,
            origin_agent: AgentId::from(agent),
            created_at: at,
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

    fn dep_edge_at(id: u64, source: NodeId, target: NodeId, at: chrono::DateTime<Utc>) -> Edge {
        Edge {
            id: NodeId(Uuid::from_u64_pair(3, id)),
            session_id: sid(),
            source,
            target,
            edge_type: EdgeType::Dependency,
            weight: 1.0,
            reinforcements: 1,
            created_at: at,
            last_reinforced: at,
        }
    }

    /// A two-agent graph with a live conflict on `c1`: agent-a's `Derives`
    /// edge and agent-b's fresh `Dependency` edge (5s before now) both touch
    /// it, so `conflict::detect` fires every cycle while the write stays in
    /// the 30s window. Returns `(graph, c1, dependency edge, agent-b
    /// interaction)` — the interaction id for planting extra agent-b writes.
    fn conflicted_graph() -> (Arc<RwLock<Graph>>, NodeId, NodeId, NodeId) {
        let mut g = Graph::new(sid());
        let i1 = interaction_at(1, None, "agent-a", wall_ts(60));
        let i1_id = i1.id;
        g.insert_interaction(i1).unwrap();
        let i2 = interaction_at(2, Some(1), "agent-b", wall_ts(30));
        let i2_id = i2.id;
        g.insert_interaction(i2).unwrap();
        let c1 = concept_at(1, i1_id, "agent-a", "shared node", wall_ts(60));
        let c1_id = c1.id;
        g.insert_concept(c1, i1_id).unwrap();
        let c2 = concept_at(2, i2_id, "agent-b", "writer b", wall_ts(30));
        let c2_id = c2.id;
        g.insert_concept(c2, i2_id).unwrap();
        let e = dep_edge_at(1, c2_id, c1_id, wall_ts(5));
        let e_id = e.id;
        g.upsert_edge(e).unwrap();
        (Arc::new(RwLock::new(g)), c1_id, e_id, i2_id)
    }

    #[cfg(feature = "fixtures")]
    #[tokio::test(start_paused = true)]
    async fn loop_emits_planted_conflict_from_rest_api_fixture() {
        use crate::fixtures;
        use crate::store::GraphStore;

        // Rebase the fixture onto wall clock so the caching layer's write
        // lands 5s before `anchor` — inside the loop's 30s conflict window.
        let anchor = Utc::now();
        let store =
            fixtures::load_store_relative("session-rest-api", anchor, Duration::from_secs(5))
                .unwrap();
        let snap = store
            .load_session(&SessionId::from("session-rest-api"))
            .await
            .unwrap();
        let g = Graph::from_snapshot(snap).unwrap();

        let daemon = Daemon::new(
            Arc::new(RwLock::new(g)),
            ScoringWeights::default(),
            Duration::from_secs(3600),
        )
        // Pin detection time to `anchor` so the planted 5s-ago write renders
        // exactly "5s" — the wall clock would make this flaky on slow CI.
        .with_clock(Arc::new(move || anchor));
        let mut rx = daemon.events();
        let handle = daemon.spawn();

        // Warm-up cycle: the planted conflict is the only event in the
        // session (no root goal → no drift; all writes < 1h old → no stale;
        // no fresh high-value writes → no high-risk).
        let evt = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("planted conflict within 2s")
            .unwrap();
        match evt {
            DaemonEvent::Conflict {
                node_id,
                agents,
                detail,
            } => {
                let caching = NodeId("f0000000-0000-4000-8000-000000001010".parse().unwrap());
                assert_eq!(
                    node_id, caching,
                    "the caching layer is the planted conflict"
                );
                assert_eq!(
                    agents,
                    vec![AgentId::from("agent-a"), AgentId::from("agent-b")]
                );
                assert!(
                    detail.contains("agent-a")
                        && detail.contains("agent-b")
                        && detail.contains("5s"),
                    "renderable detail: {detail}"
                );
            }
            other => panic!("first warm-up event must be the planted Conflict, got {other:?}"),
        }
        handle.abort();
    }

    #[cfg(feature = "fixtures")]
    #[tokio::test(start_paused = true)]
    async fn loop_emits_planted_drift_from_session_drift_fixture() {
        use crate::fixtures;
        use crate::store::GraphStore;

        let anchor = Utc::now();
        let store =
            fixtures::load_store_relative("session-drift", anchor, Duration::from_secs(5)).unwrap();
        let snap = store
            .load_session(&SessionId::from("session-drift"))
            .await
            .unwrap();
        let g = Graph::from_snapshot(snap).unwrap();
        let planted = g
            .concepts()
            .find(|c| c.content == "far budget concept")
            .expect("fixture must contain the planted drifted concept")
            .id;

        let daemon = Daemon::new(
            Arc::new(RwLock::new(g)),
            ScoringWeights::default(),
            Duration::from_secs(3600),
        );
        let mut rx = daemon.events();
        let handle = daemon.spawn();

        // Warm-up: single-agent fixture → no conflict; drift is the only event
        // kind (drift detection is clock-free, the chain is 6 hops). The planted
        // node sorts first by id; the fixture's isolated pair follows with the
        // no-path warning spec §9 requires (ALGO-5) — they are GC's step-3 food,
        // and warning once before GC's interval collects them is correct.
        let evt = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("planted drift within 2s")
            .unwrap();
        match evt {
            DaemonEvent::Drift {
                node_id,
                hops,
                detail,
            } => {
                assert_eq!(node_id, planted, "the planted drifted node");
                assert_eq!(hops, 6);
                assert!(detail.contains("6 hops"), "renderable detail: {detail}");
            }
            other => panic!("first warm-up event must be the planted Drift, got {other:?}"),
        }
        let mut no_path = 0;
        while let Ok(evt) = rx.try_recv() {
            match evt {
                DaemonEvent::Drift { hops, detail, .. } => {
                    assert_eq!(
                        hops,
                        drift::DRIFT_HOPS_NO_PATH_EVENT,
                        "unreachable sentinel: {detail}"
                    );
                    assert!(detail.contains("no path"), "renderable detail: {detail}");
                    no_path += 1;
                }
                other => panic!("the drift fixture must emit only Drift, got {other:?}"),
            }
        }
        assert_eq!(no_path, 2, "the fixture's isolated pair");
        handle.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn lagged_receiver_does_not_block_the_loop() {
        let (graph, c1_id, _, i2_id) = conflicted_graph();
        // Capacity 2 with a receiver that never drains: the daemon must stay
        // unblocked (scores keep advancing) and the consumer must see
        // `Lagged`, never a hang.
        let params = CycleParams {
            event_capacity: 2,
            ..Default::default()
        };
        let daemon = Daemon::with_params(
            graph.clone(),
            ScoringWeights::default(),
            Duration::from_secs(3600),
            params,
        );
        let mut rx = daemon.events();
        let handle = daemon.spawn();
        wait_until(|| daemon.scores().epoch == graph.read().epoch()).await;

        // Each mutation cycle introduces a NEW contested node — an agent-b
        // concept with a fresh Dependency edge from the agent-a concept c1 —
        // so emit-on-transition publishes exactly one distinct event per
        // cycle (a persisting conflict is emitted once, on entry, never
        // re-emitted). ~10 distinct events through a capacity-2 channel.
        for n in 2..=10u64 {
            let c = concept_at(n, i2_id, "agent-b", &format!("extra {n}"), wall_ts(3));
            let cid = c.id;
            graph.write().insert_concept(c, i2_id).unwrap();
            graph
                .write()
                .upsert_edge(dep_edge_at(100 + n, c1_id, cid, wall_ts(3)))
                .unwrap();
            daemon.wake();
            wait_until(|| daemon.scores().epoch == graph.read().epoch()).await;
        }

        // ~10 events through a capacity-2 channel → the loop never blocked
        // (proven above by scores advancing) and the consumer is lagged.
        match rx.recv().await {
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            other => panic!("expected Lagged, got {other:?}"),
        }
        // Re-synced to the newest retained window: the tail is Conflicts.
        match rx.recv().await {
            Ok(DaemonEvent::Conflict { .. }) => {}
            other => panic!("expected a Conflict in the retained window, got {other:?}"),
        }
        handle.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn dropped_receiver_does_not_break_the_loop() {
        let (graph, c1_id, _, i2_id) = conflicted_graph();
        let daemon = Daemon::new(
            graph.clone(),
            ScoringWeights::default(),
            Duration::from_secs(3600),
        );
        let rx = daemon.events();
        drop(rx); // zero receivers — every publish is discarded (spec §6.1)
        let handle = daemon.spawn();
        wait_until(|| daemon.scores().epoch == graph.read().epoch()).await;

        for n in 2..=6u64 {
            let c = concept_at(n, i2_id, "agent-b", &format!("extra {n}"), wall_ts(3));
            graph.write().insert_concept(c, i2_id).unwrap();
            daemon.wake();
            wait_until(|| daemon.scores().epoch == graph.read().epoch()).await;
        }

        // The loop survived every discarded send — and still maintained the
        // hot list (the detection side is independent of the consumer side).
        assert!(
            daemon.hot_list().read().contains(c1_id),
            "conflict entry must be on the hot list despite zero receivers"
        );
        handle.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn loop_maintains_hot_list_from_fresh_hits() {
        use crate::daemon::hotlist::{Condition, HotListPayload};

        let (graph, c1_id, dep_eid, _) = conflicted_graph();
        let daemon = Daemon::new(
            graph.clone(),
            ScoringWeights::default(),
            Duration::from_secs(3600),
        );
        let handle = daemon.spawn();
        wait_until(|| !daemon.hot_list().read().is_empty()).await;

        {
            let h = daemon.hot_list();
            let guard = h.read();
            let entry = guard
                .iter()
                .find(|e| e.node == c1_id)
                .expect("warm-up must put the conflicted node on the hot list");
            assert_eq!(entry.condition, Condition::Conflict);
            match &entry.payload {
                HotListPayload::Conflict { agents, .. } => assert_eq!(agents.len(), 2),
                other => panic!("expected Conflict payload, got {other:?}"),
            }
        }

        // Resolve the conflict (drop agent-b's fresh edge) → `(Conflict,
        // c1)` leaves the detected set → the next cycle's fresh-set sync
        // (retain_conditions) evicts the entry — no predicate involved.
        graph.write().remove_edge(dep_eid).unwrap();
        daemon.wake();
        wait_until(|| !daemon.hot_list().read().contains(c1_id)).await;
        handle.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn loop_runs_gc_every_gc_interval_mutations() {
        let (graph, cid) = locked_graph_with_one_concept();
        // Orphan the concept (drop its only Derives edge) so GC step 2
        // collects it — a deterministic GC-able node.
        let iid = match graph.read().node(cid).unwrap() {
            crate::types::Node::Concept(c) => c.origin_interaction,
            _ => unreachable!(),
        };
        let derives = {
            let g = graph.read();
            g.edge_between(iid, cid, EdgeType::Derives).unwrap().id
        };
        graph.write().remove_edge(derives).unwrap();

        // Interval 3: the warm-up epoch (4 mutations) already crosses it.
        let params = CycleParams {
            gc_interval: 3,
            ..Default::default()
        };
        let daemon = Daemon::with_params(
            graph.clone(),
            ScoringWeights::default(),
            Duration::from_secs(3600),
            params,
        );
        let handle = daemon.spawn();

        wait_until(|| graph.read().node(cid).is_none()).await;
        handle.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn loop_drains_deferred_survivor_bumps_to_convergence() {
        // CONC-6/XP-10: GC hands back the survivor bumps it deferred, and the
        // loop drains them a chunk per cycle. Every survivor must still end up
        // with exactly one bump for that run — chunking changes when, not which.
        let (graph, ids) = locked_graph_with_canonical_concepts(6);

        // gc_interval 3 → the warm-up epoch already crosses it; chunk 2 → the
        // run bumps 2 and defers 4, drained over the next two cycles.
        let params = CycleParams {
            gc_interval: 3,
            gc_survivor_bump_chunk: 2,
            ..Default::default()
        };
        let daemon = Daemon::with_params(
            graph.clone(),
            ScoringWeights::default(),
            Duration::from_millis(10),
            params,
        );
        let handle = daemon.spawn();

        let all_bumped = |want: i32| {
            let g = graph.read();
            ids.iter().all(|id| match g.node(*id) {
                Some(crate::types::Node::Concept(c)) => c.gc_survived >= want,
                _ => false,
            })
        };
        wait_until(|| all_bumped(1)).await;
        // And no survivor is double-counted: GC does not re-run while a drain
        // is outstanding, so every counter is exactly 1 — one run, one bump
        // each. (This asserted only `max - min <= 1` while GC still funded its
        // own next sweep off the drains; that is NEW-2's fixed point now, and
        // `idle_session_reaches_a_gc_fixed_point_after_the_bumps_drain` pins it
        // directly.)
        {
            let g = graph.read();
            let counts: Vec<i32> = ids
                .iter()
                .map(|id| match g.node(*id) {
                    Some(crate::types::Node::Concept(c)) => c.gc_survived,
                    _ => unreachable!(),
                })
                .collect();
            assert_eq!(
                counts,
                vec![1; 6],
                "chunking changes when a bump lands, never how many"
            );
        }
        handle.abort();
    }

    /// A locked graph with `n` Canonical concepts off one interaction. Canonical
    /// is protected, so every GC step spares them and the survivor set is
    /// exactly these `n` — the shape both survivor-bump tests need.
    fn locked_graph_with_canonical_concepts(n: u64) -> (Arc<RwLock<Graph>>, Vec<NodeId>) {
        let mut g = Graph::new(sid());
        let i = interaction(1);
        let iid = i.id;
        g.insert_interaction(i).unwrap();
        let mut ids: Vec<NodeId> = Vec::new();
        for k in 1..=n {
            let c = Concept {
                canonization_status: CanonizationStatus::Canonical,
                ..concept(k, iid, &format!("canonical {k}"))
            };
            ids.push(c.id);
            g.insert_concept(c, iid).unwrap();
        }
        (Arc::new(RwLock::new(g)), ids)
    }

    #[tokio::test(start_paused = true)]
    async fn idle_session_reaches_a_gc_fixed_point_after_the_bumps_drain() {
        // NEW-2: the chunked survivor bumps re-triggered GC. Bumps are
        // mutations, `epoch_after` covers only the in-`run` chunk, and the
        // deferred tail drained on later cycles — where each `UpsertNode` was
        // credited as a *session* mutation toward the next `gc_interval`. With
        // `survivors >= gc_interval + chunk` GC became fully self-sustaining on
        // an idle session: `gc_survived` climbed past canonization Stage 1's
        // `>= 3` gate with zero writes, and the epoch ran away from T5.4's
        // recall cache.
        //
        // Six survivors, interval 3, chunk 2 — pre-fix this loops forever
        // (sweep, drain, drain, sweep, …). Post-fix the drains cancel out and
        // an idle session has exactly one sweep.
        let (graph, ids) = locked_graph_with_canonical_concepts(6);
        let params = CycleParams {
            gc_interval: 3,
            gc_survivor_bump_chunk: 2,
            ..Default::default()
        };
        let daemon = Daemon::with_params(
            graph.clone(),
            ScoringWeights::default(),
            Duration::from_millis(10),
            params,
        );
        let handle = daemon.spawn();

        let survived = || -> Vec<i32> {
            let g = graph.read();
            ids.iter()
                .map(|id| match g.node(*id) {
                    Some(crate::types::Node::Concept(c)) => c.gc_survived,
                    _ => unreachable!("Canonical concepts are protected"),
                })
                .collect()
        };

        // The one sweep's bumps drain over the following cycles.
        wait_until(|| survived().iter().all(|&n| n == 1)).await;
        let settled_epoch = graph.read().epoch();
        let settled_cycles = daemon.cycles();

        // Now idle for many more cycles with ZERO session writes. Nothing may
        // move: no second sweep (`gc_survived` stays 1), and no mutation at all
        // (the epoch is the witness — a sweep's bumps would advance it).
        wait_until(|| daemon.cycles() >= settled_cycles + 40).await;
        assert_eq!(
            survived(),
            vec![1; 6],
            "an idle session must not sweep again — GC's own bumps must not \
             fund the next gc_interval"
        );
        assert_eq!(
            graph.read().epoch(),
            settled_epoch,
            "epoch must stabilize on an idle session (T5.4's recall cache keys \
             on it)"
        );
        handle.abort();
    }

    // ------------------------------------------------------------------
    // XP-5 / XP-7 / CONC-4 — observability, config plumbing, containment
    // ------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn last_gc_exposes_the_outcome_and_syncs_the_owners_index() {
        // XP-5: `GcOutcome` was dropped except `epoch_after`, so T6.4's
        // canonical-budget signal was unreachable and `sync_index` — spec §9
        // step 4 — had no production caller at all: a collected concept stayed
        // searchable in the owner's index indefinitely.
        let (graph, cid) = locked_graph_with_one_concept();
        // Orphan the concept so GC step 2 collects it deterministically.
        let iid = match graph.read().node(cid).unwrap() {
            crate::types::Node::Concept(c) => c.origin_interaction,
            _ => unreachable!(),
        };
        let derives = {
            let g = graph.read();
            g.edge_between(iid, cid, EdgeType::Derives).unwrap().id
        };
        graph.write().remove_edge(derives).unwrap();

        // The owner's index, pre-populated the way the P3 contract requires.
        let index = Arc::new(RwLock::new(InvertedIndex::new()));
        {
            let g = graph.read();
            let mut idx = index.write();
            for c in g.concepts() {
                idx.add(c);
            }
        }
        assert!(
            !index.read().search("user schema", 5).is_empty(),
            "the concept must start out searchable"
        );

        let params = CycleParams {
            gc_interval: 3,
            ..Default::default()
        };
        let daemon = Daemon::with_params(
            graph.clone(),
            ScoringWeights::default(),
            Duration::from_millis(10),
            params,
        )
        .with_index(index.clone());
        assert!(
            daemon.last_gc().is_none(),
            "no outcome before the first run"
        );
        let handle = daemon.spawn();

        wait_until(|| daemon.last_gc().is_some()).await;
        let outcome = daemon.last_gc().unwrap();
        assert!(
            outcome.concepts_collected.contains(&cid),
            "the orphan was collected: {outcome:?}"
        );
        assert_eq!(outcome.max_canonical_nodes, 1000, "the ceiling T6.4 reads");
        assert!(!outcome.canonical_over_budget);
        assert_eq!(outcome.epoch_after, graph.read().epoch());

        // Step 4: the index no longer serves the collected concept.
        wait_until(|| index.read().search("user schema", 5).is_empty()).await;
        handle.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_panicking_cycle_does_not_kill_the_loop() {
        // CONC-4: a panic inside the cycle used to kill the task silently —
        // scoring, events and GC stopped for the whole process lifetime with no
        // signal. The cycle body is contained, so the loop survives and the next
        // tick works normally.
        //
        // The panic is injected through the one seam the loop calls on every
        // cycle: the clock.
        use std::sync::atomic::AtomicUsize;

        let (graph, _) = locked_graph_with_one_concept();
        let calls = Arc::new(AtomicUsize::new(0));
        let clock: Clock = {
            let calls = calls.clone();
            Arc::new(move || {
                // Panic on the 1st cycle only; every later cycle is normal.
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    panic!("injected daemon cycle panic");
                }
                Utc::now()
            })
        };
        let daemon = Daemon::new(
            graph.clone(),
            ScoringWeights::default(),
            Duration::from_millis(10),
        )
        .with_clock(clock);
        let handle = daemon.spawn();

        // The loop must reach a later cycle and publish a score table — proof
        // it survived the panic rather than dying with the task.
        wait_until(|| daemon.scores().epoch == graph.read().epoch()).await;
        assert!(
            calls.load(Ordering::SeqCst) > 1,
            "the loop must run cycles after the panicking one"
        );
        assert!(!handle.is_finished(), "the task must still be alive");
        handle.abort();
    }

    #[test]
    fn cycle_params_come_from_config_with_no_duplicated_defaults() {
        // XP-7: `CycleParams::default()` duplicated Config's spec constants as
        // literals, so the two could drift silently. It is now derived.
        use crate::config::Config;

        assert_eq!(
            CycleParams::default(),
            CycleParams::from(&Config::default())
        );
        assert_eq!(Config::default().daemon_tick_interval, DAEMON_TICK_INTERVAL);

        // A non-default Config must reach the loop's params — including
        // `drift_threshold`, whose u32/usize split forced a cast at every use.
        let config = Config {
            hot_list_max: 7,
            conflict_recency_window: Duration::from_secs(11),
            drift_threshold: 9,
            gc_interval: 13,
            max_canonical_nodes: 17,
            daemon_tick_interval: Duration::from_millis(250),
            ..Default::default()
        };
        let params = CycleParams::from(&config);
        assert_eq!(params.hot_list_max, 7);
        assert_eq!(params.conflict_window, Duration::from_secs(11));
        assert_eq!(params.drift_threshold, 9);
        assert_eq!(params.gc_interval, 13);
        assert_eq!(params.max_canonical_nodes, 17);

        let (graph, _) = locked_graph_with_one_concept();
        let daemon = Daemon::from_config(graph, &config);
        assert_eq!(daemon.tick, Duration::from_millis(250));
        assert_eq!(daemon.params, params);
        assert_eq!(daemon.hot_list().read().max(), 7);
    }

    #[tokio::test(start_paused = true)]
    async fn wake_without_mutation_publishes_nothing() {
        let (graph, _, _, _) = conflicted_graph();
        let daemon = Daemon::new(
            graph.clone(),
            ScoringWeights::default(),
            Duration::from_secs(3600),
        );
        let mut rx = daemon.events();
        let handle = daemon.spawn();

        // Warm-up publishes exactly the one Conflict (consumed below).
        let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(first, DaemonEvent::Conflict { .. }));

        // A wake with no mutation: detection runs but the condition set is
        // unchanged → no transitions → nothing published (emit-on-transition).
        wake_and_settle(&daemon).await;
        match rx.try_recv() {
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}
            other => panic!("wake without a mutation must not publish, got {other:?}"),
        }
        handle.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn stale_fires_for_idle_session_after_window_elapses() {
        // T4.6 finding-1 regression: detection must run on EVERY tick, not
        // only on epoch change. A session with NO mutations ages a concept
        // past the stale window; pure time passing must fire
        // `DaemonEvent::Stale` (spec §9 background-daemon semantics, §6.1
        // Stale).
        use std::sync::atomic::{AtomicI64, Ordering as AtomicOrdering};

        let t0 = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let mut g = Graph::new(sid());
        let i1 = interaction_at(1, None, "agent-a", t0);
        let iid = i1.id;
        g.insert_interaction(i1).unwrap();
        let c = concept_at(1, iid, "agent-a", "idle concept", t0);
        let cid = c.id;
        g.insert_concept(c, iid).unwrap();
        let epoch0 = g.epoch();
        let graph = Arc::new(RwLock::new(g));

        // Controllable clock: the loop reads `now` from this cell; the test
        // advances it. Stale window shortened to 60s; the tick is short
        // because an idle session has no wake source — time alone must
        // drive the loop.
        let now_secs = Arc::new(AtomicI64::new(1_700_000_000));
        let clock: Clock = {
            let now_secs = now_secs.clone();
            Arc::new(move || {
                Utc.timestamp_opt(now_secs.load(AtomicOrdering::SeqCst), 0)
                    .unwrap()
            })
        };
        let params = CycleParams {
            stale_window: Duration::from_secs(60),
            ..Default::default()
        };
        let daemon = Daemon::with_params(
            graph.clone(),
            ScoringWeights::default(),
            Duration::from_millis(10),
            params,
        )
        .with_clock(clock);
        let mut rx = daemon.events();
        let handle = daemon.spawn();

        // Warm-up at t0: the concept's activity is fresh — and no other
        // detector fires — so nothing is published.
        wait_until(|| daemon.scores().epoch == epoch0).await;
        match rx.try_recv() {
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}
            other => panic!("no event before the window elapses, got {other:?}"),
        }

        // No mutation — only time passes. The next tick ages the concept
        // past the window: Stale ENTERS the detected set and is emitted once.
        now_secs.fetch_add(61, AtomicOrdering::SeqCst);
        let evt = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("idle staleness within 2s")
            .unwrap();
        match evt {
            DaemonEvent::Stale { node_id, detail } => {
                assert_eq!(node_id, cid);
                assert!(detail.contains("61s"), "renderable detail: {detail}");
            }
            other => panic!("expected Stale for the idle concept, got {other:?}"),
        }
        // The hot list mirrors the fresh hit.
        assert!(
            daemon.hot_list().read().contains(cid),
            "stale node must be on the hot list"
        );

        // Time keeps passing but the condition persists: no re-emission.
        now_secs.fetch_add(3600, AtomicOrdering::SeqCst);
        wake_and_settle(&daemon).await;
        match rx.try_recv() {
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}
            other => panic!("persisting stale must not re-emit, got {other:?}"),
        }
        handle.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn loop_emits_high_risk_for_fresh_write_to_canonical_node() {
        // Final-review finding 1a: the entered-gated HighRisk emit path (the
        // `high_risk_hits` loop in `run_loop`) + `events::high_risk_event`
        // mapper had no loop-level test. A Canonical node with a fresh
        // in-window write is a high-risk modification (spec §9 "high-risk
        // modification" hot-list condition; events.rs v0.1 rule): exactly one
        // `DaemonEvent::HighRisk` on transition, a HighRiskModification
        // hot-list entry with a renderable reason, and NO re-emit while the
        // condition persists (emit-on-transition, finding 3).
        use crate::daemon::hotlist::{Condition, HotListPayload};

        let t0 = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let mut g = Graph::new(sid());
        let i1 = interaction_at(1, None, "agent-a", t0);
        let iid = i1.id;
        g.insert_interaction(i1).unwrap();
        // The high-value node: Canonical (spec §10 Stage 3), fresh at t0 —
        // its Derives edge from `i1` is dated t0 too (graph.rs
        // insert_concept dates the edge at the concept's created_at).
        let c1 = Concept {
            canonization_status: CanonizationStatus::Canonical,
            blast_radius: Some(8),
            ..concept_at(1, iid, "agent-a", "canonical concept", t0)
        };
        let c1_id = c1.id;
        g.insert_concept(c1, iid).unwrap();
        // The modifying writer: a fresh in-window Dependency edge onto c1.
        let c2 = concept_at(2, iid, "agent-a", "modifier", t0);
        let c2_id = c2.id;
        g.insert_concept(c2, iid).unwrap();
        g.upsert_edge(dep_edge_at(1, c2_id, c1_id, t0)).unwrap();

        let graph = Arc::new(RwLock::new(g));
        // Clock pinned to t0: the t0 writes stay inside the 30s high-risk
        // window for the test's whole lifetime — the condition persists.
        let clock: Clock = Arc::new(move || t0);
        let daemon = Daemon::new(
            graph.clone(),
            ScoringWeights::default(),
            Duration::from_secs(3600),
        )
        .with_clock(clock);
        let mut rx = daemon.events();
        let handle = daemon.spawn();

        // Warm-up: the only transition is (HighRiskModification, c1) —
        // single agent (no conflict), no root goal (no drift), all activity
        // fresh (no stale).
        let evt = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("high-risk event within 2s")
            .unwrap();
        match evt {
            DaemonEvent::HighRisk { node_id, detail } => {
                assert_eq!(node_id, c1_id, "the fresh-written canonical node");
                assert!(
                    detail.contains("Canonical") && detail.contains("modified within 30s"),
                    "renderable detail: {detail}"
                );
            }
            other => panic!("first warm-up event must be the HighRisk, got {other:?}"),
        }

        // The hot list mirrors the fresh hit: a HighRiskModification entry
        // with the renderable reason.
        {
            let h = daemon.hot_list();
            let guard = h.read();
            let entry = guard
                .iter()
                .find(|e| e.node == c1_id)
                .expect("warm-up must put the canonical node on the hot list");
            assert_eq!(entry.condition, Condition::HighRiskModification);
            match &entry.payload {
                HotListPayload::HighRisk { reason } => {
                    assert!(reason.contains("Canonical"), "renderable reason: {reason}")
                }
                other => panic!("expected HighRisk payload, got {other:?}"),
            }
        }

        // The condition persists (the t0 write never leaves the 30s window):
        // wakes re-run detection but emit-on-transition publishes nothing —
        // the event fired once, on entry.
        for _ in 0..3 {
            wake_and_settle(&daemon).await;
        }
        match rx.try_recv() {
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}
            other => panic!("persisting high-risk must not re-emit, got {other:?}"),
        }
        handle.abort();
    }

    /// Parse the seconds out of a `stale_event` detail
    /// ("... untouched for <N>s").
    ///
    /// Gated with its only caller (NEW-1): CI's feature matrix runs
    /// `--no-default-features --features store-sqlite|store-cockroach` under
    /// `RUSTFLAGS="-D warnings"`, where an ungated helper whose sole use sits
    /// behind `fixtures` is a hard dead-code error, not a warning.
    #[cfg(feature = "fixtures")]
    fn stale_seconds(detail: &str) -> u64 {
        detail
            .trim_end_matches('s')
            .rsplit(' ')
            .next()
            .unwrap_or_default()
            .parse()
            .unwrap_or_else(|_| panic!("detail must render seconds-inactive: {detail:?}"))
    }

    #[cfg(feature = "fixtures")]
    #[tokio::test(start_paused = true)]
    async fn loop_emits_one_session_stale_from_rest_api_fixture_after_writes_age_out() {
        // Final-review finding 1a: staleness had only a synthetic-clock loop
        // test. This one is fixture-driven: rebase session-rest-api so its
        // newest write lands 2h before `anchor` — past the 1h STALE_WINDOW
        // and far outside the 30s conflict/high-risk windows. The session as a
        // whole is stale, so the warm-up cycle must emit exactly ONE Stale
        // (CONC-2: per session, not per concept — this asserted 22 before) and
        // nothing else: no Conflict (all writes are 2h old, outside
        // conflict_recency_window), no Drift (the session has no root goal —
        // drift.rs: no goal nodes → no hits), no HighRisk (no fresh in-window
        // writes — user schema is Canonical/blast-radius 8 but its write is 2h
        // old).
        use crate::fixtures;
        use crate::store::GraphStore;

        let anchor = Utc::now();
        let store =
            fixtures::load_store_relative("session-rest-api", anchor, Duration::from_secs(7200))
                .unwrap();
        let snap = store
            .load_session(&SessionId::from("session-rest-api"))
            .await
            .unwrap();
        let g = Graph::from_snapshot(snap).unwrap();
        assert_eq!(
            g.concepts().count(),
            22,
            "fixture must keep its 22 concepts"
        );

        let daemon = Daemon::new(
            Arc::new(RwLock::new(g)),
            ScoringWeights::default(),
            Duration::from_secs(3600),
        )
        // Pin detection time to `anchor` so the 2h-rebased writes render
        // stable seconds-inactive (the wall clock would make this flaky).
        .with_clock(Arc::new(move || anchor));
        let mut rx = daemon.events();
        let handle = daemon.spawn();

        let evt = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("session stale within 2s")
            .unwrap();
        let anchor_node = match evt {
            DaemonEvent::Stale { node_id, detail } => {
                assert!(
                    detail.contains("untouched for"),
                    "renderable detail: {detail}"
                );
                assert!(
                    stale_seconds(&detail) >= 7200,
                    "2h rebase ⇒ the newest write is ≥ 2h old, got {detail}"
                );
                node_id
            }
            other => panic!("aged-out fixture must emit one session Stale, got {other:?}"),
        };
        // One event for the whole session — 22 concepts, one Stale.
        match rx.try_recv() {
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}
            other => panic!("staleness is per session: expected exactly one, got {other:?}"),
        }
        // The hot list mirrors it: one entry, on the anchor node.
        {
            let h = daemon.hot_list();
            let guard = h.read();
            assert_eq!(guard.len(), 1, "one hot-list entry for the stale session");
            assert!(guard.contains(anchor_node));
        }
        handle.abort();
    }

    // ------------------------------------------------------------------
    // XP-4 / CONC-2 / CONC-3 — the event seam
    // ------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn non_daemon_caller_can_emit_canonized_on_the_daemon_channel() {
        // XP-4: P6's documented seam (`events::emit_canonized`) needs the
        // broadcast Sender. Before `Daemon::event_sender()` no public path to it
        // existed anywhere in the crate, so P6 could not reach the channel
        // without owning the daemon's private field. This test is the seam: a
        // caller that is *not* the daemon loop emits, and a `Daemon::events()`
        // subscriber receives.
        use crate::types::CanonizationEvent;

        let (graph, cid) = locked_graph_with_one_concept();
        let daemon = Daemon::new(
            graph.clone(),
            ScoringWeights::default(),
            Duration::from_secs(3600),
        );
        let mut rx = daemon.events();

        // The "P6 evaluator": holds only the sender, never the daemon.
        let sender = daemon.event_sender();
        let event = CanonizationEvent {
            id: NodeId(Uuid::from_u64_pair(9, 1)),
            session_id: sid(),
            node_id: cid,
            from_status: CanonizationStatus::None,
            to_status: CanonizationStatus::Candidate,
            blast_radius: Some(3),
            occurred_at: ts(0),
            last_demotion_time: None,
        };
        assert_eq!(sender.emitted_total(), 0, "nothing published yet");
        events::emit_canonized(&sender, event.clone());

        let got = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("Canonized within 2s")
            .unwrap();
        match got {
            DaemonEvent::Canonized { event: e } => assert_eq!(e, event),
            other => panic!("expected Canonized, got {other:?}"),
        }
        // NEW-3: the seam is an `events::EventSender`, not a raw broadcast
        // Sender — P6's send advanced the channel's SHARED publication counter,
        // which is what the loop's re-arm measures ring eviction against. A
        // clone taken later reads the same count.
        assert_eq!(sender.emitted_total(), 1);
        assert_eq!(daemon.event_sender().emitted_total(), 1);
        // The daemon's own handle keeps the channel open after the clone drops.
        drop(sender);
        assert_eq!(daemon.event_sender().receiver_count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn external_publisher_flood_cannot_permanently_evict_a_held_conflict() {
        // NEW-3: `event_sender()` used to hand out a raw `broadcast::Sender`
        // clone. Its sends advanced the ring but not the loop's emission count,
        // so re-arm (CONC-2) could not see that the held Conflict's event had
        // been pushed out — leaving CONC-2 only partially closed. Probe on the
        // pre-fix code: 300 external `Canonized` sends + a continuously-held
        // Conflict + 601 daemon cycles delivered **0** Conflict events.
        //
        // Capacity 4, flood 10 (> capacity, so the warm-up Conflict is certainly
        // gone from the retained window), and the subscriber drains LATE — only
        // after the flood — so the only way it can ever see the Conflict is the
        // re-arm path counting the external sends.
        use crate::types::CanonizationEvent;

        let (graph, c1_id, _, _) = conflicted_graph();
        let params = CycleParams {
            event_capacity: 4,
            ..Default::default()
        };
        let daemon = Daemon::with_params(
            graph.clone(),
            ScoringWeights::default(),
            Duration::from_secs(3600),
            params,
        );
        let mut rx = daemon.events();
        let handle = daemon.spawn();

        // Warm-up publishes the Conflict (stamp 1). Wait on the hot list, not on
        // `rx` — draining would defeat the point.
        wait_until(|| daemon.hot_list().read().contains(c1_id)).await;

        // The "P6 evaluator" floods the channel it shares with the daemon.
        let sender = daemon.event_sender();
        for n in 0..10u64 {
            events::emit_canonized(
                &sender,
                CanonizationEvent {
                    id: NodeId(Uuid::from_u64_pair(9, n)),
                    session_id: sid(),
                    node_id: c1_id,
                    from_status: CanonizationStatus::None,
                    to_status: CanonizationStatus::Candidate,
                    blast_radius: Some(3),
                    occurred_at: ts(0),
                    last_demotion_time: None,
                },
            );
        }
        assert!(
            sender.emitted_total() >= 11,
            "the warm-up Conflict plus 10 external sends must all be counted, \
             got {}",
            sender.emitted_total()
        );

        // Cycles with no graph change: nothing ENTERS the condition set, so the
        // only possible publication is the re-arm of the still-held Conflict.
        let mut saw_conflict = false;
        for _ in 0..8 {
            wake_and_settle(&daemon).await;
            while let Ok(evt) = rx.try_recv() {
                if let DaemonEvent::Conflict { node_id, .. } = evt {
                    if node_id == c1_id {
                        saw_conflict = true;
                    }
                }
            }
            if saw_conflict {
                break;
            }
        }
        assert!(
            saw_conflict,
            "an external publisher's flood must not permanently evict a held \
             Conflict — every publisher counts, so re-arm still fires"
        );
        handle.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn late_subscriber_misses_the_warm_up_condition_set() {
        // CONC-3: pins the documented P8 ordering obligation. The warm-up cycle
        // publishes the whole restored condition set — including the demo's
        // planted Conflict — and `broadcast` delivers only what is sent after a
        // receiver exists. Emission is on transition, so nothing re-publishes
        // for a late subscriber's benefit. P8 must subscribe BEFORE spawn; see
        // `Daemon::events`.
        let (graph, c1_id, _, _) = conflicted_graph();
        let daemon = Daemon::new(
            graph.clone(),
            ScoringWeights::default(),
            Duration::from_secs(3600),
        );
        let handle = daemon.spawn();
        // The warm-up cycle has run once the hot list carries its conflict.
        wait_until(|| daemon.hot_list().read().contains(c1_id)).await;

        // Subscribe *after* the warm-up, then drive several more cycles: the
        // condition still holds, so no transition fires and nothing arrives.
        let mut late = daemon.events();
        for _ in 0..3 {
            wake_and_settle(&daemon).await;
        }
        match late.try_recv() {
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}
            other => panic!("a late subscriber must not see the warm-up set, got {other:?}"),
        }
        handle.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn held_condition_is_re_emitted_once_the_ring_wraps_past_it() {
        // CONC-2: emit-on-transition alone loses an event permanently — the
        // transition is recorded whether or not any consumer got it, so an
        // event evicted from the ring while its condition still holds is never
        // re-published. The demo's Conflict is exactly such an event.
        //
        // Capacity 2, and the consumer drains fully after every cycle so no
        // event is ever lost to lag — the only way c1's Conflict can reappear
        // is the re-arm path. Two later events push its emission out of the
        // 2-slot retained window; the next cycle must re-publish it.
        let (graph, c1_id, _, i2_id) = conflicted_graph();
        let params = CycleParams {
            event_capacity: 2,
            ..Default::default()
        };
        let daemon = Daemon::with_params(
            graph.clone(),
            ScoringWeights::default(),
            Duration::from_secs(3600),
            params,
        );
        let mut rx = daemon.events();
        let handle = daemon.spawn();

        /// Everything currently queued, as `(is_conflict_on_target, node)`.
        fn drain(rx: &mut broadcast::Receiver<DaemonEvent>) -> Vec<NodeId> {
            let mut out = Vec::new();
            while let Ok(evt) = rx.try_recv() {
                if let DaemonEvent::Conflict { node_id, .. } = evt {
                    out.push(node_id);
                }
            }
            out
        }

        // Warm-up: the c1 conflict, emitted on entry.
        let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("warm-up conflict within 2s")
            .unwrap();
        assert!(matches!(
            first,
            DaemonEvent::Conflict { node_id, .. } if node_id == c1_id
        ));
        assert!(drain(&mut rx).is_empty(), "warm-up emits exactly one event");

        // Two more cycles, each introducing one new contested node: one
        // entering event per cycle, drained immediately. After the second, two
        // events have been published since c1's, so its slot is gone.
        let mut seen_c1_again = false;
        for n in 2..=3u64 {
            let c = concept_at(n, i2_id, "agent-b", &format!("extra {n}"), wall_ts(3));
            let cid = c.id;
            graph.write().insert_concept(c, i2_id).unwrap();
            graph
                .write()
                .upsert_edge(dep_edge_at(100 + n, c1_id, cid, wall_ts(3)))
                .unwrap();
            wake_and_settle(&daemon).await;
            if drain(&mut rx).contains(&c1_id) {
                seen_c1_again = true;
            }
        }

        assert!(
            seen_c1_again,
            "a still-held Conflict whose event left the retained window must be \
             re-published (CONC-2 re-arm), not lost forever"
        );
        handle.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn entering_conditions_publish_highest_severity_first() {
        // CONC-2: a burst must put the most actionable event first for a
        // consumer draining in order — Conflict, HighRisk, Drift, Stale
        // (`Condition::severity`). Pre-fix the order was Conflict, Drift,
        // Stale, HighRisk: the hazard came last.
        //
        // The graph plants a Conflict and a HighRisk that enter together: a
        // Canonical, high-blast-radius node written by a second agent. The two
        // interactions carry distinct timestamps so the edge attributes cleanly
        // to agent-b (ALGO-3) and only `contested` is contested.
        let t0 = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let t1 = t0 + chrono::Duration::seconds(5);
        let now = t0 + chrono::Duration::seconds(10);
        let mut g = Graph::new(sid());
        let i1 = interaction_at(1, None, "agent-a", t0);
        let i1_id = i1.id;
        g.insert_interaction(i1).unwrap();
        let i2 = interaction_at(2, Some(1), "agent-b", t1);
        let i2_id = i2.id;
        g.insert_interaction(i2).unwrap();
        let contested = Concept {
            canonization_status: CanonizationStatus::Canonical,
            blast_radius: Some(8),
            ..concept_at(1, i1_id, "agent-a", "user schema", t0)
        };
        let contested_id = contested.id;
        g.insert_concept(contested, i1_id).unwrap();
        let writer = concept_at(2, i2_id, "agent-b", "cache layer", t1);
        let writer_id = writer.id;
        g.insert_concept(writer, i2_id).unwrap();
        g.upsert_edge(dep_edge_at(1, writer_id, contested_id, t1))
            .unwrap();

        let daemon = Daemon::new(
            Arc::new(RwLock::new(g)),
            ScoringWeights::default(),
            Duration::from_secs(3600),
        )
        .with_clock(Arc::new(move || now));
        let mut rx = daemon.events();
        let handle = daemon.spawn();

        // Both conditions enter on the warm-up cycle; Conflict must be first.
        let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("warm-up burst within 2s")
            .unwrap();
        assert!(
            matches!(first, DaemonEvent::Conflict { node_id, .. } if node_id == contested_id),
            "Conflict outranks HighRisk in the burst, got {first:?}"
        );
        let second = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("second event within 2s")
            .unwrap();
        assert!(
            matches!(second, DaemonEvent::HighRisk { node_id, .. } if node_id == contested_id),
            "HighRisk follows Conflict, got {second:?}"
        );
        handle.abort();
    }

    /// The P5 entry reproduces T5.3's golden context block end to end: same
    /// fixture snapshot, same planted T4.3-shaped conflict entry, rescored
    /// table, pinned clock — through `Daemon::recall` (the actual entry, not
    /// the bespoke pipeline). Also proves cache hit + epoch invalidation
    /// through the entry.
    #[cfg(feature = "fixtures")]
    #[tokio::test]
    async fn recall_entry_reproduces_context_golden() {
        use crate::config::RecallWeights;
        use crate::daemon::score::rescore;
        use crate::fixtures;
        use crate::recall::cache::RecallCache;

        // T5.3's pinned clock: base + 60 minutes (its ts(60)).
        let now = Utc.timestamp_opt(1_752_000_000, 0).unwrap() + chrono::Duration::minutes(60);
        let snap = fixtures::load_snapshot("session-rest-api").unwrap();
        let graph = Arc::new(RwLock::new(Graph::from_snapshot(snap.clone()).unwrap()));
        let index = Arc::new(RwLock::new(InvertedIndex::from_snapshot(&snap)));
        let daemon = Daemon::new(
            graph.clone(),
            ScoringWeights::default(),
            Duration::from_secs(3600),
        )
        .with_clock(Arc::new(move || now))
        .with_index(index);
        daemon.scores.write().ranked = rescore(&graph.read(), &ScoringWeights::default());
        daemon.scores.write().epoch = graph.read().epoch();

        // Plant the T4.3-shaped conflict on "user schema" (001001), written
        // 11s before `now` (30s window) — identical to T5.3's golden test.
        let us = NodeId("f0000000-0000-4000-8000-000000001001".parse().unwrap());
        let agents = vec![AgentId::from("agent-a"), AgentId::from("agent-b")];
        let writer = AgentId::from("agent-a");
        let write_at = now - chrono::Duration::seconds(11);
        let entry = crate::daemon::hotlist::HotListEntry::new(
            us,
            Condition::Conflict,
            crate::daemon::hotlist::HotListPayload::Conflict {
                agents: agents.clone(),
                writer: writer.clone(),
                seconds_ago: 999, // stale sentinel: revalidate must rebuild
            },
            move |_, now| {
                let secs = (now - write_at).num_seconds();
                if (0..=30).contains(&secs) {
                    Some(crate::daemon::hotlist::HotListPayload::Conflict {
                        agents: agents.clone(),
                        writer: writer.clone(),
                        seconds_ago: secs as u64,
                    })
                } else {
                    None
                }
            },
        );
        let _ = daemon.hot.write().insert(entry);

        let store = fixtures::load_store("session-rest-api").unwrap();
        let mut cache = RecallCache::new();
        let query = RecallQuery {
            query: "update user schema".into(),
            top_k: 5,
            max_tokens: 500,
            traversal_depth: 2,
        };
        let session = SessionId::from("session-rest-api");
        let golden = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/fixtures/recall-context-golden.txt"
        ))
        .expect("golden context fixture present");

        let result = daemon
            .recall(
                &session,
                query.clone(),
                &store,
                None,
                RecallWeights::default(),
                &mut cache,
            )
            .await;
        assert_eq!(
            result.context, golden,
            "entry must reproduce the golden block"
        );
        assert_eq!(cache.len(), 1, "first call populates the cache");

        // Cache hit: identical second call does not grow the cache.
        let again = daemon
            .recall(
                &session,
                query.clone(),
                &store,
                None,
                RecallWeights::default(),
                &mut cache,
            )
            .await;
        assert_eq!(again.context, golden);
        assert_eq!(cache.len(), 1, "cache hit: no new key inserted");

        // Epoch invalidation: any mutation bumps the epoch -> miss -> new key.
        // The new interaction must link to the fixture's chain tail
        // (insert_interaction enforces previous_id = current tail).
        let tail = graph
            .read()
            .interactions()
            .max_by_key(|i| i.created_at)
            .expect("fixture has interactions")
            .id;
        graph
            .write()
            .insert_interaction(Interaction {
                id: NodeId::new(),
                session_id: session.clone(),
                agent_id: AgentId::from("agent-a"),
                prompt_text: None,
                previous_id: Some(tail),
                created_at: now,
            })
            .unwrap();
        // The loop's rescore catches up to the new epoch within one tick; the
        // entry's cache guard only skips caching while scores lag, so once
        // caught up the new epoch key is stored.
        daemon.scores.write().ranked = rescore(&graph.read(), &ScoringWeights::default());
        daemon.scores.write().epoch = graph.read().epoch();
        let _ = daemon
            .recall(
                &session,
                query,
                &store,
                None,
                RecallWeights::default(),
                &mut cache,
            )
            .await;
        assert_eq!(
            cache.len(),
            2,
            "epoch bump invalidates: new key inserted on miss"
        );
    }

    /// A reservation transition (RAM-local: no Mutation kind exists) bumps
    /// the epoch, so a same-query recall misses and re-renders the
    /// reservation line (P5 phase-close finding).
    #[cfg(feature = "fixtures")]
    #[tokio::test]
    async fn recall_reservation_transition_invalidates_cache_and_renders() {
        use crate::config::RecallWeights;
        use crate::daemon::score::rescore;
        use crate::fixtures;
        use crate::recall::cache::RecallCache;
        use crate::types::Reservation;

        let now = Utc.timestamp_opt(1_752_000_000, 0).unwrap() + chrono::Duration::minutes(60);
        let snap = fixtures::load_snapshot("session-rest-api").unwrap();
        let graph = Arc::new(RwLock::new(Graph::from_snapshot(snap.clone()).unwrap()));
        let index = Arc::new(RwLock::new(InvertedIndex::from_snapshot(&snap)));
        let daemon = Daemon::new(
            graph.clone(),
            ScoringWeights::default(),
            Duration::from_secs(3600),
        )
        .with_clock(Arc::new(move || now))
        .with_index(index);
        daemon.scores.write().ranked = rescore(&graph.read(), &ScoringWeights::default());
        daemon.scores.write().epoch = graph.read().epoch();

        let store = fixtures::load_store("session-rest-api").unwrap();
        let mut cache = RecallCache::new();
        let session = SessionId::from("session-rest-api");
        let query = RecallQuery {
            query: "update user schema".into(),
            top_k: 5,
            max_tokens: 500,
            traversal_depth: 2,
        };
        let _ = daemon
            .recall(
                &session,
                query.clone(),
                &store,
                None,
                RecallWeights::default(),
                &mut cache,
            )
            .await;
        assert_eq!(cache.len(), 1);

        // Reserve a node in the expanded set (user schema, 001001).
        let us = NodeId("f0000000-0000-4000-8000-000000001001".parse().unwrap());
        graph.write().set_reservation(Reservation {
            session_id: session.clone(),
            node_id: us,
            agent_id: AgentId::from("agent-a"),
            expires_at: now + chrono::Duration::seconds(60),
        });

        daemon.scores.write().ranked = rescore(&graph.read(), &ScoringWeights::default());
        daemon.scores.write().epoch = graph.read().epoch();
        let with_res = daemon
            .recall(
                &session,
                query,
                &store,
                None,
                RecallWeights::default(),
                &mut cache,
            )
            .await;
        assert_eq!(
            cache.len(),
            2,
            "reservation transition bumps epoch -> cache miss"
        );
        assert!(
            with_res.context.contains("Reserved by agent-a")
                || with_res
                    .warnings
                    .iter()
                    .any(|w| w.contains("Reserved by agent-a")),
            "reservation line rendered; warnings: {:?}",
            with_res.warnings
        );
    }

    /// Cache hits re-render time-sensitive output: with a mutable clock and
    /// no epoch change, a live conflict entry's age refreshes and a lapsed
    /// window drops the warning line (spec §9 "conditions re-validated on
    /// each recall()" — P5 phase-close finding).
    #[cfg(feature = "fixtures")]
    #[tokio::test]
    async fn recall_cache_hit_rerenders_fresh_warning_lines() {
        use crate::config::RecallWeights;
        use crate::daemon::score::rescore;
        use crate::fixtures;
        use crate::recall::cache::RecallCache;
        use std::sync::Mutex;

        let base = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
        let clock_now = Arc::new(Mutex::new(base));
        let snap = fixtures::load_snapshot("session-rest-api").unwrap();
        let graph = Arc::new(RwLock::new(Graph::from_snapshot(snap.clone()).unwrap()));
        let index = Arc::new(RwLock::new(InvertedIndex::from_snapshot(&snap)));
        let daemon = Daemon::new(
            graph.clone(),
            ScoringWeights::default(),
            Duration::from_secs(3600),
        )
        .with_clock(Arc::new({
            let c = clock_now.clone();
            move || *c.lock().unwrap()
        }))
        .with_index(index);
        daemon.scores.write().ranked = rescore(&graph.read(), &ScoringWeights::default());
        daemon.scores.write().epoch = graph.read().epoch();

        // Live conflict entry on user schema: written 11s before `base`,
        // 30s window.
        let us = NodeId("f0000000-0000-4000-8000-000000001001".parse().unwrap());
        let agents = vec![AgentId::from("agent-a"), AgentId::from("agent-b")];
        let write_at = base - chrono::Duration::seconds(11);
        let entry = crate::daemon::hotlist::HotListEntry::new(
            us,
            Condition::Conflict,
            crate::daemon::hotlist::HotListPayload::Conflict {
                agents: agents.clone(),
                writer: AgentId::from("agent-a"),
                seconds_ago: 999, // stale sentinel: revalidate must rebuild
            },
            move |_, now| {
                let secs = (now - write_at).num_seconds();
                if (0..=30).contains(&secs) {
                    Some(crate::daemon::hotlist::HotListPayload::Conflict {
                        agents: agents.clone(),
                        writer: AgentId::from("agent-a"),
                        seconds_ago: secs as u64,
                    })
                } else {
                    None
                }
            },
        );
        let _ = daemon.hot.write().insert(entry);

        let store = fixtures::load_store("session-rest-api").unwrap();
        let mut cache = RecallCache::new();
        let session = SessionId::from("session-rest-api");
        let query = RecallQuery {
            query: "update user schema".into(),
            top_k: 5,
            max_tokens: 500,
            traversal_depth: 2,
        };

        let first = daemon
            .recall(
                &session,
                query.clone(),
                &store,
                None,
                RecallWeights::default(),
                &mut cache,
            )
            .await;
        assert!(
            first.context.contains("wrote to it 11 seconds ago"),
            "age at read time: {}",
            first.context
        );
        assert_eq!(cache.len(), 1);

        // No epoch change; clock advances 5s -> cache HIT, age re-rendered.
        *clock_now.lock().unwrap() = base + chrono::Duration::seconds(5);
        let aged = daemon
            .recall(
                &session,
                query.clone(),
                &store,
                None,
                RecallWeights::default(),
                &mut cache,
            )
            .await;
        assert_eq!(cache.len(), 1, "same epoch -> cache hit, no new key");
        assert!(
            aged.context.contains("wrote to it 16 seconds ago"),
            "age refreshed on cache hit: {}",
            aged.context
        );

        // Window lapses (age 41s > 30s) -> warning line drops, still a hit.
        *clock_now.lock().unwrap() = base + chrono::Duration::seconds(30);
        let lapsed = daemon
            .recall(
                &session,
                query,
                &store,
                None,
                RecallWeights::default(),
                &mut cache,
            )
            .await;
        assert_eq!(cache.len(), 1, "same epoch -> cache hit");
        assert!(
            !lapsed.context.contains("wrote to it"),
            "lapsed entry's warning dropped: {}",
            lapsed.context
        );
    }

    /// The rescore-lag guard (phase-close P5-3): a compute whose daemon scores
    /// lag the graph epoch is rendered but NOT cached; once the loop's
    /// rescore catches up, the next call caches the fresh-epoch key (R2-1:
    /// the skip branch had no direct test).
    #[cfg(feature = "fixtures")]
    #[tokio::test]
    async fn recall_rescore_lag_guard_skips_cache_insert_while_scores_lag() {
        use crate::config::RecallWeights;
        use crate::daemon::score::rescore;
        use crate::fixtures;
        use crate::recall::cache::RecallCache;

        let now = Utc.timestamp_opt(1_752_000_000, 0).unwrap() + chrono::Duration::minutes(60);
        let snap = fixtures::load_snapshot("session-rest-api").unwrap();
        let graph = Arc::new(RwLock::new(Graph::from_snapshot(snap.clone()).unwrap()));
        let index = Arc::new(RwLock::new(InvertedIndex::from_snapshot(&snap)));
        let daemon = Daemon::new(
            graph.clone(),
            ScoringWeights::default(),
            Duration::from_secs(3600),
        )
        .with_clock(Arc::new(move || now))
        .with_index(index);
        daemon.scores.write().ranked = rescore(&graph.read(), &ScoringWeights::default());
        daemon.scores.write().epoch = graph.read().epoch();

        let store = fixtures::load_store("session-rest-api").unwrap();
        let mut cache = RecallCache::new();
        let session = SessionId::from("session-rest-api");
        let query = RecallQuery {
            query: "update user schema".into(),
            top_k: 5,
            max_tokens: 500,
            traversal_depth: 2,
        };

        let first = daemon
            .recall(
                &session,
                query.clone(),
                &store,
                None,
                RecallWeights::default(),
                &mut cache,
            )
            .await;
        assert!(!first.context.is_empty());
        assert_eq!(cache.len(), 1, "initial compute cached");

        // Mutation bumps the epoch; the loop's rescore has NOT caught up
        // (scores.epoch still the old one). The compute renders against the
        // lagged table but must NOT be cached under the new epoch key.
        let tail = graph
            .read()
            .interactions()
            .max_by_key(|i| i.created_at)
            .expect("fixture has interactions")
            .id;
        graph
            .write()
            .insert_interaction(Interaction {
                id: NodeId::new(),
                session_id: session.clone(),
                agent_id: AgentId::from("agent-a"),
                prompt_text: None,
                previous_id: Some(tail),
                created_at: now,
            })
            .unwrap();
        let lagged = daemon
            .recall(
                &session,
                query.clone(),
                &store,
                None,
                RecallWeights::default(),
                &mut cache,
            )
            .await;
        assert_eq!(
            cache.len(),
            1,
            "lagged-scores compute is NOT cached (P5-3 guard)"
        );
        assert!(!lagged.context.is_empty(), "output still rendered");

        // Rescore catches up; the next call stores the fresh-epoch key.
        daemon.scores.write().ranked = rescore(&graph.read(), &ScoringWeights::default());
        daemon.scores.write().epoch = graph.read().epoch();
        let caught_up = daemon
            .recall(
                &session,
                query,
                &store,
                None,
                RecallWeights::default(),
                &mut cache,
            )
            .await;
        assert_eq!(
            cache.len(),
            2,
            "after rescore the fresh-epoch key is cached"
        );
        assert!(!caught_up.context.is_empty());
    }
}
