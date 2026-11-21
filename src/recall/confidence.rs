//! A recall's top per-leg scores and its provisional combined confidence
//! (#256, split out of #174). A report only: nothing here ranks, filters,
//! abstains or thresholds.
//!
//! # The per-leg scores are the primary output
//!
//! [`TopLegs`] is the best score each phase-1 leg gave any candidate of the
//! caller's own session:
//!
//! * `bm25`: the keyword leg's best BM25, as the index returned it.
//! * `vector_cosine`: the vector leg's best cosine.
//! * `exact_name_match`: the query, normalized, is a concept's durable name.
//!
//! **"Phase-1 candidates"** are the members of the phase-1
//! [`LegProvenance`] map
//! ([`candidates_with_legs`](crate::recall::candidates::candidates_with_legs)):
//! every concept a leg scored, before phase-2 expansion and before
//! phase-3's `top_k` cut. So the maximum is over what the leg scored,
//! returned or not. The keyword leg contributes the top `top_k x 4` BM25
//! matches of the session's inverted index (plus an exact-name match,
//! wherever it ranks), but its best is read off the
//! one index search phase 1 makes before that cut (`KeywordLeg`), so `bm25`
//! is the same at every `top_k`, 0 included. The vector leg contributes the
//! `top_k` nearest stored vectors, a best-first read, so its maximum is
//! among them; the vector best is a fold over a map the pipeline already
//! built (and caches with the pipeline). Neither costs a store call or an
//! embed. A `top_k` of 0, which only a library caller can send (the CLI
//! and MCP take 1 to 100), asks the vector leg for no neighbours, so
//! `vector_cosine` is `None` there and a text recall reports
//! `"keyword-only-v0"` where `top_k = 1` would report `"noisy-or-v0"`.
//! The recent leg's flat score and expansion members are not counted (#174).
//!
//! An exact-name match raises its concept's keyword score to at least `1.0`
//! (the per-hit `legs.keyword` is the higher of its BM25 and `1.0`, #288).
//! That floor is not a BM25: `bm25` is the index's own best, read before the
//! name step, and `exact_name_match` reports the match.
//!
//! Both maxima count only concepts of the graph the recall answers from.
//! A reader (`lambo recall`, the web portal) loads a snapshot and then reads
//! vectors from the store; a concept a writer flushed in between can come
//! back from that read, and assembly skips it, so the fold skips it too.
//!
//! On every backend the vector leg's score is a cosine: an exact scan
//! (SQLite, a session holder's own graph), `1 - distance` (Postgres), the
//! dialect's distance-to-cosine map (CockroachDB), `2 x _score - 1` (the
//! Elastic recall tier). Where the search runs through an approximate index
//! (CockroachDB's vector index, a pgvector HNSW index, Elastic's HNSW) the
//! leg's best is the best of what the index returned, which may sit a
//! little under the true maximum, and can move with `top_k`, which sets how
//! many neighbours the index is asked for. On an exact search it does not
//! move with `top_k`. A store with no vector search (the in-memory store)
//! runs no vector leg.
//!
//! # The combined value is provisional
//!
//! [`ProvisionalConfidence`] has two bases, and a value is only comparable
//! with another of the same basis.
//!
//! `BASIS`, `"noisy-or-v0"`, when both legs reported:
//!
//! ```text
//! a = S / (S + 5)                      S = the keyword leg's best BM25
//! b = clamp((c - m) / (1 - m), 0, 1)   c = the vector leg's best cosine,
//!                                      m = the contract's pairwise null median
//! value = 1 - (1 - a)(1 - b)
//! ```
//!
//! `KEYWORD_ONLY_BASIS`, `"keyword-only-v0"`, when the keyword leg ran and
//! the vector leg did not (a store with no vector search, a failed query
//! embed or vector read, no embedded concept): `value = a`. It needs no null
//! median, so it is reported under any contract and calibration mode. It is
//! the two-leg value with `b = 0`, a lower quantity than the two-leg value
//! would have been for the same store, which is why it has its own name
//! (#256's acceptance: a recall with no vector leg still reports a
//! confidence). "Lower" is exact in real arithmetic (Lean
//! `Confidence.keywordOnly_le_noisyOr`); in `f64` it holds only up to an
//! ulp: `noisy_or(1.0, 0.10, 0.36)` rounds to an ulp below
//! `keyword_term(1.0)`.
//!
//! A vector leg that ran but left nothing to count counts as one that did
//! not run: when every cosine it returned is non-finite, or every concept it
//! returned is outside the graph the recall answers from (a reader's
//! snapshot, above), `vector_cosine` is `None`, and the recall reports
//! `"keyword-only-v0"`, on a calibrated contract too.
//!
//! Neither basis takes `exact_name_match` into account: a query that is a
//! concept's name but shares no word with any concept's content reports
//! `bm25` 0, and with the keyword leg alone a value of 0, beside
//! `exact_name_match: true`. Read the flag beside the value.
//!
//! `m` is the **pairwise null median**: the median cosine of a query to one
//! concept graded irrelevant to it under the session's embedding contract
//! ([`crate::embed::calibration::null_median_cosine`], each run file's
//! `retrieval.self.query_to_irrelevant.dev.p50`). It is a statistic of a
//! single pair, so it does not depend on store size. `c` is not a single
//! pair: it is a **maximum** over the store, and the maximum an unrelated
//! query finds grows with the store. At the 100 concepts of the evaluation
//! sets, the median best cosine of a query with no answer
//! (`top1_no_answer.dev.p50` of the same run files) already puts `b` at
//! 0.27 (BGE-M3), 0.29 (EmbeddingGemma 2), 0.37 and 0.39 (Qwen3 Embedding 4B
//! at 2560 and 1024 dims) and 0.38 (Elastic Jina v3), well above 0, and
//! more in a larger store. The EmbeddingGemma 2 median comes from the older
//! #87 evaluation set (`evidence/issue-87-calibration/`); the others from
//! the #190 set (`evidence/issue-221/`, `evidence/issue-237/`).
//!
//! #174 recorded the form as "cos rescaled at null q50" on synthetic data,
//! without saying which null distribution it rescaled at. Its quoted AUC
//! (0.74 to 0.75) may therefore not describe this formula; the wire shape
//! says `provisional` and names its basis for that reason. It is not a
//! probability, and not comparable across contracts or store sizes.
//!
//! The two-leg value is **absent**, never guessed, on a text recall whose
//! vector leg reported when the contract has no measured null median (a
//! contract with no calibration row; under `[embedder] calibration = "off"`
//! every contract but the BGE-M3 reference, whose median is the reference
//! value "off" keeps). Both leg maxima it reads are finite by construction
//! (a non-finite score is never a leg's best), so the non-finite guards in
//! `noisy_or` and the keyword term are defensive.
//!
//! And no confidence of either basis is reported when:
//!
//! * no keyword leg ran (no index, or no query text);
//! * the recall is by image or by a client vector: the null medians were
//!   measured on text queries;
//! * it is a layer's side of a layered recall (a layer is another store; its
//!   legs are reported apart, never combined);
//! * a dependency question was answered by traversal, which runs no leg;
//! * the recall was refused with a warning, which runs no leg.

