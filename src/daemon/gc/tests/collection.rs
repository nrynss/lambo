//! What a sweep collects: stale edges, orphans, islands, protected
//! classes and the fixture sessions.

use super::*;

fn transition(
    ev_id: u64,
    node: u64,
    from: CanonizationStatus,
    to: CanonizationStatus,
) -> CanonizationEvent {
    CanonizationEvent {
        id: nid(ev_id),
        session_id: sid(),
        node_id: nid(node),
        from_status: from,
        to_status: to,
        blast_radius: None,
        last_demotion_time: None,
        occurred_at: ts(1),
    }
}

// ------------------------------------------------------------------
// Step 1 — edge cleanup
// ------------------------------------------------------------------

#[test]
fn edge_cleanup_removes_stale_low_weight_edges_only() {
    let mut g = Graph::new(sid());
    let i1 = interaction(1, None);
    let iid = i1.id;
    g.insert_interaction(i1).unwrap();
    g.insert_concept(concept(10, 1, "kept concept", ConceptType::Entity), iid)
        .unwrap();
    g.insert_concept(concept(11, 1, "decay concept", ConceptType::Entity), iid)
        .unwrap();

    // Stale (ts 0, past the 1h TTL) AND below min weight -> removed.
    g.upsert_edge(edge(100, 10, 11, EdgeType::CoOccurrence, 0.1, 0))
        .unwrap();
    // Fresh (ts 99, inside the TTL) though below weight -> kept.
    g.upsert_edge(edge(101, 11, 10, EdgeType::CoOccurrence, 0.1, 99))
        .unwrap();
    // Stale but heavy -> kept.
    g.upsert_edge(edge(102, 10, 11, EdgeType::Semantic, 0.9, 0))
        .unwrap();

    let outcome = run(&mut g, default_params());
    assert_eq!(outcome.edges_removed, vec![nid(100)]);
    assert!(g.edge(nid(100)).is_none());
    assert!(g.edge(nid(101)).is_some(), "fresh edge must survive");
    assert!(g.edge(nid(102)).is_some(), "heavy edge must survive");
    // Concepts stay connected through the surviving edges.
    assert!(outcome.concepts_collected.is_empty());
    assert_eq!(outcome.survivors, vec![nid(10), nid(11)]);
    assert!(outcome.epoch_after > outcome.epoch_before);
}

/// ALGO-9: step 1 only cuts **decaying** edge types (spec §5 table). A
/// structural edge that is stale and under the weight bar must survive —
/// its weight is a property of its kind, not a decayed signal, and §5.7
/// depends on it.
///
/// The margin is zero today: `record_action` writes `Causal`/`Dependency` at
/// exactly `MIN_EDGE_WEIGHT` against a strict `<`, so this test uses a
/// sub-threshold structural weight — which the pre-fix pass collected — plus
/// a decaying edge at the same weight to prove the cut still bites.
#[test]
fn edge_cleanup_spares_non_decaying_edge_types() {
    let mut g = Graph::new(sid());
    let i1 = interaction(1, None);
    let iid = i1.id;
    g.insert_interaction(i1).unwrap();
    // Protected (Canonical) endpoints, so nothing is collected for score.
    for n in [10u64, 11] {
        let c = Concept {
            canonization_status: CanonizationStatus::Canonical,
            ..concept(n, 1, &format!("structural {n}"), ConceptType::Entity)
        };
        g.insert_concept(c, iid).unwrap();
    }
    // Stale + under weight, but structural: must survive (ALGO-9).
    for (id, ty) in [
        (200u64, EdgeType::Dependency),
        (201, EdgeType::Causal),
        (202, EdgeType::Hierarchical),
    ] {
        g.upsert_edge(edge(id, 10, 11, ty, 0.4, 0)).unwrap();
    }
    // Same weight and age, decaying type: must go.
    g.upsert_edge(edge(203, 10, 11, EdgeType::CoOccurrence, 0.4, 0))
        .unwrap();

    let outcome = run(&mut g, default_params());
    assert_eq!(
        outcome.edges_removed,
        vec![nid(203)],
        "only the decaying edge is collected"
    );
    for id in [200u64, 201, 202] {
        assert!(
            g.edge(nid(id)).is_some(),
            "structural edge {id} must survive step 1 (spec §5 table)"
        );
    }
    // And the Derives provenance edges — also non-decaying — are intact.
    assert!(g.edge_between(iid, nid(10), EdgeType::Derives).is_some());
}

