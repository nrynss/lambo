//! Phase-1 candidate retrieval (T5.1, spec §8) — the UNION of three legs.
//!
//! Legs:
//! 1. **Keyword** — [`InvertedIndex::search`] (`query`, `limit`): BM25 hits,
//!    golden-exact (`fixtures/recall-goldens.json` `phase1_candidates`).
//! 2. **Recent interactions** — concepts whose `origin_interaction` is one of
//!    the N = [`RECENT_INTERACTIONS`] most recent interactions by `created_at`
//!    (ties broken by node id ascending). The temporal chain order is NOT
//!    consulted: the contract is "3 most recent by `created_at`" (handoff
//!    T5.1), and a chain ordered by insertion may carry arbitrary timestamps.
//! 3. **Vector** — [`GraphStore::vector_candidates_checked`], only when the store
//!    advertises [`Capabilities::VECTOR_SEARCH`]. The call is async I/O, so it
//!    is gathered by [`gather`] BEFORE any graph lock is taken; [`candidates`]
//!    itself is pure and lock-safe.
//!
//! RAM-tier promise (spec §3.2): when the store lacks `VECTOR_SEARCH`,
//! [`gather`] makes zero async store calls and logs exactly one line. The only
//! trait interaction on that path is the synchronous `capabilities()` probe,
//! which is unavoidable (it decides whether the vector call may even be made)
//! and costs no I/O. The vector leg likewise skips (one log line, zero calls)
//! when no query embedding is available.
//!
//! Merge rule: the three legs are unioned by node id and a node present in more
//! than one leg keeps its HIGHEST score (max-merge). Keyword members keep their
//! BM25 score; recent-interaction members score a flat [`RECENT_SCORE`] (a
//! secondary signal below a genuine BM25 or similarity hit); vector members
//! keep the store-provided similarity score. The merged list is sorted
//! score-descending with ties broken by node id ascending, then truncated to
//! `limit`. Sorting is a total order (f64 `total_cmp`, then UUID), so the
//! output is deterministic regardless of leg iteration or store result order.
//!
//! The caller supplies the [`InvertedIndex`]: the graph itself owns no index
//! (P3 contract — the session owner mirrors every concept write into a separate
//! [`InvertedIndex`]; see `src/graph/mod.rs`), so building one here would
//! silently re-derive a possibly stale copy.
//!
//! Golden reconciliation (acceptance 6): the golden `phase1_candidates` lists
//! assert ONLY the keyword leg — `InvertedIndex::search` equals them EXACTLY
//! (set and order), verified by `recall_phase1_keyword_goldens_pass` in
//! `src/graph/index.rs` and by the golden test in this module (keyword members
//! filtered out of the union re-sort to the golden order). The full union is a
//! strict superset for the two golden queries: `session-rest-api`'s 3 most
//! recent interactions (09:45-09:55Z) own concepts 1009-1012, which neither
//! golden query ("pagination", "create") matches, and the fixture carries no
//! embeddings, so the vector leg is empty. That is union semantics by design
//! (spec §8), not a fixture defect; the golden EXACT assertion is authoritative
//! for the keyword leg and must not regress.

use std::collections::{HashMap, HashSet};

use crate::graph::index::InvertedIndex;
use crate::graph::Graph;
use crate::store::{validate_vector_candidate_limit, Capabilities, GraphStore};
use crate::types::{EmbeddingContract, NodeId, Scored, SessionId, StoreError};

/// Number of most-recent interactions whose concepts join phase 1 (spec §8).
pub const RECENT_INTERACTIONS: usize = 3;

/// Flat phase-1 score for recent-interaction-leg members.
///
/// Chosen as a secondary signal below a genuine BM25 hit or store similarity
/// score. BGE-M3 calibration put the lowest durable true semantic hit at
/// `0.3991`; `0.35` keeps the recency leg useful without masking that ranking
/// under max-merge. The exact value is not spec-pinned, only its rank position
/// relative to real matches. See `evidence/mooshik-g-recall-calibration/`.
pub const RECENT_SCORE: f64 = 0.35;

/// Phase-1 keyword over-fetch multiplier: the keyword leg is over-fetched so
/// strong query matches cannot be evicted by the flat-scored recent/vector
/// legs at phase-1 truncation (GPT5.6sol P1-3).
pub(crate) const KEYWORD_OVERFETCH: usize = 4;

/// The score each phase-1 leg contributed for one node, **before** max-merge.
///
/// The merged score alone cannot answer G's question — "was this a real semantic
/// hit, a lexical hit, or just the recency floor?" — because max-merge is
/// lossy: a `0.35` could be the [`RECENT_SCORE`] floor or a genuine (weak)
/// cosine, and the two mean opposite things. `None` means the leg did not
/// produce this node at all, which is itself information (a node with only
/// `recent` set was retrieved by nothing but recency).
///
/// Collected on every call rather than only under a trace subscriber: the leg
/// names already had a trace-gated map (below), but the I1 ledger needs the
/// *numbers*, and only from a run nobody thought to enable tracing on.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LegScores {
    /// BM25 score from [`InvertedIndex::search`].
    pub keyword: Option<f64>,
    /// The flat [`RECENT_SCORE`] floor, when this node belongs to one of the
    /// [`RECENT_INTERACTIONS`] most recent interactions.
    pub recent: Option<f64>,
    /// Store-provided similarity (cosine, for every shipped vector store).
    pub vector: Option<f64>,
}

impl LegScores {
    /// The max-merged score these legs produce — the value phase 1 ranks on.
    ///
    /// Kept next to the legs so the two can never disagree: the merge rule is
    /// stated once, here, and [`candidates_with_legs`] is its only caller.
    fn merged(&self) -> f64 {
        [self.keyword, self.recent, self.vector]
            .into_iter()
            .flatten()
            .fold(f64::NEG_INFINITY, f64::max)
    }

