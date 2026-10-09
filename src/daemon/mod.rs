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
pub mod access;
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
use crate::recall::detail::{Annotation, AnnotationKind, DetailedRecall};
use crate::recall::dispatch::{self, RecallKind};
use crate::recall::{assemble, candidates, expand};
use crate::types::{DaemonEvent, NodeId, RecallQuery, RecallResult, Scored, SessionId, StoreError};

/// Default daemon poll interval (XP-7).
///
/// The spec fixes no value; 1s matches `backend_flush_interval` so the daemon
/// and the write-behind loop age state on the same beat, and it bounds the
/// window in which a hot-list entry can lag the graph to one second. Mirrored
/// by [`Config::daemon_tick_interval`], which is what P8 threads in.
pub const DAEMON_TICK_INTERVAL: Duration = Duration::from_secs(1);

/// The daemon's score table. Read-side data shared with recall and
/// canonization, so it lives in [`crate::types::ScoreTable`]; re-exported here
/// so `daemon::ScoreTable` stays valid.
pub use crate::types::ScoreTable;

/// The epoch-stable recall pipeline artifact: phase-1 candidates plus the
/// phase-2 expansion. Cached as a unit; assembly and rendering re-run on
/// every call so time-sensitive output (hot-list `seconds_ago`,
/// reservations, liveness) is never frozen by a cache hit (spec §9
/// "conditions re-validated on each recall()"; P5 phase-close finding).
#[derive(Clone)]
pub struct RecallPipeline {
    phase1: Vec<Scored<NodeId>>,
    /// Per-leg phase-1 scores (I1). Cached with the rest of the pipeline
    /// because it describes the same computation: a cache hit that reused
    /// `phase1` while recomputing legs could report provenance for a ranking
    /// nobody produced.
    legs: candidates::LegProvenance,
    expanded: expand::ExpandedSet,
}

/// What kind of recall `Daemon::recall_routed` runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    /// A text recall: a structural phrasing may dispatch to traversal (T9),
    /// and a failed vector read degrades to the other legs with a warning.
    Text,
    /// A recall by an image or a client vector (#22 PR 6): never dispatched
    /// to traversal (it would skip the vector leg), and the vector leg is
    /// required, so a failed vector read fails the recall.
    ByVector,
}

impl Route {
    fn routes_structural(self) -> bool {
        self == Route::Text
    }

    fn vector_required(self) -> bool {
        self == Route::ByVector
    }
}

