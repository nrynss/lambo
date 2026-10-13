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

use crate::graph::canonical::CanonicalizeResult;
use crate::graph::Graph;
use crate::types::{EdgeType, LamboError, NodeId, StoreError};

/// One end of a planned edge: an existing concept, or one the call creates,
/// named by its canonical key (borrowed: the walk allocates no end).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum End<'k> {
    Node(NodeId),
    New(&'k str),
}

fn cycle_error(parent: &str, child: &str) -> LamboError {
    LamboError::Store(StoreError::cycle_refusal(
        crate::types::CycleKind::Hierarchical,
        format!(
            "parent_of pair ({parent}, {child}) would close a Hierarchical cycle: {child:?} is \
             already an ancestor of {parent:?}. Hierarchical edges form a DAG (spec §5.7); \
             nothing was written"
        ),
    ))
}

/// The planned end for a resolved `(match, key)`.
fn end((node, key): &(Option<NodeId>, String)) -> End<'_> {
    match node {
        Some(id) => End::Node(*id),
        None => End::New(key.as_str()),
    }
}

/// Refuse a call whose `parent_of` pairs, together with the graph's
/// existing `Hierarchical` edges, would contain a cycle. Read-only.
///
/// Cost: one walk per pair, up from its parent through the parent's
/// ancestors (existing edges and the call's pairs), each node visited once
/// per walk, reading the adjacency index directly. A pair whose parent sits
/// under a long chain is the worst case: on a 100,000-node chain one walk
/// took 20 to 30 ms (release, M3 Pro), while a pair under the chain's root
/// took about 8 µs; a typical hierarchy is far shallower than it is wide. It runs where the call is accepted and in the
/// plan phase, under the read lock; a hybrid commit does not rerun it.
pub(crate) fn reject_parent_of_cycles(
    graph: &Graph,
    parent_of: &super::ParentOf<'_>,
    concepts: &[(&str, crate::types::ConceptType)],
) -> Result<(), LamboError> {
    let pairs = parent_of.pairs();
    if pairs.is_empty() {
        return Ok(());
    }
    let resolve = |content: &str| -> Result<(Option<NodeId>, String), LamboError> {
        Ok(match crate::graph::resource::canonicalize_resource(content, graph, parent_of.end_type(content, concepts), parent_of.repo_scope())? {
            CanonicalizeResult::Matched { node, key } => (Some(node), key),
            CanonicalizeResult::Unmatched { key } => (None, key),
        })
    };
    let mut resolved: Vec<[(Option<NodeId>, String); 2]> = Vec::with_capacity(pairs.len());
    for &(parent, child) in pairs {
        resolved.push([resolve(parent)?, resolve(child)?]);
    }

    // Child -> its planned parents: the walk goes up, from a pair's parent
    // through its ancestors, looking for the pair's child.
    let mut planned_parents: HashMap<End<'_>, Vec<End<'_>>> = HashMap::new();
    for [p, c] in &resolved {
        planned_parents.entry(end(c)).or_default().push(end(p));
    }
    let mut seen: HashSet<End<'_>> = HashSet::new();
    let mut stack: Vec<End<'_>> = Vec::new();
    for (i, [p, c]) in resolved.iter().enumerate() {
        let (p, c) = (end(p), end(c));
        if p == c {
            // A reflexive pair; the pre-pass refuses it with its own message.
            continue;
        }
        if let (End::Node(pn), End::Node(cn)) = (p, c)
            && graph.edge_between(pn, cn, EdgeType::Hierarchical).is_some()
        {
            continue;
        }
        if is_ancestor(graph, &planned_parents, c, p, &mut seen, &mut stack) {
            let (parent, child) = pairs[i];
            return Err(cycle_error(parent, child));
        }
    }
    Ok(())
}

