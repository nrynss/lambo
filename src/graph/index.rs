//! In-memory inverted index over concept content + BM25 scoring (T2.6).
//!
//! Recall phase 1's keyword source (spec §8): concepts are tokenized once at
//! [`InvertedIndex::add`] time and the postings are maintained **incrementally** —
//! add / update / remove never rebuild the index. The only bulk path,
//! [`InvertedIndex::from_snapshot`], is repeated `add` under the hood.
//!
//! Per-session `df`: the index is a per-session structure (the owner, T2.3+,
//! keeps one [`InvertedIndex`] per session); document frequency is computed over
//! the documents currently indexed in it.
//!
//! Concepts only — interactions are not searchable (spec §8 phase 1 searches
//! concept content). [`InvertedIndex`] owns no lock; callers (`Memory`, T2.3+)
//! serialize access.

use std::collections::HashMap;

use crate::graph::canonical::normalize_tokens;
use crate::types::{tie_break_by_key, Concept, GraphSnapshot, NodeId, Scored};

/// BM25 term-frequency saturation (spec-pinned constant).
const BM25_K1: f64 = 1.2;
/// BM25 document-length normalization (spec-pinned constant).
const BM25_B: f64 = 0.75;

/// Inverted index over concept content.
///
/// Terms are produced by [`normalize_tokens`] (the T2.2 canonical-normalizer contract);
/// each term maps to a posting list of `(concept id -> in-document frequency)`.
/// Document lengths (token counts) are kept per concept for BM25's length
/// normalization; totals are maintained for `avgdl`.
#[derive(Debug, Default)]
pub struct InvertedIndex {
    /// term -> posting list: concept id -> in-document term frequency.
    postings: HashMap<String, HashMap<NodeId, usize>>,
    /// concept id -> its token vector (enables cheap remove / re-add).
    doc_tokens: HashMap<NodeId, Vec<String>>,
    /// concept id -> its canonical key, so `search` can break exact score ties
    /// on something stable across runs (issue #2). Mirrors `doc_tokens`'s
    /// lifecycle; no postings are touched for it.
    doc_keys: HashMap<NodeId, String>,
    /// number of indexed concepts (`N` in BM25).
    total_docs: usize,
    /// total tokens across all indexed concepts (feeds BM25 `avgdl`).
    total_tokens: usize,
}

impl InvertedIndex {
    /// Empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Index (or re-index) a concept.
    ///
    /// Idempotent per node id: re-adding an already-indexed id atomically replaces
    /// that concept's postings — a concept may never appear twice in the index.
    pub fn add(&mut self, c: &Concept) {
        if self.doc_tokens.contains_key(&c.id) {
            self.remove(c.id);
        }
        let tokens = normalize_tokens(&c.content);
        for t in &tokens {
            self.postings
                .entry(t.clone())
                .or_default()
                .entry(c.id)
                .and_modify(|tf| *tf += 1)
                .or_insert(1);
        }
        self.total_tokens += tokens.len();
        self.total_docs += 1;
        self.doc_keys.insert(c.id, c.canonical_key.clone());
        self.doc_tokens.insert(c.id, tokens);
    }

    /// Drop a concept from the index (its postings, its length contribution,
    /// its tie-break key). No-op if `id` is not indexed.
    pub fn remove(&mut self, id: NodeId) {
        let Some(tokens) = self.doc_tokens.remove(&id) else {
            return;
        };
        for t in &tokens {
            let term_emptied = match self.postings.get_mut(t) {
                Some(posts) => {
                    posts.remove(&id);
                    posts.is_empty()
                }
                None => continue,
            };
            if term_emptied {
                self.postings.remove(t);
            }
        }
        self.total_tokens -= tokens.len();
        self.total_docs -= 1;
        // The tie-break key rides the same lifecycle as the postings: a
        // removed concept leaves nothing behind (remediation round 1 — the
        // entry used to linger, a leak re-added ids can mask with a stale key).
        self.doc_keys.remove(&id);
    }

    /// Index every concept in a snapshot (bulk-load; incremental `add` underneath).
    pub fn from_snapshot(snap: &GraphSnapshot) -> Self {
        let mut idx = Self::new();
        for c in &snap.concepts {
            idx.add(c);
        }
        idx
    }

