//! Synthetic alpha/beta coverage for the offline split (#33 D2). No rig data.

use chrono::{DateTime, Duration, TimeZone, Utc};
use uuid::Uuid;

use super::*;
use crate::types::{
    AgentId, CanonizationEvent, CanonizationStatus, Concept, ConceptType, Edge, EdgeType,
    EmbeddingContract, GcMark, GraphSnapshot, Interaction, NodeId, PinnedCitations,
    PinnedReference, Reservation, SessionId, Synonym, WriteIntent, WriteIntentOutcome,
    WriteIntentPayload,
};

pub(crate) fn id(n: u64) -> NodeId {
    NodeId(Uuid::from_u64_pair(0xd2, n))
}

fn at(minutes: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 3, 1, 9, 0, 0).unwrap() + Duration::minutes(minutes)
}

/// Interaction primaries: 1 alpha, 2 beta, 3 alpha, 4 alpha, 5 general,
/// 6 alpha, 7 beta, 8 beta.
const INTERACTIONS: [(u64, &str, &str, &str); 8] = [
    (1, "alpha-build", "alpha cache work", "alpha"),
    (2, "beta-build", "beta parser work", "beta"),
    (3, "alpha-build", "alpha planning for beta", "alpha"),
    (4, "alpha-build", "alpha follow-up", "alpha"),
    (
        5,
        "operator",
        "all projects follow operator-wide review conventions",
        "general",
    ),
    (6, "alpha-build", "alpha tidy", "alpha"),
    (7, "beta-build", "beta results", "beta"),
    (8, "beta-build", "beta wrap", "beta"),
];

/// `(id, type, content, origin, session, copy_into)`.
type ConceptRow = (
    u64,
    ConceptType,
    &'static str,
    u64,
    &'static str,
    &'static [&'static str],
);

const CONCEPTS: [ConceptRow; 11] = [
    (
        11,
        ConceptType::Logic,
        "alpha cache is warm",
        1,
        "alpha",
        &[],
    ),
    (
        12,
        ConceptType::Logic,
        "beta parser is strict",
        2,
        "beta",
        &[],
    ),
    (
        13,
        ConceptType::Resource,
        "README.md",
        1,
        "alpha",
        &["alpha", "beta"],
    ),
    (
        14,
        ConceptType::Entity,
        "shared-service",
        3,
        "alpha",
        &["alpha"],
    ),
    (
        15,
        ConceptType::Logic,
        "beta waits for alpha output",
        3,
        "beta",
        &[],
    ),
    (
        16,
        ConceptType::Constraint,
        "beta deadline holds",
        3,
        "beta",
        &[],
    ),
    (
        17,
        ConceptType::Constraint,
        "all projects follow operator-wide review conventions",
        5,
        "general",
        &[],
    ),
    (18, ConceptType::Observation, "alpha note", 6, "alpha", &[]),
    (
        19,
        ConceptType::Logic,
        "beta result is green",
        7,
        "beta",
        &[],
    ),
    (
        20,
        ConceptType::Resource,
        "Cargo.toml",
        2,
        "beta",
        &["alpha", "beta"],
    ),
    (21, ConceptType::Logic, "beta wrap is done", 8, "beta", &[]),
];

/// `(id, source, target, type)` beyond the Temporal chain (ids 102..=108).
const EDGES: [(u64, u64, u64, EdgeType); 19] = [
    (201, 1, 11, EdgeType::Derives),
    (202, 2, 12, EdgeType::Derives),
    (203, 1, 13, EdgeType::Derives),
    (204, 3, 14, EdgeType::Derives),
    (205, 3, 15, EdgeType::Derives),
    (206, 3, 16, EdgeType::Derives),
    (207, 5, 17, EdgeType::Derives),
    (208, 6, 18, EdgeType::Derives),
    (209, 7, 19, EdgeType::Derives),
    (210, 2, 20, EdgeType::Derives),
    (211, 8, 21, EdgeType::Derives),
    (212, 6, 11, EdgeType::Derives),
    (213, 7, 11, EdgeType::Derives),
    (301, 11, 13, EdgeType::Dependency),
    (302, 12, 13, EdgeType::Dependency),
    (303, 13, 20, EdgeType::CoOccurrence),
    (304, 11, 12, EdgeType::Causal),
    (305, 15, 17, EdgeType::Dependency),
    (306, 19, 12, EdgeType::Semantic),
];

