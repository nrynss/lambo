//! #198: measure the Rust suspected-supersession trigger on the #211 pairs.
//!
//! Runs `lambo::graph::suspected`'s text rule (one span and value slot)
//! and the real identifier guard (`same_identifiers`) over the in-regime
//! pairs of `fixtures/eval/pairs.json`, with each pair's BGE-M3 cosine read
//! from a per-pair cosine file (`evidence/issue-198-trigger/
//! cosines-bge-m3-self.json`, the self-framed rows of an `eval_measure`
//! run), and prints the same table as `evidence/issue-198-trigger/slot.py`
//! so the two can be compared line for line.
//!
//! In regime, as in the script: both texts the same `Logic`, `Constraint`
//! or `Observation` type, not awaiting review, refused by the identifier
//! guard, labelled `supersedes`, `distinct_entity` or `same-event`, and at
//! cosine >= 0.85. A flagged `supersedes` pair is a true positive; a flagged
//! `distinct_entity` or `same-event` pair is a false positive (owner
//! decision, 2026-10-27).
//!
//! ```text
//! cargo run --example suspected_measure
//! cargo run --example suspected_measure -- <pairs.json> <cosines.json>
//! ```
//!
//! It is run by hand (it reads committed files and needs no model), and is
//! compiled by every `--all-targets` CI row. `tests/suspected_measure.rs`
//! includes this file and asserts the shipped lexicon's per-split counts
//! through the same [`rows`] and [`tally`], so the measurement is in CI.

use std::collections::BTreeMap;

use lambo::graph::canonical::same_identifiers;
use lambo::graph::suspected::slot::{judge_with, SlotVerdict};
use lambo::graph::suspected::PREDICATE_CUES;
use serde_json::Value;

const THRESHOLD: f64 = 0.85;

/// One in-regime pair and the trigger's verdicts on it.
pub struct Row {
    pub id: String,
    pub kind: String,
    pub split: String,
    pub verdict_shipped: SlotVerdict,
    pub verdict_reference: SlotVerdict,
}

/// One split's counts under one rule.
pub struct Tally {
    pub tp: usize,
    pub fp: usize,
    /// `supersedes` pairs in the split.
    pub positives: usize,
    /// Flagged pairs that are not `supersedes`.
    pub false_positives: Vec<String>,
    /// `supersedes` pairs not flagged.
    pub missed: Vec<String>,
}

/// Count `split` of `rows` under `rule`.
pub fn tally(rows: &[Row], split: &str, rule: impl Fn(&Row) -> bool) -> Tally {
    let mut t = Tally {
        tp: 0,
        fp: 0,
        positives: 0,
        false_positives: Vec::new(),
        missed: Vec::new(),
    };
    for r in rows.iter().filter(|r| r.split == split) {
        let positive = r.kind == "supersedes";
        let flagged = rule(r);
        t.positives += usize::from(positive);
        match (flagged, positive) {
            (true, true) => t.tp += 1,
            (true, false) => {
                t.fp += 1;
                t.false_positives.push(r.id.clone());
            }
            (false, true) => t.missed.push(r.id.clone()),
            (false, false) => {}
        }
    }
    t
}

fn wilson_lo(k: usize, n: usize) -> f64 {
    if n == 0 {
        return f64::NAN;
    }
    let (k, n, z) = (k as f64, n as f64, 1.96_f64);
    let ph = k / n;
    (ph + z * z / (2.0 * n) - z * ((ph * (1.0 - ph) + z * z / (4.0 * n)) / n).sqrt())
        / (1.0 + z * z / n)
}

