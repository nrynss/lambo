//! Snapshots and fixtures: round trips, synonyms, reservations and
//! rejection of invalid graphs.

use super::*;

#[test]
fn synonyms_declare_lookup_and_snapshot() {
    let mut g = Graph::new(sid());
    let epoch = g.epoch();
    g.declare_synonym("register_user", "create_user");
    assert_eq!(g.epoch(), epoch + 1);
    let unchanged = g.epoch();
    g.declare_synonym("register_user", "create_user");
    assert_eq!(g.epoch(), unchanged, "identical synonym is a no-op");
    g.declare_synonym("delete_user", "remove_user");
    assert_eq!(g.synonym("register_user"), Some("create_user"));
    assert_eq!(g.synonym("delete_user"), Some("remove_user"));
    assert_eq!(g.synonym("unknown"), None);
    // Replace wins.
    let before_replace = g.epoch();
    g.declare_synonym("register_user", "signup_user");
    assert_eq!(g.epoch(), before_replace + 1);
    assert_eq!(g.synonym("register_user"), Some("signup_user"));

    let snap = g.snapshot();
    let keys: Vec<&str> = snap
        .synonyms
        .iter()
        .map(|s| s.source_key.as_str())
        .collect();
    assert_eq!(keys, vec!["delete_user", "register_user"]); // sorted
    assert_eq!(snap.synonyms.len(), 2);
    assert!(g.mutation_log.is_empty(), "synonyms are RAM-local");
}

#[test]
fn reservations_round_trip_through_snapshot() {
    let mut g = Graph::new(sid());
    let r = Reservation {
        session_id: sid(),
        node_id: uid(42),
        agent_id: crate::types::AgentId::from("agent-a"),
        expires_at: ts(5),
    };
    g.set_reservation(r.clone());
    assert_eq!(g.reservation(uid(42)), Some(&r));
    g.clear_reservation(uid(42));
    assert_eq!(g.reservation(uid(42)), None);
    g.set_reservation(r.clone());
    let snap = g.snapshot();
    assert_eq!(snap.reservations, vec![r]);
}

#[cfg(feature = "fixtures")]
#[test]
fn fixture_rest_api_loads_and_passes_invariants() {
    let snap = crate::fixtures::load_snapshot("session-rest-api").unwrap();
    let g = Graph::from_snapshot(snap).unwrap();
    g.assert_invariants().unwrap();
    assert_eq!(g.node_count(), 12 + 22);
    assert_eq!(g.temporal_chain().len(), 12);
    // Every concept has a Derives edge.
    for c in g.concepts() {
        assert!(!g.in_neighbors_typed(c.id, EdgeType::Derives).is_empty());
    }
    // Snapshot round-trips exactly (fixture order == snapshot order).
    let snap2 = crate::fixtures::load_snapshot("session-rest-api").unwrap();
    assert_eq!(g.snapshot(), snap2);
}

#[test]
fn snapshot_roundtrip_preserves_structure() {
    // Adve-review T2.1 S5: snapshot equality is necessary but not sufficient —
    // the adjacency index and natural-key map must survive a round-trip too.
    let mut g = Graph::new(sid());
    let i1 = interaction(1, None, 0);
    let i1_id = i1.id;
    let i2 = interaction(2, Some(i1_id), 5);
    let i2_id = i2.id;
    g.insert_interaction(i1).unwrap();
    g.insert_interaction(i2).unwrap();
    let c = concept(1, i2_id, "user schema");
    let cid = c.id;
    g.insert_concept(c, i2_id).unwrap();
    let c2 = concept(2, i2_id, "auth middleware");
    let c2id = c2.id;
    g.insert_concept(c2, i2_id).unwrap();
    g.upsert_edge(edge(1, cid, c2id, EdgeType::CoOccurrence, 0.5))
        .unwrap();
    g.upsert_edge(edge(2, cid, c2id, EdgeType::Dependency, 0.7))
        .unwrap();

    let h = Graph::from_snapshot(g.snapshot()).unwrap();

    // Structural queries agree across the round-trip.
    assert_eq!(
        h.edge_between(cid, c2id, EdgeType::CoOccurrence),
        g.edge_between(cid, c2id, EdgeType::CoOccurrence)
    );
    assert_eq!(
        h.edge_between(cid, c2id, EdgeType::Dependency),
        g.edge_between(cid, c2id, EdgeType::Dependency)
    );
    assert_eq!(
        h.out_neighbors_typed(cid, EdgeType::CoOccurrence),
        g.out_neighbors_typed(cid, EdgeType::CoOccurrence)
    );
    assert_eq!(
        h.in_neighbors_typed(c2id, EdgeType::Dependency),
        g.in_neighbors_typed(c2id, EdgeType::Dependency)
    );
    assert_eq!(h.out_neighbors(cid), g.out_neighbors(cid));
    assert_eq!(h.in_neighbors(c2id), g.in_neighbors(c2id));
    assert_eq!(h.temporal_chain(), g.temporal_chain());
    assert_eq!(h.node_count(), g.node_count());
    assert_eq!(h.edge_count(), g.edge_count());
    h.assert_invariants().unwrap();
}