    /// Highest-scoring leg name, for a one-word "where did this come from".
    /// `None` for an empty [`LegScores`], which callers never construct.
    pub fn dominant(&self) -> Option<&'static str> {
        [
            ("keyword", self.keyword),
            ("recent", self.recent),
            ("vector", self.vector),
        ]
        .into_iter()
        .filter_map(|(name, s)| s.map(|s| (name, s)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(name, _)| name)
    }
}

/// Per-node phase-1 leg provenance, keyed by node id.
///
/// A node absent from the map was **not** a phase-1 candidate: it reached the
/// result through phase-2 traversal expansion, which has no leg score by
/// construction.
pub type LegProvenance = HashMap<NodeId, LegScores>;

/// Async-gathered inputs for the sync, lock-safe [`candidates`] step.
///
/// This is the only channel by which store I/O reaches phase 1: [`gather`]
/// produces it before any graph lock is taken, and [`candidates`] consumes it
/// under the lock without touching the store.
#[derive(Debug, Default)]
pub struct Phase1Input {
    /// Vector-leg candidates straight from the store (empty when the capability
    /// is absent or no query embedding was available).
    pub vector: Vec<Scored<NodeId>>,
}

/// Gather phase-1 store I/O BEFORE taking any graph lock.
///
/// The only I/O performed is the vector leg, and only when BOTH the store
/// advertises [`Capabilities::VECTOR_SEARCH`] and a query embedding is
/// available. Every other path makes zero async store calls and logs exactly
/// one line (RAM-tier promise, spec §3.2). A capability-present store that
/// errors propagates [`StoreError`]: swallowing a real backend failure into
/// empty candidates would silently poison recall.
pub async fn gather(
    store: &dyn GraphStore,
    session: &SessionId,
    embedding: Option<(&[f32], &EmbeddingContract)>,
    limit: usize,
) -> Result<Phase1Input, StoreError> {
    validate_vector_candidate_limit(limit)?;
    if !store.capabilities().contains(Capabilities::VECTOR_SEARCH) {
        tracing::debug!(
            target: "lambo::recall",
            "phase-1 vector leg disabled: store lacks VECTOR_SEARCH; zero store I/O (RAM-tier promise)"
        );
        return Ok(Phase1Input::default());
    }
    let Some((emb, expected_contract)) = embedding else {
        tracing::debug!(
            target: "lambo::recall",
            "phase-1 vector leg skipped: no query embedding available"
        );
        return Ok(Phase1Input::default());
    };
    let vector = store
        .vector_candidates_checked(session, emb, expected_contract, limit)
        .await?;
    Ok(Phase1Input { vector })
}

/// Phase-1 candidates: the deterministic union of the keyword, recent, and
/// vector legs (see module docs for the merge rule).
///
/// Pure and lock-safe: reads only `graph` and `index` (already guarded by the
/// caller) plus the previously gathered `input`; performs no I/O.
pub fn candidates(
    graph: &Graph,
    index: &InvertedIndex,
    input: Phase1Input,
    query: &str,
    limit: usize,
) -> Vec<Scored<NodeId>> {
    candidates_with_legs(graph, index, input, query, limit).0
}

/// [`candidates`], plus the per-leg scores the max-merge would otherwise
/// discard (I1).
///
/// One implementation, two views: the merge rule lives here and `candidates`
/// projects it, so the ranked list and the leg provenance can never describe
/// different arithmetic.
pub fn candidates_with_legs(
    graph: &Graph,
    index: &InvertedIndex,
    input: Phase1Input,
    query: &str,
    limit: usize,
) -> (Vec<Scored<NodeId>>, LegProvenance) {
    // Phase 1 is a candidate OVER-approximation: the final `top_k` truncation
    // is applied downstream in phase-3 assembly (by final score). Prematurely
    // truncating here let the flat-scored recent/vector legs evict strong
    // keyword matches (GPT5.6sol P1-3), so keyword is over-fetched (bounded)
    // and the union is returned unreduced.
    let keyword_cap = limit.saturating_mul(KEYWORD_OVERFETCH);
    let mut legs: LegProvenance = HashMap::new();
    for s in index.search(query, keyword_cap) {
        legs.entry(s.item).or_default().keyword = Some(s.score);
    }
    for id in recent_concepts(graph) {
        legs.entry(id).or_default().recent = Some(RECENT_SCORE);
    }
    for s in input.vector {
        merge_max(&mut legs.entry(s.item).or_default().vector, s.score);
    }
    let out = rank(&legs);
    // T9 instrumentation: which phase-1 leg(s) produced each candidate, so a
    // trace-enabled run can say whether the lexical (keyword), recent, or
    // vector arm produced an identifier-shaped hit. Unchanged output; it now
    // reads the always-collected map instead of building a second one.
    if tracing::enabled!(target: "lambo::recall", tracing::Level::TRACE) {
        for s in &out {
            let arm_vec = arm_names(legs.get(&s.item).copied().unwrap_or_default());
            tracing::trace!(
                target: "lambo::recall",
                phase = "candidates",
                node = %s.item,
                score = s.score,
                query = %query,
                arms = ?arm_vec,
                "phase-1 candidate arms={:?} score={} query={}",
                arm_vec,
                s.score,
                query
            );
        }
    }
    (out, legs)
}

