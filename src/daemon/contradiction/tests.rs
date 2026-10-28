//! #19(c) detector tests: when a contradiction fires, how it is dated and
//! attributed, which evidence outweighs it, and hot-list revalidation.

use super::*;
use crate::daemon::hotlist::HotList;
use crate::types::{Concept, ConceptType, Edge, Interaction, SessionId};
use chrono::TimeZone;
use uuid::Uuid;

const MIN_AGE: Duration = Duration::from_secs(60);

fn t(s: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(1_760_000_000 + s, 0).unwrap()
}

fn sid() -> SessionId {
    SessionId::from("19c-contradiction")
}

fn iid(n: u64) -> NodeId {
    NodeId(Uuid::from_u64_pair(0, n))
}

fn cid(n: u64) -> NodeId {
    NodeId(Uuid::from_u64_pair(1, n))
}

fn eid(n: u64) -> NodeId {
    NodeId(Uuid::from_u64_pair(2, n))
}

/// A session under construction: interactions are chained in call order.
struct Session {
    g: Graph,
    last: Option<NodeId>,
}

impl Session {
    fn new() -> Self {
        Self {
            g: Graph::new(sid()),
            last: None,
        }
    }

    fn interaction(&mut self, n: u64, agent: &str, at: i64, event_time: Option<i64>) {
        self.g
            .insert_interaction(Interaction {
                event_time: event_time.map(t),
                id: iid(n),
                session_id: sid(),
                agent_id: AgentId::from(agent),
                prompt_text: Some("p".into()),
                previous_id: self.last,
                created_at: t(at),
            })
            .unwrap();
        self.last = Some(iid(n));
    }

    fn concept(&mut self, n: u64, origin: u64, agent: &str, content: &str, at: i64) {
        self.concept_with(n, origin, agent, content, at, CanonizationStatus::None);
    }

    fn concept_with(
        &mut self,
        n: u64,
        origin: u64,
        agent: &str,
        content: &str,
        at: i64,
        status: CanonizationStatus,
    ) {
        let c = Concept {
            id: cid(n),
            session_id: sid(),
            content: content.into(),
            canonical_key: content.to_string(),
            concept_type: ConceptType::Logic,
            origin_interaction: iid(origin),
            origin_agent: AgentId::from(agent),
            created_at: t(at),
            access_count: 0,
            last_accessed: None,
            gc_survived: 0,
            canonization_status: status,
            blast_radius: None,
            last_demotion_time: None,
            embedding: None,
            human_confirmed: 0,
            embedding_source: None,
            chunk_group_id: None,
        };
        self.g.insert_concept(c, iid(origin)).unwrap();
    }

    fn edge(&mut self, n: u64, ty: EdgeType, src: u64, tgt: u64, at: i64, event_time: Option<i64>) {
        self.g
            .upsert_edge(Edge {
                event_time: event_time.map(t),
                id: eid(n),
                session_id: sid(),
                source: cid(src),
                target: cid(tgt),
                edge_type: ty,
                weight: 1.0,
                reinforcements: 1,
                created_at: t(at),
                last_reinforced: t(at),
            })
            .unwrap();
    }
}

/// The base shape, at `now = t(1000)`:
///
/// * i1 (agent-a, t0) derives X (1), Canonical.
/// * i2 (agent-b, t100) derives D (2), which depends on X at t100.
/// * i3 (agent-c, t200) derives Y (3), which contradicts X at t200.
///
/// Y's contradiction is newer than D's evidence, so it fires.
fn base(status: CanonizationStatus) -> Session {
    let mut s = Session::new();
    s.interaction(1, "agent-a", 0, None);
    s.concept_with(1, 1, "agent-a", "brand x runs small", 0, status);
    s.interaction(2, "agent-b", 100, None);
    s.concept(2, 2, "agent-b", "size up for brand x", 100);
    s.edge(10, EdgeType::Dependency, 2, 1, 100, None);
    s.interaction(3, "agent-c", 200, None);
    s.concept(3, 3, "agent-c", "brand x fits true to size", 200);
    s.edge(11, EdgeType::Contradicts, 3, 1, 200, None);
    s
}

fn at(s: &Session, now: i64) -> Option<ContradictionHit> {
    contradiction_at(&s.g, cid(1), MIN_AGE, t(now))
}

/// A later citation of X by agent-d (i4 at `when`, concept 4).
fn later_citation(s: &mut Session, when: i64) {
    s.interaction(4, "agent-d", when, None);
    s.concept(4, 4, "agent-d", "brand x size chart", when);
    s.edge(12, EdgeType::Dependency, 4, 1, when, None);
}

#[test]
fn a_newer_contradiction_of_a_canonical_or_venerable_concept_fires() {
    for status in [CanonizationStatus::Canonical, CanonizationStatus::Venerable] {
        let s = base(status);
        assert_eq!(
            at(&s, 1000),
            Some(ContradictionHit {
                node: cid(1),
                by: cid(3),
                writer: AgentId::from("agent-c"),
                at: t(200),
            }),
            "{status:?}"
        );
        assert_eq!(detect(&s.g, MIN_AGE, t(1000)).len(), 1);
    }
}

/// #179: ordinary memory is contradicted exactly like a promoted belief.
/// Through 0.7.0 a `None` or `Candidate` target was never flagged, so a stale
/// ordinary fact ranked first with no warning.
#[test]
fn a_none_or_candidate_concept_is_contradicted_too() {
    for status in [CanonizationStatus::None, CanonizationStatus::Candidate] {
        let s = base(status);
        assert_eq!(
            at(&s, 1000),
            Some(ContradictionHit {
                node: cid(1),
                by: cid(3),
                writer: AgentId::from("agent-c"),
                at: t(200),
            }),
            "{status:?}"
        );
        assert_eq!(detect(&s.g, MIN_AGE, t(1000)).len(), 1, "{status:?}");
    }
}

