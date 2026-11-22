//! `formal/oracle/Fusion.json`: Lean `Fusion.RrfRecall.rrf` and `rrfAhead`
//! against `rank` (`src/recall/candidates.rs`) under `FusionKind::Rrf`.
//!
//! A member is the id pair `(2, id)`: a concept with its key, or, keyless,
//! an id in no graph (so `tie_break_by_key` sees it as the model's `tb`
//! does). Leg scores are `n / 1000`; the recent rank is the model's, written
//! into the input (`rank` reads it from `LegScores::recent_rank`). The model
//! is exact and the Rust `f64`: the order is compared exactly (the vectors
//! hold no tie the model says rounding decides) and each value within
//! [`TOLERANCE`] of the exact fraction.

use std::collections::HashMap;

use serde::Deserialize;

use super::{vectors, Vectors};
use crate::graph::Graph;
use crate::recall::candidates::formal_oracle::rank;
use crate::recall::candidates::{FusionKind, LegScores, RecallFusion};
use crate::test_util::props;
use crate::types::{CanonizationStatus, ConceptType, NodeId};

const TOLERANCE: f64 = 1e-12;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Member {
    id: u64,
    key: Option<String>,
    keyword: Option<u64>,
    vector: Option<u64>,
    recent_rank: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    members: Vec<Member>,
    keyword_ran: bool,
    /// `[kNum, kDen, wKeyword, wVector, wRecent]`.
    config: [u64; 5],
}

/// An exact fraction `[num, den]` from the model, or the Rust `f64`,
/// compared within [`TOLERANCE`].
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(from = "(u64, u64)")]
struct Value(f64);

impl From<(u64, u64)> for Value {
    fn from((num, den): (u64, u64)) -> Self {
        Value(num as f64 / den as f64)
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        (self.0 - other.0).abs() <= TOLERANCE
    }
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Output {
    order: Vec<u64>,
    rrf: Vec<Value>,
}

fn graph(members: &[Member]) -> Graph {
    props::graph_of(members.iter().filter_map(|m| {
        let key = m.key.as_deref()?;
        let mut c = props::concept(m.id, "x", key, CanonizationStatus::None);
        c.concept_type = ConceptType::Observation;
        Some(c)
    }))
}

/// The functions in `Fusion.json`.
const FNS: [&str; 2] = [
    "Lambo.Fusion.RrfRecall.rrf",
    "Lambo.Fusion.RrfRecall.rrf zero ties",
];

fn run(input: &Input) -> Output {
    let g = graph(&input.members);
    let [k_num, k_den, wk, wv, wr] = input.config;
    let fusion = RecallFusion {
        kind: FusionKind::Rrf,
        k: k_num as f64 / k_den as f64,
        keyword_weight: wk as f64 / 10.0,
        vector_weight: wv as f64 / 10.0,
        recent_weight: wr as f64 / 10.0,
    };
    let mut legs: HashMap<NodeId, LegScores> = input
        .members
        .iter()
        .map(|m| {
            let l = LegScores {
                keyword: m.keyword.map(|n| n as f64 / 1000.0),
                vector: m.vector.map(|n| n as f64 / 1000.0),
                recent: m.recent_rank.map(|_| 0.5),
                recent_rank: m.recent_rank,
                ..LegScores::default()
            };
            (props::cid(m.id), l)
        })
        .collect();
    let out = rank(&mut legs, &g, &fusion, input.keyword_ran);
    Output {
        order: out.iter().map(|s| s.item.0.as_u64_pair().1).collect(),
        rrf: out.iter().map(|s| Value(s.score)).collect(),
    }
}

#[test]
fn fusion() {
    let v = Vectors::load("Fusion", vectors!("Fusion"), &FNS);
    v.replay("Lambo.Fusion.RrfRecall.rrf", run);
}

/// Two candidates whose exact RRF values are both zero, one ranked only in
/// legs of weight zero, the other in no leg (in the Rust, a candidate whose
/// only leg score is non-finite earns no rank; the model has no non-finite
/// scores, so the vectors stand for it with a candidate in no leg). The
/// model calls both `0` and orders them by the tie-break, and so does `rank`
/// since #328: it fuses every zero to `+0.0`. Before that an empty `f64` sum
/// gave `-0.0`, which `total_cmp` put after every `+0.0` whatever the
/// tie-break said. Found by this oracle (#291).
#[test]
fn fusion_zero_ties() {
    let v = Vectors::load("Fusion", vectors!("Fusion"), &FNS);
    v.replay("Lambo.Fusion.RrfRecall.rrf zero ties", run);
}
