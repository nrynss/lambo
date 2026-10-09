//! `TieredStore` behaviour over an in-process primary and the fake index.
//!
//! No Elasticsearch: the fake applies the engine's external-version rules,
//! and `elastic.rs` tests the wire format against an in-process HTTP mock.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;

use super::fake::FakeIndex;
use super::index::RecallIndex;
use super::project::{mirror_version, project};
use super::{TierSync, TieredStore};
use crate::store::lease::{LeaseHolder, LeaseInfo, LeaseOutcome, LeaseRefusal};
use crate::store::{
    Capabilities, EraseOutcome, GraphStore, MemoryStore, RecallBackfillReport, SessionFlushStats,
};
use crate::types::{
    AgentId, CanonizationEvent, CanonizationStatus, Concept, ConceptType, EmbeddingContract,
    GraphSnapshot, Interaction, InteractionSpan, Mutation, MutationBatch, Node, NodeId, Scored,
    SessionId, StoreError,
};

// ---------------------------------------------------------------------------
// Rig
// ---------------------------------------------------------------------------

/// Forwards every `GraphStore` method to a shared primary, so two
/// `TieredStore`s (two processes, a takeover, a restart) can sit on one
/// durable store and one index. Pinned snapshots, when queued, answer the
/// next loads instead, one each (a load that read an older snapshot than a
/// concurrent flush committed).
struct Shared(
    Arc<dyn GraphStore>,
    Arc<parking_lot::Mutex<Vec<GraphSnapshot>>>,
);

impl Shared {
    fn new(primary: Arc<dyn GraphStore>) -> Self {
        Self(primary, Arc::default())
    }
}

#[async_trait]
impl GraphStore for Shared {
    async fn init_schema(&self) -> Result<(), StoreError> {
        self.0.init_schema().await
    }
    fn capabilities(&self) -> Capabilities {
        self.0.capabilities()
    }
    fn vector_dimensions(&self) -> Option<usize> {
        self.0.vector_dimensions()
    }
    fn exact_vector_scan(&self) -> bool {
        self.0.exact_vector_scan()
    }
    async fn flush(&self, b: &MutationBatch, t: Option<u64>) -> Result<(), StoreError> {
        self.0.flush(b, t).await
    }
    async fn load_session(&self, s: &SessionId) -> Result<GraphSnapshot, StoreError> {
        let pinned = self.1.lock().pop();
        match pinned {
            Some(snap) => Ok(snap),
            None => self.0.load_session(s).await,
        }
    }
    async fn keyword_candidates(
        &self,
        s: &SessionId,
        t: &[String],
        l: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.0.keyword_candidates(s, t, l).await
    }
    async fn vector_candidates(
        &self,
        s: &SessionId,
        e: &[f32],
        l: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.0.vector_candidates(s, e, l).await
    }
    async fn vector_candidates_checked(
        &self,
        s: &SessionId,
        e: &[f32],
        c: &EmbeddingContract,
        l: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.0.vector_candidates_checked(s, e, c, l).await
    }
    async fn blast_radius(
        &self,
        s: &SessionId,
        n: NodeId,
        a: Duration,
        now: chrono::DateTime<Utc>,
    ) -> Result<u64, StoreError> {
        self.0.blast_radius(s, n, a, now).await
    }
    async fn interaction_span(
        &self,
        s: &SessionId,
        n: NodeId,
        a: Duration,
        now: chrono::DateTime<Utc>,
    ) -> Result<InteractionSpan, StoreError> {
        self.0.interaction_span(s, n, a, now).await
    }
    async fn record_canonization(
        &self,
        e: &CanonizationEvent,
        t: Option<u64>,
    ) -> Result<(), StoreError> {
        self.0.record_canonization(e, t).await
    }
    async fn erase_session(
        &self,
        s: &SessionId,
        h: &LeaseHolder,
    ) -> Result<EraseOutcome, StoreError> {
        self.0.erase_session(s, h).await
    }
    async fn acquire_lease(
        &self,
        s: &SessionId,
        h: &LeaseHolder,
        ttl: Duration,
    ) -> Result<LeaseOutcome, StoreError> {
        self.0.acquire_lease(s, h, ttl).await
    }
    async fn read_lease(&self, s: &SessionId) -> Result<Option<LeaseInfo>, StoreError> {
        self.0.read_lease(s).await
    }
    async fn refresh_lease(
        &self,
        s: &SessionId,
        h: &LeaseHolder,
        ttl: Duration,
    ) -> Result<LeaseOutcome, StoreError> {
        self.0.refresh_lease(s, h, ttl).await
    }
    async fn release_lease(&self, s: &SessionId, h: &LeaseHolder) -> Result<(), StoreError> {
        self.0.release_lease(s, h).await
    }
    async fn record_lease_refusal(
        &self,
        s: &SessionId,
        by: &str,
        cur: &str,
    ) -> Result<(), StoreError> {
        self.0.record_lease_refusal(s, by, cur).await
    }
    async fn pending_lease_refusals(
        &self,
        s: &SessionId,
        since: chrono::DateTime<Utc>,
    ) -> Result<Vec<LeaseRefusal>, StoreError> {
        self.0.pending_lease_refusals(s, since).await
    }
    async fn write_flush_stats(
        &self,
        s: &SessionId,
        st: &SessionFlushStats,
    ) -> Result<(), StoreError> {
        self.0.write_flush_stats(s, st).await
    }
    async fn read_flush_stats(
        &self,
        s: &SessionId,
    ) -> Result<Option<SessionFlushStats>, StoreError> {
        self.0.read_flush_stats(s).await
    }
}

fn memory_primary() -> Arc<dyn GraphStore> {
    Arc::new(MemoryStore::new())
}

/// A tier over `primary` and `fake` that repairs without waiting.
fn tier(primary: &Arc<dyn GraphStore>, fake: &Arc<FakeIndex>) -> TieredStore {
    TieredStore::new(
        Box::new(Shared::new(primary.clone())),
        Box::new(fake.clone()),
        Some(4),
    )
    .with_repair_backoff(Duration::ZERO)
}

fn contract() -> EmbeddingContract {
    EmbeddingContract {
        kind: "fixture".into(),
        model: None,
        dim: 4,
    }
}

fn holder(agent: &str) -> LeaseHolder {
    LeaseHolder {
        endpoint: None,
        agent: AgentId::new(agent),
        pid: 4242,
        host: "test-host".into(),
    }
}

fn interaction(sid: &SessionId, id: NodeId) -> Mutation {
    Mutation::UpsertNode {
        node: Node::Interaction(Interaction {
            id,
            session_id: sid.clone(),
            agent_id: AgentId::new("agent-a"),
            prompt_text: None,
            previous_id: None,
            created_at: Utc::now(),
            event_time: None,
        }),
    }
}

fn concept(
    sid: &SessionId,
    id: NodeId,
    origin: NodeId,
    key: &str,
    embedding: Option<Vec<f32>>,
) -> Concept {
    Concept {
        id,
        session_id: sid.clone(),
        content: key.to_string(),
        canonical_key: key.to_string(),
        concept_type: ConceptType::Entity,
        origin_interaction: origin,
        origin_agent: AgentId::new("agent-a"),
        created_at: Utc::now(),
        access_count: 0,
        last_accessed: None,
        gc_survived: 0,
        canonization_status: CanonizationStatus::None,
        blast_radius: None,
        last_demotion_time: None,
        embedding,
        human_confirmed: 0,
        chunk_group_id: None,
        embedding_source: None,
    }
}

fn upsert(c: Concept) -> Mutation {
    Mutation::UpsertNode {
        node: Node::Concept(c),
    }
}

fn set_contract(sid: &SessionId, c: Option<EmbeddingContract>) -> Mutation {
    Mutation::SetEmbedding {
        session_id: sid.clone(),
        embedding: c,
    }
}

fn batch(epoch: u64, mutations: Vec<Mutation>) -> MutationBatch {
    MutationBatch {
        mutations,
        mutation_epoch: epoch,
        gc_mark: Default::default(),
    }
}

/// Acquire, then load (the order `Memory` attaches in); returns the token.
async fn attach(store: &TieredStore, sid: &SessionId, who: &LeaseHolder) -> u64 {
    let token = match store
        .acquire_lease(sid, who, Duration::from_secs(60))
        .await
        .unwrap()
    {
        LeaseOutcome::Acquired(info) => info.token,
        LeaseOutcome::Held { current, .. } => panic!("held by {}", current.holder),
    };
    match store.load_session(sid).await {
        Ok(_) | Err(StoreError::SessionNotFound(_)) => {}
        Err(e) => panic!("load: {e}"),
    }
    token
}

/// Wait for every repair the store has in flight (repairs run in the
/// background, M1); the one place tests wait for them.
async fn settle(store: &TieredStore) {
    store.repairs_settled().await;
}

/// The common seed: a contract, an interaction, two vectors and one
/// concept without a vector.
struct Seeded {
    origin: NodeId,
    c1: NodeId,
    c2: NodeId,
    c3: NodeId,
}

fn seed_batch(sid: &SessionId, epoch: u64) -> (Seeded, MutationBatch) {
    let (origin, c1, c2, c3) = (NodeId::new(), NodeId::new(), NodeId::new(), NodeId::new());
    let b = batch(
        epoch,
        vec![
            set_contract(sid, Some(contract())),
            interaction(sid, origin),
            upsert(concept(
                sid,
                c1,
                origin,
                "alpha",
                Some(vec![1.0, 0.0, 0.0, 0.0]),
            )),
            upsert(concept(
                sid,
                c2,
                origin,
                "beta",
                Some(vec![0.0, 1.0, 0.0, 0.0]),
            )),
            upsert(concept(sid, c3, origin, "gamma", None)),
        ],
    );
    (Seeded { origin, c1, c2, c3 }, b)
}

fn ids(scored: &[Scored<NodeId>]) -> Vec<NodeId> {
    scored.iter().map(|s| s.item).collect()
}

const PROBE: [f32; 4] = [1.0, 0.2, 0.0, 0.0];

// ---------------------------------------------------------------------------
// Mirroring and the served vector leg
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_committed_flush_is_mirrored_and_the_index_serves_the_vector_leg() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let store = tier(&primary, &fake);
    let sid = SessionId::new("s1");
    let token = attach(&store, &sid, &holder("writer")).await;
    let (s, b) = seed_batch(&sid, 3);
    store.flush(&b, Some(token)).await.unwrap();

    let live = fake.live(&sid);
    assert_eq!(live.len(), 2, "only concepts with a vector are indexed");
    assert!(live.contains_key(&s.c1.0.to_string()));
    assert!(live.contains_key(&s.c2.0.to_string()));
    assert!(!live.contains_key(&s.c3.0.to_string()));
    let (_, doc) = &live[&s.c1.0.to_string()];
    assert_eq!(doc.v, mirror_version(token, 1).unwrap());
    assert_eq!(
        fake.marker(&sid),
        Some(3),
        "marker = the batch's durable epoch"
    );

    let status = store.tier_status(&sid);
    assert_eq!(status.sync, TierSync::InSync);
    assert_eq!((status.token, status.flush_counter), (token, 1));

    let got = store
        .vector_candidates_checked(&sid, &PROBE, &contract(), 10)
        .await
        .unwrap();
    assert_eq!(ids(&got), vec![s.c1, s.c2]);
    assert!(got[0].score > got[1].score);
    assert_eq!(fake.knn_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

/// M3: a hit that came back without its stored vector (a cluster that
/// excludes vectors from `_source`) keeps the engine's score; the others are
/// re-scored exactly, and the limit applies after the re-rank.
#[test]
fn rerank_scores_exactly_and_falls_back_to_the_engine_score_per_hit() {
    use super::index::KnnHit;
    let (a, b, c) = (NodeId::new(), NodeId::new(), NodeId::new());
    let probe = [1.0f32, 0.0];
    let hits = vec![
        // The engine over-rates `a`; its vector says 0.6.
        KnnHit {
            id: a,
            cosine: 0.99,
            canonical_key: "a".into(),
            embedding: Some(vec![0.6, 0.8]),
        },
        KnnHit {
            id: b,
            cosine: 0.7,
            canonical_key: "b".into(),
            embedding: None,
        },
        KnnHit {
            id: c,
            cosine: 0.1,
            canonical_key: "c".into(),
            embedding: Some(vec![0.8, 0.6]),
        },
    ];
    let got = super::rerank(&probe, &hits, 2);
    assert_eq!(ids(&got), vec![c, b]);
    assert!((got[0].score - 0.8).abs() < 1e-6);
    assert_eq!(got[1].score, 0.7);
    let all_exact: Vec<KnnHit> = hits.into_iter().filter(|h| h.id != b).collect();
    assert_eq!(ids(&super::rerank(&probe, &all_exact, 5)), vec![c, a]);
}

/// #8: a holder over the tier must reach the index, so the tier never
/// declares an exact scan; and it always offers the vector leg, with a width.
#[tokio::test]
async fn the_tier_is_never_an_exact_scan_and_always_offers_vectors() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let store = tier(&primary, &fake);
    assert!(!store.exact_vector_scan());
    assert!(store.capabilities().contains(Capabilities::VECTOR_SEARCH));
    assert_eq!(store.vector_dimensions(), Some(4));
    let graph = parking_lot::RwLock::new(crate::graph::Graph::new(SessionId::new("s")));
    assert!(matches!(
        crate::store::vector_source::VectorCandidates::for_holder(&store, &graph),
        crate::store::vector_source::VectorCandidates::Store(_)
    ));
    // #18 amending #8: a holder's derive ranks in its graph instead.
    assert!(store.holder_derives_from_graph());
    assert!(matches!(
        crate::store::vector_source::VectorCandidates::for_holder_derive(&store, &graph),
        crate::store::vector_source::VectorCandidates::Graph(_)
    ));
}

