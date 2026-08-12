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
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, Notify};

use crate::config::ScoringWeights;
use crate::daemon::hotlist::{Condition, HotList};
use crate::graph::Graph;
use crate::types::{DaemonEvent, NodeId, Scored};

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
    events: Arc<broadcast::Sender<DaemonEvent>>,
    params: CycleParams,
    clock: Clock,
    started: AtomicBool,
}

/// Daemon loop tuning (T4.6). [`Default`] mirrors [`crate::config::Config`]'s
/// defaults (hot_list_max 1000, conflict_recency_window 30s, drift_threshold
/// 5, gc_interval 10_000, max_canonical_nodes 1000) plus the T4.6-specific
/// knobs (staleness / high-risk windows and event capacity) that Config does
/// not carry; P8 merges the two when it builds the §6.1 `Memory` surface.
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
}

impl Default for CycleParams {
    fn default() -> Self {
        Self {
            conflict_window: conflict::CONFLICT_RECENCY_WINDOW,
            drift_threshold: drift::DRIFT_THRESHOLD,
            stale_window: events::STALE_WINDOW,
            high_risk_window: events::HIGH_RISK_WRITE_WINDOW,
            hot_list_max: hotlist::HOT_LIST_MAX,
            gc_interval: 10_000,
            max_canonical_nodes: 1000,
            event_capacity: events::EVENT_CAPACITY,
        }
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
        let (sender, _) = broadcast::channel(params.event_capacity);
        Self {
            graph,
            weights,
            tick,
            wake: Arc::new(Notify::new()),
            scores: Arc::new(RwLock::new(ScoreTable::default())),
            hot: Arc::new(RwLock::new(HotList::with_max(params.hot_list_max))),
            events: Arc::new(sender),
            params,
            clock: Arc::new(Utc::now),
            started: AtomicBool::new(false),
        }
    }

    /// Use `clock` as the cycle's `now` source instead of the wall clock
    /// (tests drive a controllable clock; see [`Clock`]).
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
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

    /// Subscribe to the daemon's event channel (spec §6.1). The receiver
    /// sees every `DaemonEvent` published after subscription; P8's
    /// `mem.events()` delegates here.
    ///
    /// A dropped receiver is not an error and a lagging receiver never blocks
    /// the daemon — it misses messages (`RecvError::Lagged`) and re-syncs to
    /// the newest retained window.
    pub fn events(&self) -> broadcast::Receiver<DaemonEvent> {
        self.events.subscribe()
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
    sender: Arc<broadcast::Sender<DaemonEvent>>,
    clock: Clock,
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
///    `DaemonEvent` fires only when a `(condition, node)` *enters* the
///    detected set, so a persisting condition is published once, not once
///    per cycle — a 256-capacity channel is never flooded with duplicates
///    (finding 3). Exit = stop emitting (`DaemonEvent` has no resolved
///    variant — frozen §6.1 enum). Hot-list entries still refresh per cycle.
/// 3. Run GC once the mutation counter crosses `gc_interval` (spec §9). The
///    counter spans the graph's whole lifetime; GC resets it to its own
///    `epoch_after` so the next interval measures session mutations only.
///    Detection runs before GC: events reflect what the session's writes
///    did, GC is housekeeping after.
async fn run_loop(state: LoopState, weights: ScoringWeights, tick: Duration, params: CycleParams) {
    let LoopState {
        graph,
        wake,
        scores,
        hot,
        sender,
        clock,
    } = state;
    let mut interval = tokio::time::interval(tick);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // None → the first cycle always rescorees (warm-up), then epoch-gated.
    let mut last_epoch: Option<u64> = None;
    let mut last_gc_epoch: u64 = 0;
    // Emit-on-transition (finding 3): the previous cycle's detected set; an
    // event fires only when a pair ENTERS it.
    let mut prev_conditions: HashSet<(Condition, NodeId)> = HashSet::new();
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = wake.notified() => {}
        }
        // Brief lock: read epoch, release.
        let epoch = graph.read().epoch();
        let now = clock();

        // 1. Rescore — only when the epoch changed (finding 1: detection is
        //    NOT epoch-gated; an idle session must age into staleness).
        if last_epoch != Some(epoch) {
            last_epoch = Some(epoch);
            let ranked = {
                let g = graph.read();
                score::rescore(&g, &weights)
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
        // Emit-on-transition: only pairs that ENTERED the set this cycle.
        let entered: HashSet<(Condition, NodeId)> =
            fresh.difference(&prev_conditions).copied().collect();
        prev_conditions = fresh;
        for hit in &conflict_hits {
            if entered.contains(&(Condition::Conflict, hit.node)) {
                events::emit(&sender, events::conflict_event(hit));
            }
        }
        for hit in &drift_hits {
            if entered.contains(&(Condition::Drift, hit.node)) {
                events::emit(&sender, events::drift_event(hit));
            }
        }
        for hit in &stale_hits {
            if entered.contains(&(Condition::StaleSession, hit.node)) {
                events::emit(&sender, events::stale_event(hit.node, hit.seconds_inactive));
            }
        }
        for hit in &high_risk_hits {
            if entered.contains(&(Condition::HighRiskModification, hit.node)) {
                events::emit(
                    &sender,
                    events::high_risk_event(hit.node, hit.reason.clone()),
                );
            }
        }

        // 3. Periodic GC (spec §9): every `gc_interval` session mutations.
        if epoch.saturating_sub(last_gc_epoch) >= params.gc_interval {
            let outcome = {
                let mut g = graph.write();
                gc::run(
                    &mut g,
                    gc::GcParams {
                        now,
                        // ALGO-4: GC's eviction ranking uses the session's own
                        // weights, not a second hardcoded default.
                        weights,
                        max_canonical_nodes: params.max_canonical_nodes,
                        ..Default::default()
                    },
                )
            };
            last_gc_epoch = outcome.epoch_after;
        }
    }
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

    #[tokio::test]
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

    #[tokio::test]
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
        daemon.wake();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let after = daemon.scores();
        assert_eq!(before, after, "no epoch change must not rescore");

        handle.abort();
    }

    #[tokio::test]
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

    #[tokio::test]
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

    #[tokio::test]
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
    #[tokio::test]
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
    #[tokio::test]
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

        // Warm-up: single-agent fixture → no conflict; drift is the only
        // event (drift detection is clock-free, the chain is 6 hops).
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
        handle.abort();
    }