/// The status decides only what recall does with the contradicted hit.
#[test]
fn only_a_canonical_or_venerable_target_forces_its_inclusion() {
    for (status, forced) in [
        (CanonizationStatus::None, false),
        (CanonizationStatus::Candidate, false),
        (CanonizationStatus::Venerable, true),
        (CanonizationStatus::Canonical, true),
    ] {
        let s = base(status);
        let Some(crate::types::Node::Concept(c)) = s.g.node(cid(1)) else {
            panic!("concept 1");
        };
        assert_eq!(forces_inclusion(c), forced, "{status:?}");
    }
}

#[test]
fn contradicts_either_way_is_symmetric_and_ignores_other_edges() {
    let s = base(CanonizationStatus::None);
    assert!(contradicts_either_way(&s.g, cid(3), cid(1)));
    assert!(contradicts_either_way(&s.g, cid(1), cid(3)));
    // A dependency is not a disagreement, and nothing contradicts itself.
    assert!(!contradicts_either_way(&s.g, cid(2), cid(1)));
    assert!(!contradicts_either_way(&s.g, cid(1), cid(1)));
}

#[test]
fn later_evidence_outweighs_an_older_contradiction() {
    let mut s = base(CanonizationStatus::Canonical);
    later_citation(&mut s, 300);
    assert_eq!(at(&s, 1000), None);
}

#[test]
fn evidence_at_the_same_instant_outweighs_it() {
    // Strictly newer: evidence about-time equal to the contradiction's
    // (an event_time of t200 on a citation written at t300) keeps it quiet.
    let mut s = base(CanonizationStatus::Canonical);
    s.interaction(4, "agent-d", 300, Some(200));
    s.concept(4, 4, "agent-d", "brand x size chart", 300);
    s.edge(12, EdgeType::Dependency, 4, 1, 300, Some(200));
    assert_eq!(at(&s, 1000), None);
}

/// Review M1: a future `event_time` is clamped to the write time on both
/// sides. A contradiction dated ahead fires now, dated when it was written;
/// a citation dated ahead counts as written (once old enough), dated when
/// it was written.
#[test]
fn future_dates_are_clamped_to_the_write_time() {
    let mut s = base(CanonizationStatus::Canonical);
    s.interaction(4, "agent-e", 300, Some(5000));
    s.concept(4, 4, "agent-e", "brand x was resized", 300);
    s.edge(12, EdgeType::Contradicts, 4, 1, 300, Some(5000));
    let hit = at(&s, 1000).unwrap();
    assert_eq!((hit.by, hit.at), (cid(4), t(300)));

    let mut s = base(CanonizationStatus::Canonical);
    s.interaction(4, "agent-d", 300, Some(5000));
    s.concept(4, 4, "agent-d", "brand x size chart", 300);
    s.edge(12, EdgeType::Dependency, 4, 1, 300, Some(5000));
    assert_eq!(at(&s, 1000), None, "dated t300, it outweighs Y at t200");
}

/// Review M1's scenario: a contradiction written at t200 but dated 30 days
/// ahead, and a citation written at t300 dated t500. Before the clamp the
/// contradiction stayed hidden for 30 days and then outranked the citation
/// written before its date. Now it is dated t200 and outweighed, now and
/// in 30 days.
#[test]
fn a_contradiction_dated_30_days_ahead_does_not_outrank_earlier_writes() {
    let ahead = 30 * 86_400 + 200;
    let mut s = Session::new();
    s.interaction(1, "agent-a", 0, None);
    s.concept_with(1, 1, "agent-a", "x", 0, CanonizationStatus::Canonical);
    s.interaction(3, "agent-c", 200, Some(ahead));
    s.concept(3, 3, "agent-c", "not x", 200);
    s.edge(11, EdgeType::Contradicts, 3, 1, 200, Some(ahead));
    s.interaction(4, "agent-d", 300, Some(500));
    s.concept(4, 4, "agent-d", "cites x", 300);
    s.edge(12, EdgeType::Dependency, 4, 1, 300, Some(500));
    assert_eq!(at(&s, 1000), None);
    assert_eq!(at(&s, ahead + 1000), None);
    // Without the citation it fires at once, dated when it was written.
    let mut s = Session::new();
    s.interaction(1, "agent-a", 0, None);
    s.concept_with(1, 1, "agent-a", "x", 0, CanonizationStatus::Canonical);
    s.interaction(3, "agent-c", 200, Some(ahead));
    s.concept(3, 3, "agent-c", "not x", 200);
    s.edge(11, EdgeType::Contradicts, 3, 1, 200, Some(ahead));
    assert_eq!(at(&s, 1000).map(|h| h.at), Some(t(200)));
}

/// Review M3: eligibility is by write time. A citation written at t990
/// but backdated to t300 is not eligible 10 seconds later, whatever its
/// date says; once 60 s old it counts, dated t300.
#[test]
fn a_backdated_citation_does_not_skip_the_age_floor() {
    let mut s = base(CanonizationStatus::Canonical);
    s.interaction(4, "agent-d", 990, Some(300));
    s.concept(4, 4, "agent-d", "brand x size chart", 990);
    s.edge(12, EdgeType::Dependency, 4, 1, 990, Some(300));
    assert!(at(&s, 1000).is_some(), "written 10 s ago");
    assert_eq!(at(&s, 1050), None, "written 60 s ago, dated t300 > t200");
}

/// Review M5: a `min_age` whose cutoff is out of range makes no evidence
/// eligible instead of panicking.
#[test]
fn an_unrepresentable_min_age_makes_no_evidence_eligible() {
    let mut s = base(CanonizationStatus::Canonical);
    later_citation(&mut s, 300);
    assert!(contradiction_at(&s.g, cid(1), Duration::MAX, t(1000)).is_some());
}

