use super::*;
use crate::types::{
    AgentId, Concept, ConceptType, Edge, EdgeType, EmbeddingContract, GraphSnapshot, Interaction,
    NodeId, SessionId,
};
use chrono::{TimeZone, Utc};
use uuid::Uuid;

fn id(n: u64) -> NodeId {
    NodeId(Uuid::from_u64_pair(33, n))
}

pub(crate) fn fixture() -> (GraphSnapshot, ClassificationRules) {
    let session = SessionId::from("mixed");
    let at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let mut source = GraphSnapshot {
        session_id: session.clone(),
        embedding: Some(EmbeddingContract {
            kind: "fixture".into(),
            model: None,
            dim: 2,
        }),
        ..Default::default()
    };
    for (n, agent, prompt) in [
        (1, "alpha-build", "Work in /work/alpha on alpha feature"),
        (2, "beta-build", "Work in /work/beta on beta feature"),
        (
            3,
            "operator",
            "all projects follow operator-wide review conventions",
        ),
        (4, "unknown", "Unmapped machine maintenance"),
    ] {
        source.interactions.push(Interaction {
            id: id(n),
            session_id: session.clone(),
            agent_id: AgentId::from(agent),
            prompt_text: Some(prompt.into()),
            previous_id: (n > 1).then(|| id(n - 1)),
            created_at: at + chrono::Duration::minutes(n as i64 * 10),
            event_time: None,
        });
    }
    for (n, origin, ty, content) in [
        (11, 1, ConceptType::Logic, "alpha implementation is stable"),
        (
            12,
            2,
            ConceptType::Constraint,
            "all projects follow beta rules",
        ),
        (
            13,
            3,
            ConceptType::Logic,
            "Every repository follows the operator-wide review convention",
        ),
        (14, 4, ConceptType::Observation, "The task completed"),
        (15, 1, ConceptType::Resource, "README.md"),
    ] {
        let actor = source
            .interactions
            .iter()
            .find(|i| i.id == id(origin))
            .unwrap()
            .agent_id
            .clone();
        source.concepts.push(Concept {
            id: id(n),
            session_id: session.clone(),
            content: content.into(),
            canonical_key: content.into(),
            concept_type: ty,
            origin_interaction: id(origin),
            origin_agent: actor,
            created_at: at,
            access_count: 0,
            last_accessed: None,
            gc_survived: 0,
            canonization_status: crate::types::CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: None,
            embedding: Some(vec![0.5, 0.25]),
            human_confirmed: 0,
            embedding_source: None,
            chunk_group_id: None,
        });
    }
    let mut cross_origin = source.concepts[0].clone();
    cross_origin.id = id(17);
    cross_origin.content = "/work/beta beta context".into();
    cross_origin.canonical_key = cross_origin.content.clone();
    source.concepts.push(cross_origin);
    for (n, from, to) in [(21, 11, 15), (22, 12, 15)] {
        source.edges.push(Edge {
            id: id(n),
            session_id: session.clone(),
            source: id(from),
            target: id(to),
            edge_type: EdgeType::Dependency,
            weight: 1.0,
            reinforcements: 1,
            created_at: at,
            last_reinforced: at,
            event_time: None,
        });
    }
    for (n, from, to, kind) in [
        (23, 1, 2, EdgeType::Temporal),
        (24, 2, 3, EdgeType::Temporal),
        (25, 3, 4, EdgeType::Temporal),
        (31, 1, 11, EdgeType::Derives),
        (32, 2, 12, EdgeType::Derives),
        (33, 3, 13, EdgeType::Derives),
        (34, 4, 14, EdgeType::Derives),
        (35, 1, 15, EdgeType::Derives),
        (36, 1, 17, EdgeType::Derives),
        (37, 17, 11, EdgeType::Dependency),
    ] {
        let mut edge = source.edges[0].clone();
        edge.id = id(n);
        edge.source = id(from);
        edge.target = id(to);
        edge.edge_type = kind;
        source.edges.push(edge);
    }
    let rules = ClassificationRules {
        projects: vec![
            ProjectRule {
                session: "alpha".into(),
                folders: vec!["/work/alpha".into()],
                agent_id_patterns: vec!["alpha-*".into()],
                content_keywords: vec!["alpha".into()],
                exclude_from_general: false,
            },
            ProjectRule {
                session: "beta".into(),
                folders: vec!["/work/beta".into()],
                agent_id_patterns: vec!["beta-*".into()],
                content_keywords: vec!["beta".into(), "beta context".into()],
                exclude_from_general: true,
            },
        ],
        general_agent_patterns: vec!["operator".into()],
        ..Default::default()
    };
    (source, rules)
}