pub(crate) fn source() -> GraphSnapshot {
    let session = SessionId::from("mixed");
    let mut s = GraphSnapshot {
        session_id: session.clone(),
        root_goal: Some(serde_json::json!({"goal": "mixed projects"})),
        created_at: Some(at(0)),
        closed_at: Some(at(600)),
        embedding: Some(EmbeddingContract {
            kind: "fixture".into(),
            model: None,
            dim: 2,
        }),
        mutation_epoch: 42,
        gc_mark: GcMark {
            last_gc_epoch: 40,
            last_gc_at: Some(at(300)),
            last_gc_at_reset: false,
        },
        ..Default::default()
    };
    for (n, agent, prompt, _) in INTERACTIONS {
        s.interactions.push(Interaction {
            id: id(n),
            session_id: session.clone(),
            agent_id: AgentId::from(agent),
            prompt_text: Some(prompt.into()),
            previous_id: (n > 1).then(|| id(n - 1)),
            // 5 and 6 share an instant; 4 is a historical ingest whose event
            // time is earlier than 3's.
            created_at: at(n.min(5) as i64 + n.saturating_sub(6) as i64),
            event_time: (n == 4).then(|| at(-60)),
        });
    }
    for (n, ty, content, origin, _, _) in CONCEPTS {
        s.concepts.push(Concept {
            id: id(n),
            session_id: session.clone(),
            content: content.into(),
            canonical_key: content.to_lowercase(),
            concept_type: ty,
            origin_interaction: id(origin),
            origin_agent: AgentId::from(INTERACTIONS[origin as usize - 1].1),
            created_at: at(origin as i64),
            access_count: n as i32,
            last_accessed: Some(at(100)),
            gc_survived: 3,
            human_confirmed: i32::from(n == 13),
            canonization_status: if n == 13 {
                CanonizationStatus::Candidate
            } else {
                CanonizationStatus::None
            },
            blast_radius: (n == 13).then_some(2),
            last_demotion_time: (n == 11).then(|| at(50)),
            embedding: Some(vec![1.0 / n as f32, 0.1 + n as f32 / 1000.0]),
            chunk_group_id: None,
            embedding_source: None,
        });
    }
    let edge = |n: u64, from: u64, to: u64, ty: EdgeType| Edge {
        id: id(n),
        session_id: session.clone(),
        source: id(from),
        target: id(to),
        edge_type: ty,
        weight: 0.5 + n as f64 / 1000.0,
        reinforcements: (n % 3) as i32,
        created_at: at(from.min(30) as i64),
        last_reinforced: at(40),
        event_time: (n == 205).then(|| at(-30)),
    };
    for k in 2..=8 {
        s.edges.push(edge(100 + k, k, k - 1, EdgeType::Temporal));
    }
    for (n, from, to, ty) in EDGES {
        s.edges.push(edge(n, from, to, ty));
    }
    s.synonyms = vec![
        Synonym {
            session_id: session.clone(),
            source_key: "read-me".into(),
            canonical_key: "readme".into(),
        },
        Synonym {
            session_id: session.clone(),
            source_key: "readme".into(),
            canonical_key: "readme.md".into(),
        },
    ];
    s.reservations = vec![Reservation {
        session_id: session.clone(),
        node_id: id(11),
        agent_id: AgentId::from("alpha-build"),
        expires_at: at(1000),
    }];
    for (n, node, minute) in [(401, 13, 20), (402, 11, 21)] {
        s.canonization_events.push(CanonizationEvent {
            id: id(n),
            session_id: session.clone(),
            node_id: id(node),
            from_status: CanonizationStatus::None,
            to_status: CanonizationStatus::Candidate,
            blast_radius: Some(1),
            last_demotion_time: None,
            occurred_at: at(minute),
        });
    }
    let outcome = WriteIntentOutcome {
        tag: "applied".into(),
        summary: "applied".into(),
        consumed_at: at(9),
    };
    s.write_intents = vec![
        WriteIntent {
            session_id: session.clone(),
            receipt: "r-alpha".into(),
            agent: AgentId::from("alpha-build"),
            interaction: id(3),
            lane_seq: 1,
            issued_ms: 1000,
            payload: WriteIntentPayload::Action {
                action: "planned".into(),
                produces: vec![],
                modifies: vec![],
                depends_on: vec!["alpha cache is warm".into(), "other".into()],
                depends_on_pins: vec![Some(id(11)), None],
            },
            created_at: at(3),
            outcome: Some(outcome.clone()),
        },
        WriteIntent {
            session_id: session.clone(),
            receipt: "r-beta".into(),
            agent: AgentId::from("beta-build"),
            interaction: id(7),
            lane_seq: 2,
            issued_ms: 2000,
            payload: WriteIntentPayload::DeriveCited {
                concepts: vec![("beta result is green".into(), ConceptType::Logic)],
                pairs: vec![],
                citations: PinnedCitations {
                    references: vec![PinnedReference {
                        content: "beta result is green".into(),
                        reference: "beta parser is strict".into(),
                        target: id(12),
                    }],
                    against: vec![],
                },
            },
            created_at: at(7),
            outcome: Some(outcome),
        },
    ];
    s
}

