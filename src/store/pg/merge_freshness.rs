//! #60: on a session holder over the Postgres family, hybrid derive's
//! semantic merge sees what the holder wrote seconds ago, so a paraphrase
//! derived right after the original merges into it before the write-behind
//! flush has put the original in the database. Recall keeps the database's
//! vector search.
//!
//! The mechanism is #18's purpose split: `PgStore` declares
//! [`GraphStore::holder_derive_source`] as
//! [`HolderDeriveSource::StoreAndUnflushed`], which only
//! `VectorCandidates::for_holder_derive` reads; recall's
//! `VectorCandidates::for_holder` reads `exact_vector_scan`, which the family
//! leaves `false`. The source (`graph::vector_source::StoreAndUnflushedSource`)
//! asks the database for what the flush has made durable and ranks only the
//! holder's unflushed concepts in RAM, so derive costs the indexed query plus
//! O(unflushed), not a scan of the whole graph.
//!
//! Offline, a [`LaggingDatabase`] models the one property that matters: the
//! database's vector search sees only flushed rows. It takes its source
//! declarations from a real, never-connected `PgStore<D>`, so these tests
//! exercise the family's own answer, not the double's, and it loads what it
//! flushed, so a holder can reopen a durable session. The scenario
//! [`run_merge_freshness`] runs offline against it and live against a real
//! database: the Postgres leg is the `#[ignore]`d test at the bottom (the
//! `postgres-live` CI job runs it by name) and the Cockroach leg runs inside
//! `cockroach::conformance::conformance_suite`.
//!
//! # What changes at the threshold edge
//!
//! The database ranks by its own distance (Postgres `1 - d` over pgvector's
//! cosine distance, Cockroach `1 - d²/2` over L2, possibly from the partial
//! ANN index). The union re-scores every candidate by exact `f32` cosine on
//! the graph's vectors (`rank_by_cosine`), so a pair whose similarity sits on
//! the threshold can merge on one path and not the other. The durable leg's
//! pool is still the database's own top-k, so a target the ANN beam misses
//! is missed as it was before #60. No bit parity with the database is
//! claimed for the merge leg; [`a_merge_at_the_threshold_edge_follows_exact_cosine`]
//! pins what the union does decide.

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
use crate::store::{Capabilities, GraphStore, HolderDeriveSource, StoreConfig, StoreKind};
use crate::types::{
    CanonizationEvent, Concept, ConceptType, Edge, EdgeType, EmbeddingContract, GraphSnapshot,
    Interaction, InteractionSpan, MatchStrategy, Mutation, MutationBatch, Node, NodeId, Scored,
    SessionId, StoreError,
};

/// The concept the paraphrase must merge into.
pub(crate) const ORIGINAL: &str = "register user";
/// A paraphrase of [`ORIGINAL`]: cosine about 0.989 under [`LabelVectors`].
pub(crate) const PARAPHRASE: &str = "create account";
/// Another paraphrase of [`ORIGINAL`]: cosine about 0.989 to [`ORIGINAL`]
/// and about 0.978 to [`PARAPHRASE`], so its top tier is [`ORIGINAL`] alone.
pub(crate) const REPHRASE: &str = "sign up user";

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
        assert!(dim >= 4, "the paraphrases need three axes plus others");
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
            REPHRASE => {
                let n = (1.0f32 + 0.15 * 0.15).sqrt();
                v[0] = 1.0 / n;
                v[2] = 0.15 / n;
            }
            other => {
                // FNV-1a: stable across Rust releases, unlike DefaultHasher.
                let h = other.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
                    (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
                });
                let axis = 3 + usize::try_from(h % (self.dim as u64 - 3)).expect("fits");
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

/// Whether the flushed graph links `a` and `b` by the `Semantic` edge
/// hybrid records for a merge.
fn has_merge_edge(edges: &[Edge], a: NodeId, b: NodeId) -> bool {
    edges.iter().any(|e| {
        e.edge_type == EdgeType::Semantic
            && [e.source, e.target].contains(&a)
            && [e.source, e.target].contains(&b)
    })
}