// ------------------------------------------------------------------
// Step 2 — orphan / sub-threshold concept cleanup
// ------------------------------------------------------------------

#[test]
fn orphan_and_subthreshold_collected_protected_survive() {
    let mut g = Graph::new(sid());
    let i1 = interaction(1, None);
    // Second interaction 100 min later gives the session a temporal span,
    // so concepts written at ts(0) have zero recency.
    let i2 = Interaction {
        created_at: ts(100),
        ..interaction(2, Some(1))
    };
    let iid = i1.id;
    g.insert_interaction(i1).unwrap();
    g.insert_interaction(i2).unwrap();

    // Orphan: no edges at all -> step 2, not protected.
    let orphan = insert_isolated(&mut g, concept(10, 1, "orphan", ConceptType::Entity), 1);
    // Sub-threshold (and not an orphan): a Resource whose only edge is
    // its Derives provenance — zero recency, minimum density (1 of the
    // hub's 4), no type bonus, against the 0.12 bar (resistance 1.0). It
    // has no structural dependents, so the dependents rule does not spare
    // it. (An Observation is no longer in the score cut at all.)
    let low_id = nid(11);
    g.insert_concept(concept(11, 1, "low value", ConceptType::Resource), iid)
        .unwrap();
    // Protected: Venerable, isolated -> must survive both step 2 and 3.
    let protected = insert_isolated(
        &mut g,
        concept(12, 1, "venerable island", ConceptType::Entity),
        1,
    );
    g.apply_canonization_transition(transition(
        50,
        12,
        CanonizationStatus::None,
        CanonizationStatus::Venerable,
    ))
    .unwrap();
    // A hub (13) plus three spokes: max incident = 4, so the Resource's
    // density is 1/4. All four must survive.
    for id in [13u64, 14, 15, 16] {
        g.insert_concept(
            concept(id, 1, &format!("anchor {id}"), ConceptType::Entity),
            iid,
        )
        .unwrap();
    }
    for (eid, tgt) in [(101u64, 14u64), (102, 15), (103, 16)] {
        g.upsert_edge(edge(eid, 13, tgt, EdgeType::Dependency, 1.0, 0))
            .unwrap();
    }

    // Sanity: the Resource really is sub-threshold under the cut GC
    // applies — live-dimension score vs. its own type's bar (ALGO-1/11).
    // Aged past the recency window: GC's recency is time-anchored (#29).
    // A Resource carries no type modifier, so against the default 0.12 bar
    // this fixture's leaf (density 1/4, derived by half the interactions)
    // is out of the cut's reach; 0.3 sits above the leaf's score and below
    // the Entity anchors' own bar (0.3 / 1.2 = 0.25), asserted below.
    let params = GcParams {
        min_concept_score: 0.3,
        ..aged_params()
    };
    let ctx = crate::daemon::score::SessionContext::compute(&g);
    let low = match g.node(low_id).unwrap() {
        Node::Concept(c) => c,
        _ => unreachable!(),
    };
    let low_score = eviction_score(&g, low, &ctx, params);
    let low_bar = eviction_threshold(params.min_concept_score, ConceptType::Resource);
    assert!(
        low_score < low_bar,
        "test premise: score {low_score} must be under the Resource bar {low_bar}"
    );
    for id in [13u64, 14, 15, 16] {
        let anchor = match g.node(nid(id)).unwrap() {
            Node::Concept(c) => c,
            _ => unreachable!(),
        };
        let bar = eviction_threshold(params.min_concept_score, ConceptType::Entity);
        let score = eviction_score(&g, anchor, &ctx, params);
        assert!(
            score >= bar,
            "test premise: anchor {id} score {score} must clear its bar {bar}"
        );
    }

    let outcome = run(&mut g, params);
    assert_eq!(outcome.concepts_collected, vec![nid(10), nid(11)]);
    assert!(g.node(orphan).is_none());
    assert!(g.node(low_id).is_none());
    assert!(g.node(protected).is_some(), "protected class must survive");
    for id in [13u64, 14, 15, 16] {
        assert!(g.node(nid(id)).is_some(), "anchor {id} must survive");
    }
    // The Venerable island is a survivor and its counter increments.
    assert!(outcome.survivors.contains(&protected));
    let p = match g.node(protected).unwrap() {
        Node::Concept(c) => c,
        _ => unreachable!(),
    };
    assert_eq!(p.gc_survived, 1);
    assert_eq!(p.canonization_status, CanonizationStatus::Venerable);
}

