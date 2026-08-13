//! Evaluation cycle — promotions, budget demotion, audit (T6.4, spec §10).
//!
//! One hop per cycle (documented): a node may take **at most one** legal
//! edge of the state machine per tick. The eval loop never uses
//! `None → Venerable` (that edge is reserved for `set_root_goal`). The
//! hops this cycle will emit, in order, are:
//!
//! 1. Stage 1: `None → Candidate` for nodes still-None in
//!    [`stage1_candidates`].
//! 2. Stage 2: `Candidate → Venerable` for nodes that were already
//!    Candidate *before* this cycle's Stage 1 hop (a node that just
//!    became Candidate is not re-checked for Venerable in the same tick).
//! 3. Stage 3: `Venerable → Canonical` for a **round-robin** window of
//!    at most [`EvalParams::batch_size`] Venerable nodes, **score-
//!    descending** (NodeId ascending tie-break) within the window.
//!    The cursor lives on [`Evaluator`] so the next cycle continues
//!    around the ring.
//! 4. Budget: if Canonical count exceeds `max_canonical_nodes`, demote
//!    `Canonical → None` lowest [`GraphStore::blast_radius`] first
//!    (NodeId ascending tie-break) until the count is within budget.
//!
//! ## Commit order
//!
//! Every hop (up and down) goes through [`commit_transition`]:
//!
//! 1. [`Graph::apply_canonization_transition`] — RAM + in-graph audit.
//! 2. [`GraphStore::record_canonization`] — durable `canonization_events`.
//! 3. [`events::emit_canonized`] — `DaemonEvent::Canonized`.
//!
//! Graph is applied first so a store failure cannot leave a durable
//! audit row without RAM state. A failed apply does **not** record or
//! emit (an unrecorded transition is a demo bug; a fabricated one is
//! worse). `now` is injected — the cycle has no wall clock.
//!
//! Callers that wrap the graph in `Arc<RwLock<Graph>>` must **not**
//! hold that lock across this async function (spec §6.4). Tests pass
//! an owned `&mut Graph`.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::canon::{stage1_candidates, stage2_passes, stage3_passes};
use crate::daemon::events::{self, EventSender};
use crate::daemon::ScoreTable;
use crate::graph::Graph;
use crate::store::GraphStore;
use crate::types::{CanonizationEvent, CanonizationStatus, LamboError, Node, NodeId, StoreError};

/// Round-robin cursor plus the one-cycle write path.
#[derive(Clone, Debug, Default)]
pub struct Evaluator {
    /// Index into the NodeId-sorted Venerable ring; persists across cycles.
    stage3_cursor: usize,
}

/// Knobs for one [`eval_cycle`]. Defaults match [`crate::Config`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvalParams {
    pub min_peer_count: usize,
    /// Forwarded to Stage 2 (`interaction_span` age floor).
    pub min_age: Duration,
    /// Forwarded to Stage 3 / budget (`blast_radius` age floor).
    pub min_edge_age: Duration,
    pub cooldown: Duration,
    /// Venerable nodes considered per cycle (spec default 50).
    pub batch_size: usize,
    pub max_canonical_nodes: usize,
}

/// What one cycle wrote. `promotions` then `demotions` is the commit order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EvalOutcome {
    pub promotions: Vec<CanonizationEvent>,
    pub demotions: Vec<CanonizationEvent>,
    /// Stage 3 window in the order it was evaluated (score-descending).
    pub stage3_batch: Vec<NodeId>,
}

impl EvalParams {
    pub fn from_config(config: &crate::Config) -> Self {
        Self {
            min_peer_count: config.canonization_min_peer_count,
            min_age: config.canonization_edge_min_age,
            min_edge_age: config.canonization_edge_min_age,
            cooldown: config.canonization_repromotion_cooldown,
            batch_size: config.canonization_eval_batch_size,
            max_canonical_nodes: config.max_canonical_nodes,
        }
    }
}

impl Default for EvalParams {
    fn default() -> Self {
        Self::from_config(&crate::Config::default())
    }
}

impl EvalOutcome {
    /// Promotions followed by demotions — every hop this cycle committed.
    pub fn transitions(&self) -> impl Iterator<Item = &CanonizationEvent> {
        self.promotions.iter().chain(self.demotions.iter())
    }
}

impl Evaluator {
    pub fn new() -> Self {
        Self { stage3_cursor: 0 }
    }

    /// Current Stage 3 ring index (tests; the next cycle starts here).
    pub fn stage3_cursor(&self) -> usize {
        self.stage3_cursor
    }