/// Leg names in the order T9's trace printed them (sorted, deduplicated).
fn arm_names(legs: LegScores) -> Vec<&'static str> {
    // Sorted by construction: "keyword" < "recent" < "vector".
    [
        ("keyword", legs.keyword.is_some()),
        ("recent", legs.recent.is_some()),
        ("vector", legs.vector.is_some()),
    ]
    .into_iter()
    .filter_map(|(name, present)| present.then_some(name))
    .collect()
}

/// Max-merge and sort: score descending, ties by node id ascending.
///
/// A total order (f64 `total_cmp`, then UUID), so the output is deterministic
/// regardless of leg iteration or store result order.
fn rank(legs: &LegProvenance) -> Vec<Scored<NodeId>> {
    let mut out: Vec<Scored<NodeId>> = legs
        .iter()
        .map(|(item, legs)| Scored::new(*item, legs.merged()))
        .collect();
    out.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.item.0.cmp(&b.item.0))
    });
    out
}

/// Phase-1 candidates for the RECENT + VECTOR legs only (GPT5.6sol P2-6).
///
/// Used when no inverted index is installed: the keyword leg is unavailable,
/// but the independently gathered recent and vector candidates must not be
/// discarded. Same merge/sort rule as [`candidates`] minus the keyword leg.
pub fn candidates_without_keyword(graph: &Graph, input: Phase1Input) -> Vec<Scored<NodeId>> {
    candidates_without_keyword_with_legs(graph, input).0
}

/// [`candidates_without_keyword`], plus per-leg scores (I1). The `keyword` leg
/// is `None` throughout — there is no index to ask.
pub fn candidates_without_keyword_with_legs(
    graph: &Graph,
    input: Phase1Input,
) -> (Vec<Scored<NodeId>>, LegProvenance) {
    let mut legs: LegProvenance = HashMap::new();
    for id in recent_concepts(graph) {
        legs.entry(id).or_default().recent = Some(RECENT_SCORE);
    }
    for s in input.vector {
        merge_max(&mut legs.entry(s.item).or_default().vector, s.score);
    }
    let out = rank(&legs);
    (out, legs)
}

/// Fold `score` into a leg slot by **max**, the same rule the cross-leg merge
/// uses.
///
/// Only the vector leg needs this, and it needs it for a reason that is easy to
/// lose: `vector_candidates` is a trait method whose contract does not forbid an
/// adapter returning the same `NodeId` twice, and a plain assignment makes the
/// *last* duplicate win. That silently reversed the pre-I1 arithmetic — a
/// `[(dup, 0.90), (dup, 0.10)]` input ranked at 0.1 where it used to rank at 0.9.
/// The keyword leg assigns rather than max-merges because that is what it did
/// before I1 too (the inverted index yields each id once), and the recency leg is
/// idempotent by construction; changing either would be the same kind of silent
/// arithmetic change in the other direction.
fn merge_max(slot: &mut Option<f64>, score: f64) {
    *slot = Some(match *slot {
        Some(previous) => previous.max(score),
        None => score,
    });
}

