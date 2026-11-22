//! Property tests: [`closing_edge`] against `formal/LamboProofs/Cite.lean`.

use std::collections::{HashMap, HashSet};

use proptest::collection::vec;
use proptest::prelude::*;

use super::*;
use crate::test_util::props::{self, cid};
use crate::types::Edge;

/// `n` concepts, some `Hierarchical` edges (which the check must ignore,
/// cycles included), and a sequence of planned batches of `Causal` /
/// `Dependency` edges (`true` = `Causal`).
type Case = (u64, Vec<(u64, u64)>, Vec<Vec<(u64, u64, bool)>>);

/// A planned edge `(source, target, causal)`: nine times in ten between two
/// different concepts, so most refusals come from real cycle detection
/// rather than the self-loop rule.
fn planned_edge(n: u64) -> impl Strategy<Value = (u64, u64, bool)> {
    (0..n, prop_oneof![9 => 1..n, 1 => Just(0)], any::<bool>())
        .prop_map(move |(s, d, causal)| (s, (s + d) % n, causal))
}

fn case() -> impl Strategy<Value = Case> {
    (2u64..=8).prop_flat_map(|n| {
        (
            Just(n),
            vec((0..n, 0..n), 0..6),
            vec(vec(planned_edge(n), 1..5), 1..8),
        )
    })
}

fn edge(k: &mut u64, src: NodeId, tgt: NodeId, ty: EdgeType) -> Edge {
    *k += 1;
    Edge {
        id: NodeId(uuid::Uuid::from_u64_pair(3, *k)),
        session_id: props::sid(),
        source: src,
        target: tgt,
        edge_type: ty,
        weight: 1.0,
        reinforcements: 1,
        created_at: props::t0(),
        last_reinforced: props::t0(),
        event_time: None,
    }
}

/// Does `to` reach `from` along `edges` (zero or more steps)? Lean `Reach`.
fn reaches(edges: &[(NodeId, NodeId)], from: NodeId, to: NodeId) -> bool {
    let mut out: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    for &(s, t) in edges {
        out.entry(s).or_default().push(t);
    }
    let mut seen = HashSet::from([from]);
    let mut stack = vec![from];
    while let Some(x) = stack.pop() {
        if x == to {
            return true;
        }
        for &y in out.get(&x).into_iter().flatten() {
            if seen.insert(y) {
                stack.push(y);
            }
        }
    }
    false
}

/// Lean `Acyclic`: no edge `x -> y` has `y` reaching `x`.
fn acyclic(edges: &[(NodeId, NodeId)]) -> bool {
    edges.iter().all(|&(x, y)| !reaches(edges, y, x))
}

proptest! {
    #![proptest_config(props::config())]

    /// `Cite.batch_acyclic` and `Cite.refuses_self_loop`
    /// (`formal/LamboProofs/Cite.lean`), by induction over random writes:
    /// starting from an empty `Causal`/`Dependency` graph, every batch
    /// `closing_edge` accepts is committed, and the graph after it is still
    /// acyclic. A batch it refuses is left out; the refused edge must be one
    /// of the batch, and must really close a cycle in the combined graph
    /// (`t` reaches `s` over the existing `Causal`/`Dependency` edges plus
    /// the batch), so the check neither misses a cycle nor follows the
    /// `Hierarchical` edges planted beside them.
    #[test]
    fn accepted_batches_keep_the_dependency_graph_acyclic((n, hier, batches) in case()) {
        let mut g = props::plain_graph(n);
        let mut k = 0u64;
        for (s, t) in hier {
            let _ = g.upsert_edge(edge(&mut k, cid(s), cid(t), EdgeType::Hierarchical));
        }
        let mut committed: Vec<(NodeId, NodeId)> = Vec::new();
        for batch in batches {
            let planned: Vec<(NodeId, NodeId, EdgeType)> = batch
                .iter()
                .map(|&(s, t, causal)| {
                    let ty = if causal { EdgeType::Causal } else { EdgeType::Dependency };
                    (cid(s), cid(t), ty)
                })
                .collect();
            let mut combined = committed.clone();
            combined.extend(planned.iter().map(|&(s, t, _)| (s, t)));
            let verdict = closing_edge(&g, &planned);
            // refuses_self_loop: a batch with a self-loop is never accepted.
            if planned.iter().any(|&(a, b, _)| a == b) {
                prop_assert!(verdict.is_some(), "a self-loop passed: {:?}", planned);
            }
            match verdict {
                None => {
                    for &(s, t, ty) in &planned {
                        g.upsert_edge(edge(&mut k, s, t, ty)).expect("accepted edge writes");
                    }
                    committed = combined;
                    prop_assert!(acyclic(&committed), "accepted batch closed a cycle: {:?}", planned);
                }
                Some((s, t, ty)) => {
                    prop_assert!(planned.contains(&(s, t, ty)), "refused an edge not in the batch");
                    prop_assert!(reaches(&combined, t, s), "refused {} -> {} without a cycle", s, t);
                }
            }
        }
    }
}
