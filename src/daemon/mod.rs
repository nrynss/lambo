//! Daemon task skeleton + composite scoring (P4, T4.1).
//!
//! A tokio task that polls [`Graph::epoch`] on a tick interval and rescorees
//! the session's concepts when the epoch changes (spec §9, spec §2.5 warm-up
//! note). The rescored ranking lives in a daemon-owned [`ScoreTable`]; T4.2+
//! (hot list, conflict, drift, GC) consume it.
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
//! lock, read the epoch (or run the pure-RAM rescore), release, then do
//! anything async. `parking_lot` guards are `!Send`, so the compiler enforces
//! this inside `tokio::spawn`.
//!
//! ## Stopping
//!
//! [`Daemon::spawn`] returns the `JoinHandle`; aborting it stops the loop (a
//! graceful stop is a P8 concern per the COH-6 note).
pub mod conflict;
pub mod hotlist;
pub mod score;

use parking_lot::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

use crate::config::ScoringWeights;
use crate::graph::Graph;
use crate::types::{NodeId, Scored};

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

/// Background scorer (T4.1 skeleton). Spawn with [`Daemon::spawn`].
pub struct Daemon {
    graph: Arc<RwLock<Graph>>,
    weights: ScoringWeights,
    tick: Duration,
    wake: Arc<Notify>,
    scores: Arc<RwLock<ScoreTable>>,
    started: AtomicBool,
}

impl Daemon {
    /// `tick` is the rescore poll interval; tests pass a long tick and drive
    /// cycles with [`Daemon::wake`].
    pub fn new(graph: Arc<RwLock<Graph>>, weights: ScoringWeights, tick: Duration) -> Self {
        Self {
            graph,
            weights,
            tick,
            wake: Arc::new(Notify::new()),
            scores: Arc::new(RwLock::new(ScoreTable::default())),
            started: AtomicBool::new(false),
        }
    }

    /// Spawn the rescore loop and return its handle (abort = stop).
    ///
    /// Call `spawn` **exactly once** per `Daemon` — a second call panics
    /// (single-loop enforcement, mirroring `FlushTask::spawn`). Takes `&self`
    /// so the caller keeps this handle for [`Daemon::wake`] /
    /// [`Daemon::scores`] while the task runs.
    pub fn spawn(&self) -> tokio::task::JoinHandle<()> {
        self.started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .expect("Daemon::spawn called twice — exactly one loop may run");
        let graph = self.graph.clone();
        let wake = self.wake.clone();
        let scores = self.scores.clone();
        let weights = self.weights;
        let tick = self.tick;
        tokio::spawn(async move {
            run_loop(graph, wake, scores, weights, tick).await;
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
}

/// The rescore loop.
///
/// First cycle runs immediately (the warm-up rescore; spec §2.5), then only
/// when the graph epoch changes. No lock is held across any `.await`: the
/// epoch read and the rescore are synchronous, the select is not.
async fn run_loop(
    graph: Arc<RwLock<Graph>>,
    wake: Arc<Notify>,
    scores: Arc<RwLock<ScoreTable>>,
    weights: ScoringWeights,
    tick: Duration,
) {
    let mut interval = tokio::time::interval(tick);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // None → first cycle always rescorees (warm-up), then change-gated.
    let mut last_epoch: Option<u64> = None;
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = wake.notified() => {}
        }
        // Brief lock: read epoch, release.
        let epoch = graph.read().epoch();
        if last_epoch == Some(epoch) {
            continue;
        }
        last_epoch = Some(epoch);
        // Pure RAM work under the read lock — no I/O, no `.await` while held.
        let ranked = {
            let g = graph.read();
            score::rescore(&g, &weights)
        };
        *scores.write() = ScoreTable { epoch, ranked };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Graph;
    use crate::types::{AgentId, CanonizationStatus, Concept, ConceptType, Interaction, SessionId};
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

        // Wake with no mutation: the loop must skip (epoch unchanged) and the
        // table must stay byte-identical.
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
}
