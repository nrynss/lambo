use super::*;
use crate::embed::{Embedder, FixtureEmbedder, FAR, NEAR_A, NEAR_B};
use crate::memory::Memory;
use crate::store::{Capabilities, GraphStore, SessionFlushStats};
use crate::store::{LeaseHolder, LeaseOutcome};
use crate::types::InteractionSpan;
use crate::types::{MatchStrategy, RecallQuery};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// [`SqliteStore`] with every checked vector answer recorded, in call order.
///
/// The twin of `memory.rs`'s `VectorSearchStore`, pointed at the real adapter:
/// it proves the vector leg **fired** on SQLite and what SQLite returned, rather
/// than inferring a vector hit from where a node landed in a rank. It adds no
/// behaviour — every method delegates.
///
/// Built with [`RecordingSqlite::new`] it does **not** forward
/// `exact_vector_scan` (the trait default answers `false`), so a holder over
/// it keeps reading vectors from SQLite: the store path these tests were
/// written for, and the path readers without a graph still take. Built with
/// [`RecordingSqlite::graph_ranked`] it forwards SQLite's `true`, so a holder
/// ranks in its graph exactly as one over a bare `SqliteStore` does (#8), and
/// `vector_calls` then counts the store reads that path must not make.
///
/// `answers` holds only the checked reads that **succeeded**; `vector_calls`
/// counts every entry into either vector read (the checked one and the frozen
/// unchecked `vector_candidates`) before the inner call runs, so a call that
/// errors — say a contract refusal that recall swallows into a keyword-only
/// result — is still counted.
struct RecordingSqlite {
    inner: SqliteStore,
    answers: Mutex<Vec<Vec<Scored<NodeId>>>>,
    vector_calls: AtomicUsize,
    forward_exact_scan: bool,
}

impl RecordingSqlite {
    fn new(inner: SqliteStore) -> Self {
        Self {
            inner,
            answers: Mutex::new(Vec::new()),
            vector_calls: AtomicUsize::new(0),
            forward_exact_scan: false,
        }
    }

    fn graph_ranked(inner: SqliteStore) -> Self {
        Self {
            forward_exact_scan: true,
            ..Self::new(inner)
        }
    }

    fn answers(&self) -> Vec<Vec<Scored<NodeId>>> {
        self.answers.lock().unwrap().clone()
    }

