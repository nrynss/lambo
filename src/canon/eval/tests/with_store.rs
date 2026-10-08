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
    store.flush(&batch, None).await.unwrap();
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
        event_time: None,
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
    let err = commit_transition(&mut g, &tx, bad).unwrap_err();
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
    let g = RwLock::new(g);
    assert!(
        stage2_passes(&store, &sid(), nid(20), Duration::ZERO, Utc::now())
            .await
            .unwrap(),
        "fixture premise: hub must clear Stage 2"
    );

    let mut pairs: Vec<(u64, f64)> = (1..=19).map(|i| (i, 0.1)).collect();
    pairs.push((20, 1.0));
    let scores = table(&pairs);
    let (tx, mut rx) = channel();
    let mut ev = Evaluator::new();
    let outcome = eval_cycle(&mut ev, &g, &store, &scores, &tx, &params(), ts())
        .await
        .unwrap();

    assert_eq!(status_of(&g.read(), nid(20)), CanonizationStatus::Candidate);
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
    assert_eq!(g.read().canonization_events().len(), 1);
    assert_eq!(drain_canonized(&mut rx).len(), 1);
}
// -------------------------------------------------------------------
// C-R1-1 closure — the published bands drive the ladder under solo
// -------------------------------------------------------------------

/// A solo Candidate whose resistant score sits exactly on the
/// Venerable bar climbs without stage-2 evidence. Fixture: Entity hub
/// with one extra Derives support whose about-time clusters with the
/// origin turn (one recurrence session), one human confirmation →
/// raw (1 + 4) × 1.2 = **6.0**, the inclusive bar. Stage-2 span
/// evidence cannot pass — the hub traces to two distinct origin
/// interactions, below MIN_DISTINCT — so pre-closure this hop was
/// unreachable and the concept sat at Candidate forever while
/// `classify` reported Venerable.
///
/// Mutations: revert apply's stage-2 loop to `verdicts.stage2_pass`
/// (bands unreachable again) → red. `SoloScorer::admits_hop` returning
/// `_evidence`, or its comparison made exclusive (`>`) → red.
#[tokio::test]
async fn solo_band_drives_the_candidate_to_venerable_hop() {
    let mut g = Graph::new(sid());
    let mut prev = None;
    for n in 1..=2u64 {
        let mut turn = interaction(n, prev, ts());
        turn.event_time = Some(ts() - chrono::Duration::hours(n as i64));
        g.insert_interaction(turn).unwrap();
        prev = Some(n);
    }
    let mut hub = concept(10, 1, 0, CanonizationStatus::Candidate);
    hub.concept_type = ConceptType::Entity;
    hub.human_confirmed = 1;
    let hub_id = hub.id;
    g.insert_concept(hub, iid(1)).unwrap();
    g.upsert_edge(Edge {
        id: eid(2),
        session_id: sid(),
        source: iid(2),
        target: hub_id,
        edge_type: EdgeType::Derives,
        weight: 0.9,
        reinforcements: 1,
        created_at: ts(),
        last_reinforced: ts(),
        event_time: Some(ts() - chrono::Duration::hours(2)),
    })
    .unwrap();

    let store = store_from_graph(&g).await;
    let g = RwLock::new(g);
    assert!(
        !stage2_passes(&store, &sid(), hub_id, Duration::ZERO, ts())
            .await
            .unwrap(),
        "fixture premise: two distinct origins must fail stage 2"
    );

    let solo_params = EvalParams {
        promotion_policy: PromotionPolicy::Solo,
        ..params()
    };
    let (tx, _rx) = channel();
    let mut ev = Evaluator::new();
    let outcome = eval_cycle(&mut ev, &g, &store, &table(&[]), &tx, &solo_params, ts())
        .await
        .unwrap();
    assert_eq!(status_of(&g.read(), hub_id), CanonizationStatus::Venerable);
    let hops: Vec<_> = outcome
        .transitions()
        .filter(|e| e.node_id == hub_id)
        .map(|e| (e.from_status, e.to_status))
        .collect();
    assert_eq!(
        hops,
        vec![(CanonizationStatus::Candidate, CanonizationStatus::Venerable)]
    );
}

