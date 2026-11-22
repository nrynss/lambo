//! `formal/oracle/Order.json`: Lean `Order.rankMembers` against
//! `rank_cmp` / `rank_members` (`src/recall/assemble.rs`) and Lean
//! `Order.tieBreak` against `tie_break_by_key` (`src/types/mod.rs`).
//!
//! The model's abstract orders, as the exporter instantiates them: scores by
//! `f64::total_cmp` (the vectors carry bit patterns, NaN and `-0.0`
//! included), keys by `str::cmp`, ids by member number. A member `n` is the
//! origin interaction when `n = 0`, otherwise the id pair `(2, n)`: a concept
//! when it has a key, an id in no graph when it has none. Keys never carry
//! the scoped prefix, so `tie_form` is the identity on them (checked).

use serde::Deserialize;

use super::{cmp_table, precondition, vectors, Vectors};
use crate::graph::resource::tie_form;
use crate::graph::Graph;
use crate::recall::assemble::formal_oracle::{rank_cmp, rank_members};
use crate::test_util::props;
use crate::types::{tie_break_by_key, CanonizationStatus, ConceptType, NodeId, Scored};

/// The model this module replays (`formal/oracle/Order.json`).
pub(super) const MODEL: &str = "Order";

/// `[number, key, status, score bits]`.
type Member = (u64, Option<String>, Option<String>, u64);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RankInput {
    members: Vec<Member>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct RankOutput {
    cmp: Vec<String>,
    sorted: Vec<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TieInput {
    members: Vec<(u64, Option<String>)>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct TieOutput {
    cmp: Vec<String>,
}

/// Member `n`'s id: the origin interaction for `0`, else pair `(2, n)`.
fn id(n: u64) -> NodeId {
    if n == 0 {
        props::origin()
    } else {
        props::cid(n)
    }
}

fn number(id: NodeId) -> u64 {
    if id == props::origin() {
        0
    } else {
        id.0.as_u64_pair().1
    }
}

fn status(s: &str) -> CanonizationStatus {
    match s {
        "none" => CanonizationStatus::None,
        "candidate" => CanonizationStatus::Candidate,
        "venerable" => CanonizationStatus::Venerable,
        "canonical" => CanonizationStatus::Canonical,
        other => panic!("Order.json: unknown status {other:?}"),
    }
}

/// The graph of a case: the origin interaction, and a concept for every
/// keyed member (an Observation, so colliding keys insert).
fn graph(members: &[Member]) -> Graph {
    props::graph_of(members.iter().filter_map(|(n, key, st, _)| {
        let key = key.as_deref()?;
        assert_ne!(*n, 0, "member 0 is the origin interaction and has no key");
        let st = status(st.as_deref().expect("a keyed member has a status"));
        let mut c = props::concept(*n, "x", key, st);
        c.concept_type = ConceptType::Observation;
        Some(c)
    }))
}

#[test]
fn order() {
    let v = Vectors::load(MODEL, vectors!("Order"));

    let rank_keys = v
        .inputs::<RankInput>("Lambo.Order.rankMembers")
        .into_iter()
        .flat_map(|input| input.members.into_iter().map(|(_, key, _, _)| key));
    let tie_keys = v
        .inputs::<TieInput>("Lambo.Order.tieBreak")
        .into_iter()
        .flat_map(|input| input.members.into_iter().map(|(_, key)| key));
    for k in rank_keys.chain(tie_keys).flatten() {
        precondition("Order", tie_form(&k) == k.as_str(), || {
            format!("tie_form({k:?}) is not {k:?}")
        });
    }

    v.replay("Lambo.Order.rankMembers", |input: &RankInput| {
        let g = graph(&input.members);
        let members: Vec<Scored<NodeId>> = input
            .members
            .iter()
            .map(|(n, _, _, bits)| Scored::new(id(*n), f64::from_bits(*bits)))
            .collect();
        let cmp = cmp_table(&members, |a, b| rank_cmp(&g, a, b));
        let mut sorted = members.clone();
        rank_members(&g, &mut sorted);
        RankOutput {
            cmp,
            sorted: sorted.iter().map(|s| number(s.item)).collect(),
        }
    });

    v.replay("Lambo.Order.tieBreak", |input: &TieInput| {
        let cmp = cmp_table(&input.members, |(a, ka), (b, kb)| {
            tie_break_by_key(ka.as_deref(), &id(*a), kb.as_deref(), &id(*b))
        });
        TieOutput { cmp }
    });
}
