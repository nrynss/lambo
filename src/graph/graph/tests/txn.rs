//! In-place transactions (#102) and the node-write window (#116).

use super::*;

/// Everything a rollback must restore, in comparable form.
#[derive(Debug, PartialEq)]
struct Observed {
    snapshot: GraphSnapshot,
    epoch: u64,
    log: Vec<Mutation>,
    unflushed: Vec<NodeId>,
    unflushed_contract: bool,
    keys: Vec<(String, Option<NodeId>)>,
    out_neighbors: Vec<(NodeId, Vec<NodeId>)>,
    in_neighbors: Vec<(NodeId, Vec<NodeId>)>,
    pending_accesses: usize,
}

fn observe(g: &Graph) -> Observed {
    let mut unflushed: Vec<NodeId> = g.unflushed_ids().copied().collect();
    unflushed.sort_by_key(|id| id.0);
    let mut ids: Vec<NodeId> = g.concepts().map(|c| c.id).collect();
    ids.extend(g.interactions().map(|i| i.id));
    ids.sort_by_key(|id| id.0);
    let mut keys: Vec<(String, Option<NodeId>)> = g
        .concepts()
        .map(|c| (c.canonical_key.clone(), g.concept_by_key(&c.canonical_key)))
        .collect();
    keys.sort_by(|a, b| a.0.cmp(&b.0));
    Observed {
        snapshot: g.snapshot(),
        epoch: g.epoch(),
        log: g.mutation_log.clone(),
        unflushed,
        unflushed_contract: g.contract_unflushed(),
        keys,
        out_neighbors: ids.iter().map(|id| (*id, g.out_neighbors(*id))).collect(),
        in_neighbors: ids.iter().map(|id| (*id, g.in_neighbors(*id))).collect(),
        pending_accesses: g.pending_accesses(),
    }
}

fn contract() -> crate::types::EmbeddingContract {
    crate::types::EmbeddingContract {
        kind: "txn".into(),
        model: None,
        dim: 2,
    }
}

/// A graph with two concepts joined by a CoOccurrence edge, some of it
/// flushed and some not, with an access pending: every kind of state a
/// rollback has to put back.
fn busy_graph() -> (Graph, NodeId, NodeId, NodeId) {
    let (mut g, iid, a) = small_graph();
    let b = concept(2, iid, "billing api");
    let bid = b.id;
    g.insert_concept(b, iid).unwrap();
    g.upsert_edge(edge(1, a, bid, EdgeType::CoOccurrence, 1.0))
        .unwrap();
    let flushed = g.drain_log();
    g.mark_durable_through(flushed.mutation_epoch);
    g.insert_concept(concept(3, iid, "unflushed note"), iid)
        .unwrap();
    g.record_accesses(&[(a, 1, ts(1))]);
    (g, iid, a, bid)
}

#[test]
fn a_failed_transaction_leaves_the_graph_exactly_as_it_was() {
    let (mut g, iid, a, b) = busy_graph();
    let before = observe(&g);

    let err = g
        .transaction(|g| {
            g.stamp_embedding(contract())?;
            let mut fresh = concept(10, iid, "fresh concept");
            fresh.embedding = Some(vec![1.0, 0.0]);
            let fresh_id = fresh.id;
            g.insert_concept(fresh, iid)?;
            // A re-upsert of an existing concept (a canonical match).
            let mut matched = match g.node(a) {
                Some(Node::Concept(c)) => c.clone(),
                _ => unreachable!(),
            };
            matched.content = "user schema, rewritten".into();
            g.insert_concept(matched, iid)?;
            // A reinforcement and a new edge.
            g.upsert_edge(edge(2, a, b, EdgeType::CoOccurrence, 1.0))?;
            g.upsert_edge(edge(3, fresh_id, a, EdgeType::Semantic, 0.9))?;
            // A removal, with its incident edges.
            g.remove_node(b)?;
            // Then the step that fails: a key collision.
            let mut clash = concept(11, iid, "user schema");
            clash.canonical_key = "user schema".into();
            g.insert_concept(clash, iid)
        })
        .unwrap_err();
    assert!(err.to_string().contains("collides"), "{err}");

    assert_eq!(observe(&g), before);
    g.assert_invariants().unwrap();
    // The window reports nothing for the undone writes.
    assert_eq!(g.nodes_written_since(before.epoch), Some(Vec::new()));
}

