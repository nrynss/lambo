//! Replacement candidates (#198 part c, owner decision 2026-10-28): the
//! existing concepts a hybrid write's new concept most likely replaces,
//! listed on the write's settled outcome so the writer can cite one in
//! `against` on its next call.
//!
//! Lambo never declares the relation itself. The list is **informational
//! only**: it records no edge and changes no score, merge, canonization or
//! recall result, and an outcome with no candidates is unchanged. The
//! suspected-supersession trigger ([`super::record_refused_hits`]) is the
//! other half of #198: it records an edge for a clean value swap, while this
//! list hands every close same-kind neighbour to the one party that knows
//! whether it was replaced. A pair the trigger flagged is listed too: only
//! the writer can confirm it.
//!
//! # Which hits are listed
//!
//! A vector hit on a concept this write **created** (a fresh concept or one
//! that took a similarity merge; a canonical match wrote nothing new) is a
//! candidate when every one of these holds ([`select`]):
//!
//! * its cosine is finite and at or above the floor, which is the write's
//!   effective merge threshold (`semantic_match_threshold` as the write
//!   path receives it: #87's calibrated value for the contract where it has
//!   one, so 0.85 under BGE-M3 and 0.93 under EmbeddingGemma 2 at 768
//!   dimensions, else a #176 derivation or the configured threshold);
//! * it is a concept of the same session, of the same type, and that type
//!   is `Logic`, `Constraint` or `Observation` ([`super::eligible_type`]);
//! * it has the same repository scope ([`super::repo_scope`]);
//! * no `against` or `depends_on` already joins the two, either way
//!   ([`super::settled`]);
//! * it is not the concept this write merged into, nor a concept the same
//!   call wrote or re-derived (the writer just restated it), nor an image
//!   concept (a supplied vector is a picture, not a claim).
//!
//! Ranked by cosine (then id, so ties are deterministic) and capped at
//! [`REPLACEMENT_CANDIDATE_LIMIT`] per created concept.
//!
//! # Why these defaults (measured, `evidence/issue-198c-candidates/`)
//!
//! * **Hits the write already has.** The candidates come from the store's
//!   top [`crate::graph::hybrid::VECTOR_CANDIDATE_LIMIT`] vector hits, the
//!   same ones the merge leg reads: no second search. On a private copy of
//!   the dogfood store (1,103 eligible concepts), a search over the whole
//!   session lists exactly the same concepts at every floor of 0.78 and up.
//! * **The floor is the merge threshold.** The four labelled real
//!   supersessions sit at cosine 0.857 to 0.899 (BGE-M3), all of them in
//!   the band the merge threshold already marks, and each predecessor ranks
//!   first. Lowering the floor to 0.82 makes the list non-empty on 10.5% of
//!   the store's writes instead of 2.8% for no real supersession gained; on
//!   the public #211 set it gains two of 43 reworded dev supersessions. One
//!   threshold per contract also means #87's calibration carries over with
//!   no second table.
//! * **The cap is 3.** At the floor no write on the store had a longer
//!   list, and the true predecessor was always first.

use crate::graph::Graph;
use crate::types::{Concept, Node, NodeId, Scored};

/// The most candidates listed for one created concept.
pub const REPLACEMENT_CANDIDATE_LIMIT: usize = 3;

/// Maximum retained UTF-8 bytes of candidate strings (ids and excerpts) per
/// write receipt, as #208's stale report bounds its rows
/// ([`crate::daemon::contradiction::STALE_RECEIPT_BYTES`]).
pub const REPLACEMENT_RECEIPT_BYTES: usize = 2048;

/// One existing concept a created concept may replace.
#[derive(Clone, Debug)]
pub struct ReplacementCandidate {
    /// The concept this write created.
    pub new: NodeId,
    /// The existing concept it may replace.
    pub id: NodeId,
    /// The existing concept's text, as recall excerpts it.
    pub excerpt: String,
    /// The vector hit's cosine.
    pub cosine: f64,
}

// The cosine is compared by its bits so the outcome structs that carry it
// stay `Eq`: it is a reported value, never computed on.
impl PartialEq for ReplacementCandidate {
    fn eq(&self, other: &Self) -> bool {
        self.new == other.new
            && self.id == other.id
            && self.excerpt == other.excerpt
            && self.cosine.to_bits() == other.cosine.to_bits()
    }
}

impl Eq for ReplacementCandidate {}

/// The cosine as reported: four decimals, the precision recall and the
/// evidence use.
pub fn reported_cosine(cosine: f64) -> f64 {
    (cosine * 10_000.0).round() / 10_000.0
}

