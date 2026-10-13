//! One shared case for the #20 store surface: the two supersession evidence
//! accessors and the demotion-reason audit columns, checked against the same
//! rows on every adapter (MemoryStore and SQLite offline, the Tiered
//! wrapper, Postgres by hand through `store::pg::supersession_live`, and
//! Cockroach inside `conformance_suite`) and against the graph-side reading
//! the evaluator revalidates with.
//!
//! Times are minutes before [`now`]; the age floor is [`MIN_AGE`] (60 s).
//!
//! | Concept | Origin (min) | Edge (min) | Role |
//! |---|---|---|---|
//! | `C` | `ic` (200) | | the Canonical target |
//! | `S` | `is` (190) | `S -Contradicts-> C` (130) | the contradicting source |
//! | `D1` | `i1` (150) | `D1 -Dependency-> C` (150) | C's evidence |
//! | `D2` | `i2` (120) | `C -Hierarchical-> D2` (120) | C's newest evidence: the watermark |
//! | `Dc` | `ic` | `Dc -Dependency-> C` (70) | own origin: never C's evidence |
//! | `Dy` | `i6` (20) | `Dy -Dependency-> C` (0) | written after the floor |
//! | `Db` | `i6` | `Db -Dependency-> C` (0, event 10) | backdated, still written after the floor |
//! | `Sa` | `i3` (100) | `Sa -Dependency-> S` (100) | S's support after 120 |
//! | `Sb` | `i4` (80) | `Sb -Dependency-> S` (80) | S's support |
//! | `Sc` | `i6` (20) | `S -Hierarchical-> Sc` (20) | S's support, a child; S's newest |
//! | `Sd` | `i1` (150) | `Sd -Dependency-> S` (70) | new edge, origin older than 120 |
//! | `Se` | `i3` | `Se -Dependency-> S` (90) | a second edge from `i3` (one origin) |
//! | `Sf` | `is` | `Sf -Dependency-> S` (50) | own origin |
//! | `Sg` | `ix` (other session) | `Sg -Dependency-> S` (50) | foreign origin (store only) |
//! | `Sh` | `iy` (0) | `Sh -Dependency-> S` (25) | origin written after the floor |
//! | `R1`, `R2` | `i1` | | carry the reason round-trip |
//!
//! The session extent is 200 minutes (`ic` to `iy`), so a 60-minute spread
//! is coverage 0.3.

#![cfg(test)]

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use uuid::Uuid;

use crate::store::GraphStore;
use crate::types::{
    AgentId, CanonizationEvent, CanonizationStatus, Concept, ConceptType, DemotionReason, Edge,
    EdgeType, Interaction, Mutation, MutationBatch, Node, NodeId, SessionId,
};

/// The session the case lives in.
pub fn session() -> SessionId {
    SessionId::from("supersede")
}

fn other_session() -> SessionId {
    SessionId::from("supersede-other")
}

/// The instant the case is evaluated at.
pub fn now() -> DateTime<Utc> {
    chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 10, 1, 12, 0, 0).unwrap()
}

/// The age floor.
pub const MIN_AGE: std::time::Duration = std::time::Duration::from_secs(60);

/// `minutes` before [`now`].
pub fn at(minutes: i64) -> DateTime<Utc> {
    now() - ChronoDuration::minutes(minutes)
}

const INTERACTIONS: [(&str, i64); 10] = [
    ("ic", 200),
    ("is", 190),
    ("i1", 150),
    ("i2", 120),
    ("i3", 100),
    ("i4", 80),
    ("i6", 20),
    ("iy", 0),
    ("ix", 100),
    ("ir", 160),
];

const CONCEPTS: [(&str, &str); 18] = [
    ("C", "ic"),
    ("S", "is"),
    ("D1", "i1"),
    ("D2", "i2"),
    ("Dc", "ic"),
    ("Dy", "i6"),
    ("Db", "i6"),
    ("Sa", "i3"),
    ("Sb", "i4"),
    ("Sc", "i6"),
    ("Sd", "i1"),
    ("Se", "i3"),
    ("Sf", "is"),
    ("Sg", "ix"),
    ("Sh", "iy"),
    ("R1", "ir"),
    ("R2", "ir"),
    ("R3", "ir"),
];

/// The concept named `name`.
pub fn concept_id(name: &str) -> NodeId {
    let n = CONCEPTS
        .iter()
        .position(|(c, _)| *c == name)
        .unwrap_or_else(|| panic!("no concept {name}"));
    NodeId(Uuid::from_u64_pair(0x20_02, n as u64))
}