/// #32: the counter and the token it belongs to are per session.
#[tokio::test]
async fn flush_counters_are_kept_per_session() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let store = tier(&primary, &fake);
    let (a, b) = (SessionId::new("a"), SessionId::new("b"));
    let ta = attach(&store, &a, &holder("wa")).await;
    let tb = attach(&store, &b, &holder("wb")).await;
    let (_, ba) = seed_batch(&a, 1);
    let (_, bb) = seed_batch(&b, 1);
    store.flush(&ba, Some(ta)).await.unwrap();
    store.flush(&bb, Some(tb)).await.unwrap();
    let origin = NodeId::new();
    let more = batch(
        2,
        vec![upsert(concept(
            &a,
            NodeId::new(),
            origin,
            "delta",
            Some(vec![0.0, 0.0, 1.0, 0.0]),
        ))],
    );
    store.flush(&more, Some(ta)).await.unwrap();
    assert_eq!(store.tier_status(&a).flush_counter, 2);
    assert_eq!(store.tier_status(&b).flush_counter, 1);
    assert_eq!(fake.marker(&a), Some(2));
    assert_eq!(fake.marker(&b), Some(1));
}

// ---------------------------------------------------------------------------
// Fencing and ordering
// ---------------------------------------------------------------------------

/// A stale-token flush fails at the primary and nothing reaches the index.
#[tokio::test]
async fn a_stale_token_flush_is_refused_and_mirrors_nothing() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let old = tier(&primary, &fake);
    let sid = SessionId::new("fenced");
    let a = holder("old-writer");
    let ta = match old
        .acquire_lease(&sid, &a, Duration::from_millis(1))
        .await
        .unwrap()
    {
        LeaseOutcome::Acquired(info) => info.token,
        LeaseOutcome::Held { .. } => unreachable!(),
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    let new = tier(&primary, &fake);
    let tb = attach(&new, &sid, &holder("new-writer")).await;
    assert!(tb > ta, "a takeover mints a greater token");

    let (_, b) = seed_batch(&sid, 1);
    let bulks_before = fake.bulk_calls.load(std::sync::atomic::Ordering::SeqCst);
    let err = old.flush(&b, Some(ta)).await.unwrap_err();
    assert!(matches!(err, StoreError::StaleWrite(_)), "{err:?}");
    assert_eq!(
        fake.bulk_calls.load(std::sync::atomic::Ordering::SeqCst),
        bulks_before,
        "a refused flush mirrors nothing"
    );
    assert!(fake.live(&sid).is_empty());
    assert_eq!(fake.marker(&sid), None);
}

/// A replayed or late older write loses to the newer one already indexed,
/// within one holder and across a takeover.
#[tokio::test]
async fn an_older_mirror_write_never_overwrites_a_newer_one() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let a_store = tier(&primary, &fake);
    let sid = SessionId::new("ordered");
    let a = holder("a");
    let ta = attach(&a_store, &sid, &a).await;
    let (s, first) = seed_batch(&sid, 1);
    a_store.flush(&first, Some(ta)).await.unwrap();
    let renamed = batch(
        2,
        vec![upsert(concept(
            &sid,
            s.c1,
            s.origin,
            "alpha-v2",
            Some(vec![1.0, 0.0, 0.0, 0.0]),
        ))],
    );
    a_store.flush(&renamed, Some(ta)).await.unwrap();

    // Replay the first batch at the version it was written with.
    let replay = project(&sid, &first.mutations, None, mirror_version(ta, 1));
    fake.bulk(&replay.ops).await.unwrap();
    let key = s.c1.0.to_string();
    assert_eq!(fake.live(&sid)[&key].1.canonical_key, "alpha-v2");

    // Takeover: the new holder's first write outranks every counter the old
    // one could still land late.
    a_store.release_lease(&sid, &a).await.unwrap();
    let b_store = tier(&primary, &fake);
    let tb = attach(&b_store, &sid, &holder("b")).await;
    assert!(tb > ta);
    let by_b = batch(
        3,
        vec![upsert(concept(
            &sid,
            s.c1,
            s.origin,
            "alpha-b",
            Some(vec![1.0, 0.0, 0.0, 0.0]),
        ))],
    );
    b_store.flush(&by_b, Some(tb)).await.unwrap();
    let late = project(
        &sid,
        &renamed.mutations,
        Some(contract()),
        mirror_version(ta, u32::MAX),
    );
    fake.bulk(&late.ops).await.unwrap();
    assert_eq!(fake.live(&sid)[&key].1.canonical_key, "alpha-b");
    assert_eq!(fake.marker(&sid), Some(3));
}

// ---------------------------------------------------------------------------
// The embedding contract
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_contract_mismatch_is_refused_and_a_switch_queries_only_the_new_index() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let store = tier(&primary, &fake);
    let sid = SessionId::new("contract");
    let token = attach(&store, &sid, &holder("w")).await;
    let (s, b) = seed_batch(&sid, 1);
    store.flush(&b, Some(token)).await.unwrap();

    let other = EmbeddingContract {
        model: Some("other-model".into()),
        ..contract()
    };
    let err = store
        .vector_candidates_checked(&sid, &PROBE, &other, 10)
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Invariant(_)), "{err:?}");
    assert!(err.to_string().contains("contract"), "{err}");

    // Switch the session to `other` and re-embed only c2.
    let switch = batch(
        2,
        vec![
            set_contract(&sid, Some(other.clone())),
            upsert(concept(
                &sid,
                s.c2,
                s.origin,
                "beta",
                Some(vec![1.0, 0.0, 0.0, 0.0]),
            )),
        ],
    );
    store.flush(&switch, Some(token)).await.unwrap();
    let got = store
        .vector_candidates_checked(&sid, &PROBE, &other, 10)
        .await
        .unwrap();
    assert_eq!(
        ids(&got),
        vec![s.c2],
        "c1 lives only in the old contract's index and is not ranked"
    );
    let err = store
        .vector_candidates_checked(&sid, &PROBE, &contract(), 10)
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Invariant(_)), "{err:?}");
}

#[tokio::test]
async fn a_session_without_a_contract_or_data_answers_empty() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let store = tier(&primary, &fake);
    let got = store
        .vector_candidates_checked(&SessionId::new("nobody"), &PROBE, &contract(), 5)
        .await
        .unwrap();
    assert!(got.is_empty());
    assert!(store
        .vector_candidates_checked(&SessionId::new("nobody"), &PROBE, &contract(), 0)
        .await
        .unwrap()
        .is_empty());
    let err = store
        .vector_candidates_checked(&SessionId::new("nobody"), &[0.0; 4], &contract(), 5)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("zero norm"), "{err}");
}

// ---------------------------------------------------------------------------
// Index down, lagging or behind
// ---------------------------------------------------------------------------

/// A mirror failure keeps the flush, sends reads to the primary, and the
/// next flush after the index returns repairs the session in full.
#[tokio::test]
async fn a_mirror_failure_keeps_the_flush_and_the_next_flush_repairs() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let store = tier(&primary, &fake);
    let sid = SessionId::new("outage");
    let token = attach(&store, &sid, &holder("w")).await;
    fake.set_down(true);
    let (s, b) = seed_batch(&sid, 1);
    store
        .flush(&b, Some(token))
        .await
        .expect("an index outage never fails a durable flush");
    let snap = primary.load_session(&sid).await.unwrap();
    assert_eq!(snap.concepts.len(), 3, "the primary has the batch");

    let status = store.tier_status(&sid);
    assert_eq!(status.sync, TierSync::Stale);
    assert_eq!(status.mirror_failures, 1);
    assert!(
        status
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("connection refused")),
        "{status:?}"
    );
    // Reads fall back: MemoryStore has no vector search, so the leg is empty
    // and the index is not asked.
    let got = store
        .vector_candidates_checked(&sid, &PROBE, &contract(), 10)
        .await
        .unwrap();
    assert!(got.is_empty());
    assert_eq!(fake.knn_calls.load(std::sync::atomic::Ordering::SeqCst), 0);

    fake.set_down(false);
    let c4 = NodeId::new();
    let next = batch(
        2,
        vec![upsert(concept(
            &sid,
            c4,
            s.origin,
            "delta",
            Some(vec![0.0, 0.0, 1.0, 0.0]),
        ))],
    );
    store.flush(&next, Some(token)).await.unwrap();
    settle(&store).await;
    assert_eq!(store.tier_status(&sid).sync, TierSync::InSync);
    let live = fake.live(&sid);
    for id in [s.c1, s.c2, c4] {
        assert!(live.contains_key(&id.0.to_string()), "{id:?} repaired");
    }
    assert_eq!(fake.marker(&sid), Some(2));
    let got = store
        .vector_candidates_checked(&sid, &PROBE, &contract(), 10)
        .await
        .unwrap();
    assert_eq!(got[0].item, s.c1);
}

/// While the index stays down, a stale session is not re-read from the
/// primary on every flush.
#[tokio::test]
async fn repair_attempts_back_off_while_the_index_is_down() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let store = TieredStore::new(
        Box::new(Shared::new(primary.clone())),
        Box::new(fake.clone()),
        Some(4),
    );
    let sid = SessionId::new("backoff");
    let token = attach(&store, &sid, &holder("w")).await;
    fake.set_down(true);
    let (s, b) = seed_batch(&sid, 1);
    store.flush(&b, Some(token)).await.unwrap();
    let after_first = store.tier_status(&sid).mirror_failures;
    for epoch in 2..5 {
        let more = batch(
            epoch,
            vec![upsert(concept(
                &sid,
                NodeId::new(),
                s.origin,
                "x",
                Some(vec![0.0, 0.0, 0.0, 1.0]),
            ))],
        );
        store.flush(&more, Some(token)).await.unwrap();
    }
    settle(&store).await;
    assert_eq!(
        store.tier_status(&sid).mirror_failures,
        after_first + 1,
        "one repair attempt inside the backoff window, then none"
    );
}