    /// One eval cycle. See the module docs for hop order and commit order.
    pub async fn eval_cycle(
        &mut self,
        graph: &mut Graph,
        store: &impl GraphStore,
        scores: &ScoreTable,
        events: &EventSender,
        params: &EvalParams,
        now: DateTime<Utc>,
    ) -> Result<EvalOutcome, LamboError> {
        let session = graph.session_id().clone();
        let score_of = score_map(scores);
        let mut hopped: HashSet<NodeId> = HashSet::new();
        let mut outcome = EvalOutcome::default();

        // --- Stage 1: None → Candidate (one hop; no skip to Venerable) ---
        let s1 = stage1_candidates(graph, scores, params.min_peer_count);
        for id in s1 {
            if concept_status(graph, id) != Some(CanonizationStatus::None) {
                continue;
            }
            let event = promotion_event(
                graph,
                id,
                CanonizationStatus::None,
                CanonizationStatus::Candidate,
                None,
                now,
            );
            outcome
                .promotions
                .push(commit_transition(graph, store, events, event).await?);
            hopped.insert(id);
        }

        // --- Stage 2: still-Candidate only (not this cycle's Stage 1 hops) ---
        let mut s2: Vec<NodeId> = graph
            .concepts()
            .filter(|c| {
                c.canonization_status == CanonizationStatus::Candidate && !hopped.contains(&c.id)
            })
            .map(|c| c.id)
            .collect();
        s2.sort_by_key(|id| id.0);
        for id in s2 {
            if !stage2_passes(store, &session, id, params.min_age).await? {
                continue;
            }
            if concept_status(graph, id) != Some(CanonizationStatus::Candidate) {
                continue;
            }
            let event = promotion_event(
                graph,
                id,
                CanonizationStatus::Candidate,
                CanonizationStatus::Venerable,
                None,
                now,
            );
            outcome
                .promotions
                .push(commit_transition(graph, store, events, event).await?);
            hopped.insert(id);
        }

        // --- Stage 3: Venerable ring, score-desc within the batch ---
        let mut venerable: Vec<NodeId> = graph
            .concepts()
            .filter(|c| {
                c.canonization_status == CanonizationStatus::Venerable && !hopped.contains(&c.id)
            })
            .map(|c| c.id)
            .collect();
        venerable.sort_by_key(|id| id.0);
        let mut batch = self.take_stage3_batch(&venerable, params.batch_size);
        batch.sort_by(|a, b| {
            score_lookup(&score_of, *b)
                .total_cmp(&score_lookup(&score_of, *a))
                .then_with(|| a.0.cmp(&b.0))
        });
        outcome.stage3_batch = batch.clone();

        // Cap promotions at the remaining Canonical budget (P2, phase R2): a
        // cycle must never push the count over max_canonical_nodes, so a
        // Venerable that would overflow stays Venerable for a later cycle.
        // This also rules out the same-tick promote-then-demote the budget
        // sweep would otherwise produce (the original P1-1).
        let mut remaining = params
            .max_canonical_nodes
            .saturating_sub(canonical_count(graph));
        for id in batch {
            if remaining == 0 {
                break;
            }
            if !stage3_passes(
                store,
                graph,
                &session,
                id,
                params.min_edge_age,
                params.cooldown,
                now,
            )
            .await?
            {
                continue;
            }
            if concept_status(graph, id) != Some(CanonizationStatus::Venerable) {
                continue;
            }
            let blast = store
                .blast_radius(&session, id, params.min_edge_age)
                .await?;
            let narrowed = narrow_blast_radius(blast)?;
            let event = promotion_event(
                graph,
                id,
                CanonizationStatus::Venerable,
                CanonizationStatus::Canonical,
                Some(narrowed),
                now,
            );
            outcome
                .promotions
                .push(commit_transition(graph, store, events, event).await?);
            remaining -= 1;
        }

        // --- Budget: lowest store.blast_radius first, NodeId asc tie-break ---
        demote_over_budget(graph, store, events, params, now, &mut outcome).await?;

        Ok(outcome)
    }

    /// Next `batch_size` ids on the NodeId-sorted ring, then advance the cursor.
    fn take_stage3_batch(&mut self, venerable: &[NodeId], batch_size: usize) -> Vec<NodeId> {
        if venerable.is_empty() || batch_size == 0 {
            return Vec::new();
        }
        let n = venerable.len();
        let start = self.stage3_cursor % n;
        let take = batch_size.min(n);
        let mut batch = Vec::with_capacity(take);
        for i in 0..take {
            batch.push(venerable[(start + i) % n]);
        }
        self.stage3_cursor = (start + take) % n;
        batch
    }
}

/// Free-function form of [`Evaluator::eval_cycle`].
pub async fn eval_cycle(
    evaluator: &mut Evaluator,
    graph: &mut Graph,
    store: &impl GraphStore,
    scores: &ScoreTable,
    events: &EventSender,
    params: &EvalParams,
    now: DateTime<Utc>,
) -> Result<EvalOutcome, LamboError> {
    evaluator
        .eval_cycle(graph, store, scores, events, params, now)
        .await
}

/// Graph first, then store, then emit. A failed apply does not emit.
async fn commit_transition(
    graph: &mut Graph,
    store: &impl GraphStore,
    events: &EventSender,
    event: CanonizationEvent,
) -> Result<CanonizationEvent, LamboError> {
    graph.apply_canonization_transition(event.clone())?;
    store.record_canonization(&event).await?;
    events::emit_canonized(events, event.clone());
    Ok(event)
}