use crate::graph::Graph;
use crate::recall::candidates::{KeywordLeg, LegProvenance};
use crate::types::{Node, ProvisionalConfidence, TopLegs};

/// The name and version of the two-leg combining formula. Changing the
/// formula or any constant in it changes this string.
pub(crate) const BASIS: &str = "noisy-or-v0";

/// The name and version of the keyword-only formula, `S / (S + 5)`: a
/// different quantity from [`BASIS`], never to be compared with it.
pub(crate) const KEYWORD_ONLY_BASIS: &str = "keyword-only-v0";

/// `k0` in the keyword term `S / (S + k0)` (#174: 5 beat 2).
pub(crate) const KEYWORD_HALF_SCORE: f64 = 5.0;

/// The best keyword and vector scores of a recall's phase 1 over the
/// concepts of `graph`. The keyword leg's comes from `keyword` (the index's
/// own best, read before the exact-name step raises the named concept's
/// keyword leg); the vector leg's is the
/// best finite cosine in `legs` (phase 1's own leg ranking ignores a
/// non-finite one too).
pub(crate) fn top_legs(graph: &Graph, legs: &LegProvenance, keyword: KeywordLeg) -> TopLegs {
    let vector_cosine = legs
        .iter()
        .filter(|(id, _)| matches!(graph.node(**id), Some(Node::Concept(_))))
        .filter_map(|(_, l)| l.vector)
        .filter(|s| s.is_finite())
        .reduce(f64::max);
    TopLegs {
        bm25: if keyword.ran { keyword.best_bm25 } else { None },
        vector_cosine,
        exact_name_match: keyword.ran && keyword.exact_name,
    }
}