/// Ids of the concepts owned by the [`RECENT_INTERACTIONS`] most recent
/// interactions (by `created_at`; ties broken by node id ascending).
fn recent_concepts(graph: &Graph) -> Vec<NodeId> {
    let mut recent: Vec<&crate::types::Interaction> = graph.interactions().collect();
    recent.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.id.0.cmp(&b.id.0))
    });
    let keep: HashSet<NodeId> = recent
        .iter()
        .take(RECENT_INTERACTIONS)
        .map(|i| i.id)
        .collect();
    graph
        .concepts()
        .filter(|c| keep.contains(&c.origin_interaction))
        .map(|c| c.id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::capture_logs;
    use chrono::{DateTime, TimeZone, Utc};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use uuid::Uuid;

    fn ts(minutes: i64) -> DateTime<Utc> {
        let base = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
        base + chrono::Duration::minutes(minutes)
    }

    fn sid() -> SessionId {
        SessionId::from("test-session")
    }

    fn interaction(id: u64, prev: Option<NodeId>, at_min: i64) -> crate::types::Interaction {
        crate::types::Interaction {
            id: NodeId(Uuid::from_u64_pair(1, id)),
            session_id: sid(),
            agent_id: crate::types::AgentId::from("agent-a"),
            prompt_text: Some(format!("prompt {id}")),
            previous_id: prev,
            created_at: ts(at_min),
        }
    }

    fn concept(id: u64, origin: NodeId, content: &str) -> crate::types::Concept {
        crate::types::Concept {
            id: NodeId(Uuid::from_u64_pair(2, id)),
            session_id: sid(),
            content: content.into(),
            canonical_key: content.into(),
            concept_type: crate::types::ConceptType::Entity,
            origin_interaction: origin,
            origin_agent: crate::types::AgentId::from("agent-a"),
            created_at: ts(0),
            access_count: 0,
            last_accessed: None,
            gc_survived: 0,
            canonization_status: crate::types::CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: None,
            embedding: None,
            chunk_group_id: None,
        }
    }

    /// Graph + index with a 4-interaction chain whose `created_at` values are
    /// deliberately NOT monotonic in chain order: chain tail (i4) is the OLDEST,
    /// so a chain-order implementation would pick {c4, c3, c2} while the
    /// contract (by `created_at`) picks {c2, c3, c1}.
    fn planted_graph() -> (Graph, InvertedIndex) {
        let mut g = Graph::new(sid());
        let i1 = interaction(1, None, 0); // 09:00
        let i2 = interaction(2, Some(i1.id), 120); // 11:00
        let i3 = interaction(3, Some(i2.id), 60); // 10:00
        let i4 = interaction(4, Some(i3.id), -60); // 08:00
        g.insert_interaction(i1.clone()).unwrap();
        g.insert_interaction(i2.clone()).unwrap();
        g.insert_interaction(i3.clone()).unwrap();
        g.insert_interaction(i4.clone()).unwrap();
        for (id, origin, content) in [
            (1u64, i1.id, "alpha"),
            (2, i2.id, "beta"),
            (3, i3.id, "gamma"),
            (4, i4.id, "delta"),
        ] {
            g.insert_concept(concept(id, origin, content), origin)
                .unwrap();
        }
        let index = InvertedIndex::from_snapshot(&g.snapshot());
        (g, index)
    }

    fn ids(results: Vec<Scored<NodeId>>) -> Vec<NodeId> {
        results.into_iter().map(|s| s.item).collect()
    }

    fn contract() -> EmbeddingContract {
        EmbeddingContract {
            kind: "fixture".into(),
            model: Some("fixture-v1".into()),
            dim: 8,
        }
    }

    // -----------------------------------------------------------------------
    // Spy store
    // -----------------------------------------------------------------------

    /// `GraphStore` double: counts every async trait call and returns canned
    /// vector hits when the capability is advertised. Any async method other
    /// than `vector_candidates_checked` that gets called is a contract violation and
    /// panics (the count is asserted independently).
    struct SpyVectorStore {
        caps: Capabilities,
        vector_hits: Vec<Scored<NodeId>>,
        async_calls: AtomicUsize,
        current_contract: Mutex<EmbeddingContract>,
    }

    impl SpyVectorStore {
        fn without_vector() -> Self {
            Self {
                caps: Capabilities::HISTORY,
                vector_hits: Vec::new(),
                async_calls: AtomicUsize::new(0),
                current_contract: Mutex::new(contract()),
            }
        }

        fn with_vector(hits: Vec<Scored<NodeId>>) -> Self {
            Self {
                caps: Capabilities::VECTOR_SEARCH | Capabilities::HISTORY,
                vector_hits: hits,
                async_calls: AtomicUsize::new(0),
                current_contract: Mutex::new(contract()),
            }
        }

        fn async_calls(&self) -> usize {
            self.async_calls.load(Ordering::SeqCst)
        }

        fn change_contract(&self, model: &str) {
            self.current_contract.lock().unwrap().model = Some(model.into());
        }
    }

    impl SpyVectorStore {
        /// Count the call and fail the test: gather must never reach any async
        /// trait method other than `vector_candidates_checked`.
        fn unexpected_call(&self) -> ! {
            self.async_calls.fetch_add(1, Ordering::SeqCst);
            panic!("SpyVectorStore: unexpected async store call");
        }
    }

    #[async_trait::async_trait]
    impl GraphStore for SpyVectorStore {
        async fn init_schema(&self) -> Result<(), StoreError> {
            self.unexpected_call()
        }
        fn capabilities(&self) -> Capabilities {
            self.caps
        }
        async fn flush(
            &self,
            _batch: &crate::types::MutationBatch,
            _token: Option<u64>,
        ) -> Result<(), StoreError> {
            self.unexpected_call()
        }
        async fn load_session(
            &self,
            session: &SessionId,
        ) -> Result<crate::types::GraphSnapshot, StoreError> {
            Ok(crate::types::GraphSnapshot {
                session_id: session.clone(),
                embedding: Some(self.current_contract.lock().unwrap().clone()),
                ..crate::types::GraphSnapshot::default()
            })
        }
        async fn keyword_candidates(
            &self,
            _session: &SessionId,
            _tokens: &[String],
            _limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.unexpected_call()
        }
        async fn vector_candidates(
            &self,
            _session: &SessionId,
            _embedding: &[f32],
            _limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.unexpected_call()
        }
        async fn vector_candidates_checked(
            &self,
            _session: &SessionId,
            _embedding: &[f32],
            expected_contract: &EmbeddingContract,
            _limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.async_calls.fetch_add(1, Ordering::SeqCst);
            self.current_contract
                .lock()
                .unwrap()
                .ensure_compatible(expected_contract)
                .map_err(|err| StoreError::Invariant(err.to_string()))?;
            Ok(self.vector_hits.clone())
        }
        async fn blast_radius(
            &self,
            _session: &SessionId,
            _node: NodeId,
            _min_edge_age: std::time::Duration,
            _now: chrono::DateTime<chrono::Utc>,
        ) -> Result<u64, StoreError> {
            self.unexpected_call()
        }
        async fn interaction_span(
            &self,
            _session: &SessionId,
            _node: NodeId,
            _min_age: std::time::Duration,
            _now: chrono::DateTime<chrono::Utc>,
        ) -> Result<crate::types::InteractionSpan, StoreError> {
            self.unexpected_call()
        }
        async fn record_canonization(
            &self,
            _event: &crate::types::CanonizationEvent,
            _token: Option<u64>,
        ) -> Result<(), StoreError> {
            self.unexpected_call()
        }
    }

    // -----------------------------------------------------------------------
    // gather: capability gate
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn gather_without_capability_zero_store_calls_one_log() {
        let store = SpyVectorStore::without_vector();
        let (logs, _guard) = capture_logs(tracing::Level::TRACE);

        let contract = contract();
        let input = gather(&store, &sid(), Some((&[0.5; 8], &contract)), 5)
            .await
            .expect("absent capability degrades, not errors");
        assert!(input.vector.is_empty());
        assert_eq!(store.async_calls(), 0, "zero async store calls (RAM-tier)");

        let lines = logs.lines();
        assert_eq!(lines.len(), 1, "exactly one log line, got {lines:?}");
        assert!(
            lines[0].contains("VECTOR_SEARCH"),
            "log names the capability"
        );
    }

    #[tokio::test]
    async fn gather_with_capability_calls_vector_and_merges() {
        let (graph, index) = planted_graph();
        let c1 = NodeId(Uuid::from_u64_pair(2, 1));
        let c2 = NodeId(Uuid::from_u64_pair(2, 2));
        let c3 = NodeId(Uuid::from_u64_pair(2, 3));
        let store = SpyVectorStore::with_vector(vec![Scored::new(c2, 0.8)]);
        let (logs, _guard) = capture_logs(tracing::Level::TRACE);

        let contract = contract();
        let input = gather(&store, &sid(), Some((&[0.5; 8], &contract)), 5)
            .await
            .expect("capability present");
        assert_eq!(input.vector.len(), 1);
        assert_eq!(store.async_calls(), 1, "exactly one store I/O call");
        assert!(logs.lines().is_empty(), "no capability-absent log");

        // "beta" is the keyword hit for query "beta"; c2 is BOTH the keyword
        // hit and the vector hit -> max-merge keeps the BM25 score, and the
        // recent leg (c1/c2/c3 at RECENT_SCORE) must not lower it.
        let out = candidates(&graph, &index, input, "beta", 10);
        // Union: c2 (keyword + vector) first, then the recent-leg members c1/c3
        // at the flat recent score, ties by id asc.
        assert_eq!(ids(out.clone()), vec![c2, c1, c3]);
        let s = out[0].score;
        assert!(s > 0.8, "max-merge keeps the higher BM25 score, got {s}");
    }

    #[tokio::test]
    async fn h1_contract_change_between_initial_load_and_candidate_read_returns_no_rankings() {
        let hit = Scored::new(NodeId(Uuid::from_u64_pair(9, 1)), 0.99);
        let store = SpyVectorStore::with_vector(vec![hit]);

        // This is the reader's startup snapshot/check at contract A.
        let loaded = store.load_session(&sid()).await.unwrap();
        let expected = loaded.embedding.unwrap();
        assert_eq!(expected.model.as_deref(), Some("fixture-v1"));

        // Deterministically model the writer's atomic A -> B contract/vector
        // commit while the reader is awaiting query embedding.
        store.change_contract("fixture-v2");
        let err = gather(&store, &sid(), Some((&[0.5; 8], &expected)), 5)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("fixture-v2"), "{err}");
        assert_eq!(store.async_calls(), 1);
    }

    #[tokio::test]
    async fn gather_with_capability_but_no_embedding_skips_vector() {
        let store = SpyVectorStore::with_vector(vec![Scored::new(NodeId::nil(), 0.9)]);
        let (logs, _guard) = capture_logs(tracing::Level::TRACE);

        let input = gather(&store, &sid(), None, 5)
            .await
            .expect("missing embedding degrades, not errors");
        assert!(input.vector.is_empty());
        assert_eq!(store.async_calls(), 0, "no embedding -> no store I/O");

        let lines = logs.lines();
        assert_eq!(lines.len(), 1, "exactly one log line, got {lines:?}");
        assert!(
            lines[0].contains("no query embedding"),
            "log explains the skip"
        );
    }

    #[tokio::test]
    async fn gather_rejects_oversized_top_k_before_store_io() {
        let store = SpyVectorStore::with_vector(Vec::new());
        let contract = contract();
        let err = gather(
            &store,
            &sid(),
            Some((&[0.5; 8], &contract)),
            crate::store::MAX_VECTOR_CANDIDATE_LIMIT + 1,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, StoreError::Invariant(_)));
        assert_eq!(
            store.async_calls(),
            0,
            "invalid top_k performs no store I/O"
        );
    }

    // -----------------------------------------------------------------------
    // candidates: recent-interactions leg
    // -----------------------------------------------------------------------

    #[test]
    fn recent_leg_keeps_only_three_most_recent_by_created_at() {
        let (graph, index) = planted_graph();
        // Chain order i1(09:00) -> i2(11:00) -> i3(10:00) -> i4(08:00); the
        // 3 most recent BY CREATED_AT are i2, i3, i1 -> concepts 2, 3, 1.
        let out = candidates(&graph, &index, Phase1Input::default(), "zzz", 10);
        let expected = vec![
            NodeId(Uuid::from_u64_pair(2, 1)),
            NodeId(Uuid::from_u64_pair(2, 2)),
            NodeId(Uuid::from_u64_pair(2, 3)),
        ];
        assert_eq!(ids(out.clone()), expected);
        assert!(
            out.iter().all(|s| s.score == RECENT_SCORE),
            "recent members carry the flat score"
        );
    }

    /// G2: the F BGE-M3 evidence found the right durable vector at 0.3991,
    /// below the former 0.50 recent floor. A recent leg must remain secondary:
    /// that real semantic ranking has to survive the phase-1 max merge rather
    /// than tie by node id with every young concept.
    #[test]
    fn true_vector_hit_below_the_former_recent_floor_surfaces_over_recency() {
        let (graph, index) = planted_graph();
        let target = NodeId(Uuid::from_u64_pair(2, 3));
        let out = candidates(
            &graph,
            &index,
            Phase1Input {
                vector: vec![Scored::new(target, 0.3991)],
            },
            "changes that do not break existing clients",
            3,
        );

        assert_eq!(out[0].item, target, "the semantic hit must surface first");
        assert_eq!(out[0].score, 0.3991, "max merge must retain vector lift");
        assert!(
            out.iter()
                .filter(|candidate| candidate.item != target)
                .all(|candidate| candidate.score == RECENT_SCORE),
            "unrelated recent concepts retain only the secondary score"
        );
        assert!(
            RECENT_SCORE < out[0].score,
            "the calibrated recent floor must not mask the recorded true hit"
        );
    }

    #[test]
    fn recent_ties_break_by_node_id() {
        let mut g = Graph::new(sid());
        let i1 = interaction(1, None, 60);
        let i2 = interaction(2, Some(i1.id), 60);
        let i3 = interaction(3, Some(i2.id), 60); // ties i2 on created_at
        let i4 = interaction(4, Some(i3.id), 120);
        g.insert_interaction(i1.clone()).unwrap();
        g.insert_interaction(i2.clone()).unwrap();
        g.insert_interaction(i3.clone()).unwrap();
        g.insert_interaction(i4.clone()).unwrap();
        for (id, origin, content) in [
            (1u64, i1.id, "alpha"),
            (2, i2.id, "beta"),
            (3, i3.id, "gamma"),
            (4, i4.id, "delta"),
        ] {
            g.insert_concept(concept(id, origin, content), origin)
                .unwrap();
        }
        let index = InvertedIndex::from_snapshot(&g.snapshot());

        // i4 (120) is strictly most recent; i1/i2/i3 tie at 60. Recent =
        // i4 + {i1, i2} (tie broken by id asc) -> concepts c4, c1, c2; final
        // output is score-desc (all RECENT_SCORE) then id asc -> c1, c2, c4. A
        // chain-order or id-desc selection would yield {c4, c3, c2}.
        let out = ids(candidates(&g, &index, Phase1Input::default(), "zzz", 10));
        let expected = vec![
            NodeId(Uuid::from_u64_pair(2, 1)),
            NodeId(Uuid::from_u64_pair(2, 2)),
            NodeId(Uuid::from_u64_pair(2, 4)),
        ];
        assert_eq!(out, expected);
    }

    #[test]
    fn empty_graph_and_zero_limit_yield_nothing() {
        let g = Graph::new(sid());
        let index = InvertedIndex::new();
        assert!(candidates(&g, &index, Phase1Input::default(), "user", 10).is_empty());
        assert!(candidates(&g, &index, Phase1Input::default(), "zzz", 0).is_empty());
    }

    // -----------------------------------------------------------------------
    // Union: determinism
    // -----------------------------------------------------------------------

    #[cfg(feature = "fixtures")]
    #[test]
    fn union_is_deterministic_across_input_orders() {
        let snap = load_rest_api_fixture();
        let graph = Graph::from_snapshot(snap.clone()).expect("fixture loads");
        let index = InvertedIndex::from_snapshot(&snap);
        let c5 = NodeId(Uuid::from_u64_pair(2, 5));
        let c9 = NodeId(Uuid::from_u64_pair(2, 9));

        // Same store results in different orders must produce identical output.
        let a = candidates(
            &graph,
            &index,
            Phase1Input {
                vector: vec![Scored::new(c5, 0.8), Scored::new(c9, 0.7)],
            },
            "pagination",
            10,
        );
        let b = candidates(
            &graph,
            &index,
            Phase1Input {
                vector: vec![Scored::new(c9, 0.7), Scored::new(c5, 0.8)],
            },
            "pagination",
            10,
        );
        let c = candidates(
            &graph,
            &index,
            Phase1Input {
                vector: vec![Scored::new(c5, 0.8), Scored::new(c9, 0.7)],
            },
            "pagination",
            10,
        );
        assert_eq!(a, b);
        assert_eq!(a, c);

        // Full determinism: identical runs, identical output.
        let d = candidates(&graph, &index, Phase1Input::default(), "pagination", 5);
        let e = candidates(&graph, &index, Phase1Input::default(), "pagination", 5);
        assert_eq!(d, e);
    }

    #[cfg(feature = "fixtures")]
    #[test]
    fn vector_leg_merges_below_keyword_above_recent() {
        let snap = load_rest_api_fixture();
        let graph = Graph::from_snapshot(snap.clone()).expect("fixture loads");
        let index = InvertedIndex::from_snapshot(&snap);
        let c5 = NodeId(Uuid::from_u64_pair(2, 5)); // vector-only member
        let c8 = NodeId(Uuid::parse_str("f0000000-0000-4000-8000-000000001008").unwrap());
        let c9 = NodeId(Uuid::parse_str("f0000000-0000-4000-8000-000000001009").unwrap());
        let c10 = NodeId(Uuid::parse_str("f0000000-0000-4000-8000-000000001010").unwrap());
        let c11 = NodeId(Uuid::parse_str("f0000000-0000-4000-8000-000000001011").unwrap());
        let c12 = NodeId(Uuid::parse_str("f0000000-0000-4000-8000-000000001012").unwrap());

        let out = candidates(
            &graph,
            &index,
            Phase1Input {
                vector: vec![
                    Scored::new(c8, 0.9), // keyword hit too: max-merge keeps BM25
                    Scored::new(c5, 0.8), // vector-only member
                ],
            },
            "pagination",
            10,
        );
        assert_eq!(
            ids(out.clone()),
            vec![c8, c5, c9, c10, c11, c12],
            "keyword first, then vector 0.8, then recent leg, ties by id"
        );
        let score8 = out.iter().find(|s| s.item == c8).unwrap().score;
        assert!(score8 > 0.9, "max-merge keeps the BM25 score, got {score8}");
    }

    // -----------------------------------------------------------------------
    // Golden keyword-leg reconciliation
    // -----------------------------------------------------------------------

    #[cfg(feature = "fixtures")]
    fn load_rest_api_fixture() -> crate::types::GraphSnapshot {
        use crate::fixtures;
        fixtures::load_snapshot("session-rest-api").expect("fixture loads")
    }

    /// The golden `phase1_candidates` list asserts the KEYWORD leg EXACTLY (set
    /// and order). We verify the same through the union: restricting the union
    /// to non-recent members must reproduce the golden list, and the recent
    /// members must account for exactly the rest (the fixture carries no
    /// embeddings, so the vector leg is empty).
    #[cfg(feature = "fixtures")]
    #[test]
    fn golden_keyword_leg_exact_within_union() {
        use crate::fixtures;
        let snap = fixtures::load_snapshot("session-rest-api").unwrap();
        let graph = Graph::from_snapshot(snap.clone()).expect("fixture loads");
        let index = InvertedIndex::from_snapshot(&snap);
        let goldens = fixtures::load_recall_goldens().unwrap();

        let mut recent: Vec<&crate::types::Interaction> = snap.interactions.iter().collect();
        recent.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| a.id.0.cmp(&b.id.0))
        });
        let recent_interactions: HashSet<NodeId> = recent
            .iter()
            .take(RECENT_INTERACTIONS)
            .map(|i| i.id)
            .collect();
        let recent_concept_ids: HashSet<NodeId> = snap
            .concepts
            .iter()
            .filter(|c| recent_interactions.contains(&c.origin_interaction))
            .map(|c| c.id)
            .collect();
        assert!(
            !recent_concept_ids.is_empty(),
            "fixture must exercise the recent leg"
        );

        let cases = goldens["cases"].as_array().expect("golden cases array");
        for case in cases {
            let query = case["query"].as_str().expect("golden query");
            let top_k = case["top_k"].as_u64().expect("golden top_k") as usize;
            let expected: Vec<NodeId> = case["phase1_candidates"]
                .as_array()
                .expect("golden phase1_candidates")
                .iter()
                .map(|v| serde_json::from_value(v.clone()).expect("parse NodeId"))
                .collect();

            let union = candidates(&graph, &index, Phase1Input::default(), query, top_k);
            let union_ids: Vec<NodeId> = ids(union);

            // 1. Every golden (keyword) member survives in the union...
            for want in &expected {
                assert!(
                    union_ids.contains(want),
                    "golden keyword member {want} missing from union for {query:?}"
                );
            }
            // 2. ...and the non-recent members re-sort EXACTLY to the golden
            //    list (set AND order) — the union never reorders keyword hits.
            let keyword_members: Vec<NodeId> = union_ids
                .iter()
                .copied()
                .filter(|id| !recent_concept_ids.contains(id))
                .collect();
            assert_eq!(
                keyword_members, expected,
                "keyword leg must be golden-exact for query {query:?}"
            );
            // 3. The recent members are exactly the rest (no vector leg: the
            //    fixture carries no embeddings).
            let recent_members: Vec<NodeId> = union_ids
                .iter()
                .copied()
                .filter(|id| recent_concept_ids.contains(id))
                .collect();
            let mut recent_expected: Vec<NodeId> = recent_concept_ids.iter().copied().collect();
            recent_expected.sort_by_key(|a| a.0);
            assert_eq!(
                recent_members, recent_expected,
                "recent-leg members for query {query:?}"
            );
        }
    }

    // P1-3 (GPT5.6sol): a bounded recent-interactions leg must not evict every
    // strong lexical match at phase-1 truncation. With keyword over-fetch and
    // NO union truncation in `candidates`, all keyword matches survive even
    // when an unrelated recent leg is present.
    #[test]
    fn recent_leg_does_not_evict_keyword_matches() {
        let mut g = Graph::new(sid());
        // One recent interaction owning 8 unrelated concepts (the recent leg).
        let ri = interaction(100, None, 0);
        g.insert_interaction(ri.clone()).unwrap();
        for k in 0..8 {
            g.insert_concept(concept(200 + k, ri.id, &format!("recent-{k}")), ri.id)
                .unwrap();
        }
        // A second interaction owning 20 concepts whose content matches "user".
        let ki = interaction(101, Some(ri.id), 1);
        g.insert_interaction(ki.clone()).unwrap();
        let mut matched = Vec::new();
        for k in 0..20 {
            g.insert_concept(concept(300 + k, ki.id, &format!("user topic {k}")), ki.id)
                .unwrap();
            matched.push(NodeId(Uuid::from_u64_pair(2, 300 + k)));
        }
        let index = InvertedIndex::from_snapshot(&g.snapshot());

        let out = candidates(&g, &index, Phase1Input::default(), "user", 5);
        let ids: Vec<NodeId> = out.iter().map(|s| s.item).collect();
        // Every keyword match survives (over-fetch, no truncation).
        for want in &matched {
            assert!(
                ids.contains(want),
                "keyword match {want} must survive in the phase-1 union"
            );
        }
        // And the recent concepts are also present (union not keyword-only).
        for k in 0..8 {
            assert!(ids.contains(&NodeId(Uuid::from_u64_pair(2, 200 + k))));
        }
    }

    // -----------------------------------------------------------------------
    // I1 — per-leg provenance
    // -----------------------------------------------------------------------

    /// **I1.** The per-leg scores survive the max-merge, and the merged score
    /// the ranking uses is exactly the maximum over the legs that fired.
    ///
    /// This is the distinction the whole of DOGFOOD metric 4 rests on: a `0.35`
    /// from the recency floor and a genuine `0.35` cosine rank identically and
    /// mean opposite things, and before I1 the ledger could not have told them
    /// apart because `candidates` returned one `f64`.
    #[test]
    fn i1_per_leg_scores_survive_the_max_merge() {
        let (g, index) = planted_graph();
        // A vector hit on a node the keyword leg also finds, plus one only the
        // vector leg finds.
        // "alpha" is concept 1, which the keyword leg finds AND which belongs
        // to one of the three most recent interactions — so all three legs fire
        // on one node, with the vector leg deliberately the weakest.
        let shared = NodeId(Uuid::from_u64_pair(2, 1));
        let vector_only = NodeId(Uuid::from_u64_pair(9, 77));
        let input = Phase1Input {
            vector: vec![Scored::new(shared, 0.42), Scored::new(vector_only, 0.91)],
        };

        let (ranked, legs) = candidates_with_legs(&g, &index, input, "alpha", 5);

        // Ranking is unchanged by collecting provenance.
        let plain = candidates(
            &g,
            &index,
            Phase1Input {
                vector: vec![Scored::new(shared, 0.42), Scored::new(vector_only, 0.91)],
            },
            "alpha",
            5,
        );
        let ranked_pairs: Vec<(NodeId, f64)> = ranked.iter().map(|s| (s.item, s.score)).collect();
        let plain_pairs: Vec<(NodeId, f64)> = plain.iter().map(|s| (s.item, s.score)).collect();
        assert_eq!(
            ranked_pairs, plain_pairs,
            "candidates() must be exactly the projection of candidates_with_legs()"
        );

        // Every ranked node's score is the max over its own legs — stated as an
        // invariant over the whole result, not spot-checked on one node.
        for s in &ranked {
            let l = legs.get(&s.item).expect("every ranked node has legs");
            let max = [l.keyword, l.recent, l.vector]
                .into_iter()
                .flatten()
                .fold(f64::NEG_INFINITY, f64::max);
            assert_eq!(
                s.score, max,
                "the merged score is the maximum over the legs that fired: {:?}",
                l
            );
        }

        // The vector-only node reports the vector leg and nothing else.
        let l = legs.get(&vector_only).expect("vector-only candidate");
        assert_eq!(l.vector, Some(0.91));
        assert_eq!(l.keyword, None, "the keyword leg did not produce it");
        assert_eq!(l.recent, None, "nor the recency leg");
        assert_eq!(l.dominant(), Some("vector"));

        // The shared node keeps BOTH numbers: the higher one won the ranking,
        // and the lower one is still recoverable — which is the point.
        let l = legs.get(&shared).expect("shared candidate");
        assert_eq!(
            l.vector,
            Some(0.42),
            "the losing vector score is retained, not overwritten by the merge: {l:?}"
        );
        assert!(
            l.keyword.is_some() || l.recent.is_some(),
            "the shared node must also carry the leg that outscored the vector one: {l:?}"
        );

        // A DUPLICATE id inside the vector leg max-merges rather than
        // last-writes. The `vector_candidates` trait contract does not forbid
        // duplicates and no shipped adapter emits them, so this is the arithmetic
        // no test held down: a plain assignment made `[(dup, 0.90), (dup, 0.10)]`
        // rank at 0.1, reversing what the pre-I1 merge did with the same input.
        let dup = NodeId(Uuid::from_u64_pair(11, 42));
        let (ranked, legs) = candidates_with_legs(
            &g,
            &index,
            Phase1Input {
                vector: vec![Scored::new(dup, 0.90), Scored::new(dup, 0.10)],
            },
            "zzzznomatch",
            5,
        );
        let l = legs.get(&dup).expect("the duplicated id is a candidate");
        assert_eq!(
            l.vector,
            Some(0.90),
            "duplicate vector entries max-merge; the later, weaker score must not \
             overwrite the stronger one: {l:?}"
        );
        let scored = ranked
            .iter()
            .find(|s| s.item == dup)
            .expect("the duplicated id is ranked");
        assert_eq!(
            scored.score, 0.90,
            "and the ranking sees the merged maximum, not the last write"
        );
        assert_eq!(
            ranked.iter().filter(|s| s.item == dup).count(),
            1,
            "a duplicated input id is still one candidate"
        );
    }

    /// **I1.** The recency floor is distinguishable from a real score of the
    /// same magnitude. A node retrieved by nothing but recency reports
    /// `recent: 0.35` and no other leg.
    #[test]
    fn i1_the_recency_floor_is_distinguishable_from_a_real_hit() {
        let (g, index) = planted_graph();
        // A query that matches nothing lexically, so only the recency leg fires.
        let (ranked, legs) =
            candidates_with_legs(&g, &index, Phase1Input::default(), "zzzznomatch", 5);
        assert!(
            !ranked.is_empty(),
            "the recency leg still yields candidates"
        );
        for s in &ranked {
            let l = legs.get(&s.item).expect("legs");
            assert_eq!(l.recent, Some(RECENT_SCORE), "{l:?}");
            assert_eq!(l.keyword, None, "no lexical match for this query: {l:?}");
            assert_eq!(l.vector, None, "no vector leg was gathered: {l:?}");
            assert_eq!(l.dominant(), Some("recent"));
            assert_eq!(s.score, RECENT_SCORE);
        }
    }

    /// **I1.** The keyword-less path (no inverted index installed) still
    /// reports its two legs.
    #[test]
    fn i1_the_keywordless_path_reports_its_legs_too() {
        let (g, _index) = planted_graph();
        let vector_only = NodeId(Uuid::from_u64_pair(9, 78));
        let (ranked, legs) = candidates_without_keyword_with_legs(
            &g,
            Phase1Input {
                vector: vec![Scored::new(vector_only, 0.8)],
            },
        );
        assert_eq!(ranked.first().map(|s| s.item), Some(vector_only));
        assert_eq!(legs.get(&vector_only).and_then(|l| l.vector), Some(0.8));
        for l in legs.values() {
            assert_eq!(
                l.keyword, None,
                "there is no index to ask, so the keyword leg is never set: {l:?}"
            );
        }
    }
}
