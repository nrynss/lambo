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
//! Opt-in (`[selection] skip_redundant = true`, [`RecallSelection`]; the
//! signals beyond identical content need `similar_signals = true` too).
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
//! A dependency question answered by traversal (`recall::dispatch`) lists
//! an anchor's dependents without assembly, so the rule does not apply to
//! it.
//!
//! ## Redundancy signals, cheapest first
//!
//! 1. **Same content** ([`normalized_content`]): equal after NFC, invisible
//!    codepoints stripped and whitespace runs collapsed, case kept, across
//!    concept types. Required, not a fallback: the store's existing
//!    duplicates carry no `Semantic` edge. `canonical_key` is not reused: it
//!    sorts tokens, stems words and drops stopwords, so "X replaces Y" and
//!    "Y replaces X" share a key. **The only signal by default.**
//!
//! Only under `[selection] similar_signals = true`:
//!
//! 2. **A `Semantic` edge** between the two, in either direction.
//! 3. **Cosine** at or above the session's effective
//!    `semantic_match_threshold` (the #87 calibrated or derived one), when
//!    both members carry a vector of the session's embedding contract. A
//!    member without one gives no cosine evidence; nothing is guessed.
//!
//! The two are off by default (owner decision, 2026-10-28). Against the
//! same-content-only control on the #190 set (the archived run, under the
//! older guard) they added 3 facets over the 8 injected-duplicate cells
//! (split × `top_k` × budget) and lost 42 over the 8 labelled
//! related-but-distinct cells, every wrong skip from one of five pairs
//! hybrid derive had merged at write time (`p-can-02-rbd1`,
//! `p-can-02-rbd2`, `p-can-03-rbd1`, `p-con-05-rbd1`, `p-pro-01-rbd1`).
//!
//! ## Never redundant
//!
//! Under any signal: a pair joined by a `Contradicts` or (#198) a
//! `SuspectedSupersedes` edge either way ([`never_redundant`]), and a pair
//! whose texts are different claims by their surface
//! ([`distinct_claims`]: different identifiers (snake_case, CamelCase,
//! hex-like, all-caps, paths, versions) or digits of any script, a
//! different count of negation words, a word against its opposite, or
//! their shared content words in another order). The text guard is
//! trivially false for identical content; it guards the edge signal too,
//! because a `Semantic` edge written before #193 can join two
//! version-distinct facts. It is stricter than the write path's merge
//! guard (`graph::canonical::same_identifiers`), which it includes and
//! leaves unchanged.
//!
//! ## Cost
//!
//! Each checked member is compared with at most [`WINDOW_MAX`] taken hits.
//! Assembly may walk its members up to three times (the reservation,
//! cold-start and final passes), so each pair's verdict is memoized for the
//! whole assembly, and a recall computes at most [`PAIR_BUDGET`]
//! (`20 × 20`) verdicts across its passes; past that budget, a pair not yet
//! judged is not redundant. A cosine is computed only for a pair that has
//! no cheaper signal and two vectors.

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::graph::Graph;
use crate::types::{Concept, EdgeType, NodeId};

/// The most ordinary members the rule checks, whatever `top_k` is.
pub const WINDOW_MAX: usize = 20;

/// The most pair verdicts one recall computes (`WINDOW_MAX²`), across all
/// of its assembly passes.
pub const PAIR_BUDGET: usize = WINDOW_MAX * WINDOW_MAX;

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
    /// first `top_k` members in score order, as before. On its own it uses
    /// the same-content signal only.
    #[serde(default)]
    pub skip_redundant: bool,
    /// Also count a `Semantic` edge and cosine at or above the session's
    /// merge threshold as redundancy. Default `false`: on the #190 set they
    /// added 3 facets over 8 injected-duplicate cells and cost 42 over 8
    /// related-but-distinct cells, every wrong skip from a pair the write
    /// path had merged (`evidence/issue-181/`). Needs `skip_redundant`.
    #[serde(default)]
    pub similar_signals: bool,
}

impl RecallSelection {
    /// Refuse `similar_signals` without `skip_redundant`, which would
    /// otherwise read as on while doing nothing.
    pub fn validate(&self) -> Result<(), String> {
        if self.similar_signals && !self.skip_redundant {
            return Err(
                "[selection] similar_signals = true needs skip_redundant = true".to_string(),
            );
        }
        Ok(())
    }
}