fn row(plan: &ClassificationPlan, n: u64) -> &Assignment {
    plan.assignments.iter().find(|a| a.id == id(n)).unwrap()
}

#[test]
fn synthetic_multi_project_plan_is_complete_conservative_and_reviewable() {
    let (source, rules) = fixture();
    let plan = classify(&source, &rules, &[]).unwrap();
    assert_eq!(plan.assignments.len(), 10);
    assert_eq!(row(&plan, 17).session, "beta");
    assert_eq!(row(&plan, 11).session, "alpha");
    assert_eq!(row(&plan, 12).session, "beta");
    assert_eq!(row(&plan, 13).session, "general");
    assert_eq!(row(&plan, 3).session, "general");
    assert_eq!(row(&plan, 14).session, "general-usage");
    assert_eq!(row(&plan, 15).copy_into, ["alpha", "beta"]);
    assert_eq!(row(&plan, 15).confidence, Confidence::Hub);
    assert!(plan.low_confidence.contains(&ItemRef {
        kind: ItemKind::Concept,
        id: id(14)
    }));
    validate_plan(&plan, &source).unwrap();
    // A public plan carries labels/ids, never source prompt/content/vectors.
    let json = serde_json::to_string(&plan).unwrap();
    assert!(!json.contains("implementation is stable"));
    assert!(!json.contains("[0.5,0.25]"));
}

#[test]
fn sorted_full_source_binding_detects_vectors_metadata_and_audit_changes() {
    let (source, rules) = fixture();
    let plan = classify(&source, &rules, &[]).unwrap();
    let mut reordered = source.clone();
    reordered.interactions.reverse();
    reordered.concepts.reverse();
    reordered.edges.reverse();
    assert_eq!(classify(&reordered, &rules, &[]).unwrap(), plan);
    let mut changed = source.clone();
    changed.concepts[0].embedding.as_mut().unwrap()[0] = 0.75;
    assert!(validate_plan(&plan, &changed).is_err());
    changed = source.clone();
    changed.root_goal = Some(serde_json::json!({"id": "other", "nested": {"b": 1, "a": 2}}));
    assert!(validate_plan(&plan, &changed).is_err());
    changed = source.clone();
    changed.embedding.as_mut().unwrap().model = Some("different".into());
    assert!(validate_plan(&plan, &changed).is_err());
    changed = source.clone();
    changed
        .canonization_events
        .push(crate::types::CanonizationEvent {
            id: id(99),
            session_id: source.session_id.clone(),
            node_id: id(11),
            from_status: crate::types::CanonizationStatus::None,
            to_status: crate::types::CanonizationStatus::Candidate,
            blast_radius: None,
            last_demotion_time: None,
            occurred_at: source.interactions[0].created_at,
        });
    assert!(validate_plan(&plan, &changed).is_err());
    changed = source.clone();
    changed.concepts[0].canonical_key = "identity changed".into();
    assert!(validate_plan(&plan, &changed).is_err());
}

