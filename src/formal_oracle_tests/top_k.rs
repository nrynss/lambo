//! `formal/oracle/TopK.json`: Lean `TopK.emit` and `TopK.emitR` /
//! `TopK.skipsR` against `emitted` (`src/recall/assemble.rs`), without and
//! with a #181 selection.
//!
//! A ranked entry is a member number (`0` the origin interaction, else the
//! id pair `(2, n)`), a content class and spelling (a concept with that
//! content; none: not a concept, an id in no graph), and its forced and
//! warned flags. The model's pair verdict `red` is "same class"; the test
//! first checks that `Redundancy::signal` agrees on every pair of concepts.
//! Rust keeps the skips in a map from the kept hit, so they are compared in
//! the model's scan order: by the skipped member's rank.

use serde::Deserialize;

use super::{precondition, vectors, Vectors};
use crate::graph::Graph;
use crate::recall::assemble::formal_oracle::emitted;
use crate::recall::redundancy::{normalized_content, Redundancy};
use crate::test_util::props;
use crate::types::{CanonizationStatus, Node, NodeId, Scored};

/// The model this module replays (`formal/oracle/TopK.json`).
pub(super) const MODEL: &str = "TopK";

/// `[member, class, content, forced, warns]`.
type Entry = (u64, Option<u64>, Option<String>, bool, bool);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    ranked: Vec<Entry>,
    top_k: usize,
    similar_signals: Option<bool>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct EmitOutput {
    hits: Vec<u64>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct EmitROutput {
    hits: Vec<u64>,
    skips: Vec<(u64, u64)>,
}

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

fn graph(ranked: &[Entry]) -> Graph {
    props::graph_of(ranked.iter().filter_map(|(n, _, content, _, _)| {
        let content = content.as_deref()?;
        assert_ne!(*n, 0, "member 0 is the origin interaction");
        Some(props::concept(
            *n,
            content,
            &format!("k{n}"),
            CanonizationStatus::None,
        ))
    }))
}

/// Run `emitted` on a case: the hit numbers and the skips in rank order of
/// the skipped member.
fn run(input: &Input) -> (Vec<u64>, Vec<(u64, u64)>) {
    let g = graph(&input.ranked);
    let ranked: Vec<Scored<NodeId>> = input
        .ranked
        .iter()
        .map(|e| Scored::new(id(e.0), 0.5))
        .collect();
    let forced = |x: NodeId| input.ranked.iter().any(|e| id(e.0) == x && e.3);
    let mut warns = |x: NodeId| input.ranked.iter().any(|e| id(e.0) == x && e.4);
    let redundancy = input.similar_signals.map(|similar| Redundancy {
        threshold: 0.85,
        similar,
    });
    let selection = redundancy
        .as_ref()
        .map(|r| (r, &mut warns as &mut dyn FnMut(NodeId) -> bool));
    let (hits, skips) = emitted(&g, &ranked, forced, input.top_k, selection);
    let rank = |x: NodeId| ranked.iter().position(|s| s.item == x).expect("ranked");
    let mut skips: Vec<(NodeId, NodeId)> = skips;
    skips.sort_by_key(|&(_, skipped)| rank(skipped));
    (
        hits.into_iter().map(number).collect(),
        skips
            .into_iter()
            .map(|(k, s)| (number(k), number(s)))
            .collect(),
    )
}

#[test]
fn top_k() {
    let v = Vectors::load(MODEL, vectors!("TopK"));

    for input in v.inputs::<Input>("Lambo.TopK.emitR") {
        let g = graph(&input.ranked);
        let r = Redundancy {
            threshold: 0.85,
            similar: input.similar_signals.expect("emitR has a selection"),
        };
        let concepts: Vec<(u64, &crate::types::Concept)> = input
            .ranked
            .iter()
            .filter(|e| e.1.is_some())
            .map(|e| match g.node(id(e.0)) {
                Some(Node::Concept(c)) => (e.1.unwrap(), c),
                _ => unreachable!("a member with a class is a concept"),
            })
            .collect();
        for &(ca, a) in &concepts {
            for &(cb, b) in &concepts {
                if a.id == b.id {
                    continue;
                }
                let (na, nb) = (
                    normalized_content(&a.content),
                    normalized_content(&b.content),
                );
                let signal = r.signal(&g, (a, &na), (b, &nb)).is_some();
                precondition("TopK", signal == (ca == cb), || {
                    format!(
                        "Redundancy::signal on {:?} and {:?} is {signal}, the classes say {}",
                        a.content,
                        b.content,
                        ca == cb
                    )
                });
            }
        }
    }

    v.replay("Lambo.TopK.emit", |input: &Input| {
        assert!(input.similar_signals.is_none());
        EmitOutput { hits: run(input).0 }
    });
    v.replay("Lambo.TopK.emitR", |input: &Input| {
        let (hits, skips) = run(input);
        EmitROutput { hits, skips }
    });
}