/// The scenario both the offline double and the live databases run, over
/// one session of `store`, in two holder lifetimes:
///
/// 1. **Seed.** A holder derives an unrelated fact and closes, so the session
///    and its embedding contract are durable.
/// 2. **Freshness (#60).** A reopened holder derives [`ORIGINAL`] and, before
///    any flush, [`PARAPHRASE`]: the paraphrase merges into the unflushed
///    original, which the database cannot see yet (asserted). Closing makes
///    the merge durable.
///
/// Returns the session, for the caller to erase.
pub(crate) async fn run_merge_freshness(store: Arc<dyn GraphStore>, session: &str) -> SessionId {
    let dim = store.vector_dimensions().expect("pg stores carry vectors");
    let embedder = Arc::new(LabelVectors::new(dim));
    let contract = embedder.contract();
    let sid = SessionId::from(session);
    let durable_ids =
        |snap: &GraphSnapshot| -> Vec<NodeId> { snap.concepts.iter().map(|c| c.id).collect() };

    // 1. Seed.
    let seed = open_holder(store.clone(), session, embedder.clone(), None).await;
    seed.derive(
        &[("an unrelated fact", ConceptType::Entity)],
        &ParentOf::none(),
    )
    .await
    .expect("derive the seed");
    seed.close().await.expect("close flushes the seed");

    // 2. Freshness over a durable session.
    let mem = open_holder(store.clone(), session, embedder.clone(), None).await;
    let first = mem
        .derive(&[(ORIGINAL, ConceptType::Entity)], &ParentOf::none())
        .await
        .expect("derive the original")
        .created[0];
    let durable = store.load_session(&sid).await.expect("the seed is durable");
    assert!(
        !durable_ids(&durable).contains(&first),
        "the original must still be unflushed here"
    );
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
    let snap = store
        .load_session(&sid)
        .await
        .expect("load after the flush");
    assert!(
        has_merge_edge(&snap.edges, first, paraphrase),
        "no Semantic merge edge was flushed: {:?}",
        snap.edges
    );

    // Recall's source, unchanged: the database's own search, after the flush.
    let hits = store
        .vector_candidates_checked(&sid, &embedder.vector(PARAPHRASE), &contract, 5)
        .await
        .expect("the database answers once the session is flushed");
    assert!(
        hits.iter().any(|s| s.item == first),
        "the database finds the original for its paraphrase: {hits:?}"
    );
    sid
}

/// The live check both dialects run: [`run_merge_freshness`] over a real
/// pg-family store, then the session is erased.
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
    let sid = run_merge_freshness(store.clone(), session).await;
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

/// What the double has flushed for its one session.
#[derive(Default)]
struct Durable {
    interactions: HashMap<NodeId, Interaction>,
    concepts: HashMap<NodeId, Concept>,
    edges: HashMap<NodeId, Edge>,
    contract: Option<EmbeddingContract>,
    epoch: u64,
}

/// Runs once, right after the double has computed a vector answer and
/// before it returns it (the window test's commit).
type AfterSearch = Box<dyn FnOnce() + Send>;

/// A durable store whose vector search sees only what was flushed: the
/// write-behind lag of the real family, with no database. Every vector read
/// (checked or unchecked) is counted on entry, before it answers. It loads
/// what it flushed, vectors included, so a reopened holder sees it.
///
/// `capabilities`, `exact_vector_scan` and `holder_derive_source` come from
/// a real `PgStore<D>`; everything else is the double's.
struct LaggingDatabase<D: Dialect> {
    family: PgStore<D>,
    durable: parking_lot::Mutex<Durable>,
    vector_calls: AtomicUsize,
    after_search: parking_lot::Mutex<Option<AfterSearch>>,
}

