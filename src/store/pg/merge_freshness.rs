//! #60: on a session holder over the Postgres family, hybrid derive's
//! semantic merge ranks in the holder's in-memory graph, so a paraphrase
//! derived seconds after the original merges into it before the write-behind
//! flush has put the original in the database. Recall keeps the database's
//! vector search.
//!
//! The mechanism is #18's purpose split, unchanged: `PgStore` declares
//! [`GraphStore::holder_derives_from_graph`], which only
//! `VectorCandidates::for_holder_derive` reads; recall's
//! `VectorCandidates::for_holder` reads `exact_vector_scan`, which the family
//! leaves `false`.
//!
//! Offline, a [`LaggingDatabase`] models the one property that matters: the
//! database's vector search sees only flushed rows. It takes its two source
//! declarations from a real, never-connected `PgStore<D>`, so these tests
//! exercise the family's own answer, not the double's. Live, the Postgres leg
//! is the `#[ignore]`d test at the bottom (the `postgres-live` CI job runs it
//! by name) and the Cockroach leg runs inside
//! `cockroach::conformance::conformance_suite`.
//!
//! # What the graph path changes at the threshold edge
//!
//! The database scores by its own distance (Postgres `1 - d` over pgvector's
//! cosine distance, Cockroach `1 - d²/2` over L2, possibly from the partial
//! ANN index, which can also miss a near neighbour). The graph scores exact
//! `f32` cosine (`rank_by_cosine`). So a pair whose similarity sits on the
//! threshold can merge on one path and not the other, and a target the ANN
//! beam missed is now found. No bit parity with the database is claimed for
//! the merge leg; [`a_merge_at_the_threshold_edge_follows_exact_cosine`] pins
//! what the graph path does decide.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::{Dialect, PgStore};
use crate::embed::{EmbedError, Embedder};
use crate::graph::derive::ParentOf;
use crate::memory::Memory;
use crate::store::{Capabilities, GraphStore, StoreConfig, StoreKind};
use crate::types::{
    CanonizationEvent, ConceptType, EmbeddingContract, GraphSnapshot, InteractionSpan,
    MatchStrategy, Mutation, MutationBatch, Node, NodeId, Scored, SessionId, StoreError,
};

/// The concept the paraphrase must merge into.
pub(crate) const ORIGINAL: &str = "register user";
/// A paraphrase of [`ORIGINAL`]: cosine about 0.989 under [`LabelVectors`].
pub(crate) const PARAPHRASE: &str = "create account";

/// A deterministic embedder that needs no feature: the two labels above are
/// near, every other text lands on its own axis. Hybrid embeds a concept with
/// its origin context (`"Concept: <label> — <prompt>"`); the label is what is
/// embedded, as the fixture-backed holder tests do.
#[derive(Debug)]
pub(crate) struct LabelVectors {
    dim: usize,
}

impl LabelVectors {
    pub(crate) fn new(dim: usize) -> Self {
        assert!(dim >= 4, "the paraphrase pair needs two axes plus others");
        Self { dim }
    }

    pub(crate) fn contract(&self) -> EmbeddingContract {
        EmbeddingContract {
            kind: "pg-merge-freshness".into(),
            model: None,
            dim: self.dim,
        }
    }

    /// The vector for `text`, unit norm.
    pub(crate) fn vector(&self, text: &str) -> Vec<f32> {
        let label = text
            .strip_prefix("Concept: ")
            .unwrap_or(text)
            .split(" — ")
            .next()
            .unwrap_or(text)
            .trim()
            .to_lowercase();
        let mut v = vec![0.0f32; self.dim];
        match label.as_str() {
            ORIGINAL => v[0] = 1.0,
            PARAPHRASE => {
                let n = (1.0f32 + 0.15 * 0.15).sqrt();
                v[0] = 1.0 / n;
                v[1] = 0.15 / n;
            }
            other => {
                // FNV-1a: stable across Rust releases, unlike DefaultHasher.
                let h = other.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
                    (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
                });
                let axis = 2 + usize::try_from(h % (self.dim as u64 - 2)).expect("fits");
                v[axis] = 1.0;
            }
        }
        v
    }
}

