use super::*;
use crate::test_util::citations::{concept, graph};
use crate::types::{CanonizationStatus, Edge, GraphSnapshot};

fn related(g: &mut Graph, source: NodeId, target: NodeId, kind: EdgeType) -> Edge {
    let now = g.logical_now();
    let edge = Edge {
        id: NodeId::new(),
        session_id: g.session_id().clone(),
        source,
        target,
        edge_type: kind,
        weight: 0.7,
        reinforcements: 3,
        created_at: now,
        event_time: Some(now - chrono::Duration::days(2)),
        last_reinforced: now,
    };
    g.upsert_edge(edge.clone()).unwrap();
    edge
}

#[test]
fn names_normalize_survive_restart_and_remain_reserved_after_collection() {
    let (mut g, i) = graph("name-restart");
    let target = concept(
        &mut g,
        i,
        "the writer keeps its lease",
        ConceptType::Constraint,
    );
    assert!(g.declare_name("Caf\u{e9} lease rule", target).unwrap());
    assert!(!g.declare_name("RULE cafe\u{301} LEASE", target).unwrap());
    let mut g = Graph::from_snapshot(g.snapshot()).unwrap();
    assert_eq!(g.name_target("cafe\u{301} lease rule"), Some(target));
    assert_eq!(
        crate::graph::canonical::canonicalize("Caf\u{e9} lease rule", &g).unwrap(),
        crate::graph::canonical::CanonicalizeResult::Matched {
            key: crate::graph::canonical::canonical_key("the writer keeps its lease", |_| None),
            node: target
        }
    );
    g.remove_node(target).unwrap();
    let mut g = Graph::from_snapshot(g.snapshot()).unwrap();
    let other = concept(&mut g, i, "some other constraint", ConceptType::Constraint);
    assert!(g.declare_name("Caf\u{e9} lease rule", other).is_err());
    assert!(crate::graph::canonical::canonicalize("Caf\u{e9} lease rule", &g).is_err());
    assert_eq!(g.name_target("Caf\u{e9} lease rule"), Some(target));
}

#[test]
fn names_cannot_shadow_concepts_observations_or_identifiers() {
    let (mut g, i) = graph("name-namespace");
    let target = concept(&mut g, i, "a durable ruling", ConceptType::Logic);
    concept(&mut g, i, "existing name", ConceptType::Observation);
    assert!(g.declare_name("Existing Name", target).is_err());
    assert!(g.declare_name("the and of", target).is_err());
    assert!(g.declare_name("deadbeef", target).is_err());
    assert!(g.declare_name(&target.to_string(), target).is_err());
    assert!(g.declare_name("invisible\u{202e} name", target).is_err());
}

#[test]
fn rollback_and_panic_undo_name_map_epoch_and_log() {
    let (mut g, i) = graph("name-undo");
    let target = concept(&mut g, i, "a durable ruling", ConceptType::Logic);
    let before = g.snapshot();
    let len = g.log_len();
    let error: Result<(), _> = g.transaction(|g| {
        g.declare_name("rule name", target)?;
        Err(invariant("later refusal"))
    });
    assert!(error.is_err());
    assert_eq!(g.snapshot(), before);
    assert_eq!(g.log_len(), len);
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _: Result<(), LamboError> = g.transaction(|g| {
            g.declare_name("rule name", target)?;
            panic!("after binding")
        });
    }));
    assert!(panicked.is_err());
    assert_eq!(g.snapshot(), before);
    assert_eq!(g.log_len(), len);
}

#[test]
fn snapshot_refuses_duplicate_normalized_names_and_cross_session_records() {
    let (mut g, i) = graph("name-snapshot");
    let target = concept(&mut g, i, "a durable ruling", ConceptType::Logic);
    g.declare_name("writer rule", target).unwrap();
    let mut duplicate = g.snapshot();
    let mut n = duplicate.names[0].clone();
    n.name = "RULE WRITER".into();
    duplicate.names.push(n);
    assert!(Graph::from_snapshot(duplicate).is_err());
    let mut wrong = g.snapshot();
    wrong.names[0].session_id = "other".into();
    assert!(Graph::from_snapshot(wrong).is_err());
}

#[test]
fn migration_imports_surviving_unicode_spellings_without_new_concepts() {
    let (mut g, i) = graph("name-import");
    let target = concept(&mut g, i, "a durable ruling", ConceptType::Logic);
    g.declare_synonym("Caf\u{e9} rule", "a durable ruling");
    g.declare_synonym("RULE cafe\u{301}", "a durable ruling");
    let count = g.concepts().count();
    let report = upgrade(&mut g, &[]).unwrap();
    assert_eq!(report.names_migrated, 1);
    assert_eq!(g.concepts().count(), count);
    assert_eq!(g.name_target("cafe\u{301} rule"), Some(target));
    let before = g.snapshot();
    let report = upgrade(&mut g, &[]).unwrap();
    assert_eq!(report.names_migrated, 0);
    assert_eq!(g.snapshot(), before);
}

