use super::*;
use crate::embed::{Embedder, FixtureEmbedder, NEAR_A, NEAR_B};
use crate::memory::Memory;
use crate::store::{Capabilities, GraphStore, SessionFlushStats};
use crate::store::{LeaseHolder, LeaseOutcome};
use crate::types::InteractionSpan;
use crate::types::{MatchStrategy, RecallQuery};
use std::sync::{Arc, Mutex};

/// [`SqliteStore`] with every checked vector answer recorded, in call order.
///
/// The twin of `memory.rs`'s `VectorSearchStore`, pointed at the real adapter:
/// it proves the vector leg **fired** on SQLite and what SQLite returned, rather
/// than inferring a vector hit from where a node landed in a rank. It adds no
/// behaviour — every method delegates.
struct RecordingSqlite {
    inner: SqliteStore,
    answers: Mutex<Vec<Vec<Scored<NodeId>>>>,
}

impl RecordingSqlite {
    fn new(inner: SqliteStore) -> Self {
        Self {
            inner,
            answers: Mutex::new(Vec::new()),
        }
    }

    fn answers(&self) -> Vec<Vec<Scored<NodeId>>> {
        self.answers.lock().unwrap().clone()
    }
}

#[async_trait]
impl GraphStore for RecordingSqlite {
    async fn init_schema(&self) -> Result<(), StoreError> {
        self.inner.init_schema().await
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    fn vector_dimensions(&self) -> Option<usize> {
        self.inner.vector_dimensions()
    }
    async fn flush(&self, batch: &MutationBatch, token: Option<u64>) -> Result<(), StoreError> {
        self.inner.flush(batch, token).await
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
    async fn vector_candidates_checked(
        &self,
        session: &SessionId,
        embedding: &[f32],
        expected_contract: &EmbeddingContract,
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        let hits = self
            .inner
            .vector_candidates_checked(session, embedding, expected_contract, limit)
            .await?;
        self.answers.lock().unwrap().push(hits.clone());
        Ok(hits)
    }
    async fn blast_radius(
        &self,
        session: &SessionId,
        node: NodeId,
        min_edge_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<u64, StoreError> {
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
    ) -> Result<InteractionSpan, StoreError> {
        self.inner
            .interaction_span(session, node, min_age, now)
            .await
    }
    async fn record_canonization(
        &self,
        event: &CanonizationEvent,
        token: Option<u64>,
    ) -> Result<(), StoreError> {
        self.inner.record_canonization(event, token).await
    }
    async fn acquire_lease(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
        ttl: Duration,
    ) -> Result<LeaseOutcome, StoreError> {
        self.inner.acquire_lease(session, holder, ttl).await
    }
    async fn refresh_lease(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
        ttl: Duration,
    ) -> Result<LeaseOutcome, StoreError> {
        self.inner.refresh_lease(session, holder, ttl).await
    }
    async fn release_lease(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
    ) -> Result<(), StoreError> {
        self.inner.release_lease(session, holder).await
    }
    async fn write_flush_stats(
        &self,
        session: &SessionId,
        stats: &SessionFlushStats,
    ) -> Result<(), StoreError> {
        self.inner.write_flush_stats(session, stats).await
    }
    async fn read_flush_stats(
        &self,
        session: &SessionId,
    ) -> Result<Option<SessionFlushStats>, StoreError> {
        self.inner.read_flush_stats(session).await
    }
}

/// Hybrid embeds a concept **with** its origin context (`"register user — <prompt>"`)
/// while recall embeds the bare query (`"create account"`). A real semantic
/// embedder still scores those two as near; `FixtureEmbedder` is hash-seeded per
/// exact phrase and cannot. This reduces the framing back to the concept label
/// before delegating — the same wrapper `memory.rs` uses for the MemoryStore
/// version of this test, and nothing else about the embedding path changes.
#[derive(Debug)]
struct ContextTolerantEmbedder(FixtureEmbedder);

#[async_trait]
impl Embedder for ContextTolerantEmbedder {
    fn dimensions(&self) -> usize {
        self.0.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        let label = text
            .strip_prefix("Concept: ")
            .unwrap_or(text)
            .split(" — ")
            .next()
            .unwrap_or(text);
        self.0.embed(label).await
    }
}

/// **F, end to end.** A concept created by the ordinary derive surface on SQLite
/// persists its vector, the vector survives the write-behind flush and a session
/// reload, and recall's vector leg finds it from a query that shares no token
/// with it. Before F this was impossible on the default local store: hybrid
/// derive logged `hybrid matching disabled: store lacks VECTOR_SEARCH` and
/// recall was keyword/recency only, which made semantic recall a property of the
/// cloud tier.
///
/// The vector leg is proven to have **fired** by `RecordingSqlite::answers`, not
/// by the recalled node's rank.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_vector_leg_fires_on_an_organically_derived_concept() {
    let (logs, _guard) = crate::test_util::capture_logs(tracing::Level::WARN);
    let session = SessionId::from("sqlite-organic-vectors");
    let (_dir, path) = scratch_db();
    let store = Arc::new(RecordingSqlite::new(SqliteStore::connect(&path).unwrap()));
    store.init_schema().await.unwrap();

    let contract = EmbeddingContract {
        kind: "fixture".into(),
        model: None,
        dim: FixtureEmbedder::new().dimensions(),
    };
    let session_name = session.0.clone();
    let open = |store: Arc<RecordingSqlite>, contract: EmbeddingContract| {
        let session_name = session_name.clone();
        async move {
            Memory::builder()
                .session(session_name)
                .agent("agent-a")
                .flush_interval(Duration::from_secs(3_600))
                .match_strategy(MatchStrategy::Hybrid)
                .store(store as Arc<dyn GraphStore>)
                .embedder(
                    Arc::new(ContextTolerantEmbedder(FixtureEmbedder::new())) as Arc<dyn Embedder>
                )
                .embedding_contract(contract)
                .build()
                .await
                .expect("build")
        }
    };

    let mem = open(store.clone(), contract.clone()).await;
    let out = mem
        .derive(
            &[(NEAR_A, ConceptType::Entity)],
            &crate::graph::derive::ParentOf::none(),
        )
        .await
        .unwrap();
    assert_eq!(out.created.len(), 1);
    let organic = out.created[0];
    // close() drains the tail: the vector must be durable, not RAM-only.
    mem.close().await.unwrap();

    let snapshot = store.load_session(&session).await.unwrap();
    let stored = snapshot
        .concepts
        .iter()
        .find(|c| c.id == organic)
        .expect("the derived concept is durable");
    assert_eq!(
        stored.embedding.as_ref().map(Vec::len),
        Some(contract.dim),
        "an organically-derived concept persists its vector through SQLite"
    );
    assert_eq!(
        snapshot.embedding.as_ref(),
        Some(&contract),
        "and the contract that makes it interpretable is durable with it"
    );
    assert_eq!(
        store.answers(),
        vec![Vec::new()],
        "the derive's own hybrid gather queried SQLite and found an empty pool"
    );

    // Reopen — proving the vector round-trips through load_session — and recall
    // with text sharing NO token with the stored concept but near it in the
    // embedding space. The keyword leg cannot score it; only the vector leg can.
    let reopened = open(store.clone(), contract.clone()).await;
    let result = reopened
        .recall(RecallQuery {
            query: NEAR_B.into(),
            top_k: 5,
            max_tokens: 500,
            traversal_depth: 1,
        })
        .await
        .unwrap();

    let answers = store.answers();
    assert_eq!(answers.len(), 2, "recall issued exactly one vector query");
    let scored = answers[1]
        .iter()
        .find(|s| s.item == organic)
        .expect("SQLite's vector leg returned the organically-derived concept");
    assert!(
        scored.score >= 0.85,
        "scored by real cosine similarity, got {}",
        scored.score
    );
    assert!(
        result.hits.iter().any(|h| h.node_id == organic),
        "the vector-leg candidate reaches the assembled result: {result:?}"
    );
    assert!(
        !logs.contains("store lacks VECTOR_SEARCH"),
        "SQLite must no longer log the hybrid degradation warning: {}",
        logs.contents()
    );
    assert!(
        !logs.contains("capability miss"),
        "nor the capability-refusal degradation: {}",
        logs.contents()
    );

    reopened.close().await.unwrap();
    drop(store);
}

/// The same store, the same session, a renamed embedder: the checked read
/// refuses rather than ranking vectors from another space. On SQLite this path
/// was previously unreachable (no capability meant hybrid never called it).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mid_session_model_swap_is_refused_on_the_recall_path() {
    let _quiet = crate::test_util::quiet_logs();
    let session = SessionId::from("sqlite-contract-swap");
    let (_dir, path) = scratch_db();
    let store = Arc::new(RecordingSqlite::new(SqliteStore::connect(&path).unwrap()));
    store.init_schema().await.unwrap();
    let dim = FixtureEmbedder::new().dimensions();

    let mem = Memory::builder()
        .session(session.0.clone())
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .match_strategy(MatchStrategy::Hybrid)
        .store(store.clone() as Arc<dyn GraphStore>)
        .embedder(Arc::new(ContextTolerantEmbedder(FixtureEmbedder::new())) as Arc<dyn Embedder>)
        .embedding_contract(EmbeddingContract {
            kind: "fixture".into(),
            model: Some("model-v1".into()),
            dim,
        })
        .build()
        .await
        .expect("build");
    mem.derive(
        &[(NEAR_A, ConceptType::Entity)],
        &crate::graph::derive::ParentOf::none(),
    )
    .await
    .unwrap();
    mem.close().await.unwrap();

    let renamed = EmbeddingContract {
        kind: "fixture".into(),
        model: Some("model-v2".into()),
        dim,
    };
    // A probe with a direction. This test is about the contract, and
    // since B-E2E-R2-3 a zero-norm probe is refused before the
    // contract is read (as it always was on the pg family).
    let mut probe = vec![0.0f32; dim];
    probe[0] = 1.0;
    let err = store
        .vector_candidates_checked(&session, &probe, &renamed, 5)
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Invariant(_)), "{err:?}");
    assert!(
        err.to_string().contains("embedding contract changed"),
        "{err}"
    );

    drop(store);
}