#[test]
fn a_panicking_transaction_is_undone_and_the_panic_passed_on() {
    let (mut g, iid, _a, _b) = busy_graph();
    let before = observe(&g);
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = g.transaction(|g| -> Result<(), LamboError> {
            g.insert_concept(concept(10, iid, "fresh concept"), iid)?;
            panic!("mid-commit");
        });
    }));
    assert!(caught.is_err(), "the panic reaches the caller");
    assert_eq!(observe(&g), before);
    g.assert_invariants().unwrap();
}

#[test]
fn a_committed_transaction_keeps_its_writes() {
    let (mut g, iid, _a, _b) = busy_graph();
    let epoch = g.epoch();
    let id = g
        .transaction(|g| {
            let c = concept(10, iid, "fresh concept");
            let id = c.id;
            g.insert_concept(c, iid)?;
            Ok(id)
        })
        .unwrap();
    assert!(g.node(id).is_some());
    assert_eq!(g.concept_by_key("fresh concept"), Some(id));
    assert_eq!(g.epoch(), epoch + 2, "the node and its Derives edge");
    assert_eq!(g.nodes_written_since(epoch), Some(vec![id]));
    // A later transaction can open: the first one closed.
    g.transaction(|_| Ok(())).unwrap();
    g.assert_invariants().unwrap();
}

#[test]
fn removing_and_recreating_an_edge_key_rolls_back_to_the_original_edge() {
    let (mut g, _iid, a, b) = busy_graph();
    let before = observe(&g);
    let original = g.edge_between(a, b, EdgeType::CoOccurrence).unwrap().id;
    g.transaction(|g| -> Result<(), LamboError> {
        g.remove_edge(original)?;
        g.upsert_edge(edge(9, a, b, EdgeType::CoOccurrence, 1.0))?;
        Err(LamboError::Config("abort".into()))
    })
    .unwrap_err();
    assert_eq!(observe(&g), before);
    assert_eq!(
        g.edge_between(a, b, EdgeType::CoOccurrence).unwrap().id,
        original
    );
}

#[test]
fn the_node_write_window_reports_recent_writes_and_refuses_past_its_reach() {
    let (mut g, iid, a) = small_graph();
    let start = g.epoch();
    assert_eq!(g.nodes_written_since(start), Some(Vec::new()));
    let b = concept(2, iid, "billing api");
    let bid = b.id;
    g.insert_concept(b, iid).unwrap();
    // Edges and non-node mutations are not node writes.
    g.upsert_edge(edge(1, a, bid, EdgeType::CoOccurrence, 1.0))
        .unwrap();
    assert_eq!(g.nodes_written_since(start), Some(vec![bid]));
    g.remove_node(bid).unwrap();
    assert_eq!(g.nodes_written_since(start), Some(vec![bid, bid]));

    // A re-upsert that changes only counters or content is not a write a
    // plan reads, so the window does not record it.
    let before_counters = g.epoch();
    let mut touched = match g.node(a) {
        Some(Node::Concept(c)) => c.clone(),
        _ => unreachable!(),
    };
    touched.access_count += 3;
    touched.content = "user schema, reworded".into();
    g.insert_concept(touched.clone(), iid).unwrap();
    g.bump_gc_survived(&[a]);
    assert!(g.epoch() > before_counters);
    assert_eq!(g.nodes_written_since(before_counters), Some(Vec::new()));
    // A retype is.
    touched.concept_type = ConceptType::Logic;
    g.insert_concept(touched, iid).unwrap();
    assert_eq!(g.nodes_written_since(before_counters), Some(vec![a]));

    // Past the window, an old epoch cannot be answered.
    for k in 0..(super::super::txn::NODE_WRITE_WINDOW as u64 + 1) {
        g.insert_concept(concept(100 + k, iid, &format!("bulk {k}")), iid)
            .unwrap();
    }
    assert_eq!(g.nodes_written_since(start), None);
    let recent = g.epoch() - 2;
    let last = NodeId(Uuid::from_u64_pair(
        2,
        100 + super::super::txn::NODE_WRITE_WINDOW as u64,
    ));
    assert_eq!(g.nodes_written_since(recent), Some(vec![last]));
}

#[cfg(feature = "fixtures")]
#[test]
fn a_loaded_graph_answers_from_its_load_epoch() {
    let mut snap = crate::fixtures::load_snapshot("session-rest-api").unwrap();
    snap.mutation_epoch = 40;
    let g = Graph::from_snapshot(snap).unwrap();
    assert_eq!(g.nodes_written_since(40), Some(Vec::new()));
    assert_eq!(g.nodes_written_since(39), None);
}