fn read(path: &str) -> Value {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

fn report(name: &str, rows: &[Row], rule: impl Fn(&Row) -> bool, list: bool) {
    println!("\n== {name}");
    for split in ["dev", "held_out"] {
        let t = tally(rows, split, &rule);
        let flagged = t.tp + t.fp;
        let precision = if flagged == 0 {
            f64::NAN
        } else {
            t.tp as f64 / flagged as f64
        };
        println!(
            "  {split:8} TP {:2} FP {:2} precision {precision:.2} (95% low {:.2}) recall {}/{}",
            t.tp,
            t.fp,
            wilson_lo(t.tp, flagged),
            t.tp,
            t.positives
        );
    }
    if list {
        for split in ["dev", "held_out"] {
            let t = tally(rows, split, &rule);
            println!("  {split} false positives: {:?}", t.false_positives);
            println!("  {split} missed supersedes: {:?}", t.missed);
        }
    }
}

/// The committed pairs file and cosine file.
pub fn default_paths() -> (String, String) {
    let root = env!("CARGO_MANIFEST_DIR");
    (
        format!("{root}/fixtures/eval/pairs.json"),
        format!("{root}/evidence/issue-198-trigger/cosines-bge-m3-self.json"),
    )
}

/// The in-regime pairs of `pairs_path` at cosine >= [`THRESHOLD`] (cosines
/// from `cosines_path`), each judged with the shipped and the reference
/// lexicon, plus the per-(split, label) counts.
pub fn rows(pairs_path: &str, cosines_path: &str) -> (Vec<Row>, BTreeMap<(String, String), usize>) {
    let pairs = read(pairs_path);
    let cosines = read(cosines_path);
    let cosine: BTreeMap<String, f64> = cosines["pairs"]
        .as_array()
        .expect("pairs")
        .iter()
        .filter(|p| p["framing"] == "self")
        .map(|p| {
            (
                p["id"].as_str().unwrap().to_string(),
                p["cosine"].as_f64().unwrap(),
            )
        })
        .collect();
    // The reference script's lexicon: the shipped one without `reports`.
    let reference: Vec<&str> = PREDICATE_CUES
        .iter()
        .copied()
        .filter(|c| *c != "reports")
        .collect();

    let mut rows = Vec::new();
    let mut counts: BTreeMap<(String, String), usize> = BTreeMap::new();
    for p in pairs.as_array().expect("pairs.json is an array") {
        let types: Vec<&str> = p["concept_types"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let in_types = types.len() == 2
            && types[0] == types[1]
            && matches!(types[0], "Logic" | "Constraint" | "Observation");
        if !in_types || p.get("review").is_some() {
            continue;
        }
        let (a, b) = (p["a"].as_str().unwrap(), p["b"].as_str().unwrap());
        if same_identifiers(a, b) {
            continue; // the guard merges it: never a candidate
        }
        let id = p["id"].as_str().unwrap().to_string();
        let kind = p["relation"]
            .as_str()
            .or_else(|| p["family"].as_str())
            .unwrap_or_default()
            .to_string();
        if !matches!(
            kind.as_str(),
            "supersedes" | "distinct_entity" | "same-event"
        ) {
            continue;
        }
        let Some(&cos) = cosine.get(&id) else {
            continue;
        };
        if cos < THRESHOLD {
            continue;
        }
        let split = p["split"].as_str().unwrap().to_string();
        *counts.entry((split.clone(), kind.clone())).or_default() += 1;
        rows.push(Row {
            id,
            kind,
            split,
            verdict_shipped: judge_with(a, b, PREDICATE_CUES),
            verdict_reference: judge_with(a, b, &reference),
        });
    }
    (rows, counts)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (default_pairs, default_cosines) = default_paths();
    let pairs_path = args.first().cloned().unwrap_or(default_pairs);
    let cosines_path = args.get(1).cloned().unwrap_or(default_cosines);
    let (rows, counts) = rows(&pairs_path, &cosines_path);
    println!("in-regime pairs at cosine >= {THRESHOLD}: {counts:?}");
    report(
        "one span (reference lexicon)",
        &rows,
        |r| r.verdict_reference.spans == 1,
        false,
    );
    report(
        "value slot (reference lexicon)",
        &rows,
        |r| r.verdict_reference.value_slot,
        false,
    );
    report(
        "one span + value slot (reference lexicon, = slot.py)",
        &rows,
        |r| r.verdict_reference.suspected(),
        true,
    );
    report(
        "one span + value slot (shipped PREDICATE_CUES, + \"reports\")",
        &rows,
        |r| r.verdict_shipped.suspected(),
        true,
    );
}
