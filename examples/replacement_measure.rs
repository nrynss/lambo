//! #198 part c: measure the replacement-candidate list on the #211 pairs.
//!
//! For each labelled pair of `fixtures/eval/pairs.json` whose two texts are
//! the same `Logic`, `Constraint` or `Observation` type, not awaiting
//! review, and labelled `supersedes` or `distinct_entity`, it writes the
//! pair's first text into a graph, then judges the second as the write path
//! does: one vector hit, the first text at the pair's BGE-M3 cosine (from
//! `evidence/issue-198-trigger/cosines-bge-m3-self.json`). The hit is the
//! concept the write merged with when the identifier guard passes and the
//! cosine reaches the merge threshold (a single hit is the whole top tier),
//! and is then listed marked merged, as the write path lists it. Everything
//! else is `lambo::graph::suspected::replacement::select`, the function the
//! write path calls.
//!
//! A listed `supersedes` pair is a found predecessor; a listed
//! `distinct_entity` pair is noise. A pair has one predecessor, so the
//! list length here is at most 1: the length distribution and the rate on
//! writes that replaced nothing come from real memory (the README).
//!
//! ```text
//! cargo run --example replacement_measure
//! cargo run --example replacement_measure -- <pairs.json> <cosines.json>
//! ```
//!
//! `tests/replacement_measure.rs` includes this file and pins the counts
//! at the shipped floor, so the measurement is in CI.

use std::collections::BTreeMap;

use chrono::{TimeZone, Utc};
use lambo::graph::canonical::same_identifiers;
use lambo::graph::hybrid::SEMANTIC_MATCH_THRESHOLD_DEFAULT;
use lambo::graph::suspected::replacement::select;
use lambo::graph::Graph;
use lambo::types::{
    AgentId, CanonizationStatus, Concept, ConceptType, Interaction, NodeId, Scored, SessionId,
};
use serde_json::Value;

/// The floors the sweep prints. The shipped floor is the merge threshold,
/// BGE-M3's [`SEMANTIC_MATCH_THRESHOLD_DEFAULT`] (0.85).
pub const FLOORS: [f64; 7] = [0.75, 0.78, 0.80, 0.82, 0.85, 0.86, 0.88];

/// One in-regime pair.
pub struct Row {
    pub id: String,
    pub relation: String,
    pub split: String,
    pub ty: ConceptType,
    pub a: String,
    pub b: String,
    pub cosine: f64,
}

/// One split and relation's count at one floor.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Count {
    pub listed: usize,
    pub of: usize,
}