#[test]
fn ambiguous_legacy_spellings_fail_atomically() {
    let (mut g, i) = graph("name-import-conflict");
    concept(&mut g, i, "first ruling", ConceptType::Logic);
    concept(&mut g, i, "second ruling", ConceptType::Logic);
    g.declare_synonym("lease rule", "first ruling");
    g.declare_synonym("RULE LEASE", "second ruling");
    let before = g.snapshot();
    assert!(upgrade(&mut g, &[]).is_err());
    assert_eq!(g.snapshot(), before);
}

#[test]
fn explicit_assignments_resolve_legacy_missing_and_conflicting_targets() {
    let (mut g, i) = graph("name-legacy-reviewed-choice");
    let target = concept(
        &mut g,
        i,
        "the reviewed existing ruling",
        ConceptType::Logic,
    );
    g.declare_synonym("lease rule", "lost target");
    g.declare_synonym("RULE LEASE", "another lost target");
    assert!(upgrade(&mut g, &[]).is_err());
    g.declare_name("Lease rule", target).unwrap();
    let before = g.snapshot();
    assert_eq!(upgrade(&mut g, &[]).unwrap().names_migrated, 0);
    assert_eq!(g.snapshot(), before);
    assert_eq!(g.name_target("rule lease"), Some(target));
}

#[test]
fn legacy_target_keys_are_not_assumed_to_normalize_idempotently() {
    let (mut g, i) = graph("name-legacy-key");
    let target = concept(&mut g, i, "a writer releases its lease", ConceptType::Logic);
    let key = match g.node(target).unwrap() {
        Node::Concept(c) => c.canonical_key.clone(),
        _ => unreachable!(),
    };
    g.declare_synonym("existing lease name", &key);
    upgrade(&mut g, &[]).unwrap();
    assert_eq!(g.name_target("existing lease name"), Some(target));
}

fn repair_graph() -> (Graph, NodeId, NodeId, NodeId, Edge) {
    let (mut g, i) = graph("name-repair");
    let alias = concept(&mut g, i, "writer lease", ConceptType::Entity);
    let target = concept(
        &mut g,
        i,
        "the writer keeps its lease until released",
        ConceptType::Logic,
    );
    let citer = concept(
        &mut g,
        i,
        "implemented fenced writes",
        ConceptType::Resource,
    );
    let original = related(&mut g, citer, alias, EdgeType::Dependency);
    let mut snap = g.snapshot();
    snap.concepts
        .iter_mut()
        .find(|c| c.id == alias)
        .unwrap()
        .canonization_status = CanonizationStatus::Venerable;
    (
        Graph::from_snapshot(snap).unwrap(),
        alias,
        target,
        citer,
        original,
    )
}

#[test]
fn reviewed_repair_preserves_history_moves_support_and_releases_namespace() {
    let (mut g, alias, target, citer, original) = repair_graph();
    let report = upgrade(&mut g, &[(alias, target)]).unwrap();
    assert_eq!((report.aliases_retired, report.citations_moved), (1, 1));
    assert!(g.node(alias).is_none());
    assert_eq!(g.name_target("WRITER LEASE"), Some(target));
    let moved = g.edge_between(citer, target, EdgeType::Dependency).unwrap();
    assert_eq!(
        (
            moved.created_at,
            moved.event_time,
            moved.last_reinforced,
            moved.reinforcements,
            moved.weight
        ),
        (
            original.created_at,
            original.event_time,
            original.last_reinforced,
            original.reinforcements,
            original.weight
        )
    );
    let archived = g.retired_alias(alias).unwrap();
    assert_eq!(
        archived.concept.canonization_status,
        CanonizationStatus::Venerable
    );
    assert!(archived.edges.contains(&original));
    assert!(archived
        .edges
        .iter()
        .any(|e| e.edge_type == EdgeType::Derives));
    assert!(matches!(
        crate::surface::resolve::resolve_reference(&g, &alias.to_string()),
        crate::surface::resolve::Resolved::Retired { .. }
    ));
    g.assert_invariants().unwrap();
    let mut g = Graph::from_snapshot(g.snapshot()).unwrap();
    let before = g.snapshot();
    assert_eq!(
        upgrade(&mut g, &[(alias, target)]).unwrap().already_applied,
        1
    );
    assert_eq!(g.snapshot(), before);
    let archived = archived_concept(&before, alias);
    let origin = archived.origin_interaction;
    assert!(g.insert_concept(archived, origin).is_err());
}

