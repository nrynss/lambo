//! One shared graph for the #36 dependents relation, so every implementation
//! (MemoryStore, SQLite, the graph-side `recall::format` functions, the
//! structural dispatch and the portal's inspect list) is checked against the
//! same rows and the same expected answers.
//!
//! The pillar `X` (origin `i1`) has, by name:
//!
//! | Concept | Edge | Dependent of X? |
//! |---|---|---|
//! | `A1` (`i2`) | `A1 -Dependency-> X` (an action's `depends_on`) | yes |
//! | `A2` (`i3`) | `A2 -Dependency-> X`, `A2 -Dependency-> Y` | yes, and of `Y` too (shared) |
//! | `C1` (`i1`) | `X -Hierarchical-> C1` | yes; same origin as X, so no span support |
//! | `C2` (`i4`) | `X -Hierarchical-> C2` | yes |
//! | `G` (foreign origin) | `G -Dependency-> X` | yes; its origin is another session's, so no span support |
//! | `Z` (young `iy`) | `Z -Dependency-> X`, edge aged | yes; its interaction is young, so no span support when aged |
//! | `W` (`i4`) | `W -Dependency-> X`, edge young | only with no age floor |
//! | `P` (`i5`) | `P -Hierarchical-> X` | no: X is P's child |
//! | `D` (`i1`) | `X -Dependency-> D` | no: X depends on D |
//! | `K` (`i5`) | `K -Causal-> X` | no: Causal confers no load |
//! | `N` (`i6`) | `N -CoOccurrence-> X`, `N -Semantic-> X` | no: association |
//! | interaction `i6` | `i6 -Derives-> X`, `i6 -Temporal-> i5` | no: provenance |
//! | `X` | `X -Dependency-> X` (store-only self-loop) | no |
//! | `F` (session `other`) | `F -Dependency-> X` in session `deps` | no: foreign concept |
//! | `V` (`i3`) | `V -Dependency-> X` and `X -Hierarchical-> V` (store-only 2-cycle) | yes, counted once |
//! | `E` (`ie`) | `E -Dependency-> X`; edge and interaction flushed now but carrying an old `event_time` | yes, aged by its event time |
//!
//! [`store_batch`] carries everything above (stores accept the self-loop,
//! the 2-cycle, the foreign concept and the foreign origin); [`graph`] drops
//! the rows a legal in-RAM graph cannot hold (the self-loop, `V`'s
//! `Hierarchical` edge, `G` with its foreign origin, and the edge from `F`),
//! so the graph-side answers differ from the stores' only by `G` and by
//! having no age floor.
//!
//! `ie` and `E`'s edge are flushed at [`now`] but carry an `event_time` 100
//! minutes back, so an adapter that ages on `created_at` instead of
//! `COALESCE(event_time, created_at)` drops `E` and `ie` at the age floor.

#![cfg(test)]

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use uuid::Uuid;

use crate::types::{
    AgentId, CanonizationStatus, Concept, ConceptType, Edge, EdgeType, Interaction, Mutation,
    MutationBatch, Node, NodeId, SessionId,
};

/// The session the case lives in.
pub fn session() -> SessionId {
    SessionId::from("deps")
}

/// The foreign session holding `F` and `G`'s origin interaction.
pub fn other_session() -> SessionId {
    SessionId::from("deps-other")
}

/// The concept named `name` (`"X"`, `"A1"`, ...).
pub fn concept_id(name: &str) -> NodeId {
    let n = CONCEPTS
        .iter()
        .position(|(c, _, _)| *c == name)
        .unwrap_or_else(|| panic!("no concept {name}"));
    NodeId(Uuid::from_u64_pair(0x36_02, n as u64))
}

fn interaction_id(name: &str) -> NodeId {
    let n = INTERACTIONS
        .iter()
        .position(|(i, _)| *i == name)
        .unwrap_or_else(|| panic!("no interaction {name}"));
    NodeId(Uuid::from_u64_pair(0x36_01, n as u64))
}

/// The instant the case is evaluated at.
pub fn now() -> DateTime<Utc> {
    chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 10, 1, 12, 0, 0).unwrap()
}

