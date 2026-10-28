//! The text half of the suspected-supersession trigger (#198): **one span
//! and value slot**.
//!
//! Two texts are a candidate supersession, by their words alone, when both
//! of these hold (owner decision, 2026-10-27, replacing the masked-text
//! Jaccard condition):
//!
//! 1. **One span.** Aligning the two token sequences leaves exactly one
//!    differing span ([`differing_spans`]).
//! 2. **Value slot.** In *both* texts that span starts strictly after the
//!    first predicate cue ([`first_cue`]): a word of [`PREDICATE_CUES`] or a
//!    token ending in `:`. A change before the first cue is in the subject
//!    position, which names a different thing, not a new value of the same
//!    thing ("J1 round-1 verdict: …" against "J2 round-1 verdict: …").
//!
//! The tokenisation, normalisation and alignment reproduce the measured
//! reference script (`evidence/issue-198-trigger/slot.py`) exactly:
//! whitespace tokens, each compared after stripping `.,;()'"` from both
//! ends and lowercasing, aligned with a port of Python's
//! `difflib.SequenceMatcher` (no junk heuristic, `autojunk=False`), so
//! the Rust trigger and the Python measurement agree pair for pair.
//!
//! **Caveat, carried with the rule:** [`PREDICATE_CUES`] is a hand-written
//! English lexicon. A predicate verb missing from it makes a real
//! supersession look like a subject change (a miss, never a false
//! warning). Extend it only against the dev split of the #190 set, never
//! the held-out split.

use std::collections::HashMap;

/// The predicate cues of the value-slot rule (#198): words after which a
/// sentence states its value. Copulas, common predicate verbs (present and
/// past), `=` and `:`, and the prepositions that introduce a value in the
/// #190 templates (`at`, `to`, `in`, `of`, `now`). Compared against a token
/// after [`normalize`]; a token *ending* in `:` is a cue as well, whatever
/// precedes the colon.
///
/// It is the reference script's list (`evidence/issue-198-trigger/slot.py`)
/// plus `reports`, added on the dev split only: its omission made the dev
/// supersession `p-198-s34` ("The demo's user schema reports 15
/// dependents.") a miss. The evidence README carries both measurements.
pub const PREDICATE_CUES: &[&str] = &[
    // Copulas.
    "is",
    "are",
    "was",
    "were",
    "be",
    "been",
    "becomes",
    "became",
    "equals",
    // Separators that introduce a value.
    "=",
    ":",
    // Predicate verbs.
    "runs",
    "ran",
    "serves",
    "served",
    "uses",
    "used",
    "pins",
    "pinned",
    "defaults",
    "default",
    "takes",
    "took",
    "holds",
    "held",
    "has",
    "have",
    "had",
    "sets",
    "set",
    "reads",
    "returns",
    "gives",
    "gave",
    "scores",
    "scored",
    "applies",
    "applied",
    "acknowledged",
    "measured",
    "caps",
    "capped",
    "listens",
    "listened",
    "binds",
    "bound",
    "points",
    "pointed",
    "lands",
    "landed",
    "ships",
    "shipped",
    "stays",
    "reports",
    // Value-introducing prepositions and adverbs.
    "now",
    "at",
    "to",
    "in",
    "of",
];

/// The characters [`normalize`] strips from both ends of a token.
const STRIP: &[char] = &['.', ',', ';', '(', ')', '\'', '"'];

/// A text's tokens: its whitespace-separated words, verbatim.
pub fn tokens(text: &str) -> Vec<&str> {
    text.split_whitespace().collect()
}

/// A token as the alignment and the cue lookup compare it: `.,;()'"`
/// stripped from both ends, then lowercased.
pub fn normalize(token: &str) -> String {
    token.trim_matches(STRIP).to_lowercase()
}

/// The index of the first predicate cue among `tokens`: the first token
/// whose [normalized](normalize) form is in [`PREDICATE_CUES`], or which
/// ends in `:`. `None` when the text has none.
pub fn first_cue(tokens: &[&str]) -> Option<usize> {
    first_cue_in(tokens, PREDICATE_CUES)
}

/// [`first_cue`] against another lexicon (the measurement compares the
/// reference script's list without `reports`).
pub fn first_cue_in(tokens: &[&str], cues: &[&str]) -> Option<usize> {
    tokens.iter().position(|t| {
        let n = normalize(t);
        cues.contains(&n.as_str()) || t.ends_with(':')
    })
}

/// One differing span of an alignment: tokens `a[a_start..a_end]` of the
/// first text stand where `b[b_start..b_end]` of the second do (one range
/// may be empty: an insertion or a deletion).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    /// First differing token in the first text.
    pub a_start: usize,
    /// One past the last, in the first text.
    pub a_end: usize,
    /// First differing token in the second text.
    pub b_start: usize,
    /// One past the last, in the second text.
    pub b_end: usize,
}

/// The differing spans of two token sequences, in order: the non-`equal`
/// opcodes of Python's `difflib.SequenceMatcher(a, b, autojunk=False)`,
/// ported exactly (longest matching block first, earliest on ties, then
/// recursively on either side). Tokens are compared [normalized](normalize).
pub fn differing_spans(a: &[&str], b: &[&str]) -> Vec<Span> {
    let a: Vec<String> = a.iter().map(|t| normalize(t)).collect();
    let b: Vec<String> = b.iter().map(|t| normalize(t)).collect();
    let mut spans = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    for (ai, bj, size) in matching_blocks(&a, &b) {
        if i < ai || j < bj {
            spans.push(Span {
                a_start: i,
                a_end: ai,
                b_start: j,
                b_end: bj,
            });
        }
        i = ai + size;
        j = bj + size;
    }
    spans
}