#[async_trait]
impl Embedder for LabelVectors {
    fn dimensions(&self) -> usize {
        self.dim
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        Ok(self.vector(text))
    }
}

/// A hybrid holder over `store`, with an hour-long flush interval so nothing
/// reaches the database until `close`.
pub(crate) async fn open_holder(
    store: Arc<dyn GraphStore>,
    session: &str,
    embedder: Arc<LabelVectors>,
    threshold: Option<f64>,
) -> Memory {
    let contract = embedder.contract();
    let mut builder = Memory::builder();
    if let Some(threshold) = threshold {
        builder = builder.config(crate::Config {
            semantic_match_threshold: threshold,
            ..crate::Config::default()
        });
    }
    builder
        .session(session)
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .match_strategy(MatchStrategy::Hybrid)
        .store(store)
        .embedder(embedder as Arc<dyn Embedder>)
        .embedding_contract(contract)
        .build()
        .await
        .expect("build")
}

/// The live check both dialects run: on a holder over a real pg-family
/// store, a paraphrase derived right after the original merges into it while
/// the original is still unflushed; the flush makes that merge durable; and
/// the database's own vector search (recall's source) then finds the
/// original. Erases the session at the end.
///
/// Compiled where a caller is: the Postgres live test, or the Cockroach
/// conformance suite (which needs `fixtures`).
#[cfg(any(
    feature = "store-postgres",
    all(feature = "store-cockroach", feature = "fixtures")
))]
pub(crate) async fn check_holder_merges_an_unflushed_paraphrase<D: Dialect>(
    store: Arc<PgStore<D>>,
    session: &str,
) {
    let dim = store.vector_dimensions().expect("pg stores carry vectors");
    let embedder = Arc::new(LabelVectors::new(dim));
    let contract = embedder.contract();
    let sid = SessionId::from(session);
    let mem = open_holder(store.clone(), session, embedder.clone(), None).await;

    let first = mem
        .derive(&[(ORIGINAL, ConceptType::Entity)], &ParentOf::none())
        .await
        .expect("derive the original")
        .created[0];
    let durable = store
        .load_session(&sid)
        .await
        .map(|snap| snap.concepts.iter().any(|c| c.id == first))
        .unwrap_or(false);
    assert!(!durable, "the original must still be unflushed here");

    let second = mem
        .derive(&[(PARAPHRASE, ConceptType::Entity)], &ParentOf::none())
        .await
        .expect("derive the paraphrase");
    assert_eq!(
        second.semantic_merged,
        vec![first],
        "the paraphrase became a near-duplicate of an unflushed concept: {second:?}"
    );
    let paraphrase = second.created[0];
    mem.close().await.expect("close flushes");

    // The merge is durable: the flushed graph links the two by the
    // `Semantic` edge hybrid records for a merge.
    let snap = store
        .load_session(&sid)
        .await
        .expect("load after the flush");
    assert!(
        snap.edges.iter().any(|e| {
            e.edge_type == crate::types::EdgeType::Semantic
                && [e.source, e.target].contains(&first)
                && [e.source, e.target].contains(&paraphrase)
        }),
        "no Semantic merge edge was flushed: {:?}",
        snap.edges
    );

    // Recall's source, unchanged: the database's own search, after the flush.
    let hits = store
        .vector_candidates_checked(&sid, &embedder.vector(ORIGINAL), &contract, 5)
        .await
        .expect("the database answers once the session is flushed");
    assert_eq!(
        hits.first().map(|s| s.item),
        Some(first),
        "the database ranks the original first for its own vector: {hits:?}"
    );

    store
        .erase_session(
            &sid,
            &crate::store::lease::LeaseHolder {
                endpoint: None,
                agent: crate::types::AgentId::new("pg-merge-freshness"),
                pid: 1,
                host: "test".into(),
            },
        )
        .await
        .expect("erase the test session");
}

// ---------------------------------------------------------------------------
// Offline: the family's declarations over a database that lags the holder
// ---------------------------------------------------------------------------

