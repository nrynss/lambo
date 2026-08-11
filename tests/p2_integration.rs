//! P2 branch-level integration tests (muse-spark M3/S3): all three write APIs
//! on one graph, mutation-log ordering across modules, snapshot round-trip,
//! reservations, and the `InvertedIndex` owner-sync contract.

use lambo::graph::action::{record_action, Action};
use lambo::graph::demote::demote;
use lambo::graph::derive::{derive, ParentOf};
use lambo::graph::index::InvertedIndex;
use lambo::graph::reserve::{active_reservation, release, reserve};
use lambo::graph::Graph;
use lambo::types::{AgentId, Concept, ConceptType, Interaction, Mutation, Node, NodeId, SessionId};
use std::collections::HashSet;
use std::time::Duration;

fn ts() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::<chrono::Utc>::from_timestamp(1_752_000_000, 0).unwrap()
}

fn fresh_graph() -> (Graph, NodeId) {
    let sid = SessionId::from("p2-integration");
    let mut g = Graph::new(sid.clone());
    let i = Interaction {
        id: NodeId::new(),
        session_id: sid,
        agent_id: AgentId::from("agent-a"),
        prompt_text: Some("kickoff".into()),
        previous_id: None,
        created_at: ts(),
    };
    let iid = i.id;
    g.insert_interaction(i).unwrap();
    (g, iid)
}

/// Clone a concept node out of the graph (the owner's mirror reads).
fn concept_of(g: &Graph, id: NodeId) -> Concept {
    match g.node(id) {
        Some(Node::Concept(c)) => c.clone(),
        _ => panic!("{id} is not a concept"),
    }
}

/// The owner-sync contract for InvertedIndex (muse-spark M3): every concept
/// the graph creates must be mirrored into the index by the session owner.
/// This test proves the sequence works when wired — the graph itself is
/// index-free by design.
#[test]
fn inverted_index_manual_sync_contract() {
    let (mut g, iid) = fresh_graph();
    let agent = AgentId::from("agent-a");
    let mut idx = InvertedIndex::new();

    let out = derive(
        &mut g,
        iid,
        &agent,
        &[
            ("pagination", ConceptType::Entity),
            ("rate limiter", ConceptType::Entity),
            ("api docs", ConceptType::Entity),
        ],
        &ParentOf::none(),
        10,
    )
    .unwrap();
    assert_eq!(out.created.len(), 3);
    for &n in &out.created {
        idx.add(&concept_of(&g, n));
    }

    // Derived concept is searchable.
    let hits = idx.search("pagination", 10);
    assert!(hits.iter().any(|s| s.item == out.created[0]));

    // remove_node -> index.remove -> search no longer returns it.
    g.remove_node(out.created[1]).unwrap();
    idx.remove(out.created[1]);
    assert!(idx.search("limiter", 10).is_empty(), "stale posting");

    // demote -> mirror the new Observations -> search finds them.
    let obs = demote(
        &mut g,
        iid,
        &agent,
        "caching layer was the bottleneck.",
        "chunk-9",
    )
    .unwrap();
    assert_eq!(obs.len(), 1);
    for &n in &obs {
        idx.add(&concept_of(&g, n));
    }
    assert!(idx
        .search("caching", 10)
        .iter()
        .any(|s| obs.contains(&s.item)));
}

/// All three write APIs interleaved on one graph: invariants hold, the
/// mutation log stays chronological with nodes before referencing edges, and
/// a snapshot round-trip preserves everything including reservations.
#[test]
fn write_paths_interleave_with_mutation_log_and_invariants() {
    let (mut g, iid) = fresh_graph();
    let agent_a = AgentId::from("agent-a");
    let agent_b = AgentId::from("agent-b");

    // derive: two concepts (one derive call -> pairwise CoOccurrence too).
    let out = derive(
        &mut g,
        iid,
        &agent_a,
        &[
            ("user schema", ConceptType::Entity),
            ("auth middleware", ConceptType::Entity),
        ],
        &ParentOf::none(),
        10,
    )
    .unwrap();
    assert_eq!(out.created.len(), 2);
    let us = out.created[0];

    // record_action: Resource concept + Causal/Dependency edges.
    let action = Action {
        action: "created migrations/003.sql",
        produces: &["migrations/003.sql"],
        modifies: &[],
        depends_on: &["user schema"],
    };
    let ao = record_action(&mut g, iid, &agent_a, &action).unwrap();
    assert!(g.edge_count() > 0);

    // demote: two Observations sharing a chunk group.
    let obs = demote(
        &mut g,
        iid,
        &agent_a,
        "First sentence. Second sentence!",
        "chunk-1",
    )
    .unwrap();
    assert_eq!(obs.len(), 2);

    // reserve: agent-b locks user schema for 60s.
    let t0 = chrono::Utc::now();
    reserve(&mut g, us, &agent_b, Duration::from_secs(60), t0).unwrap();
    assert!(active_reservation(&g, us, t0 + chrono::Duration::seconds(30)).is_some());
    // Cross-agent re-reserve while live is denied.
    assert!(reserve(&mut g, us, &agent_a, Duration::from_secs(60), t0).is_err());

    g.assert_invariants().unwrap();
    assert!(g.node_count() >= 2 + 2 + 1 + 2); // interaction + derives + action + observations

    // Mutation log: chronological, nodes before the edges that reference them.
    let batch = g.drain_log();
    let mut seen_nodes: HashSet<NodeId> = HashSet::new();
    for m in &batch.mutations {
        match m {
            Mutation::UpsertNode { node } => {
                seen_nodes.insert(node.id());
            }
            Mutation::UpsertEdge { edge } => {
                assert!(
                    seen_nodes.contains(&edge.source),
                    "edge before source: {m:?}"
                );
                assert!(
                    seen_nodes.contains(&edge.target),
                    "edge before target: {m:?}"
                );
            }
            _ => {}
        }
    }

    // Snapshot round-trip preserves structure AND reservations (which the
    // mutation log never carries — muse-spark S5).
    let snap = g.snapshot();
    let h = Graph::from_snapshot(snap).unwrap();
    h.assert_invariants().unwrap();
    assert_eq!(h.node_count(), g.node_count());
    assert_eq!(h.edge_count(), g.edge_count());
    assert!(active_reservation(&h, us, t0 + chrono::Duration::seconds(30)).is_some());

    // The action concept is a Resource with a Derives edge (structural).
    let action_node = concept_of(&g, ao.action_node);
    assert_eq!(action_node.concept_type, ConceptType::Resource);

    // Owner release works; a non-owner is denied.
    release(&mut g, us, &agent_b).unwrap();
    assert!(active_reservation(&g, us, t0 + chrono::Duration::seconds(30)).is_none());
}
