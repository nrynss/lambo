//! Redundancy-aware selection within `top_k` (#181).
//!
//! Assembly fills `top_k` slots in final-score order, and a reader sees two
//! or three hits. Two copies of one fact can take two of those slots:
//! near-duplicates exist by design (a semantic match writes its own node
//! plus a `Semantic` edge, `graph::hybrid`'s merge shape), and stores written
//! before embedding coverage hold identical texts with no edge at all (the
//! 2026-09-07 efficacy run's `Entity` and `Observation` copies of one
//! finding).
//!
//! ## The rule (skip if redundant)
//!
//! Opt-in (`[selection] skip_redundant = true`, [`RecallSelection`]).
//! After final scoring, assembly walks its members in recall order exactly
//! as before. Within the first [`window`]`(top_k)` ordinary members (at most
//! [`WINDOW_MAX`]), a member redundant with a hit already taken is skipped
//! and the next distinct member fills its slot; past the window nothing is
//! checked. Then `top_k` and `fit_blocks` apply as before. When nothing in
//! the window is redundant the walk takes exactly the members it took
//! without the rule, so the result is byte-identical.
//!
//! **Never skipped:** a forced member (hot-list, contradiction companion,
//! #19 canonical or venerable forcing, a kept reservation holder), and a
//! member that would carry a warning line of its own (a hot-list payload, a
//! #207 stale citation, a #20 supersession line, an active reservation):
//! skipping drops a member's block and warnings, and a duplicate's warning
//! (say, a contradiction declared against that copy) is not on the copy
//! that was kept. Canonical-first order is untouched: the rule only skips.
//!
//! ## Redundancy signals, cheapest first
//!
//! 1. **Same content** ([`normalized_content`]): equal after NFC, invisible
//!    codepoints stripped, lowercase and whitespace runs collapsed, across
//!    concept types. Required, not a fallback: the store's existing
//!    duplicates carry no `Semantic` edge. `canonical_key` is not reused: it
//!    sorts tokens, stems words and drops stopwords, so "X replaces Y" and
//!    "Y replaces X" share a key.
//! 2. **A `Semantic` edge** between the two, in either direction.
//! 3. **Cosine** at or above the session's effective
//!    `semantic_match_threshold` (the #87 calibrated or derived one), when
//!    both members carry a vector of the session's embedding contract. A
//!    member without one gives no cosine evidence; nothing is guessed.
//!
//! ## Never redundant
//!
//! Under any signal: a pair joined by a `Contradicts` edge either way
//! ([`never_redundant`], the one predicate #198's `SuspectedSupersedes`
//! extends), and a pair whose texts are different claims by their surface
//! ([`distinct_claims`]: different identifiers or digits, or a different
//! count of negation words). The text guard is trivially false for
//! identical content; it guards the edge signal too, because a `Semantic`
//! edge written before #193 can join two version-distinct facts.
//!
//! ## Cost
//!
//! Each checked member is compared with at most [`WINDOW_MAX`] taken hits,
//! so a recall makes at most `20 × 20` pair checks, and computes a cosine
//! only for a pair that has no cheaper signal and two vectors.

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::graph::Graph;
use crate::types::{Concept, EdgeType, NodeId};

/// The most ordinary members the rule checks, whatever `top_k` is.
pub const WINDOW_MAX: usize = 20;

/// Ordinary members checked per `top_k` slot, below [`WINDOW_MAX`].
pub const WINDOW_PER_SLOT: usize = 4;

/// The most skipped members a hit's label names (`also <id>`); the rest
/// are counted (`+N more`). Every one is in the structured detail.
pub const ALSO_MENTIONS: usize = 3;

/// How many ordinary members the rule checks for a recall of `top_k`:
/// `min(20, 4 · top_k)`.
pub fn window(top_k: usize) -> usize {
    WINDOW_MAX.min(top_k.saturating_mul(WINDOW_PER_SLOT))
}

/// `[selection]` in `lambo.toml`: how assembly fills `top_k` (#181).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecallSelection {
    /// Skip a member redundant with a hit already taken, so the next
    /// distinct member fills its slot. Default `false`: assembly takes the
    /// first `top_k` members in score order, as before.
    #[serde(default)]
    pub skip_redundant: bool,
}

/// Which signal found a skipped member redundant (cheapest first).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedundancySignal {
    /// The same text once normalized ([`normalized_content`]).
    SameContent,
    /// A `Semantic` edge joins the two.
    SemanticEdge,
    /// Their vectors are at or above the session's merge threshold.
    Cosine,
}

impl RedundancySignal {
    /// The wire name (`same_content`, `semantic_edge`, `cosine`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SameContent => "same_content",
            Self::SemanticEdge => "semantic_edge",
            Self::Cosine => "cosine",
        }
    }
}

/// A member skipped as redundant with the hit that names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedundantSkip {
    /// The skipped member.
    pub node_id: NodeId,
    /// What found it redundant.
    pub signal: RedundancySignal,
}

/// The text the same-content signal compares: NFC, invisible codepoints
/// ([`crate::graph::canonical::INVISIBLE_RANGES`]) stripped, lowercase,
/// whitespace runs collapsed to one space, trimmed. Nothing else is erased,
/// so identifiers, digits, punctuation, word order and negation all count.
pub fn normalized_content(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut gap = false;
    let visible = content
        .chars()
        .filter(|c| !crate::graph::canonical::is_invisible(*c));
    for c in visible.nfc() {
        if c.is_whitespace() {
            gap = true;
            continue;
        }
        if gap && !out.is_empty() {
            out.push(' ');
        }
        gap = false;
        out.extend(c.to_lowercase());
    }
    out
}

