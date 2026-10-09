//! #14: the per-session query-embedding cache.
//!
//! Every test runs on both vector sources a holder can have: the store's
//! checked read (`VectorSearchStore::new`) and the holder's own graph
//! (`VectorSearchStore::graph_ranked`, #8). "Uncached" below means the same
//! recall on the same handle after `clear_query_embeddings`, so the two
//! answers are compared over one graph state; the recall cache never serves a
//! vector-dependent pipeline (P1-2), so neither answer comes from it.

use super::writes::{ContextTolerantEmbedder, VectorSearchStore};
use super::*;
use crate::recall::detail::DetailedRecall;
use crate::store::erase::ERASED_HOLDER;

/// Counts every embed, and fails the first `fail_first` of them.
#[derive(Debug)]
struct CountingEmbedder {
    inner: ContextTolerantEmbedder,
    calls: AtomicUsize,
    /// Every text embedded, in call order.
    texts: parking_lot::Mutex<Vec<String>>,
    fail_remaining: AtomicUsize,
    /// Embeds of exactly this text answer a bad (half-width, NaN) vector.
    poison: Option<&'static str>,
}

impl CountingEmbedder {
    fn new() -> Arc<Self> {
        Self::failing_first(0)
    }

    fn failing_first(n: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: ContextTolerantEmbedder(FixtureEmbedder::new()),
            calls: AtomicUsize::new(0),
            texts: Default::default(),
            fail_remaining: AtomicUsize::new(n),
            poison: None,
        })
    }

    fn poisoned(text: &'static str) -> Arc<Self> {
        Arc::new(Self {
            inner: ContextTolerantEmbedder(FixtureEmbedder::new()),
            calls: AtomicUsize::new(0),
            texts: Default::default(),
            fail_remaining: AtomicUsize::new(0),
            poison: Some(text),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Embeds of exactly `text`, so a background embed (the write
    /// pipeline's calibration probe) can never shift the count.
    fn calls_of(&self, text: &str) -> usize {
        self.texts.lock().iter().filter(|t| *t == text).count()
    }
}

#[async_trait]
impl Embedder for CountingEmbedder {
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.embed_as(text, crate::test_util::TextRole::Document)
            .await
    }
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.embed_as(text, crate::test_util::TextRole::Query).await
    }
    fn modalities(&self) -> crate::embed::Modalities {
        self.inner.modalities()
    }
    async fn embed_image(
        &self,
        image: crate::embed::ImageInput<'_>,
    ) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.inner.embed_image(image).await
    }
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        self.inner.as_any()
    }
}

impl CountingEmbedder {
    /// The `embed` behaviour above, in either text role (#22: a wrapper
    /// forwards the role, so its inner embedder sees what the caller asked).
    async fn embed_as(
        &self,
        text: &str,
        role: crate::test_util::TextRole,
    ) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.texts.lock().push(text.to_owned());
        let fail = self
            .fail_remaining
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n > 0).then(|| n - 1)
            })
            .is_ok();
        if fail {
            return Err(crate::embed::EmbedError::Unavailable(
                "simulated embedder outage".into(),
            ));
        }
        if self.poison == Some(text) {
            let mut bad = role.embed(&self.inner, text).await?;
            bad.truncate(bad.len() / 2);
            bad[0] = f32::NAN;
            return Ok(bad);
        }
        role.embed(&self.inner, text).await
    }
}

#[derive(Clone, Copy, Debug)]
enum Source {
    Store,
    Graph,
}

const SOURCES: [Source; 2] = [Source::Store, Source::Graph];

fn vector_store(source: Source) -> Arc<VectorSearchStore> {
    let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    Arc::new(match source {
        Source::Store => VectorSearchStore::new(inner),
        Source::Graph => VectorSearchStore::graph_ranked(inner),
    })
}