pub(crate) fn rules() -> ClassificationRules {
    ClassificationRules {
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
                content_keywords: vec!["beta".into()],
                exclude_from_general: true,
            },
        ],
        general_agent_patterns: vec!["operator".into()],
        ..Default::default()
    }
}

pub(crate) fn labels() -> Vec<ReviewedAssignment> {
    let mut out: Vec<ReviewedAssignment> = INTERACTIONS
        .iter()
        .map(|(n, _, _, session)| ReviewedAssignment {
            kind: ItemKind::Interaction,
            id: id(*n),
            project: (*session).into(),
            confidence: if *n == 8 {
                Confidence::Low
            } else {
                Confidence::High
            },
            signals: vec!["synthetic".into()],
            pass2: false,
            copy_into: vec![],
        })
        .collect();
    out.extend(
        CONCEPTS
            .iter()
            .map(|(n, _, _, _, session, copy)| ReviewedAssignment {
                kind: ItemKind::Concept,
                id: id(*n),
                project: (*session).into(),
                confidence: match (*n, copy.is_empty()) {
                    (_, false) => Confidence::Hub,
                    (21, _) => Confidence::Low,
                    _ => Confidence::High,
                },
                signals: vec!["synthetic".into()],
                pass2: false,
                copy_into: copy.iter().map(|s| (*s).into()).collect(),
            }),
    );
    out
}

/// The explicit owner correction that keeps the ruling in general.
pub(crate) fn overrides() -> Vec<OperatorOverride> {
    vec![OperatorOverride {
        shared_hub_assertion: false,
        kind: ItemKind::Concept,
        id: id(17),
        session: "general".into(),
        reason: "owner: operator-wide review ruling".into(),
        from: Some("general".into()),
        copy_into: vec![],
    }]
}

/// The seal D1 computes, for tests that edit and reseal a plan.
pub(crate) fn reseal(plan: &ClassificationPlan) -> String {
    let mut sealed = plan.clone();
    sealed.plan_sha256.clear();
    super::plan::digest(&sealed).unwrap()
}

pub(crate) fn plan_for(source: &GraphSnapshot) -> ClassificationPlan {
    classify_reviewed(source, &rules(), &labels(), &overrides()).unwrap()
}

fn target<'a>(out: &'a SplitOutput, name: &str) -> &'a GraphSnapshot {
    &out.targets
        .iter()
        .find(|t| t.session == name)
        .unwrap()
        .snapshot
}

fn mapped(out: &SplitOutput, n: u64, session: &str) -> NodeId {
    out.placements
        .iter()
        .find(|p| p.id == id(n))
        .unwrap()
        .targets[session]
}

fn entry(out: &SplitOutput, n: u64) -> &LedgerEntry {
    out.ledger.iter().find(|l| l.edge == id(n)).unwrap()
}

fn refusal(source: &GraphSnapshot) -> String {
    let plan = plan_for(source);
    build_split(&plan, source).unwrap_err().to_string()
}