#[test]
fn evidence_counts_only_once_it_is_age_eligible() {
    let mut s = base(CanonizationStatus::Canonical);
    later_citation(&mut s, 970);
    assert!(at(&s, 1000).is_some(), "30s old: younger than min_age");
    assert_eq!(at(&s, 1030), None, "60s old: eligible");
    assert_eq!(
        contradiction_at(&s.g, cid(1), Duration::ZERO, t(1000)),
        None,
        "no age floor"
    );
}

#[test]
fn a_dependent_from_the_targets_own_origin_is_not_evidence() {
    let mut s = base(CanonizationStatus::Canonical);
    // Concept 5 was derived with X (i1); it cites X later, at i4's instant.
    s.concept(5, 1, "agent-a", "brand x return policy", 0);
    s.interaction(4, "agent-d", 300, None);
    s.edge(12, EdgeType::Dependency, 5, 1, 300, None);
    assert!(at(&s, 1000).is_some());
}

#[test]
fn hierarchical_evidence_is_the_targets_children_not_its_parents() {
    // X's child, written later: evidence.
    let mut s = base(CanonizationStatus::Canonical);
    s.interaction(4, "agent-d", 300, None);
    s.concept(4, 4, "agent-d", "brand x jeans", 300);
    s.edge(12, EdgeType::Hierarchical, 1, 4, 300, None);
    assert_eq!(at(&s, 1000), None, "a child rests on X");

    // X's parent, written later: not evidence for X.
    let mut s = base(CanonizationStatus::Canonical);
    s.interaction(4, "agent-d", 300, None);
    s.concept(4, 4, "agent-d", "brand x", 300);
    s.edge(12, EdgeType::Hierarchical, 4, 1, 300, None);
    assert!(
        at(&s, 1000).is_some(),
        "X rests on its parent, not the reverse"
    );
}

#[test]
fn causal_and_contradicts_edges_are_not_evidence() {
    let mut s = base(CanonizationStatus::Canonical);
    s.interaction(4, "agent-d", 300, None);
    s.concept(4, 4, "agent-d", "reprinted the brand x label", 300);
    s.edge(12, EdgeType::Causal, 4, 1, 300, None);
    s.edge(13, EdgeType::Contradicts, 1, 4, 300, None);
    assert!(at(&s, 1000).is_some());
}

#[test]
fn without_evidence_the_targets_origin_about_time_is_the_baseline() {
    // X's origin is written at t250 and backdated to `origin_event`; the
    // contradiction is written at t300, backdated to t200.
    let build = |origin_event: i64| {
        let mut s = Session::new();
        s.interaction(1, "agent-a", 250, Some(origin_event));
        s.concept_with(1, 1, "agent-a", "x", 250, CanonizationStatus::Canonical);
        s.interaction(3, "agent-c", 300, Some(200));
        s.concept(3, 3, "agent-c", "not x", 300);
        s.edge(11, EdgeType::Contradicts, 3, 1, 300, Some(200));
        s
    };
    assert!(at(&build(150), 1000).is_some(), "newer than X's about-time");
    assert_eq!(at(&build(200), 1000), None, "equal: not newer");
    assert_eq!(at(&build(240), 1000), None, "X is about a later time");
}

/// The owner's dating rule (option A): both sides on the about-time clock.
/// A historical ingest writes everything now, backdated: a contradiction
/// about 2018 is outweighed by citations about 2020, though written later.
#[test]
fn a_backdated_contradiction_is_outweighed_by_later_dated_evidence() {
    let build = |contradiction_event: i64| {
        let mut s = Session::new();
        s.interaction(1, "agent-a", 0, Some(-30_000));
        s.concept_with(1, 1, "agent-a", "x", 0, CanonizationStatus::Canonical);
        s.interaction(2, "agent-b", 100, Some(-5_000));
        s.concept(2, 2, "agent-b", "cites x", 100);
        s.edge(10, EdgeType::Dependency, 2, 1, 100, Some(-5_000));
        s.interaction(3, "agent-c", 200, Some(contradiction_event));
        s.concept(3, 3, "agent-c", "not x", 200);
        s.edge(
            11,
            EdgeType::Contradicts,
            3,
            1,
            200,
            Some(contradiction_event),
        );
        s
    };
    assert_eq!(
        at(&build(-10_000), 1000),
        None,
        "written last, but about an earlier time than the evidence"
    );
    let hit = at(&build(-1_000), 1000).expect("about a later time than the evidence");
    assert_eq!(
        hit.at,
        t(-1_000),
        "dated by the declaring write's event_time"
    );
}

#[test]
fn re_sending_a_contradiction_does_not_re_date_or_re_attribute_it() {
    // Outweighed, then re-sent after the evidence: still outweighed.
    let mut s = base(CanonizationStatus::Canonical);
    later_citation(&mut s, 300);
    s.interaction(5, "agent-e", 400, None);
    s.edge(14, EdgeType::Contradicts, 3, 1, 400, None);
    let edge =
        s.g.edge_between(cid(3), cid(1), EdgeType::Contradicts)
            .unwrap();
    assert_eq!(edge.reinforcements, 2, "one edge, reinforced");
    assert_eq!(edge.about_time(), t(200));
    assert_eq!(at(&s, 1000), None);

    // Firing, then re-sent by another agent: same date, same writer.
    let mut s = base(CanonizationStatus::Canonical);
    s.interaction(5, "agent-e", 400, None);
    s.edge(14, EdgeType::Contradicts, 3, 1, 400, None);
    let hit = at(&s, 1000).unwrap();
    assert_eq!((hit.at, hit.writer), (t(200), AgentId::from("agent-c")));
}