fn interaction_id(name: &str) -> NodeId {
    let n = INTERACTIONS
        .iter()
        .position(|(i, _)| *i == name)
        .unwrap_or_else(|| panic!("no interaction {name}"));
    NodeId(Uuid::from_u64_pair(0x20_01, n as u64))
}

/// Edge `n`'s id.
pub fn edge_id(n: u64) -> NodeId {
    NodeId(Uuid::from_u64_pair(0x20_03, n))
}

fn minutes_of(interaction: &str) -> i64 {
    INTERACTIONS
        .iter()
        .find(|(i, _)| *i == interaction)
        .map(|(_, m)| *m)
        .unwrap()
}

fn interaction(name: &str) -> Interaction {
    Interaction {
        event_time: None,
        id: interaction_id(name),
        session_id: if name == "ix" {
            other_session()
        } else {
            session()
        },
        agent_id: AgentId::from("agent-20"),
        prompt_text: Some(name.into()),
        previous_id: None,
        created_at: at(minutes_of(name)),
    }
}

fn concept(name: &str, origin: &str) -> Concept {
    Concept {
        id: concept_id(name),
        session_id: session(),
        content: format!("supersession {name}"),
        canonical_key: format!("supersession {}", name.to_lowercase()),
        concept_type: ConceptType::Logic,
        origin_interaction: interaction_id(origin),
        origin_agent: AgentId::from("agent-20"),
        created_at: at(minutes_of(origin)),
        access_count: 0,
        last_accessed: None,
        gc_survived: 0,
        canonization_status: CanonizationStatus::None,
        blast_radius: None,
        last_demotion_time: None,
        embedding: None,
        human_confirmed: 0,
        embedding_source: None,
        chunk_group_id: None,
    }
}

fn edge(n: u64, source: &str, target: &str, ty: EdgeType, minutes: i64) -> Edge {
    Edge {
        event_time: None,
        id: edge_id(n),
        session_id: session(),
        source: concept_id(source),
        target: concept_id(target),
        edge_type: ty,
        weight: 1.0,
        reinforcements: 1,
        created_at: at(minutes),
        last_reinforced: at(minutes),
    }
}

/// Every edge, with whether a legal in-RAM graph holds it (`Sg`'s concept
/// has a foreign origin, so the graph leaves it and its edge out).
fn edges() -> Vec<(Edge, bool)> {
    use EdgeType::{Contradicts, Dependency, Hierarchical};
    vec![
        (edge(1, "S", "C", Contradicts, 130), true),
        (edge(2, "D1", "C", Dependency, 150), true),
        (edge(3, "C", "D2", Hierarchical, 120), true),
        (edge(4, "Dc", "C", Dependency, 70), true),
        (edge(5, "Dy", "C", Dependency, 0), true),
        (
            Edge {
                event_time: Some(at(10)),
                ..edge(6, "Db", "C", Dependency, 0)
            },
            true,
        ),
        (edge(7, "Sa", "S", Dependency, 100), true),
        (edge(8, "Sb", "S", Dependency, 80), true),
        (edge(9, "S", "Sc", Hierarchical, 20), true),
        (edge(10, "Sd", "S", Dependency, 70), true),
        (edge(11, "Se", "S", Dependency, 90), true),
        (edge(12, "Sf", "S", Dependency, 50), true),
        (edge(13, "Sg", "S", Dependency, 50), false),
        (edge(14, "Sh", "S", Dependency, 25), true),
    ]
}

/// Every row, for a store.
pub fn store_batch() -> MutationBatch {
    let mut batch = MutationBatch::new();
    for (name, _) in INTERACTIONS {
        batch.push(Mutation::UpsertNode {
            node: Node::Interaction(interaction(name)),
        });
    }
    for (name, origin) in CONCEPTS {
        batch.push(Mutation::UpsertNode {
            node: Node::Concept(concept(name, origin)),
        });
    }
    for (e, _) in edges() {
        batch.push(Mutation::UpsertEdge { edge: e });
    }
    batch
}