// ------------------------------------------------------------------
// Step 2 — calibration (ALGO-1 / ALGO-4 / ALGO-11)
// ------------------------------------------------------------------

/// ALGO-1: the shipped demo session must survive a sweep.
///
/// Pre-fix (flat `MIN_CONCEPT_SCORE = 0.3` against the full spec §9
/// composite) this collected **15 of the 22** concepts on the first run —
/// `auth middleware` (spec §13 step 1) among them — leaving 6 non-Canonical
/// peers where canonization Stage 1 needs 20, i.e. GC starved the pipeline
/// it exists to feed.
#[cfg(feature = "fixtures")]
#[test]
fn rest_api_demo_session_survives_a_gc_sweep() {
    let snap = crate::fixtures::load_snapshot("session-rest-api").unwrap();
    let mut g = Graph::from_snapshot(snap).unwrap();
    assert_eq!(g.concepts().count(), 22, "fixture premise");

    // Every fixture edge is >= 0.6 (> MIN_EDGE_WEIGHT), so step 1 cannot
    // fire and `now` only has to be past the TTL to prove that.
    let outcome = run(
        &mut g,
        GcParams {
            now: Utc
                .with_ymd_and_hms(2026, 8, 12, 0, 0, 0)
                .single()
                .expect("valid timestamp"),
            ..Default::default()
        },
    );
    assert!(
        outcome.edges_removed.is_empty(),
        "no fixture edge is below min_edge_weight"
    );
    assert!(
        outcome.concepts_collected.is_empty(),
        "a healthy session must survive a sweep intact, collected: {:?}",
        outcome
            .concepts_collected
            .iter()
            .map(|id| match g.node(*id) {
                Some(Node::Concept(c)) => c.content.clone(),
                _ => id.to_string(),
            })
            .collect::<Vec<_>>()
    );

    // The concepts spec §13 names by content (`session store` has no
    // counterpart in this fixture) plus the planted conflict node.
    let surviving: Vec<&str> = g.concepts().map(|c| c.content.as_str()).collect();
    for named in ["user schema", "auth middleware", "caching layer"] {
        assert!(
            surviving.contains(&named),
            "spec §13 names {named}; it must survive GC"
        );
    }

    // Canonization Stage 1 needs >= 20 non-Canonical peers in the session.
    let peers = g
        .concepts()
        .filter(|c| c.canonization_status != CanonizationStatus::Canonical)
        .count();
    assert!(
        peers >= 20,
        "Stage 1 needs >= 20 non-Canonical peers, found {peers}"
    );
}

// ------------------------------------------------------------------
// Step 3 — disconnected-component cleanup
// ------------------------------------------------------------------

/// ALGO-6: an **array** root goal must protect every concept it names.
///
/// GC's exclusion list read only the string and object shapes, so an array
/// goal — spec §6.1's own example — protected nothing: the session's goal
/// concepts became ordinary GC candidates. Isolated (step-3) goal concepts
/// make the exclusion the only thing keeping them alive.
#[test]
fn array_root_goal_protects_every_named_concept() {
    let mut g = Graph::new(sid());
    let i1 = interaction(1, None);
    let iid = i1.id;
    g.insert_interaction(i1).unwrap();
    g.insert_concept(concept(10, 1, "anchored", ConceptType::Entity), iid)
        .unwrap();
    // Both goal concepts are isolated: only the root-goal exclusion can
    // save them from step 3.
    let goal_a = insert_isolated(
        &mut g,
        concept(20, 1, "launch the product", ConceptType::Entity),
        1,
    );
    let goal_b = insert_isolated(
        &mut g,
        concept(21, 1, "ship the API", ConceptType::Entity),
        1,
    );
    // A third isolated concept the goal does NOT name — the control.
    let bystander = insert_isolated(
        &mut g,
        concept(22, 1, "unnamed island", ConceptType::Entity),
        1,
    );

    g.set_root_goal(Some(serde_json::json!([
        "launch the product",
        "ship the API"
    ])));
    assert_eq!(g.root_goal_texts().len(), 2);
    // `set_root_goal` also auto-promotes both to Venerable (spec §9), which
    // would protect them by status alone. Demote them so the *exclusion
    // list* is the only thing under test.
    for (ev, node) in [(60u64, 20u64), (61, 21)] {
        g.apply_canonization_transition(transition(
            ev,
            node,
            CanonizationStatus::Venerable,
            CanonizationStatus::Canonical,
        ))
        .unwrap();
        g.apply_canonization_transition(transition(
            ev + 100,
            node,
            CanonizationStatus::Canonical,
            CanonizationStatus::None,
        ))
        .unwrap();
    }

    let outcome = run(&mut g, default_params());
    for id in [goal_a, goal_b] {
        assert!(
            g.node(id).is_some(),
            "an array-named goal concept must be protected: {outcome:?}"
        );
    }
    assert!(
        g.node(bystander).is_none(),
        "an unnamed isolated concept is still collected"
    );
}