#[test]
fn every_source_edge_is_in_the_ledger_exactly_once_with_exact_accounting() {
    let source = source();
    let out = build_split(&plan_for(&source), &source).unwrap();
    let mut ids: Vec<_> = out.ledger.iter().map(|l| l.edge.0).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), source.edges.len());
    assert_eq!(out.ledger.len(), source.edges.len());
    let e = &out.accounting.edges;
    assert_eq!(
        (
            e.source_edges,
            e.intra,
            e.cross_temporal_rebuilt,
            e.hub_reattached,
            e.origin_reattached,
            e.dropped
        ),
        (26, 14, 5, 2, 2, 3)
    );
    assert_eq!(
        e.intra + e.cross_temporal_rebuilt + e.hub_reattached + e.origin_reattached + e.dropped,
        e.source_edges
    );
    assert_eq!(e.dropped_ids, vec![id(213), id(304), id(305)]);
    assert_eq!(e.dropped_by_type["Causal"], 1);
    assert_eq!(e.dropped_by_type["Dependency"], 1);
    assert_eq!(e.dropped_by_type["Derives"], 1);
    assert_eq!(e.dropped_by_sessions["beta -> general"], 1);
    assert_eq!(e.dropped_by_sessions["alpha -> beta"], 1);
    assert_eq!(e.dropped_by_sessions["beta -> alpha"], 1);
    assert_eq!((e.intra_temporal, e.intra_temporal_preserved), (2, 2));
    assert_eq!(e.intra_temporal_replaced, 0);
    assert_eq!(
        (
            e.output_edges,
            e.output_chain_links,
            e.output_preserved_links,
            e.output_new_links,
            e.output_structural,
            e.output_structural_extra
        ),
        (27, 8, 6, 2, 19, 3)
    );
    let a = &out.accounting;
    assert_eq!((a.source_concepts, a.target_concepts), (11, 13));
    assert_eq!((a.hubs, a.hub_placements, a.hub_extra_rows), (3, 5, 2));
    assert_eq!(a.hubs_by_placements, [(1, 1), (2, 2)].into());
    assert_eq!(
        (
            a.source_interactions,
            a.target_interactions,
            a.interaction_replicas
        ),
        (8, 11, 3)
    );
    // i3 for concepts 15/16 in beta is a primary replica; i1 (README copy
    // in beta) and i2 (Cargo.toml copy in alpha) exist only for hub copies.
    assert_eq!(
        (
            a.replicas_for_primary_concepts,
            a.replicas_for_hub_copies_only
        ),
        (1, 2)
    );
    assert_eq!(
        (
            a.target_vectors,
            a.target_canonization_events,
            a.target_synonyms,
            a.target_write_intents,
            a.excluded_reservations
        ),
        (13, 3, 4, 2, 1)
    );
    assert_eq!(
        out.targets
            .iter()
            .map(|t| t.session.as_str())
            .collect::<Vec<_>>(),
        ["alpha", "beta", "general"]
    );
    // Output rows add up per target, and every kept edge produced a row.
    for (name, counts) in &a.per_target {
        let snap = target(&out, name);
        assert_eq!(counts.edges, snap.edges.len());
        assert_eq!(
            counts.chain_links,
            snap.interactions.len().saturating_sub(1)
        );
        assert_eq!(
            counts.chain_links,
            counts.preserved_links + counts.new_links
        );
    }
    for l in &out.ledger {
        let materialized =
            l.edge_type != EdgeType::Temporal && l.category != EdgeDisposition::Dropped;
        assert_eq!(
            materialized,
            !l.outputs.is_empty() && l.edge_type != EdgeType::Temporal
        );
    }
    // Low-confidence items are inventoried with their placement, not blocked.
    assert_eq!(out.low_confidence.len(), 2);
    assert!(out.low_confidence.iter().all(|i| i.session == "beta"));
}

#[test]
fn temporal_chains_are_subsequences_of_the_original_chain() {
    let source = source();
    let out = build_split(&plan_for(&source), &source).unwrap();
    let chain = |name: &str| -> Vec<NodeId> {
        target(&out, name)
            .interactions
            .iter()
            .map(|i| i.id)
            .collect()
    };
    // Alpha holds 2 as the origin replica of the Cargo.toml hub; beta holds
    // 1 and 3 as origins of its README copy and of concepts 15 and 16.
    let alpha: Vec<_> = [1, 2, 3, 4, 6].map(|n| mapped(&out, n, "alpha")).into();
    let beta: Vec<_> = [1, 2, 3, 7, 8].map(|n| mapped(&out, n, "beta")).into();
    assert_eq!(chain("alpha"), alpha);
    assert_eq!(chain("beta"), beta);
    assert_eq!(chain("general"), vec![mapped(&out, 5, "general")]);
    // Despite 4's earlier event time and 5/6 sharing an instant, order is
    // the original chain's: previous ids link the subsequence.
    let a = target(&out, "alpha");
    for w in a.interactions.windows(2) {
        assert_eq!(w[1].previous_id, Some(w[0].id));
    }
    assert_eq!(a.interactions[0].previous_id, None);
    // Kept adjacency keeps the original edge's metadata under a new id.
    let original = source.edges.iter().find(|e| e.id == id(104)).unwrap();
    let kept = a
        .edges
        .iter()
        .find(|e| e.source == mapped(&out, 4, "alpha") && e.edge_type == EdgeType::Temporal)
        .unwrap();
    assert_eq!(kept.target, mapped(&out, 3, "alpha"));
    assert_eq!(
        (
            kept.weight,
            kept.reinforcements,
            kept.created_at,
            kept.last_reinforced
        ),
        (
            original.weight,
            original.reinforcements,
            original.created_at,
            original.last_reinforced
        )
    );
    assert_ne!(kept.id, original.id);
    assert_eq!(entry(&out, 104).outputs, vec![("alpha".into(), kept.id)]);
    // A cross-session Temporal edge whose adjacency survives in a target
    // through replicas keeps its metadata there; its category is unchanged.
    assert_eq!(
        entry(&out, 102).category,
        EdgeDisposition::CrossTemporalRebuilt
    );
    assert_eq!(entry(&out, 102).outputs.len(), 2);
    // A new adjacency gets the documented new link.
    let new = a
        .edges
        .iter()
        .find(|e| e.source == mapped(&out, 6, "alpha") && e.edge_type == EdgeType::Temporal)
        .unwrap();
    let six = source.interactions.iter().find(|i| i.id == id(6)).unwrap();
    assert_eq!(new.target, mapped(&out, 4, "alpha"));
    assert_eq!(
        (
            new.weight,
            new.reinforcements,
            new.created_at,
            new.last_reinforced,
            new.event_time
        ),
        (
            NEW_CHAIN_LINK_WEIGHT,
            0,
            six.created_at,
            six.created_at,
            six.event_time
        )
    );
}

