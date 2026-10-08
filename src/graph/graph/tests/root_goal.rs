//! The root goal: promotion to venerable and its stored shapes.

use super::*;

// ------------------------------------------------------------------
// set_root_goal — spec §9: root goal nodes are automatically Venerable
// ------------------------------------------------------------------

/// Fresh graph with a goal concept ("launch the product") + an unrelated
/// concept, both derived from one interaction.
fn goal_graph() -> (Graph, NodeId, NodeId) {
    let mut g = Graph::new(sid());
    let i = interaction(1, None, 0);
    let iid = i.id;
    g.insert_interaction(i).unwrap();
    let goal = concept(1, iid, "launch the product");
    let goal_id = goal.id;
    g.insert_concept(goal, iid).unwrap();
    let other = concept(2, iid, "unrelated concept");
    let other_id = other.id;
    g.insert_concept(other, iid).unwrap();
    (g, goal_id, other_id)
}

fn status_of(g: &Graph, id: NodeId) -> crate::types::CanonizationStatus {
    match g.node(id).unwrap() {
        Node::Concept(c) => c.canonization_status,
        _ => panic!("concept"),
    }
}

#[test]
fn set_root_goal_promotes_matching_concept_to_venerable() {
    let (mut g, goal_id, other_id) = goal_graph();

    g.set_root_goal(Some(serde_json::json!("launch the product")));

    assert_eq!(
        g.root_goal(),
        Some(&serde_json::json!("launch the product"))
    );
    assert_eq!(
        status_of(&g, goal_id),
        crate::types::CanonizationStatus::Venerable,
        "goal concept auto-promoted to Venerable"
    );
    assert_eq!(
        status_of(&g, other_id),
        crate::types::CanonizationStatus::None,
        "non-goal concept untouched"
    );
    // Audited through the T2.1 mutation path: one transition event and one
    // `Mutation::CanonizationTransition` in the write-behind log.
    assert_eq!(g.canonization_events().len(), 1);
    let ev = &g.canonization_events()[0];
    assert_eq!(ev.node_id, goal_id);
    assert_eq!(ev.from_status, crate::types::CanonizationStatus::None);
    assert_eq!(ev.to_status, crate::types::CanonizationStatus::Venerable);
    let batch = g.drain_log();
    assert_eq!(
        batch
            .mutations
            .iter()
            .filter(|m| matches!(m, Mutation::CanonizationTransition { .. }))
            .count(),
        1
    );
}

/// ALGO-6: spec §6.1's own `root_goal` example is a **list**. A string-only
/// reading stored the array but named no concept, silently disabling drift
/// detection, auto-`Venerable` promotion and GC's root-goal exclusion.
///
/// ALGO-12: **every** match is promoted, id-ascending, so the outcome does
/// not depend on `HashMap` iteration order.
#[test]
fn set_root_goal_accepts_an_array_and_promotes_every_match() {
    let (mut g, goal_id, other_id) = goal_graph();
    g.set_root_goal(Some(serde_json::json!([
        "launch the product",
        "unrelated concept"
    ])));

    assert_eq!(
        g.root_goal_texts(),
        vec![
            "launch the product".to_string(),
            "unrelated concept".to_string()
        ],
        "both names are read out of the array, sorted"
    );
    for id in [goal_id, other_id] {
        assert_eq!(
            status_of(&g, id),
            crate::types::CanonizationStatus::Venerable,
            "every named concept is auto-promoted, not just the first match"
        );
    }
    // Audited id-ascending, so the event order is deterministic under
    // multiple matches.
    let events = g.canonization_events();
    assert_eq!(events.len(), 2);
    assert!(events[0].node_id.0 < events[1].node_id.0);

    // ALGO-12: `occurred_at` is logical time — the session's newest
    // interaction stamp — not `Utc::now()`, so the audit trail stays
    // monotonic against the rows around it.
    let logical = g.logical_now();
    assert_eq!(logical, ts(0));
    assert!(
        events.iter().all(|e| e.occurred_at == logical),
        "occurred_at must come from logical time: {events:?}"
    );
}

/// ALGO-6: an object goal keeps working, and an unrecognised shape is
/// stored but names nothing (spec §6.1 types `root_goal` as free-form JSON).
#[test]
fn root_goal_texts_reads_every_supported_shape() {
    assert!(root_goal_texts(None).is_empty());
    assert_eq!(
        root_goal_texts(Some(&serde_json::json!("one"))),
        vec!["one".to_string()]
    );
    assert_eq!(
        root_goal_texts(Some(&serde_json::json!(["b", "a", "a"]))),
        vec!["a".to_string(), "b".to_string()],
        "sorted and deduplicated for determinism"
    );
    assert_eq!(
        root_goal_texts(Some(&serde_json::json!({"content": "c", "key": "k"}))),
        vec!["c".to_string(), "k".to_string()]
    );
    assert_eq!(
        root_goal_texts(Some(&serde_json::json!(["ok", 7, null]))),
        vec!["ok".to_string()],
        "non-string elements are ignored, not fatal"
    );
    assert!(root_goal_texts(Some(&serde_json::json!(42))).is_empty());
}