async fn open(
    store: Arc<VectorSearchStore>,
    session: &str,
    embedder: Arc<CountingEmbedder>,
) -> Memory {
    Memory::builder()
        .session(session)
        .agent("agent-a")
        // Short: the store source answers from what was flushed.
        .flush_interval(Duration::from_millis(10))
        .match_strategy(MatchStrategy::Hybrid)
        .store(store as Arc<dyn GraphStore>)
        .embedder(embedder as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .build()
        .await
        .expect("build")
}

fn recall_query(text: &str, top_k: usize, depth: usize) -> RecallQuery {
    RecallQuery {
        query: text.into(),
        top_k,
        max_tokens: 500,
        traversal_depth: depth,
    }
}

/// Hits as (id, "score content"), legs as (id, "{legs:?}") sorted, warnings.
type Answer = (Vec<(NodeId, String)>, Vec<(NodeId, String)>, Vec<String>);

/// What must not differ between a cached and an uncached recall: the hits
/// (ids, scores, content), the per-leg provenance and the warnings.
fn answer(r: &DetailedRecall) -> Answer {
    let hits = r
        .hits
        .iter()
        .map(|h| (h.node_id, format!("{:.12} {}", h.score, h.content)))
        .collect();
    let mut legs: Vec<(NodeId, String)> = r
        .legs
        .iter()
        .map(|(id, l)| (*id, format!("{l:?}")))
        .collect();
    legs.sort_by_key(|(id, _)| id.to_string());
    (hits, legs, r.warnings.clone())
}

/// Recall `q` from the cache, then again with the cache cleared, and assert
/// the two answers are identical. Returns the cached answer.
async fn cached_equals_uncached(mem: &Memory, q: RecallQuery) -> DetailedRecall {
    let cached = mem.recall_detailed(q.clone()).await.unwrap();
    mem.clear_query_embeddings();
    let uncached = mem.recall_detailed(q).await.unwrap();
    assert_eq!(answer(&cached), answer(&uncached), "cached != uncached");
    cached
}

/// Wait until every graph mutation so far is durable, so the store source
/// and the graph source see the same concepts.
async fn flushed(mem: &Memory) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let stats = mem.stats();
            if stats.log_depth == 0 && stats.flush_depth == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the flush drains within 10 s");
}

/// Two concepts, flushed; returns the one near the test query.
async fn seed(mem: &Memory) -> NodeId {
    let near = mem
        .derive(
            &[
                (crate::embed::NEAR_A, ConceptType::Entity),
                ("billing ledger", ConceptType::Entity),
            ],
            &ParentOf::none(),
        )
        .await
        .unwrap()
        .created[0];
    flushed(mem).await;
    near
}

/// **#14 acceptance 1.** An identical repeated recall does not call the
/// embedder, and its vector leg still fires.
#[tokio::test]
async fn an_identical_repeated_recall_skips_the_embed() {
    for source in SOURCES {
        let embedder = CountingEmbedder::new();
        let store = vector_store(source);
        let mem = open(store.clone(), "q14-repeat", embedder.clone()).await;
        let near = seed(&mem).await;
        let q = recall_query(crate::embed::NEAR_B, 5, 1);

        let before = embedder.calls();
        let reads = store.answers().len();
        let first = mem.recall_detailed(q.clone()).await.unwrap();
        assert_eq!(embedder.calls(), before + 1, "{source:?}: a miss embeds");
        // Pin which vector source ran, so a silent fallback from the graph
        // to the store's checked read (or back) fails here (#14 review L2).
        match source {
            Source::Graph => assert!(
                store.answers().is_empty(),
                "the graph source never calls the store's checked read"
            ),
            Source::Store => assert!(
                store.answers().len() > reads,
                "the store source answered from the checked read"
            ),
        }
        for _ in 0..3 {
            let again = mem.recall_detailed(q.clone()).await.unwrap();
            assert_eq!(answer(&again), answer(&first), "{source:?}");
        }
        assert_eq!(
            embedder.calls(),
            before + 1,
            "{source:?}: three repeats, no further embed"
        );
        assert!(
            first.legs.get(&near).and_then(|l| l.vector).is_some(),
            "{source:?}: the vector leg fired"
        );
        mem.close().await.unwrap();
    }
}

/// A write between two identical recalls advances the epoch, which the
/// query vector does not depend on: the second recall reuses the vector,
/// sees the new concept, and answers exactly as an uncached recall would.
#[tokio::test]
async fn a_write_between_recalls_reuses_the_vector_and_sees_the_write() {
    for source in SOURCES {
        let embedder = CountingEmbedder::new();
        let store = vector_store(source);
        let mem = open(store.clone(), "q14-write", embedder.clone()).await;
        seed(&mem).await;
        let q = recall_query(crate::embed::NEAR_B, 5, 1);
        mem.recall_detailed(q.clone()).await.unwrap();
        let epoch = mem.stats().epoch;

        // Shares the token "account" with the query, so the keyword leg finds
        // it; far from it in the fixture space, so it is created, not merged.
        let added = mem
            .derive(
                &[("account deletion policy", ConceptType::Logic)],
                &ParentOf::none(),
            )
            .await
            .unwrap()
            .created[0];
        assert!(
            mem.stats().epoch > epoch,
            "premise: the write moved the epoch"
        );
        flushed(&mem).await;
        let calls = embedder.calls();
        let after = mem.recall_detailed(q.clone()).await.unwrap();
        assert_eq!(
            embedder.calls(),
            calls,
            "{source:?}: no embed after a write"
        );
        assert!(
            after.hits.iter().any(|h| h.node_id == added),
            "{source:?}: the recall after the write sees it"
        );
        cached_equals_uncached(&mem, q).await;
        mem.close().await.unwrap();
    }
}