#[test]
fn hubs_are_replicated_with_local_ids_and_hub_edges_multiply_per_placement() {
    let source = source();
    let out = build_split(&plan_for(&source), &source).unwrap();
    let readme_alpha = mapped(&out, 13, "alpha");
    let readme_beta = mapped(&out, 13, "beta");
    assert_ne!(readme_alpha, readme_beta);
    let original = source.concepts.iter().find(|c| c.id == id(13)).unwrap();
    for (name, copy) in [("alpha", readme_alpha), ("beta", readme_beta)] {
        let c = target(&out, name)
            .concepts
            .iter()
            .find(|c| c.id == copy)
            .unwrap();
        // Everything but id, session and origin is verbatim, vector bits too.
        let expected = Concept {
            id: copy,
            session_id: SessionId::from(name),
            origin_interaction: mapped(&out, 1, name),
            ..original.clone()
        };
        assert_eq!(c, &expected);
        let bits = |v: &Option<Vec<f32>>| -> Vec<u32> {
            v.as_ref().unwrap().iter().map(|x| x.to_bits()).collect()
        };
        assert_eq!(bits(&c.embedding), bits(&original.embedding));
    }
    // The single-placement identity is a hub with one row.
    assert_eq!(
        out.placements
            .iter()
            .find(|p| p.id == id(14))
            .unwrap()
            .targets
            .len(),
        1
    );
    // beta -> alpha hub dependency attaches to beta's README copy.
    let hub_edge = entry(&out, 302);
    assert_eq!(hub_edge.category, EdgeDisposition::HubReattached);
    assert_eq!(hub_edge.outputs.len(), 1);
    let row = target(&out, "beta")
        .edges
        .iter()
        .find(|e| e.id == hub_edge.outputs[0].1)
        .unwrap();
    assert_eq!(
        (row.source, row.target),
        (mapped(&out, 12, "beta"), readme_beta)
    );
    // A hub-hub edge is one ledger item and one row per shared placement.
    let pair = entry(&out, 303);
    assert_eq!(pair.category, EdgeDisposition::HubReattached);
    assert_eq!(
        pair.outputs
            .iter()
            .map(|(t, _)| t.as_str())
            .collect::<Vec<_>>(),
        ["alpha", "beta"]
    );
    // Canonization audit and synonyms follow every hub placement.
    for name in ["alpha", "beta"] {
        let t = target(&out, name);
        assert!(t
            .canonization_events
            .iter()
            .any(|e| e.node_id == mapped(&out, 13, name) && e.occurred_at == at(20)));
        assert_eq!(t.synonyms.len(), 2);
    }
    assert!(target(&out, "general").synonyms.is_empty());
}

#[test]
fn derives_reattach_to_one_shared_origin_replica() {
    let source = source();
    let out = build_split(&plan_for(&source), &source).unwrap();
    let beta = target(&out, "beta");
    let three = mapped(&out, 3, "beta");
    assert_eq!(
        beta.interactions.iter().filter(|i| i.id == three).count(),
        1
    );
    let original = source.interactions.iter().find(|i| i.id == id(3)).unwrap();
    let replica = beta.interactions.iter().find(|i| i.id == three).unwrap();
    assert_eq!(
        (&replica.agent_id, &replica.prompt_text, replica.created_at),
        (
            &original.agent_id,
            &original.prompt_text,
            original.created_at
        )
    );
    for (edge, concept) in [(205, 15), (206, 16)] {
        let l = entry(&out, edge);
        assert_eq!(l.category, EdgeDisposition::OriginReattached);
        let row = beta.edges.iter().find(|e| e.id == l.outputs[0].1).unwrap();
        assert_eq!(
            (row.source, row.target),
            (three, mapped(&out, concept, "beta"))
        );
        let c = beta
            .concepts
            .iter()
            .find(|c| c.id == mapped(&out, concept, "beta"))
            .unwrap();
        assert_eq!(c.origin_interaction, three);
    }
    // The replica does not drag its other concepts along.
    assert!(!beta.concepts.iter().any(|c| c.content == "shared-service"));
    // A secondary cross-session Derives with no replica is dropped.
    assert_eq!(entry(&out, 213).category, EdgeDisposition::Dropped);
    assert!(entry(&out, 213).outputs.is_empty());
}