/// The age floor under which `W`'s edge and `iy` are young.
pub const MIN_AGE: std::time::Duration = std::time::Duration::from_secs(60);

/// Interactions: name and minutes before [`now`]. `iy` is at `now`;
/// `ix` belongs to [`other_session`].
const INTERACTIONS: [(&str, i64); 9] = [
    ("i1", 120),
    ("i2", 110),
    ("i3", 100),
    ("i4", 90),
    ("i5", 80),
    ("i6", 70),
    ("iy", 0),
    ("ix", 100),
    // Flushed now; its event time is 100 minutes back (see `EVENT_TIMED`).
    ("ie", 0),
];

/// Interactions and edges flushed at [`now`] that carry an older event time,
/// in minutes before [`now`].
const EVENT_TIMED: i64 = 100;

/// Concepts: name, origin interaction, session is `deps` unless `F`.
const CONCEPTS: [(&str, &str, ConceptType); 16] = [
    ("X", "i1", ConceptType::Logic),
    ("A1", "i2", ConceptType::Resource),
    ("A2", "i3", ConceptType::Resource),
    ("Y", "i1", ConceptType::Entity),
    ("C1", "i1", ConceptType::Entity),
    ("C2", "i4", ConceptType::Entity),
    ("G", "ix", ConceptType::Resource),
    ("Z", "iy", ConceptType::Resource),
    ("W", "i4", ConceptType::Resource),
    ("P", "i5", ConceptType::Entity),
    ("D", "i1", ConceptType::Entity),
    ("K", "i5", ConceptType::Resource),
    ("N", "i6", ConceptType::Entity),
    ("F", "ix", ConceptType::Resource),
    ("V", "i3", ConceptType::Entity),
    ("E", "ie", ConceptType::Resource),
];

fn at(minutes_before: i64) -> DateTime<Utc> {
    now() - ChronoDuration::minutes(minutes_before)
}

fn interaction(name: &str, minutes_before: i64) -> Interaction {
    let session = if name == "ix" {
        other_session()
    } else {
        session()
    };
    Interaction {
        event_time: (name == "ie").then(|| at(EVENT_TIMED)),
        id: interaction_id(name),
        session_id: session,
        agent_id: AgentId::from("agent-36"),
        prompt_text: Some(name.into()),
        previous_id: None,
        created_at: at(minutes_before),
    }
}

