//! `Hierarchical` cycles from `parent_of` (spec §5.7): refused, in both
//! derive strategies.
//!
//! `Hierarchical` is a DAG constraint by definition (`assert_invariants`
//! detects its cycles as a safety net), but before this only the one-node
//! cycle, a reflexive pair, was refused at write time. `parent_of: [(a, b)]`
//! followed by a later `parent_of: [(b, a)]` (or both pairs in one call, or a
//! rival writing `b -> a` between a hybrid plan and its commit) wrote a
//! 2-cycle, and longer ones the same way.
//!
//! Two checks, the same shape as `record_action`'s Causal/Dependency cycle
//! refusal:
//!
//! * [`reject_parent_of_cycles`], in each strategy's read-only pre-pass
//!   (sync `validate`, hybrid `validate_graph_inputs`), so the common case is
//!   refused at call time, including on J3's asynchronous ack. It sees the
//!   graph's existing `Hierarchical` edges plus every pair of the call, with
//!   an end the call will create identified by its canonical key.
//! * [`reject_closing_edge`], at the edge write, under the write lock,
//!   against the live graph with the call's own earlier edges in it. It is
//!   the authority: it catches what changed after the pre-pass (a concurrent
//!   writer). Both strategies write inside one [`Graph::transaction`], so a
//!   refusal there leaves nothing written.
//!
//! An edge that already exists (a re-declared pair) is a reinforcement, not a
//! new edge, so it never counts as closing a cycle.

use std::collections::{HashMap, HashSet};

use crate::graph::canonical::{canonicalize, CanonicalizeResult};
use crate::graph::Graph;
use crate::types::{EdgeType, LamboError, NodeId, StoreError};

/// One end of a planned edge: an existing concept, or one the call creates,
/// named by its canonical key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum End {
    Node(NodeId),
    New(String),
}

fn cycle_error(parent: &str, child: &str) -> LamboError {
    LamboError::Store(StoreError::Invariant(format!(
        "parent_of pair ({parent}, {child}) would close a Hierarchical cycle: {child:?} is \
         already an ancestor of {parent:?}. Hierarchical edges form a DAG (spec §5.7); \
         nothing was written"
    )))
}

/// Refuse a call whose `parent_of` pairs, together with the graph's
/// existing `Hierarchical` edges, would contain a cycle. Read-only.
pub(crate) fn reject_parent_of_cycles(
    graph: &Graph,
    pairs: &[(&str, &str)],
) -> Result<(), LamboError> {
    if pairs.is_empty() {
        return Ok(());
    }
    let end = |content: &str| -> Result<End, LamboError> {
        Ok(match canonicalize(content, graph)? {
            CanonicalizeResult::Matched { node, .. } => End::Node(node),
            CanonicalizeResult::Unmatched { key } => End::New(key),
        })
    };
    let mut planned: Vec<(End, End, usize)> = Vec::with_capacity(pairs.len());
    for (i, &(parent, child)) in pairs.iter().enumerate() {
        planned.push((end(parent)?, end(child)?, i));
    }
    let mut out: HashMap<&End, Vec<&End>> = HashMap::new();
    for (p, c, _) in &planned {
        out.entry(p).or_default().push(c);
    }
    for (p, c, i) in &planned {
        if let (End::Node(pn), End::Node(cn)) = (p, c)
            && graph
                .edge_between(*pn, *cn, EdgeType::Hierarchical)
                .is_some()
        {
            continue;
        }
        if p == c {
            // A reflexive pair; the pre-pass refuses it with its own message.
            continue;
        }
        if reaches(graph, &out, c, p) {
            let (parent, child) = pairs[*i];
            return Err(cycle_error(parent, child));
        }
    }
    Ok(())
}

/// Whether `to` is reachable from `from` over existing `Hierarchical` edges
/// and the planned ones.
fn reaches(graph: &Graph, planned: &HashMap<&End, Vec<&End>>, from: &End, to: &End) -> bool {
    let mut seen: HashSet<End> = HashSet::new();
    let mut stack: Vec<End> = vec![from.clone()];
    while let Some(at) = stack.pop() {
        if &at == to {
            return true;
        }
        if !seen.insert(at.clone()) {
            continue;
        }
        if let End::Node(id) = &at {
            stack.extend(
                graph
                    .out_neighbors_typed(*id, EdgeType::Hierarchical)
                    .into_iter()
                    .map(End::Node),
            );
        }
        if let Some(next) = planned.get(&at) {
            stack.extend(next.iter().map(|e| (*e).clone()));
        }
    }
    false
}