/// The keyword term `S / (S + 5)`. A negative score counts as none; `None`
/// for a non-finite one.
fn keyword_term(bm25: f64) -> Option<f64> {
    if !bm25.is_finite() {
        return None;
    }
    let s = bm25.max(0.0);
    Some(s / (s + KEYWORD_HALF_SCORE))
}

/// The noisy-OR of a best keyword score and a best cosine rescaled at
/// `null_median`. `None` for a non-finite input or a median that leaves no
/// scale (`>= 1`).
pub(crate) fn noisy_or(bm25: f64, vector_cosine: f64, null_median: f64) -> Option<f64> {
    if !(vector_cosine.is_finite() && null_median.is_finite()) || null_median >= 1.0 {
        return None;
    }
    let a = keyword_term(bm25)?;
    let b = ((vector_cosine - null_median) / (1.0 - null_median)).clamp(0.0, 1.0);
    Some(1.0 - (1.0 - a) * (1.0 - b))
}

/// The provisional confidence of a **text** recall of the caller's own
/// session whose legs' best scores are `top`, under a contract whose
/// pairwise null median is `null_median` (module docs). The caller decides
/// the route: a recall by image or vector, and a layer's side, never call
/// this.
///
/// * Both legs reported: [`BASIS`], `None` without a median.
/// * Only the keyword leg: [`KEYWORD_ONLY_BASIS`], whatever the median.
/// * No keyword leg: `None`.
pub(crate) fn provisional(top: TopLegs, null_median: Option<f64>) -> Option<ProvisionalConfidence> {
    let bm25 = top.bm25?;
    let (value, basis) = match top.vector_cosine {
        Some(cosine) => (noisy_or(bm25, cosine, null_median?)?, BASIS),
        None => (keyword_term(bm25)?, KEYWORD_ONLY_BASIS),
    };
    Some(ProvisionalConfidence {
        value,
        basis: basis.to_string(),
        provisional: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recall::candidates::LegScores;
    use crate::test_util::citations::{concept, graph};
    use crate::types::{ConceptType, NodeId};
    use uuid::Uuid;

    /// A graph holding one concept per row, and a leg map scoring them as
    /// the rows say, plus one id the graph does not hold.
    fn legs(rows: &[(Option<f64>, Option<f64>, Option<f64>)]) -> (Graph, LegProvenance) {
        let (mut g, i) = graph("confidence-unit");
        let mut map = LegProvenance::new();
        for (n, (keyword, recent, vector)) in rows.iter().enumerate() {
            let id = concept(&mut g, i, &format!("concept {n}"), ConceptType::Logic);
            map.insert(
                id,
                LegScores {
                    keyword: *keyword,
                    recent: *recent,
                    vector: *vector,
                    ..LegScores::default()
                },
            );
        }
        (g, map)
    }

    fn ran(best_bm25: Option<f64>) -> KeywordLeg {
        KeywordLeg {
            ran: true,
            best_bm25,
            exact_name: false,
        }
    }

    fn top(bm25: Option<f64>, vector_cosine: Option<f64>) -> TopLegs {
        TopLegs {
            bm25,
            vector_cosine,
            exact_name_match: false,
        }
    }

    #[test]
    fn the_arithmetic_is_the_noisy_or_recorded_on_174() {
        // a = 5 / (5 + 5) = 0.5; b = (0.68 - 0.36) / (1 - 0.36) = 0.5.
        let v = noisy_or(5.0, 0.68, 0.36).unwrap();
        assert!((v - 0.75).abs() < 1e-12, "{v}");
        // The keyword term alone, and the cosine term alone.
        assert!((noisy_or(15.0, 0.36, 0.36).unwrap() - 0.75).abs() < 1e-12);
        assert!((noisy_or(0.0, 0.68, 0.36).unwrap() - 0.5).abs() < 1e-12);
        // A cosine at or under the null median adds nothing; one at 1 is 1.
        assert_eq!(noisy_or(0.0, 0.10, 0.36), Some(0.0));
        assert_eq!(noisy_or(0.0, -0.5, 0.36), Some(0.0));
        assert_eq!(noisy_or(0.0, 1.0, 0.36), Some(1.0));
        // A negative keyword score counts as none.
        assert_eq!(noisy_or(-3.0, 0.36, 0.36), Some(0.0));
        // Always in [0, 1], and monotone in each leg.
        let mut last = 0.0;
        for i in 0..50 {
            let v = noisy_or(f64::from(i), 0.5, 0.36).unwrap();
            assert!((0.0..=1.0).contains(&v) && v >= last);
            last = v;
        }
    }

    #[test]
    fn the_basis_follows_the_legs_and_the_value_is_never_guessed() {
        let both = top(Some(4.0), Some(0.7));
        let got = provisional(both, Some(0.36)).expect("both legs and a median");
        assert_eq!(got.basis, "noisy-or-v0");
        assert!(got.provisional);
        assert_eq!(Some(got.value), noisy_or(4.0, 0.7, 0.36));
        // Both legs on an uncalibrated contract: absent, not keyword-only.
        assert_eq!(provisional(both, None), None);
        // No vector leg: the keyword-only basis, with or without a median.
        let keyword_only = top(Some(4.0), None);
        for median in [Some(0.36), None] {
            let got = provisional(keyword_only, median).expect("keyword-only");
            assert_eq!(got.basis, "keyword-only-v0");
            assert!(got.provisional);
            assert!((got.value - 4.0 / 9.0).abs() < 1e-12, "{got:?}");
        }
        // The keyword leg ran and matched nothing: a value of 0, reported.
        let nothing = provisional(top(Some(0.0), None), None).expect("ran");
        assert_eq!(
            (nothing.value, nothing.basis.as_str()),
            (0.0, "keyword-only-v0")
        );
        // No keyword leg, with or without a vector leg.
        assert_eq!(provisional(top(None, Some(0.7)), Some(0.36)), None);
        assert_eq!(provisional(TopLegs::default(), Some(0.36)), None);
        // Non-finite inputs and a median with no scale left.
        assert_eq!(noisy_or(f64::NAN, 0.7, 0.36), None);
        assert_eq!(noisy_or(4.0, f64::INFINITY, 0.36), None);
        assert_eq!(noisy_or(4.0, 0.7, 1.0), None);
        assert_eq!(provisional(top(Some(f64::INFINITY), None), None), None);
    }

    #[test]
    fn top_legs_are_the_maxima_and_ignore_the_recent_leg() {
        let (g, map) = legs(&[
            (Some(2.5), None, Some(0.41)),
            (Some(7.25), Some(0.35), None),
            (None, Some(0.35), Some(0.66)),
            (None, Some(0.35), None),
            (None, None, Some(f64::NAN)),
        ]);
        // The keyword best is the leg's own report, not a fold over `legs`
        // (whose keyword slot an exact-name match raises to at least 1.0).
        assert_eq!(
            top_legs(&g, &map, ran(Some(7.25))),
            top(Some(7.25), Some(0.66))
        );
        // Recent-only candidates give neither leg a score.
        let (g, recent) = legs(&[(None, Some(0.35), None)]);
        assert_eq!(
            top_legs(&g, &recent, ran(Some(0.0))),
            top(Some(0.0), None),
            "the keyword leg ran and matched nothing"
        );
        assert_eq!(
            top_legs(&g, &recent, KeywordLeg::default()),
            TopLegs::default()
        );
        // A leg whose every score was non-finite reports nothing.
        assert_eq!(top_legs(&g, &recent, ran(None)), TopLegs::default());
        // An exact-name match is reported only when the leg ran.
        let named = KeywordLeg {
            exact_name: true,
            ..ran(Some(1.5))
        };
        assert!(top_legs(&g, &recent, named).exact_name_match);
        let not_ran = KeywordLeg {
            ran: false,
            ..named
        };
        assert_eq!(top_legs(&g, &recent, not_ran), TopLegs::default());
    }

    /// A vector candidate the graph does not hold (a reader's store read
    /// returning a concept flushed after its snapshot load) is not counted:
    /// assembly cannot return it either.
    #[test]
    fn a_vector_candidate_outside_the_graph_is_not_a_top_leg() {
        let (g, mut map) = legs(&[(None, None, Some(0.41))]);
        map.insert(
            NodeId(Uuid::from_u64_pair(9, 9)),
            LegScores {
                vector: Some(0.93),
                ..LegScores::default()
            },
        );
        assert_eq!(
            top_legs(&g, &map, KeywordLeg::default()).vector_cosine,
            Some(0.41)
        );
    }
}