    /// BM25 keyword search over concept content.
    ///
    /// The query is tokenized with the **same** tokenizer as documents. Concepts
    /// matching at least one query term are scored (OR semantics, scores summed
    /// across matching terms); the result is score-descending with ties broken by
    /// canonical key ascending, then concept id ascending (`tie_break_by_key`),
    /// truncated to `limit`. Only concepts with a strictly positive score are
    /// returned; there is no zero-score padding.
    ///
    /// Duplicate query tokens count ONCE (query-term frequency is 1 for scoring,
    /// grok G1) — `search("user user")` scores identically to `search("user")`;
    /// document-side duplicates still matter via tf.
    pub fn search(&self, query: &str, limit: usize) -> Vec<Scored<NodeId>> {
        if self.total_docs == 0 {
            return Vec::new();
        }
        let mut terms = normalize_tokens(query);
        terms.sort();
        terms.dedup();
        if terms.is_empty() {
            return Vec::new();
        }
        let n = self.total_docs as f64;
        // Guard (grok G2): a corpus of zero-token documents (stopword-only or
        // empty content) would otherwise make avg_dl = 0 and tf_component NaN.
        // No postings exist in that case so results stay empty either way, but
        // keep the math sound for future callers.
        let avg_dl = if self.total_tokens == 0 {
            1.0
        } else {
            self.total_tokens as f64 / n
        };
        let mut scores: HashMap<NodeId, f64> = HashMap::new();
        for term in terms {
            let Some(posts) = self.postings.get(&term) else {
                continue;
            };
            // Per-session df: number of indexed concepts containing the term.
            let df = posts.len() as f64;
            // BM25+ idf smoothing keeps every matching term's contribution positive.
            let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
            for (&doc, &tf) in posts {
                let dl = self.doc_tokens.get(&doc).map_or(0, |t| t.len()) as f64;
                let tf_component = tf as f64 * (BM25_K1 + 1.0)
                    / (tf as f64 + BM25_K1 * (1.0 - BM25_B + BM25_B * dl / avg_dl));
                *scores.entry(doc).or_insert(0.0) += idf * tf_component;
            }
        }
        let mut results: Vec<Scored<NodeId>> = scores
            .into_iter()
            .filter(|(_, s)| *s > 0.0)
            .map(|(item, score)| Scored { item, score })
            .collect();
        // Exact score ties break on canonical key asc, then id asc (issue #2):
        // the id alone is minted per run. The key map lives on the index (the
        // cheapest source that needs no restructuring), and the lookup only
        // runs inside the tie arm.
        results.sort_by(|a, b| {
            b.score.total_cmp(&a.score).then_with(|| {
                tie_break_by_key(
                    self.doc_keys.get(&a.item).map(String::as_str),
                    &a.item,
                    self.doc_keys.get(&b.item).map(String::as_str),
                    &b.item,
                )
            })
        });
        results.truncate(limit);
        results
    }
}