/// Which signal found a skipped member redundant (cheapest first).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedundancySignal {
    /// The same text once normalized ([`normalized_content`], case kept).
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
/// ([`crate::graph::canonical::INVISIBLE_RANGES`]) stripped, whitespace
/// runs collapsed to one space, trimmed. Nothing else is erased, so case
/// (`src/Foo.rs` is not `src/foo.rs`, `PATH` is not `path`), identifiers,
/// digits, punctuation, word order and negation all count.
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
        out.push(c);
    }
    out
}

/// Words that negate (contractions are counted by their `n't`): the plain
/// negators, `avoid` and its forms, `unless`, and `non` (a hyphenated
/// `non-` splits into its own word).
const NEGATIONS: [&str; 32] = [
    "not", "no", "never", "none", "nobody", "nothing", "nowhere", "neither", "nor", "cannot",
    "without", "dont", "doesnt", "didnt", "isnt", "arent", "wasnt", "werent", "cant", "couldnt",
    "wont", "wouldnt", "shouldnt", "mustnt", "hasnt", "havent", "avoid", "avoids", "avoided",
    "avoiding", "unless", "non",
];

/// Pairs of words that state opposites; a text with one where the other
/// text has the other is a different claim. Prefix antonyms (`safe` /
/// `unsafe`) are found by [`ANTONYM_PREFIXES`] instead.
const ANTONYMS: [(&str, &str); 22] = [
    ("enable", "disable"),
    ("enabled", "disabled"),
    ("enables", "disables"),
    ("allow", "deny"),
    ("allowed", "denied"),
    ("allows", "denies"),
    ("accept", "reject"),
    ("accepted", "rejected"),
    ("include", "exclude"),
    ("included", "excluded"),
    ("add", "remove"),
    ("added", "removed"),
    ("true", "false"),
    ("before", "after"),
    ("increase", "decrease"),
    ("increased", "decreased"),
    ("minimum", "maximum"),
    ("min", "max"),
    ("start", "stop"),
    ("first", "last"),
    ("success", "failure"),
    ("pass", "fail"),
];

/// Prefixes that negate the word they precede: a text with `w` where the
/// other has `un` + `w` (`dis`, `non`, `in`, `im`) states the opposite.
const ANTONYM_PREFIXES: [&str; 5] = ["un", "dis", "non", "in", "im"];

/// Words the order check skips: they move with phrasing, not meaning.
const ORDER_STOPWORDS: [&str; 26] = [
    "a", "an", "the", "is", "are", "was", "were", "be", "been", "to", "of", "in", "on", "at",
    "for", "by", "with", "and", "or", "it", "its", "this", "that", "as", "do", "does",
];

/// The token a negation word becomes in [`claim_words`], so the order
/// check sees where a text negates.
const NEGATED: &str = "\u{ac}";

/// `content` with invisible codepoints stripped, then NFC: what every
/// surface check reads, so two texts that render alike read alike.
fn visible(content: &str) -> String {
    content
        .chars()
        .filter(|c| !crate::graph::canonical::is_invisible(*c))
        .nfc()
        .collect()
}

/// The words of `content`, lowercase, typographic apostrophes (U+2019,
/// U+2018, U+02BC) read as `'`, split on everything but letters, digits
/// and `'`, which is trimmed from each end.
fn words(content: &str) -> Vec<String> {
    visible(content)
        .to_lowercase()
        .replace(['\u{2019}', '\u{2018}', '\u{2BC}'], "'")
        .split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .map(|w| w.trim_matches('\'').to_string())
        .filter(|w| !w.is_empty())
        .collect()
}

fn is_negation(word: &str) -> bool {
    word.ends_with("n't") || NEGATIONS.contains(&word)
}

/// How many negation words `content` holds: [`NEGATIONS`] and every word
/// ending in `n't` (any apostrophe, U+02BC included), case-insensitive.
/// Invisible codepoints are ignored.
pub fn negation_count(content: &str) -> usize {
    words(content).iter().filter(|w| is_negation(w)).count()
}