/// Poll until `cond` holds (background work has reached a point), failing
/// after two seconds.
async fn wait_until(what: &str, cond: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !cond() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting: {what}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Make the session stale with the seed batch durable, the index back up.
async fn stale_after_an_outage(
    store: &TieredStore,
    fake: &FakeIndex,
    sid: &SessionId,
    token: u64,
) -> Seeded {
    fake.set_down(true);
    let (s, b) = seed_batch(sid, 1);
    store.flush(&b, Some(token)).await.unwrap();
    fake.set_down(false);
    assert_eq!(store.tier_status(sid).sync, TierSync::Stale);
    s
}

fn one_more(sid: &SessionId, s: &Seeded, epoch: u64) -> (NodeId, MutationBatch) {
    let id = NodeId::new();
    let b = batch(
        epoch,
        vec![upsert(concept(
            sid,
            id,
            s.origin,
            &format!("more-{epoch}"),
            Some(vec![0.0, 0.0, 1.0, 0.0]),
        ))],
    );
    (id, b)
}

/// M1: a flush that finds its session stale asks for a repair and returns.
/// The repair (a whole-session read, bulk re-index and delete-by-query) runs
/// in the background, so it delays neither this batch's durability nor the
/// next one's.
#[tokio::test]
async fn a_flush_does_not_wait_for_the_repair_it_triggers() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let store = tier(&primary, &fake);
    let sid = SessionId::new("flush-repair");
    let token = attach(&store, &sid, &holder("w")).await;
    let s = stale_after_an_outage(&store, &fake, &sid, token).await;
    fake.hold_deletes();
    let (c4, next) = one_more(&sid, &s, 2);
    tokio::time::timeout(Duration::from_secs(2), store.flush(&next, Some(token)))
        .await
        .expect("the flush waited for the repair")
        .unwrap();
    assert_ne!(store.tier_status(&sid).sync, TierSync::InSync);

    fake.release_deletes();
    settle(&store).await;
    assert_eq!(store.tier_status(&sid).sync, TierSync::InSync);
    assert!(fake.live(&sid).contains_key(&c4.0.to_string()));
    assert_eq!(fake.marker(&sid), Some(2));
}

/// M1: attaching to a session whose marker is behind returns at once; reads
/// fall back to the primary until the background repair lands.
#[tokio::test]
async fn attaching_does_not_wait_for_the_repair() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let first = tier(&primary, &fake);
    let sid = SessionId::new("attach-repair");
    let w = holder("w");
    let token = attach(&first, &sid, &w).await;
    let s = stale_after_an_outage(&first, &fake, &sid, token).await;
    first.release_lease(&sid, &w).await.unwrap();

    fake.hold_deletes();
    let next = tier(&primary, &fake);
    tokio::time::timeout(Duration::from_secs(2), attach(&next, &sid, &holder("w2")))
        .await
        .expect("the attach waited for the repair");
    let calls = fake.knn_calls.load(std::sync::atomic::Ordering::SeqCst);
    next.vector_candidates_checked(&sid, &PROBE, &contract(), 5)
        .await
        .unwrap();
    assert_eq!(
        fake.knn_calls.load(std::sync::atomic::Ordering::SeqCst),
        calls,
        "a read during the repair falls back"
    );

    fake.release_deletes();
    settle(&next).await;
    assert_eq!(next.tier_status(&sid).sync, TierSync::InSync);
    assert!(fake.live(&sid).contains_key(&s.c1.0.to_string()));
}

/// M1/M4: one repair per session at a time. Requests that arrive while it
/// runs collapse into a single rerun, which picks up what they committed.
#[tokio::test]
async fn repairs_run_one_at_a_time_per_session() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let store = tier(&primary, &fake);
    let sid = SessionId::new("single-flight");
    let token = attach(&store, &sid, &holder("w")).await;
    let s = stale_after_an_outage(&store, &fake, &sid, token).await;
    fake.hold_deletes();
    let (_, e2) = one_more(&sid, &s, 2);
    store.flush(&e2, Some(token)).await.unwrap();
    let deletes = || fake.delete_calls.load(std::sync::atomic::Ordering::SeqCst);
    wait_until("the first repair reaches its sweep", || deletes() == 1).await;
    let (c3, e3) = one_more(&sid, &s, 3);
    let (c4, e4) = one_more(&sid, &s, 4);
    store.flush(&e3, Some(token)).await.unwrap();
    store.flush(&e4, Some(token)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(deletes(), 1, "a second repair ran beside the first");

    fake.release_deletes();
    settle(&store).await;
    assert_eq!(deletes(), 2, "the queued requests collapse into one rerun");
    assert_eq!(store.tier_status(&sid).sync, TierSync::InSync);
    assert_eq!(fake.marker(&sid), Some(4));
    let live = fake.live(&sid);
    assert!(live.contains_key(&c3.0.to_string()) && live.contains_key(&c4.0.to_string()));
}

/// F4: a flush that commits while a repair runs is not in the repair's
/// snapshot (and was not mirrored: mirrors wait for the session to be in
/// sync). The repair must not mark the session in sync when it ends; reads
/// keep falling back until the rerun has indexed that flush too, and then
/// the busy session does settle.
#[tokio::test]
async fn a_repair_overtaken_by_a_flush_is_not_trusted_until_the_rerun() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let store = tier(&primary, &fake);
    let sid = SessionId::new("overtaken");
    let token = attach(&store, &sid, &holder("w")).await;
    let s = stale_after_an_outage(&store, &fake, &sid, token).await;
    let knn = || fake.knn_calls.load(std::sync::atomic::Ordering::SeqCst);

    // Pass 1 repairs from a snapshot at e2 and parks in its sweep.
    fake.hold_deletes();
    let (_, e2) = one_more(&sid, &s, 2);
    store.flush(&e2, Some(token)).await.unwrap();
    wait_until("pass 1 reaches its sweep", || {
        fake.delete_calls.load(std::sync::atomic::Ordering::SeqCst) == 1
    })
    .await;
    // e3 commits under it.
    let (c3, e3) = one_more(&sid, &s, 3);
    store.flush(&e3, Some(token)).await.unwrap();

    // Pass 1 ends; the rerun parks at its marker check.
    let reads = fake.marker_reads.load(std::sync::atomic::Ordering::SeqCst);
    fake.hold_marker_reads();
    fake.release_deletes();
    wait_until("the rerun reaches its marker check", || {
        fake.marker_reads.load(std::sync::atomic::Ordering::SeqCst) > reads
    })
    .await;
    assert_eq!(fake.marker(&sid), Some(2), "pass 1 wrote its own epoch");
    assert_ne!(
        store.tier_status(&sid).sync,
        TierSync::InSync,
        "a repair from e2 was trusted after e3 committed"
    );
    store
        .vector_candidates_checked(&sid, &PROBE, &contract(), 10)
        .await
        .unwrap();
    assert_eq!(knn(), 0, "a read was served from an index without e3");

    fake.release_marker_reads();
    settle(&store).await;
    assert_eq!(store.tier_status(&sid).sync, TierSync::InSync);
    assert_eq!(fake.marker(&sid), Some(3));
    assert!(fake.live(&sid).contains_key(&c3.0.to_string()));
}

/// M1: a mirror against a slow cluster is bounded by the mirror deadline;
/// the flush returns and the session goes stale for a repair.
#[tokio::test]
async fn a_mirror_on_a_slow_cluster_is_bounded() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let store =
        tier(&primary, &fake).with_deadlines(Duration::from_millis(50), Duration::from_secs(10));
    let sid = SessionId::new("slow-mirror");
    let token = attach(&store, &sid, &holder("w")).await;
    fake.delay_bulk_ms
        .store(5_000, std::sync::atomic::Ordering::SeqCst);
    let (_, b) = seed_batch(&sid, 1);
    let started = std::time::Instant::now();
    store.flush(&b, Some(token)).await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    let status = store.tier_status(&sid);
    assert_eq!(status.sync, TierSync::Stale);
    assert!(
        status
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("deadline")),
        "{status:?}"
    );
}

/// M1: a repair stuck past its deadline is abandoned and the session marked
/// stale (the backoff spaces the next attempt).
#[tokio::test]
async fn a_repair_past_its_deadline_marks_the_session_stale() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let store =
        tier(&primary, &fake).with_deadlines(Duration::from_secs(10), Duration::from_millis(50));
    let sid = SessionId::new("slow-repair");
    let token = attach(&store, &sid, &holder("w")).await;
    let s = stale_after_an_outage(&store, &fake, &sid, token).await;
    fake.hold_deletes();
    let (_, e2) = one_more(&sid, &s, 2);
    store.flush(&e2, Some(token)).await.unwrap();
    settle(&store).await;
    let status = store.tier_status(&sid);
    assert_eq!(status.sync, TierSync::Stale);
    assert!(
        status
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("deadline")),
        "{status:?}"
    );
    fake.release_deletes();
}

/// F5: a repair task that panics does not wedge the session. Its
/// single-flight flag is cleared, the session is marked stale, and the next
/// request starts a fresh repair that lands; the entry stays evictable.
#[tokio::test]
async fn a_panicking_repair_does_not_wedge_the_session() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let store = tier(&primary, &fake);
    let sid = SessionId::new("panicking-repair");
    let w = holder("w");
    let token = attach(&store, &sid, &w).await;
    let s = stale_after_an_outage(&store, &fake, &sid, token).await;
    fake.panic_next_bulk
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (_, e2) = one_more(&sid, &s, 2);
    store.flush(&e2, Some(token)).await.unwrap();
    settle(&store).await;
    assert!(
        !store.with_state(&sid, |st| st.repairing),
        "a panicked repair left the session marked as repairing"
    );
    let status = store.tier_status(&sid);
    assert_eq!(status.sync, TierSync::Stale);
    assert!(
        status
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("panicked")),
        "{status:?}"
    );

    let (c3, e3) = one_more(&sid, &s, 3);
    store.flush(&e3, Some(token)).await.unwrap();
    settle(&store).await;
    assert_eq!(store.tier_status(&sid).sync, TierSync::InSync);
    assert!(fake.live(&sid).contains_key(&c3.0.to_string()));
    assert_eq!(fake.marker(&sid), Some(3));
    store.release_lease(&sid, &w).await.unwrap();
    assert!(!store.tracked_sessions().contains(&sid), "never evicted");
}

/// F2: the holder stalls past its lease TTL in the middle of a repair, and
/// another process erases the session. The repair checks the durable lease
/// before each bulk chunk, the sweep and the marker, so once it resumes it
/// writes nothing more: the erased session is not rebuilt in the index. The
/// one request already in flight when the lease was lost can still land.
#[tokio::test]
async fn a_repair_that_lost_its_lease_stops_writing() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let store = tier(&primary, &fake).with_repair_chunk(1);
    let sid = SessionId::new("stalled-holder");
    let token = match store
        .acquire_lease(&sid, &holder("w"), Duration::from_millis(600))
        .await
        .unwrap()
    {
        LeaseOutcome::Acquired(info) => info.token,
        LeaseOutcome::Held { .. } => panic!("held"),
    };
    let _ = store.load_session(&sid).await;
    let s = stale_after_an_outage(&store, &fake, &sid, token).await;
    let failures = store.tier_status(&sid).mirror_failures;

    // The repair's first chunk is slow; the lease lapses under it.
    fake.delay_bulk_ms
        .store(1_600, std::sync::atomic::Ordering::SeqCst);
    let (_, e2) = one_more(&sid, &s, 2);
    store.flush(&e2, Some(token)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    let eraser = tier(&primary, &fake);
    let erased = eraser
        .erase_session(&sid, &holder("lambo-erase-session"))
        .await
        .unwrap();
    assert!(matches!(erased, EraseOutcome::Erased(_)));

    settle(&store).await;
    assert_eq!(
        fake.bulk_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the repair kept bulk-writing after its lease was lost"
    );
    assert_eq!(fake.marker(&sid), None, "the repair rewrote the marker");
    assert!(fake.live(&sid).len() <= 1, "{:?}", fake.live(&sid).keys());
    let status = store.tier_status(&sid);
    assert_eq!(status.sync, TierSync::Stale);
    assert_eq!(
        status.mirror_failures, failures,
        "a lost lease is not an index failure"
    );
    assert_eq!(store.with_state(&sid, |st| st.held), None);
}