#[test]
fn a_writer_cannot_outweigh_its_own_contradiction() {
    // agent-c, who contradicted X at t200, cites X from another concept at
    // t300: its own citation does not silence its contradiction.
    let mut s = base(CanonizationStatus::Canonical);
    s.interaction(4, "agent-c", 300, None);
    s.concept(4, 4, "agent-c", "brand x size chart", 300);
    s.edge(12, EdgeType::Dependency, 4, 1, 300, None);
    assert_eq!(at(&s, 1000).map(|h| h.by), Some(cid(3)));

    // The contradicting concept itself cites X later (written by agent-d):
    // still not evidence against its own contradiction.
    let mut s = base(CanonizationStatus::Canonical);
    s.interaction(4, "agent-d", 300, None);
    s.edge(12, EdgeType::Dependency, 3, 1, 300, None);
    assert_eq!(at(&s, 1000).map(|h| h.by), Some(cid(3)));

    // Control: the same citation by another agent from another concept
    // outweighs it (`later_evidence_outweighs_an_older_contradiction`).
}

#[test]
fn several_contradictions_resolve_to_the_newest_then_key_then_id() {
    let mut s = base(CanonizationStatus::Canonical);
    s.interaction(4, "agent-d", 250, None);
    s.concept(4, 4, "agent-d", "brand x fits large", 250);
    s.edge(12, EdgeType::Contradicts, 4, 1, 250, None);
    let hit = at(&s, 1000).unwrap();
    assert_eq!((hit.by, hit.writer), (cid(4), AgentId::from("agent-d")));

    // Same about-time (event_time t200 on a t250 write): the smaller
    // canonical key wins, "brand x fits large" < "brand x fits true to size".
    let mut s = base(CanonizationStatus::Canonical);
    s.interaction(4, "agent-d", 250, Some(200));
    s.concept(4, 4, "agent-d", "brand x fits large", 250);
    s.edge(12, EdgeType::Contradicts, 4, 1, 250, Some(200));
    assert_eq!(at(&s, 1000).map(|h| h.by), Some(cid(4)));
    // Run twice: deterministic.
    assert_eq!(at(&s, 1000), at(&s, 1000));
}

#[test]
fn the_writer_falls_back_to_the_contradicting_concepts_origin_agent() {
    let mut s = base(CanonizationStatus::Canonical);
    // A contradiction written at an instant no interaction was written at.
    s.interaction(4, "agent-d", 300, None);
    s.concept(4, 4, "agent-d", "brand x fits large", 300);
    s.edge(12, EdgeType::Contradicts, 4, 1, 350, None);
    let hit = at(&s, 1000).unwrap();
    assert_eq!((hit.by, hit.writer), (cid(4), AgentId::from("agent-d")));
}

#[test]
fn a_self_contradiction_never_fires() {
    let mut s = Session::new();
    s.interaction(1, "agent-a", 0, None);
    s.concept_with(1, 1, "agent-a", "x", 0, CanonizationStatus::Canonical);
    s.interaction(2, "agent-b", 200, None);
    s.edge(11, EdgeType::Contradicts, 1, 1, 200, None);
    assert_eq!(at(&s, 1000), None);
    assert!(detect(&s.g, MIN_AGE, t(1000)).is_empty());
}

#[test]
fn detect_is_sorted_and_empty_without_contradictions() {
    let mut s = base(CanonizationStatus::Canonical);
    s.interaction(4, "agent-d", 300, None);
    s.concept_with(4, 4, "agent-d", "w", 300, CanonizationStatus::Venerable);
    s.interaction(5, "agent-c", 400, None);
    s.edge(12, EdgeType::Contradicts, 3, 4, 400, None);
    let hits = detect(&s.g, MIN_AGE, t(1000));
    assert_eq!(
        hits.iter().map(|h| h.node).collect::<Vec<_>>(),
        vec![cid(1), cid(4)]
    );

    let mut s = Session::new();
    s.interaction(1, "agent-a", 0, None);
    s.concept_with(1, 1, "agent-a", "x", 0, CanonizationStatus::Canonical);
    assert!(detect(&s.g, MIN_AGE, t(1000)).is_empty());
}

fn revalidated(hot: &mut HotList, g: &Graph, now: i64) -> Option<HotListPayload> {
    hot.revalidate_members(g, [cid(1)], t(now))
        .remove(&cid(1))
        .and_then(|mut v| v.pop())
}

#[test]
fn insert_then_revalidate_keeps_a_live_contradiction() {
    let s = base(CanonizationStatus::Canonical);
    let mut hot = HotList::new();
    let hits = insert_contradictions(&mut hot, &s.g, MIN_AGE, t(1000));
    assert_eq!(hits.len(), 1);
    // Re-running refreshes, never duplicates.
    insert_contradictions(&mut hot, &s.g, MIN_AGE, t(1001));
    assert_eq!(hot.len(), 1);
    assert_eq!(hot.peek().unwrap().condition(), Condition::Contradiction);
    assert_eq!(
        revalidated(&mut hot, &s.g, 2000),
        Some(HotListPayload::Contradiction {
            by: cid(3),
            writer: AgentId::from("agent-c"),
            at: t(200),
        })
    );
}