/// The Canonical bar likewise: a solo Venerable at 10.8 (one
/// recurrence session + two confirmations on an Entity) reaches
/// Canonical on the band alone — blast-radius evidence fails (no
/// dependents), so pre-closure this hop was unreachable too.
///
/// Mutations: revert apply's stage-3 loop to `verdicts.stage3_pass`
/// → red; `admits_hop` returning `_evidence` → red.
#[tokio::test]
async fn solo_band_drives_the_venerable_to_canonical_hop() {
    let mut g = Graph::new(sid());
    let mut prev = None;
    for n in 1..=2u64 {
        let mut turn = interaction(n, prev, ts());
        turn.event_time = Some(ts() - chrono::Duration::hours(n as i64));
        g.insert_interaction(turn).unwrap();
        prev = Some(n);
    }
    let mut hub = concept(10, 1, 0, CanonizationStatus::Venerable);
    hub.concept_type = ConceptType::Entity;
    hub.human_confirmed = 2;
    let hub_id = hub.id;
    g.insert_concept(hub, iid(1)).unwrap();
    g.upsert_edge(Edge {
        id: eid(2),
        session_id: sid(),
        source: iid(2),
        target: hub_id,
        edge_type: EdgeType::Derives,
        weight: 0.9,
        reinforcements: 1,
        created_at: ts(),
        last_reinforced: ts(),
        event_time: Some(ts() - chrono::Duration::hours(2)),
    })
    .unwrap();

    let store = store_from_graph(&g).await;
    let g = RwLock::new(g);
    let solo_params = EvalParams {
        promotion_policy: PromotionPolicy::Solo,
        ..params()
    };
    let (tx, _rx) = channel();
    let mut ev = Evaluator::new();
    let outcome = eval_cycle(&mut ev, &g, &store, &table(&[]), &tx, &solo_params, ts())
        .await
        .unwrap();
    assert_eq!(status_of(&g.read(), hub_id), CanonizationStatus::Canonical);
    let hop = outcome
        .transitions()
        .find(|e| e.node_id == hub_id)
        .expect("the canonical hop must be audited");
    assert_eq!(
        (hop.from_status, hop.to_status),
        (CanonizationStatus::Venerable, CanonizationStatus::Canonical)
    );
}