/// F6: an erase through the store that is running a repair of the session
/// (its lease lapsed, so the erase may proceed) stops that repair and waits
/// for it before sweeping. Nothing the repair had in flight survives the
/// erase, and its state does not come back.
#[tokio::test]
async fn erase_stops_this_stores_repair_before_it_sweeps() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let store = tier(&primary, &fake).with_repair_chunk(1);
    let sid = SessionId::new("erase-mid-repair");
    let token = match store
        .acquire_lease(&sid, &holder("w"), Duration::from_millis(600))
        .await
        .unwrap()
    {
        LeaseOutcome::Acquired(info) => info.token,
        LeaseOutcome::Held { .. } => panic!("held"),
    };
    let _ = store.load_session(&sid).await;
    let s = stale_after_an_outage(&store, &fake, &sid, token).await;
    fake.delay_bulk_ms
        .store(1_600, std::sync::atomic::Ordering::SeqCst);
    let (_, e2) = one_more(&sid, &s, 2);
    store.flush(&e2, Some(token)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;

    let erased = store
        .erase_session(&sid, &holder("lambo-erase-session"))
        .await
        .unwrap();
    assert!(matches!(erased, EraseOutcome::Erased(_)));
    settle(&store).await;
    // Past the slow chunk, whether or not anything still tracks its task.
    tokio::time::sleep(Duration::from_millis(1_600)).await;
    fake.refresh_now();
    assert!(
        fake.live(&sid).is_empty(),
        "the repair re-inserted documents after the erase: {:?}",
        fake.live(&sid).keys()
    );
    assert_eq!(fake.marker(&sid), None);
    assert!(
        !store.tracked_sessions().contains(&sid),
        "the repair re-created the erased session's state"
    );
}

/// F2, the mirror side: a flush the primary accepted is not mirrored once
/// this store has recorded the lease as lost (a refused heartbeat between
/// the commit and the mirror); the session goes stale instead.
#[tokio::test]
async fn a_flush_is_not_mirrored_after_the_lease_was_lost() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let store = tier(&primary, &fake);
    let sid = SessionId::new("lost-before-mirror");
    let token = attach(&store, &sid, &holder("w")).await;
    let (s, b) = seed_batch(&sid, 1);
    store.flush(&b, Some(token)).await.unwrap();
    let bulks = fake.bulk_calls.load(std::sync::atomic::Ordering::SeqCst);
    store.with_state(&sid, |st| st.held = None);
    let (_, e2) = one_more(&sid, &s, 2);
    store.flush(&e2, Some(token)).await.unwrap();
    assert_eq!(
        fake.bulk_calls.load(std::sync::atomic::Ordering::SeqCst),
        bulks,
        "mirrored without the lease"
    );
    assert_eq!(fake.marker(&sid), Some(1));
    assert_eq!(store.tier_status(&sid).sync, TierSync::Stale);
}

/// M1: releasing the lease gives an in-flight repair a grace period, then
/// abandons it rather than holding the release (and `close`) hostage.
#[tokio::test]
async fn releasing_the_lease_abandons_a_stuck_repair_after_the_grace() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let store = tier(&primary, &fake).with_release_grace(Duration::from_millis(50));
    let sid = SessionId::new("release-stuck");
    let w = holder("w");
    let token = attach(&store, &sid, &w).await;
    let s = stale_after_an_outage(&store, &fake, &sid, token).await;
    fake.hold_deletes();
    let (c2, e2) = one_more(&sid, &s, 2);
    store.flush(&e2, Some(token)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), store.release_lease(&sid, &w))
        .await
        .expect("the release waited for the stuck repair")
        .unwrap();

    // The next holder repairs at load.
    fake.release_deletes();
    let next = tier(&primary, &fake);
    attach(&next, &sid, &holder("w2")).await;
    settle(&next).await;
    assert_eq!(next.tier_status(&sid).sync, TierSync::InSync);
    assert!(fake.live(&sid).contains_key(&c2.0.to_string()));
}

/// F7: every fetched hit carries its stored vector, so the over-fetch is
/// what bounds a read's response. It stays `limit + 16` for ordinary limits
/// and never exceeds the store-wide candidate limit.
#[tokio::test]
async fn the_overfetch_never_exceeds_the_candidate_limit() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let store = tier(&primary, &fake);
    let sid = SessionId::new("fetch-bound");
    let token = attach(&store, &sid, &holder("w")).await;
    let (_, b) = seed_batch(&sid, 1);
    store.flush(&b, Some(token)).await.unwrap();
    let asked = |limit| {
        let store = &store;
        let sid = &sid;
        let fake = &fake;
        async move {
            store
                .vector_candidates_checked(sid, &PROBE, &contract(), limit)
                .await
                .unwrap();
            fake.last_knn_k.load(std::sync::atomic::Ordering::SeqCst)
        }
    };
    assert_eq!(asked(5).await, 5 + super::KNN_OVERFETCH);
    let max = crate::store::MAX_VECTOR_CANDIDATE_LIMIT;
    assert_eq!(asked(max).await, max);
    assert_eq!(asked(max - 4).await, max);
}

/// A query-side failure on an in-sync session serves that read from the
/// primary.
#[tokio::test]
async fn an_index_query_failure_serves_the_read_from_the_primary() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let store = tier(&primary, &fake);
    let sid = SessionId::new("query-down");
    let token = attach(&store, &sid, &holder("w")).await;
    let (_, b) = seed_batch(&sid, 1);
    store.flush(&b, Some(token)).await.unwrap();
    fake.knn_down
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let got = store
        .vector_candidates_checked(&sid, &PROBE, &contract(), 10)
        .await
        .expect("a failed query degrades, it does not fail recall");
    assert!(got.is_empty(), "MemoryStore primary: no vector leg");
    assert_eq!(store.tier_status(&sid).sync, TierSync::InSync);
}

/// M2: after `threshold` consecutive failed reads the breaker opens and
/// reads go straight to the durable store without asking the index; after
/// the cool-down one read probes it, and a success closes the breaker.
#[tokio::test]
async fn failing_index_reads_open_a_breaker_that_a_probe_closes() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let store = tier(&primary, &fake).with_read_breaker(
        3,
        Duration::from_millis(100),
        Duration::from_secs(5),
    );
    let sid = SessionId::new("breaker");
    let token = attach(&store, &sid, &holder("w")).await;
    let (s, b) = seed_batch(&sid, 1);
    store.flush(&b, Some(token)).await.unwrap();
    let knn = || fake.knn_calls.load(std::sync::atomic::Ordering::SeqCst);
    let expected = contract();
    let read = || store.vector_candidates_checked(&sid, &PROBE, &expected, 5);

    fake.knn_down
        .store(true, std::sync::atomic::Ordering::SeqCst);
    for _ in 0..3 {
        read().await.expect("a failed read falls back");
    }
    assert_eq!(knn(), 3);
    for _ in 0..5 {
        read().await.unwrap();
    }
    assert_eq!(knn(), 3, "an open breaker does not ask the index");

    fake.knn_down
        .store(false, std::sync::atomic::Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(150)).await;
    let probed = read().await.unwrap();
    assert_eq!(knn(), 4, "one probe after the cool-down");
    assert_eq!(
        probed.first().map(|h| h.item),
        Some(s.c1),
        "served by the index"
    );
    read().await.unwrap();
    assert_eq!(knn(), 5, "closed again");
}

/// M2: a read that hangs (a blackholed cluster) is cut at the read
/// deadline, served from the durable store, and counts toward the breaker.
#[tokio::test]
async fn a_hanging_index_read_is_cut_at_the_deadline_and_counts() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let store = tier(&primary, &fake).with_read_breaker(
        2,
        Duration::from_secs(60),
        Duration::from_millis(50),
    );
    let sid = SessionId::new("blackhole");
    let token = attach(&store, &sid, &holder("w")).await;
    let (_, b) = seed_batch(&sid, 1);
    store.flush(&b, Some(token)).await.unwrap();
    fake.delay_knn_ms
        .store(5_000, std::sync::atomic::Ordering::SeqCst);
    let started = std::time::Instant::now();
    for _ in 0..4 {
        store
            .vector_candidates_checked(&sid, &PROBE, &contract(), 5)
            .await
            .unwrap();
    }
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        fake.knn_calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "two timeouts open the breaker"
    );
}

/// L5: releasing the lease drops the session's tier state.
#[tokio::test]
async fn releasing_the_lease_forgets_the_session() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let store = tier(&primary, &fake);
    let sid = SessionId::new("forget-me");
    let w = holder("w");
    let token = attach(&store, &sid, &w).await;
    let (_, b) = seed_batch(&sid, 1);
    store.flush(&b, Some(token)).await.unwrap();
    assert!(store.tracked_sessions().contains(&sid));
    store.release_lease(&sid, &w).await.unwrap();
    assert!(!store.tracked_sessions().contains(&sid));
}

/// L5: a reader that touches many sessions keeps a bounded map, evicting
/// sessions it neither holds nor repairs, least recently used first; a held
/// session is never evicted.
#[tokio::test]
async fn reader_state_is_bounded_and_held_sessions_stay() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let store = tier(&primary, &fake).with_max_sessions(4);
    let held = SessionId::new("held");
    let token = attach(&store, &held, &holder("w")).await;
    let (_, b) = seed_batch(&held, 1);
    store.flush(&b, Some(token)).await.unwrap();
    for i in 0..20 {
        store
            .vector_candidates_checked(&SessionId::new(format!("r{i}")), &PROBE, &contract(), 5)
            .await
            .unwrap();
    }
    let tracked = store.tracked_sessions();
    assert!(tracked.len() <= 4, "{tracked:?}");
    assert!(tracked.contains(&held));
    assert!(
        tracked.contains(&SessionId::new("r19")),
        "the most recent stays"
    );
    assert_eq!(
        store.tier_status(&held).flush_counter,
        1,
        "its counter survived"
    );
}

/// A crash between the primary's commit and the mirror leaves the marker
/// behind: a reader serves from the primary, the next holder repairs at load.
#[tokio::test]
async fn a_marker_behind_the_durable_epoch_is_caught_at_the_next_load() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let first = tier(&primary, &fake);
    let sid = SessionId::new("crash");
    let w = holder("w");
    let token = attach(&first, &sid, &w).await;
    let (s, b) = seed_batch(&sid, 1);
    first.flush(&b, Some(token)).await.unwrap();
    // The "crash": the primary commits, the mirror never runs.
    let c4 = NodeId::new();
    primary
        .flush(
            &batch(
                2,
                vec![upsert(concept(
                    &sid,
                    c4,
                    s.origin,
                    "delta",
                    Some(vec![0.0, 0.0, 1.0, 0.0]),
                ))],
            ),
            Some(token),
        )
        .await
        .unwrap();
    first.release_lease(&sid, &w).await.unwrap();

    let reader = tier(&primary, &fake);
    reader.load_session(&sid).await.unwrap();
    assert_eq!(reader.tier_status(&sid).sync, TierSync::Stale);
    let calls = fake.knn_calls.load(std::sync::atomic::Ordering::SeqCst);
    reader
        .vector_candidates_checked(&sid, &PROBE, &contract(), 10)
        .await
        .unwrap();
    assert_eq!(
        fake.knn_calls.load(std::sync::atomic::Ordering::SeqCst),
        calls,
        "a reader never trusts a marker that is behind"
    );
    assert!(
        !fake.live(&sid).contains_key(&c4.0.to_string()),
        "and a reader never writes to the index"
    );

    let next = tier(&primary, &fake);
    attach(&next, &sid, &holder("w2")).await;
    settle(&next).await;
    assert_eq!(next.tier_status(&sid).sync, TierSync::InSync);
    assert!(fake.live(&sid).contains_key(&c4.0.to_string()));
    assert_eq!(fake.marker(&sid), Some(2));
}