/// A retraction (a deletion) between recalls: the retracted concept is gone
/// from the cached recall exactly as from an uncached one.
#[tokio::test]
async fn a_retraction_between_recalls_matches_an_uncached_recall() {
    for source in SOURCES {
        let embedder = CountingEmbedder::new();
        let mem = open(vector_store(source), "q14-retract", embedder.clone()).await;
        let near = seed(&mem).await;
        let q = recall_query(crate::embed::NEAR_B, 5, 1);
        let before = mem.recall_detailed(q.clone()).await.unwrap();
        assert!(before.hits.iter().any(|h| h.node_id == near));

        mem.retract(crate::embed::NEAR_A, DryRun::No).await.unwrap();
        flushed(&mem).await;
        let calls = embedder.calls();
        let after = cached_equals_uncached(&mem, q).await;
        assert_eq!(
            embedder.calls(),
            calls + 1,
            "only the uncached twin embedded"
        );
        assert!(
            !after.hits.iter().any(|h| h.node_id == near),
            "{source:?}: the retracted concept is gone"
        );
        mem.close().await.unwrap();
    }
}

/// The vector is keyed on the text alone, so different `top_k` and depth
/// share one embed, and each answer is the uncached answer for its own
/// parameters.
#[tokio::test]
async fn different_recall_parameters_share_one_embed() {
    for source in SOURCES {
        let embedder = CountingEmbedder::new();
        let mem = open(vector_store(source), "q14-params", embedder.clone()).await;
        seed(&mem).await;
        let calls = embedder.calls();
        let mut answers = Vec::new();
        for (top_k, depth) in [(5, 1), (1, 0), (3, 2)] {
            let r = mem
                .recall_detailed(recall_query(crate::embed::NEAR_B, top_k, depth))
                .await
                .unwrap();
            answers.push(answer(&r));
        }
        assert_eq!(
            embedder.calls(),
            calls + 1,
            "{source:?}: one embed for three"
        );
        assert_ne!(answers[0], answers[1], "premise: the parameters matter");
        for (top_k, depth) in [(5, 1), (1, 0), (3, 2)] {
            cached_equals_uncached(&mem, recall_query(crate::embed::NEAR_B, top_k, depth)).await;
        }
        mem.close().await.unwrap();
    }
}

/// Distinct texts get their own vectors: alternating two queries, each
/// cached answer is that query's uncached answer, and the two differ.
#[tokio::test]
async fn distinct_queries_are_answered_with_their_own_vectors() {
    for source in SOURCES {
        let embedder = CountingEmbedder::new();
        let mem = open(vector_store(source), "q14-distinct", embedder.clone()).await;
        seed(&mem).await;
        let near = recall_query(crate::embed::NEAR_B, 5, 1);
        let far = recall_query(crate::embed::FAR, 5, 1);
        for q in [&near, &far] {
            mem.recall_detailed(q.clone()).await.unwrap();
        }
        let calls = embedder.calls();
        let a = mem.recall_detailed(near.clone()).await.unwrap();
        let b = mem.recall_detailed(far.clone()).await.unwrap();
        assert_eq!(embedder.calls(), calls, "{source:?}: both were hits");
        assert_ne!(answer(&a), answer(&b), "premise: the queries differ");
        cached_equals_uncached(&mem, far.clone()).await;
        mem.recall_detailed(near.clone()).await.unwrap();
        cached_equals_uncached(&mem, near).await;
        mem.close().await.unwrap();
    }
}