/// Score-driven stage-3 admission bypasses the verdict phase — where
/// the re-promotion cooldown normally gates — so apply gates it
/// itself: a cooling Venerable stays put no matter what its band says.
///
/// Mutation: drop the `in_repromotion_cooldown` check on the
/// score-admitted path → red.
#[tokio::test]
async fn solo_score_admission_still_honors_the_repromotion_cooldown() {
    let mut g = Graph::new(sid());
    let mut prev = None;
    for n in 1..=2u64 {
        let mut turn = interaction(n, prev, ts());
        turn.event_time = Some(ts() - chrono::Duration::hours(n as i64));
        g.insert_interaction(turn).unwrap();
        prev = Some(n);
    }
    let mut hub = concept(10, 1, 0, CanonizationStatus::Venerable);
    hub.concept_type = ConceptType::Entity;
    hub.human_confirmed = 2;
    hub.last_demotion_time = Some(ts() - chrono::Duration::seconds(60));
    let hub_id = hub.id;
    g.insert_concept(hub, iid(1)).unwrap();
    g.upsert_edge(Edge {
        id: eid(2),
        session_id: sid(),
        source: iid(2),
        target: hub_id,
        edge_type: EdgeType::Derives,
        weight: 0.9,
        reinforcements: 1,
        created_at: ts(),
        last_reinforced: ts(),
        event_time: Some(ts() - chrono::Duration::hours(2)),
    })
    .unwrap();

    let store = store_from_graph(&g).await;
    let g = RwLock::new(g);
    let solo_params = EvalParams {
        promotion_policy: PromotionPolicy::Solo,
        ..params()
    };
    let (tx, _rx) = channel();
    let mut ev = Evaluator::new();
    let outcome = eval_cycle(&mut ev, &g, &store, &table(&[]), &tx, &solo_params, ts())
        .await
        .unwrap();
    assert_eq!(status_of(&g.read(), hub_id), CanonizationStatus::Venerable);
    assert!(
        outcome.promotions.is_empty(),
        "cooling node must not climb on the band alone: {:?}",
        outcome.promotions
    );
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
    let g = RwLock::new(g);
    assert_eq!(
        store
            .blast_radius(&sid(), nid(10), Duration::ZERO, Utc::now())
            .await
            .unwrap(),
        8
    );
    assert_eq!(
        store
            .blast_radius(&sid(), nid(11), Duration::ZERO, Utc::now())
            .await
            .unwrap(),
        1
    );

    let mut p = params();
    p.max_canonical_nodes = 1;
    let (tx, mut rx) = channel();
    let mut ev = Evaluator::new();
    let now = ts();
    let outcome = eval_cycle(&mut ev, &g, &store, &table(&[]), &tx, &p, now)
        .await
        .unwrap();

    assert_eq!(status_of(&g.read(), nid(10)), CanonizationStatus::Canonical);
    assert_eq!(status_of(&g.read(), nid(11)), CanonizationStatus::None);
    match g.read().node(nid(11)) {
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
    let graph_events = g.read().canonization_events().to_vec();
    let recorded: Vec<_> = graph_events
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
    let g = RwLock::new(g);
    // Higher id → higher score, so score-desc of window 1..=50 starts at 50.
    let pairs: Vec<(u64, f64)> = (1..=55).map(|i| (i, i as f64)).collect();
    let scores = table(&pairs);
    let mut p = params();
    p.batch_size = 50;
    p.max_canonical_nodes = 10_000;
    let (tx, _rx) = channel();
    let mut ev = Evaluator::new();

    let first = eval_cycle(&mut ev, &g, &store, &scores, &tx, &p, ts())
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
    assert_eq!(ev.stage3_cursor(), Some(nid(50)));

    let second = eval_cycle(&mut ev, &g, &store, &scores, &tx, &p, ts())
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
    assert_eq!(ev.stage3_cursor(), Some(nid(45)));
}

/// `MemoryStore` with an injectable fault, so the cycle's
/// store-facing phases can be exercised (F19 / R2-4: previously
/// unasserted). Everything else delegates.
struct FaultyStore {
    inner: MemoryStore,
    /// Phase 4 — the durable audit write (F3).
    fail_record: bool,
    /// Phase 2 — Stage 2's span query (R2-4).
    fail_span: bool,
}

impl FaultyStore {
    fn record_fails(inner: MemoryStore) -> Self {
        Self {
            inner,
            fail_record: true,
            fail_span: false,
        }
    }

    fn span_fails(inner: MemoryStore) -> Self {
        Self {
            inner,
            fail_record: false,
            fail_span: true,
        }
    }
}

#[async_trait::async_trait]
impl GraphStore for FaultyStore {
    async fn init_schema(&self) -> Result<(), crate::types::StoreError> {
        self.inner.init_schema().await
    }
    fn capabilities(&self) -> crate::store::Capabilities {
        self.inner.capabilities()
    }
    async fn flush(
        &self,
        batch: &MutationBatch,
        token: Option<u64>,
    ) -> Result<(), crate::types::StoreError> {
        self.inner.flush(batch, token).await
    }
    async fn load_session(
        &self,
        session: &SessionId,
    ) -> Result<crate::types::GraphSnapshot, crate::types::StoreError> {
        self.inner.load_session(session).await
    }
    async fn keyword_candidates(
        &self,
        session: &SessionId,
        tokens: &[String],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, crate::types::StoreError> {
        self.inner.keyword_candidates(session, tokens, limit).await
    }
    async fn vector_candidates(
        &self,
        session: &SessionId,
        embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, crate::types::StoreError> {
        self.inner
            .vector_candidates(session, embedding, limit)
            .await
    }
    async fn vector_candidates_checked(
        &self,
        session: &SessionId,
        embedding: &[f32],
        expected_contract: &crate::types::EmbeddingContract,
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, crate::types::StoreError> {
        self.inner
            .vector_candidates_checked(session, embedding, expected_contract, limit)
            .await
    }
    async fn blast_radius(
        &self,
        session: &SessionId,
        node: NodeId,
        min_edge_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<u64, crate::types::StoreError> {
        self.inner
            .blast_radius(session, node, min_edge_age, now)
            .await
    }
    async fn interaction_span(
        &self,
        session: &SessionId,
        node: NodeId,
        min_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<crate::types::InteractionSpan, crate::types::StoreError> {
        if self.fail_span {
            return Err(StoreError::Backend("interaction_span is down".into()));
        }
        self.inner
            .interaction_span(session, node, min_age, now)
            .await
    }
    async fn record_canonization(
        &self,
        event: &CanonizationEvent,
        token: Option<u64>,
    ) -> Result<(), crate::types::StoreError> {
        if self.fail_record {
            return Err(StoreError::Backend("record_canonization is down".into()));
        }
        self.inner.record_canonization(event, token).await
    }
}

/// F3: the graph apply is the commit point. A `record_canonization`
/// failure mid-cycle must not lose the `DaemonEvent::Canonized` (the
/// old order emitted only *after* the durable write, so a store hiccup
/// dropped the event forever — the flush replay re-records the row but
/// publishes nothing), and must not discard the `EvalOutcome` naming
/// the hops the graph has already committed.
///
/// Realistic trigger: `NotFound` for a concept the flush loop has not
/// persisted yet — the graph runs ahead of the store by one flush
/// interval by design.
#[tokio::test]
async fn record_failure_keeps_the_emitted_event_and_the_partial_outcome() {
    let mut g = Graph::new(sid());
    g.insert_interaction(interaction(1, None, ts())).unwrap();
    for id in 1..=20u64 {
        g.insert_concept(concept(id, 1, 5, CanonizationStatus::None), iid(1))
            .unwrap();
    }
    let store = FaultyStore::record_fails(store_from_graph(&g).await);
    let g = RwLock::new(g);
    let mut pairs: Vec<(u64, f64)> = (1..=19).map(|i| (i, 0.1)).collect();
    pairs.push((20, 1.0));
    let (tx, mut rx) = channel();
    let mut ev = Evaluator::new();

    let err = eval_cycle(&mut ev, &g, &store, &table(&pairs), &tx, &params(), ts())
        .await
        .unwrap_err();
    assert!(
        err.source
            .to_string()
            .contains("record_canonization is down"),
        "the store error must surface: {err}"
    );
    assert_eq!(
        err.outcome
            .transitions()
            .map(|e| (e.node_id, e.to_status))
            .collect::<Vec<_>>(),
        vec![(nid(20), CanonizationStatus::Candidate)],
        "the partial outcome must name the hop the graph committed"
    );
    assert_eq!(
        status_of(&g.read(), nid(20)),
        CanonizationStatus::Candidate,
        "the graph apply is the commit point; it stands"
    );
    assert_eq!(
        g.read().canonization_events().len(),
        1,
        "the in-graph audit carries the hop"
    );
    let emitted = drain_canonized(&mut rx);
    assert_eq!(
        emitted.len(),
        1,
        "the Canonized event must survive a failed durable write"
    );
    assert_eq!(emitted[0].node_id, nid(20));
    // And the write-behind log still carries it to the store later.
    assert!(!g.write().drain_log().is_empty());
}

/// R2-4: a phase-2 store fault must not hold back Stage 1, which
/// needs no verdict at all.
///
/// `verdicts()` issues every Stage-2/Stage-3 query before anything
/// commits — that atomicity is deliberate — but it also meant a single
/// failing `interaction_span` returned an EvalError with an *empty*
/// outcome, stalling progression at the first stage for as long as the
/// store was unhealthy. The Stage-1 hop must commit, emit, and appear
/// in the partial outcome; Stages 2/3 must be dropped whole rather
/// than guessed at.
#[tokio::test]
async fn store_fault_still_lands_the_io_free_stage_1_hops() {
    let mut g = Graph::new(sid());
    g.insert_interaction(interaction(1, None, ts())).unwrap();
    for id in 1..=20u64 {
        g.insert_concept(concept(id, 1, 5, CanonizationStatus::None), iid(1))
            .unwrap();
    }
    // One pre-existing Candidate, so Stage 2 has a window member and
    // the cycle actually reaches the failing query.
    g.insert_concept(concept(21, 1, 5, CanonizationStatus::Candidate), iid(1))
        .unwrap();
    let store = FaultyStore::span_fails(store_from_graph(&g).await);
    let g = RwLock::new(g);
    let mut pairs: Vec<(u64, f64)> = (1..=19).map(|i| (i, 0.1)).collect();
    pairs.push((20, 1.0));
    pairs.push((21, 0.1));
    let (tx, mut rx) = channel();
    let mut ev = Evaluator::new();

    let err = eval_cycle(&mut ev, &g, &store, &table(&pairs), &tx, &params(), ts())
        .await
        .unwrap_err();
    assert!(
        err.source.to_string().contains("interaction_span is down"),
        "the store error must still surface: {err}"
    );
    assert_eq!(
        err.outcome
            .transitions()
            .map(|e| (e.node_id, e.to_status))
            .collect::<Vec<_>>(),
        vec![(nid(20), CanonizationStatus::Candidate)],
        "the I/O-free Stage-1 hop must land and be reported"
    );
    assert_eq!(status_of(&g.read(), nid(20)), CanonizationStatus::Candidate);
    assert_eq!(
        status_of(&g.read(), nid(21)),
        CanonizationStatus::Candidate,
        "Stage 2 had no verdict, so it must not have run"
    );
    assert!(
        err.outcome.stage3_batch.is_empty(),
        "Stage 3 was dropped whole: {:?}",
        err.outcome.stage3_batch
    );
    let emitted = drain_canonized(&mut rx);
    assert_eq!(emitted.len(), 1, "the commit point still emits");
    assert_eq!(emitted[0].node_id, nid(20));
    // Phase 4 was skipped (the store is what failed); the write-behind
    // log is what carries the hop to it.
    assert!(!g.write().drain_log().is_empty());
}

/// Graph carrying only `hubs` (as Venerable) plus the seed
/// interaction — blast radius comes from the store, so the evaluated
/// graph needs no dependents.
fn venerable_graph(hubs: &[u64]) -> Graph {
    let mut g = Graph::new(sid());
    g.insert_interaction(interaction(1, None, ts())).unwrap();
    for &h in hubs {
        g.insert_concept(concept(h, 1, 0, CanonizationStatus::Venerable), iid(1))
            .unwrap();
    }
    g
}

/// Store view of the same session: every hub with `blast` exclusive
/// dependents, so `blast_radius(hub) == blast`.
async fn store_with_hubs(hubs: &[(u64, u64)]) -> MemoryStore {
    let mut full = Graph::new(sid());
    full.insert_interaction(interaction(1, None, ts())).unwrap();
    for (i, &(h, blast)) in hubs.iter().enumerate() {
        full.insert_concept(concept(h, 1, 0, CanonizationStatus::Venerable), iid(1))
            .unwrap();
        attach_blast(&mut full, h, blast, 1_000 + 100 * i as u64);
    }
    store_from_graph(&full).await
}

/// F1: every successful promotion removes ring members, so a
/// **positional** cursor into the rebuilt ring skids past the
/// longest-waiting nodes. 6 Venerables, `batch_size = 2`: cycle 1
/// evaluates [1,2] and promotes both; cycle 2's ring is [3,4,5,6] and
/// must resume at 3. The positional cursor computed `2 % 4 = 2` and
/// produced [5,6] — 3 and 4 skipped, on every promoting cycle.
#[tokio::test]
async fn promotion_churn_does_not_skip_the_next_ring_members() {
    let hubs = [1u64, 2, 3, 4, 5, 6];
    let store = store_with_hubs(&hubs.map(|h| (h, 6))).await;
    let g = RwLock::new(venerable_graph(&hubs));
    let mut p = params();
    p.batch_size = 2;
    p.max_canonical_nodes = 10_000;
    let (tx, _rx) = channel();
    let mut ev = Evaluator::new();

    let first = eval_cycle(&mut ev, &g, &store, &table(&[]), &tx, &p, ts())
        .await
        .unwrap();
    assert_eq!(first.stage3_batch, vec![nid(1), nid(2)]);
    assert_eq!(first.promotions.len(), 2, "both must promote (blast 6)");
    assert_eq!(ev.stage3_cursor(), Some(nid(2)));

    let second = eval_cycle(&mut ev, &g, &store, &table(&[]), &tx, &p, ts())
        .await
        .unwrap();
    assert_eq!(
        second.stage3_batch,
        vec![nid(3), nid(4)],
        "the ring shrank under the cursor; the next window must resume \
                 at the first id after the last one evaluated"
    );
}

/// F1: with a steady Stage-2 inflow the positional skid repeats and
/// the same victims are starved **forever** — the review's
/// demonstration, reproduced.
///
/// Victims 10 and 11 have blast 0, so they never promote and never
/// leave the ring. Each cycle two fresh Venerables arrive and promote
/// out, alternately sorting *before* and *after* the victims, which
/// keeps a 4-element ring whose positional cursor alternates 0 → 2 →
/// 0 → 2 and lands on the inflow every time: [1,2], [20,21], [3,4],
/// [22,23] — the victims are never evaluated, for as long as the
/// session keeps producing Venerables. The identity cursor reaches
/// them on cycle 2, because "the first id after 2" is 10 whatever the
/// ring did in between.
#[tokio::test]
async fn sustained_inflow_does_not_starve_the_waiting_ring_members() {
    // Everything promotes (blast 6) except the two victims.
    let store = store_with_hubs(&[
        (1, 6),
        (2, 6),
        (3, 6),
        (4, 6),
        (10, 0),
        (11, 0),
        (20, 6),
        (21, 6),
        (22, 6),
        (23, 6),
    ])
    .await;
    let g = RwLock::new(venerable_graph(&[1, 2, 10, 11]));
    let mut p = params();
    p.batch_size = 2;
    p.max_canonical_nodes = 10_000;
    let (tx, _rx) = channel();
    let mut ev = Evaluator::new();

    // Alternating inflow, sustained: after the victims, then before,
    // then after… The alternation is what pins the positional cursor
    // to 0 → 2 → 0 → 2 while the ring stays four wide.
    let inflow = [[20u64, 21], [3, 4], [22, 23], [5, 6]];
    let mut evaluated: Vec<NodeId> = Vec::new();
    for cycle in 0..4 {
        let outcome = eval_cycle(&mut ev, &g, &store, &table(&[]), &tx, &p, ts())
            .await
            .unwrap();
        evaluated.extend(outcome.stage3_batch);
        if let Some(fresh) = inflow.get(cycle) {
            let mut graph = g.write();
            for &id in fresh {
                graph
                    .insert_concept(concept(id, 1, 0, CanonizationStatus::Venerable), iid(1))
                    .unwrap();
            }
        }
    }

    assert!(
        evaluated.contains(&nid(10)) && evaluated.contains(&nid(11)),
        "the longest-waiting Venerables must be evaluated, not starved \
                 by the inflow: {evaluated:?}"
    );
    assert!(
        evaluated.iter().filter(|&&id| id == nid(20)).count() == 1 && evaluated.contains(&nid(22)),
        "the inflow is still served — fairness, not victim priority: \
                 {evaluated:?}"
    );
    assert_eq!(
        status_of(&g.read(), nid(10)),
        CanonizationStatus::Venerable,
        "a blast-0 victim stays Venerable; being evaluated is the point"
    );
}

/// F1/F10: with the Canonical budget full the ring must not rotate at
/// all, and `stage3_batch` must report nothing — it names the window
/// the predicate **ran on**. The old shape took the window first and
/// broke out of the promotion loop, so the cursor advanced over nodes
/// that were never evaluated and the outcome claimed them anyway.
#[tokio::test]
async fn full_budget_evaluates_nothing_and_does_not_rotate_the_ring() {
    let store = store_with_hubs(&[(1, 6), (2, 6), (3, 6)]).await;
    let mut graph = venerable_graph(&[1, 2, 3]);
    // One pre-existing Canonical fills a budget of 1.
    let mut full = concept(10, 1, 5, CanonizationStatus::Canonical);
    full.blast_radius = Some(8);
    graph.insert_concept(full, iid(1)).unwrap();
    let g = RwLock::new(graph);

    let mut p = params();
    p.batch_size = 2;
    p.max_canonical_nodes = 1;
    let (tx, _rx) = channel();
    let mut ev = Evaluator::new();

    let outcome = eval_cycle(&mut ev, &g, &store, &table(&[]), &tx, &p, ts())
        .await
        .unwrap();
    assert!(
        outcome.stage3_batch.is_empty(),
        "no budget, so nothing was evaluated: {:?}",
        outcome.stage3_batch
    );
    assert!(outcome.promotions.is_empty());
    assert_eq!(
        ev.stage3_cursor(),
        None,
        "the ring must not rotate over nodes the cycle could not evaluate"
    );
}

/// F19: budget contention — `remaining == 1` with two eligible
/// Venerables. Exactly one may promote, and spec §10's
/// score-descending order within the batch is what decides which: the
/// higher-scoring node wins even though it sorts later in the ring.
/// (Ranking the window before spending the budget is what makes that
/// true; spending it in ring order would hand the slot to the lowest
/// NodeId instead.)
///
/// R2-2: both nodes are *evaluated* — the budget cut happens in
/// `apply`, on the nodes that passed, so `stage3_batch` names the
/// loser too. It has to: which of them can pass is not knowable at
/// gather time, and pre-selecting the top `remaining` starved the
/// ring whenever the top scorer could not pass.
#[tokio::test]
async fn budget_contention_gives_the_last_slot_to_the_higher_score() {
    let store = store_with_hubs(&[(1, 6), (2, 6)]).await;
    let g = RwLock::new(venerable_graph(&[1, 2]));
    let mut p = params();
    p.max_canonical_nodes = 1;
    let (tx, _rx) = channel();
    let mut ev = Evaluator::new();

    // Ring order is [1, 2]; scores put 2 first.
    let outcome = eval_cycle(
        &mut ev,
        &g,
        &store,
        &table(&[(1, 0.1), (2, 0.9)]),
        &tx,
        &p,
        ts(),
    )
    .await
    .unwrap();

    assert_eq!(
        outcome.stage3_batch,
        vec![nid(2), nid(1)],
        "the whole window is evaluated, score-descending"
    );
    assert_eq!(outcome.promotions.len(), 1);
    assert_eq!(outcome.promotions[0].node_id, nid(2));
    assert_eq!(status_of(&g.read(), nid(1)), CanonizationStatus::Venerable);
    assert_eq!(status_of(&g.read(), nid(2)), CanonizationStatus::Canonical);
    assert_eq!(canonical_count(&g.read()), 1);
}

/// R2-2: a top-scoring Venerable that cannot pass must not hold the
/// last budget slot hostage.
///
/// Ring `[A(score .9, blast 2), B(.5, blast 8), C(.4, blast 8)]` with
/// `max_canonical_nodes = 1`. A fails Stage 3's `> 5`; B and C pass.
/// While gather truncated the score-ranked window to the remaining
/// budget, `stage3_batch` was `[A]` on all ten cycles and the budget
/// never filled — the ring fits in `batch_size`, so the truncation
/// re-selected the same blocked node forever. B must promote on the
/// first cycle, and the remaining nine must then evaluate nothing
/// (budget full).
#[tokio::test]
async fn a_blocked_top_scorer_does_not_starve_the_rest_of_the_ring() {
    let store = store_with_hubs(&[(1, 2), (2, 8), (3, 8)]).await;
    let g = RwLock::new(venerable_graph(&[1, 2, 3]));
    let mut p = params();
    p.max_canonical_nodes = 1;
    let scores = table(&[(1, 0.9), (2, 0.5), (3, 0.4)]);
    let (tx, _rx) = channel();
    let mut ev = Evaluator::new();

    let mut promoted = Vec::new();
    for cycle in 0..10 {
        let outcome = eval_cycle(&mut ev, &g, &store, &scores, &tx, &p, ts())
            .await
            .unwrap();
        promoted.extend(outcome.promotions.iter().map(|e| e.node_id));
        if cycle == 0 {
            assert_eq!(
                outcome.stage3_batch,
                vec![nid(1), nid(2), nid(3)],
                "the whole ring is evaluated score-descending, not just \
                         the top slot's worth"
            );
        } else {
            assert!(
                outcome.stage3_batch.is_empty(),
                "budget full from cycle 1 on: {:?}",
                outcome.stage3_batch
            );
        }
    }

    assert_eq!(
        promoted,
        vec![nid(2)],
        "the highest-scoring node that can actually pass takes the slot"
    );
    assert_eq!(status_of(&g.read(), nid(2)), CanonizationStatus::Canonical);
    assert_eq!(
        status_of(&g.read(), nid(1)),
        CanonizationStatus::Venerable,
        "the blocked top scorer stays put — it just stops blocking"
    );
    assert_eq!(status_of(&g.read(), nid(3)), CanonizationStatus::Venerable);
}

/// F19 + issue #2: demotion ties. Two Canonicals with the **same**
/// blast radius over a budget of 1; spec §10 demotes the lowest blast
/// radius first, and the tie-break is canonical key ascending with
/// NodeId ascending only behind that. Without a stable order the
/// victim would depend on `HashMap` walk order; with the old
/// id-first chain it would depend on the per-run ids.
#[tokio::test]
async fn demotion_blast_radius_tie_breaks_on_canonical_key_ahead_of_node_id() {
    let mut g = Graph::new(sid());
    g.insert_interaction(interaction(1, None, ts())).unwrap();
    // Equal blast radii (3 each) — the tie-break is the only signal.
    // The keys contradict the id order ("zeta" rides the SMALLER id),
    // so the old NodeId-first chain fails this test.
    let mut zeta = concept(20, 1, 5, CanonizationStatus::Canonical);
    zeta.canonical_key = "zeta".into();
    zeta.content = "zeta".into();
    zeta.blast_radius = Some(3);
    g.insert_concept(zeta, iid(1)).unwrap();
    let mut alpha = concept(21, 1, 5, CanonizationStatus::Canonical);
    alpha.canonical_key = "alpha".into();
    alpha.content = "alpha".into();
    alpha.blast_radius = Some(3);
    g.insert_concept(alpha, iid(1)).unwrap();
    attach_blast(&mut g, 20, 3, 100);
    attach_blast(&mut g, 21, 3, 200);

    let store = store_from_graph(&g).await;
    for id in [20u64, 21] {
        assert_eq!(
            store
                .blast_radius(&sid(), nid(id), Duration::ZERO, ts())
                .await
                .unwrap(),
            3,
            "fixture premise: the two hubs must tie"
        );
    }
    let g = RwLock::new(g);
    let mut p = params();
    p.max_canonical_nodes = 1;
    let (tx, _rx) = channel();
    let mut ev = Evaluator::new();

    let outcome = eval_cycle(&mut ev, &g, &store, &table(&[]), &tx, &p, ts())
        .await
        .unwrap();
    assert_eq!(outcome.demotions.len(), 1);
    assert_eq!(
        outcome.demotions[0].node_id,
        nid(21),
        "a blast tie demotes the smaller canonical key (alpha), not the smaller id"
    );
    assert_eq!(status_of(&g.read(), nid(20)), CanonizationStatus::Canonical);
}

/// F7 at the eval seam: `EvalParams::min_edge_age` must reach the
/// Stage-3 blast query. The hub's dependents are attached with edges
/// created AT `now`, so the 60s inflation guard sees blast 0 and
/// refuses; the same graph promotes at `min_edge_age = 0`. A refactor
/// that forwards `Duration::ZERO` (the mutant the review shipped green
/// through both gates) fails the first half.
#[tokio::test]
async fn eval_forwards_min_edge_age_to_the_blast_query() {
    let store = store_with_hubs(&[(1, 6)]).await;
    let mut p = params();
    p.min_edge_age = Duration::from_secs(60);
    let (tx, _rx) = channel();

    // `now` is the instant the fixture edges were created, so every
    // dependent edge is younger than the 60s floor.
    let guarded = RwLock::new(venerable_graph(&[1]));
    let mut ev = Evaluator::new();
    let outcome = eval_cycle(&mut ev, &guarded, &store, &table(&[]), &tx, &p, ts())
        .await
        .unwrap();
    assert_eq!(
        outcome.stage3_batch,
        vec![nid(1)],
        "the node is evaluated — it just must not pass"
    );
    assert!(
        outcome.promotions.is_empty(),
        "fresh edges must not inflate blast radius past the guard"
    );
    assert_eq!(
        status_of(&guarded.read(), nid(1)),
        CanonizationStatus::Venerable
    );

    // Same graph, guard off: the promotion is real, so the first half
    // above cannot pass for want of evidence.
    let open = RwLock::new(venerable_graph(&[1]));
    let mut ev = Evaluator::new();
    let outcome = eval_cycle(&mut ev, &open, &store, &table(&[]), &tx, &params(), ts())
        .await
        .unwrap();
    assert_eq!(outcome.promotions.len(), 1);
    assert_eq!(
        status_of(&open.read(), nid(1)),
        CanonizationStatus::Canonical
    );
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
    let g = RwLock::new(g);
    let mut pairs: Vec<(u64, f64)> = (1..=19).map(|i| (i, 0.1)).collect();
    pairs.push((20, 1.0));
    let (tx, mut rx) = channel();
    let mut ev = Evaluator::new();
    eval_cycle(&mut ev, &g, &store, &table(&pairs), &tx, &params(), ts())
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
    let g = RwLock::new(g);
    let mut pairs: Vec<(u64, f64)> = (1..=19).map(|i| (i, 0.1)).collect();
    pairs.push((20, 1.0));
    let (tx, mut rx) = channel();
    let mut ev = Evaluator::new();
    let outcome = eval_cycle(&mut ev, &g, &store, &table(&pairs), &tx, &params(), ts())
        .await
        .unwrap();

    let committed: Vec<_> = outcome.transitions().cloned().collect();
    assert!(!committed.is_empty());
    assert_eq!(g.read().canonization_events(), committed.as_slice());
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
    let g = RwLock::new(g);
    let mut p = params();
    p.max_canonical_nodes = 1;
    let (tx, mut rx) = channel();
    let mut ev = Evaluator::new();
    let outcome = eval_cycle(&mut ev, &g, &store, &table(&[]), &tx, &p, ts())
        .await
        .unwrap();

    assert_eq!(
        status_of(&g.read(), nid(20)),
        CanonizationStatus::Venerable,
        "budget full: the Venerable must wait, not overflow"
    );
    assert_eq!(
        status_of(&g.read(), nid(10)),
        CanonizationStatus::Canonical,
        "the pre-existing Canonical is untouched when not over budget"
    );
    assert!(outcome.promotions.is_empty(), "no promotion over budget");
    assert!(outcome.demotions.is_empty(), "no demotion at exact budget");
    assert_eq!(canonical_count(&g.read()), 1, "count stays within budget");
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
    let g = RwLock::new(g);
    let mut pairs: Vec<(u64, f64)> = (1..=19).map(|i| (i, 0.1)).collect();
    pairs.push((20, 1.0));
    let (tx, _rx) = channel();
    let mut ev = Evaluator::new();
    let outcome = eval_cycle(&mut ev, &g, &store, &table(&pairs), &tx, &params(), ts())
        .await
        .unwrap();
    let committed: Vec<_> = outcome.transitions().cloned().collect();
    assert_eq!(committed.len(), 1, "one None→Candidate hop");

    // Replay the write-behind log (the same transition as the live write).
    let batch = g.write().drain_log();
    assert!(!batch.is_empty());
    store.flush(&batch, None).await.unwrap();

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