/// F1: a reader loads the session after the holder's flush committed but
/// before its mirror landed, so it sees the marker behind. That is the same
/// race as a marker ahead, from the other side: once the mirror lands, the
/// reader's next read re-checks and serves from the index again.
#[tokio::test]
async fn a_reader_that_saw_the_marker_behind_returns_once_the_mirror_lands() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let writer = tier(&primary, &fake);
    let sid = SessionId::new("behind-race");
    let token = attach(&writer, &sid, &holder("w")).await;
    let (s, b) = seed_batch(&sid, 1);
    writer.flush(&b, Some(token)).await.unwrap();

    let reader = tier(&primary, &fake);
    let knn = || fake.knn_calls.load(std::sync::atomic::Ordering::SeqCst);
    fake.delay_bulk_ms
        .store(300, std::sync::atomic::Ordering::SeqCst);
    let (c4, e2) = one_more(&sid, &s, 2);
    let reader_side = async {
        // The holder's commit is durable; its mirror is still in flight.
        while primary.load_session(&sid).await.unwrap().mutation_epoch < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        reader.load_session(&sid).await.unwrap();
        assert_eq!(reader.tier_status(&sid).sync, TierSync::Stale);
        reader
            .vector_candidates_checked(&sid, &PROBE, &contract(), 10)
            .await
            .unwrap();
        assert_eq!(knn(), 0, "a marker behind is not trusted");
    };
    let (flushed, ()) = tokio::join!(writer.flush(&e2, Some(token)), reader_side);
    flushed.unwrap();
    assert_eq!(fake.marker(&sid), Some(2), "the holder's mirror landed");

    let got = reader
        .vector_candidates_checked(&sid, &[0.0, 0.0, 1.0, 0.0], &contract(), 10)
        .await
        .unwrap();
    assert_eq!(knn(), 1, "the reader never went back to the index");
    assert_eq!(got.first().map(|h| h.item), Some(c4));
    assert_eq!(reader.tier_status(&sid).sync, TierSync::InSync);
}

/// F1: the re-check is bounded. A reader that keeps finding the marker
/// behind costs one durable load per session per backoff, not one per read.
#[tokio::test]
async fn a_reader_rechecks_a_stale_session_at_most_once_per_backoff() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let writer = tier(&primary, &fake);
    let sid = SessionId::new("behind-backoff");
    let w = holder("w");
    let token = attach(&writer, &sid, &w).await;
    let (s, b) = seed_batch(&sid, 1);
    writer.flush(&b, Some(token)).await.unwrap();
    let (_, e2) = one_more(&sid, &s, 2);
    primary.flush(&e2, Some(token)).await.unwrap();
    writer.release_lease(&sid, &w).await.unwrap();

    let loads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let reader = TieredStore::new(
        Box::new(CountingLoads(Shared::new(primary.clone()), loads.clone())),
        Box::new(fake.clone()),
        Some(4),
    );
    reader.load_session(&sid).await.unwrap();
    for _ in 0..3 {
        reader
            .vector_candidates_checked(&sid, &PROBE, &contract(), 10)
            .await
            .unwrap();
    }
    assert_eq!(loads.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(reader.tier_status(&sid).sync, TierSync::Stale);
}

/// F1, the holder's side: a holder whose session went stale and that does
/// not flush again still gets its index back. A read asks for the repair.
#[tokio::test]
async fn a_holder_read_of_a_stale_session_asks_for_a_repair() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let store = tier(&primary, &fake);
    let sid = SessionId::new("idle-holder");
    let token = attach(&store, &sid, &holder("w")).await;
    let s = stale_after_an_outage(&store, &fake, &sid, token).await;
    let knn = || fake.knn_calls.load(std::sync::atomic::Ordering::SeqCst);
    store
        .vector_candidates_checked(&sid, &PROBE, &contract(), 10)
        .await
        .unwrap();
    assert_eq!(knn(), 0, "served from the primary while stale");
    settle(&store).await;
    assert_eq!(store.tier_status(&sid).sync, TierSync::InSync);
    let got = store
        .vector_candidates_checked(&sid, &PROBE, &contract(), 10)
        .await
        .unwrap();
    assert_eq!(knn(), 1);
    assert_eq!(got.first().map(|h| h.item), Some(s.c1));
}

/// M4: the holder's process loads an older snapshot (e1) while its own flush
/// has already committed and mirrored e2, so the marker is *ahead* of the
/// load. That is not staleness: repairing from the older snapshot would
/// re-index the node e2 deleted, delete the concept e2 added and rewind the
/// marker, all while the session reports in sync. A marker ahead is
/// re-checked against a fresh load, never repaired from.
#[tokio::test]
async fn a_marker_ahead_of_the_loaded_snapshot_is_rechecked_not_repaired() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let shared = Shared::new(primary.clone());
    let pin = shared.1.clone();
    let store = TieredStore::new(Box::new(shared), Box::new(fake.clone()), Some(4))
        .with_repair_backoff(Duration::ZERO);
    let sid = SessionId::new("ahead");
    let token = attach(&store, &sid, &holder("w")).await;
    let (s, b) = seed_batch(&sid, 1);
    store.flush(&b, Some(token)).await.unwrap();
    let older = primary.load_session(&sid).await.unwrap();
    let c4 = NodeId::new();
    store
        .flush(
            &batch(
                2,
                vec![
                    upsert(concept(
                        &sid,
                        c4,
                        s.origin,
                        "delta",
                        Some(vec![0.0, 0.0, 1.0, 0.0]),
                    )),
                    Mutation::DeleteNode { id: s.c2 },
                ],
            ),
            Some(token),
        )
        .await
        .unwrap();
    assert_eq!(fake.marker(&sid), Some(2));

    pin.lock().push(older);
    store.load_session(&sid).await.unwrap();
    settle(&store).await;

    let live = fake.live(&sid);
    assert!(
        live.contains_key(&c4.0.to_string()),
        "e2's concept was removed"
    );
    assert!(
        !live.contains_key(&s.c2.0.to_string()),
        "e2's delete was undone"
    );
    assert_eq!(fake.marker(&sid), Some(2), "the marker was rewound");
    assert_eq!(store.tier_status(&sid).sync, TierSync::InSync);
}

/// M4: a marker that is still ahead after the re-load is left alone: the
/// session is not trusted (reads fall back) and nothing is repaired from a
/// snapshot older than what the index already reflects.
#[tokio::test]
async fn a_marker_still_ahead_after_a_reload_is_never_repaired_from() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let shared = Shared::new(primary.clone());
    let pin = shared.1.clone();
    let store = TieredStore::new(Box::new(shared), Box::new(fake.clone()), Some(4))
        .with_repair_backoff(Duration::ZERO);
    let sid = SessionId::new("still-ahead");
    let token = attach(&store, &sid, &holder("w")).await;
    let (s, b) = seed_batch(&sid, 1);
    store.flush(&b, Some(token)).await.unwrap();
    let older = primary.load_session(&sid).await.unwrap();
    store
        .flush(
            &batch(2, vec![Mutation::DeleteNode { id: s.c2 }]),
            Some(token),
        )
        .await
        .unwrap();
    let bulks = fake.bulk_calls.load(std::sync::atomic::Ordering::SeqCst);

    // Every load for a while answers with the older snapshot.
    pin.lock().extend([older.clone(), older.clone(), older]);
    store.load_session(&sid).await.unwrap();
    settle(&store).await;

    assert_eq!(
        fake.bulk_calls.load(std::sync::atomic::Ordering::SeqCst),
        bulks,
        "repaired from an older snapshot"
    );
    assert!(!fake.live(&sid).contains_key(&s.c2.0.to_string()));
    assert_eq!(fake.marker(&sid), Some(2));
    assert_eq!(store.tier_status(&sid).sync, TierSync::Unknown);
}

/// F3: a marker that stays ahead for a holder (the durable store was
/// restored from a backup while the index survived) is not a race. It is
/// never repaired from, and the holder backs off: its flushes do not each
/// pay a full durable load (and a warning) to find the same thing.
#[tokio::test]
async fn a_marker_that_stays_ahead_backs_off_the_holder() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let loads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let store = TieredStore::new(
        Box::new(CountingLoads(Shared::new(primary.clone()), loads.clone())),
        Box::new(fake.clone()),
        Some(4),
    );
    let sid = SessionId::new("restored");
    let token = attach(&store, &sid, &holder("w")).await;
    let (s, b) = seed_batch(&sid, 1);
    store.flush(&b, Some(token)).await.unwrap();
    assert_eq!(store.tier_status(&sid).sync, TierSync::InSync);
    // The index has seen far more than the durable store now holds.
    fake.write_marker(
        &sid,
        super::index::SyncMarker { synced_epoch: 99 },
        Some(u64::MAX),
    )
    .await
    .unwrap();
    store.load_session(&sid).await.unwrap();
    settle(&store).await;
    assert_eq!(store.tier_status(&sid).sync, TierSync::Unknown);

    let bulks = fake.bulk_calls.load(std::sync::atomic::Ordering::SeqCst);
    let before = loads.load(std::sync::atomic::Ordering::SeqCst);
    for epoch in 2..=4 {
        let (_, more) = one_more(&sid, &s, epoch);
        store.flush(&more, Some(token)).await.unwrap();
        settle(&store).await;
    }
    assert_eq!(
        loads.load(std::sync::atomic::Ordering::SeqCst),
        before,
        "every flush re-read the whole durable session"
    );
    assert_eq!(
        fake.bulk_calls.load(std::sync::atomic::Ordering::SeqCst),
        bulks
    );
    assert_eq!(fake.marker(&sid), Some(99), "never repaired from");
    assert_eq!(store.tier_status(&sid).sync, TierSync::Unknown);
}

/// A holder attaching while the index is unreachable keeps serving from the
/// primary, and a reader never trusts an unknown state either.
#[tokio::test]
async fn an_unreachable_index_at_load_is_never_trusted() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let seed = tier(&primary, &fake);
    let sid = SessionId::new("unreachable");
    let w = holder("w");
    let token = attach(&seed, &sid, &w).await;
    let (_, b) = seed_batch(&sid, 1);
    seed.flush(&b, Some(token)).await.unwrap();
    seed.release_lease(&sid, &w).await.unwrap();

    fake.set_down(true);
    let reader = tier(&primary, &fake);
    reader.load_session(&sid).await.unwrap();
    assert_ne!(reader.tier_status(&sid).sync, TierSync::InSync);
    reader
        .vector_candidates_checked(&sid, &PROBE, &contract(), 10)
        .await
        .unwrap();
    assert_eq!(fake.knn_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}

/// Counts durable loads, to see what a read costs while the index is down.
struct CountingLoads(Shared, Arc<std::sync::atomic::AtomicUsize>);

#[async_trait]
impl GraphStore for CountingLoads {
    async fn init_schema(&self) -> Result<(), StoreError> {
        self.0.init_schema().await
    }
    fn capabilities(&self) -> Capabilities {
        self.0.capabilities()
    }
    async fn flush(&self, b: &MutationBatch, t: Option<u64>) -> Result<(), StoreError> {
        self.0.flush(b, t).await
    }
    async fn load_session(&self, s: &SessionId) -> Result<GraphSnapshot, StoreError> {
        self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.0.load_session(s).await
    }
    async fn keyword_candidates(
        &self,
        s: &SessionId,
        t: &[String],
        l: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.0.keyword_candidates(s, t, l).await
    }
    async fn vector_candidates(
        &self,
        s: &SessionId,
        e: &[f32],
        l: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.0.vector_candidates(s, e, l).await
    }
    async fn blast_radius(
        &self,
        s: &SessionId,
        n: NodeId,
        a: Duration,
        now: chrono::DateTime<Utc>,
    ) -> Result<u64, StoreError> {
        self.0.blast_radius(s, n, a, now).await
    }
    async fn interaction_span(
        &self,
        s: &SessionId,
        n: NodeId,
        a: Duration,
        now: chrono::DateTime<Utc>,
    ) -> Result<InteractionSpan, StoreError> {
        self.0.interaction_span(s, n, a, now).await
    }
    async fn record_canonization(
        &self,
        e: &CanonizationEvent,
        t: Option<u64>,
    ) -> Result<(), StoreError> {
        self.0.record_canonization(e, t).await
    }
}