/// A failed embed degrades the recall to keyword + recent with the warning,
/// and is not cached: the next recall embeds again and gets its vector leg.
#[tokio::test]
async fn a_failed_embed_is_not_cached() {
    for source in SOURCES {
        let embedder = CountingEmbedder::new();
        let mem = open(vector_store(source), "q14-fail", embedder.clone()).await;
        let near = seed(&mem).await;
        embedder.fail_remaining.store(1, Ordering::SeqCst);
        let q = recall_query(crate::embed::NEAR_B, 5, 1);

        let calls = embedder.calls();
        let degraded = mem.recall_detailed(q.clone()).await.unwrap();
        assert!(
            degraded
                .warnings
                .iter()
                .any(|w| w.contains("query embedding failed")),
            "{source:?}: {:?}",
            degraded.warnings
        );
        assert!(degraded.legs.values().all(|l| l.vector.is_none()));
        let healed = mem.recall_detailed(q.clone()).await.unwrap();
        assert_eq!(
            embedder.calls(),
            calls + 2,
            "{source:?}: the failure was not cached"
        );
        assert!(healed.legs.get(&near).and_then(|l| l.vector).is_some());
        mem.recall_detailed(q.clone()).await.unwrap();
        assert_eq!(embedder.calls(), calls + 2, "{source:?}: the success was");
        // An outage after the vector was cached: the hit never reaches the
        // embedder, so the recall keeps its vector leg and has no warning.
        embedder.fail_remaining.store(1, Ordering::SeqCst);
        let through_outage = mem.recall_detailed(q).await.unwrap();
        assert_eq!(embedder.calls(), calls + 2);
        assert!(
            through_outage.warnings.is_empty(),
            "{:?}",
            through_outage.warnings
        );
        assert_eq!(answer(&through_outage), answer(&healed), "{source:?}");
        mem.close().await.unwrap();
    }
}

/// #23: the cache is consulted after `ensure_open`, so a closed or erased
/// handle refuses a recall the cache could have answered.
#[tokio::test]
async fn a_closed_or_erased_session_never_answers_from_the_cache() {
    for source in SOURCES {
        for erase in [false, true] {
            let embedder = CountingEmbedder::new();
            let mem = open(vector_store(source), "q14-closed", embedder.clone()).await;
            seed(&mem).await;
            let q = recall_query(crate::embed::NEAR_B, 5, 1);
            mem.recall_detailed(q.clone()).await.unwrap();
            assert_eq!(mem.query_embeddings.lock().len(), 1, "premise: cached");
            let calls = embedder.calls();

            if erase {
                mem.simulate_lease_loss_to(ERASED_HOLDER);
                assert!(mem.erased());
            } else {
                mem.close().await.unwrap();
            }
            let err = mem
                .recall_detailed(q)
                .await
                .expect_err("a closed or erased handle refuses the read");
            if erase {
                assert!(err.to_string().contains("erased"), "{err}");
            } else {
                assert!(err.to_string().contains("closed"), "{err}");
            }
            assert_eq!(embedder.calls(), calls);
        }
    }
}

/// Concurrent identical recalls on one handle all give the same answer, and
/// once one has cached the vector a later recall embeds nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_identical_recalls_agree() {
    for source in SOURCES {
        let embedder = CountingEmbedder::new();
        let mem = Arc::new(open(vector_store(source), "q14-concurrent", embedder.clone()).await);
        seed(&mem).await;
        let calls = embedder.calls();
        let q = recall_query(crate::embed::NEAR_B, 5, 1);
        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let mem = mem.clone();
                let q = q.clone();
                tokio::spawn(async move { answer(&mem.recall_detailed(q).await.unwrap()) })
            })
            .collect();
        let mut answers = Vec::new();
        for t in tasks {
            answers.push(t.await.unwrap());
        }
        assert!(answers.windows(2).all(|w| w[0] == w[1]), "{source:?}");
        let concurrent = embedder.calls() - calls;
        assert!((1..=8).contains(&concurrent), "{concurrent}");
        mem.recall_detailed(q).await.unwrap();
        assert_eq!(
            embedder.calls() - calls,
            concurrent,
            "{source:?}: now a hit"
        );
        assert_eq!(mem.query_embeddings.lock().len(), 1);
        mem.close().await.unwrap();
    }
}

/// #32 decision 13: the cache is per session. Two sessions sharing one
/// embedder instance in one process each pay their own first embed of the
/// same text, so one cannot learn from timing that the other ran it.
#[tokio::test]
async fn sessions_do_not_share_query_embeddings() {
    let embedder = CountingEmbedder::new();
    let store = vector_store(Source::Graph);
    let a = open(store.clone(), "q14-tenant-a", embedder.clone()).await;
    let b = open(store, "q14-tenant-b", embedder.clone()).await;
    const TEXT: &str = "a private question";
    let q = recall_query(TEXT, 5, 1);
    a.recall_detailed(q.clone()).await.unwrap();
    a.recall_detailed(q.clone()).await.unwrap();
    assert_eq!(embedder.calls_of(TEXT), 1);
    b.recall_detailed(q).await.unwrap();
    assert_eq!(embedder.calls_of(TEXT), 2, "b embeds for itself");
    a.close().await.unwrap();
    b.close().await.unwrap();
}