/// The session as a legal in-RAM graph (without `Sg`).
pub fn graph() -> crate::graph::Graph {
    let mut g = crate::graph::Graph::new(session());
    let mut chain: Vec<(&str, i64)> = INTERACTIONS
        .iter()
        .copied()
        .filter(|(name, _)| *name != "ix")
        .collect();
    chain.sort_by_key(|(_, m)| -m);
    let mut previous = None;
    for (name, _) in chain {
        let mut i = interaction(name);
        i.previous_id = previous;
        previous = Some(i.id);
        g.insert_interaction(i).unwrap();
    }
    for (name, origin) in CONCEPTS {
        if name == "Sg" {
            continue;
        }
        g.insert_concept(concept(name, origin), interaction_id(origin))
            .unwrap();
    }
    for (e, legal) in edges() {
        if legal {
            g.upsert_edge(e).unwrap();
        }
    }
    g
}

/// `(node, min_age, exclude, expected)` for the watermark.
pub fn expected_latest() -> Vec<(
    &'static str,
    std::time::Duration,
    Vec<NodeId>,
    Option<DateTime<Utc>>,
)> {
    use std::time::Duration;
    vec![
        // D2's child edge is the newest eligible; Dc is own-origin; Dy and
        // Db were written after the floor (Db's backdated event time does
        // not let it skip it).
        ("C", MIN_AGE, vec![], Some(at(120))),
        // Excluding it (self-suppression) falls back to D1.
        ("C", MIN_AGE, vec![edge_id(3)], Some(at(150))),
        ("C", MIN_AGE, vec![edge_id(2), edge_id(3)], None),
        // With no floor Dy (written now) is the newest.
        ("C", Duration::ZERO, vec![], Some(at(0))),
        // S: Sc's child edge.
        ("S", MIN_AGE, vec![], Some(at(20))),
        // R1 has no evidence at all.
        ("R1", MIN_AGE, vec![], None),
    ]
}

/// `(node, since, distinct, coverage)` for the span after a cutoff, at
/// [`MIN_AGE`] and the default fourteen-day cap.
pub fn expected_span() -> Vec<(&'static str, DateTime<Utc>, u64, f64)> {
    vec![
        // i3, i4, i6: Sd's origin is older than the cutoff, Se shares i3, Sf
        // is own-origin, Sg is foreign and Sh's origin is young. 80 of 200.
        ("S", at(120), 3, 0.4),
        // Strictly after: Sa's and Se's i3 (100, 90) do not count at 90.
        // i4 and i6, 60 of 200.
        ("S", at(90), 2, 0.3),
        // Equality with the cutoff refuses: nothing is after Sc's 20.
        ("S", at(20), 0, 0.0),
        ("C", at(200), 2, 0.15),
    ]
}

/// Flush the case and check both accessors, plus [`check_graph`]'s reading.
pub async fn check_accessors(store: &dyn GraphStore) {
    store.flush(&store_batch(), None).await.expect("flush");
    let sid = session();
    for (name, age, exclude, want) in expected_latest() {
        let got = store
            .latest_structural_evidence_at(&sid, concept_id(name), age, now(), &exclude)
            .await
            .expect("latest_structural_evidence_at");
        assert_eq!(got, want, "latest({name}, {age:?}, {exclude:?})");
    }
    for (name, since, distinct, coverage) in expected_span() {
        let span = store
            .interaction_span_since(
                &sid,
                concept_id(name),
                MIN_AGE,
                now(),
                crate::store::DEFAULT_COVERAGE_WINDOW,
                since,
            )
            .await
            .expect("interaction_span_since");
        assert_eq!(span.distinct, distinct, "span_since({name}, {since})");
        assert!(
            (span.coverage - coverage).abs() < 1e-9,
            "span_since({name}, {since}) coverage {} want {coverage}",
            span.coverage
        );
    }
    // A session the store never saw is SessionNotFound, as for the span.
    let missing = SessionId::from("supersede-missing");
    assert!(matches!(
        store
            .latest_structural_evidence_at(&missing, concept_id("C"), MIN_AGE, now(), &[])
            .await,
        Err(crate::types::StoreError::SessionNotFound(_))
    ));
    // An invalid cap is refused as invalid before anything is read.
    assert!(matches!(
        store
            .interaction_span_since(
                &sid,
                concept_id("S"),
                MIN_AGE,
                now(),
                std::time::Duration::from_secs(60),
                at(120)
            )
            .await,
        Err(crate::types::StoreError::Invariant(_))
    ));
}