/// Tokenizer for indexing and querying: the shared canonical normalizer
/// (T2.2, `src/graph/canonical.rs::normalize_tokens`) — lowercase, split on
/// `[-_ ]` + camelCase boundaries, drop stopwords, Porter-stem. Duplicate
/// tokens are retained so in-document term frequency counts occurrences.
/// Imported, never forked (T2.6 phase-doc contract; the local seam copy was
/// removed at integration).
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, TimeZone, Utc};
    use uuid::Uuid;

    fn ts(min: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(min * 60, 0).unwrap()
    }

    fn sid() -> crate::types::SessionId {
        crate::types::SessionId::from("test-session")
    }

    fn concept(id: u64, content: &str) -> Concept {
        Concept {
            id: NodeId(Uuid::from_u64_pair(2, id)),
            session_id: sid(),
            content: content.into(),
            canonical_key: content.into(),
            concept_type: crate::types::ConceptType::Entity,
            origin_interaction: NodeId(Uuid::from_u64_pair(0, 1)),
            origin_agent: crate::types::AgentId::from("agent-a"),
            created_at: ts(0),
            access_count: 0,
            last_accessed: None,
            gc_survived: 0,
            canonization_status: crate::types::CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: None,
            embedding: None,
            human_confirmed: 0,
            chunk_group_id: None,
        }
    }

    fn ids(results: Vec<Scored<NodeId>>) -> Vec<NodeId> {
        results.into_iter().map(|s| s.item).collect()
    }

    #[test]
    fn tokenize_matches_canonical_contract() {
        assert_eq!(normalize_tokens("Create User"), vec!["creat", "user"]);
        assert_eq!(normalize_tokens("create"), vec!["creat"]);
        assert_eq!(normalize_tokens("pagination"), vec!["pagin"]);
        assert_eq!(normalize_tokens("rate-limiter"), vec!["rate", "limit"]);
        assert_eq!(normalize_tokens("rate_limiter"), vec!["rate", "limit"]);
        assert_eq!(normalize_tokens("RateLimiter"), vec!["rate", "limit"]);
        // Acronym runs are NOT split: the canonical tokenizer follows the fixture
        // convention (lower->Upper camelCase boundaries only, matching
        // gen-fixtures.py's regex), so "APIKey" indexes as one term "apikey".
        // The integration seam copy split acronym runs ("api","key"); reconciled
        // in favor of the canonical tokenizer (import, don't fork).
        assert_eq!(normalize_tokens("APIKey"), vec!["apikey"]);
        assert_eq!(normalize_tokens(""), Vec::<String>::new());
        // Stopwords are dropped, not stemmed.
        assert_eq!(normalize_tokens("the of and to"), Vec::<String>::new());
        // Duplicates retained: term frequency counts occurrences.
        assert_eq!(
            normalize_tokens("user user schema"),
            vec!["user", "user", "schema"]
        );
    }

    #[test]
    fn add_remove_readd_is_idempotent_per_id() {
        let mut idx = InvertedIndex::new();
        let c = concept(1, "user schema");
        idx.add(&c);
        // Same id, same content: still exactly one document.
        idx.add(&c);
        assert_eq!(idx.doc_tokens.len(), 1);
        assert_eq!(ids(idx.search("schema", 10)), vec![c.id]);

        // Re-add with new content replaces the postings (same id, never twice).
        let updated = concept(1, "create user");
        idx.add(&updated);
        assert_eq!(idx.doc_tokens.len(), 1);
        assert!(idx.search("schema", 10).is_empty());
        assert_eq!(ids(idx.search("create", 10)), vec![updated.id]);
        assert_eq!(ids(idx.search("user", 10)), vec![updated.id]);
    }

    #[test]
    fn remove_drops_postings_and_lengths() {
        let mut idx = InvertedIndex::new();
        let a = concept(1, "rate limiter");
        let b = concept(2, "rate limit");
        idx.add(&a);
        idx.add(&b);
        assert_eq!(ids(idx.search("rate", 10)).len(), 2);

        idx.remove(a.id);
        assert_eq!(ids(idx.search("rate", 10)), vec![b.id]);
        assert_eq!(idx.total_docs, 1);
        assert_eq!(idx.total_tokens, 2);

        // Removing an unindexed id is a no-op.
        idx.remove(a.id);
        assert_eq!(idx.total_docs, 1);

        // Removing the last document empties the index entirely.
        idx.remove(b.id);
        assert_eq!(idx.total_docs, 0);
        assert!(idx.search("rate", 10).is_empty());
        assert!(idx.postings.is_empty());
    }

    #[test]
    fn remove_drops_the_tie_break_key() {
        // Remediation round 1: `remove` used to leave the concept's canonical
        // key in `doc_keys` forever. A re-added id overwrote it, so the leak
        // never surfaced in `search` (no postings -> never consulted), but the
        // map must mirror the postings' lifecycle, not grow past it.
        let mut idx = InvertedIndex::new();
        let a = concept(1, "rate limiter");
        idx.add(&a);
        assert_eq!(
            idx.doc_keys.get(&a.id).map(String::as_str),
            Some("rate limiter")
        );

        idx.remove(a.id);
        assert!(
            idx.doc_keys.is_empty(),
            "a removed concept leaves no key behind"
        );

        // Re-adding the id inserts its key fresh.
        let rekeyed = concept(1, "create user");
        idx.add(&rekeyed);
        assert_eq!(
            idx.doc_keys.get(&a.id).map(String::as_str),
            Some("create user")
        );
    }

    #[test]
    fn empty_query_returns_empty() {
        let mut idx = InvertedIndex::new();
        idx.add(&concept(1, "user schema"));
        assert!(idx.search("", 10).is_empty());
        assert!(idx.search("   ", 10).is_empty());
        // Query consisting only of stopwords tokenizes to nothing.
        assert!(idx.search("the of", 10).is_empty());
    }

    #[test]
    fn no_concept_returns_empty() {
        let idx = InvertedIndex::new();
        assert!(idx.search("anything", 10).is_empty());
    }

    #[test]
    fn duplicate_query_terms_count_once() {
        // grok G1: query-term frequency is 1 — "user user" must score exactly
        // like "user" (same scores, not k-times).
        let mut idx = InvertedIndex::new();
        idx.add(&concept(1, "user schema"));
        idx.add(&concept(2, "user role"));
        assert_eq!(idx.search("user", 10), idx.search("user user", 10));
        assert_eq!(ids(idx.search("user user", 10)).len(), 2);
    }

    #[test]
    fn zero_token_corpus_stays_empty_and_sane() {
        // grok G2: a corpus of stopword-only documents has no postings; results
        // must stay empty with no NaN leakage (avg_dl guarded).
        let mut idx = InvertedIndex::new();
        idx.add(&concept(1, "the and of is"));
        assert_eq!(idx.total_docs, 1);
        assert_eq!(idx.total_tokens, 0);
        assert!(idx.search("user", 10).is_empty());
        assert!(idx.search("the", 10).is_empty());
        assert!(idx.search("", 10).is_empty());
    }

    #[test]
    fn search_orders_by_score_then_key_then_id_and_truncates() {
        let mut idx = InvertedIndex::new();
        // id1 "alpha" (dl=1) scores highest; id2/id3 (dl=2, same tf) tie. The
        // tie-break is canonical key asc, id asc behind that (issue #2): the
        // keys are chosen to contradict the id order, so the old id-first
        // chain would have returned [id2, id3] on the tie.
        let a = concept(1, "alpha");
        let mut b = concept(2, "alpha beta");
        b.canonical_key = "zeta".into();
        let mut c = concept(3, "alpha gamma");
        c.canonical_key = "alpha".into();
        idx.add(&a);
        idx.add(&b);
        idx.add(&c);

        let limited = ids(idx.search("alpha", 1));
        assert_eq!(limited, vec![a.id]);

        let limited2 = ids(idx.search("alpha", 2));
        assert_eq!(
            limited2,
            vec![a.id, c.id],
            "key order beats id order on the tie"
        );

        let all = ids(idx.search("alpha", 10));
        assert_eq!(all, vec![a.id, c.id, b.id]);
        assert!(idx.search("alpha", 0).is_empty());
    }

    #[test]
    fn multi_term_query_scores_union_of_terms() {
        let mut idx = InvertedIndex::new();
        let a = concept(1, "create user");
        let b = concept(2, "user schema");
        idx.add(&a);
        idx.add(&b);
        // "create user" matches both terms; "user schema" matches one.
        let results = idx.search("create user", 10);
        assert_eq!(ids(results.clone()), vec![a.id, b.id]);
        let score_a = results.iter().find(|s| s.item == a.id).unwrap().score;
        let score_b = results.iter().find(|s| s.item == b.id).unwrap().score;
        assert!(score_a > score_b);
    }

    #[cfg(feature = "fixtures")]
    #[test]
    fn recall_phase1_keyword_goldens_pass() {
        let snap = crate::fixtures::load_snapshot("session-rest-api").unwrap();
        let idx = InvertedIndex::from_snapshot(&snap);
        let goldens = crate::fixtures::load_recall_goldens().unwrap();
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
            let got = ids(idx.search(query, top_k));
            // Exact set AND order (score-desc, tie by id) — not float comparisons.
            assert_eq!(got, expected, "phase-1 candidates for query {query:?}");
        }
    }

    #[cfg(feature = "fixtures")]
    #[test]
    fn from_snapshot_indexes_every_concept() {
        let snap = crate::fixtures::load_snapshot("session-rest-api").unwrap();
        let idx = InvertedIndex::from_snapshot(&snap);
        assert_eq!(idx.total_docs, snap.concepts.len());
        for c in &snap.concepts {
            assert!(idx.doc_tokens.contains_key(&c.id));
        }
    }
}