/// Words that negate (contractions are counted by their `n't`).
const NEGATIONS: [&str; 26] = [
    "not", "no", "never", "none", "nobody", "nothing", "nowhere", "neither", "nor", "cannot",
    "without", "dont", "doesnt", "didnt", "isnt", "arent", "wasnt", "werent", "cant", "couldnt",
    "wont", "wouldnt", "shouldnt", "mustnt", "hasnt", "havent",
];

/// How many negation words `content` holds: [`NEGATIONS`] and every word
/// ending in `n't` (either apostrophe), case-insensitive.
pub fn negation_count(content: &str) -> usize {
    crate::graph::canonical::fold(content)
        .replace('\u{2019}', "'")
        .split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .filter(|w| {
            let w = w.trim_matches('\'');
            w.ends_with("n't") || NEGATIONS.contains(&w)
        })
        .count()
}

/// Whether two texts are different claims by their surface alone: they
/// name different identifiers or digits
/// ([`crate::graph::canonical::same_identifiers`] false), or one negates
/// more often than the other. Such a pair is never redundant (#181), however
/// close its vectors or whatever edge joins it.
pub fn distinct_claims(a: &str, b: &str) -> bool {
    !crate::graph::canonical::same_identifiers(a, b) || negation_count(a) != negation_count(b)
}

/// Whether the graph records `a` and `b` as two different claims, so they
/// are never redundant under any signal: a `Contradicts` edge either way
/// ([`crate::daemon::contradiction::contradicts_either_way`]). A
/// contradiction written as a close paraphrase can carry a `Semantic` edge
/// too (the #179 repro); the newer fact must not be skipped as a duplicate
/// of the stale one.
///
/// #198's `SuspectedSupersedes` edge belongs here as a second arm, so a
/// suspected supersession is never redundant either.
pub fn never_redundant(graph: &Graph, a: NodeId, b: NodeId) -> bool {
    crate::daemon::contradiction::contradicts_either_way(graph, a, b)
}

/// The redundancy test of one session's recalls: its effective merge
/// threshold for the cosine signal.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Redundancy {
    /// Cosine at or above which two vectors are the same memory.
    pub(crate) threshold: f64,
}

impl Redundancy {
    /// The first signal that finds `b` redundant with `a`, cheapest first,
    /// or `None`; `None` too for a pair [`never_redundant`] or
    /// [`distinct_claims`] rules out. `norm` is each one's
    /// [`normalized_content`].
    pub(crate) fn signal(
        &self,
        graph: &Graph,
        (a, a_norm): (&Concept, &str),
        (b, b_norm): (&Concept, &str),
    ) -> Option<RedundancySignal> {
        if a.id == b.id {
            return None;
        }
        let signal = if a_norm == b_norm {
            RedundancySignal::SameContent
        } else if graph.edge_between(a.id, b.id, EdgeType::Semantic).is_some()
            || graph.edge_between(b.id, a.id, EdgeType::Semantic).is_some()
        {
            RedundancySignal::SemanticEdge
        } else if self.cosine_at_threshold(graph, a, b) {
            RedundancySignal::Cosine
        } else {
            return None;
        };
        if never_redundant(graph, a.id, b.id) || distinct_claims(&a.content, &b.content) {
            return None;
        }
        Some(signal)
    }

    /// Both members carry a vector of the session's contract width and
    /// their cosine is at or above the threshold. A member without one, or
    /// a session with no contract, gives no evidence.
    fn cosine_at_threshold(&self, graph: &Graph, a: &Concept, b: &Concept) -> bool {
        let Some(dim) = graph.embedding().map(|c| c.dim) else {
            return false;
        };
        match (a.embedding.as_deref(), b.embedding.as_deref()) {
            (Some(x), Some(y)) if dim > 0 && x.len() == dim && y.len() == dim => {
                f64::from(crate::embed::cosine(x, y)) >= self.threshold
            }
            _ => false,
        }
    }
}

/// The label meta items naming `skipped` (`also <short id>`, at most
/// [`ALSO_MENTIONS`], then `+N more`), in skip order. Empty for none, so a
/// hit that skipped nothing renders as before. The ids are
/// [`crate::recall::format::short_id`]s, valid inspect foci.
pub(crate) fn also_meta(skipped: &[RedundantSkip]) -> Vec<String> {
    let mut meta: Vec<String> = skipped
        .iter()
        .take(ALSO_MENTIONS)
        .map(|s| format!("also {}", crate::recall::format::short_id(s.node_id)))
        .collect();
    if skipped.len() > ALSO_MENTIONS {
        meta.push(format!("+{} more", skipped.len() - ALSO_MENTIONS));
    }
    meta
}

/// `skipped` as the wire and ledger carry it:
/// `[{"node_id": "<uuid>", "signal": "same_content"}, …]`, in skip order.
pub(crate) fn skips_json(skipped: &[RedundantSkip]) -> serde_json::Value {
    serde_json::Value::Array(
        skipped
            .iter()
            .map(|s| {
                serde_json::json!({
                    "node_id": s.node_id.0.to_string(),
                    "signal": s.signal.as_str(),
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests;