/// A never-connected `PgStore<D>`: construction parses the DSN and builds no
/// pool, so its declarations can be read with no database.
fn unconnected<D: Dialect>(kind: StoreKind) -> PgStore<D> {
    PgStore::<D>::new(StoreConfig {
        kind,
        dsn: Some("postgresql://u@127.0.0.1:1/lambo?sslmode=disable".into()),
        path: None,
        vector_dim: None,
    })
    .expect("construction is I/O-free")
}

/// Width of the offline tests' vectors.
const DIM: usize = 8;

/// A durable store whose vector search sees only what was flushed: the
/// write-behind lag of the real family, with no database. Every vector read
/// (checked or unchecked) is counted on entry, before it answers.
///
/// `capabilities`, `exact_vector_scan` and `holder_derives_from_graph` come
/// from a real `PgStore<D>`; everything else is the double's.
struct LaggingDatabase<D: Dialect> {
    family: PgStore<D>,
    /// Flushed concepts with a vector, by id.
    flushed: parking_lot::Mutex<HashMap<NodeId, (Vec<f32>, String)>>,
    contract: parking_lot::Mutex<Option<EmbeddingContract>>,
    /// Flushed edges, so a test can see a merge became durable.
    edges: parking_lot::Mutex<Vec<crate::types::Edge>>,
    vector_calls: AtomicUsize,
}

impl<D: Dialect> LaggingDatabase<D> {
    fn new(kind: StoreKind) -> Self {
        Self {
            family: unconnected::<D>(kind),
            flushed: parking_lot::Mutex::default(),
            contract: parking_lot::Mutex::default(),
            edges: parking_lot::Mutex::default(),
            vector_calls: AtomicUsize::new(0),
        }
    }

    fn vector_calls(&self) -> usize {
        self.vector_calls.load(Ordering::SeqCst)
    }

    /// What the database's search answers: exact cosine over the flushed
    /// rows only.
    fn search(&self, probe: &[f32], limit: usize) -> Vec<Scored<NodeId>> {
        let rows = self.flushed.lock().clone();
        crate::store::vector_source::rank_by_cosine(
            probe,
            rows.iter()
                .map(|(id, (v, key))| (*id, v.as_slice(), key.as_str())),
            limit,
        )
    }
}

#[async_trait]
impl<D: Dialect> GraphStore for LaggingDatabase<D> {
    async fn init_schema(&self) -> Result<(), StoreError> {
        Ok(())
    }
    fn capabilities(&self) -> Capabilities {
        self.family.capabilities()
    }
    fn vector_dimensions(&self) -> Option<usize> {
        Some(DIM)
    }
    fn exact_vector_scan(&self) -> bool {
        self.family.exact_vector_scan()
    }
    fn holder_derives_from_graph(&self) -> bool {
        self.family.holder_derives_from_graph()
    }
    async fn flush(&self, batch: &MutationBatch, _token: Option<u64>) -> Result<(), StoreError> {
        let mut flushed = self.flushed.lock();
        for m in &batch.mutations {
            match m {
                Mutation::UpsertNode {
                    node: Node::Concept(c),
                } => {
                    if let Some(v) = &c.embedding {
                        flushed.insert(c.id, (v.clone(), c.canonical_key.clone()));
                    }
                }
                Mutation::UpsertEdge { edge } => self.edges.lock().push(edge.clone()),
                Mutation::DeleteNode { id } => {
                    flushed.remove(id);
                }
                Mutation::SetEmbedding { embedding, .. } => {
                    *self.contract.lock() = embedding.clone();
                }
                _ => {}
            }
        }
        Ok(())
    }
    async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
        // Every holder here starts a fresh session.
        Err(StoreError::SessionNotFound(session.0.clone()))
    }
    async fn keyword_candidates(
        &self,
        _: &SessionId,
        _: &[String],
        _: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        Ok(Vec::new())
    }
    async fn vector_candidates(
        &self,
        _: &SessionId,
        embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.vector_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.search(embedding, limit))
    }
    async fn vector_candidates_checked(
        &self,
        _: &SessionId,
        embedding: &[f32],
        expected: &EmbeddingContract,
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.vector_calls.fetch_add(1, Ordering::SeqCst);
        // Like the family: no durable contract yet means no vectors to rank.
        match self.contract.lock().as_ref() {
            None => return Ok(Vec::new()),
            Some(stored) => stored
                .ensure_compatible(expected)
                .map_err(|e| StoreError::Invariant(e.to_string()))?,
        }
        Ok(self.search(embedding, limit))
    }
    async fn blast_radius(
        &self,
        _: &SessionId,
        _: NodeId,
        _: Duration,
        _: DateTime<Utc>,
    ) -> Result<u64, StoreError> {
        Ok(0)
    }
    async fn interaction_span(
        &self,
        _: &SessionId,
        _: NodeId,
        _: Duration,
        _: DateTime<Utc>,
    ) -> Result<InteractionSpan, StoreError> {
        Ok(InteractionSpan {
            distinct: 0,
            coverage: 0.0,
        })
    }
    async fn record_canonization(
        &self,
        _: &CanonizationEvent,
        _: Option<u64>,
    ) -> Result<(), StoreError> {
        Ok(())
    }
}