    #[tokio::test]
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

    #[tokio::test]
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

    #[tokio::test]
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

    #[tokio::test]
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

    #[tokio::test]
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
        daemon.wake();
        tokio::time::sleep(Duration::from_millis(150)).await;
        match rx.try_recv() {
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}
            other => panic!("wake without a mutation must not publish, got {other:?}"),
        }
        handle.abort();
    }

    #[tokio::test]
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
        tokio::time::sleep(Duration::from_millis(50)).await;
        match rx.try_recv() {
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}
            other => panic!("persisting stale must not re-emit, got {other:?}"),
        }
        handle.abort();
    }

    #[tokio::test]
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
            daemon.wake();
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        match rx.try_recv() {
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}
            other => panic!("persisting high-risk must not re-emit, got {other:?}"),
        }
        handle.abort();
    }

    /// Parse the seconds out of a `stale_event` detail
    /// ("node <id> untouched for <N>s").
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
    #[tokio::test]
    async fn loop_emits_stale_from_rest_api_fixture_after_writes_age_out() {
        // Final-review finding 1a: staleness had only a synthetic-clock loop
        // test. This one is fixture-driven: rebase session-rest-api so its
        // newest write lands 2h before `anchor` — past the 1h STALE_WINDOW
        // and far outside the 30s conflict/high-risk windows. Every concept
        // ages out, so the warm-up cycle must emit exactly the fixture's 22
        // stale concepts, and nothing else: no Conflict (all writes are 2h
        // old, outside conflict_recency_window), no Drift (the session has
        // no root goal — drift.rs: no goal nodes → no hits), no HighRisk (no
        // fresh in-window writes — user schema is Canonical/blast-radius 8
        // but its write is 2h old).
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
        let n_concepts = g.concepts().count();
        assert_eq!(n_concepts, 22, "fixture must keep its 22 concepts");

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

        // The warm-up cycle emits every stale concept (id-ascending) in one
        // pass — drain all 22 and demand no other event kind.
        let mut stales: Vec<NodeId> = Vec::new();
        for _ in 0..n_concepts {
            let evt = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("stale event within 2s")
                .unwrap();
            match evt {
                DaemonEvent::Stale { node_id, detail } => {
                    assert!(
                        detail.contains("untouched for"),
                        "renderable detail: {detail}"
                    );
                    assert!(
                        stale_seconds(&detail) >= 7200,
                        "2h rebase ⇒ every write is ≥ 2h old, got {detail}"
                    );
                    stales.push(node_id);
                }
                other => {
                    panic!("aged-out fixture must emit only Stale, got {other:?}")
                }
            }
        }
        // Exactly the 22 concepts, once each — and nothing after them.
        assert_eq!(stales.len(), n_concepts);
        match rx.try_recv() {
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}
            other => panic!("exactly {n_concepts} stale events, got more: {other:?}"),
        }
        // The hot list mirrors the fresh stale set.
        assert!(
            daemon.hot_list().read().contains(stales[0]),
            "stale node must be on the hot list"
        );
        handle.abort();
    }
}