async fn demote_over_budget(
    graph: &mut Graph,
    store: &impl GraphStore,
    events: &EventSender,
    params: &EvalParams,
    now: DateTime<Utc>,
    outcome: &mut EvalOutcome,
) -> Result<(), LamboError> {
    let session = graph.session_id().clone();
    let canonicals: Vec<NodeId> = graph
        .concepts()
        .filter(|c| c.canonization_status == CanonizationStatus::Canonical)
        .map(|c| c.id)
        .collect();
    if canonicals.len() <= params.max_canonical_nodes {
        return Ok(());
    }

    let mut ranked: Vec<(u64, NodeId)> = Vec::with_capacity(canonicals.len());
    for id in canonicals {
        let blast = store
            .blast_radius(&session, id, params.min_edge_age)
            .await?;
        ranked.push((blast, id));
    }
    ranked.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1 .0.cmp(&b.1 .0)));

    // Promotions are capped at the budget inside eval_cycle, so an
    // over-budget session here is pre-existing; demote lowest blast first
    // until within budget.
    let overflow = ranked.len() - params.max_canonical_nodes;
    for &(_, id) in ranked.iter().take(overflow) {
        if concept_status(graph, id) != Some(CanonizationStatus::Canonical) {
            continue;
        }
        let event = CanonizationEvent {
            id: NodeId::new(),
            session_id: session.clone(),
            node_id: id,
            from_status: CanonizationStatus::Canonical,
            to_status: CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: Some(now),
            occurred_at: now,
        };
        outcome
            .demotions
            .push(commit_transition(graph, store, events, event).await?);
    }
    Ok(())
}

/// Stage 1 / 2 keep the concept's current blast so apply does not wipe it.
/// Stage 3 supplies the narrowed measurement. Promotions never stamp
/// `last_demotion_time` (must not clobber a prior demotion).
fn promotion_event(
    graph: &Graph,
    node: NodeId,
    from: CanonizationStatus,
    to: CanonizationStatus,
    blast_radius: Option<i32>,
    now: DateTime<Utc>,
) -> CanonizationEvent {
    let blast_radius = match blast_radius {
        Some(b) => Some(b),
        None => match graph.node(node) {
            Some(Node::Concept(c)) => c.blast_radius,
            _ => None,
        },
    };
    CanonizationEvent {
        id: NodeId::new(),
        session_id: graph.session_id().clone(),
        node_id: node,
        from_status: from,
        to_status: to,
        blast_radius,
        last_demotion_time: None,
        occurred_at: now,
    }
}

/// CON-6: never `as i32`. An unrepresentable store count is an invariant.
fn narrow_blast_radius(blast: u64) -> Result<i32, StoreError> {
    i32::try_from(blast).map_err(|_| {
        StoreError::Invariant(format!("blast_radius {blast} exceeds i32::MAX (CON-6)"))
    })
}

fn concept_status(graph: &Graph, id: NodeId) -> Option<CanonizationStatus> {
    match graph.node(id) {
        Some(Node::Concept(c)) => Some(c.canonization_status),
        _ => None,
    }
}

fn canonical_count(graph: &Graph) -> usize {
    graph
        .concepts()
        .filter(|c| c.canonization_status == CanonizationStatus::Canonical)
        .count()
}

fn score_map(scores: &ScoreTable) -> HashMap<NodeId, f64> {
    scores.ranked.iter().map(|s| (s.item, s.score)).collect()
}