#[test]
fn targets_get_the_approved_metadata_and_local_receipts() {
    let source = source();
    let out = build_split(&plan_for(&source), &source).unwrap();
    assert!(out.root_goal_cleared);
    for t in &out.targets {
        let s = &t.snapshot;
        assert_eq!(s.root_goal, None);
        assert_eq!(s.created_at, source.created_at);
        assert_eq!(s.closed_at, None);
        assert_eq!(s.mutation_epoch, 42);
        assert_eq!(s.gc_mark, source.gc_mark);
        assert!(s.reservations.is_empty());
        assert_eq!(s.embedding, source.embedding);
        assert_eq!(t.sha256, target_sha256(s).unwrap());
    }
    let alpha = target(&out, "alpha");
    assert_eq!(alpha.write_intents.len(), 1);
    let w = &alpha.write_intents[0];
    assert_eq!(w.interaction, mapped(&out, 3, "alpha"));
    let WriteIntentPayload::Action {
        depends_on_pins, ..
    } = &w.payload
    else {
        panic!("action kept");
    };
    assert_eq!(
        depends_on_pins,
        &vec![Some(mapped(&out, 11, "alpha")), None]
    );
    // A consumed cited derive is kept in its stored, settled form.
    let beta = target(&out, "beta");
    assert_eq!(beta.write_intents.len(), 1);
    assert_eq!(
        beta.write_intents[0].payload,
        WriteIntentPayload::Derive {
            concepts: vec![],
            pairs: vec![]
        }
    );
}

#[test]
fn the_split_is_deterministic_and_independent_of_source_row_order() {
    let source = source();
    let plan = plan_for(&source);
    let first = build_split(&plan, &source).unwrap();
    assert_eq!(build_split(&plan, &source).unwrap(), first);
    let mut reordered = source.clone();
    reordered.interactions.reverse();
    reordered.concepts.reverse();
    reordered.edges.reverse();
    reordered.synonyms.reverse();
    // Audit and intent order are bound by the source digest (D1 keeps them
    // ordered), so only the unordered row sets are permuted here.
    let again = build_split(&plan_for(&reordered), &reordered).unwrap();
    assert_eq!(again, first);
    // No output id reuses a source id.
    let source_ids: std::collections::HashSet<_> = source
        .interactions
        .iter()
        .map(|i| i.id)
        .chain(source.concepts.iter().map(|c| c.id))
        .chain(source.edges.iter().map(|e| e.id))
        .collect();
    for t in &first.targets {
        assert!(t
            .snapshot
            .concepts
            .iter()
            .all(|c| !source_ids.contains(&c.id)));
        assert!(t.snapshot.edges.iter().all(|e| !source_ids.contains(&e.id)));
    }
    // A changed seal is refused rather than giving other ids.
    let mut other = plan.clone();
    other.plan_sha256 = "0".repeat(64);
    assert!(build_split(&other, &source).is_err());
}

#[test]
fn pending_intents_are_refused_before_anything_is_built() {
    let mut source = source();
    source.write_intents[0].outcome = None;
    let msg = refusal(&source);
    assert!(msg.contains("1 pending write intent(s)"), "{msg}");
    assert!(msg.contains("r-alpha"), "{msg}");
}

#[test]
fn a_consumed_pin_without_a_local_concept_is_refused() {
    let mut source = source();
    // Interaction 3 is alpha's; concept 12 lives only in beta.
    if let WriteIntentPayload::Action {
        depends_on_pins, ..
    } = &mut source.write_intents[0].payload
    {
        depends_on_pins[0] = Some(id(12));
    }
    let msg = refusal(&source);
    assert!(
        msg.contains("pins a concept outside its target alpha"),
        "{msg}"
    );
}

#[test]
fn unplaceable_or_unrepresentable_sources_are_refused() {
    let mut source = source();
    source.synonyms[1].canonical_key = "nothing-has-this-key".into();
    assert!(refusal(&source).contains("resolves to no concept"));

    let mut source = self::source();
    source.synonyms[1].canonical_key = "read-me".into();
    assert!(refusal(&source).contains("cyclic"));

    let mut source = self::source();
    source.concepts[0].last_accessed = Some(at(1) + Duration::nanoseconds(1));
    assert!(refusal(&source).contains("sub-millisecond"));

    let mut source = self::source();
    source.created_at = None;
    assert!(refusal(&source).contains("created_at"));

    // A canonical-key collision makes the source graph invalid.
    let mut source = self::source();
    source.concepts[1].canonical_key = source.concepts[0].canonical_key.clone();
    assert!(refusal(&source).contains("source graph is invalid"));

    let mut source = self::source();
    source.canonization_events[0].node_id = id(1);
    assert!(refusal(&source).contains("names no source concept"));
}

#[test]
fn stale_or_tampered_plans_are_refused() {
    let source = source();
    let plan = plan_for(&source);
    let mut changed = source.clone();
    changed.concepts[0].embedding.as_mut().unwrap()[0] = 0.25;
    assert!(build_split(&plan, &changed).is_err());
    let mut tampered = plan.clone();
    tampered
        .assignments
        .iter_mut()
        .find(|a| a.id == id(11))
        .unwrap()
        .session = "beta".into();
    assert!(build_split(&tampered, &source).is_err());
}