/// Whether `ancestor` is `node` or one of its ancestors over existing
/// `Hierarchical` edges and the planned ones, walking up from `node`.
/// `seen` and `stack` are scratch space reused across a call's pairs.
fn is_ancestor<'k>(
    graph: &Graph,
    planned_parents: &HashMap<End<'k>, Vec<End<'k>>>,
    ancestor: End<'k>,
    node: End<'k>,
    seen: &mut HashSet<End<'k>>,
    stack: &mut Vec<End<'k>>,
) -> bool {
    seen.clear();
    stack.clear();
    stack.push(node);
    while let Some(at) = stack.pop() {
        if at == ancestor {
            return true;
        }
        if !seen.insert(at) {
            continue;
        }
        if let End::Node(id) = at {
            stack.extend(graph.hierarchical_parents(id).map(End::Node));
        }
        if let Some(next) = planned_parents.get(&at) {
            stack.extend(next.iter().copied());
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

    /// #116 review L-cost: what one cycle walk costs on a large hierarchy.
    /// A 100,000-node chain, and a pair whose parent is its leaf, so each walk
    /// climbs the whole chain: the worst case for one pair. Then a pair over
    /// the root, which has no ancestors: the common, shallow case. Ignored; run by
    /// hand in release:
    /// `cargo test --release --lib derive::cycles -- --ignored --nocapture`.
    #[test]
    #[ignore = "benchmark: run by hand in release (#116 review L-cost)"]
    fn a_cycle_walk_over_a_large_hierarchy_is_timed() {
        use crate::types::{CanonizationStatus, Concept, ConceptType, Edge, GraphSnapshot};
        const N: u64 = 100_000;
        let (seed, i) = graph();
        let interaction = match seed.node(i) {
            Some(crate::types::Node::Interaction(x)) => x.clone(),
            _ => unreachable!(),
        };
        let sid = seed.session_id().clone();
        let at = interaction.created_at;
        fn no_synonym(_: &str) -> Option<&str> {
            None
        }
        let mut concepts = Vec::new();
        let mut edges = Vec::new();
        let ids: Vec<NodeId> = (0..N).map(|_| NodeId::new()).collect();
        for (k, id) in ids.iter().enumerate() {
            let content = format!("chain node {k}");
            concepts.push(Concept {
                id: *id,
                session_id: sid.clone(),
                canonical_key: crate::graph::canonical::canonical_key(&content, no_synonym),
                content,
                concept_type: ConceptType::Entity,
                origin_interaction: i,
                origin_agent: AgentId::from("agent-a"),
                created_at: at,
                access_count: 0,
                last_accessed: None,
                gc_survived: 0,
                canonization_status: CanonizationStatus::None,
                blast_radius: None,
                last_demotion_time: None,
                embedding: None,
                human_confirmed: 0,
                embedding_source: None,
                chunk_group_id: None,
            });
            let edge = |source, target, edge_type| Edge {
                event_time: None,
                id: NodeId::new(),
                session_id: sid.clone(),
                source,
                target,
                edge_type,
                weight: 0.5,
                reinforcements: 1,
                created_at: at,
                last_reinforced: at,
            };
            edges.push(edge(i, *id, EdgeType::Derives));
            if k > 0 {
                edges.push(edge(ids[k - 1], *id, EdgeType::Hierarchical));
            }
        }
        let g = Graph::from_snapshot(GraphSnapshot {
            session_id: sid,
            interactions: vec![interaction],
            concepts,
            edges,
            ..Default::default()
        })
        .unwrap();
        let leaf = format!("chain node {}", N - 1);
        let started = std::time::Instant::now();
        let err = reject_parent_of_cycles(&g, &crate::graph::derive::ParentOf::from_pairs(&[(leaf.as_str(), "chain node 0")]), &[]).unwrap_err();
        let prepass = started.elapsed();
        assert!(err.to_string().contains("Hierarchical cycle"));
        let started = std::time::Instant::now();
        assert!(reject_closing_edge(&g, "leaf", "root", ids[N as usize - 1], ids[0]).is_err());
        let write_time = started.elapsed();
        let started = std::time::Instant::now();
        reject_parent_of_cycles(&g, &crate::graph::derive::ParentOf::from_pairs(&[("a new parent", "chain node 0")]), &[]).unwrap();
        let no_cycle = started.elapsed();
        println!(
            "cycle walk over a {N}-node chain: pre-pass {prepass:?}, write-time {write_time:?}, \
             a new parent over the root (no cycle) {no_cycle:?}"
        );
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
                    &mut std::collections::HashMap::new(),
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