impl<D: Dialect> LaggingDatabase<D> {
    fn new(kind: StoreKind) -> Self {
        Self {
            family: unconnected::<D>(kind),
            durable: parking_lot::Mutex::default(),
            vector_calls: AtomicUsize::new(0),
            after_search: parking_lot::Mutex::default(),
        }
    }

    fn vector_calls(&self) -> usize {
        self.vector_calls.load(Ordering::SeqCst)
    }

    fn holds(&self, id: NodeId) -> bool {
        self.durable.lock().concepts.contains_key(&id)
    }

    fn edges(&self) -> Vec<Edge> {
        self.durable.lock().edges.values().cloned().collect()
    }

    /// What the database's search answers: exact cosine over the flushed
    /// rows only.
    fn search(&self, probe: &[f32], limit: usize) -> Vec<Scored<NodeId>> {
        let hits = {
            let durable = self.durable.lock();
            crate::store::vector_source::rank_by_cosine(
                probe,
                durable.concepts.values().filter_map(|c| {
                    c.embedding
                        .as_deref()
                        .map(|v| (c.id, v, c.canonical_key.as_str()))
                }),
                limit,
            )
        };
        if let Some(hook) = self.after_search.lock().take() {
            hook();
        }
        hits
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
    fn holder_derive_source(&self) -> HolderDeriveSource {
        self.family.holder_derive_source()
    }
    async fn flush(&self, batch: &MutationBatch, _token: Option<u64>) -> Result<(), StoreError> {
        let mut durable = self.durable.lock();
        durable.epoch = durable.epoch.max(batch.mutation_epoch);
        for m in &batch.mutations {
            match m {
                Mutation::UpsertNode { node } => match node {
                    Node::Concept(c) => {
                        durable.concepts.insert(c.id, c.clone());
                    }
                    Node::Interaction(i) => {
                        durable.interactions.insert(i.id, i.clone());
                    }
                },
                Mutation::UpsertEdge { edge } => {
                    durable.edges.insert(edge.id, edge.clone());
                }
                Mutation::DeleteNode { id } => {
                    durable.concepts.remove(id);
                    durable
                        .edges
                        .retain(|_, e| e.source != *id && e.target != *id);
                }
                Mutation::DeleteEdge { id } => {
                    durable.edges.remove(id);
                }
                Mutation::SetEmbedding { embedding, .. } => {
                    durable.contract = embedding.clone();
                }
                _ => {}
            }
        }
        Ok(())
    }
    async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
        let durable = self.durable.lock();
        if durable.interactions.is_empty() {
            return Err(StoreError::SessionNotFound(session.0.clone()));
        }
        Ok(GraphSnapshot {
            session_id: session.clone(),
            interactions: durable.interactions.values().cloned().collect(),
            concepts: durable.concepts.values().cloned().collect(),
            edges: durable.edges.values().cloned().collect(),
            embedding: durable.contract.clone(),
            mutation_epoch: durable.epoch,
            ..GraphSnapshot::default()
        })
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
        let stored = self.durable.lock().contract.clone();
        match stored {
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
/// takes the store plus the holder's unflushed concepts
/// (`for_holder_derive`).
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
        assert_eq!(
            store.holder_derive_source(),
            HolderDeriveSource::StoreAndUnflushed,
            "{name}"
        );
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
                VectorCandidates::StoreAndUnflushed(_)
            ),
            "{name}: derive's source"
        );
    }
    for_each_dialect!(check);
}

/// **#60 acceptance 1.** The shared scenario over the double: a paraphrase
/// merges into an unflushed original over a durable session, through the
/// union of the database and the holder's unflushed concepts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_holder_merges_an_unflushed_paraphrase_over_a_durable_session() {
    async fn check<D: Dialect>(kind: StoreKind, name: &str) {
        let store = Arc::new(LaggingDatabase::<D>::new(kind));
        run_merge_freshness(store.clone(), &format!("pg-merge-{name}")).await;
        assert!(
            store.vector_calls() > 0,
            "{name}: the durable leg must ask the database"
        );
    }
    for_each_dialect!(check);
}