/// The SQLite destination: transaction boundaries, verification and
/// zero-write reruns.
#[cfg(feature = "store-sqlite")]
mod sqlite {
    use std::collections::BTreeSet;
    use std::io::Read;

    use super::*;
    use crate::resolve::SplitDestination;
    use crate::store::{GraphStore, SplitImportOutcome, SplitReceipt, SqliteStore};
    use crate::types::StoreError;

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lambo-split-{}", Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    async fn new_destination(path: &std::path::Path) -> SqliteStore {
        std::fs::File::create(path).unwrap();
        let store = SplitDestination::open(path);
        store.provision_split_destination().await.unwrap();
        store
    }

    fn receipt(out: &SplitOutput, t: &SplitTarget) -> SplitReceipt {
        SplitReceipt::for_target(
            &t.snapshot,
            &out.split_sha256,
            &t.sha256,
            out.algorithm_version,
            &out.source_sha256,
            &out.plan_sha256,
        )
    }

    fn names(out: &SplitOutput) -> BTreeSet<String> {
        out.targets.iter().map(|t| t.session.clone()).collect()
    }

    fn file_bytes(path: &std::path::Path) -> Vec<u8> {
        let mut out = Vec::new();
        for p in [
            path.to_path_buf(),
            std::path::PathBuf::from(format!("{}-wal", path.display())),
        ] {
            if let Ok(mut f) = std::fs::File::open(&p) {
                f.read_to_end(&mut out).unwrap();
            }
        }
        out
    }

    async fn import_all(store: &SqliteStore, out: &SplitOutput) -> Vec<SplitImportOutcome> {
        let mut outcomes = Vec::new();
        for t in &out.targets {
            outcomes.push(
                store
                    .import_split_target(&receipt(out, t), &t.snapshot, &names(out))
                    .await
                    .unwrap(),
            );
        }
        outcomes
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn import_round_trips_and_a_verified_rerun_writes_nothing() {
        let source = source();
        let out = build_split(&plan_for(&source), &source).unwrap();
        let dir = temp_dir();
        let path = dir.join("split.db");
        let store = new_destination(&path).await;
        let first = import_all(&store, &out).await;
        assert!(first
            .iter()
            .all(|o| matches!(o, SplitImportOutcome::Imported { changes } if *changes > 0)));
        for t in &out.targets {
            let loaded = store
                .load_session(&SessionId::from(t.session.as_str()))
                .await
                .unwrap();
            assert_eq!(
                canonical_target(loaded),
                canonical_target(t.snapshot.clone())
            );
            // A loaded target is a valid graph.
            crate::graph::Graph::from_snapshot(t.snapshot.clone()).unwrap();
        }
        store.close().await;

        // Rerun: read-only destination check, then verification only.
        let before = file_bytes(&path);
        let store = SplitDestination::open(&path);
        store
            .verify_split_destination(&out.split_sha256, &names(&out))
            .await
            .unwrap();
        let second = import_all(&store, &out).await;
        assert!(second
            .iter()
            .all(|o| *o == SplitImportOutcome::AlreadyPresent));
        store.close().await;
        assert_eq!(
            file_bytes(&path),
            before,
            "a verified rerun changed the file"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_target_leaves_no_rows_and_a_rerun_resumes() {
        let source = source();
        let out = build_split(&plan_for(&source), &source).unwrap();
        let dir = temp_dir();
        let path = dir.join("split.db");
        let store = new_destination(&path).await;
        let (alpha, beta) = (&out.targets[0], &out.targets[1]);
        store
            .import_split_target(&receipt(&out, alpha), &alpha.snapshot, &names(&out))
            .await
            .unwrap();
        // Fail beta after its edges, before its receipt: everything since
        // its BEGIN IMMEDIATE rolls back.
        for step in ["sessions", "edges", "receipt"] {
            let fault = move |at: &str| {
                if at == step {
                    Err(StoreError::Backend(format!("injected at {at}")))
                } else {
                    Ok(())
                }
            };
            let err = store
                .import_split_target_hooked(
                    &receipt(&out, beta),
                    &beta.snapshot,
                    &names(&out),
                    &fault,
                )
                .await
                .unwrap_err();
            assert!(err.to_string().contains("injected"), "{err}");
            assert!(matches!(
                store.load_session(&SessionId::from("beta")).await,
                Err(StoreError::SessionNotFound(_))
            ));
        }
        store.close().await;
        let raw = sqlx::SqlitePool::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        for table in [
            "interactions",
            "concepts",
            "edges",
            "synonyms",
            "write_intents",
        ] {
            let n: i64 = sqlx::query_scalar(&format!(
                "SELECT count(*) FROM {table} WHERE session_id = 'beta'"
            ))
            .fetch_one(&raw)
            .await
            .unwrap();
            assert_eq!(n, 0, "{table}");
        }
        let receipts: i64 = sqlx::query_scalar("SELECT count(*) FROM split_imports")
            .fetch_one(&raw)
            .await
            .unwrap();
        assert_eq!(receipts, 1);
        raw.close().await;

        let store = SplitDestination::open(&path);
        let outcomes = import_all(&store, &out).await;
        assert_eq!(outcomes[0], SplitImportOutcome::AlreadyPresent);
        assert!(matches!(outcomes[1], SplitImportOutcome::Imported { .. }));
        assert!(matches!(outcomes[2], SplitImportOutcome::Imported { .. }));
        store.close().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mismatched_existing_outputs_are_refused_without_writing() {
        let source = source();
        let out = build_split(&plan_for(&source), &source).unwrap();
        let dir = temp_dir();
        let path = dir.join("split.db");
        let store = new_destination(&path).await;
        import_all(&store, &out).await;
        store.close().await;

        // A drifted row in a committed target.
        let raw = sqlx::SqlitePool::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        sqlx::query(
            "UPDATE concepts SET access_count = access_count + 1 WHERE session_id = 'alpha'",
        )
        .execute(&raw)
        .await
        .unwrap();
        raw.close().await;
        let before = file_bytes(&path);
        let store = SplitDestination::open(&path);
        let t = &out.targets[0];
        let err = store
            .import_split_target(&receipt(&out, t), &t.snapshot, &names(&out))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no longer verifies"), "{err}");
        // Another split's identity is refused at the destination check.
        assert!(store
            .verify_split_destination(&"f".repeat(64), &names(&out))
            .await
            .is_err());
        store.close().await;
        assert_eq!(file_bytes(&path), before);

        // A plain store file is not a split output.
        let plain = dir.join("plain.db");
        let ordinary = SqliteStore::connect(plain.to_str().unwrap()).unwrap();
        ordinary.init_schema().await.unwrap();
        ordinary.close().await;
        let store = SplitDestination::open(&plain);
        let err = store
            .verify_split_destination(&out.split_sha256, &names(&out))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not a split output"), "{err}");
        store.close().await;

        // A destination holding a session this split never receipted.
        let path = dir.join("foreign.db");
        let store = new_destination(&path).await;
        store.close().await;
        let raw = sqlx::SqlitePool::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        sqlx::query("INSERT INTO sessions (session_id) VALUES ('alpha')")
            .execute(&raw)
            .await
            .unwrap();
        raw.close().await;
        let store = SplitDestination::open(&path);
        let err = store
            .import_split_target(
                &receipt(&out, &out.targets[0]),
                &out.targets[0].snapshot,
                &names(&out),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("without a receipt"), "{err}");
        store.close().await;
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn general_origin_replicas_from_excluded_projects_are_surfaced() {
    // An audited general ruling whose origin interaction the plan assigns to
    // beta (excluded from general) through a neutral actor: the replica is
    // provenance, copied as design section 8 requires, and listed for review.
    let mut source = source();
    source.interactions[7].agent_id = AgentId::from("worker");
    let mut ruling = source.concepts[10].clone();
    ruling.id = id(22);
    ruling.content = "all projects keep review notes short".into();
    ruling.canonical_key = ruling.content.clone();
    ruling.concept_type = ConceptType::Constraint;
    ruling.origin_agent = AgentId::from("worker");
    source.concepts.push(ruling);
    for c in &mut source.concepts {
        if c.origin_interaction == id(8) {
            c.origin_agent = AgentId::from("worker");
        }
    }
    let mut derives = source
        .edges
        .iter()
        .find(|e| e.id == id(211))
        .unwrap()
        .clone();
    derives.id = id(214);
    derives.target = id(22);
    source.edges.push(derives);
    let mut labels = labels();
    labels.push(ReviewedAssignment {
        kind: ItemKind::Concept,
        id: id(22),
        project: "general".into(),
        confidence: Confidence::High,
        signals: vec!["synthetic".into()],
        pass2: false,
        copy_into: vec![],
    });
    let mut overrides = overrides();
    overrides.push(OperatorOverride {
        id: id(22),
        reason: "owner: operator-wide ruling".into(),
        ..overrides[0].clone()
    });
    let plan = classify_reviewed(&source, &rules(), &labels, &overrides).unwrap();
    let out = build_split(&plan, &source).unwrap();
    let a = &out.accounting;
    assert_eq!(a.general_replicas, 1);
    assert_eq!(a.general_replicas_from_excluded_projects, vec![id(8)]);
    let general = target(&out, "general");
    assert_eq!(general.interactions.len(), 2);
    assert!(general
        .interactions
        .iter()
        .any(|i| i.id == mapped(&out, 8, "general")));
}