/// Runs `check` once per compiled pg-family dialect.
macro_rules! for_each_dialect {
    ($check:ident) => {{
        #[cfg(feature = "store-postgres")]
        $check::<super::postgres::PostgresDialect>(StoreKind::Postgres, "postgres").await;
        #[cfg(feature = "store-cockroach")]
        $check::<super::cockroach::CockroachDialect>(StoreKind::Cockroach, "cockroach").await;
    }};
}

/// The family's declarations: recall stays on the database
/// (`exact_vector_scan` false, so `for_holder` keeps the store) and derive
/// ranks in the holder's graph (`for_holder_derive`).
#[tokio::test]
async fn the_family_splits_merge_from_recall() {
    async fn check<D: Dialect>(kind: StoreKind, name: &str) {
        use crate::store::vector_source::VectorCandidates;
        let store = unconnected::<D>(kind);
        let graph = parking_lot::RwLock::new(crate::graph::Graph::new(SessionId::from("s")));
        assert!(store.capabilities().contains(Capabilities::VECTOR_SEARCH));
        assert!(
            !store.exact_vector_scan(),
            "{name}: recall must stay in the database"
        );
        assert!(store.holder_derives_from_graph(), "{name}");
        assert!(
            matches!(
                VectorCandidates::for_holder(&store, &graph),
                VectorCandidates::Store(_)
            ),
            "{name}: recall's source"
        );
        assert!(
            matches!(
                VectorCandidates::for_holder_derive(&store, &graph),
                VectorCandidates::Graph(_)
            ),
            "{name}: derive's source"
        );
    }
    for_each_dialect!(check);
}