/// The daemon cycle's `now` source (T4.6 finding-1 regression seam).
///
/// Production uses [`Utc::now`]; tests swap in a controllable clock
/// ([`Daemon::with_clock`]) so an idle session can be aged past a detector
/// window (e.g. staleness) without waiting on the wall clock.
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
    /// The owner's read-access ledger (issue #30), when it gave the daemon
    /// one: each cycle applies what the read paths noted since the last.
    accesses: Option<Arc<access::AccessLedger>>,
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
    /// ...or once this much time has passed since the last sweep
    /// (`gc_max_interval`, issue #29)...
    pub gc_max_interval: Duration,
    /// ...provided at least this many session mutations happened since it
    /// (`gc_idle_floor`, issue #29). See [`gc::sweep_due`].
    pub gc_idle_floor: u64,
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
            gc_max_interval: config.gc_max_interval,
            gc_idle_floor: config.gc_idle_floor,
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
            accesses: None,
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

    /// Give the daemon the owner's [`access::AccessLedger`] (issue #30): every
    /// cycle applies the accesses the owner's read paths noted since the last
    /// one, in a single graph write section, **before** GC runs, so a sweep
    /// scores against counts no older than one tick.
    ///
    /// The daemon applies; it never records. What counts as an access is the
    /// owner's decision (`Memory`'s recall and inspect surfaces), which is why
    /// a reader that builds a `Daemon` only to run one recall — `lambo recall`,
    /// `serve-web` — passes no ledger and counts nothing.
    pub fn with_access_ledger(mut self, ledger: Arc<access::AccessLedger>) -> Self {
        self.accesses = Some(ledger);
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
            accesses: self.accesses.clone(),
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

    /// The wake handle itself, for a caller that must poke the daemon from a
    /// task holding no `Daemon` (J3's background write workers — they carry
    /// `Arc` clones of shared state rather than a handle on `Memory`, so they
    /// cannot call [`Daemon::wake`]). Notifying through this is exactly what
    /// `wake` does.
    pub(crate) fn waker(&self) -> Arc<Notify> {
        self.wake.clone()
    }

    /// Snapshot of the daemon-owned score table.
    pub fn scores(&self) -> ScoreTable {
        self.scores.read().clone()
    }

    /// Handle to the daemon-owned score table, for readers that must follow
    /// it across cycles rather than take one snapshot — P6's
    /// [`CanonizationTask`] reads it once per `canonization_eval_interval`
    /// for Stage 1's P90 population.
    ///
    /// Daemon-owned: the rescore loop replaces the table wholesale each
    /// cycle, so holders must only ever read. Lock order is unchanged (this
    /// lock is never taken while the hot list is held).
    ///
    /// [`CanonizationTask`]: crate::canon::CanonizationTask
    pub fn score_table(&self) -> Arc<RwLock<ScoreTable>> {
        self.scores.clone()
    }

    /// Three-phase recall (spec §8; P5).
    ///
    /// Store I/O happens in [`crate::recall::candidates::gather`] BEFORE any
    /// lock: the vector leg is async and must not run while the graph lock is
    /// held. The pipeline then runs under the documented lock order
    /// (graph read -> index read -> hot write). The daemon's inverted index
    /// must be installed via [`Daemon::with_index`]; without it recall returns
    /// an empty hit list with a warning (P8 wires the owner's index).
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
    /// Three-phase recall (spec §8; P5), projected onto the public flattened
    /// [`RecallResult`] from the same single execution that builds the H3
    /// presentation model (`Self::recall_detailed`).
    pub async fn recall(
        &self,
        session: &SessionId,
        query: RecallQuery,
        store: &dyn crate::store::GraphStore,
        embedding: Option<(&[f32], &crate::types::EmbeddingContract)>,
        weights: RecallWeights,
        cache: &mut RecallCache<RecallPipeline>,
    ) -> RecallResult {
        self.recall_detailed(session, query, store, embedding, weights, cache)
            .await
            .into()
    }

    /// The H3 detailed recall: one execution producing the rendered context
    /// block AND the presentation model (status + typed annotations captured
    /// at assembly/dispatch). Store I/O happens in
    /// [`crate::recall::candidates::gather`] BEFORE any lock: the vector leg
    /// is async and must not run while the graph lock is held. The pipeline
    /// then runs under the documented lock order (graph read -> index read
    /// -> hot write). The daemon's inverted index must be installed via
    /// [`Daemon::with_index`]; without it recall returns an empty hit list
    /// with a warning (P8 wires the owner's index).
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
    pub(crate) async fn recall_detailed(
        &self,
        session: &SessionId,
        query: RecallQuery,
        store: &dyn crate::store::GraphStore,
        embedding: Option<(&[f32], &crate::types::EmbeddingContract)>,
        weights: RecallWeights,
        cache: &mut RecallCache<RecallPipeline>,
    ) -> DetailedRecall {
        self.recall_with(
            session,
            query,
            crate::store::vector_source::VectorCandidates::from_store(store),
            embedding,
            weights,
            cache,
        )
        .await
    }

    /// [`Daemon::recall_detailed`], reaching vector candidates through the
    /// source the caller was given (#27's caller-side seam) instead of a
    /// store. `Memory::recall_detailed` calls this; with
    /// `VectorCandidates::Store` it is exactly `recall_detailed`.
    pub(crate) async fn recall_with(
        &self,
        session: &SessionId,
        query: RecallQuery,
        vectors: crate::store::vector_source::VectorCandidates<'_>,
        embedding: Option<(&[f32], &crate::types::EmbeddingContract)>,
        weights: RecallWeights,
        cache: &mut RecallCache<RecallPipeline>,
    ) -> DetailedRecall {
        match self
            .recall_routed(
                session,
                query,
                vectors,
                embedding,
                weights,
                cache,
                Route::Text,
            )
            .await
        {
            Ok(result) => result,
            // Unreachable: a text recall degrades a failed vector read to a
            // warning and never returns `Err`. Kept total rather than a panic,
            // and the detail stays in the log.
            Err(err) => {
                tracing::warn!(target: "lambo::recall", "recall: {err}");
                DetailedRecall::warn_only("recall: the vector read failed".into())
            }
        }
    }

    /// Recall by a query vector the caller already holds (#22 PR 6: recall
    /// by image or by a client vector), instead of the query text's
    /// embedding.
    ///
    /// The blended pipeline always runs: a structural phrasing in the
    /// (optional) text is **not** dispatched to traversal, because that
    /// path skips the vector leg and would silently drop the image the
    /// caller asked about. The keyword leg reads the text as usual (an
    /// empty text finds nothing), the recent leg is unchanged, and the
    /// vector leg searches with `embedding`. Nothing is cached: a
    /// vector-dependent pipeline never is (P1-2), and the caller passes a
    /// cache of its own for the signature's sake.
    ///
    /// **The vector leg is required.** A store error on the vector read
    /// (a backend failure, a timeout, a tier whose durable fallback also
    /// failed, an embedding-contract race) is returned as `Err` instead of
    /// degrading to the other legs: with no text, those legs would answer
    /// "what is near this image" with whatever was derived last. This is
    /// the same rule as a failed image embed (`query_vector::resolve`).
    pub(crate) async fn recall_by_vector_with(
        &self,
        session: &SessionId,
        query: RecallQuery,
        vectors: crate::store::vector_source::VectorCandidates<'_>,
        embedding: (&[f32], &crate::types::EmbeddingContract),
        weights: RecallWeights,
        cache: &mut RecallCache<RecallPipeline>,
    ) -> Result<DetailedRecall, StoreError> {
        self.recall_routed(
            session,
            query,
            vectors,
            Some(embedding),
            weights,
            cache,
            Route::ByVector,
        )
        .await
    }

    /// [`Daemon::recall_with`] or [`Daemon::recall_by_vector_with`], as
    /// `route` says. `Err` only on [`Route::ByVector`], whose vector leg is
    /// required.
    #[allow(clippy::too_many_arguments)]
    async fn recall_routed(
        &self,
        session: &SessionId,
        query: RecallQuery,
        vectors: crate::store::vector_source::VectorCandidates<'_>,
        embedding: Option<(&[f32], &crate::types::EmbeddingContract)>,
        weights: RecallWeights,
        cache: &mut RecallCache<RecallPipeline>,
        route: Route,
    ) -> Result<DetailedRecall, StoreError> {
        if let Err(err) = crate::store::validate_vector_candidate_limit(query.top_k) {
            return Ok(DetailedRecall::warn_only(format!("recall: {err}")));
        }
        // P2-8: the caller's `session` must match the graph's authoritative
        // session — the keyword/recent/expansion legs come from the daemon's
        // graph while the vector leg is namespace-keyed by `session`. Deriving
        // the vector namespace from the graph prevents mixing graph A with
        // vector-session B. On mismatch, refuse (warn), never mix.
        let graph_session = self.graph.read().session_id().clone();
        if session != &graph_session {
            return Ok(DetailedRecall::warn_only(format!(
                "recall: caller session {session} != graph session {graph_session}; \
                 refusing to mix graph and vector namespaces"
            )));
        }
        // T9: route by query kind. A structural/dependency question is answered
        // by traversal below; the gather and the blended pipeline are skipped
        // only for a DISPATCHED structural query (N2) - one that actually resolves
        // an anchor with structural dependents.
        let structural =
            route.routes_structural() && dispatch::classify(&query.query) == RecallKind::Structural;

        // Cheap in-memory dispatch check (no store I/O) under a brief graph read:
        // does the query resolve an anchor WITH structural dependents? Only then
        // can the (async, vector-I/O) gather be skipped (T9-R1-3); a structural
        // phrasing that does not dispatch falls through to the FULL blend below,
        // never a degraded keyword-only answer.
        let dispatch_ready = if structural {
            let g = self.graph.read();
            dispatch::fits_structural(&g, &query.query)
        } else {
            false
        };

        // Gather store I/O BEFORE any lock (the vector leg is async). Uses the
        // graph's authoritative session as the vector namespace (P2-8). Skipped
        // only when the traversal dispatch is about to fire (T9-R1-3).
        // E2E-6: annotations for a vector leg refused mid-flight (checked
        // read `Invariant` on an embedding-contract race). Gathered before
        // the lock, appended to the result's response annotations below.
        let mut vector_leg_refused: Vec<Annotation> = Vec::new();

        let input = if dispatch_ready {
            candidates::Phase1Input::default()
        } else {
            match candidates::gather_from(vectors, &graph_session, embedding, query.top_k).await {
                Ok(input) => input,
                // #22 PR 6: a recall by image or vector has nothing to
                // degrade to; its vector leg is the question.
                Err(err) if route.vector_required() => {
                    tracing::warn!(
                        target: "lambo::recall",
                        "recall by vector failed: the vector read failed: {err}"
                    );
                    return Err(err);
                }
                Err(err) => {
                    tracing::warn!(target: "lambo::recall", "phase-1 gather degraded: {err}");
                    // E2E-6: a mid-flight refusal of the checked vector read
                    // (H1's `Invariant` when the durable embedding contract
                    // changed between the reader's load and its vector query)
                    // is a client-visible degradation, not just a log line:
                    // attach the `vector_degraded` annotation so both the CLI
                    // header and the portal's card/verbatim views say the
                    // results are keyword-only. Ranking stays fail-closed —
                    // only the explanation is new. Distinct from the
                    // CLI-side embed-failure annotation (that path has no
                    // query embedding, so `gather` returns early and never
                    // reaches the store), so the two never duplicate.
                    //
                    // Any other failure of the vector read (a backend error,
                    // a timeout, a tier whose durable fallback failed too)
                    // drops the same leg, so it says so the same way; its
                    // text names no detail (that can carry a store URL or a
                    // driver string), the log above has it.
                    let text = if matches!(&err, StoreError::Invariant(msg) if msg.contains("embedding contract changed"))
                    {
                        "recall: vector leg refused because the embedding contract changed \
                         mid-query; results are keyword-only"
                    } else {
                        "recall: the store's vector read failed (detail logged); vector leg \
                         skipped, results are keyword-only"
                    };
                    vector_leg_refused.push(Annotation::new(AnnotationKind::VectorDegraded, text));
                    candidates::Phase1Input::default()
                }
            }
        };

        // ONE graph guard spans pipeline acquisition AND assembly, so the epoch
        // in the cache key identifies the exact graph snapshot the context is
        // built from (P1-1). Lock order: graph read -> index read -> hot write.
        let graph = self.graph.read();

        // T9 dispatch: answer a structural question by traversal when the graph has
        // an anchor with dependents; otherwise fall through to the blended pipeline
        // (refusal), which is the honest full-blend outcome (T9-R1-3 - the gather
        // is skipped only when this dispatch fires).
        //
        // T9-R1-7: structural results are intentionally NOT cached, and the hotlist
        // is not refreshed. The recall cache stores the blended RecallPipeline to
        // amortize the expensive async phase-1 gather I/O, which a dispatched
        // structural query skips entirely (the traversal is a cheap in-memory scan),
        // so caching buys nothing and would risk a stale traversal after graph
        // mutation. The daemon HotList tracks conflict/condition entries, not recall
        // recency - neither the blend nor this path bumps recall recency - so there
        // is no recency to refresh here.
        if structural
            && let Some(result) =
                dispatch::try_structural(&graph, &query.query, query.top_k, query.max_tokens)
        {
            return Ok(result);
        }

        let scores = self.scores.read().clone();
        let index = self.index.as_ref().map(|i| i.read());
        let mut hot = self.hot.write();
        let epoch = graph.epoch();
        let key = CacheKey::new(&query.query, query.top_k, query.traversal_depth, epoch);

        // P1-2: the cache key cannot capture vector-source state (embedding
        // presence, transient store success/failure, write-behind progress), so
        // vector-dependent results are NEVER cached or served from cache.
        let can_cache = embedding.is_none();
        let build_pipeline = |graph: &Graph,
                              index: Option<&InvertedIndex>,
                              input: crate::recall::candidates::Phase1Input,
                              query: &RecallQuery| {
            // P2-6: without an index, the independently gathered recent and
            // vector legs still yield candidates (only lexical lookup is lost).
            let (phase1, legs) = match index {
                Some(index) => {
                    candidates::candidates_with_legs(graph, index, input, &query.query, query.top_k)
                }
                None => candidates::candidates_without_keyword_with_legs(graph, input),
            };
            let expanded = expand::expand(graph, phase1.clone(), query.traversal_depth);
            RecallPipeline {
                phase1,
                legs,
                expanded,
            }
        };
        let pipeline = if can_cache {
            match cache.get(&key) {
                Some(cached) => cached.clone(),
                None => {
                    let pipeline = build_pipeline(&graph, index.as_deref(), input, &query);
                    // P5-3: never cache a compute whose daemon scores lag the
                    // graph epoch (rescore is epoch-gated).
                    if scores.epoch == epoch {
                        cache.insert(key, pipeline.clone());
                    }
                    pipeline
                }
            }
        } else {
            build_pipeline(&graph, index.as_deref(), input, &query)
        };

        let now = (self.clock)();
        // T5.3 / XP-3: re-validate the expanded members' hot-list entries at
        // the SAME `now` the assembly renders with, under the guards already
        // held (graph read, then index read, then hot write). Lapsed entries
        // are evicted here; the survivors' freshly rebuilt payloads are what
        // assembly force-includes and renders. Done here rather than inside
        // `assemble` so recall reads a map and never mutates daemon state.
        let hot_payloads = hot.revalidate_members(
            &graph,
            pipeline
                .expanded
                .required
                .iter()
                .chain(pipeline.expanded.siblings.iter())
                .map(|s| s.item),
            now,
        );
        let mut result = assemble::assemble(
            &graph,
            &pipeline.expanded,
            &pipeline.phase1,
            &scores,
            &hot_payloads,
            &query,
            weights,
            now,
            assemble::default_token_count,
        );
        // I1: attach the phase-1 leg provenance for the hits that survived
        // assembly. Filtered to the returned hits rather than passed whole —
        // phase 1 is a bounded over-approximation (keyword is over-fetched by
        // `KEYWORD_OVERFETCH`), and the ledger only ever asks about a hit it
        // was given. Hits absent from the map came in through traversal
        // expansion; that absence is the honest answer, not a gap.
        result.legs = result
            .hits
            .iter()
            .filter_map(|h| pipeline.legs.get(&h.node_id).map(|l| (h.node_id, *l)))
            .collect();
        if self.index.is_none() {
            result.warnings.push(
                "recall: no inverted index installed (Daemon::with_index) - keyword leg unavailable"
                    .to_string(),
            );
        }
        // E2E-6: the refused-leg note lands in producer order after the
        // pipeline's own response annotations (assembled above). The only
        // other response annotation on this path is `traversal`, which is
        // produced by a dispatched structural query that skips `gather`
        // entirely — the two never coexist.
        //
        // The same line also goes to `warnings`, which is what `Memory::recall`
        // and `lambo_recall` hand a caller (as the query-embed failure's line
        // is): without it a library or MCP caller saw a recall that had
        // silently dropped its vector leg. The CLI renderer skips a warning an
        // annotation already rendered, so the header shows it once.
        result
            .warnings
            .extend(vector_leg_refused.iter().map(|a| a.text.clone()));
        result.response_annotations.extend(vector_leg_refused);
        Ok(result)
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
    /// `daemon::tests::conditions::late_subscriber_misses_the_warm_up_condition_set`.
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

    /// Handle to the daemon-owned hot list (tests assert maintenance here).
    ///
    /// Two writers maintain it. The loop keeps it equal to each cycle's fresh
    /// detector hits ([`HotList::retain_conditions`]), and recall **mutates**
    /// it too: `Daemon::recall_detailed` re-validates the expanded members'
    /// entries at the recall's `now` ([`HotList::revalidate_members`], T5.3 /
    /// XP-3), evicting lapsed ones and rebuilding the survivors' payloads.
    /// Both take the graph lock before this one, so consumers must never take
    /// the graph lock while holding this one.
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
    accesses: Option<Arc<access::AccessLedger>>,
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
/// 3. Run GC when [`gc::sweep_due`] says so: `gc_interval` session mutations
///    since the last sweep (spec §9), or `gc_max_interval` elapsed since it with
///    at least `gc_idle_floor` session mutations (issue #29). The counter spans
///    the deployment's whole lifetime — the epoch resumes from the durable
///    snapshot on a writer restart (issue #17), so a low-write deployment
///    crosses the interval cumulatively instead of never — and GC moves the
///    watermark ([`crate::types::GcMark`]) to its own `epoch_after` so the next
///    interval measures session mutations only.
///
///    **The watermark is durable too (issue #29).** It lives on the graph,
///    rides every flushed batch beside the epoch and resumes with it, so a
///    restart measures from the last sweep, not from 0. Before, it was
///    per-process state starting at 0: once a session's lifetime count passed
///    `gc_interval`, *every* writer restart swept once and bumped every
///    `gc_survived` — three restarts alone reached Stage 1's floor. A session
///    that has never swept still has watermark 0, so #17's one catch-up sweep
///    on attach is unchanged for it. The time of the last sweep persists the
///    same way; a never-swept session's clock is anchored (not swept) the
///    first time a writer observes it. A previously swept session is not
///    swept *because* of a restart, but a writer that was down for N
///    intervals with at least `gc_idle_floor` unswept mutations finds a sweep
///    due and sweeps once on its first cycle back, not N times (intended).
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
///
/// GC's watermark is **not** here (issue #29): it is durable graph state
/// ([`Graph::gc_mark`]) so a restart resumes it instead of starting from 0.
#[derive(Default)]
struct CycleState {
    /// `None` → the first cycle always rescores (warm-up), then epoch-gated.
    last_epoch: Option<u64>,
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
        accesses,
        cycles,
        ..
    } = state;

    // 0. Apply the read accesses noted since the last cycle (issue #30) — one
    //    brief write section, ledger taken first (it is a leaf lock). First,
    //    so GC below scores against counts at most one tick old. Accesses do
    //    not advance the epoch, so this neither triggers a rescore nor counts
    //    toward `gc_interval` (see `Graph::record_accesses`).
    if let Some(ledger) = accesses.as_ref() {
        ledger.apply(graph);
    }

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
        let mut g = graph.write();
        let applied =
            gc::drain_survivor_bumps(&mut g, &mut cs.gc_pending, params.gc_survivor_bump_chunk);
        // NEW-2: a drain's own mutations must not be credited as session
        // mutations toward the next `gc_interval`. `bump_gc_survived` appends
        // exactly one `UpsertNode` per applied bump and the epoch bumps once
        // per appended mutation, so advancing the watermark by `applied`
        // cancels GC's own writes out of 3b's measure exactly. Same guard as
        // the bumps (issue #29), so the batch that carries them carries the
        // advanced watermark too.
        g.exempt_from_gc_measure(applied as u64);
    }

    // 3b. Periodic GC: `gc_interval` session mutations, or `gc_max_interval`
    //     elapsed with at least `gc_idle_floor` of them (issue #29).
    let mut mark = graph.read().gc_mark();
    if mark.last_gc_at.is_none() {
        // Issue #29: a session that has never swept starts its time bound
        // when a writer first sees it — never "overdue" on attach. Rides the
        // next flushed batch. A mark-only batch cannot persist (sessions are
        // resolved from mutations), so a restart before any write re-anchors.
        // For a never-swept session already over the idle floor this postpones
        // the timed catch-up: it needs one write or `gc_max_interval` of uptime.
        // The count-based catch-up at `gc_interval` (#17) is unaffected.
        let mut g = graph.write();
        g.anchor_gc_clock(now);
        mark = g.gc_mark();
    } else if gc::gc_clock_ahead(mark, now) {
        // Issue #29: the stored sweep time is in the future beyond the skew
        // tolerance — a forward wall-clock jump, since corrected, stamped it
        // and the monotonic merge kept it. Re-anchor at `now` (the mark's one
        // permitted regression, persisted through `last_gc_at_reset`) instead
        // of leaving the time trigger off until real time catches up.
        tracing::warn!(
            target: "lambo::daemon::gc",
            last_gc_at = ?mark.last_gc_at,
            now = %now,
            "GC sweep time is in the future (wall-clock jump?); re-anchoring the \
             gc_max_interval clock at now"
        );
        let mut g = graph.write();
        g.reanchor_gc_clock(now);
        mark = g.gc_mark();
    }
    // The drain above may have moved the watermark past this cycle's `epoch`
    // snapshot; `sweep_due` saturates, so that reads as zero elapsed (GC one
    // cycle late, never early — NEW-2).
    let trigger = if cs.gc_pending.is_empty() {
        gc::sweep_due(
            epoch,
            mark,
            now,
            params.gc_interval,
            params.gc_max_interval,
            params.gc_idle_floor,
        )
    } else {
        None
    };
    if let Some(trigger) = trigger {
        let outcome = {
            let mut g = graph.write();
            let mut outcome = gc::run(
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
            outcome.trigger = Some(trigger);
            // Issue #29: the durable watermark and sweep time, set under the
            // sweep's own guard so they drain with its mutations.
            g.record_gc_sweep(outcome.epoch_after, now);
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
            trigger = ?outcome.trigger,
            collection_cap = outcome.collection_cap,
            collections_deferred = outcome.collections_deferred,
            edges_removed = outcome.edges_removed.len(),
            concepts_collected = outcome.concepts_collected.len(),
            survivors = outcome.survivors.len(),
            survivors_deferred = outcome.survivors_pending.len(),
            canonical_count = outcome.canonical_count,
            canonical_over_budget = outcome.canonical_over_budget,
            epoch_after = outcome.epoch_after,
            "GC sweep complete"
        );
        cs.gc_pending = outcome.survivors_pending.clone();
        *last_gc.write() = Some(outcome);
    }

    // Last statement: a panicking cycle (CONC-4) does not count as completed,
    // so `Daemon::cycles` is a witness that the whole body ran (XP-6).
    cycles.fetch_add(1, Ordering::Release);
}

#[cfg(test)]
mod tests;