/// The identifier-shaped tokens of `content`, case kept, in order. A token
/// is a whitespace-separated run with leading and trailing punctuation
/// trimmed (`_` kept); it is an identifier when it holds a digit of any
/// script (`is_numeric`, so full-width digits count), a `_`, `/` or `.`
/// (snake_case, paths, file names, versions), a lowercase letter followed
/// later by an uppercase one (CamelCase), at least two uppercase letters
/// and no lowercase (`PATH`, `API`), or is seven or more hex digits
/// (`deadbeef`, a hash without digits).
pub fn claim_identifiers(content: &str) -> Vec<String> {
    visible(content)
        .split_whitespace()
        .map(|t| t.trim_matches(|c: char| !(c.is_alphanumeric() || c == '_')))
        .filter(|t| is_claim_identifier(t))
        .map(str::to_string)
        .collect()
}

fn is_claim_identifier(t: &str) -> bool {
    if t.is_empty() {
        return false;
    }
    let numeric = t.chars().any(char::is_numeric);
    let joined = t.contains(['_', '/', '.']);
    let camel = t
        .chars()
        .skip_while(|c| !c.is_lowercase())
        .any(char::is_uppercase);
    let upper = t.chars().filter(|c| c.is_uppercase()).count();
    let caps = upper >= 2 && !t.chars().any(char::is_lowercase);
    let hex = t.len() >= 7 && t.chars().all(|c| c.is_ascii_hexdigit());
    numeric || joined || camel || caps || hex
}

/// The maximal runs of digits of any script (`is_numeric`), in order.
fn numeric_runs(content: &str) -> Vec<String> {
    visible(content)
        .split(|c: char| !c.is_numeric())
        .filter(|r| !r.is_empty())
        .map(str::to_string)
        .collect()
}

/// [`words`] with every negation word replaced by one marker, for the
/// order and antonym checks.
fn claim_words(content: &str) -> Vec<String> {
    words(content)
        .into_iter()
        .map(|w| {
            if is_negation(&w) {
                NEGATED.to_string()
            } else {
                w
            }
        })
        .collect()
}

/// Whether the content words two texts share (negation included, the
/// [`ORDER_STOPWORDS`] aside) come in the same order in both, each counted
/// at its first occurrence. "warn replaces verify" and "verify replaces
/// warn" do not; neither do "use A, not B" and "do not use A".
fn same_order(a: &[String], b: &[String]) -> bool {
    fn shared<'w>(x: &'w [String], y: &[String]) -> Vec<&'w str> {
        let mut seen = std::collections::HashSet::new();
        x.iter()
            .map(String::as_str)
            .filter(|w| !ORDER_STOPWORDS.contains(w) && y.iter().any(|v| v == w))
            .filter(|w| seen.insert(*w))
            .collect()
    }
    shared(a, b) == shared(b, a)
}

/// Whether one text has a word where the other has its opposite: an
/// [`ANTONYMS`] pair, or a word and its [`ANTONYM_PREFIXES`] form.
fn opposite_words(a: &[String], b: &[String]) -> bool {
    use std::collections::HashSet;
    let (sa, sb): (HashSet<&str>, HashSet<&str>) = (
        a.iter().map(String::as_str).collect(),
        b.iter().map(String::as_str).collect(),
    );
    // `x` in one text and `y` in the other, which lacks `x`.
    let swapped = |x: &str, y: &str| {
        (sa.contains(x) && sb.contains(y) && !sb.contains(x))
            || (sb.contains(x) && sa.contains(y) && !sa.contains(x))
    };
    if ANTONYMS
        .iter()
        .any(|&(x, y)| swapped(x, y) || swapped(y, x))
    {
        return true;
    }
    sa.iter().chain(&sb).any(|w| {
        w.chars().count() >= 3
            && w.chars().all(char::is_alphabetic)
            && ANTONYM_PREFIXES
                .iter()
                .any(|p| swapped(w, &format!("{p}{w}")))
    })
}

