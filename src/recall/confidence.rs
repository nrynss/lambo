//! A recall's top per-leg scores and its provisional combined confidence
//! (#256, split out of #174). A report only: nothing here ranks, filters,
//! abstains or thresholds.
//!
//! # The per-leg scores are the primary output
//!
//! [`TopLegs`] is the best score each phase-1 leg gave any candidate of the
//! caller's own session:
//!
//! * `bm25`: the keyword leg's best score.
//! * `vector_cosine`: the vector leg's best cosine.
//!
//! **"Phase-1 candidates"** are the members of the phase-1
//! [`LegProvenance`] map
//! ([`candidates_with_legs`](crate::recall::candidates::candidates_with_legs)):
//! every concept a leg scored, before phase-2 expansion and before
//! phase-3's `top_k` cut. So the maximum is over what the leg scored,
//! returned or not. The keyword leg contributes the top `top_k x 4` BM25
//! matches of the session's inverted index (and the exact-name match, at
//! `1.0`); the vector leg contributes the `top_k` nearest stored vectors.
//! Both are best-first reads, so the leg's maximum is always among them and
//! reading it costs no store call and no embed: it is a fold over a map the
//! pipeline already built (and caches with the pipeline). The recent leg's
//! flat score and expansion members are not counted (#174).
//!
//! On every backend the vector leg's score is a cosine: an exact scan
//! (SQLite, a session holder's own graph), `1 - distance` (Postgres), the
//! dialect's distance-to-cosine map (CockroachDB), `2 x _score - 1` (the
//! Elastic recall tier). Where the search runs through an approximate index
//! (CockroachDB's vector index, a pgvector HNSW index, Elastic's HNSW) the
//! leg's best is the best of what the index returned, which may sit a
//! little under the true maximum. A store with no vector search (the
//! in-memory store) runs no vector leg.
//!
//! # The combined value is provisional
//!
//! [`ProvisionalConfidence`] is the noisy-OR recorded on #174:
//!
//! ```text
//! a = S / (S + 5)                      S = the keyword leg's best score
//! b = clamp((c - m) / (1 - m), 0, 1)   c = the vector leg's best cosine,
//!                                      m = the contract's null median
//! value = 1 - (1 - a)(1 - b)
//! ```
//!
//! `m` is the median cosine of a query to an irrelevant concept under the
//! session's embedding contract
//! ([`crate::embed::calibration::null_median_cosine`]). The form was
//! measured on synthetic data only, so the wire shape says `provisional`
//! and names its [`BASIS`]; it is not a probability, and not comparable
//! across contracts or store sizes.
//!
//! It is **absent**, never guessed, unless both legs ran on a text query
//! under a contract with a measured null median:
//!
//! * no vector leg (no vector search, no embedding, a failed embed or
//!   vector read) or a vector leg with no candidate. The basis is defined
//!   over two legs; a keyword-only value would be a different, lower
//!   quantity under the same name. `top_legs.bm25` still reports the leg;
//! * no keyword leg (no index, or no query text);
//! * a recall by image or by a client vector: the null medians were
//!   measured on text queries;
//! * a contract with no measured null median, or `[embedder] calibration =
//!   "off"`;
//! * a dependency question answered by traversal, which runs no leg;
//! * a non-finite input.

use crate::recall::candidates::LegProvenance;
use crate::types::{ProvisionalConfidence, TopLegs};

/// The name and version of the combining formula. Changing the formula or
/// any constant in it changes this string.
pub const BASIS: &str = "noisy-or-v0";

/// `k0` in the keyword term `S / (S + k0)` (#174: 5 beat 2).
pub const KEYWORD_HALF_SCORE: f64 = 5.0;

/// The best keyword and vector scores over every phase-1 candidate.
/// `keyword_ran` says the keyword leg searched (a text query against an
/// installed index): it then reports `0.0` when nothing matched, the BM25 of
/// a concept sharing no term with the query. Non-finite scores are ignored,
/// as phase 1's own leg ranking ignores them.
pub(crate) fn top_legs(legs: &LegProvenance, keyword_ran: bool) -> TopLegs {
    let best = |pick: fn(&crate::recall::candidates::LegScores) -> Option<f64>| {
        legs.values()
            .filter_map(pick)
            .filter(|s| s.is_finite())
            .reduce(f64::max)
    };
    TopLegs {
        bm25: best(|l| l.keyword).or(keyword_ran.then_some(0.0)),
        vector_cosine: best(|l| l.vector),
    }
}

