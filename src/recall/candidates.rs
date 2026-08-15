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
//! 3. **Vector** — [`GraphStore::vector_candidates`], only when the store
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
use crate::types::{NodeId, Scored, SessionId, StoreError};

/// Number of most-recent interactions whose concepts join phase 1 (spec §8).
pub const RECENT_INTERACTIONS: usize = 3;

/// Flat phase-1 score for recent-interaction-leg members.
///
/// Chosen as a secondary signal below a genuine BM25 hit or store similarity
/// score (both are typically > 0.5 for a match worth returning); the exact
/// value is not spec-pinned, only its rank position relative to real matches.
pub const RECENT_SCORE: f64 = 0.5;

/// Phase-1 keyword over-fetch multiplier: the keyword leg is over-fetched so
/// strong query matches cannot be evicted by the flat-scored recent/vector
/// legs at phase-1 truncation (GPT5.6sol P1-3).
pub(crate) const KEYWORD_OVERFETCH: usize = 4;

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
    embedding: Option<&[f32]>,
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
    let Some(emb) = embedding else {
        tracing::debug!(
            target: "lambo::recall",
            "phase-1 vector leg skipped: no query embedding available"
        );
        return Ok(Phase1Input::default());
    };
    let vector = store.vector_candidates(session, emb, limit).await?;
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
    // Phase 1 is a candidate OVER-approximation: the final `top_k` truncation
    // is applied downstream in phase-3 assembly (by final score). Prematurely
    // truncating here let the flat-scored recent/vector legs evict strong
    // keyword matches (GPT5.6sol P1-3), so keyword is over-fetched (bounded)
    // and the union is returned unreduced.
    let keyword_cap = limit.saturating_mul(KEYWORD_OVERFETCH);
    let mut merged: HashMap<NodeId, f64> = HashMap::new();
    for s in index.search(query, keyword_cap) {
        merged.insert(s.item, s.score);
    }
    for id in recent_concepts(graph) {
        merged
            .entry(id)
            .and_modify(|v| *v = v.max(RECENT_SCORE))
            .or_insert(RECENT_SCORE);
    }
    for s in input.vector {
        merged
            .entry(s.item)
            .and_modify(|v| *v = v.max(s.score))
            .or_insert(s.score);
    }
    let mut out: Vec<Scored<NodeId>> = merged
        .into_iter()
        .map(|(item, score)| Scored::new(item, score))
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
    let mut merged: HashMap<NodeId, f64> = HashMap::new();
    for id in recent_concepts(graph) {
        merged
            .entry(id)
            .and_modify(|v| *v = v.max(RECENT_SCORE))
            .or_insert(RECENT_SCORE);
    }
    for s in input.vector {
        merged
            .entry(s.item)
            .and_modify(|v| *v = v.max(s.score))
            .or_insert(s.score);
    }
    let mut out: Vec<Scored<NodeId>> = merged
        .into_iter()
        .map(|(item, score)| Scored::new(item, score))
        .collect();
    out.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.item.0.cmp(&b.item.0))
    });
    out
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

    // -----------------------------------------------------------------------
    // Spy store
    // -----------------------------------------------------------------------

    /// `GraphStore` double: counts every async trait call and returns canned
    /// vector hits when the capability is advertised. Any async method other
    /// than `vector_candidates` that gets called is a contract violation and
    /// panics (the count is asserted independently).
    struct SpyVectorStore {
        caps: Capabilities,
        vector_hits: Vec<Scored<NodeId>>,
        async_calls: AtomicUsize,
    }

    impl SpyVectorStore {
        fn without_vector() -> Self {
            Self {
                caps: Capabilities::HISTORY,
                vector_hits: Vec::new(),
                async_calls: AtomicUsize::new(0),
            }
        }

        fn with_vector(hits: Vec<Scored<NodeId>>) -> Self {
            Self {
                caps: Capabilities::VECTOR_SEARCH | Capabilities::HISTORY,
                vector_hits: hits,
                async_calls: AtomicUsize::new(0),
            }
        }

        fn async_calls(&self) -> usize {
            self.async_calls.load(Ordering::SeqCst)
        }
    }

    impl SpyVectorStore {
        /// Count the call and fail the test: gather must never reach any async
        /// trait method other than `vector_candidates`.
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
            _session: &SessionId,
        ) -> Result<crate::types::GraphSnapshot, StoreError> {
            self.unexpected_call()
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
            self.async_calls.fetch_add(1, Ordering::SeqCst);
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

        let input = gather(&store, &sid(), Some(&[0.5; 8]), 5)
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

        let input = gather(&store, &sid(), Some(&[0.5; 8]), 5)
            .await
            .expect("capability present");
        assert_eq!(input.vector.len(), 1);
        assert_eq!(store.async_calls(), 1, "exactly one store I/O call");
        assert!(logs.lines().is_empty(), "no capability-absent log");

        // "beta" is the keyword hit for query "beta"; c2 is BOTH the keyword
        // hit and the vector hit -> max-merge keeps the BM25 score, and the
        // recent leg (c1/c2/c3 at 0.5) must not lower it.
        let out = candidates(&graph, &index, input, "beta", 10);
        // Union: c2 (keyword + vector) first, then the recent-leg members c1/c3
        // at the flat 0.5 score, ties by id asc.
        assert_eq!(ids(out.clone()), vec![c2, c1, c3]);
        let s = out[0].score;
        assert!(s > 0.8, "max-merge keeps the higher BM25 score, got {s}");
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
        let err = gather(
            &store,
            &sid(),
            Some(&[0.5; 8]),
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
        // output is score-desc (all 0.5) then id asc -> c1, c2, c4. A
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
            "keyword first, then vector 0.8, then recent 0.5, ties by id"
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
}