#[test]
fn protected_classes_survive_disconnected_component() {
    let mut g = Graph::new(sid());
    let i1 = interaction(1, None);
    let i2 = interaction(2, Some(1));
    let iid = i1.id;
    g.insert_interaction(i1).unwrap();
    g.insert_interaction(i2).unwrap();
    // Anchored concept keeps the main component reachable.
    g.insert_concept(concept(10, 1, "anchored", ConceptType::Entity), iid)
        .unwrap();

    // Two isolated islands, protected by status, with no path to the chain.
    let venerable = insert_isolated(
        &mut g,
        concept(20, 1, "venerable island", ConceptType::Entity),
        1,
    );
    let canonical = insert_isolated(
        &mut g,
        concept(21, 1, "canonical island", ConceptType::Entity),
        1,
    );
    g.apply_canonization_transition(transition(
        50,
        20,
        CanonizationStatus::None,
        CanonizationStatus::Venerable,
    ))
    .unwrap();
    g.apply_canonization_transition(transition(
        51,
        21,
        CanonizationStatus::None,
        CanonizationStatus::Venerable,
    ))
    .unwrap();
    g.apply_canonization_transition(transition(
        52,
        21,
        CanonizationStatus::Venerable,
        CanonizationStatus::Canonical,
    ))
    .unwrap();

    let outcome = run(&mut g, default_params());
    assert!(
        outcome.concepts_collected.is_empty(),
        "only unprotected concepts may be collected"
    );
    assert!(g.node(venerable).is_some());
    assert!(g.node(canonical).is_some());
    assert_eq!(outcome.survivors.len(), 3, "anchored + both islands");
    // The islands' counters increment even though they are protected.
    for id in [venerable, canonical] {
        let c = match g.node(id).unwrap() {
            Node::Concept(c) => c,
            _ => unreachable!(),
        };
        assert_eq!(c.gc_survived, 1);
    }
}

#[test]
fn disconnected_bfs_is_cycle_safe() {
    // A Hierarchical cycle in a component with no path to the temporal
    // chain (G6: multi-hop Hierarchical cycles are writable; the BFS must
    // terminate and collect the whole component).
    let mut g = Graph::new(sid());
    let i1 = interaction(1, None);
    let iid = i1.id;
    g.insert_interaction(i1).unwrap();
    g.insert_concept(concept(10, 1, "main", ConceptType::Entity), iid)
        .unwrap();
    insert_isolated(&mut g, concept(20, 1, "cycle x", ConceptType::Entity), 1);
    insert_isolated(&mut g, concept(21, 1, "cycle y", ConceptType::Entity), 1);
    insert_isolated(&mut g, concept(22, 1, "cycle z", ConceptType::Entity), 1);
    g.upsert_edge(edge(100, 20, 21, EdgeType::Hierarchical, 1.0, 0))
        .unwrap();
    g.upsert_edge(edge(101, 21, 22, EdgeType::Hierarchical, 1.0, 0))
        .unwrap();
    g.upsert_edge(edge(102, 22, 20, EdgeType::Hierarchical, 1.0, 0))
        .unwrap();

    let outcome = run(&mut g, default_params());
    assert_eq!(outcome.concepts_collected, vec![nid(20), nid(21), nid(22)]);
    assert!(g.node(nid(10)).is_some(), "anchored concept survives");
    // The cycle component is gone, so the graph is well-formed again.
    g.assert_invariants().unwrap();
}

// ------------------------------------------------------------------
// Steps 5–7 — survivors, canonical budget, epoch
// ------------------------------------------------------------------