/// #179: a demotion no longer drops the entry; the target stays
/// contradicted whatever its status.
#[test]
fn revalidation_keeps_the_entry_when_the_target_is_demoted() {
    let mut s = base(CanonizationStatus::Venerable);
    let mut hot = HotList::new();
    insert_contradictions(&mut hot, &s.g, MIN_AGE, t(1000));
    s.g.apply_canonization_transition(crate::types::CanonizationEvent {
        id: eid(90),
        session_id: sid(),
        node_id: cid(1),
        from_status: CanonizationStatus::Venerable,
        to_status: CanonizationStatus::Canonical,
        blast_radius: None,
        last_demotion_time: None,
        occurred_at: t(500),
        reason: None,
    })
    .unwrap();
    assert!(
        revalidated(&mut hot, &s.g, 1000).is_some(),
        "still promoted"
    );
    s.g.apply_canonization_transition(crate::types::CanonizationEvent {
        id: eid(91),
        session_id: sid(),
        node_id: cid(1),
        from_status: CanonizationStatus::Canonical,
        to_status: CanonizationStatus::None,
        blast_radius: None,
        last_demotion_time: Some(t(600)),
        occurred_at: t(600),
        reason: None,
    })
    .unwrap();
    assert_eq!(
        revalidated(&mut hot, &s.g, 1000),
        Some(HotListPayload::Contradiction {
            by: cid(3),
            writer: AgentId::from("agent-c"),
            at: t(200),
        }),
        "a demoted target stays contradicted"
    );
    assert_eq!(hot.len(), 1);
}

#[test]
fn revalidation_drops_the_entry_when_newer_evidence_arrives_or_ages_in() {
    let mut s = base(CanonizationStatus::Canonical);
    let mut hot = HotList::new();
    insert_contradictions(&mut hot, &s.g, MIN_AGE, t(1000));
    later_citation(&mut s, 990);
    assert!(
        revalidated(&mut hot, &s.g, 1000).is_some(),
        "10s-old evidence is not eligible yet"
    );
    assert_eq!(revalidated(&mut hot, &s.g, 1050), None, "aged in");
    assert!(hot.is_empty());
}

#[test]
fn revalidation_drops_the_entry_when_the_contradicting_concept_is_collected() {
    let mut s = base(CanonizationStatus::Canonical);
    let mut hot = HotList::new();
    insert_contradictions(&mut hot, &s.g, MIN_AGE, t(1000));
    s.g.remove_node(cid(3)).unwrap();
    assert_eq!(revalidated(&mut hot, &s.g, 1000), None);
}

#[test]
fn revalidation_moves_to_the_next_contradiction_when_one_lapses() {
    let mut s = base(CanonizationStatus::Canonical);
    s.interaction(4, "agent-d", 250, None);
    s.concept(4, 4, "agent-d", "brand x fits large", 250);
    s.edge(12, EdgeType::Contradicts, 4, 1, 250, None);
    let mut hot = HotList::new();
    insert_contradictions(&mut hot, &s.g, MIN_AGE, t(1000));
    s.g.remove_node(cid(4)).unwrap();
    assert_eq!(
        revalidated(&mut hot, &s.g, 1000),
        Some(HotListPayload::Contradiction {
            by: cid(3),
            writer: AgentId::from("agent-c"),
            at: t(200),
        })
    );
}

/// Review M2: recall's re-validation builds the writer timeline at most
/// once per graph epoch across every contradicted member, not once each.
#[test]
fn revalidating_many_contradictions_builds_the_timeline_once_per_epoch() {
    use crate::daemon::conflict::TIMELINE_BUILDS;
    let mut s = base(CanonizationStatus::Canonical);
    for (n, k) in [(5u64, 400i64), (6, 500)] {
        s.interaction(n, "agent-d", k, None);
        s.concept_with(
            n,
            n,
            "agent-d",
            &format!("w{n}"),
            k,
            CanonizationStatus::Venerable,
        );
    }
    s.interaction(7, "agent-c", 600, None);
    s.edge(20, EdgeType::Contradicts, 3, 5, 600, None);
    s.edge(21, EdgeType::Contradicts, 3, 6, 600, None);
    let mut hot = HotList::new();
    assert_eq!(
        insert_contradictions(&mut hot, &s.g, MIN_AGE, t(1000)).len(),
        3
    );
    let members = [cid(1), cid(5), cid(6)];
    let builds = || TIMELINE_BUILDS.with(|n| n.get());
    let before = builds();
    assert_eq!(hot.revalidate_members(&s.g, members, t(1000)).len(), 3);
    assert_eq!(builds() - before, 1, "one build for three predicates");
    let before = builds();
    hot.revalidate_members(&s.g, members, t(1001));
    assert_eq!(builds() - before, 0, "same epoch: reused");
    s.interaction(8, "agent-e", 700, None);
    let before = builds();
    assert_eq!(hot.revalidate_members(&s.g, members, t(1002)).len(), 3);
    assert_eq!(
        builds() - before,
        1,
        "a write moved the epoch: rebuilt once"
    );
}

/// Review L2: self-suppression is per contradiction. agent-c contradicts at
/// t200, agent-d at t250, and agent-c then cites X at t300. agent-c's
/// citation does not count against agent-c's own contradiction, but it does
/// outweigh agent-d's; so the hit shows agent-c's older contradiction,
/// though agent-d's is newer.
#[test]
fn self_suppression_is_judged_per_contradiction() {
    let mut s = base(CanonizationStatus::Canonical);
    s.interaction(4, "agent-d", 250, None);
    s.concept(4, 4, "agent-d", "brand x fits large", 250);
    s.edge(12, EdgeType::Contradicts, 4, 1, 250, None);
    s.interaction(5, "agent-c", 300, None);
    s.concept(5, 5, "agent-c", "brand x size chart", 300);
    s.edge(13, EdgeType::Dependency, 5, 1, 300, None);
    let hit = at(&s, 1000).unwrap();
    assert_eq!((hit.by, hit.writer), (cid(3), AgentId::from("agent-c")));
}

/// PR #154 review: an edge written after `now` does not exist yet at `now`,
/// even when its `event_time` is earlier.
#[test]
fn a_contradiction_written_after_now_does_not_fire_even_if_backdated() {
    let mut s = Session::new();
    s.interaction(1, "agent-a", 0, None);
    s.concept_with(1, 1, "agent-a", "x", 0, CanonizationStatus::Canonical);
    s.interaction(3, "agent-c", 2000, Some(200));
    s.concept(3, 3, "agent-c", "not x", 2000);
    s.edge(11, EdgeType::Contradicts, 3, 1, 2000, Some(200));
    assert_eq!(at(&s, 1000), None, "written at t2000, examined at t1000");
    assert_eq!(at(&s, 3000).map(|h| h.at), Some(t(200)));
}