/// **#60 acceptance 1 and 3, fresh session.** Before the session's
/// embedding contract is durable the database cannot answer under it, so
/// derive ranks the holder's graph and makes no vector call to the store,
/// synchronously and through the write queue; the paraphrase still merges.
/// Recall still asks the database.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fresh_session_merges_before_its_first_flush() {
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
            !store.holds(first),
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
        // constructor: its paraphrase merges into the same unflushed concept.
        let agent = crate::types::AgentId::from("agent-a");
        let submitted = mem
            .derive_async_as(
                &agent,
                &[(REPHRASE, ConceptType::Entity)],
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
        let rephrase = mem
            .graph()
            .read()
            .concepts()
            .find(|c| c.content == REPHRASE)
            .map(|c| c.id)
            .expect("applied");
        let edges: Vec<Edge> = mem.graph().read().edges().cloned().collect();
        assert!(
            has_merge_edge(&edges, first, rephrase),
            "{name}: the queued paraphrase did not merge"
        );

        assert_eq!(
            store.vector_calls(),
            0,
            "{name}: derive asked the database before its contract was durable"
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
        assert!(
            has_merge_edge(&store.edges(), first, second.created[0]),
            "{name}: the flush carried no Semantic merge edge"
        );
    }
    for_each_dialect!(check);
}

/// A graph over the double with a durable contract, for the source-level
/// tests: flushes what it has, as the flush task would, and marks it
/// durable.
fn commit<D: Dialect>(
    graph: &parking_lot::RwLock<crate::graph::Graph>,
    store: &LaggingDatabase<D>,
) {
    let batch = graph.write().drain_log();
    futures_lite_block(store.flush(&batch, None)).expect("the double accepts every batch");
    graph.write().mark_durable_through(batch.mutation_epoch);
}

/// Drive a future that never actually waits (the double's flush) to
/// completion on the current thread.
fn futures_lite_block<F: std::future::Future>(f: F) -> F::Output {
    let mut f = std::pin::pin!(f);
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    match f.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(out) => out,
        std::task::Poll::Pending => panic!("the double's flush never waits"),
    }
}

/// A one-interaction graph stamped with [`LabelVectors`]' contract, with
/// `contents` as concepts embedded by it.
fn graph_with(
    session: &SessionId,
    embedder: &LabelVectors,
    contents: &[&str],
) -> (parking_lot::RwLock<crate::graph::Graph>, Vec<NodeId>) {
    let mut g = crate::graph::Graph::new(session.clone());
    g.stamp_embedding(embedder.contract()).unwrap();
    g.insert_interaction(Interaction {
        event_time: None,
        id: NodeId(uuid::Uuid::from_u64_pair(1, 1)),
        session_id: session.clone(),
        agent_id: crate::types::AgentId::from("agent-a"),
        prompt_text: Some("seed".into()),
        previous_id: None,
        created_at: Utc::now(),
    })
    .unwrap();
    let graph = parking_lot::RwLock::new(g);
    let ids = contents
        .iter()
        .map(|content| add_concept(&graph, embedder, content))
        .collect();
    (graph, ids)
}

/// Write `content` as a concept embedded by [`LabelVectors`], derived from
/// the graph's one interaction.
fn add_concept(
    graph: &parking_lot::RwLock<crate::graph::Graph>,
    embedder: &LabelVectors,
    content: &str,
) -> NodeId {
    use crate::types::{AgentId, CanonizationStatus};
    let mut g = graph.write();
    let origin = g.interactions().next().expect("one interaction").id;
    let session = g.session_id().clone();
    let id = NodeId(uuid::Uuid::new_v4());
    g.insert_concept(
        Concept {
            id,
            session_id: session,
            content: content.into(),
            canonical_key: content.into(),
            concept_type: ConceptType::Entity,
            origin_interaction: origin,
            origin_agent: AgentId::from("agent-a"),
            created_at: Utc::now(),
            access_count: 0,
            last_accessed: None,
            gc_survived: 0,
            canonization_status: CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: None,
            embedding: Some(embedder.vector(content)),
            human_confirmed: 0,
            embedding_source: None,
            chunk_group_id: None,
        },
        origin,
    )
    .unwrap();
    id
}