/// **#60 acceptance 1 and 3.** A paraphrase derived right after the original
/// merges into it although the database has not seen the original, and
/// derive (synchronous and through the write queue) makes no vector call to
/// the store. Recall still asks the database.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_holder_merges_a_paraphrase_the_database_has_not_seen() {
    async fn check<D: Dialect>(kind: StoreKind, name: &str) {
        let store = Arc::new(LaggingDatabase::<D>::new(kind));
        let embedder = Arc::new(LabelVectors::new(DIM));
        let session = format!("pg-fresh-merge-{name}");
        let mem = open_holder(store.clone(), &session, embedder, None).await;

        let first = mem
            .derive(&[(ORIGINAL, ConceptType::Entity)], &ParentOf::none())
            .await
            .unwrap()
            .created[0];
        assert!(
            store.flushed.lock().is_empty(),
            "{name}: the original must still be unflushed"
        );
        let second = mem
            .derive(&[(PARAPHRASE, ConceptType::Entity)], &ParentOf::none())
            .await
            .unwrap();
        assert_eq!(
            second.semantic_merged,
            vec![first],
            "{name}: the paraphrase became a near-duplicate: {second:?}"
        );

        // The background write path chooses its source with the same
        // constructor; drive it once and wait for the apply.
        let agent = crate::types::AgentId::from("agent-a");
        let submitted = mem
            .derive_async_as(
                &agent,
                &[("an unrelated fact", ConceptType::Entity)],
                &ParentOf::none(),
                None,
            )
            .await
            .unwrap();
        let answer = mem
            .pipeline()
            .wait(&agent, submitted.receipt, Duration::from_secs(10))
            .await;
        assert_eq!(answer.tag(), "applied", "{name}: {answer:?}");

        assert_eq!(
            store.vector_calls(),
            0,
            "{name}: derive made a vector call to the database"
        );

        mem.recall_detailed(crate::types::RecallQuery {
            query: PARAPHRASE.into(),
            top_k: 5,
            max_tokens: 500,
            traversal_depth: 1,
        })
        .await
        .unwrap();
        assert!(
            store.vector_calls() > 0,
            "{name}: recall must still read the database"
        );
        mem.close().await.unwrap();
        let paraphrase = second.created[0];
        assert!(
            store.edges.lock().iter().any(|e| {
                e.edge_type == crate::types::EdgeType::Semantic
                    && [e.source, e.target].contains(&first)
                    && [e.source, e.target].contains(&paraphrase)
            }),
            "{name}: the flush carried no Semantic merge edge"
        );
    }
    for_each_dialect!(check);
}

/// **#60 parity.** On the graph path the merge decision is exact `f32`
/// cosine against the threshold, inclusive: the pair merges at a threshold
/// equal to its cosine and not one ulp above. The database's score for the
/// same pair comes from its own distance arithmetic (or an ANN index) and is
/// not consulted, so a pair this close to the edge may decide differently
/// than it did before #60 (see the module docs).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_merge_at_the_threshold_edge_follows_exact_cosine() {
    async fn check<D: Dialect>(kind: StoreKind, name: &str) {
        let embedder = Arc::new(LabelVectors::new(DIM));
        let cosine = f64::from(crate::embed::cosine(
            &embedder.vector(ORIGINAL),
            &embedder.vector(PARAPHRASE),
        ));
        for (threshold, merges) in [(cosine, true), (cosine.next_up(), false)] {
            let store = Arc::new(LaggingDatabase::<D>::new(kind));
            let session = format!("pg-edge-{name}-{merges}");
            let mem = open_holder(store.clone(), &session, embedder.clone(), Some(threshold)).await;
            let first = mem
                .derive(&[(ORIGINAL, ConceptType::Entity)], &ParentOf::none())
                .await
                .unwrap()
                .created[0];
            let second = mem
                .derive(&[(PARAPHRASE, ConceptType::Entity)], &ParentOf::none())
                .await
                .unwrap();
            let want = if merges { vec![first] } else { Vec::new() };
            assert_eq!(
                second.semantic_merged, want,
                "{name}: threshold {threshold} against cosine {cosine}"
            );
            assert_eq!(store.vector_calls(), 0, "{name}");
            mem.close().await.unwrap();
        }
    }
    for_each_dialect!(check);
}

/// **#60 live, Postgres.** The `postgres-live` CI job runs this by name.
#[cfg(feature = "store-postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "live: requires LAMBO_POSTGRES_DSN against pinned pgvector/pgvector:pg17"]
async fn postgres_holder_merges_an_unflushed_paraphrase() {
    use super::postgres::{postgres_dsn_or_skip, PostgresStore};
    let Some(dsn) = postgres_dsn_or_skip("postgres_holder_merges_an_unflushed_paraphrase") else {
        return;
    };
    let store = PostgresStore::new(StoreConfig {
        kind: StoreKind::Postgres,
        dsn: Some(dsn),
        path: None,
        vector_dim: None,
    })
    .expect("construct");
    store.init_schema().await.expect("init_schema");
    let session = format!("pg-merge-freshness-{}", uuid::Uuid::new_v4());
    check_holder_merges_an_unflushed_paraphrase(Arc::new(store), &session).await;
}