// ---------------------------------------------------------------------------
// #207: stale by dependency
// ---------------------------------------------------------------------------

fn stale(s: &Session, node: u64, now: i64) -> Vec<ContradictionHit> {
    stale_citations(&s.g, cid(node), MIN_AGE, t(now))
}

/// In `base`, D (2) cites X (1) and Y (3) contradicts X: D rests on a
/// contradicted concept, reported as exactly `contradiction_at`'s hit.
#[test]
fn a_concept_citing_a_contradicted_concept_is_stale() {
    let s = base(CanonizationStatus::None);
    let hits = stale(&s, 2, 1000);
    assert_eq!(hits, vec![at(&s, 1000).expect("X is contradicted")]);
    assert_eq!((hits[0].node, hits[0].by), (cid(1), cid(3)));
    // X is cited, it cites nothing: an inbound citation never makes the
    // cited concept stale on itself.
    assert!(stale(&s, 1, 1000).is_empty());
}

#[test]
fn inverse_report_matches_recall_and_pages() {
    let mut s = base(CanonizationStatus::None);
    // A citing contradictor is excluded even if another contradictor wins.
    s.edge(40, EdgeType::Dependency, 3, 1, 200, None);
    for now in [150, 320, 1000] {
        let report = stale_report(&s.g, [cid(1), cid(1)], MIN_AGE, t(now));
        let expected: Vec<_> =
            s.g.concepts()
                .filter(|c| {
                    stale_citations(&s.g, c.id, MIN_AGE, t(now))
                        .iter()
                        .any(|h| h.node == cid(1))
                })
                .map(|c| c.id.0.to_string())
                .collect();
        let actual: Vec<_> = report.dependents.iter().map(|d| d.id.clone()).collect();
        assert_eq!(actual.len(), expected.len());
        assert!(actual.iter().all(|id| expected.contains(id)));
        assert_eq!(report.total, expected.len());
        assert_eq!(
            stale_report_page(&s.g, [cid(1)], MIN_AGE, t(now), report.total)
                .dependents
                .len(),
            0
        );
    }
    later_citation(&mut s, 300);
    assert!(stale_report(&s.g, [cid(1)], MIN_AGE, t(1000))
        .dependents
        .is_empty());
}

#[test]
fn inverse_report_is_bounded_and_pages_every_pair() {
    let mut s = base(CanonizationStatus::None);
    for n in 10..70 {
        s.concept(n, 2, "agent-b", &format!("dependent {n}"), 100);
        s.edge(100 + n, EdgeType::Dependency, n, 1, 100, None);
    }
    let first = stale_report(&s.g, [cid(1)], MIN_AGE, t(1000));
    let second = stale_report_page(&s.g, [cid(1)], MIN_AGE, t(1000), STALE_REPORT_LIMIT);
    assert_eq!(first.total, 61);
    assert_eq!(first.dependents.len(), STALE_REPORT_LIMIT);
    assert_eq!(second.total, 61);
    assert_eq!(second.dependents.len(), 11);
    assert!(first
        .dependents
        .iter()
        .all(|a| second.dependents.iter().all(|b| a.id != b.id)));
}

/// Before the contradiction is written, nothing is stale; the citation
/// alone flags nothing.
#[test]
fn a_citation_of_an_uncontradicted_concept_is_not_stale() {
    let mut s = Session::new();
    s.interaction(1, "agent-a", 0, None);
    s.concept(1, 1, "agent-a", "brand x runs small", 0);
    s.interaction(2, "agent-b", 100, None);
    s.concept(2, 2, "agent-b", "size up for brand x", 100);
    s.edge(10, EdgeType::Dependency, 2, 1, 100, None);
    assert!(stale(&s, 2, 1000).is_empty());
    // At an instant before the contradiction was written, `base` is clean
    // too.
    assert!(stale(&base(CanonizationStatus::None), 2, 150).is_empty());
}

/// #179's rule decides: once newer eligible evidence supports X, X is not
/// contradicted, so nothing that cites it is stale either.
#[test]
fn a_cited_concept_supported_after_its_contradiction_is_not_stale() {
    let mut s = base(CanonizationStatus::None);
    later_citation(&mut s, 300);
    assert_eq!(at(&s, 1000), None);
    assert!(stale(&s, 2, 1000).is_empty());
    assert!(stale(&s, 4, 1000).is_empty());
    // Before the later citation ages in, X is still contradicted, and both
    // of its citers are stale.
    assert_eq!(stale(&s, 2, 320).len(), 1);
    assert_eq!(stale(&s, 4, 320).len(), 1);
}

/// Several contradicted citations come back in the cited concepts' id
/// order; an uncontradicted citation in between is skipped.
#[test]
fn several_stale_citations_are_in_cited_id_order() {
    let mut s = Session::new();
    s.interaction(1, "agent-a", 0, None);
    for n in [1, 2, 3, 4] {
        s.concept(n, 1, "agent-a", &format!("claim {n}"), 0);
    }
    s.interaction(2, "agent-b", 100, None);
    s.concept(10, 2, "agent-b", "the citer", 100);
    // Written in reverse order, so the result's order is not edge order.
    for (e, cited) in [(20, 4), (21, 3), (22, 2), (23, 1)] {
        s.edge(e, EdgeType::Dependency, 10, cited, 100, None);
    }
    s.interaction(3, "agent-c", 200, None);
    s.concept(11, 3, "agent-c", "correction", 200);
    for (e, target) in [(30, 4), (31, 1), (32, 3)] {
        s.edge(e, EdgeType::Contradicts, 11, target, 200, None);
    }
    let cited: Vec<NodeId> = stale(&s, 10, 1000).iter().map(|h| h.node).collect();
    assert_eq!(cited, vec![cid(1), cid(3), cid(4)]);
}