#[test]
fn empty_graph_run_is_safe() {
    let mut g = Graph::new(sid());
    let outcome = run(&mut g, default_params());
    assert!(outcome.concepts_collected.is_empty());
    assert!(outcome.edges_removed.is_empty());
    assert!(outcome.survivors.is_empty());
    assert!(!outcome.canonical_over_budget);
    // Nothing to bump: a concept-free session has no survivors (see docs).
    assert_eq!(outcome.epoch_after, outcome.epoch_before);
}

#[test]
fn canonical_budget_records_over_budget_without_demotion() {
    let mut g = Graph::new(sid());
    let i1 = interaction(1, None);
    let iid = i1.id;
    g.insert_interaction(i1).unwrap();
    for id in [10u64, 11] {
        g.insert_concept(
            concept(id, 1, &format!("canon {id}"), ConceptType::Entity),
            iid,
        )
        .unwrap();
        g.apply_canonization_transition(transition(
            id,
            id,
            CanonizationStatus::None,
            CanonizationStatus::Venerable,
        ))
        .unwrap();
        g.apply_canonization_transition(transition(
            id + 100,
            id,
            CanonizationStatus::Venerable,
            CanonizationStatus::Canonical,
        ))
        .unwrap();
    }

    let outcome = run(
        &mut g,
        GcParams {
            max_canonical_nodes: 1,
            ..default_params()
        },
    );
    assert_eq!(outcome.canonical_count, 2);
    assert!(outcome.canonical_over_budget);
    assert_eq!(outcome.max_canonical_nodes, 1);
    assert!(
        outcome.warnings.iter().any(|w| w.contains("T6.4")),
        "over-budget must be recorded for T6.4"
    );
    // No demotion happened here (T6.4's job): both remain Canonical.
    for id in [10u64, 11] {
        let c = match g.node(nid(id)).unwrap() {
            Node::Concept(c) => c,
            _ => unreachable!(),
        };
        assert_eq!(c.canonization_status, CanonizationStatus::Canonical);
    }
}

#[test]
fn max_concept_nodes_warns_without_evicting() {
    let mut g = Graph::new(sid());
    let i1 = interaction(1, None);
    let iid = i1.id;
    g.insert_interaction(i1).unwrap();
    g.insert_concept(concept(10, 1, "concept a", ConceptType::Entity), iid)
        .unwrap();
    g.insert_concept(concept(11, 1, "concept b", ConceptType::Entity), iid)
        .unwrap();

    let outcome = run(
        &mut g,
        GcParams {
            max_concept_nodes: 1,
            ..default_params()
        },
    );
    assert!(outcome
        .warnings
        .iter()
        .any(|w| w.contains("max_concept_nodes")));
    assert!(
        outcome.concepts_collected.is_empty(),
        "advisory capacity must never evict"
    );
    assert!(g.node(nid(10)).is_some() && g.node(nid(11)).is_some());
}

// ------------------------------------------------------------------
// Step 4 — T2.6 index maintenance hook
// ------------------------------------------------------------------

#[cfg(feature = "fixtures")]
#[test]
fn index_sync_removes_collected_concepts() {
    let snap = crate::fixtures::load_snapshot("session-drift").unwrap();
    let mut index = InvertedIndex::from_snapshot(&snap);
    let isolated = NodeId(Uuid::parse_str("f0000000-0000-4000-8000-000000005020").unwrap());
    assert!(
        index
            .search("widget", 10)
            .iter()
            .any(|s| s.item == isolated),
        "test premise: isolated widget is indexed"
    );

    let mut g = Graph::from_snapshot(snap).unwrap();
    for eid in [
        "f0000000-0000-4000-8000-000000014009",
        "f0000000-0000-4000-8000-000000014010",
    ] {
        g.remove_edge(NodeId(Uuid::parse_str(eid).unwrap()))
            .unwrap();
    }
    let outcome = run(&mut g, default_params());
    sync_index(&outcome, &mut index);
    assert!(
        !index
            .search("widget", 10)
            .iter()
            .any(|s| s.item == isolated),
        "collected concept must leave the inverted index"
    );
}

// ------------------------------------------------------------------
// Fixture — session-drift planted disconnected component (done-when)
// ------------------------------------------------------------------

