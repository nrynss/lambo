//! The #190 evaluation set's file formats and its schema check.
//!
//! Shared by `tests/eval_set.rs` (which runs [`validate`] in CI) and the
//! `eval_measure` example (which refuses to measure a set that fails it).
//! Kept dependency-free beyond serde so both can include it with `#[path]`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Deserialize;

/// Pair labels.
pub const LABELS: [&str; 3] = ["related_distinct", "paraphrase", "unrelated"];
/// Pair families allowed under each label.
pub const RBD_FAMILIES: [&str; 7] = [
    "identifier",
    "quantity",
    "polarity",
    "repository",
    "correction",
    "exception",
    "topic-neighbour",
];
pub const PARAPHRASE_FAMILIES: [&str; 4] = ["rewording", "terse", "near-verbatim", "same-event"];
pub const UNRELATED_FAMILIES: [&str; 1] = ["cross-area"];
/// Families whose pairs must say how the two texts relate (#198).
pub const RELATION_REQUIRED: [&str; 2] = ["identifier", "correction"];
pub const RELATIONS: [&str; 3] = ["supersedes", "distinct_entity", "contradicts"];
/// What a pair's `concept_types` may name: the concept type each text would
/// be derived as (#198's trigger compares them).
pub const CONCEPT_TYPES: [&str; 5] = ["Entity", "Logic", "Constraint", "Resource", "Observation"];
/// Query families.
pub const ANSWERABLE_FAMILIES: [&str; 7] = [
    "fact-lookup",
    "vocabulary-mismatch",
    "exact-identifier",
    "multi-hop",
    "contrast",
    "superseded",
    "rationale",
];
/// `ledger-miss` is reserved for the real ledger misses the owner extracts.
pub const NO_ANSWER_FAMILIES: [&str; 5] = [
    "near-domain",
    "false-premise",
    "off-domain",
    "efficacy",
    "ledger-miss",
];
pub const SPLITS: [&str; 2] = ["dev", "held_out"];
pub const ORIGINS: [&str; 2] = ["dogfood", "authored"];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Concept {
    pub id: String,
    pub topic: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub text: String,
    pub origin: String,
    pub sources: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Relevant {
    pub concept: String,
    pub grade: u8,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub id: String,
    pub query: String,
    pub kind: String,
    pub family: String,
    pub topic: String,
    pub split: String,
    pub relevant: Vec<Relevant>,
    pub facets: Vec<String>,
    pub source: String,
    pub labeller: String,
    pub labelled_on: String,
    #[serde(default)]
    pub review: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pair {
    pub id: String,
    pub a: String,
    pub b: String,
    pub label: String,
    pub family: String,
    #[serde(default)]
    pub relation: Option<String>,
    /// `[type of a, type of b]` (#198), required on a `supersedes` pair,
    /// whose `a` is the newer text.
    #[serde(default)]
    pub concept_types: Vec<String>,
    pub split: String,
    pub source: String,
    pub labeller: String,
    pub labelled_on: String,
    #[serde(default)]
    pub concepts: Vec<String>,
    #[serde(default)]
    pub review: Option<String>,
}

/// The `[embedder] related_pairs` file format.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportPair {
    pub a: String,
    pub b: String,
}

pub struct EvalSet {
    pub corpus: Vec<Concept>,
    pub queries: Vec<Query>,
    pub pairs: Vec<Pair>,
    pub export: Vec<ExportPair>,
}

fn read<T: serde::de::DeserializeOwned>(dir: &Path, name: &str) -> Result<T, String> {
    let path = dir.join(name);
    let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn load(dir: &Path) -> Result<EvalSet, String> {
    Ok(EvalSet {
        corpus: read(dir, "corpus.json")?,
        queries: read(dir, "queries.json")?,
        pairs: read(dir, "pairs.json")?,
        export: read(dir, "related_pairs.json")?,
    })
}

impl Pair {
    /// Counted in reported numbers and exported: not awaiting the owner.
    pub fn adjudicated(&self) -> bool {
        self.review.is_none()
    }
}

impl Query {
    pub fn adjudicated(&self) -> bool {
        self.review.is_none()
    }
}

/// Id prefix of the pairs added for #198's suspected-supersession trigger.
/// They stay out of the export: the identifier guard refuses every one of
/// them before any threshold applies, so they barely move the threshold the
/// export derives, while they lengthen the canary's embed past its timeout.
pub const ISSUE_198_PREFIX: &str = "p-198-";

/// Whether the export carries this pair: an adjudicated related-but-distinct
/// pair that is not one of #198's.
pub fn exported(p: &Pair) -> bool {
    p.label == "related_distinct" && p.adjudicated() && !p.id.starts_with(ISSUE_198_PREFIX)
}

/// The related-but-distinct pairs the export must hold, in order.
pub fn expected_export(pairs: &[Pair]) -> Vec<ExportPair> {
    pairs
        .iter()
        .filter(|p| exported(p))
        .map(|p| ExportPair {
            a: p.a.clone(),
            b: p.b.clone(),
        })
        .collect()
}

fn check_text(errors: &mut Vec<String>, what: &str, text: &str, max_bytes: usize) {
    if text.trim().is_empty() {
        errors.push(format!("{what}: empty text"));
    }
    if text.len() > max_bytes {
        errors.push(format!(
            "{what}: {} bytes, over the {max_bytes}-byte concept cap",
            text.len()
        ));
    }
    if text.contains("[REDACTED") {
        errors.push(format!("{what}: carries a redaction marker"));
    }
}

fn check_common(
    errors: &mut Vec<String>,
    what: &str,
    split: &str,
    labeller: &str,
    on: &str,
    review: Option<&str>,
) {
    if !SPLITS.contains(&split) {
        errors.push(format!("{what}: unknown split {split:?}"));
    }
    if labeller.trim().is_empty() {
        errors.push(format!("{what}: no labeller"));
    }
    let date_ok = on.len() == 10
        && on.bytes().enumerate().all(|(i, b)| {
            if i == 4 || i == 7 {
                b == b'-'
            } else {
                b.is_ascii_digit()
            }
        });
    if !date_ok {
        errors.push(format!("{what}: labelled_on {on:?} is not YYYY-MM-DD"));
    }
    if review.is_some_and(|r| r.trim().is_empty()) {
        errors.push(format!("{what}: an empty review note"));
    }
}

/// Every schema rule; returns all violations, not just the first.
pub fn validate(set: &EvalSet, max_bytes: usize) -> Vec<String> {
    let mut errors = Vec::new();

    let mut ids = BTreeSet::new();
    let mut texts = BTreeSet::new();
    for c in &set.corpus {
        let what = format!("corpus {}", c.id);
        if !ids.insert(c.id.as_str()) {
            errors.push(format!("{what}: duplicate id"));
        }
        if !texts.insert(c.text.as_str()) {
            errors.push(format!("{what}: duplicate text"));
        }
        if !ORIGINS.contains(&c.origin.as_str()) {
            errors.push(format!("{what}: unknown origin {:?}", c.origin));
        }
        if c.sources.is_empty() || c.sources.iter().any(|s| s.trim().is_empty()) {
            errors.push(format!("{what}: missing source"));
        }
        if c.topic.trim().is_empty() || c.kind.trim().is_empty() {
            errors.push(format!("{what}: missing topic or type"));
        }
        check_text(&mut errors, &what, &c.text, max_bytes);
    }

    let mut qids = BTreeSet::new();
    let mut qtexts = BTreeSet::new();
    for q in &set.queries {
        let what = format!("query {}", q.id);
        if !qids.insert(q.id.as_str()) {
            errors.push(format!("{what}: duplicate id"));
        }
        if !qtexts.insert(q.query.as_str()) {
            errors.push(format!("{what}: duplicate query text"));
        }
        check_text(&mut errors, &what, &q.query, max_bytes);
        check_common(
            &mut errors,
            &what,
            &q.split,
            &q.labeller,
            &q.labelled_on,
            q.review.as_deref(),
        );
        if q.source.trim().is_empty() || q.topic.trim().is_empty() {
            errors.push(format!("{what}: missing source or topic"));
        }
        let mut seen = BTreeSet::new();
        for r in &q.relevant {
            if !ids.contains(r.concept.as_str()) {
                errors.push(format!("{what}: unknown relevant concept {}", r.concept));
            }
            if !seen.insert(r.concept.as_str()) {
                errors.push(format!("{what}: {} listed twice", r.concept));
            }
            if !(1..=2).contains(&r.grade) {
                errors.push(format!(
                    "{what}: grade {} for {} is not 1 or 2",
                    r.grade, r.concept
                ));
            }
        }
        match q.kind.as_str() {
            "answerable" => {
                if !ANSWERABLE_FAMILIES.contains(&q.family.as_str()) {
                    errors.push(format!("{what}: unknown answerable family {:?}", q.family));
                }
                if !q.relevant.iter().any(|r| r.grade == 2) {
                    errors.push(format!("{what}: answerable with no grade-2 concept"));
                }
                if q.facets.is_empty() {
                    errors.push(format!("{what}: answerable with no answer facets"));
                }
            }
            "no_answer" => {
                if !NO_ANSWER_FAMILIES.contains(&q.family.as_str()) {
                    errors.push(format!("{what}: unknown no-answer family {:?}", q.family));
                }
                if !q.relevant.is_empty() || !q.facets.is_empty() {
                    errors.push(format!(
                        "{what}: a no-answer query lists relevant concepts or facets"
                    ));
                }
            }
            other => errors.push(format!("{what}: unknown kind {other:?}")),
        }
    }

    let mut pids = BTreeSet::new();
    let mut seen_pairs = BTreeSet::new();
    for p in &set.pairs {
        let what = format!("pair {}", p.id);
        if !pids.insert(p.id.as_str()) {
            errors.push(format!("{what}: duplicate id"));
        }
        let key = if p.a <= p.b {
            (p.a.as_str(), p.b.as_str())
        } else {
            (p.b.as_str(), p.a.as_str())
        };
        if !seen_pairs.insert(key) {
            errors.push(format!("{what}: the same two texts appear in another pair"));
        }
        if p.a == p.b {
            errors.push(format!("{what}: identical texts"));
        }
        check_text(&mut errors, &format!("{what} a"), &p.a, max_bytes);
        check_text(&mut errors, &format!("{what} b"), &p.b, max_bytes);
        check_common(
            &mut errors,
            &what,
            &p.split,
            &p.labeller,
            &p.labelled_on,
            p.review.as_deref(),
        );
        if p.source.trim().is_empty() {
            errors.push(format!("{what}: missing source"));
        }
        if !LABELS.contains(&p.label.as_str()) {
            errors.push(format!("{what}: unknown label {:?}", p.label));
        }
        let families: &[&str] = match p.label.as_str() {
            "related_distinct" => &RBD_FAMILIES,
            "paraphrase" => &PARAPHRASE_FAMILIES,
            "unrelated" => &UNRELATED_FAMILIES,
            _ => &[],
        };
        if !families.is_empty() && !families.contains(&p.family.as_str()) {
            errors.push(format!(
                "{what}: family {:?} is not allowed under {}",
                p.family, p.label
            ));
        }
        match p.relation.as_deref() {
            Some(r) if !RELATIONS.contains(&r) => {
                errors.push(format!("{what}: unknown relation {r:?}"))
            }
            Some(_) if p.label != "related_distinct" => {
                errors.push(format!("{what}: a relation on a {} pair", p.label))
            }
            None if RELATION_REQUIRED.contains(&p.family.as_str()) => {
                errors.push(format!("{what}: family {} needs a relation", p.family))
            }
            _ => {}
        }
        if !p.concept_types.is_empty()
            && (p.concept_types.len() != 2
                || p.concept_types
                    .iter()
                    .any(|t| !CONCEPT_TYPES.contains(&t.as_str())))
        {
            errors.push(format!(
                "{what}: concept_types {:?} is not two of {CONCEPT_TYPES:?}",
                p.concept_types
            ));
        }
        if p.relation.as_deref() == Some("supersedes") && p.concept_types.is_empty() {
            errors.push(format!("{what}: a supersedes pair needs concept_types"));
        }
        for c in &p.concepts {
            if !ids.contains(c.as_str()) {
                errors.push(format!("{what}: unknown concept {c}"));
            }
        }
    }

    if set.export != expected_export(&set.pairs) {
        errors.push(
            "related_pairs.json is not the adjudicated, non-#198 related_distinct pairs of pairs.json, in order \
             (rerun fixtures/eval/authoring/build.py)"
                .to_string(),
        );
    }
    errors
}

/// Counts by a key, for reports.
pub fn count_by<'a, T>(items: &'a [T], key: impl Fn(&'a T) -> String) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    for i in items {
        *m.entry(key(i)).or_default() += 1;
    }
    m
}