/// The noisy-OR of a best keyword score and a best cosine rescaled at
/// `null_median`. `None` for a non-finite input or a median that leaves no
/// scale (`>= 1`).
pub fn noisy_or(bm25: f64, vector_cosine: f64, null_median: f64) -> Option<f64> {
    if !(bm25.is_finite() && vector_cosine.is_finite() && null_median.is_finite())
        || null_median >= 1.0
    {
        return None;
    }
    let s = bm25.max(0.0);
    let a = s / (s + KEYWORD_HALF_SCORE);
    let b = ((vector_cosine - null_median) / (1.0 - null_median)).clamp(0.0, 1.0);
    Some(1.0 - (1.0 - a) * (1.0 - b))
}

/// The provisional confidence of a text recall whose legs' best scores are
/// `top`, under a contract whose null median is `null_median`. `None`
/// unless both legs reported and the median is known (module docs).
pub(crate) fn provisional(top: TopLegs, null_median: Option<f64>) -> Option<ProvisionalConfidence> {
    let value = noisy_or(top.bm25?, top.vector_cosine?, null_median?)?;
    Some(ProvisionalConfidence {
        value,
        basis: BASIS.to_string(),
        provisional: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recall::candidates::LegScores;
    use crate::types::NodeId;
    use uuid::Uuid;

    fn legs(rows: &[(Option<f64>, Option<f64>, Option<f64>)]) -> LegProvenance {
        rows.iter()
            .enumerate()
            .map(|(i, (keyword, recent, vector))| {
                (
                    NodeId(Uuid::from_u64_pair(9, i as u64)),
                    LegScores {
                        keyword: *keyword,
                        recent: *recent,
                        vector: *vector,
                        ..LegScores::default()
                    },
                )
            })
            .collect()
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
    fn the_value_is_absent_rather_than_guessed() {
        let both = TopLegs {
            bm25: Some(4.0),
            vector_cosine: Some(0.7),
        };
        let got = provisional(both, Some(0.36)).expect("both legs and a median");
        assert_eq!(got.basis, "noisy-or-v0");
        assert!(got.provisional);
        assert_eq!(Some(got.value), noisy_or(4.0, 0.7, 0.36));
        // An uncalibrated contract.
        assert_eq!(provisional(both, None), None);
        // No vector leg: the keyword leg is still reported by `top_legs`.
        let keyword_only = TopLegs {
            vector_cosine: None,
            ..both
        };
        assert_eq!(provisional(keyword_only, Some(0.36)), None);
        // No keyword leg.
        let vector_only = TopLegs { bm25: None, ..both };
        assert_eq!(provisional(vector_only, Some(0.36)), None);
        // No candidates at all.
        assert_eq!(provisional(TopLegs::default(), Some(0.36)), None);
        // Non-finite inputs and a median with no scale left.
        assert_eq!(noisy_or(f64::NAN, 0.7, 0.36), None);
        assert_eq!(noisy_or(4.0, f64::INFINITY, 0.36), None);
        assert_eq!(noisy_or(4.0, 0.7, 1.0), None);
    }

    #[test]
    fn top_legs_are_the_maxima_and_ignore_the_recent_leg() {
        let map = legs(&[
            (Some(2.5), None, Some(0.41)),
            (Some(7.25), Some(0.35), None),
            (None, Some(0.35), Some(0.66)),
            (None, Some(0.35), None),
            (None, None, Some(f64::NAN)),
        ]);
        assert_eq!(
            top_legs(&map, true),
            TopLegs {
                bm25: Some(7.25),
                vector_cosine: Some(0.66),
            }
        );
        // Recent-only candidates give neither leg a score.
        let recent = legs(&[(None, Some(0.35), None)]);
        assert_eq!(
            top_legs(&recent, true),
            TopLegs {
                bm25: Some(0.0),
                vector_cosine: None,
            },
            "the keyword leg ran and matched nothing"
        );
        assert_eq!(top_legs(&recent, false), TopLegs::default());
        assert_eq!(top_legs(&LegProvenance::new(), false), TopLegs::default());
    }
}
