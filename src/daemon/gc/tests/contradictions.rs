//! #19(c) owner decision 1, option C: a `Contradicts` edge does not count
//! toward GC connectivity, except that a concept contradicting a Venerable
//! or Canonical concept is protected while the target holds that status.

use super::*;

fn promote(g: &mut Graph, ev: u64, node: u64, to: CanonizationStatus) {
    let from = match g.node(nid(node)) {
        Some(Node::Concept(c)) => c.canonization_status,
        _ => panic!("concept {node}"),
    };
    g.apply_canonization_transition(CanonizationEvent {
        id: nid(ev),
        session_id: sid(),
        node_id: nid(node),
        from_status: from,
        to_status: to,
        blast_radius: None,
        last_demotion_time: (to == CanonizationStatus::None).then(|| ts(2)),
        occurred_at: ts(1),
        reason: None,
    })
    .unwrap();
}

/// One interaction, an anchored concept 10 (reachable through its
/// `Derives` edge), and the isolated concept 20 (no edges at all).
fn session_with_island() -> Graph {
    let mut g = Graph::new(sid());
    let i1 = interaction(1, None);
    let iid = i1.id;
    g.insert_interaction(i1).unwrap();
    g.insert_concept(concept(10, 1, "anchored", ConceptType::Entity), iid)
        .unwrap();
    insert_isolated(&mut g, concept(20, 1, "island", ConceptType::Entity), 1);
    g
}

#[test]
fn a_contradiction_saves_no_orphan_in_either_direction() {
    for (src, tgt) in [(20u64, 10u64), (10, 20)] {
        let mut g = session_with_island();
        g.upsert_edge(edge(200, src, tgt, EdgeType::Contradicts, 1.0, 0))
            .unwrap();
        let outcome = run(&mut g, default_params());
        assert_eq!(
            outcome.concepts_collected,
            vec![nid(20)],
            "{src} -Contradicts-> {tgt}: the island is still an orphan"
        );
        assert!(g.node(nid(10)).is_some());
    }
}

#[test]
fn a_contradiction_carries_no_reachability() {
    // Island 20 is tied to island 21 by CoOccurrence (so neither is an
    // orphan) and contradicts the anchored concept 10: the pair is a
    // disconnected component.
    let mut g = session_with_island();
    insert_isolated(&mut g, concept(21, 1, "island two", ConceptType::Entity), 1);
    g.upsert_edge(edge(201, 20, 21, EdgeType::CoOccurrence, 0.5, 99))
        .unwrap();
    g.upsert_edge(edge(200, 20, 10, EdgeType::Contradicts, 1.0, 0))
        .unwrap();
    let outcome = run(&mut g, default_params());
    assert_eq!(outcome.concepts_collected, vec![nid(20), nid(21)]);

    // Control: a Dependency in the same place connects the pair.
    let mut g = session_with_island();
    insert_isolated(&mut g, concept(21, 1, "island two", ConceptType::Entity), 1);
    g.upsert_edge(edge(201, 20, 21, EdgeType::CoOccurrence, 0.5, 99))
        .unwrap();
    g.upsert_edge(edge(200, 20, 10, EdgeType::Dependency, 1.0, 0))
        .unwrap();
    let outcome = run(&mut g, default_params());
    assert!(outcome.concepts_collected.is_empty());
}

#[test]
fn contradicting_a_venerable_or_canonical_concept_is_protected() {
    for status in [CanonizationStatus::Venerable, CanonizationStatus::Canonical] {
        let mut g = session_with_island();
        promote(&mut g, 50, 10, CanonizationStatus::Venerable);
        if status == CanonizationStatus::Canonical {
            promote(&mut g, 51, 10, CanonizationStatus::Canonical);
        }
        // Island 20 has only its contradiction of the promoted concept.
        g.upsert_edge(edge(200, 20, 10, EdgeType::Contradicts, 1.0, 0))
            .unwrap();
        // A neighbour reached only through the protected island is not
        // protected: protection is not connectivity.
        insert_isolated(&mut g, concept(21, 1, "hanger-on", ConceptType::Entity), 1);
        g.upsert_edge(edge(201, 21, 20, EdgeType::CoOccurrence, 0.5, 99))
            .unwrap();
        let outcome = run(&mut g, default_params());
        assert_eq!(
            outcome.concepts_collected,
            vec![nid(21)],
            "{status:?}: the contradicting concept survives, its hanger-on does not"
        );
        assert!(g.node(nid(20)).is_some());
        assert!(outcome.survivors.contains(&nid(20)));
        // Protected concepts are outside the collection cap's base.
        assert!(outcome.deferred.is_empty());
    }
}

