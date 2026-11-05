//! #175 controlled propagation experiment. Test-only: no runtime policy.
use std::collections::{BTreeMap, HashMap, HashSet};

use super::expand::{ExpandedSet, TRAVERSAL_ORDER};
use crate::graph::Graph;
use crate::types::{Node, NodeId, Scored};

/// Two bounded propagation hops; exactly the current directed traversal types.
/// Seeds are nonnegative and sum to one. All accumulation is in UUID order.
/// Each arm is max-normalized over graph-only BFS members before applying gain;
/// direct seeds keep their production relevance in assembly.
pub(super) fn propagate(
    graph: &Graph,
    seeds: &[Scored<NodeId>],
    expanded: &ExpandedSet,
    damping: f64,
    gain: f64,
    divide_sender_degree: bool,
) -> HashMap<NodeId, f64> {
    assert!(damping.is_finite() && (0.0..=1.0).contains(&damping));
    assert!(gain.is_finite() && gain >= 0.0);
    let direct: HashSet<_> = seeds.iter().map(|s| s.item).collect();
    let allowed: HashSet<_> = expanded.required.iter().map(|s| s.item).collect();
    let mut frontier = BTreeMap::new();
    for s in seeds {
        if s.score.is_finite()
            && s.score > 0.0
            && matches!(graph.node(s.item), Some(Node::Concept(_)))
        {
            frontier.insert(s.item.0, s.score);
        }
    }
    let sum: f64 = frontier.values().sum();
    if sum == 0.0 {
        return HashMap::new();
    }
    for v in frontier.values_mut() {
        *v /= sum;
    }
    let mut total: BTreeMap<_, f64> = BTreeMap::new();
    for _ in 0..2 {
        let mut next = BTreeMap::new();
        for (source, mass) in frontier {
            let mut neighbors = Vec::new();
            for ty in TRAVERSAL_ORDER {
                neighbors.extend(
                    graph
                        .out_neighbors_typed(NodeId(source), ty)
                        .into_iter()
                        .filter(|id| matches!(graph.node(*id), Some(Node::Concept(_))))
                        .map(|id| id.0),
                );
            }
            neighbors.sort_unstable();
            neighbors.dedup();
            if neighbors.is_empty() {
                continue;
            }
            let divisor = if divide_sender_degree {
                neighbors.len() as f64
            } else {
                1.0
            };
            for target in neighbors {
                *next.entry(target).or_insert(0.0) += damping * mass / divisor;
            }
        }
        for (id, mass) in &next {
            let node = NodeId(*id);
            if allowed.contains(&node) && !direct.contains(&node) {
                *total.entry(*id).or_default() += mass;
            }
        }
        frontier = next;
    }
    let max = total.values().copied().fold(0.0, f64::max);
    if max == 0.0 {
        return HashMap::new();
    }
    total
        .into_iter()
        .map(|(id, v)| (NodeId(id), gain * v / max))
        .collect()
}

#[cfg(feature = "store-sqlite")]
mod run;