/// `SequenceMatcher.get_matching_blocks`: maximal matching blocks
/// `(i, j, size)`, sorted, adjacent ones collapsed, ending with the
/// `(len a, len b, 0)` sentinel.
fn matching_blocks(a: &[String], b: &[String]) -> Vec<(usize, usize, usize)> {
    let mut b2j: HashMap<&str, Vec<usize>> = HashMap::new();
    for (j, t) in b.iter().enumerate() {
        b2j.entry(t.as_str()).or_default().push(j);
    }
    let mut queue = vec![(0, a.len(), 0, b.len())];
    let mut blocks: Vec<(usize, usize, usize)> = Vec::new();
    while let Some((alo, ahi, blo, bhi)) = queue.pop() {
        let (i, j, k) = longest_match(a, b, &b2j, alo, ahi, blo, bhi);
        if k > 0 {
            blocks.push((i, j, k));
            if alo < i && blo < j {
                queue.push((alo, i, blo, j));
            }
            if i + k < ahi && j + k < bhi {
                queue.push((i + k, ahi, j + k, bhi));
            }
        }
    }
    blocks.sort_unstable();
    let mut collapsed: Vec<(usize, usize, usize)> = Vec::with_capacity(blocks.len() + 1);
    let (mut i1, mut j1, mut k1) = (0, 0, 0);
    for (i2, j2, k2) in blocks {
        if i1 + k1 == i2 && j1 + k1 == j2 {
            k1 += k2;
        } else {
            if k1 > 0 {
                collapsed.push((i1, j1, k1));
            }
            (i1, j1, k1) = (i2, j2, k2);
        }
    }
    if k1 > 0 {
        collapsed.push((i1, j1, k1));
    }
    collapsed.push((a.len(), b.len(), 0));
    collapsed
}

/// `SequenceMatcher.find_longest_match` with no junk: the longest block of
/// equal tokens in `a[alo..ahi]` and `b[blo..bhi]`, the earliest in `a`
/// (then in `b`) on ties.
fn longest_match(
    a: &[String],
    b: &[String],
    b2j: &HashMap<&str, Vec<usize>>,
    alo: usize,
    ahi: usize,
    blo: usize,
    bhi: usize,
) -> (usize, usize, usize) {
    let (mut best_i, mut best_j, mut best_size) = (alo, blo, 0usize);
    // j2len[j]: length of the match ending at a[i - 1] and b[j].
    let mut j2len: HashMap<usize, usize> = HashMap::new();
    for (i, token) in a.iter().enumerate().take(ahi).skip(alo) {
        let mut next: HashMap<usize, usize> = HashMap::new();
        if let Some(js) = b2j.get(token.as_str()) {
            for &j in js {
                if j < blo {
                    continue;
                }
                if j >= bhi {
                    break;
                }
                let k = j
                    .checked_sub(1)
                    .and_then(|p| j2len.get(&p))
                    .copied()
                    .unwrap_or(0)
                    + 1;
                next.insert(j, k);
                if k > best_size {
                    (best_i, best_j, best_size) = (i + 1 - k, j + 1 - k, k);
                }
            }
        }
        j2len = next;
    }
    // difflib then extends the block over equal neighbours; with no junk a
    // longer run would already have been found, so these never move it.
    while best_i > alo && best_j > blo && a[best_i - 1] == b[best_j - 1] {
        best_i -= 1;
        best_j -= 1;
        best_size += 1;
    }
    while best_i + best_size < ahi
        && best_j + best_size < bhi
        && a[best_i + best_size] == b[best_j + best_size]
    {
        best_size += 1;
    }
    (best_i, best_j, best_size)
}

/// What the text rule found for one pair of texts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotVerdict {
    /// How many differing spans the alignment left.
    pub spans: usize,
    /// Whether at least one span differs and every differing span starts
    /// strictly after the first predicate cue in both texts.
    pub value_slot: bool,
}

impl SlotVerdict {
    /// The rule: exactly one differing span, in the value slot.
    pub fn suspected(self) -> bool {
        self.spans == 1 && self.value_slot
    }
}

/// Judge two texts by the one-span and value-slot rule ([`SlotVerdict`]),
/// `a` aligned against `b` as the reference script does. The trigger passes
/// the older text first.
pub fn judge(a: &str, b: &str) -> SlotVerdict {
    judge_with(a, b, PREDICATE_CUES)
}

/// [`judge`] against another cue lexicon, for measuring a change to
/// [`PREDICATE_CUES`] before making it.
pub fn judge_with(a: &str, b: &str, cues: &[&str]) -> SlotVerdict {
    let (ta, tb) = (tokens(a), tokens(b));
    let spans = differing_spans(&ta, &tb);
    let value_slot = match (first_cue_in(&ta, cues), first_cue_in(&tb, cues)) {
        (Some(ca), Some(cb)) => {
            !spans.is_empty() && spans.iter().all(|s| s.a_start > ca && s.b_start > cb)
        }
        _ => false,
    };
    SlotVerdict {
        spans: spans.len(),
        value_slot,
    }
}

/// Whether two texts differ by one value-slot span: [`judge`]`(a,
/// b).suspected()`.
pub fn one_span_value_slot(a: &str, b: &str) -> bool {
    judge(a, b).suspected()
}