/// A reader that cannot reach the index serves from the primary without
/// re-reading the whole durable session on every recall to re-check.
#[tokio::test]
async fn an_unreachable_index_is_not_rechecked_on_every_read() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let seed = tier(&primary, &fake);
    let sid = SessionId::new("recheck");
    let w = holder("w");
    let token = attach(&seed, &sid, &w).await;
    let (_, b) = seed_batch(&sid, 1);
    seed.flush(&b, Some(token)).await.unwrap();
    seed.release_lease(&sid, &w).await.unwrap();

    fake.set_down(true);
    let loads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let reader = TieredStore::new(
        Box::new(CountingLoads(Shared::new(primary.clone()), loads.clone())),
        Box::new(fake.clone()),
        Some(4),
    );
    for _ in 0..3 {
        reader
            .vector_candidates_checked(&sid, &PROBE, &contract(), 10)
            .await
            .unwrap();
    }
    assert_eq!(
        loads.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "one durable load to check, then the backoff holds"
    );
}

/// A counter that would wrap refuses the mirror instead (the issue's
/// "never wrap"), and the session repairs at a fresh token.
#[tokio::test]
async fn an_exhausted_flush_counter_refuses_the_mirror() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let store = tier(&primary, &fake);
    let sid = SessionId::new("exhausted");
    let token = attach(&store, &sid, &holder("w")).await;
    let (_, b) = seed_batch(&sid, 1);
    store.flush(&b, Some(token)).await.unwrap();
    store.with_state(&sid, |st| st.counter = u32::MAX);
    let (_, more) = seed_batch(&sid, 2);
    store.flush(&more, Some(token)).await.unwrap();
    let status = store.tier_status(&sid);
    assert_eq!(status.sync, TierSync::Stale);
    assert!(
        status
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("exhausted")),
        "{status:?}"
    );
    assert_eq!(fake.marker(&sid), Some(1), "nothing written past the wrap");
}

/// A hand-built batch over two sessions cannot be attributed: nothing is
/// mirrored and both sessions go stale rather than being trusted.
#[tokio::test]
async fn a_batch_over_several_sessions_marks_each_stale() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let store = tier(&primary, &fake);
    let (a, b) = (SessionId::new("a"), SessionId::new("b"));
    let _ = store.load_session(&a).await;
    let _ = store.load_session(&b).await;
    let (_, ba) = seed_batch(&a, 1);
    let (_, bb) = seed_batch(&b, 1);
    let mut both = ba.mutations;
    both.extend(bb.mutations);
    store.flush(&batch(1, both), None).await.unwrap();
    assert_eq!(store.tier_status(&a).sync, TierSync::Stale);
    assert_eq!(store.tier_status(&b).sync, TierSync::Stale);
    assert!(fake.live(&a).is_empty() && fake.live(&b).is_empty());
}

// ---------------------------------------------------------------------------
// Deletes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_deleted_node_leaves_the_index() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let store = tier(&primary, &fake);
    let sid = SessionId::new("deletes");
    let token = attach(&store, &sid, &holder("w")).await;
    let (s, b) = seed_batch(&sid, 1);
    store.flush(&b, Some(token)).await.unwrap();
    // A delete-only batch (a GC sweep) names no session: it is attributed
    // through the lease this store holds under the same token.
    store
        .flush(
            &batch(2, vec![Mutation::DeleteNode { id: s.c1 }]),
            Some(token),
        )
        .await
        .unwrap();
    assert!(!fake.live(&sid).contains_key(&s.c1.0.to_string()));
    assert_eq!(
        fake.marker(&sid),
        Some(2),
        "attributed, so the marker advances"
    );
    assert_eq!(store.tier_status(&sid).sync, TierSync::InSync);
}

#[tokio::test]
async fn an_unattributable_delete_only_batch_deletes_by_id() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let store = tier(&primary, &fake);
    let sid = SessionId::new("unleased");
    // An index holding the session (from an earlier holder), and no lease
    // held here: a delete-only batch names no session and cannot be
    // attributed.
    let (s, b) = seed_batch(&sid, 1);
    primary.flush(&b, None).await.unwrap();
    plant_snapshot(&primary, &fake, &sid).await;
    fake.write_marker(&sid, super::index::SyncMarker { synced_epoch: 1 }, Some(1))
        .await
        .unwrap();
    assert_eq!(fake.live(&sid).len(), 2);
    store
        .flush(&batch(2, vec![Mutation::DeleteNode { id: s.c2 }]), None)
        .await
        .unwrap();
    assert!(!fake.live(&sid).contains_key(&s.c2.0.to_string()));
    assert_eq!(
        fake.marker(&sid),
        Some(1),
        "an unattributed batch leaves the marker behind, so the next load repairs"
    );
}

/// L4: an unleased write (the seed and fixture path) is never mirrored. The
/// engine would apply it with internal versioning, which bumps an externally
/// versioned document to V+1 and collides with the next leased write at
/// `(T << 32) | (c + 1)`: that write's 409 counts as success and a stale
/// document is served while in sync. Instead the session goes stale and the
/// next holder load (or `lambo recall-index backfill`) repairs it.
#[tokio::test]
async fn an_unleased_flush_is_not_mirrored_and_the_next_holder_repairs() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let store = tier(&primary, &fake);
    let sid = SessionId::new("seeded");
    let _ = store.load_session(&sid).await;
    let (s, b) = seed_batch(&sid, 1);
    store.flush(&b, None).await.unwrap();
    assert!(
        fake.live(&sid).is_empty(),
        "an unleased write reached the index"
    );
    assert_eq!(fake.bulk_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    let status = store.tier_status(&sid);
    assert_eq!(status.sync, TierSync::Stale);
    assert_eq!(status.mirror_failures, 0, "not a failure, a policy");
    store
        .vector_candidates_checked(&sid, &PROBE, &contract(), 5)
        .await
        .unwrap();
    assert_eq!(fake.knn_calls.load(std::sync::atomic::Ordering::SeqCst), 0);

    let holder_store = tier(&primary, &fake);
    attach(&holder_store, &sid, &holder("w")).await;
    settle(&holder_store).await;
    assert_eq!(holder_store.tier_status(&sid).sync, TierSync::InSync);
    assert!(fake.live(&sid).contains_key(&s.c1.0.to_string()));
}

/// Plant every vector of the session's durable snapshot in the index, as an
/// earlier holder would have mirrored it.
async fn plant_snapshot(primary: &Arc<dyn GraphStore>, fake: &FakeIndex, sid: &SessionId) {
    let snap = primary.load_session(sid).await.unwrap();
    for c in &snap.concepts {
        if let Some(doc) = super::project::index_doc(c, &contract(), Some(1)) {
            fake.plant(&contract(), doc);
        }
    }
}

/// H1, the by-id half: a delete-by-id that loses a document to a version
/// conflict is retried, not counted as done.
#[tokio::test]
async fn an_unattributed_delete_retries_a_version_conflict() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let store = tier(&primary, &fake);
    let sid = SessionId::new("by-id-conflict");
    let (s, b) = seed_batch(&sid, 1);
    primary.flush(&b, None).await.unwrap();
    plant_snapshot(&primary, &fake, &sid).await;
    fake.conflicts_next_delete
        .store(1, std::sync::atomic::Ordering::SeqCst);
    store
        .flush(&batch(2, vec![Mutation::DeleteNode { id: s.c2 }]), None)
        .await
        .unwrap();
    assert!(
        !fake.live(&sid).contains_key(&s.c2.0.to_string()),
        "the conflicting delete was not retried"
    );
    assert!(fake.live(&sid).contains_key(&s.c1.0.to_string()));
}

// ---------------------------------------------------------------------------
// Backfill and erase
// ---------------------------------------------------------------------------

#[tokio::test]
async fn backfill_converges_and_refuses_a_live_writer() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let seeder = tier(&primary, &fake);
    let sid = SessionId::new("backfill");
    let w = holder("w");
    let token = attach(&seeder, &sid, &w).await;
    let (s, b) = seed_batch(&sid, 5);
    seeder.flush(&b, Some(token)).await.unwrap();

    let ops = TieredStore::new(
        Box::new(Shared::new(primary.clone())),
        Box::new(fake.clone()),
        Some(4),
    );
    let err = ops
        .backfill_recall_index(&sid, &holder("operator"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("live writer"), "{err}");
    seeder.release_lease(&sid, &w).await.unwrap();

    // Junk the index: a ghost the durable store never had, and a lost doc.
    let ghost = NodeId::new();
    fake.plant(
        &contract(),
        super::project::index_doc(
            &concept(
                &sid,
                ghost,
                s.origin,
                "ghost",
                Some(vec![1.0, 0.0, 0.0, 0.0]),
            ),
            &contract(),
            Some(1),
        )
        .unwrap(),
    );
    fake.docs
        .lock()
        .retain(|(_, id), _| id != &s.c2.0.to_string());

    let report: RecallBackfillReport = ops
        .backfill_recall_index(&sid, &holder("operator"))
        .await
        .unwrap()
        .expect("a tiered store has an index to rebuild");
    assert_eq!(report.indexed, 2);
    assert_eq!(report.mutation_epoch, 5);
    assert_eq!(report.index, Some(fake.index_name(&contract())));
    let live = fake.live(&sid);
    assert!(!live.contains_key(&ghost.0.to_string()), "ghost removed");
    assert!(live.contains_key(&s.c2.0.to_string()), "lost doc restored");
    assert_eq!(fake.marker(&sid), Some(5));
    assert!(
        primary
            .read_lease(&sid)
            .await
            .unwrap()
            .is_some_and(|l| l.expires_at <= Utc::now()),
        "the backfill released its lease"
    );
}

#[tokio::test]
async fn a_store_without_a_tier_has_nothing_to_backfill() {
    let store = MemoryStore::new();
    let got = store
        .backfill_recall_index(&SessionId::new("s"), &holder("operator"))
        .await
        .unwrap();
    assert!(got.is_none());
}

/// #23: erase reaches the index, refuses to report done while vectors are
/// still searchable, and a rerun finishes the job.
#[tokio::test]
async fn erase_removes_the_session_from_the_index_and_a_rerun_finishes_it() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let store = tier(&primary, &fake);
    let sid = SessionId::new("erase-me");
    let other = SessionId::new("keep-me");
    let w = holder("w");
    let token = attach(&store, &sid, &w).await;
    let (_, b) = seed_batch(&sid, 1);
    store.flush(&b, Some(token)).await.unwrap();
    let tw = attach(&store, &other, &holder("w-other")).await;
    let (_, keep) = seed_batch(&other, 1);
    store.flush(&keep, Some(tw)).await.unwrap();

    let eraser = holder("lambo-erase-session");
    let held = store.erase_session(&sid, &eraser).await.unwrap();
    assert!(matches!(held, EraseOutcome::Held { .. }));
    assert_eq!(fake.live(&sid).len(), 2, "a refused erase touches nothing");
    store.release_lease(&sid, &w).await.unwrap();

    fake.set_down(true);
    let err = store.erase_session(&sid, &eraser).await.unwrap_err();
    assert!(err.to_string().contains("Run erase-session again"), "{err}");
    fake.set_down(false);
    assert_eq!(
        fake.live(&sid).len(),
        2,
        "still searchable, so not reported done"
    );

    match store.erase_session(&sid, &eraser).await.unwrap() {
        EraseOutcome::Erased(report) => assert!(report.already_absent),
        EraseOutcome::Held { .. } => panic!("not held"),
    }
    assert!(fake.live(&sid).is_empty());
    assert_eq!(fake.marker(&sid), None);
    assert_eq!(fake.live(&other).len(), 2, "other sessions untouched");
}