impl serde::Serialize for ReplacementCandidate {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("ReplacementCandidate", 5)?;
        // The id recall shows, which `against` accepts, then the full one.
        st.serialize_field("id", &crate::recall::format::short_id(self.id))?;
        st.serialize_field("node_id", &self.id.0.to_string())?;
        st.serialize_field("excerpt", &self.excerpt)?;
        st.serialize_field("cosine", &reported_cosine(self.cosine))?;
        st.serialize_field("new_id", &crate::recall::format::short_id(self.new))?;
        st.end()
    }
}

/// A write's candidates as its receipt carries them: the rows kept under
/// [`REPLACEMENT_RECEIPT_BYTES`] and the true count.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ReplacementReport {
    pub candidates: Vec<ReplacementCandidate>,
    pub total: usize,
}

impl ReplacementReport {
    /// The receipt's report of `candidates`, or `None` when there are none,
    /// so a write with no candidates carries nothing new.
    pub fn for_receipt(candidates: &[ReplacementCandidate]) -> Option<Self> {
        if candidates.is_empty() {
            return None;
        }
        let mut bytes = 0;
        let kept = candidates
            .iter()
            .take_while(|c| {
                // Both ids as their 36-byte rendered form, and the excerpt.
                bytes += 2 * 36 + c.excerpt.len();
                bytes <= REPLACEMENT_RECEIPT_BYTES
            })
            .cloned()
            .collect();
        Some(Self {
            candidates: kept,
            total: candidates.len(),
        })
    }

    /// The receipt lines: a clause for the summary sentence and one line
    /// per kept candidate, then how many more were cut.
    pub fn describe(&self) -> String {
        let mut out = format!(
            "; {} replacement candidate(s): existing concepts this write may replace \
             (nothing recorded; cite one in against on a later call only if it really is \
             replaced)",
            self.total
        );
        for c in &self.candidates {
            out.push_str(&format!(
                "\n  - {} \"{}\" (cosine {:.4}, for {})",
                crate::recall::format::short_id(c.id),
                c.excerpt,
                reported_cosine(c.cosine),
                crate::recall::format::short_id(c.new)
            ));
        }
        if self.total > self.candidates.len() {
            out.push_str(&format!(
                "\n  ({} more)",
                self.total - self.candidates.len()
            ));
        }
        out
    }

    /// The cosines, rounded as reported, for the `--ledger` completion line
    /// (counts and numbers only: no ids, no text).
    pub fn cosines(&self) -> Vec<f64> {
        self.candidates
            .iter()
            .map(|c| reported_cosine(c.cosine))
            .collect()
    }
}

/// Every condition but the vector ones: `old` may be listed for `new`.
/// Reuses the suspected-supersession predicates, so the two halves of #198
/// agree on type, eligibility, scope and what settles a pair.
pub fn eligible_pair(g: &Graph, new: &Concept, old: &Concept) -> bool {
    new.id != old.id
        && new.session_id == old.session_id
        && new.concept_type == old.concept_type
        && super::eligible_type(new.concept_type)
        && super::repo_scope(new) == super::repo_scope(old)
        && old.embedding_source.is_none()
        && !super::settled(g, new.id, old.id)
}

/// The candidates for the created concept `new` among `hits` (its vector
/// hits as the store returned them, in any order), at or above `floor`,
/// never naming a concept in `skip` (the write path passes the concept
/// `new` merged into and every concept the call itself wrote or
/// re-derived). Ranked by cosine, then id; at most
/// [`REPLACEMENT_CANDIDATE_LIMIT`]. Empty when `new` is not a concept of `g`.
pub fn select(
    g: &Graph,
    new: NodeId,
    hits: &[Scored<NodeId>],
    floor: f64,
    skip: &[NodeId],
) -> Vec<ReplacementCandidate> {
    let Some(Node::Concept(new_c)) = g.node(new) else {
        return Vec::new();
    };
    let mut kept: Vec<(&Concept, f64)> = Vec::new();
    for hit in hits {
        let score = hit.score;
        if !score.is_finite() || !(0.0..=1.0).contains(&score) || score < floor {
            continue;
        }
        if skip.contains(&hit.item) || kept.iter().any(|(c, _)| c.id == hit.item) {
            continue;
        }
        if let Some(Node::Concept(old)) = g.node(hit.item)
            && eligible_pair(g, new_c, old)
        {
            kept.push((old, score));
        }
    }
    kept.sort_by(|(a, x), (b, y)| y.total_cmp(x).then(a.id.0.cmp(&b.id.0)));
    kept.truncate(REPLACEMENT_CANDIDATE_LIMIT);
    kept.into_iter()
        .map(|(old, cosine)| ReplacementCandidate {
            new,
            id: old.id,
            excerpt: crate::recall::format::excerpt(&old.content),
            cosine,
        })
        .collect()
}

#[cfg(test)]
#[path = "replacement_tests.rs"]
mod tests;