/// XP-8: the root goal is durable. Before `Mutation::SetRootGoal` a reload
/// replayed an empty goal, so drift detection silently stopped and GC's
/// root-goal exclusion emptied. The mutation also bumps the epoch, so T5.4's
/// recall cache cannot serve results computed against the old goal.
#[test]
fn set_root_goal_emits_a_mutation_and_bumps_the_epoch() {
    let (mut g, _, _) = goal_graph();
    g.drain_log();
    let epoch_before = g.epoch();

    g.set_root_goal(Some(serde_json::json!("launch the product")));
    assert!(g.epoch() > epoch_before, "a goal change bumps the epoch");
    let batch = g.drain_log();
    let goals: Vec<&Mutation> = batch
        .mutations
        .iter()
        .filter(|m| matches!(m, Mutation::SetRootGoal { .. }))
        .collect();
    assert_eq!(goals.len(), 1, "one SetRootGoal: {:?}", batch.mutations);
    match goals[0] {
        Mutation::SetRootGoal { session_id, goal } => {
            assert_eq!(*session_id, sid());
            assert_eq!(
                goal.as_ref(),
                Some(&serde_json::json!("launch the product"))
            );
        }
        other => panic!("unexpected {other:?}"),
    }

    // Re-setting the same goal is a no-op: no mutation, no epoch bump.
    let epoch_after = g.epoch();
    g.set_root_goal(Some(serde_json::json!("launch the product")));
    assert_eq!(g.epoch(), epoch_after, "an unchanged goal writes nothing");
    assert!(g.drain_log().is_empty());

    // Clearing emits the clear so a reload does not resurrect the old goal.
    g.set_root_goal(None);
    let batch = g.drain_log();
    assert!(batch
        .mutations
        .iter()
        .any(|m| matches!(m, Mutation::SetRootGoal { goal: None, .. })));
}

#[test]
fn set_root_goal_is_idempotent_for_venerable_goal() {
    let (mut g, goal_id, _) = goal_graph();
    g.set_root_goal(Some(serde_json::json!("launch the product")));
    g.set_root_goal(Some(serde_json::json!("launch the product")));

    assert_eq!(
        status_of(&g, goal_id),
        crate::types::CanonizationStatus::Venerable
    );
    // The §10 state machine has no Venerable -> Venerable edge: the second
    // call must not attempt a self-loop transition.
    assert_eq!(g.canonization_events().len(), 1);
}

#[test]
fn set_root_goal_leaves_canonical_goal_untouched() {
    let (mut g, goal_id, _) = goal_graph();
    g.set_root_goal(Some(serde_json::json!("launch the product")));
    // Earn Canonical via the mutation path (Venerable -> Canonical).
    let promote = |to, at| CanonizationEvent {
        id: NodeId::new(),
        session_id: sid(),
        node_id: goal_id,
        from_status: crate::types::CanonizationStatus::Venerable,
        to_status: to,
        blast_radius: Some(3),
        last_demotion_time: None,
        occurred_at: ts(at),
    };
    g.apply_canonization_transition(promote(crate::types::CanonizationStatus::Canonical, 1))
        .unwrap();
    let events_before = g.canonization_events().len();

    g.set_root_goal(Some(serde_json::json!("launch the product")));

    assert_eq!(
        status_of(&g, goal_id),
        crate::types::CanonizationStatus::Canonical,
        "a Canonical root goal must not be downgraded (no Canonical -> Venerable edge)"
    );
    assert_eq!(g.canonization_events().len(), events_before);
}

#[test]
fn clearing_root_goal_never_demotes() {
    let (mut g, goal_id, _) = goal_graph();
    g.set_root_goal(Some(serde_json::json!("launch the product")));

    g.set_root_goal(None);

    assert_eq!(g.root_goal(), None);
    assert_eq!(
        status_of(&g, goal_id),
        crate::types::CanonizationStatus::Venerable,
        "clearing the goal stores the clear but never demotes (demotion is T6.4's)"
    );
    assert_eq!(g.canonization_events().len(), 1);
}

#[test]
fn set_root_goal_matches_canonical_key_when_content_differs() {
    let mut g = Graph::new(sid());
    let i = interaction(1, None, 0);
    let iid = i.id;
    g.insert_interaction(i).unwrap();
    let mut c = concept(1, iid, "the product launch");
    c.canonical_key = "launch product".into();
    let goal_id = c.id;
    g.insert_concept(c, iid).unwrap();

    g.set_root_goal(Some(serde_json::json!("launch product")));

    assert_eq!(
        status_of(&g, goal_id),
        crate::types::CanonizationStatus::Venerable,
        "goal matching by canonical_key promotes"
    );
}

#[test]
fn set_root_goal_promotes_candidate_goal_to_venerable() {
    let (mut g, goal_id, _) = goal_graph();
    let promote = |from, to, at| CanonizationEvent {
        id: NodeId::new(),
        session_id: sid(),
        node_id: goal_id,
        from_status: from,
        to_status: to,
        blast_radius: Some(2),
        last_demotion_time: None,
        occurred_at: ts(at),
    };
    g.apply_canonization_transition(promote(
        crate::types::CanonizationStatus::None,
        crate::types::CanonizationStatus::Candidate,
        1,
    ))
    .unwrap();

    g.set_root_goal(Some(serde_json::json!("launch the product")));

    assert_eq!(
        status_of(&g, goal_id),
        crate::types::CanonizationStatus::Venerable,
        "Candidate -> Venerable is a legal §10 edge (Stage 2 promotion)"
    );
    assert_eq!(g.canonization_events().len(), 2);
}

#[test]
fn structured_root_goal_is_stored_without_promotion() {
    let (mut g, goal_id, _) = goal_graph();
    let structured = serde_json::json!({ "goal": "launch the product" });

    g.set_root_goal(Some(structured.clone()));

    assert_eq!(g.root_goal(), Some(&structured), "structured goal stored");
    assert_eq!(
        status_of(&g, goal_id),
        crate::types::CanonizationStatus::None,
        "non-string goal names no concept -> no promotion"
    );
    assert!(g.canonization_events().is_empty());
}