/// H1: the writer's last flush was mirrored with `refresh=false` and erase
/// runs inside one refresh interval. Delete-by-query only sees what a
/// refresh exposed, so the erase must refresh first, and must not report
/// done while anything of the session is still searchable.
#[tokio::test]
async fn erase_removes_documents_a_refresh_has_not_exposed_yet() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::lagging()));
    let store = tier(&primary, &fake);
    let sid = SessionId::new("erase-pending");
    let w = holder("w");
    let token = attach(&store, &sid, &w).await;
    let (_, b) = seed_batch(&sid, 1);
    store.flush(&b, Some(token)).await.unwrap();
    assert!(
        fake.searchable(&sid).is_empty(),
        "the flush is still pending"
    );
    store.release_lease(&sid, &w).await.unwrap();

    let outcome = store
        .erase_session(&sid, &holder("lambo-erase-session"))
        .await
        .unwrap();
    assert!(matches!(outcome, EraseOutcome::Erased(_)));
    fake.refresh_now();
    assert!(
        fake.searchable(&sid).is_empty(),
        "erase reported done but the session is searchable: {:?}",
        fake.searchable(&sid).keys().collect::<Vec<_>>()
    );
    assert!(fake.live(&sid).is_empty());
    assert_eq!(fake.marker(&sid), None);
}

/// H1, the repair half: a repair's "delete everything below the version I
/// just wrote" must also catch documents mirrored inside the last refresh
/// interval, or a node deleted durably keeps a searchable vector.
#[tokio::test]
async fn a_repair_sweeps_documents_a_refresh_has_not_exposed_yet() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::lagging()));
    let first = tier(&primary, &fake);
    let sid = SessionId::new("repair-pending");
    let w = holder("w");
    let token = attach(&first, &sid, &w).await;
    let (s, b) = seed_batch(&sid, 1);
    first.flush(&b, Some(token)).await.unwrap();
    // The "crash": c1 is deleted durably and the mirror never runs.
    primary
        .flush(
            &batch(2, vec![Mutation::DeleteNode { id: s.c1 }]),
            Some(token),
        )
        .await
        .unwrap();
    first.release_lease(&sid, &w).await.unwrap();

    let next = tier(&primary, &fake);
    attach(&next, &sid, &holder("w2")).await;
    settle(&next).await;
    fake.refresh_now();
    let searchable = fake.searchable(&sid);
    assert!(
        !searchable.contains_key(&s.c1.0.to_string()),
        "a durably deleted node is still searchable after the repair"
    );
    assert!(searchable.contains_key(&s.c2.0.to_string()));
    assert_eq!(fake.marker(&sid), Some(2));
}

/// H1: a document rewritten between delete-by-query's scroll and its delete
/// is a version conflict the engine skips under `conflicts=proceed`. Erase
/// retries it, and reports done only once a count finds nothing left.
#[tokio::test]
async fn erase_retries_a_version_conflict_and_confirms_nothing_is_left() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let store = tier(&primary, &fake);
    let sid = SessionId::new("erase-conflict");
    let w = holder("w");
    let token = attach(&store, &sid, &w).await;
    let (_, b) = seed_batch(&sid, 1);
    store.flush(&b, Some(token)).await.unwrap();
    store.release_lease(&sid, &w).await.unwrap();

    fake.conflicts_next_delete
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let outcome = store
        .erase_session(&sid, &holder("lambo-erase-session"))
        .await
        .unwrap();
    assert!(matches!(outcome, EraseOutcome::Erased(_)));
    fake.refresh_now();
    assert!(
        fake.live(&sid).is_empty() && fake.searchable(&sid).is_empty(),
        "a conflicting document survived a successful erase"
    );
}

// ---------------------------------------------------------------------------
// End to end: a stale index hit never reaches the assembled recall
// ---------------------------------------------------------------------------

/// The index can hold a node the durable store already deleted (a lagging or
/// dirty index). `lambo recall` must skip it without spending a `top_k` slot.
#[tokio::test]
async fn a_stale_index_hit_never_reaches_the_assembled_recall() {
    use crate::embed::{EmbedderConfig, EmbedderKind, FixtureEmbedder};
    let fixture = FixtureEmbedder::new();
    let full = EmbeddingContract {
        kind: "fixture".into(),
        model: None,
        dim: 1024,
    };
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let writer = TieredStore::new(
        Box::new(Shared::new(primary.clone())),
        Box::new(fake.clone()),
        Some(1024),
    );
    let sid = SessionId::new("stale-hit");
    let w = holder("w");
    let token = attach(&writer, &sid, &w).await;
    let origin = NodeId::new();
    let live_id = NodeId::new();
    let text = "postgres connection pool sizing";
    writer
        .flush(
            &batch(
                4,
                vec![
                    set_contract(&sid, Some(full.clone())),
                    interaction(&sid, origin),
                    upsert(concept(
                        &sid,
                        live_id,
                        origin,
                        text,
                        Some(fixture.embed_sync(text)),
                    )),
                    Mutation::UpsertEdge {
                        edge: crate::types::Edge {
                            event_time: None,
                            id: NodeId::new(),
                            session_id: sid.clone(),
                            source: origin,
                            target: live_id,
                            edge_type: crate::types::EdgeType::Derives,
                            weight: 1.0,
                            reinforcements: 1,
                            created_at: Utc::now(),
                            last_reinforced: Utc::now(),
                        },
                    },
                ],
            ),
            Some(token),
        )
        .await
        .unwrap();
    writer.release_lease(&sid, &w).await.unwrap();
    // The ghost scores a perfect match for the query and is not in the graph.
    let query = "connection pool";
    let ghost = NodeId::new();
    fake.plant(
        &full,
        super::project::index_doc(
            &concept(
                &sid,
                ghost,
                origin,
                "ghost-concept",
                Some(fixture.embed_sync(query)),
            ),
            &full,
            mirror_version(token, 100),
        )
        .unwrap(),
    );

    let backends = crate::resolve::ResolvedBackends {
        store: Box::new(TieredStore::new(
            Box::new(Shared::new(primary.clone())),
            Box::new(fake.clone()),
            Some(1024),
        )),
        embedder: Box::new(FixtureEmbedder::new()),
        store_cfg: Default::default(),
        embedder_cfg: EmbedderConfig {
            kind: EmbedderKind::Fixture,
            dim: 1024,
            ..Default::default()
        },
        embedding: full,
        allow_embedding_mismatch: false,
        config: crate::Config::default(),
    };
    // The vector leg really does rank the ghost first.
    let probe = fixture.embed_sync(query);
    let leg = backends
        .store
        .vector_candidates_checked(&sid, &probe, &backends.embedding, 2)
        .await
        .unwrap();
    assert_eq!(leg.first().map(|s| s.item), Some(ghost), "{leg:?}");
    let out = crate::cli::recall::run(&backends, &sid.0, query, Some(1), None, Some(0))
        .await
        .unwrap();
    assert!(
        fake.knn_calls.load(std::sync::atomic::Ordering::SeqCst) > 0,
        "the reader asked the index"
    );
    assert!(out.contains(text), "the live concept fills the slot: {out}");
    assert!(!out.contains("ghost-concept"), "{out}");
}

// ---------------------------------------------------------------------------
// Parity with the primary's own exact read (SQLite)
// ---------------------------------------------------------------------------

#[cfg(feature = "store-sqlite")]
mod sqlite_parity {
    use super::*;
    use crate::store::SqliteStore;

    async fn sqlite_primary() -> Arc<dyn GraphStore> {
        let store = SqliteStore::connect("sqlite::memory:")
            .unwrap()
            .with_vector_dim(4)
            .unwrap();
        store.init_schema().await.unwrap();
        Arc::new(store)
    }

    fn spread(sid: &SessionId, origin: NodeId) -> Vec<Mutation> {
        let vectors: [[f32; 4]; 6] = [
            [1.0, 0.0, 0.0, 0.0],
            [0.9, 0.1, 0.0, 0.0],
            [0.5, 0.5, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.3, 0.3, 0.3, 0.3],
        ];
        let mut out = vec![
            set_contract(sid, Some(contract())),
            interaction(sid, origin),
        ];
        for (i, v) in vectors.iter().enumerate() {
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            let unit: Vec<f32> = v.iter().map(|x| x / norm).collect();
            out.push(upsert(concept(
                sid,
                NodeId::new(),
                origin,
                &format!("k{i}"),
                Some(unit),
            )));
        }
        out
    }

    /// The index ranks like the primary's exact scan on the same vectors.
    #[tokio::test]
    async fn index_ranking_matches_the_exact_scan() {
        let primary = sqlite_primary().await;
        let fake = Arc::new(FakeIndex::new());
        let store = tier(&primary, &fake);
        let sid = SessionId::new("parity");
        let token = attach(&store, &sid, &holder("w")).await;
        store
            .flush(&batch(1, spread(&sid, NodeId::new())), Some(token))
            .await
            .unwrap();
        let from_index = store
            .vector_candidates_checked(&sid, &PROBE, &contract(), 4)
            .await
            .unwrap();
        let exact = primary
            .vector_candidates_checked(&sid, &PROBE, &contract(), 4)
            .await
            .unwrap();
        assert_eq!(ids(&from_index), ids(&exact));
        for (a, b) in from_index.iter().zip(&exact) {
            assert!(
                (a.score - b.score).abs() < 1e-6,
                "{} vs {}",
                a.score,
                b.score
            );
        }
        assert!(fake.knn_calls.load(std::sync::atomic::Ordering::SeqCst) > 0);
    }

    /// M3: an engine that scores approximately (int8 quantization, an f32
    /// `_score`) only prunes the pool. The tier re-ranks what it returns with
    /// the stored vectors, so ranks and scores are the exact scan's, bit for
    /// bit, and near-ties at the limit resolve by the issue-2 tie-break.
    #[tokio::test]
    async fn index_scores_are_exact_against_an_approximate_engine() {
        let primary = sqlite_primary().await;
        let fake = Arc::new(FakeIndex::new().quantized());
        let store = tier(&primary, &fake);
        let sid = SessionId::new("approx");
        let token = attach(&store, &sid, &holder("w")).await;
        let origin = NodeId::new();
        let mut muts = spread(&sid, origin);
        // Two near-ties the quantizer can reorder.
        for (i, v) in [[0.95f32, 0.31, 0.02, 0.0], [0.95, 0.31, 0.0, 0.02]]
            .iter()
            .enumerate()
        {
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            muts.push(upsert(concept(
                &sid,
                NodeId::new(),
                origin,
                &format!("tie{i}"),
                Some(v.iter().map(|x| x / norm).collect()),
            )));
        }
        store.flush(&batch(1, muts), Some(token)).await.unwrap();
        for limit in [1, 3, 5, 8] {
            let from_index = store
                .vector_candidates_checked(&sid, &PROBE, &contract(), limit)
                .await
                .unwrap();
            let exact = primary
                .vector_candidates_checked(&sid, &PROBE, &contract(), limit)
                .await
                .unwrap();
            assert_eq!(ids(&from_index), ids(&exact), "limit {limit}");
            for (a, b) in from_index.iter().zip(&exact) {
                assert_eq!(a.score.to_bits(), b.score.to_bits(), "limit {limit}");
            }
        }
        assert!(fake.knn_calls.load(std::sync::atomic::Ordering::SeqCst) > 0);
    }

    /// M3: two vectors the quantizer orders the wrong way round. Asked for
    /// one, the engine alone would return the wrong one; the over-fetch
    /// hands both to the exact re-rank, which picks the right one.
    #[tokio::test]
    async fn the_overfetch_lets_the_exact_rank_decide_the_boundary() {
        let primary = sqlite_primary().await;
        let fake = Arc::new(FakeIndex::new().quantized());
        let store = tier(&primary, &fake);
        let sid = SessionId::new("boundary");
        let token = attach(&store, &sid, &holder("w")).await;
        let origin = NodeId::new();
        let mut muts = vec![
            set_contract(&sid, Some(contract())),
            interaction(&sid, origin),
        ];
        for (key, v) in [
            ("exact-winner", [0.9715f32, 0.2356, 0.0222, 0.0135]),
            ("engine-winner", [0.974, 0.2228, 0.0403, 0.0006]),
        ] {
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            muts.push(upsert(concept(
                &sid,
                NodeId::new(),
                origin,
                key,
                Some(v.iter().map(|x| x / norm).collect()),
            )));
        }
        store.flush(&batch(1, muts), Some(token)).await.unwrap();
        let engine_alone = fake.knn(&contract(), &sid, &PROBE, 1).await.unwrap();
        assert_eq!(
            engine_alone[0].canonical_key, "engine-winner",
            "precondition: the approximate engine misorders the pair"
        );
        let got = store
            .vector_candidates_checked(&sid, &PROBE, &contract(), 1)
            .await
            .unwrap();
        let exact = primary
            .vector_candidates_checked(&sid, &PROBE, &contract(), 1)
            .await
            .unwrap();
        assert_eq!(ids(&got), ids(&exact));
    }