/// Only a direct `Dependency` citation counts: not a chain through an
/// uncontradicted concept, not a `parent_of` child, not the contradicting
/// edge itself, and not another edge type.
#[test]
fn only_a_direct_dependency_citation_counts() {
    let mut s = base(CanonizationStatus::None);
    // 5 cites D (2), which cites the contradicted X: a chain.
    s.interaction(5, "agent-e", 300, None);
    s.concept(5, 5, "agent-e", "chain end", 300);
    s.edge(20, EdgeType::Dependency, 5, 2, 300, None);
    assert!(stale(&s, 5, 1000).is_empty());
    // 6 is X's parent_of parent: it does not cite X. (A child of X would
    // be #179 evidence for X instead.)
    s.concept(6, 5, "agent-e", "x's parent", 300);
    s.edge(21, EdgeType::Hierarchical, 6, 1, 300, None);
    // 8 reaches X through other edge types only.
    s.concept(8, 5, "agent-e", "x's neighbour", 300);
    for (e, ty) in [
        (23, EdgeType::Causal),
        (24, EdgeType::Semantic),
        (25, EdgeType::CoOccurrence),
    ] {
        s.edge(e, ty, 8, 1, 300, None);
    }
    assert!(at(&s, 1000).is_some(), "X is still contradicted");
    for n in [6, 8] {
        assert!(stale(&s, n, 1000).is_empty(), "concept {n}");
    }
    // Y contradicts X; it does not cite it.
    assert!(stale(&s, 3, 1000).is_empty());
    let dependency = s.g.edges().find(|e| e.id == eid(10)).expect("D's citation");
    assert_eq!(direct_citation(&s.g, dependency), Some((cid(2), cid(1))));
    let contradicts = s.g.edges().find(|e| e.id == eid(11)).unwrap();
    assert_eq!(direct_citation(&s.g, contradicts), None);
}

/// A concept that both cites X and is X's contradiction is never warned
/// that it rests on what it contradicts.
#[test]
fn the_contradicting_concept_is_not_stale_on_its_own_target() {
    let mut s = base(CanonizationStatus::None);
    s.edge(20, EdgeType::Dependency, 3, 1, 250, None);
    assert!(
        at(&s, 1000).is_some(),
        "Y's own citation does not support X"
    );
    assert!(stale(&s, 3, 1000).is_empty());
    assert_eq!(stale(&s, 2, 1000).len(), 1, "D still rests on X");
}

/// Review regression (#207): Y contradicts X and cites it, then a newer Z
/// contradicts X too, so `contradiction_at` names Z, not Y. Y is still
/// not warned that it rests on X: the exclusion is by Y's own
/// `Contradicts` edge, not by which contradictor wins. D is still warned,
/// naming Z.
#[test]
fn a_self_contradicting_citer_is_not_stale_when_a_newer_contradictor_wins() {
    let mut s = base(CanonizationStatus::None);
    s.edge(20, EdgeType::Dependency, 3, 1, 250, None);
    s.interaction(6, "agent-f", 300, None);
    s.concept(6, 6, "agent-f", "brand x was resized in 2026", 300);
    s.edge(21, EdgeType::Contradicts, 6, 1, 300, None);
    assert_eq!(at(&s, 1000).map(|h| h.by), Some(cid(6)), "Z wins");
    assert!(stale(&s, 3, 1000).is_empty(), "Y contradicts X itself");
    assert_eq!(
        stale(&s, 2, 1000)
            .iter()
            .map(|h| (h.node, h.by))
            .collect::<Vec<_>>(),
        vec![(cid(1), cid(6))]
    );
    // The reusable edge test drops Y's citation and keeps D's, so an
    // inverse "who cites X" caller gets the same exclusion.
    let ys = s.g.edges().find(|e| e.id == eid(20)).unwrap();
    assert_eq!(supporting_citation(&s.g, ys, t(1000)), None);
    assert_eq!(direct_citation(&s.g, ys), Some((cid(3), cid(1))));
    let ds = s.g.edges().find(|e| e.id == eid(10)).unwrap();
    assert_eq!(
        supporting_citation(&s.g, ds, t(1000)),
        Some((cid(2), cid(1)))
    );
    // Before Y's contradiction is written, Y's citation is an ordinary one.
    let mut early = Session::new();
    early.interaction(1, "agent-a", 0, None);
    early.concept(1, 1, "agent-a", "brand x runs small", 0);
    early.interaction(3, "agent-c", 100, None);
    early.concept(3, 3, "agent-c", "brand x fits true to size", 100);
    early.edge(20, EdgeType::Dependency, 3, 1, 100, None);
    early.edge(11, EdgeType::Contradicts, 3, 1, 500, None);
    let ys = early.g.edges().find(|e| e.id == eid(20)).unwrap();
    assert_eq!(
        supporting_citation(&early.g, ys, t(200)),
        Some((cid(3), cid(1)))
    );
    assert_eq!(supporting_citation(&early.g, ys, t(1000)), None);
}

/// A citation written after `now` (a mocked clock) is not one yet.
#[test]
fn a_citation_written_after_now_is_not_stale_yet() {
    let mut s = base(CanonizationStatus::None);
    // Written at 980: at 1000 it is not yet age-eligible evidence for X.
    s.interaction(5, "agent-e", 980, None);
    s.concept(5, 5, "agent-e", "late citer", 980);
    s.edge(20, EdgeType::Dependency, 5, 1, 980, None);
    assert!(stale(&s, 5, 500).is_empty());
    assert_eq!(stale(&s, 5, 1000).len(), 1);
}

