//! #198: a `SuspectedSupersedes` edge connects nothing for GC (it saves no
//! orphan and carries no reachability, with no promoted-target exception),
//! and it lapses with either concept: collecting one removes the edge.

use super::*;

/// One interaction, an anchored concept 10 and the isolated concept 20.
fn session_with_island() -> Graph {
    let mut g = Graph::new(sid());
    let i1 = interaction(1, None);
    let iid = i1.id;
    g.insert_interaction(i1).unwrap();
    g.insert_concept(concept(10, 1, "anchored", ConceptType::Logic), iid)
        .unwrap();
    insert_isolated(&mut g, concept(20, 1, "island", ConceptType::Logic), 1);
    g
}

fn suspected_edges(g: &Graph) -> usize {
    g.edges()
        .filter(|e| e.edge_type == EdgeType::SuspectedSupersedes)
        .count()
}

#[test]
fn a_suspicion_saves_no_orphan_and_lapses_with_it() {
    for (src, tgt) in [(20u64, 10u64), (10, 20)] {
        let mut g = session_with_island();
        g.upsert_edge(edge(200, src, tgt, EdgeType::SuspectedSupersedes, 1.0, 0))
            .unwrap();
        assert_eq!(suspected_edges(&g), 1);
        let outcome = run(&mut g, default_params());
        assert_eq!(
            outcome.concepts_collected,
            vec![nid(20)],
            "{src} -SuspectedSupersedes-> {tgt}: the island is still an orphan"
        );
        assert!(g.node(nid(10)).is_some());
        assert_eq!(suspected_edges(&g), 0, "the edge lapsed with the island");
        g.assert_invariants().unwrap();
    }
}

#[test]
fn a_suspicion_carries_no_reachability() {
    let mut g = session_with_island();
    insert_isolated(&mut g, concept(21, 1, "island two", ConceptType::Logic), 1);
    g.upsert_edge(edge(201, 20, 21, EdgeType::CoOccurrence, 0.5, 99))
        .unwrap();
    g.upsert_edge(edge(200, 20, 10, EdgeType::SuspectedSupersedes, 1.0, 0))
        .unwrap();
    let outcome = run(&mut g, default_params());
    assert_eq!(outcome.concepts_collected, vec![nid(20), nid(21)]);
    assert_eq!(suspected_edges(&g), 0);
}

/// Unlike a contradiction, suspecting a promoted concept protects nothing.
#[test]
fn suspecting_a_promoted_concept_protects_nothing() {
    let mut g = session_with_island();
    for (ev, to) in [
        (50, CanonizationStatus::Venerable),
        (51, CanonizationStatus::Canonical),
    ] {
        let from = match g.node(nid(10)) {
            Some(Node::Concept(c)) => c.canonization_status,
            _ => unreachable!(),
        };
        g.apply_canonization_transition(CanonizationEvent {
            id: nid(ev),
            session_id: sid(),
            node_id: nid(10),
            from_status: from,
            to_status: to,
            blast_radius: None,
            last_demotion_time: None,
            occurred_at: ts(1),
            reason: None,
        })
        .unwrap();
    }
    g.upsert_edge(edge(200, 20, 10, EdgeType::SuspectedSupersedes, 1.0, 0))
        .unwrap();
    let outcome = run(&mut g, default_params());
    assert_eq!(outcome.concepts_collected, vec![nid(20)]);
}

/// The edge never decays: a sweep leaves its weight alone.
#[test]
fn the_edge_does_not_decay() {
    let mut g = session_with_island();
    g.upsert_edge(edge(201, 20, 10, EdgeType::Causal, 1.0, 0))
        .unwrap();
    g.upsert_edge(edge(200, 20, 10, EdgeType::SuspectedSupersedes, 1.0, 0))
        .unwrap();
    let _ = run(&mut g, default_params());
    let e = g
        .edge_between(nid(20), nid(10), EdgeType::SuspectedSupersedes)
        .expect("kept: both ends survive");
    assert_eq!(e.weight, 1.0);
}
