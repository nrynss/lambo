//! Canonization status transitions and their events.

use super::*;

#[test]
fn transition_applies_status_and_appends_event() {
    let (mut g, _, cid) = small_graph();
    let ev = CanonizationEvent {
        id: NodeId::new(),
        session_id: sid(),
        node_id: cid,
        from_status: crate::types::CanonizationStatus::None,
        to_status: crate::types::CanonizationStatus::Candidate,
        blast_radius: Some(3),
        last_demotion_time: None,
        occurred_at: ts(10),
    };
    g.apply_canonization_transition(ev.clone()).unwrap();
    let c = match g.node(cid).unwrap() {
        Node::Concept(c) => c,
        _ => panic!("concept"),
    };
    assert_eq!(
        c.canonization_status,
        crate::types::CanonizationStatus::Candidate
    );
    assert_eq!(c.blast_radius, Some(3));
    assert_eq!(g.canonization_events(), &[ev]);
    g.assert_invariants().unwrap();
}

#[test]
fn transition_from_status_mismatch_is_rejected() {
    // GRAPH-4: the audit trail must match reality — an event whose
    // from_status does not equal the concept's current status is fabricated
    // and must be rejected before anything is written.
    let (mut g, _, cid) = small_graph(); // concept status: None
    let ev = CanonizationEvent {
        id: NodeId::new(),
        session_id: sid(),
        node_id: cid,
        from_status: crate::types::CanonizationStatus::Venerable,
        to_status: crate::types::CanonizationStatus::Canonical,
        blast_radius: Some(5),
        last_demotion_time: None,
        occurred_at: ts(10),
    };
    let err = g.apply_canonization_transition(ev).unwrap_err().to_string();
    assert!(err.contains("current status"), "{err}");
    // Nothing changed: no status, no blast radius, no audit row, no mutation.
    match g.node(cid).unwrap() {
        Node::Concept(c) => {
            assert_eq!(
                c.canonization_status,
                crate::types::CanonizationStatus::None
            );
            assert_eq!(c.blast_radius, None);
        }
        _ => panic!("concept"),
    }
    assert!(g.canonization_events().is_empty());
    assert_eq!(g.log_len(), 3, "only the seed writes");
    g.assert_invariants().unwrap();
}

#[test]
fn illegal_transition_pairs_are_rejected() {
    // GRAPH-4: spec §10 state machine — stage skips, downgrades and
    // self-loops are not edges of the machine and must be rejected at the
    // write gate (the demo's canonization_events table only ever shows
    // legal transitions).
    let (mut g, _, cid) = small_graph();
    // Stage skip: None -> Canonical requires passing through the stages.
    let skip = CanonizationEvent {
        id: NodeId::new(),
        session_id: sid(),
        node_id: cid,
        from_status: crate::types::CanonizationStatus::None,
        to_status: crate::types::CanonizationStatus::Canonical,
        blast_radius: Some(5),
        last_demotion_time: None,
        occurred_at: ts(10),
    };
    let err = g
        .apply_canonization_transition(skip)
        .unwrap_err()
        .to_string();
    assert!(err.contains("illegal"), "{err}");
    // Self-loop: a transition must change status.
    let self_loop = CanonizationEvent {
        id: NodeId::new(),
        session_id: sid(),
        node_id: cid,
        from_status: crate::types::CanonizationStatus::None,
        to_status: crate::types::CanonizationStatus::None,
        blast_radius: None,
        last_demotion_time: None,
        occurred_at: ts(11),
    };
    assert!(g.apply_canonization_transition(self_loop).is_err());
    // Downgrade: demotion is only Canonical -> None.
    let promote = CanonizationEvent {
        id: NodeId::new(),
        session_id: sid(),
        node_id: cid,
        from_status: crate::types::CanonizationStatus::None,
        to_status: crate::types::CanonizationStatus::Venerable,
        blast_radius: None,
        last_demotion_time: None,
        occurred_at: ts(12),
    };
    g.apply_canonization_transition(promote).unwrap();
    let downgrade = CanonizationEvent {
        id: NodeId::new(),
        session_id: sid(),
        node_id: cid,
        from_status: crate::types::CanonizationStatus::Venerable,
        to_status: crate::types::CanonizationStatus::None,
        blast_radius: None,
        last_demotion_time: Some(ts(13)),
        occurred_at: ts(13),
    };
    let err = g
        .apply_canonization_transition(downgrade)
        .unwrap_err()
        .to_string();
    assert!(err.contains("illegal"), "{err}");
    // Only the single legal promotion was recorded.
    assert_eq!(g.canonization_events().len(), 1);
    g.assert_invariants().unwrap();
}

