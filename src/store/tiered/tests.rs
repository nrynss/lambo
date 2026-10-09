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
/// durable store and one index.
struct Shared(Arc<dyn GraphStore>);

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
        Box::new(Shared(primary.clone())),
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
        Box::new(Shared(primary.clone())),
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
    assert_eq!(
        store.tier_status(&sid).mirror_failures,
        after_first + 1,
        "one repair attempt inside the backoff window, then none"
    );
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
    assert_eq!(next.tier_status(&sid).sync, TierSync::InSync);
    assert!(fake.live(&sid).contains_key(&c4.0.to_string()));
    assert_eq!(fake.marker(&sid), Some(2));
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
        Box::new(CountingLoads(Shared(primary.clone()), loads.clone())),
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
    // Unleased writes (no lease row): the seed / fixture path.
    let _ = store.load_session(&sid).await;
    let (s, b) = seed_batch(&sid, 1);
    store.flush(&b, None).await.unwrap();
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
        Box::new(Shared(primary.clone())),
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
        Box::new(Shared(primary.clone())),
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
            Box::new(Shared(primary.clone())),
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