    /// Every vector read that reached the store, successful or not.
    fn vector_calls(&self) -> usize {
        self.vector_calls.load(Ordering::SeqCst)
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
    fn exact_vector_scan(&self) -> bool {
        self.forward_exact_scan && self.inner.exact_vector_scan()
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
        self.vector_calls.fetch_add(1, Ordering::SeqCst);
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
        self.vector_calls.fetch_add(1, Ordering::SeqCst);
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
    assert_eq!(
        store.vector_calls(),
        2,
        "and no other vector read reached SQLite, failed or unchecked"
    );
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

// -----------------------------------------------------------------------
// #8: the holder ranks the vectors its graph already holds
// -----------------------------------------------------------------------

fn fixture_contract() -> EmbeddingContract {
    EmbeddingContract {
        kind: "fixture".into(),
        model: None,
        dim: FixtureEmbedder::new().dimensions(),
    }
}

async fn open_holder(store: Arc<RecordingSqlite>, session: &str) -> Memory {
    Memory::builder()
        .session(session)
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .match_strategy(MatchStrategy::Hybrid)
        .store(store as Arc<dyn GraphStore>)
        .embedder(Arc::new(ContextTolerantEmbedder(FixtureEmbedder::new())) as Arc<dyn Embedder>)
        .embedding_contract(fixture_contract())
        .build()
        .await
        .expect("build")
}

/// The vector leg's own score for `node`, read from recall's leg provenance
/// rather than inferred from rank (the recent leg would surface a concept
/// from the latest interaction either way).
async fn vector_leg_score(mem: &Memory, query: &str, node: NodeId) -> Option<f64> {
    let detailed = mem
        .recall_detailed(RecallQuery {
            query: query.into(),
            top_k: 5,
            max_tokens: 500,
            traversal_depth: 1,
        })
        .await
        .unwrap();
    detailed.legs.get(&node).and_then(|legs| legs.vector)
}

/// **#8 acceptance 1.** On the holder, recall's vector leg and hybrid
/// derive's semantic match (synchronous and through the write queue) make
/// zero store calls for vectors, and the leg still fires: it ranks the
/// vectors the graph loaded at attach.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_holder_reads_no_vectors_from_the_store() {
    let _quiet = crate::test_util::quiet_logs();
    let session = "sqlite-graph-ranked";
    let (_dir, path) = scratch_db();
    let store = Arc::new(RecordingSqlite::graph_ranked(
        SqliteStore::connect(&path).unwrap(),
    ));
    store.init_schema().await.unwrap();

    let mem = open_holder(store.clone(), session).await;
    let organic = mem
        .derive(
            &[(NEAR_A, ConceptType::Entity)],
            &crate::graph::derive::ParentOf::none(),
        )
        .await
        .unwrap()
        .created[0];
    mem.close().await.unwrap();

    // Reopen: the vector now comes from session load, not from this
    // process's derive, and recall must still find it without the store.
    let reopened = open_holder(store.clone(), session).await;
    let score = vector_leg_score(&reopened, NEAR_B, organic)
        .await
        .expect("the vector leg found the concept from the graph");
    assert!(score >= 0.85, "real cosine, got {score}");

    // The background write path (WriteCtx) chooses its source with the same
    // constructor; drive it once and wait for the apply.
    let agent = AgentId::from("agent-a");
    let submitted = reopened
        .derive_async_as(
            &agent,
            &[(FAR, ConceptType::Entity)],
            &crate::graph::derive::ParentOf::none(),
            None,
        )
        .await
        .unwrap();
    let answer = reopened
        .pipeline()
        .wait(&agent, submitted.receipt, Duration::from_secs(10))
        .await;
    assert_eq!(answer.tag(), "applied", "{answer:?}");

    assert_eq!(
        store.vector_calls(),
        0,
        "derive, recall and the queued derive made no vector call to SQLite, \
         successful or failed, checked or unchecked: answers {:?}",
        store.answers()
    );
    reopened.close().await.unwrap();
}

/// **#8 acceptance 2, recall.** A concept derived and not yet flushed is
/// returned by the vector leg. The same sequence over the store path (a
/// holder that does not rank in its graph) misses it, which is the gap #8
/// closes: the store holds only flushed vectors.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_holder_vector_leg_sees_an_unflushed_concept() {
    let _quiet = crate::test_util::quiet_logs();
    for graph_ranked in [true, false] {
        let session = format!("sqlite-fresh-{graph_ranked}");
        let (_dir, path) = scratch_db();
        let inner = SqliteStore::connect(&path).unwrap();
        let store = Arc::new(if graph_ranked {
            RecordingSqlite::graph_ranked(inner)
        } else {
            RecordingSqlite::new(inner)
        });
        store.init_schema().await.unwrap();
        let mem = open_holder(store.clone(), &session).await;
        let organic = mem
            .derive(
                &[(NEAR_A, ConceptType::Entity)],
                &crate::graph::derive::ParentOf::none(),
            )
            .await
            .unwrap()
            .created[0];
        // Not flushed: the hour-long interval has not elapsed.
        let durable = store
            .load_session(&SessionId::from(session.as_str()))
            .await
            .map(|snap| snap.concepts.iter().any(|c| c.id == organic))
            .unwrap_or(false);
        assert!(!durable, "the concept must still be RAM-only here");

        let score = vector_leg_score(&mem, NEAR_B, organic).await;
        if graph_ranked {
            assert!(
                score.is_some_and(|s| s >= 0.85),
                "the graph-ranked leg returns the unflushed concept: {score:?}"
            );
        } else {
            assert_eq!(score, None, "the store path cannot see it before a flush");
        }
        mem.close().await.unwrap();
    }
}

/// **#8 acceptance 2, derive.** Hybrid derive's semantic match sees a concept
/// an earlier derive created and has not flushed: the near paraphrase merges
/// into it. Over the store path the earlier concept is invisible until the
/// flush, so the paraphrase lands as an unrelated fresh concept.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_holder_semantic_match_sees_an_unflushed_concept() {
    let _quiet = crate::test_util::quiet_logs();
    for graph_ranked in [true, false] {
        let session = format!("sqlite-fresh-merge-{graph_ranked}");
        let (_dir, path) = scratch_db();
        let inner = SqliteStore::connect(&path).unwrap();
        let store = Arc::new(if graph_ranked {
            RecordingSqlite::graph_ranked(inner)
        } else {
            RecordingSqlite::new(inner)
        });
        store.init_schema().await.unwrap();
        let mem = open_holder(store.clone(), &session).await;
        let first = mem
            .derive(
                &[(NEAR_A, ConceptType::Entity)],
                &crate::graph::derive::ParentOf::none(),
            )
            .await
            .unwrap()
            .created[0];
        let second = mem
            .derive(
                &[(NEAR_B, ConceptType::Entity)],
                &crate::graph::derive::ParentOf::none(),
            )
            .await
            .unwrap();
        if graph_ranked {
            assert_eq!(
                second.semantic_merged,
                vec![first],
                "the paraphrase merges into the unflushed concept"
            );
        } else {
            assert!(
                second.semantic_merged.is_empty(),
                "the store path cannot see the unflushed concept: {second:?}"
            );
        }
        mem.close().await.unwrap();
    }
}