fn read(path: &str) -> Value {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

/// The committed pairs file and cosine file.
pub fn default_paths() -> (String, String) {
    let root = env!("CARGO_MANIFEST_DIR");
    (
        format!("{root}/fixtures/eval/pairs.json"),
        format!("{root}/evidence/issue-198-trigger/cosines-bge-m3-self.json"),
    )
}

fn concept_type(name: &str) -> Option<ConceptType> {
    match name {
        "Logic" => Some(ConceptType::Logic),
        "Constraint" => Some(ConceptType::Constraint),
        "Observation" => Some(ConceptType::Observation),
        _ => None,
    }
}

/// The in-regime pairs with a self-framed cosine.
pub fn rows(pairs_path: &str, cosines_path: &str) -> Vec<Row> {
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
    let mut out = Vec::new();
    for p in pairs.as_array().expect("pairs.json is an array") {
        let relation = p["relation"].as_str().unwrap_or_default();
        if !matches!(relation, "supersedes" | "distinct_entity") || p.get("review").is_some() {
            continue;
        }
        let types: Vec<&str> = p["concept_types"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let Some(ty) = (types.len() == 2 && types[0] == types[1])
            .then(|| concept_type(types[0]))
            .flatten()
        else {
            continue;
        };
        let id = p["id"].as_str().unwrap().to_string();
        let Some(&cosine) = cosine.get(&id) else {
            continue;
        };
        out.push(Row {
            id,
            relation: relation.to_string(),
            split: p["split"].as_str().unwrap().to_string(),
            ty,
            a: p["a"].as_str().unwrap().to_string(),
            b: p["b"].as_str().unwrap().to_string(),
            cosine,
        });
    }
    out
}

fn concept(n: u64, session: &SessionId, origin: NodeId, content: &str, ty: ConceptType) -> Concept {
    Concept {
        id: NodeId(uuid::Uuid::from_u64_pair(0x198c, n)),
        session_id: session.clone(),
        content: content.into(),
        canonical_key: content.to_lowercase(),
        concept_type: ty,
        origin_interaction: origin,
        origin_agent: AgentId::from("measure"),
        created_at: Utc.with_ymd_and_hms(2026, 10, 28, 0, 0, 0).unwrap(),
        access_count: 0,
        last_accessed: None,
        gc_survived: 0,
        human_confirmed: 0,
        canonization_status: CanonizationStatus::None,
        blast_radius: None,
        last_demotion_time: None,
        embedding: None,
        chunk_group_id: None,
        embedding_source: None,
    }
}

/// Whether the write of `row.b` lists `row.a` at `floor`.
pub fn listed(row: &Row, floor: f64) -> bool {
    let session = SessionId::from("measure");
    let mut g = Graph::new(session.clone());
    let i = Interaction {
        id: NodeId(uuid::Uuid::from_u64_pair(0x198d, 1)),
        session_id: session.clone(),
        agent_id: AgentId::from("measure"),
        prompt_text: None,
        previous_id: None,
        created_at: Utc.with_ymd_and_hms(2026, 10, 28, 0, 0, 0).unwrap(),
        event_time: None,
    };
    g.insert_interaction(i.clone()).unwrap();
    let old = concept(1, &session, i.id, &row.a, row.ty);
    let new = concept(2, &session, i.id, &row.b, row.ty);
    let (old_id, new_id) = (old.id, new.id);
    g.insert_concept(old, i.id).unwrap();
    g.insert_concept(new, i.id).unwrap();
    let hits = [Scored::new(old_id, row.cosine)];
    select(&g, new_id, &hits, floor, &[], &merged_with(row))
        .iter()
        .any(|c| c.id == old_id)
}

/// The merge tier the write of `row.b` merged with: the first text, when
/// the identifier guard passes and the cosine reaches the merge threshold
/// (a single hit is the whole top tier). Listed, marked merged.
fn merged_with(row: &Row) -> Vec<NodeId> {
    let merged = same_identifiers(&row.a, &row.b) && row.cosine >= SEMANTIC_MATCH_THRESHOLD_DEFAULT;
    if merged {
        vec![NodeId(uuid::Uuid::from_u64_pair(0x198c, 1))]
    } else {
        Vec::new()
    }
}

/// Whether the write of `row.b` merged with `row.a` (so a listing of it is
/// marked merged).
pub fn merged(row: &Row) -> bool {
    !merged_with(row).is_empty()
}

/// Per (split, relation), how many pairs are listed at `floor`.
pub fn tally(rows: &[Row], floor: f64) -> BTreeMap<(String, String), Count> {
    let mut out: BTreeMap<(String, String), Count> = BTreeMap::new();
    for r in rows {
        let c = out
            .entry((r.split.clone(), r.relation.clone()))
            .or_default();
        c.of += 1;
        c.listed += usize::from(listed(r, floor));
    }
    out
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (default_pairs, default_cosines) = default_paths();
    let pairs_path = args.first().cloned().unwrap_or(default_pairs);
    let cosines_path = args.get(1).cloned().unwrap_or(default_cosines);
    let rows = rows(&pairs_path, &cosines_path);
    println!(
        "in-regime pairs (same eligible type, not under review): {}",
        rows.len()
    );
    println!("floor | dev supersedes | dev distinct | held-out supersedes | held-out distinct");
    for floor in FLOORS {
        let t = tally(&rows, floor);
        let cell = |split: &str, rel: &str| {
            let c = t
                .get(&(split.to_string(), rel.to_string()))
                .copied()
                .unwrap_or_default();
            format!("{}/{}", c.listed, c.of)
        };
        let mark = if floor == SEMANTIC_MATCH_THRESHOLD_DEFAULT {
            " (shipped)"
        } else {
            ""
        };
        println!(
            "{floor:.2}{mark} | {} | {} | {} | {}",
            cell("dev", "supersedes"),
            cell("dev", "distinct_entity"),
            cell("held_out", "supersedes"),
            cell("held_out", "distinct_entity"),
        );
    }
    let missed: Vec<&str> = rows
        .iter()
        .filter(|r| r.relation == "supersedes" && !listed(r, SEMANTIC_MATCH_THRESHOLD_DEFAULT))
        .map(|r| r.id.as_str())
        .collect();
    println!("supersedes not listed at the shipped floor: {missed:?}");
    let t = SEMANTIC_MATCH_THRESHOLD_DEFAULT;
    let merged_count = |split: &str, rel: &str| {
        let of_split: Vec<&Row> = rows
            .iter()
            .filter(|r| r.split == split && r.relation == rel && listed(r, t))
            .collect();
        format!(
            "{}/{}",
            of_split.iter().filter(|r| merged(r)).count(),
            of_split.len()
        )
    };
    println!(
        "listed at the shipped floor, of which merged: dev supersedes {}, dev distinct {}, \
         held-out supersedes {}, held-out distinct {}",
        merged_count("dev", "supersedes"),
        merged_count("dev", "distinct_entity"),
        merged_count("held_out", "supersedes"),
        merged_count("held_out", "distinct_entity"),
    );
}