/// The graph-side reading (`store::evidence::rows_from_graph`) gives the
/// stores' answers.
pub fn check_graph() {
    use crate::store::evidence::{latest_at, rows_from_graph, span_since};
    let g = graph();
    for (name, age, exclude, want) in expected_latest() {
        let rows = rows_from_graph(&g, concept_id(name));
        assert_eq!(
            latest_at(&rows, age, now(), &exclude),
            want,
            "graph latest({name})"
        );
    }
    for (name, since, distinct, coverage) in expected_span() {
        let rows = rows_from_graph(&g, concept_id(name));
        let span = span_since(
            &rows,
            MIN_AGE,
            now(),
            crate::store::DEFAULT_COVERAGE_WINDOW,
            since,
        );
        assert_eq!(span.distinct, distinct, "graph span_since({name})");
        assert!(
            (span.coverage - coverage).abs() < 1e-9,
            "graph coverage {name}"
        );
    }
}

fn event(
    node: &str,
    n: u64,
    from: CanonizationStatus,
    to: CanonizationStatus,
) -> CanonizationEvent {
    let demotion = to == CanonizationStatus::None;
    CanonizationEvent {
        id: NodeId(Uuid::from_u64_pair(0x20_04, n)),
        session_id: session(),
        node_id: concept_id(node),
        from_status: from,
        to_status: to,
        blast_radius: None,
        last_demotion_time: demotion.then(|| at(30)),
        occurred_at: at(40) + ChronoDuration::seconds(n as i64),
        reason: None,
    }
}

/// The audit columns: a supersession demotion and an explicit Budget one
/// round-trip through both write paths, a reasonless demotion reads as
/// Budget, promotions carry none, and a malformed event is refused at the
/// gate. Call after [`check_accessors`] flushed the case.
pub async fn check_reason_persistence(store: &dyn GraphStore) {
    use CanonizationStatus::{Canonical, Venerable};
    let sid = session();
    let mut ladder = Vec::new();
    for (k, name) in ["R1", "R2", "R3"].into_iter().enumerate() {
        let k = k as u64 * 10;
        ladder.push(event(name, k + 1, CanonizationStatus::None, Venerable));
        ladder.push(event(name, k + 2, Venerable, Canonical));
    }
    for e in &ladder {
        store.record_canonization(e, None).await.expect("promote");
    }
    let superseded = CanonizationEvent {
        reason: Some(DemotionReason::Superseded {
            by: concept_id("S"),
        }),
        ..event("R1", 3, Canonical, CanonizationStatus::None)
    };
    let budget = CanonizationEvent {
        reason: Some(DemotionReason::Budget),
        ..event("R2", 13, Canonical, CanonizationStatus::None)
    };
    let legacy = event("R3", 23, Canonical, CanonizationStatus::None);
    store
        .record_canonization(&superseded, None)
        .await
        .expect("record the supersession");
    let mut batch = MutationBatch::new();
    for event in [budget.clone(), legacy.clone()] {
        batch.push(Mutation::CanonizationTransition { event });
    }
    store
        .flush(&batch, None)
        .await
        .expect("flush the demotions");

    let refused = [
        CanonizationEvent {
            reason: Some(DemotionReason::Budget),
            ..event(
                "R1",
                4,
                CanonizationStatus::None,
                CanonizationStatus::Candidate,
            )
        },
        CanonizationEvent {
            reason: Some(DemotionReason::Superseded {
                by: concept_id("R2"),
            }),
            ..event("R2", 14, Canonical, CanonizationStatus::None)
        },
    ];
    // The second names R2 superseding itself.
    for bad in &refused {
        assert!(matches!(
            store.record_canonization(bad, None).await,
            Err(crate::types::StoreError::Invariant(_))
        ));
    }

    let snap = store.load_session(&sid).await.expect("load");
    let find = |id: NodeId| {
        snap.canonization_events
            .iter()
            .find(|e| e.id == id)
            .cloned()
            .unwrap_or_else(|| panic!("event {id} reloaded"))
    };
    assert_eq!(find(superseded.id), superseded);
    assert_eq!(find(budget.id), budget);
    assert_eq!(find(legacy.id), legacy);
    assert_eq!(
        find(legacy.id).demotion_reason(),
        Some(DemotionReason::Budget),
        "a demotion without a reason reads as Budget"
    );
    for e in &ladder {
        assert_eq!(find(e.id).demotion_reason(), None);
    }
    for bad in &refused {
        assert!(snap.canonization_events.iter().all(|e| e.id != bad.id));
    }
    let status = |name| {
        snap.concepts
            .iter()
            .find(|c| c.id == concept_id(name))
            .map(|c| c.canonization_status)
    };
    assert_eq!(status("R1"), Some(CanonizationStatus::None));
}