fn score_lookup(map: &HashMap<NodeId, f64>, id: NodeId) -> f64 {
    map.get(&id).copied().unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        AgentId, Concept, ConceptType, DaemonEvent, Edge, EdgeType, Interaction, Scored, SessionId,
    };
    use chrono::TimeZone;
    use uuid::Uuid;

    fn ts() -> DateTime<Utc> {
        Utc.timestamp_opt(1_752_000_000, 0).unwrap()
    }

    fn sid() -> SessionId {
        SessionId::from("test-session")
    }

    fn nid(id: u64) -> NodeId {
        NodeId(Uuid::from_u64_pair(2, id))
    }

    fn iid(id: u64) -> NodeId {
        NodeId(Uuid::from_u64_pair(1, id))
    }

    fn eid(id: u64) -> NodeId {
        NodeId(Uuid::from_u64_pair(3, id))
    }

    fn interaction(id: u64, prev: Option<u64>, at: DateTime<Utc>) -> Interaction {
        Interaction {
            id: iid(id),
            session_id: sid(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: Some(format!("i{id}")),
            previous_id: prev.map(iid),
            created_at: at,
        }
    }

    fn concept(id: u64, origin: u64, gc: i32, status: CanonizationStatus) -> Concept {
        Concept {
            id: nid(id),
            session_id: sid(),
            content: format!("c{id}"),
            canonical_key: format!("c{id}"),
            concept_type: ConceptType::Entity,
            origin_interaction: iid(origin),
            origin_agent: AgentId::from("agent-a"),
            created_at: ts(),
            access_count: 0,
            last_accessed: None,
            gc_survived: gc,
            canonization_status: status,
            blast_radius: None,
            last_demotion_time: None,
            embedding: None,
            chunk_group_id: None,
        }
    }

    fn table(pairs: &[(u64, f64)]) -> ScoreTable {
        ScoreTable {
            epoch: 0,
            ranked: pairs
                .iter()
                .map(|&(id, score)| Scored::new(nid(id), score))
                .collect(),
        }
    }

    fn params() -> EvalParams {
        EvalParams {
            min_age: Duration::ZERO,
            min_edge_age: Duration::ZERO,
            ..EvalParams::default()
        }
    }

    fn status_of(graph: &Graph, id: NodeId) -> CanonizationStatus {
        concept_status(graph, id).expect("concept")
    }

    /// CON-6: `as i32` would wrap `i32::MAX + 1` to `i32::MIN`.
    #[test]
    fn blast_radius_narrow_rejects_unrepresentable_u64() {
        assert_eq!(narrow_blast_radius(0).unwrap(), 0);
        assert_eq!(narrow_blast_radius(i32::MAX as u64).unwrap(), i32::MAX);
        let err = narrow_blast_radius(i32::MAX as u64 + 1).unwrap_err();
        match err {
            StoreError::Invariant(msg) => {
                assert!(msg.contains("i32"), "{msg}");
                assert!(msg.contains("CON-6"), "{msg}");
            }
            other => panic!("expected Invariant, got {other:?}"),
        }
    }

    #[cfg(feature = "store-memory")]
    mod with_store {
        use super::*;
        use crate::store::{GraphStore, MemoryStore};
        use crate::types::{Mutation, MutationBatch};

        async fn store_from_graph(graph: &Graph) -> MemoryStore {
            let store = MemoryStore::new();
            let snap = graph.snapshot();
            let mut batch = MutationBatch::new();
            for i in snap.interactions {
                batch.push(Mutation::UpsertNode {
                    node: Node::Interaction(i),
                });
            }
            for c in snap.concepts {
                batch.push(Mutation::UpsertNode {
                    node: Node::Concept(c),
                });
            }
            for e in snap.edges {
                batch.push(Mutation::UpsertEdge { edge: e });
            }
            store.flush(&batch).await.unwrap();
            store
        }

        fn channel() -> (EventSender, tokio::sync::broadcast::Receiver<DaemonEvent>) {
            crate::daemon::events::event_channel()
        }

        fn drain_canonized(
            rx: &mut tokio::sync::broadcast::Receiver<DaemonEvent>,
        ) -> Vec<CanonizationEvent> {
            let mut out = Vec::new();
            loop {
                match rx.try_recv() {
                    Ok(DaemonEvent::Canonized { event }) => out.push(event),
                    Ok(other) => panic!("unexpected event {other:?}"),
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                    Err(e) => panic!("recv error {e:?}"),
                }
            }
            out
        }

        fn edge(id: u64, src: u64, tgt: u64, at: DateTime<Utc>) -> Edge {
            Edge {
                id: eid(id),
                session_id: sid(),
                source: nid(src),
                target: nid(tgt),
                edge_type: EdgeType::Dependency,
                weight: 1.0,
                reinforcements: 1,
                created_at: at,
                last_reinforced: at,
            }
        }

        /// `n` exclusive dependents of `hub` so `blast_radius(hub) == n`.
        fn attach_blast(graph: &mut Graph, hub: u64, n: u64, first_dep: u64) {
            let at = ts();
            for i in 0..n {
                let dep = first_dep + i;
                graph
                    .insert_concept(concept(dep, 1, 0, CanonizationStatus::None), iid(1))
                    .unwrap();
                graph.upsert_edge(edge(dep, hub, dep, at)).unwrap();
            }
        }

        #[tokio::test]
        async fn failed_apply_does_not_record_or_emit() {
            let mut g = Graph::new(sid());
            g.insert_interaction(interaction(1, None, ts())).unwrap();
            g.insert_concept(concept(10, 1, 0, CanonizationStatus::None), iid(1))
                .unwrap();
            let store = store_from_graph(&g).await;
            let (tx, mut rx) = channel();

            let bad = CanonizationEvent {
                id: NodeId::new(),
                session_id: sid(),
                node_id: nid(10),
                from_status: CanonizationStatus::Venerable,
                to_status: CanonizationStatus::Canonical,
                blast_radius: Some(8),
                last_demotion_time: None,
                occurred_at: ts(),
            };
            let err = commit_transition(&mut g, &store, &tx, bad)
                .await
                .unwrap_err();
            assert!(
                err.to_string().contains("current status"),
                "fabricated apply must fail: {err}"
            );
            assert_eq!(status_of(&g, nid(10)), CanonizationStatus::None);
            assert!(g.canonization_events().is_empty());
            assert!(store
                .load_session(&sid())
                .await
                .unwrap()
                .canonization_events
                .is_empty());
            assert!(
                drain_canonized(&mut rx).is_empty(),
                "failed apply must not emit"
            );
        }

        #[tokio::test]
        async fn one_hop_per_cycle_none_with_stage2_evidence_becomes_candidate() {
            // 20 non-Canonical peers so Stage 1 opens; hub also has Stage 2
            // evidence. First cycle must stop at Candidate.
            let mut g = Graph::new(sid());
            let at = ts();
            g.insert_interaction(interaction(1, None, at)).unwrap();
            g.insert_interaction(interaction(2, Some(1), at + chrono::Duration::seconds(20)))
                .unwrap();
            g.insert_interaction(interaction(3, Some(2), at + chrono::Duration::seconds(40)))
                .unwrap();
            g.insert_interaction(interaction(4, Some(3), at + chrono::Duration::seconds(80)))
                .unwrap();

            for id in 1..=19u64 {
                g.insert_concept(concept(id, 1, 5, CanonizationStatus::None), iid(1))
                    .unwrap();
            }
            g.insert_concept(concept(20, 1, 5, CanonizationStatus::None), iid(1))
                .unwrap();
            // Distinct origins 1/2/3 covering 40/80 = 0.5 of the session.
            g.insert_concept(concept(31, 1, 0, CanonizationStatus::None), iid(1))
                .unwrap();
            g.insert_concept(concept(32, 2, 0, CanonizationStatus::None), iid(2))
                .unwrap();
            g.insert_concept(concept(33, 3, 0, CanonizationStatus::None), iid(3))
                .unwrap();
            g.upsert_edge(edge(1, 31, 20, at)).unwrap();
            g.upsert_edge(edge(2, 32, 20, at + chrono::Duration::seconds(20)))
                .unwrap();
            g.upsert_edge(edge(3, 33, 20, at + chrono::Duration::seconds(40)))
                .unwrap();

            let store = store_from_graph(&g).await;
            assert!(
                stage2_passes(&store, &sid(), nid(20), Duration::ZERO)
                    .await
                    .unwrap(),
                "fixture premise: hub must clear Stage 2"
            );

            let mut pairs: Vec<(u64, f64)> = (1..=19).map(|i| (i, 0.1)).collect();
            pairs.push((20, 1.0));
            let scores = table(&pairs);
            let (tx, mut rx) = channel();
            let mut ev = Evaluator::new();
            let outcome = eval_cycle(&mut ev, &mut g, &store, &scores, &tx, &params(), ts())
                .await
                .unwrap();

            assert_eq!(status_of(&g, nid(20)), CanonizationStatus::Candidate);
            let hops: Vec<_> = outcome
                .transitions()
                .filter(|e| e.node_id == nid(20))
                .map(|e| (e.from_status, e.to_status))
                .collect();
            assert_eq!(
                hops,
                vec![(CanonizationStatus::None, CanonizationStatus::Candidate)],
                "Stage 2 evidence must not skip a stage in one tick: {hops:?}"
            );
            assert_eq!(g.canonization_events().len(), 1);
            assert_eq!(drain_canonized(&mut rx).len(), 1);
        }

        #[tokio::test]
        async fn budget_demotes_lowest_blast_and_records_demotion() {
            let mut g = Graph::new(sid());
            g.insert_interaction(interaction(1, None, ts())).unwrap();

            let mut high = concept(10, 1, 5, CanonizationStatus::Canonical);
            high.blast_radius = Some(8);
            g.insert_concept(high, iid(1)).unwrap();
            attach_blast(&mut g, 10, 8, 100);

            let mut low = concept(11, 1, 5, CanonizationStatus::Canonical);
            low.blast_radius = Some(1);
            g.insert_concept(low, iid(1)).unwrap();
            attach_blast(&mut g, 11, 1, 200);

            let store = store_from_graph(&g).await;
            assert_eq!(
                store
                    .blast_radius(&sid(), nid(10), Duration::ZERO)
                    .await
                    .unwrap(),
                8
            );
            assert_eq!(
                store
                    .blast_radius(&sid(), nid(11), Duration::ZERO)
                    .await
                    .unwrap(),
                1
            );

            let mut p = params();
            p.max_canonical_nodes = 1;
            let (tx, mut rx) = channel();
            let mut ev = Evaluator::new();
            let now = ts();
            let outcome = eval_cycle(&mut ev, &mut g, &store, &table(&[]), &tx, &p, now)
                .await
                .unwrap();

            assert_eq!(status_of(&g, nid(10)), CanonizationStatus::Canonical);
            assert_eq!(status_of(&g, nid(11)), CanonizationStatus::None);
            match g.node(nid(11)) {
                Some(Node::Concept(c)) => {
                    assert_eq!(c.blast_radius, None);
                    assert_eq!(c.last_demotion_time, Some(now));
                }
                other => panic!("low-blast hub must remain a concept, got {other:?}"),
            }

            assert_eq!(outcome.demotions.len(), 1);
            let d = &outcome.demotions[0];
            assert_eq!(d.node_id, nid(11));
            assert_eq!(d.from_status, CanonizationStatus::Canonical);
            assert_eq!(d.to_status, CanonizationStatus::None);
            assert_eq!(d.blast_radius, None);
            assert_eq!(d.last_demotion_time, Some(now));

            let store_snap = store.load_session(&sid()).await.unwrap();
            let recorded: Vec<_> = g
                .canonization_events()
                .iter()
                .chain(store_snap.canonization_events.iter())
                .filter(|e| e.to_status == CanonizationStatus::None)
                .collect();
            assert_eq!(recorded.len(), 2, "graph + store each record the demotion");
            assert_eq!(drain_canonized(&mut rx).len(), 1);
        }

        #[tokio::test]
        async fn stage3_batch_is_capped_and_round_robins_score_desc() {
            let mut g = Graph::new(sid());
            g.insert_interaction(interaction(1, None, ts())).unwrap();
            // 55 Venerable, NodeId order = 1..=55.
            for id in 1..=55u64 {
                g.insert_concept(concept(id, 1, 0, CanonizationStatus::Venerable), iid(1))
                    .unwrap();
            }
            let store = store_from_graph(&g).await;
            // Higher id → higher score, so score-desc of window 1..=50 starts at 50.
            let pairs: Vec<(u64, f64)> = (1..=55).map(|i| (i, i as f64)).collect();
            let scores = table(&pairs);
            let mut p = params();
            p.batch_size = 50;
            p.max_canonical_nodes = 10_000;
            let (tx, _rx) = channel();
            let mut ev = Evaluator::new();

            let first = eval_cycle(&mut ev, &mut g, &store, &scores, &tx, &p, ts())
                .await
                .unwrap();
            assert_eq!(
                first.stage3_batch.len(),
                50,
                "first cycle considers at most 50"
            );
            assert_eq!(
                first.stage3_batch[0],
                nid(50),
                "score-desc within the NodeId window 1..=50"
            );
            let first_set: HashSet<NodeId> = first.stage3_batch.iter().copied().collect();
            assert!(first_set.contains(&nid(1)) && first_set.contains(&nid(50)));
            assert!(
                !first_set.contains(&nid(51)),
                "id 51 is past the first window: {:?}",
                first.stage3_batch
            );
            assert_eq!(ev.stage3_cursor(), 50);

            let second = eval_cycle(&mut ev, &mut g, &store, &scores, &tx, &p, ts())
                .await
                .unwrap();
            assert_eq!(second.stage3_batch.len(), 50);
            let second_set: HashSet<NodeId> = second.stage3_batch.iter().copied().collect();
            assert_ne!(
                first_set, second_set,
                "second cycle must be a different window"
            );
            assert!(
                second_set.contains(&nid(51)) && second_set.contains(&nid(55)),
                "round-robin must reach the tail: {:?}",
                second.stage3_batch
            );
            // Window is 51..=55 + 1..=45; max score in that set is 55.
            assert_eq!(second.stage3_batch[0], nid(55));
            assert_eq!(ev.stage3_cursor(), 45);
        }

        #[tokio::test]
        async fn emit_canonized_reaches_event_sender_subscriber() {
            let mut g = Graph::new(sid());
            g.insert_interaction(interaction(1, None, ts())).unwrap();
            for id in 1..=20u64 {
                g.insert_concept(concept(id, 1, 5, CanonizationStatus::None), iid(1))
                    .unwrap();
            }
            let store = store_from_graph(&g).await;
            let mut pairs: Vec<(u64, f64)> = (1..=19).map(|i| (i, 0.1)).collect();
            pairs.push((20, 1.0));
            let (tx, mut rx) = channel();
            let mut ev = Evaluator::new();
            eval_cycle(
                &mut ev,
                &mut g,
                &store,
                &table(&pairs),
                &tx,
                &params(),
                ts(),
            )
            .await
            .unwrap();

            let got = tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("Canonized within 1s")
                .unwrap();
            match got {
                DaemonEvent::Canonized { event } => {
                    assert_eq!(event.node_id, nid(20));
                    assert_eq!(event.from_status, CanonizationStatus::None);
                    assert_eq!(event.to_status, CanonizationStatus::Candidate);
                    assert!(event.last_demotion_time.is_none());
                }
                other => panic!("expected Canonized, got {other:?}"),
            }
        }

        #[tokio::test]
        async fn audit_rows_equal_committed_transitions() {
            let mut g = Graph::new(sid());
            g.insert_interaction(interaction(1, None, ts())).unwrap();
            for id in 1..=20u64 {
                g.insert_concept(concept(id, 1, 5, CanonizationStatus::None), iid(1))
                    .unwrap();
            }
            let store = store_from_graph(&g).await;
            let mut pairs: Vec<(u64, f64)> = (1..=19).map(|i| (i, 0.1)).collect();
            pairs.push((20, 1.0));
            let (tx, mut rx) = channel();
            let mut ev = Evaluator::new();
            let outcome = eval_cycle(
                &mut ev,
                &mut g,
                &store,
                &table(&pairs),
                &tx,
                &params(),
                ts(),
            )
            .await
            .unwrap();

            let committed: Vec<_> = outcome.transitions().cloned().collect();
            assert!(!committed.is_empty());
            assert_eq!(g.canonization_events(), committed.as_slice());
            assert_eq!(
                store
                    .load_session(&sid())
                    .await
                    .unwrap()
                    .canonization_events,
                committed
            );
            assert_eq!(drain_canonized(&mut rx), committed);
        }

        #[tokio::test]
        async fn stage3_promotion_capped_at_remaining_budget() {
            // P2 (phase R2): a cycle must never push the Canonical count over
            // max_canonical_nodes. A Venerable that would overflow stays
            // Venerable; the pre-existing Canonical is not displaced and no
            // same-tick promote-then-demote occurs (the original P1-1).
            let mut g = Graph::new(sid());
            g.insert_interaction(interaction(1, None, ts())).unwrap();

            // Pre-existing Canonical, blast 8.
            let mut existing = concept(10, 1, 5, CanonizationStatus::Canonical);
            existing.blast_radius = Some(8);
            g.insert_concept(existing, iid(1)).unwrap();
            attach_blast(&mut g, 10, 8, 100);

            // Venerable that would clear Stage 3 (blast 6 > 5).
            let venerable = concept(20, 1, 5, CanonizationStatus::Venerable);
            g.insert_concept(venerable, iid(1)).unwrap();
            attach_blast(&mut g, 20, 6, 200);

            let store = store_from_graph(&g).await;
            let mut p = params();
            p.max_canonical_nodes = 1;
            let (tx, mut rx) = channel();
            let mut ev = Evaluator::new();
            let outcome = eval_cycle(&mut ev, &mut g, &store, &table(&[]), &tx, &p, ts())
                .await
                .unwrap();

            assert_eq!(
                status_of(&g, nid(20)),
                CanonizationStatus::Venerable,
                "budget full: the Venerable must wait, not overflow"
            );
            assert_eq!(
                status_of(&g, nid(10)),
                CanonizationStatus::Canonical,
                "the pre-existing Canonical is untouched when not over budget"
            );
            assert!(outcome.promotions.is_empty(), "no promotion over budget");
            assert!(outcome.demotions.is_empty(), "no demotion at exact budget");
            assert_eq!(canonical_count(&g), 1, "count stays within budget");
            assert!(
                drain_canonized(&mut rx).is_empty(),
                "no transitions committed"
            );
        }

        #[tokio::test]
        async fn flush_after_eval_does_not_duplicate_audit_rows() {
            // P1-2: record_canonization (immediate) + write-behind flush must not
            // double the demo audit trail. Reload after drain_log+flush must carry
            // each committed transition exactly once.
            let mut g = Graph::new(sid());
            g.insert_interaction(interaction(1, None, ts())).unwrap();
            for id in 1..=20u64 {
                g.insert_concept(concept(id, 1, 5, CanonizationStatus::None), iid(1))
                    .unwrap();
            }
            let store = store_from_graph(&g).await;
            let mut pairs: Vec<(u64, f64)> = (1..=19).map(|i| (i, 0.1)).collect();
            pairs.push((20, 1.0));
            let (tx, _rx) = channel();
            let mut ev = Evaluator::new();
            let outcome = eval_cycle(
                &mut ev,
                &mut g,
                &store,
                &table(&pairs),
                &tx,
                &params(),
                ts(),
            )
            .await
            .unwrap();
            let committed: Vec<_> = outcome.transitions().cloned().collect();
            assert_eq!(committed.len(), 1, "one None→Candidate hop");

            // Replay the write-behind log (the same transition as the live write).
            let batch = g.drain_log();
            assert!(!batch.is_empty());
            store.flush(&batch).await.unwrap();

            let reloaded = store
                .load_session(&sid())
                .await
                .unwrap()
                .canonization_events;
            assert_eq!(
                reloaded.len(),
                committed.len(),
                "flush must not duplicate the audit trail"
            );
            assert_eq!(reloaded, committed, "reloaded audit matches committed hops");
        }
    }

    #[cfg(all(feature = "store-memory", feature = "fixtures"))]
    mod fixture {
        use super::*;
        use crate::config::ScoringWeights;
        use crate::daemon::score::rescore;
        use crate::store::GraphStore;

        fn rest_sid() -> SessionId {
            SessionId::from("session-rest-api")
        }

        fn rewind_canonicals(snap: &mut crate::types::GraphSnapshot) {
            for c in snap.concepts.iter_mut() {
                if c.canonization_status == CanonizationStatus::Canonical {
                    c.canonization_status = CanonizationStatus::None;
                    c.blast_radius = None;
                }
            }
        }

        fn find_id(graph: &Graph, content: &str) -> NodeId {
            graph
                .concepts()
                .find(|c| c.content == content)
                .unwrap_or_else(|| panic!("{content} present"))
                .id
        }

        fn hops_for<'a>(
            events: impl IntoIterator<Item = &'a CanonizationEvent>,
            id: NodeId,
        ) -> Vec<(CanonizationStatus, CanonizationStatus)> {
            events
                .into_iter()
                .filter(|e| e.node_id == id)
                .map(|e| (e.from_status, e.to_status))
                .collect()
        }

        #[tokio::test]
        async fn rest_api_user_schema_progresses_three_hops_with_audit() {
            let mut snap = crate::fixtures::load_snapshot("session-rest-api").unwrap();
            rewind_canonicals(&mut snap);
            let mut graph = Graph::from_snapshot(snap.clone()).unwrap();
            let store = crate::store::MemoryStore::new();
            store.seed(snap).unwrap();

            let us = find_id(&graph, "user schema");
            assert_eq!(status_of(&graph, us), CanonizationStatus::None);

            let scores = ScoreTable {
                epoch: graph.epoch(),
                ranked: rescore(&graph, &ScoringWeights::default()),
            };
            let mut p = params();
            p.min_peer_count = crate::Config::default().canonization_min_peer_count;

            let (tx, mut rx) = crate::daemon::events::event_channel();
            let mut ev = Evaluator::new();
            let mut all_committed = Vec::new();
            for _ in 0..3 {
                let outcome = eval_cycle(&mut ev, &mut graph, &store, &scores, &tx, &p, ts())
                    .await
                    .unwrap();
                all_committed.extend(outcome.transitions().cloned());
            }

            assert_eq!(status_of(&graph, us), CanonizationStatus::Canonical);
            match graph.node(us) {
                Some(Node::Concept(c)) => {
                    assert_eq!(c.blast_radius, Some(8), "Stage 3 must stamp measured blast");
                    assert!(c.last_demotion_time.is_none());
                }
                other => panic!("user schema must be a concept, got {other:?}"),
            }

            let us_hops = hops_for(graph.canonization_events(), us);
            assert_eq!(
                us_hops,
                vec![
                    (CanonizationStatus::None, CanonizationStatus::Candidate),
                    (CanonizationStatus::Candidate, CanonizationStatus::Venerable),
                    (CanonizationStatus::Venerable, CanonizationStatus::Canonical),
                ],
                "one row per hop; a skipped audit would drop a pair: {us_hops:?}"
            );
            assert_eq!(
                hops_for(&all_committed, us),
                us_hops,
                "outcome transitions must match the in-graph audit"
            );
            let store_hops = hops_for(
                &store
                    .load_session(&rest_sid())
                    .await
                    .unwrap()
                    .canonization_events,
                us,
            );
            assert_eq!(store_hops, us_hops, "store audit must have one row per hop");

            let emitted = {
                let mut out = Vec::new();
                while let Ok(DaemonEvent::Canonized { event }) = rx.try_recv() {
                    out.push(event);
                }
                out
            };
            assert_eq!(
                hops_for(&emitted, us),
                us_hops,
                "emit_canonized must fire once per hop"
            );
        }

        #[tokio::test]
        async fn rest_api_api_layer_reaches_venerable_never_canonical() {
            let mut snap = crate::fixtures::load_snapshot("session-rest-api").unwrap();
            let api_id = snap
                .concepts
                .iter()
                .find(|c| c.content == "api layer")
                .expect("api layer present")
                .id;
            {
                let api = snap
                    .concepts
                    .iter_mut()
                    .find(|c| c.id == api_id)
                    .expect("api layer present");
                // Start at Candidate so Stage 2 is the first hop; blast 1
                // must refuse Canonical.
                api.canonization_status = CanonizationStatus::Candidate;
            }
            let mut graph = Graph::from_snapshot(snap.clone()).unwrap();
            let store = crate::store::MemoryStore::new();
            store.seed(snap).unwrap();
            let scores = ScoreTable {
                epoch: graph.epoch(),
                ranked: rescore(&graph, &ScoringWeights::default()),
            };
            let (tx, _rx) = crate::daemon::events::event_channel();
            let mut ev = Evaluator::new();
            for _ in 0..3 {
                eval_cycle(&mut ev, &mut graph, &store, &scores, &tx, &params(), ts())
                    .await
                    .unwrap();
            }
            assert_eq!(status_of(&graph, api_id), CanonizationStatus::Venerable);
            let hops = hops_for(graph.canonization_events(), api_id);
            assert!(
                hops.iter().any(|&(f, t)| f == CanonizationStatus::Candidate
                    && t == CanonizationStatus::Venerable),
                "api layer must hop to Venerable: {hops:?}"
            );
            assert!(
                hops.iter()
                    .all(|&(_, t)| t != CanonizationStatus::Canonical),
                "api layer blast=1 must never become Canonical: {hops:?}"
            );
        }
    }
}