fn concept(name: &str, origin: &str, ty: ConceptType) -> Concept {
    let session = if name == "F" {
        other_session()
    } else {
        session()
    };
    let minutes = INTERACTIONS
        .iter()
        .find(|(i, _)| *i == origin)
        .map(|(_, m)| *m)
        .unwrap();
    Concept {
        id: concept_id(name),
        session_id: session,
        content: format!("concept {name}"),
        canonical_key: format!("concept {}", name.to_lowercase()),
        concept_type: ty,
        origin_interaction: interaction_id(origin),
        origin_agent: AgentId::from("agent-36"),
        created_at: at(minutes),
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

fn edge(n: u64, source: NodeId, target: NodeId, ty: EdgeType, minutes_before: i64) -> Edge {
    Edge {
        event_time: None,
        id: NodeId(Uuid::from_u64_pair(0x36_03, n)),
        session_id: session(),
        source,
        target,
        edge_type: ty,
        weight: 0.5 + n as f64 / 100.0,
        reinforcements: 1,
        created_at: at(minutes_before),
        last_reinforced: at(minutes_before),
    }
}

/// Every edge of the case, with whether a legal in-RAM graph can hold it.
fn edges() -> Vec<(Edge, bool)> {
    let c = concept_id;
    let i = interaction_id;
    vec![
        (edge(1, c("A1"), c("X"), EdgeType::Dependency, 110), true),
        (edge(2, c("A2"), c("X"), EdgeType::Dependency, 100), true),
        (edge(3, c("A2"), c("Y"), EdgeType::Dependency, 100), true),
        (edge(4, c("X"), c("C1"), EdgeType::Hierarchical, 120), true),
        (edge(5, c("X"), c("C2"), EdgeType::Hierarchical, 90), true),
        (edge(6, c("G"), c("X"), EdgeType::Dependency, 100), false),
        (edge(7, c("Z"), c("X"), EdgeType::Dependency, 60), true),
        (edge(8, c("W"), c("X"), EdgeType::Dependency, 0), true),
        (edge(9, c("P"), c("X"), EdgeType::Hierarchical, 80), true),
        (edge(10, c("X"), c("D"), EdgeType::Dependency, 120), true),
        (edge(11, c("K"), c("X"), EdgeType::Causal, 80), true),
        (edge(12, c("N"), c("X"), EdgeType::CoOccurrence, 70), true),
        (edge(13, c("N"), c("X"), EdgeType::Semantic, 70), true),
        (edge(14, i("i6"), c("X"), EdgeType::Derives, 70), true),
        (edge(15, i("i6"), i("i5"), EdgeType::Temporal, 70), true),
        (edge(16, c("X"), c("X"), EdgeType::Dependency, 120), false),
        (edge(17, c("F"), c("X"), EdgeType::Dependency, 100), false),
        (edge(18, c("V"), c("X"), EdgeType::Dependency, 100), true),
        (edge(19, c("X"), c("V"), EdgeType::Hierarchical, 100), false),
        (
            Edge {
                event_time: Some(at(EVENT_TIMED)),
                ..edge(20, c("E"), c("X"), EdgeType::Dependency, 0)
            },
            true,
        ),
    ]
}

/// Every row, for a store: both sessions, the self-loop, `G` and `F`.
pub fn store_batch() -> MutationBatch {
    let mut batch = MutationBatch::new();
    for (name, m) in INTERACTIONS {
        batch.push(Mutation::UpsertNode {
            node: Node::Interaction(interaction(name, m)),
        });
    }
    for (name, origin, ty) in CONCEPTS {
        batch.push(Mutation::UpsertNode {
            node: Node::Concept(concept(name, origin, ty)),
        });
    }
    for (e, _) in edges() {
        batch.push(Mutation::UpsertEdge { edge: e });
    }
    batch
}

/// A 60-day session with exactly three aged support origins spanning 4.2
/// days, all older than fourteen days at the injected clock. Two young rows
/// and an own-origin dependent must not contribute. IDs do not collide with
/// the ordinary dependents case, including on persistent live adapters.
pub fn window_batch() -> MutationBatch {
    let start = now() - ChronoDuration::days(60);
    let iid = |n| NodeId(Uuid::from_u64_pair(0x36_04, n));
    let cid = |n| NodeId(Uuid::from_u64_pair(0x36_05, n));
    let mut batch = MutationBatch::new();
    for (n, seconds) in [
        (0, 0),
        (1, 86_400),
        (2, 267_840),
        (3, 449_280),
        (4, 49 * 86_400),
        (5, 60 * 86_400),
    ] {
        let mut i = interaction("i1", 0);
        i.id = iid(n);
        i.session_id = window_session();
        i.created_at = start + ChronoDuration::seconds(seconds);
        if (1..=3).contains(&n) {
            i.event_time = Some(i.created_at);
            i.created_at = now();
        }
        batch.push(Mutation::UpsertNode {
            node: Node::Interaction(i),
        });
    }
    for (n, origin) in [(0, 0), (1, 1), (2, 2), (3, 3), (4, 0), (5, 3), (6, 5)] {
        let mut c = concept("X", "i1", ConceptType::Logic);
        c.id = cid(n);
        c.origin_interaction = iid(origin);
        c.session_id = window_session();
        c.content = format!("window-{n}");
        c.canonical_key = c.content.clone();
        c.canonization_status = if n == 0 {
            CanonizationStatus::Candidate
        } else {
            CanonizationStatus::None
        };
        batch.push(Mutation::UpsertNode {
            node: Node::Concept(c),
        });
        if n > 0 {
            let mut e = edge(n, cid(n), cid(0), EdgeType::Dependency, 0);
            e.id = NodeId(Uuid::from_u64_pair(0x36_06, n));
            e.session_id = window_session();
            // n=5 is an unaged edge; n=6 has an aged edge but a young origin.
            e.event_time = (n != 5).then_some(start);
            batch.push(Mutation::UpsertEdge { edge: e });
        }
    }
    batch
}

pub fn window_session() -> SessionId {
    SessionId::from("deps-window")
}
pub fn window_pillar() -> NodeId {
    NodeId(Uuid::from_u64_pair(0x36_05, 0))
}

/// Independent expected arithmetic and the same injected-clock/age checks
/// for every adapter. No rolling expiry is allowed; old evidence still wins.
pub async fn check_window_case(store: &dyn crate::store::GraphStore) {
    use std::time::Duration;
    store.flush(&window_batch(), None).await.unwrap();
    let sid = window_session();
    let pillar = window_pillar();
    let raw = store
        .interaction_span(&sid, pillar, MIN_AGE, now())
        .await
        .unwrap();
    assert_eq!(raw.distinct, 3);
    // The young interaction still extends the full session to sixty days.
    assert!((raw.coverage - 4.2 / 60.0).abs() < 1e-12);
    let capped = store
        .interaction_span_with_window(
            &sid,
            pillar,
            MIN_AGE,
            now(),
            crate::store::DEFAULT_COVERAGE_WINDOW,
        )
        .await
        .unwrap();
    assert_eq!(capped.distinct, 3);
    assert_eq!(capped.coverage, 0.3);
    assert!(crate::canon::stage2_evidence_passes(capped));
    assert!(
        crate::canon::stage2_passes(store, &sid, pillar, MIN_AGE, now())
            .await
            .unwrap()
    );
    let wide = Duration::from_secs(28 * 86_400);
    let wider = store
        .interaction_span_with_window(&sid, pillar, MIN_AGE, now(), wide)
        .await
        .unwrap();
    assert_eq!(wider.distinct, 3);
    assert_eq!(wider.coverage, 0.15);
    assert!(
        !crate::canon::stage2_passes_with_window(store, &sid, pillar, MIN_AGE, now(), wide)
            .await
            .unwrap()
    );
    let later = now() + ChronoDuration::days(30);
    // The two young rows now qualify; coverage saturates, never exceeds one.
    let later_span = store
        .interaction_span_with_window(
            &sid,
            pillar,
            MIN_AGE,
            later,
            crate::store::DEFAULT_COVERAGE_WINDOW,
        )
        .await
        .unwrap();
    assert_eq!(later_span.distinct, 4);
    assert_eq!(later_span.coverage, 1.0);
    let premature = store
        .interaction_span_with_window(
            &sid,
            pillar,
            MIN_AGE,
            now() - ChronoDuration::days(70),
            crate::store::DEFAULT_COVERAGE_WINDOW,
        )
        .await
        .unwrap();
    assert_eq!(premature.distinct, 0);
    assert_eq!(premature.coverage, 0.0);
    for bad in [Duration::ZERO, Duration::from_nanos(1), Duration::MAX] {
        assert!(store
            .interaction_span_with_window(&sid, pillar, MIN_AGE, now(), bad)
            .await
            .is_err());
    }
}

/// One extents session: interactions at the given offsets (seconds after
/// `start`), a pillar originating in the first, and one `Dependency`
/// dependent per listed support interaction. Edges are aged at `start`.
fn extents_batch(
    session: &SessionId,
    tag: u64,
    start: DateTime<Utc>,
    offsets: &[i64],
    supports: &[usize],
) -> MutationBatch {
    let iid = |n: usize| NodeId(Uuid::from_u64_pair(0x36_07, (tag << 8) | n as u64));
    let mut batch = MutationBatch::new();
    for (n, seconds) in offsets.iter().enumerate() {
        let mut i = interaction("i1", 0);
        i.id = iid(n);
        i.session_id = session.clone();
        i.created_at = start + ChronoDuration::seconds(*seconds);
        batch.push(Mutation::UpsertNode {
            node: Node::Interaction(i),
        });
    }
    let concept_at = |n: usize, origin: usize| {
        let mut c = concept("X", "i1", ConceptType::Logic);
        c.id = extents_concept(tag, n);
        c.origin_interaction = iid(origin);
        c.session_id = session.clone();
        c.created_at = start;
        c.content = format!("extents-{tag}-{n}");
        c.canonical_key = c.content.clone();
        c
    };
    batch.push(Mutation::UpsertNode {
        node: Node::Concept(concept_at(0, 0)),
    });
    for (k, origin) in supports.iter().enumerate() {
        let n = k + 1;
        batch.push(Mutation::UpsertNode {
            node: Node::Concept(concept_at(n, *origin)),
        });
        let mut e = edge(
            n as u64,
            extents_concept(tag, n),
            extents_concept(tag, 0),
            EdgeType::Dependency,
            0,
        );
        e.id = NodeId(Uuid::from_u64_pair(0x36_09, (tag << 8) | n as u64));
        e.session_id = session.clone();
        e.created_at = start;
        e.last_reinforced = start;
        batch.push(Mutation::UpsertEdge { edge: e });
    }
    batch
}

fn extents_concept(tag: u64, n: usize) -> NodeId {
    NodeId(Uuid::from_u64_pair(0x36_08, (tag << 8) | n as u64))
}

/// #36 PR 2 extents: the cap leaves every session no longer than fourteen
/// days exactly as it was, keeps the F1 single-point rule and the empty and
/// single-point evidence answers, and does not turn one supporting instant
/// into spread. Raw and capped answers are compared on the same store, so an
/// adapter that capped its raw accessor, or capped a short session, fails.
pub async fn check_extents_case(store: &dyn crate::store::GraphStore) {
    use std::time::Duration;
    let day = 86_400;
    let wide = Duration::from_secs(28 * 86_400);
    let caps = [crate::store::DEFAULT_COVERAGE_WINDOW, wide];

    // The ordinary `deps` session (120 minutes): every concept, both ages,
    // under both caps, answers exactly what the raw accessor answers.
    store.flush(&store_batch(), None).await.unwrap();
    for (name, _, _) in CONCEPTS {
        for age in [std::time::Duration::ZERO, MIN_AGE] {
            let node = concept_id(name);
            let raw = store
                .interaction_span(&session(), node, age, now())
                .await
                .unwrap();
            for cap in caps {
                let capped = store
                    .interaction_span_with_window(&session(), node, age, now(), cap)
                    .await
                    .unwrap();
                assert_eq!(capped, raw, "short session {name} at {age:?}, cap {cap:?}");
            }
        }
    }
    // Not vacuous: X's coverage is a proper fraction (20/120 aged).
    let x = store
        .interaction_span_with_window(&session(), concept_id("X"), MIN_AGE, now(), caps[0])
        .await
        .unwrap();
    assert!((x.coverage - 20.0 / 120.0).abs() < 1e-9, "{x:?}");

    // Single-point extent: four interactions at one instant, thirty days
    // old. Three supports cover the whole (zero-length) session: 1.0.
    let point = SessionId::from("deps-extent-point");
    let start = now() - ChronoDuration::days(30);
    store
        .flush(
            &extents_batch(&point, 1, start, &[0, 0, 0, 0], &[1, 2, 3]),
            None,
        )
        .await
        .unwrap();
    // Exactly fourteen days: three supports spread 4.2 days. Raw and capped
    // denominators coincide, so both land exactly on the 0.3 bar.
    let fortnight = SessionId::from("deps-extent-fortnight");
    let start14 = now() - ChronoDuration::days(20);
    store
        .flush(
            &extents_batch(
                &fortnight,
                2,
                start14,
                &[0, day, 2 * day, day + 362_880, 14 * day],
                &[1, 2, 3],
            ),
            None,
        )
        .await
        .unwrap();
    // A sixty-day session: a pillar with no dependents (zero points), and
    // a pillar whose only support is one interaction (one point, spread 0).
    let long = SessionId::from("deps-extent-long");
    let start60 = now() - ChronoDuration::days(70);
    store
        .flush(
            &extents_batch(&long, 3, start60, &[0, 30 * day, 60 * day], &[]),
            None,
        )
        .await
        .unwrap();
    store
        .flush(
            &extents_batch(&long, 4, start60, &[0, 30 * day, 60 * day], &[1]),
            None,
        )
        .await
        .unwrap();

    for (sid, tag, distinct, coverage, passes) in [
        (&point, 1, 3, 1.0, true),
        (&fortnight, 2, 3, 0.3, true),
        (&long, 3, 0, 0.0, false),
        (&long, 4, 1, 0.0, false),
    ] {
        let pillar = extents_concept(tag, 0);
        let raw = store
            .interaction_span(sid, pillar, MIN_AGE, now())
            .await
            .unwrap();
        assert_eq!(
            (raw.distinct, raw.coverage),
            (distinct, coverage),
            "raw {sid:?}"
        );
        for cap in caps {
            let capped = store
                .interaction_span_with_window(sid, pillar, MIN_AGE, now(), cap)
                .await
                .unwrap();
            assert_eq!(capped, raw, "{sid:?} under cap {cap:?}");
            assert_eq!(
                crate::canon::stage2_passes_with_window(store, sid, pillar, MIN_AGE, now(), cap)
                    .await
                    .unwrap(),
                passes,
                "{sid:?} under cap {cap:?}"
            );
        }
    }
}

/// The `deps` session as a legal in-RAM graph: the interactions chained in
/// time order, every concept inserted with its origin's `Derives` edge, and
/// every concept-to-concept edge except the self-loop, `G` (foreign origin)
/// and `F`'s edge. The explicit `Derives`/`Temporal` rows are left to the
/// graph's own writes.
pub fn graph() -> crate::graph::Graph {
    let mut g = crate::graph::Graph::new(session());
    let mut previous = None;
    let mut chain: Vec<(&str, i64)> = INTERACTIONS
        .iter()
        .copied()
        .filter(|(name, _)| *name != "ix")
        .collect();
    chain.sort_by_key(|(_, m)| -m);
    for (name, m) in chain {
        let mut i = interaction(name, m);
        i.previous_id = previous;
        previous = Some(i.id);
        g.insert_interaction(i).unwrap();
    }
    for (name, origin, ty) in CONCEPTS {
        if name == "F" || name == "G" {
            continue;
        }
        g.insert_concept(concept(name, origin, ty), interaction_id(origin))
            .unwrap();
    }
    for (e, legal) in edges() {
        if legal && !matches!(e.edge_type, EdgeType::Derives | EdgeType::Temporal) {
            g.upsert_edge(e).unwrap();
        }
    }
    g
}

/// Expected store answers: `(concept, min_age, blast_radius, span distinct,
/// span coverage)`. The session extent is `i1` (120 min before) to `iy`
/// (now), 120 minutes.
pub fn expected_store() -> Vec<(&'static str, std::time::Duration, u64, u64, f64)> {
    let zero = std::time::Duration::ZERO;
    vec![
        // Aged: A1, A2, C1, C2, G, Z, V (two edges, counted once) and E
        // (aged by event time). Span: i2, i3, i4, ie (C1 is X's own origin,
        // G's is foreign, Z's interaction is young) spanning 110..90 = 20/120.
        ("X", MIN_AGE, 8, 4, 20.0 / 120.0),
        // No floor: W joins (i4, already counted); iy now counts too.
        ("X", zero, 9, 5, 110.0 / 120.0),
        ("V", MIN_AGE, 0, 0, 0.0),
        ("E", MIN_AGE, 0, 0, 0.0),
        ("Y", MIN_AGE, 1, 1, 0.0),
        // X originates in i1, D's own origin: no span support.
        ("D", MIN_AGE, 1, 0, 0.0),
        // X originates in i1, which is not P's own origin (i5).
        ("P", MIN_AGE, 1, 1, 0.0),
        ("A1", MIN_AGE, 0, 0, 0.0),
        ("A2", MIN_AGE, 0, 0, 0.0),
        ("C1", MIN_AGE, 0, 0, 0.0),
        ("K", MIN_AGE, 0, 0, 0.0),
        ("N", MIN_AGE, 0, 0, 0.0),
    ]
}

/// Expected graph-side dependents (no age floor): name of pillar, names of
/// its dependents.
pub fn expected_graph() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![
        ("X", vec!["A1", "A2", "C1", "C2", "Z", "W", "V", "E"]),
        ("Y", vec!["A2"]),
        ("D", vec!["X"]),
        ("P", vec!["X"]),
        ("A1", vec![]),
        ("A2", vec![]),
        ("C1", vec![]),
        ("K", vec![]),
        ("N", vec![]),
        ("W", vec![]),
    ]
}