/// Refuse writing `parent_node -> child_node` when `child_node` already
/// reaches `parent_node` over `Hierarchical` edges in the live graph. A no-op
/// when the edge exists (it reinforces).
pub(crate) fn reject_closing_edge(
    graph: &Graph,
    parent: &str,
    child: &str,
    parent_node: NodeId,
    child_node: NodeId,
) -> Result<(), LamboError> {
    if graph
        .edge_between(parent_node, child_node, EdgeType::Hierarchical)
        .is_some()
    {
        return Ok(());
    }
    if graph.hierarchical_path(child_node, parent_node) {
        return Err(cycle_error(parent, child));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::{derive, derive_after_validation, ParentOf};
    use super::*;
    use crate::types::{AgentId, Interaction, SessionId};
    use chrono::{TimeZone, Utc};

    fn graph() -> (Graph, NodeId) {
        let mut g = Graph::new(SessionId::from("cycles"));
        let i = Interaction {
            event_time: None,
            id: NodeId::new(),
            session_id: SessionId::from("cycles"),
            agent_id: AgentId::from("agent-a"),
            prompt_text: Some("hierarchy".into()),
            previous_id: None,
            created_at: Utc.timestamp_opt(1_752_000_000, 0).unwrap(),
        };
        let iid = i.id;
        g.insert_interaction(i).unwrap();
        (g, iid)
    }

    fn pairs(g: &mut Graph, i: NodeId, pairs: &[(&str, &str)]) -> Result<(), LamboError> {
        derive(
            g,
            i,
            &AgentId::from("agent-a"),
            &[],
            &ParentOf::from_pairs(pairs),
            10,
        )
        .map(|_| ())
    }

    fn refused_untouched(g: &mut Graph, i: NodeId, call: &[(&str, &str)]) {
        let (epoch, nodes, edges) = (g.epoch(), g.node_count(), g.edge_count());
        let err = pairs(g, i, call).unwrap_err().to_string();
        assert!(err.contains("Hierarchical cycle"), "{err}");
        assert_eq!(
            (g.epoch(), g.node_count(), g.edge_count()),
            (epoch, nodes, edges)
        );
        g.assert_invariants().unwrap();
    }

    #[test]
    fn a_two_cycle_across_calls_is_refused() {
        let (mut g, i) = graph();
        pairs(&mut g, i, &[("cyc alpha", "cyc beta")]).unwrap();
        refused_untouched(&mut g, i, &[("cyc beta", "cyc alpha")]);
        // Re-declaring the existing pair reinforces; it closes nothing.
        pairs(&mut g, i, &[("cyc alpha", "cyc beta")]).unwrap();
    }

    #[test]
    fn a_two_cycle_in_one_call_is_refused() {
        let (mut g, i) = graph();
        refused_untouched(
            &mut g,
            i,
            &[("cyc alpha", "cyc beta"), ("cyc beta", "cyc alpha")],
        );
    }

    #[test]
    fn a_two_step_cycle_is_refused() {
        let (mut g, i) = graph();
        pairs(
            &mut g,
            i,
            &[("cyc alpha", "cyc beta"), ("cyc beta", "cyc gamma")],
        )
        .unwrap();
        refused_untouched(&mut g, i, &[("cyc gamma", "cyc alpha")]);
        // A new branch off the chain is not a cycle.
        pairs(&mut g, i, &[("cyc gamma", "cyc delta")]).unwrap();
        g.assert_invariants().unwrap();
    }

    /// The write-time check is the authority: a cycle the pre-pass did not
    /// see (here it is skipped, as a concurrent writer would slip past it)
    /// is refused at the edge write, and the transaction `derive` runs the
    /// write loop in undoes the concepts written before it.
    #[test]
    fn the_write_time_check_refuses_and_the_transaction_undoes_the_call() {
        let (mut g, i) = graph();
        pairs(&mut g, i, &[("cyc alpha", "cyc beta")]).unwrap();
        let (epoch, nodes, edges) = (g.epoch(), g.node_count(), g.edge_count());
        let session = g.session_id().clone();
        let call = [("cyc fresh", "cyc alpha"), ("cyc beta", "cyc alpha")];
        let err = g
            .transaction(|g| {
                derive_after_validation(
                    g,
                    i,
                    &AgentId::from("agent-a"),
                    &[],
                    &ParentOf::from_pairs(&call),
                    10,
                    Utc.timestamp_opt(1_752_000_000, 0).unwrap(),
                    None,
                    session,
                )
            })
            .unwrap_err()
            .to_string();
        assert!(err.contains("Hierarchical cycle"), "{err}");
        assert_eq!(
            (g.epoch(), g.node_count(), g.edge_count()),
            (epoch, nodes, edges)
        );
    }
}