/// A store without a vector leg never embeds the query, so nothing is
/// cached either.
#[tokio::test]
async fn no_vector_leg_means_no_embed_and_no_entry() {
    let embedder = CountingEmbedder::new();
    let mem = Memory::builder()
        .session("q14-no-vectors")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::new(MemoryStore::new()) as Arc<dyn GraphStore>)
        .embedder(embedder.clone() as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .build()
        .await
        .expect("build");
    mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
        .await
        .unwrap();
    let calls = embedder.calls();
    mem.recall(query("user schema")).await.unwrap();
    assert_eq!(embedder.calls(), calls);
    assert!(mem.query_embeddings.lock().is_empty());
    mem.close().await.unwrap();
}

/// #14 review L1: a bad vector from a non-validating embedder (wrong width,
/// non-finite) is used for that one recall but never cached, so a repeat
/// embeds afresh instead of running keyword-only until eviction.
#[tokio::test]
async fn a_bad_query_vector_is_not_cached() {
    const BAD: &str = "a query the embedder botches";
    for source in SOURCES {
        let embedder = CountingEmbedder::poisoned(BAD);
        let mem = open(vector_store(source), "q14-bad-vector", embedder.clone()).await;
        seed(&mem).await;
        let q = recall_query(BAD, 5, 1);
        let before = embedder.calls();
        mem.recall_detailed(q.clone()).await.unwrap();
        mem.recall_detailed(q).await.unwrap();
        assert_eq!(
            embedder.calls(),
            before + 2,
            "{source:?}: each recall embeds again"
        );
        assert!(mem.query_embeddings.lock().is_empty(), "{source:?}");
        mem.close().await.unwrap();
    }
}

/// #22: an asymmetric embedder. Its query role embeds a prefixed text, so a
/// query and a document of the same words get different vectors; each role
/// records the texts it was asked for.
#[derive(Debug)]
struct QueryPrefixed {
    inner: ContextTolerantEmbedder,
    documents: parking_lot::Mutex<Vec<String>>,
    queries: parking_lot::Mutex<Vec<String>>,
}

const QUERY_ROLE_PREFIX: &str = "query role: ";

#[async_trait]
impl Embedder for QueryPrefixed {
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.documents.lock().push(text.to_owned());
        self.inner.embed(text).await
    }
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.queries.lock().push(text.to_owned());
        self.inner
            .embed(&format!("{QUERY_ROLE_PREFIX}{text}"))
            .await
    }
    fn modalities(&self) -> crate::embed::Modalities {
        self.inner.modalities()
    }
    async fn embed_image(
        &self,
        image: crate::embed::ImageInput<'_>,
    ) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.inner.embed_image(image).await
    }
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        self.inner.as_any()
    }
}

/// **#22 PR 1.** Recall embeds its query through `embed_query`, and the
/// query-embedding cache holds that query-role vector: a hit serves exactly
/// what a miss embeds, never a document-role vector of the same text.
#[tokio::test]
async fn recall_caches_the_query_role_vector() {
    for source in SOURCES {
        let embedder = Arc::new(QueryPrefixed {
            inner: ContextTolerantEmbedder(FixtureEmbedder::new()),
            documents: Default::default(),
            queries: Default::default(),
        });
        let mem = Memory::builder()
            .session("q22-query-role")
            .agent("agent-a")
            .flush_interval(Duration::from_millis(10))
            .match_strategy(MatchStrategy::Hybrid)
            .store(vector_store(source) as Arc<dyn GraphStore>)
            .embedder(embedder.clone() as Arc<dyn Embedder>)
            .embedding_contract(contract("fixture", 1024))
            .build()
            .await
            .expect("build");
        mem.derive(
            &[("billing ledger", ConceptType::Entity)],
            &ParentOf::none(),
        )
        .await
        .unwrap();
        flushed(&mem).await;
        let query = crate::embed::NEAR_B;
        let q = recall_query(query, 5, 1);

        mem.recall_detailed(q.clone()).await.unwrap();
        mem.recall_detailed(q).await.unwrap();

        let queries = embedder.queries.lock().clone();
        assert_eq!(
            queries,
            vec![query.to_owned()],
            "{source:?}: one query embed"
        );
        assert!(
            !embedder.documents.lock().iter().any(|t| t == query),
            "{source:?}: the query never went through the document role"
        );
        let expected = FixtureEmbedder::new()
            .embed(&format!("{QUERY_ROLE_PREFIX}{query}"))
            .await
            .unwrap();
        let cached = mem
            .query_embeddings
            .lock()
            .get(query, &contract("fixture", 1024))
            .expect("the query vector is cached");
        assert_eq!(&*cached, expected.as_slice(), "{source:?}");
        mem.close().await.unwrap();
    }
}