/// Whether two texts are different claims by their surface alone, so the
/// pair is never redundant (#181) however close its vectors or whatever
/// edge joins it. Stricter than the write path's merge guard
/// ([`crate::graph::canonical::same_identifiers`], which it includes):
///
/// * different [`claim_identifiers`] (snake_case, CamelCase, hex-like,
///   all-caps, paths, versions; case kept) or different digit runs of any
///   script;
/// * a different [`negation_count`];
/// * a word in one where the other has its opposite (`enable` / `disable`,
///   `safe` / `unsafe`: [`ANTONYMS`], [`ANTONYM_PREFIXES`]);
/// * the content words both share, negation included, in a different
///   order (swapped roles, or a negation that moved).
///
/// Identical texts are never distinct claims.
pub fn distinct_claims(a: &str, b: &str) -> bool {
    if !crate::graph::canonical::same_identifiers(a, b)
        || claim_identifiers(a) != claim_identifiers(b)
        || numeric_runs(a) != numeric_runs(b)
        || negation_count(a) != negation_count(b)
    {
        return true;
    }
    let (wa, wb) = (claim_words(a), claim_words(b));
    opposite_words(&wa, &wb) || !same_order(&wa, &wb)
}

/// Whether the graph records `a` and `b` as two different claims, so they
/// are never redundant under any signal: a `Contradicts` edge either way
/// ([`crate::daemon::contradiction::contradicts_either_way`]), or a
/// `SuspectedSupersedes` edge either way
/// ([`crate::graph::suspected::is_suspected_pair`], owner decision on
/// #198). A contradiction written as a close paraphrase can carry a
/// `Semantic` edge too (the #179 repro), and a suspected pair is a near
/// vector match by construction; either way the newer fact must not be
/// skipped as a duplicate of the stale one.
pub fn never_redundant(graph: &Graph, a: NodeId, b: NodeId) -> bool {
    crate::daemon::contradiction::contradicts_either_way(graph, a, b)
        || crate::graph::suspected::is_suspected_pair(graph, a, b)
}

#[cfg(test)]
thread_local! {
    /// How many times [`Redundancy::signal`] ran on this thread, for the
    /// pair-budget test.
    pub(crate) static SIGNAL_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The redundancy test of one session's recalls: which signals count, and
/// its effective merge threshold for the cosine signal.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Redundancy {
    /// Cosine at or above which two vectors are the same memory. Read only
    /// when `similar` is set.
    pub(crate) threshold: f64,
    /// `[selection] similar_signals`: the `Semantic`-edge and cosine
    /// signals count too. Without it only the same content does.
    pub(crate) similar: bool,
}

impl Redundancy {
    /// The test a session under `selection` applies, `None` when it does
    /// not skip redundant members (the default). `threshold` is the
    /// session's effective merge threshold.
    pub(crate) fn of(selection: &RecallSelection, threshold: f64) -> Option<Self> {
        selection.skip_redundant.then_some(Self {
            threshold,
            similar: selection.similar_signals,
        })
    }

    /// The first signal that finds `b` redundant with `a`, cheapest first,
    /// or `None`; `None` too for a pair [`never_redundant`] or
    /// [`distinct_claims`] rules out. Without `similar`, only
    /// [`RedundancySignal::SameContent`] is tried. `norm` is each one's
    /// [`normalized_content`].
    pub(crate) fn signal(
        &self,
        graph: &Graph,
        (a, a_norm): (&Concept, &str),
        (b, b_norm): (&Concept, &str),
    ) -> Option<RedundancySignal> {
        #[cfg(test)]
        SIGNAL_CALLS.with(|n| n.set(n.get() + 1));
        if a.id == b.id {
            return None;
        }
        let signal = if a_norm == b_norm {
            RedundancySignal::SameContent
        } else if !self.similar {
            return None;
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

/// [`also_meta`] for a farther tier's copy merged into a layered hit: the
/// members its own session skipped for it, `also <short id> in <session>`
/// (at most [`ALSO_MENTIONS`], then `+N more in <session>`), so a reader
/// knows which session each id resolves in. Empty for none.
pub(crate) fn also_in_meta(skipped: &[RedundantSkip], session: &str) -> Vec<String> {
    let mut meta: Vec<String> = skipped
        .iter()
        .take(ALSO_MENTIONS)
        .map(|s| {
            format!(
                "also {} in {session}",
                crate::recall::format::short_id(s.node_id)
            )
        })
        .collect();
    if skipped.len() > ALSO_MENTIONS {
        meta.push(format!(
            "+{} more in {session}",
            skipped.len() - ALSO_MENTIONS
        ));
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
mod assembly_tests;
#[cfg(test)]
mod tests;