/// A node that is not a concept of the graph has no citations.
#[test]
fn a_missing_node_has_no_stale_citations() {
    let s = base(CanonizationStatus::None);
    assert!(stale(&s, 99, 1000).is_empty());
}

/// #179 cost, measured, not asserted: a 9,500-concept session where 1,000
/// ordinary concepts are contradicted (one interaction per concept). Times
/// one detector pass into the hot list (the daemon cycle's share) and one
/// recall's re-validation of 60 members, 30 of them contradicted, on a
/// fresh epoch (it rebuilds the writer timeline) and again on the same
/// epoch. Run with `--release -- --ignored --nocapture`.
///
/// The default hot list holds at most `default_contradiction_max` (500)
/// contradictions since #196 and keeps the most recently declared, so the
/// 30 contradicted members are the last 30 declared (concepts 971..=1000),
/// which are on the list; the first 30 were evicted by the budget.
#[test]
#[ignore = "timing: run by hand with --release"]
fn contradiction_timing() {
    const CONCEPTS: u64 = 9_500;
    const CONTRADICTED: u64 = 1_000;
    let mut s = Session::new();
    for n in 1..=CONCEPTS {
        s.interaction(
            n,
            if n % 2 == 0 { "agent-a" } else { "agent-b" },
            n as i64,
            None,
        );
        s.concept(n, n, "agent-a", &format!("concept {n}"), n as i64);
    }
    // Concept CONCEPTS - k contradicts concept k, written after both.
    for k in 1..=CONTRADICTED {
        let by = CONCEPTS - k + 1;
        s.edge(
            100_000 + k,
            EdgeType::Contradicts,
            by,
            k,
            CONCEPTS as i64 + k as i64,
            None,
        );
    }
    let now = t(100_000);
    let reps = 20;
    let mut detect_us = Vec::new();
    let mut hot = HotList::new();
    for _ in 0..reps {
        let started = std::time::Instant::now();
        let hits = insert_contradictions(&mut hot, &s.g, MIN_AGE, now);
        detect_us.push(started.elapsed().as_secs_f64() * 1e6);
        assert_eq!(hits.len(), CONTRADICTED as usize);
    }
    // Contradiction k is declared at CONCEPTS + k: the last 30 are the most
    // recently declared, inside the #196 budget.
    assert!(crate::daemon::hotlist::default_contradiction_max(hot.max()) >= 30);
    let members: Vec<NodeId> = (CONTRADICTED - 29..=CONTRADICTED)
        .chain(5_000..5_030)
        .map(cid)
        .collect();
    let mut cold_us = Vec::new();
    let mut warm_us = Vec::new();
    for _ in 0..reps {
        let mut fresh = HotList::new();
        insert_contradictions(&mut fresh, &s.g, MIN_AGE, now);
        let started = std::time::Instant::now();
        let got = fresh.revalidate_members(&s.g, members.iter().copied(), now);
        cold_us.push(started.elapsed().as_secs_f64() * 1e6);
        assert_eq!(got.len(), 30);
        let started = std::time::Instant::now();
        let got = fresh.revalidate_members(&s.g, members.iter().copied(), now);
        warm_us.push(started.elapsed().as_secs_f64() * 1e6);
        assert_eq!(got.len(), 30);
    }
    let p50 = |mut v: Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    println!(
        "contradiction_timing concepts={CONCEPTS} contradicted={CONTRADICTED}: \
         detect+insert p50 {:.0} us; revalidate 60 members p50 {:.0} us first, {:.0} us again",
        p50(detect_us),
        p50(cold_us),
        p50(warm_us)
    );
}

#[test]
fn stale_page_builds_one_timeline_for_many_targets() {
    let mut s = base(CanonizationStatus::None);
    for n in 10..30 {
        s.concept(n, 2, "agent-b", &format!("target {n}"), 100);
        s.edge(100 + n, EdgeType::Contradicts, 2, n, 200, None);
    }
    crate::daemon::conflict::TIMELINE_BUILDS.with(|n| n.set(0));
    let _ = stale_report(&s.g, (10..30).map(cid), MIN_AGE, t(1000));
    crate::daemon::conflict::TIMELINE_BUILDS.with(|n| assert_eq!(n.get(), 1));
}

#[test]
fn receipt_report_bounds_worst_case_utf8_and_keeps_total() {
    let mut report = StaleReport {
        dependents: (0..STALE_REPORT_LIMIT)
            .map(|_| StaleDependent {
                id: "a".repeat(36),
                cited_id: "b".repeat(36),
                content: "😀".repeat(160),
            })
            .collect(),
        total: 500,
    };
    report.bound_for_receipt();
    let bytes: usize = report
        .dependents
        .iter()
        .map(|d| d.id.len() + d.cited_id.len() + d.content.len())
        .sum();
    assert!(bytes <= STALE_RECEIPT_BYTES);
    assert_eq!(report.dependents.len(), 2);
    assert_eq!(report.total, 500);
    assert!(report.total > report.dependents.len());
}

#[test]
fn stale_page_skips_evidence_evaluation_without_inbound_contradiction() {
    let mut s = base(CanonizationStatus::None);
    // Many targets have evidence, but only X has an inbound contradiction.
    for n in 10..30 {
        s.concept(n, 2, "agent-b", &format!("uncontradicted target {n}"), 100);
        s.edge(100 + n, EdgeType::Dependency, 2, n, 100, None);
    }
    CONTRADICTION_EVALUATIONS.with(|count| count.set(0));
    let report = stale_report(&s.g, (1..30).map(cid), MIN_AGE, t(1000));
    CONTRADICTION_EVALUATIONS.with(|count| assert_eq!(count.get(), 1));
    assert_eq!(report.total, 1);
    assert_eq!(report.dependents[0].id, cid(2).0.to_string());
}
