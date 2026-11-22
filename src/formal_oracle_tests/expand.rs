//! `formal/oracle/Expand.json`: Lean `Expand.expand` against `expand`
//! (`src/recall/expand.rs`).
//!
//! Each case is a graph of concepts `1..n` (id pair `(2, k)`) with typed
//! edges, the seeds (`0` the origin interaction, `99` an id in no graph), and
//! the depth. The model's neighbour function `nbrs` is given per node; the
//! test first checks that the graph's own adjacency (`TRAVERSAL_ORDER`, then
//! `out_neighbors_typed`) gives exactly those lists, and reports a
//! difference as a model mismatch (the Rust's traversal order changed).
//! Only `required` is compared: scores and chunk-group siblings are not
//! modelled.

use std::collections::HashMap;

use serde::Deserialize;

use super::{precondition, vectors, Vectors};
use crate::graph::Graph;
use crate::recall::expand::{expand, TRAVERSAL_ORDER};
use crate::test_util::props;
use crate::types::{CanonizationStatus, Edge, EdgeType, NodeId, Scored};

/// The model this module replays (`formal/oracle/Expand.json`).
pub(super) const MODEL: &str = "Expand";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    concepts: u64,
    edges: Vec<(u64, EdgeType, u64)>,
    nbrs: Vec<(u64, Vec<u64>)>,
    seeds: Vec<u64>,
    depth: usize,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Output {
    required: Vec<u64>,
}

/// A member number's id: `0` the origin interaction, else pair `(2, n)`
/// (`99` is in no graph).
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

fn graph(input: &Input) -> Graph {
    let mut g = props::graph_of((1..=input.concepts).map(|i| {
        let text = format!("c{i}");
        props::concept(i, &text, &text, CanonizationStatus::None)
    }));
    for (k, &(s, ty, t)) in input.edges.iter().enumerate() {
        let edge = Edge {
            id: NodeId(uuid::Uuid::from_u64_pair(3, k as u64)),
            session_id: props::sid(),
            source: id(s),
            target: id(t),
            edge_type: ty,
            weight: 1.0,
            reinforcements: 1,
            created_at: props::t0(),
            last_reinforced: props::t0(),
            event_time: None,
        };
        let ok = g.upsert_edge(edge).is_ok();
        precondition("Expand", ok, || {
            format!("the graph refuses the edge {s} -{ty:?}-> {t}")
        });
    }
    g
}

/// Replay this model's vectors; run by `super::oracle`, not a test of its
/// own, so it cannot be ignored or gated alone.
pub(super) fn replay() {
    let v = Vectors::load(MODEL, vectors!("Expand"));

    for input in v.inputs::<Input>("Lambo.Expand.expand") {
        let g = graph(&input);
        let nbrs: HashMap<u64, Vec<u64>> = input.nbrs.iter().cloned().collect();
        for x in (0..=input.concepts).chain([99]) {
            let real: Vec<u64> = TRAVERSAL_ORDER
                .iter()
                .flat_map(|&ty| g.out_neighbors_typed(id(x), ty))
                .map(number)
                .collect();
            let model = nbrs.get(&x).cloned().unwrap_or_default();
            // Not a `precondition`: the vector's lists are the model's
            // neighbour function as `Expand.lean`'s header describes the
            // Rust (per `TRAVERSAL_ORDER` type, id-ascending). If the graph
            // disagrees, the Rust's traversal order changed under the model.
            assert_eq!(
                real, model,
                "Expand: the Rust's neighbour order for node {x} no longer matches the model's \
                 nbrs (TRAVERSAL_ORDER, then out_neighbors_typed id-ascending, as \
                 formal/LamboProofs/Expand.lean's header and formal/OracleExport/Expand.lean \
                 state it): a model mismatch; re-read Expand.lean and regenerate"
            );
        }
    }

    v.replay("Lambo.Expand.expand", |input: &Input| {
        let g = graph(input);
        let seeds = input
            .seeds
            .iter()
            .map(|&n| Scored::new(id(n), 0.5))
            .collect();
        let set = expand(&g, seeds, input.depth);
        Output {
            required: set.required.iter().map(|s| number(s.item)).collect(),
        }
    });
}