#[test]
fn malformed_plans_overrides_and_rules_refuse_before_any_output() {
    let (source, rules) = fixture();
    let plan = classify(&source, &rules, &[]).unwrap();
    for mutate in [0, 1, 2, 3] {
        let mut p = plan.clone();
        match mutate {
            0 => p.version += 1,
            1 => {
                p.assignments.pop();
            }
            2 => p.assignments.push(p.assignments[0].clone()),
            _ => p.assignments[0].session = "foreign".into(),
        }
        assert!(validate_plan(&p, &source).is_err());
    }
    let correction = OperatorOverride {
        kind: ItemKind::Concept,
        id: id(11),
        session: "general-usage".into(),
        reason: "Operator reviewed this item".into(),
        from: Some("alpha".into()),
        copy_into: vec![],
    };
    let corrected = classify(&source, &rules, std::slice::from_ref(&correction)).unwrap();
    assert_eq!(row(&corrected, 11).session, "general-usage");
    assert_eq!(
        corrected.overrides.as_slice(),
        std::slice::from_ref(&correction)
    );
    assert!(classify(&source, &rules, &[correction.clone(), correction.clone()]).is_err());
    let mut bad = correction.clone();
    bad.id = id(999);
    assert!(classify(&source, &rules, &[bad]).is_err());
    let mut bad = correction.clone();
    bad.from = Some("wrong".into());
    assert!(classify(&source, &rules, &[bad]).is_err());
    let mut bad = correction;
    bad.id = id(12);
    bad.from = Some("beta".into());
    bad.session = "general".into();
    assert!(classify(&source, &rules, &[bad]).is_err());
    let mut bad = rules.clone();
    bad.projects.push(bad.projects[0].clone());
    assert!(bad.validate().is_err());
    bad = rules.clone();
    bad.fallback_session = "../escape".into();
    assert!(bad.validate().is_err());
    assert!(serde_json::from_str::<ClassificationRules>(r#"{"unknown":true}"#).is_err());
}

#[test]
fn general_rejects_references_and_identity_even_with_operator_signals() {
    let (source, rules) = fixture();
    for reference in [
        "all projects use src/main.rs",
        "all projects follow PR #27",
        "all projects release v1.2",
        "all projects use commit abc1234",
        "all projects work in branch work/topic",
        "all projects use alpha",
    ] {
        let mut changed = source.clone();
        changed.concepts[2].content = reference.into();
        assert_ne!(
            row(&classify(&changed, &rules, &[]).unwrap(), 13).session,
            "general",
            "{reference}"
        );
    }
    let mut changed = source;
    changed.concepts[2].concept_type = ConceptType::Entity;
    assert_ne!(
        row(&classify(&changed, &rules, &[]).unwrap(), 13).session,
        "general"
    );
}

#[test]
fn temporal_context_is_bounded_and_does_not_cascade_or_cross_the_clock() {
    let (mut source, mut rules) = fixture();
    // Actor alpha alone is ambiguous with beta; a high-confidence alpha
    // neighbour is only a signal, never three independent confirmations.
    rules.projects[1].agent_id_patterns.push("alpha-*".into());
    source.interactions[3].agent_id = AgentId::from("alpha-build");
    source.interactions[3].created_at =
        source.interactions[0].created_at + chrono::Duration::minutes(1);
    let p = classify(&source, &rules, &[]).unwrap();
    assert!(row(&p, 4).pass2);
    assert!(row(&p, 4).signals.iter().any(|s| s.contains("neighbours")));
    assert_eq!(row(&p, 4).confidence, Confidence::Low);
    source.interactions[3].event_time =
        Some(source.interactions[0].created_at + chrono::Duration::days(2));
    let p = classify(&source, &rules, &[]).unwrap();
    assert!(row(&p, 4).candidates.is_empty() || row(&p, 4).candidates.len() == 2);
}

#[test]
fn glob_and_folder_hints_are_component_bounded() {
    assert!(super::signals::glob("model-*?", "model-abc"));
    assert!(!super::signals::glob("model-*", "not-model-abc"));
    let (_, rules) = fixture();
    assert!(super::signals::content(&rules, "/work/alpha/src/main.rs")
        .1
        .contains(&"folder:alpha:0".into()));
    assert!(!super::signals::content(&rules, "/work/alpha-other/file")
        .1
        .contains(&"folder:alpha:0".into()));
    assert!(!super::signals::content(&rules, "/work/alphabeta/file")
        .1
        .contains(&"folder:alpha:0".into()));
}

#[test]
fn reviewed_labels_and_owner_override_shape_are_audited_and_complete() {
    let (source, rules) = fixture();
    let generated = classify(&source, &rules, &[]).unwrap();
    let mut labels: Vec<_> = generated
        .assignments
        .iter()
        .map(|a| ReviewedAssignment {
            kind: a.kind,
            id: a.id,
            project: a.session.clone(),
            confidence: a.confidence,
            signals: a.signals.clone(),
            pass2: a.pass2,
            copy_into: a.copy_into.clone(),
        })
        .collect();
    let correction: OperatorOverride = serde_json::from_value(serde_json::json!({"kind":"concept","id":id(11),"from":"alpha","to":"general-usage","reason":"Operator reviewed placement"})).unwrap();
    let imported =
        classify_reviewed(&source, &rules, &labels, std::slice::from_ref(&correction)).unwrap();
    assert_eq!(row(&imported, 11).session, "general-usage");
    assert!(imported.reviewed_assignments_sha256.is_some());
    assert_eq!(
        imported.overrides.as_slice(),
        std::slice::from_ref(&correction)
    );
    labels.reverse();
    assert_eq!(
        classify_reviewed(&source, &rules, &labels, std::slice::from_ref(&correction)).unwrap(),
        imported
    );
    labels.pop();
    assert!(classify_reviewed(&source, &rules, &labels, &[]).is_err());
    let mut bad = generated
        .assignments
        .iter()
        .map(|a| ReviewedAssignment {
            kind: a.kind,
            id: a.id,
            project: a.session.clone(),
            confidence: a.confidence,
            signals: a.signals.clone(),
            pass2: a.pass2,
            copy_into: a.copy_into.clone(),
        })
        .collect::<Vec<_>>();
    bad.iter_mut().find(|a| a.id == id(12)).unwrap().project = "general".into();
    assert!(
        classify_reviewed(&source, &rules, &bad, &[]).is_err(),
        "protected origin cannot leak through reviewed labels"
    );
}

#[test]
fn ordered_vectors_audit_and_metadata_arrays_remain_in_the_fingerprint() {
    let (mut source, rules) = fixture();
    for n in [51, 52] {
        source
            .canonization_events
            .push(crate::types::CanonizationEvent {
                id: id(n),
                session_id: source.session_id.clone(),
                node_id: id(11),
                from_status: crate::types::CanonizationStatus::None,
                to_status: crate::types::CanonizationStatus::Candidate,
                blast_radius: None,
                last_demotion_time: None,
                occurred_at: source.interactions[0].created_at,
            });
    }
    source.root_goal = Some(serde_json::json!({"ordered":[1,2], "map":{"a":1,"b":2}}));
    let plan = classify(&source, &rules, &[]).unwrap();
    let mut changed = source.clone();
    changed.concepts[0].embedding.as_mut().unwrap().reverse();
    assert!(validate_plan(&plan, &changed).is_err());
    changed = source.clone();
    changed.canonization_events.reverse();
    assert!(validate_plan(&plan, &changed).is_err());
    changed = source.clone();
    changed.root_goal.as_mut().unwrap()["ordered"] = serde_json::json!([2, 1]);
    assert!(validate_plan(&plan, &changed).is_err());
    changed = source.clone();
    changed.root_goal = Some(serde_json::json!({"map":{"b":2,"a":1}, "ordered":[1,2]}));
    validate_plan(&plan, &changed).unwrap();
}

#[test]
fn strict_second_pass_resolves_only_three_agreeing_signals_inside_two_hours() {
    let (mut source, rules) = fixture();
    source.interactions[3].agent_id = AgentId::from("alpha-build");
    source.interactions[3].prompt_text = Some("alpha".into());
    source.interactions[3].event_time =
        Some(source.interactions[0].created_at + chrono::Duration::minutes(1));
    let plan = classify(&source, &rules, &[]).unwrap();
    assert_eq!(row(&plan, 4).session, "alpha");
    assert_eq!(row(&plan, 4).confidence, Confidence::Medium);
    assert!(row(&plan, 4)
        .signals
        .iter()
        .any(|s| s == "pass2:three-independent-signals-agree"));
    source.interactions[3].event_time =
        Some(source.interactions[0].created_at + chrono::Duration::days(2));
    let plan = classify(&source, &rules, &[]).unwrap();
    assert_eq!(row(&plan, 4).confidence, Confidence::Low);
}

#[test]
fn reviewed_general_cannot_erase_protected_source_provenance() {
    for mode in 0..3 {
        let (mut source, rules) = fixture();
        if mode == 0 {
            source.concepts[2].origin_agent = AgentId::from("beta-build");
        } else if mode == 1 {
            source.interactions[2].prompt_text = Some("all projects in /work/beta".into());
        } else {
            source.concepts[2].content = "all projects beta".into();
        }
        let generated = classify(&source, &rules, &[]).unwrap();
        let mut labels: Vec<_> = generated
            .assignments
            .iter()
            .map(|a| ReviewedAssignment {
                kind: a.kind,
                id: a.id,
                project: a.session.clone(),
                confidence: a.confidence,
                signals: a.signals.clone(),
                pass2: a.pass2,
                copy_into: a.copy_into.clone(),
            })
            .collect();
        for a in &mut labels {
            if a.id == id(3) || a.id == id(13) {
                a.project = "general".into();
            }
        }
        let audit = OperatorOverride {
            kind: ItemKind::Concept,
            id: id(13),
            session: "general".into(),
            reason: "Operator correction".into(),
            from: None,
            copy_into: vec![],
        };
        assert!(classify_reviewed(&source, &rules, &labels, &[audit]).is_err());
    }
    let (source, rules) = fixture();
    let generated = classify(&source, &rules, &[]).unwrap();
    let mut labels: Vec<_> = generated
        .assignments
        .iter()
        .map(|a| ReviewedAssignment {
            kind: a.kind,
            id: a.id,
            project: a.session.clone(),
            confidence: a.confidence,
            signals: a.signals.clone(),
            pass2: a.pass2,
            copy_into: a.copy_into.clone(),
        })
        .collect();
    labels.iter_mut().find(|a| a.id == id(2)).unwrap().project = "general".into();
    assert!(classify_reviewed(&source, &rules, &labels, &[]).is_err());
    labels.iter_mut().find(|a| a.id == id(2)).unwrap().project = "beta".into();
    labels.iter_mut().find(|a| a.id == id(4)).unwrap().project = "general".into();
    assert!(classify_reviewed(&source, &rules, &labels, &[]).is_err());
}

#[test]
fn nonpath_resource_and_entity_hubs_use_fixed_incident_assignments() {
    for ty in [ConceptType::Resource, ConceptType::Entity] {
        let (mut source, rules) = fixture();
        source.concepts[4].concept_type = ty;
        source.concepts[4].content = "service identity".into();
        let plan = classify(&source, &rules, &[]).unwrap();
        assert_eq!(row(&plan, 15).copy_into, vec!["alpha", "beta"]);
        let override_ = OperatorOverride {
            kind: ItemKind::Concept,
            id: id(15),
            session: "alpha".into(),
            reason: "Reviewed shared identity".into(),
            from: None,
            copy_into: vec!["alpha".into(), "beta".into()],
        };
        let plan = classify(&source, &rules, &[override_]).unwrap();
        assert_eq!(row(&plan, 15).copy_into, vec!["alpha", "beta"]);
    }
}