#[test]
fn empty_snapshot_roundtrips() {
    // GRAPH-6: a zero-interaction snapshot is a valid empty graph, not a
    // malformed chain ("expected exactly one chain head, found 0").
    let g = Graph::new(sid());
    let snap = g.snapshot();
    let h = Graph::from_snapshot(snap.clone()).unwrap();
    assert!(h.is_empty());
    assert_eq!(h.snapshot(), snap);
    h.assert_invariants().unwrap();
}

#[test]
fn from_snapshot_rejects_duplicate_natural_key_edges() {
    // GRAPH-7: two edges with the same (source, target, edge_type) in one
    // snapshot must be rejected — record_edge would silently merge them via
    // reinforcement, leaving a loaded graph that disagrees with the stored
    // snapshot.
    let (mut g, iid, cid) = small_graph();
    let c2 = concept(2, iid, "auth middleware");
    let c2id = c2.id;
    g.insert_concept(c2, iid).unwrap();
    g.upsert_edge(edge(1, cid, c2id, EdgeType::CoOccurrence, 0.5))
        .unwrap();
    let mut snap = g.snapshot();
    // A second edge with the same natural key (fresh id) — must be rejected.
    let mut dup = edge(2, cid, c2id, EdgeType::CoOccurrence, 0.5);
    dup.id = NodeId::new();
    snap.edges.push(dup);
    let err = Graph::from_snapshot(snap).unwrap_err().to_string();
    assert!(err.contains("duplicate natural-key edge"), "{err}");
}

#[cfg(feature = "fixtures")]
#[test]
fn fixture_drift_loads_and_passes_invariants() {
    let snap = crate::fixtures::load_snapshot("session-drift").unwrap();
    let g = Graph::from_snapshot(snap).unwrap();
    g.assert_invariants().unwrap();
    assert_eq!(g.temporal_chain().len(), 2);
    assert_eq!(g.edge_count(), 17);
    assert_eq!(g.node_count(), 9 + 2);
}

#[cfg(feature = "fixtures")]
#[test]
fn from_snapshot_rejects_violating_graphs() {
    // Edge referencing a missing node.
    let mut snap = crate::fixtures::load_snapshot("session-rest-api").unwrap();
    let mut bad_edge = snap.edges[0].clone();
    bad_edge.source = uid(999);
    snap.edges.push(bad_edge);
    assert!(Graph::from_snapshot(snap).is_err());

    // Forked temporal chain: two interactions claiming the same predecessor.
    let mut snap = crate::fixtures::load_snapshot("session-drift").unwrap();
    let mut fork = snap.interactions[1].clone();
    fork.previous_id = snap.interactions[0].previous_id; // both now head-adjacent
    fork.id = uid(700);
    fork.session_id = snap.session_id.clone();
    snap.interactions.push(fork);
    // Its own Derives/Temporal edges are missing, but the chain fork fires first.
    assert!(Graph::from_snapshot(snap).is_err());

    // Negative edge weight rejected.
    let mut snap = crate::fixtures::load_snapshot("session-drift").unwrap();
    let mut bad = snap.edges[0].clone();
    bad.weight = -0.5;
    snap.edges[0] = bad;
    assert!(Graph::from_snapshot(snap).is_err());
}