/// **The window between commit and clear.** The source reads the holder's
/// unflushed set before it asks the database. Here the database answers
/// from its state before a flush commits [`ORIGINAL`], and the flush then
/// commits it and clears it from the set before the source re-ranks: read
/// in that order, [`ORIGINAL`] is still found. Reading the set after the
/// query would find it in neither.
#[tokio::test]
async fn a_concept_committed_during_the_query_is_not_missed() {
    async fn check<D: Dialect>(kind: StoreKind, name: &str) {
        use crate::store::vector_source::VectorCandidates;
        let session = SessionId::from(format!("pg-window-{name}").as_str());
        let embedder = LabelVectors::new(DIM);
        let store = Arc::new(LaggingDatabase::<D>::new(kind));
        let (graph, _) = graph_with(&session, &embedder, &["an unrelated fact"]);
        commit(&graph, &store);
        let original = add_concept(&graph, &embedder, ORIGINAL);
        assert!(!store.holds(original));
        let graph = Arc::new(graph);
        {
            let (graph, hook_store) = (graph.clone(), store.clone());
            *store.after_search.lock() = Some(Box::new(move || commit(&graph, &hook_store)));
        }

        let source = VectorCandidates::for_holder_derive(store.as_ref(), graph.as_ref());
        let hits = source
            .checked(
                &session,
                &embedder.vector(PARAPHRASE),
                &embedder.contract(),
                8,
            )
            .await
            .unwrap();
        assert!(store.holds(original), "{name}: the hook committed it");
        assert!(
            !graph.read().is_unflushed(&original),
            "{name}: and cleared it"
        );
        assert_eq!(
            hits.first().map(|s| s.item),
            Some(original),
            "{name}: a concept committed during the query was missed: {hits:?}"
        );
    }
    for_each_dialect!(check);
}

/// The graph is the authority over the database's rows: a concept the
/// holder deleted (not yet flushed) is dropped although the database still
/// returns it, and a concept the holder rewrote is scored by its new vector.
#[tokio::test]
async fn the_graph_overrides_the_database_for_unflushed_rewrites() {
    async fn check<D: Dialect>(kind: StoreKind, name: &str) {
        use crate::store::vector_source::VectorCandidates;
        let session = SessionId::from(format!("pg-override-{name}").as_str());
        let embedder = LabelVectors::new(DIM);
        let store = LaggingDatabase::<D>::new(kind);
        let (graph, ids) = graph_with(&session, &embedder, &[ORIGINAL, PARAPHRASE]);
        commit(&graph, &store);
        let probe = embedder.vector(ORIGINAL);

        graph.write().remove_node(ids[0]).unwrap();
        // The database still ranks the deleted original first.
        assert_eq!(store.search(&probe, 1)[0].item, ids[0]);
        let source = VectorCandidates::for_holder_derive(&store, &graph);
        let hits = source
            .checked(&session, &probe, &embedder.contract(), 8)
            .await
            .unwrap();
        assert!(
            hits.iter().all(|s| s.item != ids[0]),
            "{name}: a deleted concept was offered as a merge target: {hits:?}"
        );
        let want = f64::from(crate::embed::cosine(&probe, &embedder.vector(PARAPHRASE)));
        assert_eq!(hits.first().map(|s| s.item), Some(ids[1]), "{name}");
        assert_eq!(hits[0].score, want, "{name}: scored on the graph's vector");
    }
    for_each_dialect!(check);
}

/// **#60 parity.** The merge decision is exact `f32` cosine against the
/// threshold, inclusive: the pair merges at a threshold equal to its cosine
/// and not one ulp above. The database's score for the same pair comes from
/// its own distance arithmetic (or an ANN index) and is not used for the
/// final ranking, so a pair this close to the edge may decide differently
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