/// **#8 parity on a live holder.** The parity module compares SQLite with a
/// graph loaded *from* SQLite, whose vectors have already been through the
/// BLOB decode. A holder ranks a different graph: one whose vectors came
/// straight from the embedder and whose keys came from the live derive,
/// never round-tripped. This snapshots that live graph before any reload,
/// lets `close()` flush it, and asks SQLite the same questions: the answers
/// must match id for id and score bit for bit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vector_graph_parity_on_a_live_holder_graph() {
    let _quiet = crate::test_util::quiet_logs();
    let session = "sqlite-live-parity";
    let sid = SessionId::from(session);
    let (_dir, path) = scratch_db();
    let store = Arc::new(RecordingSqlite::graph_ranked(
        SqliteStore::connect(&path).unwrap(),
    ));
    store.init_schema().await.unwrap();
    let mem = open_holder(store.clone(), session).await;

    let phrases = [
        NEAR_A,
        FAR,
        "billing invoice export",
        "rate limiter token bucket",
        "postgres connection pool",
        "retry with exponential backoff",
        "user profile avatar upload",
    ];
    for phrase in phrases {
        mem.derive(
            &[(phrase, ConceptType::Entity)],
            &crate::graph::derive::ParentOf::none(),
        )
        .await
        .unwrap();
    }
    let contract = fixture_contract();
    // The live graph, before any flush or reload.
    let live = mem.graph().read().clone();
    let embedded = live.concepts().filter(|c| c.embedding.is_some()).count();
    assert!(
        embedded >= phrases.len(),
        "every derived concept carries a vector"
    );

    // close() drains the write-behind log; nothing reloads `live`.
    mem.close().await.unwrap();

    let fixture = FixtureEmbedder::new();
    let mut probes = Vec::new();
    for text in [NEAR_B, FAR, "invoice", "connection backoff", "avatar"] {
        probes.push(fixture.embed(text).await.unwrap());
    }
    let bits = |hits: &[Scored<NodeId>]| -> Vec<(NodeId, u64)> {
        hits.iter().map(|s| (s.item, s.score.to_bits())).collect()
    };
    for (p, probe) in probes.iter().enumerate() {
        for limit in [1, 3, embedded, crate::store::MAX_VECTOR_CANDIDATE_LIMIT] {
            let from_graph = crate::graph::vector_source::graph_vector_candidates(
                &live, &sid, probe, &contract, limit,
            )
            .unwrap();
            let from_store = store
                .inner
                .vector_candidates_checked(&sid, probe, &contract, limit)
                .await
                .unwrap();
            assert_eq!(
                from_graph.len(),
                limit.min(embedded),
                "probe {p} limit {limit}"
            );
            assert_eq!(
                bits(&from_graph),
                bits(&from_store),
                "probe {p} limit {limit}: the live graph ranks bit-identically to the flushed store"
            );
        }
    }
}