    /// Index down, or the session stale: the answer is the primary's own.
    #[tokio::test]
    async fn the_fallback_is_the_primary_exact_read() {
        let primary = sqlite_primary().await;
        let fake = Arc::new(FakeIndex::new());
        let store = tier(&primary, &fake);
        let sid = SessionId::new("fallback");
        let token = attach(&store, &sid, &holder("w")).await;
        fake.set_down(true);
        store
            .flush(&batch(1, spread(&sid, NodeId::new())), Some(token))
            .await
            .unwrap();
        let got = store
            .vector_candidates_checked(&sid, &PROBE, &contract(), 3)
            .await
            .unwrap();
        let exact = primary
            .vector_candidates_checked(&sid, &PROBE, &contract(), 3)
            .await
            .unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(ids(&got), ids(&exact));
    }
}

/// The marker a mirror writes and the one a repair writes agree, so a reload
/// right after either is in sync.
#[tokio::test]
async fn a_reload_after_a_clean_mirror_is_in_sync() {
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let store = tier(&primary, &fake);
    let sid = SessionId::new("reload");
    let w = holder("w");
    let token = attach(&store, &sid, &w).await;
    let (_, b) = seed_batch(&sid, 9);
    store.flush(&b, Some(token)).await.unwrap();
    store.release_lease(&sid, &w).await.unwrap();
    let bulks = fake.bulk_calls.load(std::sync::atomic::Ordering::SeqCst);
    let again = tier(&primary, &fake);
    attach(&again, &sid, &w).await;
    assert_eq!(again.tier_status(&sid).sync, TierSync::InSync);
    assert_eq!(
        fake.bulk_calls.load(std::sync::atomic::Ordering::SeqCst),
        bulks,
        "an in-sync session is not re-indexed at load"
    );
}

// ---------------------------------------------------------------------------
// Hybrid derive on a holder over the tier (#18 review M6, amending #8)
// ---------------------------------------------------------------------------

/// Reduces hybrid's "concept - context" framing to the label, so the
/// hash-seeded fixture embedder scores the near paraphrases as near (the
/// same wrapper the SQLite holder tests use).
#[derive(Debug)]
struct LabelEmbedder(crate::embed::FixtureEmbedder);

#[async_trait]
impl crate::embed::Embedder for LabelEmbedder {
    fn dimensions(&self) -> usize {
        self.0.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.embed_as(text, crate::test_util::TextRole::Document)
            .await
    }
    // #22 design R6: a delegating embedder forwards every method, or it
    // silently inherits the defaults (a query embedded as a document, an
    // image refused).
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.embed_as(text, crate::test_util::TextRole::Query).await
    }
    fn modalities(&self) -> crate::embed::Modalities {
        self.0.modalities()
    }
    async fn embed_image(
        &self,
        image: crate::embed::ImageInput<'_>,
    ) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.0.embed_image(image).await
    }
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        self.0.as_any()
    }
}

impl LabelEmbedder {
    /// The label reduction, in either text role.
    async fn embed_as(
        &self,
        text: &str,
        role: crate::test_util::TextRole,
    ) -> Result<Vec<f32>, crate::embed::EmbedError> {
        let label = text
            .strip_prefix("Concept: ")
            .unwrap_or(text)
            .split(" — ")
            .next()
            .unwrap_or(text);
        role.embed(&self.0, label).await
    }
}

/// M6: a paraphrase derived seconds after the original merges into it on a
/// holder over the tier, although the index has not seen the original (not
/// flushed, and the index lags). Derive's semantic merge ranks in the
/// holder's graph and never asks the index; recall still does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_holder_derive_merges_a_paraphrase_the_index_has_not_seen() {
    use crate::embed::{Embedder, FixtureEmbedder, NEAR_A, NEAR_B};
    use crate::graph::derive::ParentOf;
    use crate::memory::Memory;
    use crate::types::{MatchStrategy, RecallQuery};
    let _quiet = crate::test_util::quiet_logs();
    let dim = FixtureEmbedder::new().dimensions();
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::lagging()));
    let contract = EmbeddingContract {
        kind: "fixture".into(),
        model: None,
        dim,
    };
    // A session that already has its contract durably, so recall has an
    // index to ask.
    let sid = SessionId::new("tier-derive");
    primary
        .flush(
            &batch(
                1,
                vec![
                    set_contract(&sid, Some(contract.clone())),
                    interaction(&sid, NodeId::new()),
                ],
            ),
            None,
        )
        .await
        .unwrap();
    let store: Arc<dyn GraphStore> = Arc::new(TieredStore::new(
        Box::new(Shared::new(primary.clone())),
        Box::new(fake.clone()),
        Some(dim),
    ));
    let mem = Memory::builder()
        .session("tier-derive")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .match_strategy(MatchStrategy::Hybrid)
        .store(store)
        .embedder(Arc::new(LabelEmbedder(FixtureEmbedder::new())) as Arc<dyn Embedder>)
        .embedding_contract(contract)
        .build()
        .await
        .expect("build");
    let first = mem
        .derive(&[(NEAR_A, ConceptType::Entity)], &ParentOf::none())
        .await
        .unwrap()
        .created[0];
    let second = mem
        .derive(&[(NEAR_B, ConceptType::Entity)], &ParentOf::none())
        .await
        .unwrap();
    assert_eq!(
        second.semantic_merged,
        vec![first],
        "the paraphrase became a near-duplicate: {second:?}"
    );
    let knn = || fake.knn_calls.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(knn(), 0, "derive asked the index");
    mem.recall_detailed(RecallQuery {
        query: NEAR_B.into(),
        top_k: 5,
        max_tokens: 500,
        traversal_depth: 1,
    })
    .await
    .unwrap();
    assert!(knn() > 0, "recall still reads the tier");
    mem.close().await.unwrap();
}

/// #22 PR 3 on a holder over the tier (M6): an image derive asks the index
/// nothing; a text derive whose nearest neighbour in the holder's graph is
/// the image (cosine 1 here) stays a separate concept instead of merging
/// into a picture; and the image's vector is mirrored into the index like
/// any concept's, so recall's tier serves it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_image_on_a_holder_over_the_tier_is_indexed_and_never_absorbs_text() {
    use crate::embed::{png_with_label, Embedder, FixtureEmbedder, NEAR_B};
    use crate::graph::derive::ParentOf;
    use crate::graph::image::{ImageDerive, ImagePayload};
    use crate::memory::Memory;
    use crate::types::MatchStrategy;
    let _quiet = crate::test_util::quiet_logs();
    let dim = FixtureEmbedder::new().dimensions();
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::lagging()));
    let contract = EmbeddingContract {
        kind: "fixture".into(),
        model: None,
        dim,
    };
    let sid = SessionId::new("tier-image");
    primary
        .flush(
            &batch(
                1,
                vec![
                    set_contract(&sid, Some(contract.clone())),
                    interaction(&sid, NodeId::new()),
                ],
            ),
            None,
        )
        .await
        .unwrap();
    let store: Arc<dyn GraphStore> = Arc::new(TieredStore::new(
        Box::new(Shared::new(primary.clone())),
        Box::new(fake.clone()),
        Some(dim),
    ));
    let mem = Memory::builder()
        .session("tier-image")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .match_strategy(MatchStrategy::Hybrid)
        .store(store)
        .embedder(Arc::new(LabelEmbedder(FixtureEmbedder::new())) as Arc<dyn Embedder>)
        .embedding_contract(contract)
        .build()
        .await
        .expect("build");
    let knn = || fake.knn_calls.load(std::sync::atomic::Ordering::SeqCst);

    // The image's vector is exactly the one a text "create account" gets.
    let png = png_with_label(NEAR_B);
    let image = mem
        .derive_image_as(
            &"agent-a".into(),
            ImageDerive {
                caption: "render 17",
                concept_type: ConceptType::Resource,
                image_id: Some("r17"),
                payload: ImagePayload::Bytes(
                    crate::surface::image::validate(&png, "image/png").unwrap(),
                ),
                parent_of: &[],
                event_time: None,
            },
        )
        .await
        .unwrap()
        .created[0];
    let text = mem
        .derive(&[(NEAR_B, ConceptType::Entity)], &ParentOf::none())
        .await
        .unwrap();
    assert!(
        text.semantic_merged.is_empty(),
        "a text claim never merges into a picture: {text:?}"
    );
    assert_eq!(text.created.len(), 1);
    assert_eq!(knn(), 0, "neither derive asked the index");

    mem.close().await.unwrap();
    let docs = fake.live(&sid);
    let (_, doc) = docs
        .get(&image.0.to_string())
        .expect("the image concept is mirrored into the index");
    assert_eq!(doc.content, "render 17 [image:r17]");
    assert_eq!(doc.embedding, FixtureEmbedder::new().embed_sync(NEAR_B));
}

/// #22 PR 6 on a holder over the tier (#18): a recall by image or by a
/// client vector reaches its vector leg through the index, like a text
/// recall, and the Dresscode path holds: the look dismissed for Onam is
/// the top vector-leg hit and the top hit. No store contract changed.
#[cfg(feature = "embed-fixture")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recall_by_image_or_vector_over_the_tier_reads_the_index() {
    use crate::embed::{Embedder, FixtureEmbedder};
    use crate::memory::Memory;
    use crate::recall::query_vector::QueryBy;
    use crate::test_util::dresscode::{
        assert_dismissed_is_the_top_vector_hit, client_query_vector, derive_wardrobe,
        imageless_text, similar_query_png,
    };
    use crate::types::MatchStrategy;
    let _quiet = crate::test_util::quiet_logs();
    let dim = FixtureEmbedder::new().dimensions();
    let (primary, fake) = (memory_primary(), Arc::new(FakeIndex::new()));
    let contract = EmbeddingContract {
        kind: "fixture".into(),
        model: None,
        dim,
    };
    let store: Arc<dyn GraphStore> = Arc::new(
        TieredStore::new(
            Box::new(Shared::new(primary.clone())),
            Box::new(fake.clone()),
            Some(dim),
        )
        .with_repair_backoff(Duration::ZERO),
    );
    let mem = Memory::builder()
        .session("tier-recall-by")
        .agent("agent-a")
        .flush_interval(Duration::from_millis(10))
        .match_strategy(MatchStrategy::Hybrid)
        .store(store)
        .embedder(Arc::new(LabelEmbedder(FixtureEmbedder::new())) as Arc<dyn Embedder>)
        .embedding_contract(contract.clone())
        .build()
        .await
        .expect("build");
    let wardrobe = derive_wardrobe(&mem).await;
    let sid = SessionId::new("tier-recall-by");
    wait_until("the looks are mirrored", || {
        let live = fake.live(&sid);
        [wardrobe.dismissed, wardrobe.kept, wardrobe.other]
            .iter()
            .all(|id| live.contains_key(&id.0.to_string()))
    })
    .await;
    mem.settle_daemon().await;

    let knn = || fake.knn_calls.load(std::sync::atomic::Ordering::SeqCst);
    let png = similar_query_png();
    for by in [
        QueryBy::Image(crate::surface::image::validate(&png, "image/png").unwrap()),
        QueryBy::Vector {
            values: client_query_vector(),
            declared: contract.clone(),
        },
    ] {
        let before = knn();
        let detailed = mem.recall_by_detailed(imageless_text(5), by).await.unwrap();
        assert!(knn() > before, "the vector leg read the index");
        assert_dismissed_is_the_top_vector_hit(&detailed, &wardrobe);
    }
    mem.close().await.unwrap();
}