#[cfg(feature = "fixtures")]
#[test]
fn session_drift_disconnected_component_collected() {
    let snap = crate::fixtures::load_snapshot("session-drift").unwrap();
    let mut g = Graph::from_snapshot(snap).unwrap();

    // Materialize the planted disconnection (see module docs): the
    // "isolated" pair's only link to the temporal chain is its two
    // Derives provenance edges — drop them (TEST-ONLY state
    // reconstruction; step 1's predicate never touches 0.9-weight
    // edges), so step 3 sees the component the generator planted.
    for eid in [
        "f0000000-0000-4000-8000-000000014009",
        "f0000000-0000-4000-8000-000000014010",
    ] {
        g.remove_edge(NodeId(Uuid::parse_str(eid).unwrap()))
            .unwrap();
    }
    let isolated_widget = NodeId(Uuid::parse_str("f0000000-0000-4000-8000-000000005020").unwrap());
    let isolated_sibling = NodeId(Uuid::parse_str("f0000000-0000-4000-8000-000000005021").unwrap());
    let goal = NodeId(Uuid::parse_str("f0000000-0000-4000-8000-000000005010").unwrap());
    let step_one = NodeId(Uuid::parse_str("f0000000-0000-4000-8000-000000005011").unwrap());

    let now = Utc
        .with_ymd_and_hms(2026, 8, 12, 0, 0, 0)
        .single()
        .expect("valid timestamp");
    let epoch_before = g.epoch();
    let outcome = run(
        &mut g,
        GcParams {
            now,
            ..Default::default()
        },
    );

    // Step 3 collected exactly the planted disconnected component.
    assert_eq!(
        outcome.concepts_collected,
        vec![isolated_widget, isolated_sibling]
    );
    assert!(
        outcome.edges_removed.is_empty(),
        "fixture edges are all above min_edge_weight"
    );

    // Protected classes survive: the Venerable root goal is still present
    // and its counter incremented 5 -> 6 (step 5).
    let goal_c = match g.node(goal).unwrap() {
        Node::Concept(c) => c,
        _ => unreachable!(),
    };
    assert_eq!(goal_c.canonization_status, CanonizationStatus::Venerable);
    assert_eq!(goal_c.gc_survived, 6);
    // A path concept also survives and increments 2 -> 3.
    let step_c = match g.node(step_one).unwrap() {
        Node::Concept(c) => c,
        _ => unreachable!(),
    };
    assert_eq!(step_c.gc_survived, 3);

    // The planted component is gone; every remaining concept is a survivor.
    assert!(g.node(isolated_widget).is_none());
    assert!(g.node(isolated_sibling).is_none());
    assert_eq!(outcome.survivors.len(), 7);
    assert!(outcome.survivors.contains(&goal));

    // Step 7: the epoch bumped (removals + survivor upserts all append
    // mutations).
    assert!(outcome.epoch_after > outcome.epoch_before);
    assert_eq!(outcome.epoch_after, g.epoch());
    assert_eq!(outcome.epoch_before, epoch_before);

    // The graph is well-formed again after cleanup.
    g.assert_invariants().unwrap();
}

/// Issue #29: the CoOccurrence margin is zero on purpose. `derive` writes
/// co-occurrence edges at exactly `MIN_EDGE_WEIGHT` and step 1 is a strict
/// `<`, so a stale, never-reinforced co-occurrence edge survives. If
/// either constant moves, this fails and the change has to be made
/// deliberately (see `MIN_EDGE_WEIGHT`).
#[test]
fn cooccurrence_edges_sit_exactly_on_the_step_one_bar_and_survive() {
    assert_eq!(crate::graph::derive::COOCCURRENCE_WEIGHT, MIN_EDGE_WEIGHT);
    let mut g = Graph::new(sid());
    let i1 = interaction(1, None);
    let iid = i1.id;
    g.insert_interaction(i1).unwrap();
    g.insert_concept(concept(10, 1, "a", ConceptType::Entity), iid)
        .unwrap();
    g.insert_concept(concept(11, 1, "b", ConceptType::Entity), iid)
        .unwrap();
    g.upsert_edge(edge(
        100,
        10,
        11,
        EdgeType::CoOccurrence,
        crate::graph::derive::COOCCURRENCE_WEIGHT,
        0,
    ))
    .unwrap();
    let outcome = run(&mut g, aged_params());
    assert!(outcome.edges_removed.is_empty());
    assert!(g.edge(nid(100)).is_some());
}