#[test]
fn contradicting_a_lesser_concept_protects_nothing() {
    for status in [CanonizationStatus::None, CanonizationStatus::Candidate] {
        let mut g = session_with_island();
        if status == CanonizationStatus::Candidate {
            promote(&mut g, 50, 10, CanonizationStatus::Candidate);
        }
        g.upsert_edge(edge(200, 20, 10, EdgeType::Contradicts, 1.0, 0))
            .unwrap();
        let outcome = run(&mut g, default_params());
        assert_eq!(outcome.concepts_collected, vec![nid(20)], "{status:?}");
    }
}

/// Being contradicted protects nothing: only the contradicting end of an
/// edge into a promoted concept is protected.
#[test]
fn being_contradicted_by_a_promoted_concept_protects_nothing() {
    let mut g = session_with_island();
    promote(&mut g, 50, 10, CanonizationStatus::Venerable);
    g.upsert_edge(edge(200, 10, 20, EdgeType::Contradicts, 1.0, 0))
        .unwrap();
    let outcome = run(&mut g, default_params());
    assert_eq!(outcome.concepts_collected, vec![nid(20)]);
}

/// The boundary: the protection holds while the target is Canonical, and
/// lapses on the first sweep after the target is demoted.
#[test]
fn demoting_the_target_makes_the_contradicting_concept_collectable() {
    let mut g = session_with_island();
    promote(&mut g, 50, 10, CanonizationStatus::Venerable);
    promote(&mut g, 51, 10, CanonizationStatus::Canonical);
    g.upsert_edge(edge(200, 20, 10, EdgeType::Contradicts, 1.0, 0))
        .unwrap();

    let first = run(&mut g, default_params());
    assert!(
        first.concepts_collected.is_empty(),
        "protected while Canonical"
    );

    promote(&mut g, 52, 10, CanonizationStatus::None);
    let mut pending = first.survivors_pending.clone();
    drain_survivor_bumps(&mut g, &mut pending, usize::MAX);
    let second = run(&mut g, default_params());
    assert_eq!(second.concepts_collected, vec![nid(20)]);
    assert!(
        g.edge(nid(200)).is_none(),
        "the contradiction goes with its concept"
    );
}

/// The score cut honours the protection too: a sub-threshold Resource that
/// contradicts a Canonical concept is kept, and cut once the target is
/// demoted.
#[test]
fn the_score_cut_spares_a_protected_contradicting_concept() {
    let mut g = hub_session(&[(30, ConceptType::Resource)]);
    promote(&mut g, 50, 13, CanonizationStatus::Venerable);
    promote(&mut g, 51, 13, CanonizationStatus::Canonical);
    g.upsert_edge(edge(200, 30, 13, EdgeType::Contradicts, 1.0, 0))
        .unwrap();
    // A bar every unprotected Resource is under (as in
    // `resources_with_dependents_survive_the_score_cut_isolated_ones_age_out`).
    let params = GcParams {
        min_concept_score: 5.0,
        ..aged_params()
    };
    let kept = run(&mut g.clone(), params);
    assert!(!kept.concepts_collected.contains(&nid(30)));

    promote(&mut g, 52, 13, CanonizationStatus::None);
    let cut = run(&mut g, params);
    assert!(cut.concepts_collected.contains(&nid(30)));
}

/// Review M1: GC dates a contradiction as the detector does (about-time
/// clamped to the write time). One dated 30 days ahead but written now
/// protects now, as its recall warning fires now; one not yet written at
/// the sweep's clock protects nothing yet.
#[test]
fn protection_uses_the_detectors_declaration_date() {
    let mut g = session_with_island();
    promote(&mut g, 50, 10, CanonizationStatus::Venerable);
    g.upsert_edge(Edge {
        event_time: Some(ts(0) + ChronoDuration::days(30)),
        ..edge(200, 20, 10, EdgeType::Contradicts, 1.0, 0)
    })
    .unwrap();
    assert!(run(&mut g, default_params()).concepts_collected.is_empty());

    let mut g = session_with_island();
    promote(&mut g, 50, 10, CanonizationStatus::Venerable);
    // Written at ts(200), after the sweep's clock (ts(100)).
    g.upsert_edge(edge(200, 20, 10, EdgeType::Contradicts, 1.0, 200))
        .unwrap();
    assert_eq!(
        run(&mut g, default_params()).concepts_collected,
        vec![nid(20)]
    );

    // Written after the clock but backdated before it (PR #154 review):
    // still not written at the sweep, so still no protection.
    let mut g = session_with_island();
    promote(&mut g, 50, 10, CanonizationStatus::Venerable);
    g.upsert_edge(Edge {
        event_time: Some(ts(0)),
        ..edge(200, 20, 10, EdgeType::Contradicts, 1.0, 200)
    })
    .unwrap();
    assert_eq!(
        run(&mut g, default_params()).concepts_collected,
        vec![nid(20)]
    );
}