#[test]
fn legal_transitions_apply_and_demotion_carries_last_demotion_time() {
    // GRAPH-4 + COH-3: walk the full §10 path None -> Candidate ->
    // Venerable -> Canonical -> None. Demotion nulls blast_radius and
    // stamps last_demotion_time (spec §10); non-demotion events leave
    // last_demotion_time untouched; the carry survives a snapshot round-trip.
    let (mut g, _, cid) = small_graph();
    let promote = |from, to, blast, last_demotion, at| CanonizationEvent {
        id: NodeId::new(),
        session_id: sid(),
        node_id: cid,
        from_status: from,
        to_status: to,
        blast_radius: blast,
        last_demotion_time: last_demotion,
        occurred_at: ts(at),
    };
    g.apply_canonization_transition(promote(
        crate::types::CanonizationStatus::None,
        crate::types::CanonizationStatus::Candidate,
        Some(3),
        None,
        1,
    ))
    .unwrap();
    g.apply_canonization_transition(promote(
        crate::types::CanonizationStatus::Candidate,
        crate::types::CanonizationStatus::Venerable,
        Some(3),
        None,
        2,
    ))
    .unwrap();
    g.apply_canonization_transition(promote(
        crate::types::CanonizationStatus::Venerable,
        crate::types::CanonizationStatus::Canonical,
        Some(7),
        None,
        3,
    ))
    .unwrap();
    // Demotion: blast_radius nulled, last_demotion_time stamped.
    g.apply_canonization_transition(promote(
        crate::types::CanonizationStatus::Canonical,
        crate::types::CanonizationStatus::None,
        None,
        Some(ts(4)),
        4,
    ))
    .unwrap();
    match g.node(cid).unwrap() {
        Node::Concept(c) => {
            assert_eq!(
                c.canonization_status,
                crate::types::CanonizationStatus::None
            );
            assert_eq!(c.blast_radius, None);
            assert_eq!(c.last_demotion_time, Some(ts(4)));
        }
        _ => panic!("concept"),
    }
    // A later non-demotion promotion must NOT clobber the carry.
    g.apply_canonization_transition(promote(
        crate::types::CanonizationStatus::None,
        crate::types::CanonizationStatus::Candidate,
        Some(2),
        None,
        5,
    ))
    .unwrap();
    match g.node(cid).unwrap() {
        Node::Concept(c) => assert_eq!(c.last_demotion_time, Some(ts(4))),
        _ => panic!("concept"),
    }
    // The carry survives a snapshot round-trip (from_snapshot -> to_snapshot).
    let h = Graph::from_snapshot(g.snapshot()).unwrap();
    match h.node(cid).unwrap() {
        Node::Concept(c) => assert_eq!(c.last_demotion_time, Some(ts(4))),
        _ => panic!("concept"),
    }
    let demote_ev = h
        .canonization_events()
        .iter()
        .find(|e| e.to_status == crate::types::CanonizationStatus::None)
        .expect("demotion event recorded");
    assert_eq!(demote_ev.last_demotion_time, Some(ts(4)));
    assert_eq!(h.canonization_events().len(), 5);
    g.assert_invariants().unwrap();
}