fn archived_concept(s: &GraphSnapshot, id: NodeId) -> crate::types::Concept {
    s.retired_aliases
        .iter()
        .find(|a| a.concept.id == id)
        .unwrap()
        .concept
        .clone()
}

#[test]
fn repair_deduplicates_existing_citations_and_refuses_substantive_relations() {
    let (mut g, alias, target, citer, _) = repair_graph();
    related(&mut g, citer, target, EdgeType::Dependency);
    upgrade(&mut g, &[(alias, target)]).unwrap();
    assert_eq!(
        g.edges()
            .filter(|e| e.source == citer
                && e.target == target
                && e.edge_type == EdgeType::Dependency)
            .count(),
        1
    );
    let (mut g, alias, target, citer, _) = repair_graph();
    related(&mut g, alias, citer, EdgeType::Hierarchical);
    let before = g.snapshot();
    assert!(upgrade(&mut g, &[(alias, target)]).is_err());
    assert_eq!(g.snapshot(), before);
}

#[test]
fn repair_refuses_cycle_and_rolls_back_all_prior_changes() {
    let (mut g, alias, target, citer, _) = repair_graph();
    related(&mut g, target, citer, EdgeType::Hierarchical);
    let before = g.snapshot();
    assert!(upgrade(&mut g, &[(alias, target)]).is_err());
    assert_eq!(g.snapshot(), before);
    let (mut g, alias, target, _, _) = repair_graph();
    g.declare_synonym("unrecoverable name", "missing target");
    let before = g.snapshot();
    assert!(upgrade(&mut g, &[(alias, target)]).is_err());
    assert_eq!(g.snapshot(), before);
}

#[test]
fn old_mixed_action_pins_replay_text_but_still_enforce_id_identity() {
    use crate::graph::action::{
        legacy_replay_pins, record_action_pinned, Action, ActionEmbeddings,
    };
    let (mut g, i) = graph("legacy-action-name-upgrade");
    let id_target = concept(&mut g, i, "the id dependency", ConceptType::Logic);
    let text_target = concept(&mut g, i, "the text dependency", ConceptType::Logic);
    let reference = id_target.to_string();
    let deps = [reference.as_str(), "the text dependency"];
    let action = Action {
        repo_scope: None,
        event_time: None,
        action: "completed legacy work",
        produces: &[],
        modifies: &[],
        depends_on: &deps,
    };
    // This is the actual on-disk meaning of a mixed pre-#146 action:
    // None text meant unpinned, while the id pin was authoritative.
    let old_pins = [Some(id_target), None];
    let effective = legacy_replay_pins(&g, &action, &old_pins).unwrap();
    let result = record_action_pinned(
        &mut g,
        i,
        &crate::types::AgentId::new("legacy-agent"),
        &action,
        &ActionEmbeddings::new(),
        &effective,
    )
    .unwrap();
    assert!(g
        .edge_between(result.action_node, text_target, EdgeType::Dependency)
        .is_some());
    assert!(g
        .edge_between(result.action_node, id_target, EdgeType::Dependency)
        .is_some());
    let before = g.snapshot();
    let stale = legacy_replay_pins(&g, &action, &[Some(NodeId::new()), None]).unwrap();
    assert!(record_action_pinned(
        &mut g,
        i,
        &crate::types::AgentId::new("legacy-agent"),
        &action,
        &ActionEmbeddings::new(),
        &stale
    )
    .is_err());
    assert_eq!(g.snapshot(), before);
}

#[test]
fn a_name_created_after_ack_cannot_redirect_new_action_text() {
    use crate::graph::action::{record_action_pinned, validate_pinned, Action, ActionEmbeddings};
    let (mut g, i) = graph("new-action-name-race");
    let target = concept(
        &mut g,
        i,
        "the actual ownership constraint",
        ConceptType::Logic,
    );
    let deps = ["ownership name"];
    let action = Action {
        repo_scope: None,
        event_time: None,
        action: "queued ownership work",
        produces: &[],
        modifies: &[],
        depends_on: &deps,
    };
    let pins = validate_pinned(&g, &action).unwrap();
    assert_eq!(pins, vec![None]);
    g.declare_name("ownership name", target).unwrap();
    let before = g.snapshot();
    assert!(record_action_pinned(
        &mut g,
        i,
        &crate::types::AgentId::new("new-agent"),
        &action,
        &ActionEmbeddings::new(),
        &pins
    )
    .is_err());
    assert_eq!(g.snapshot(), before);
}
