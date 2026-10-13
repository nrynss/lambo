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

#[test]
fn a_none_or_candidate_concept_is_never_contradicted() {
    for status in [CanonizationStatus::None, CanonizationStatus::Candidate] {
        let s = base(status);
        assert_eq!(at(&s, 1000), None, "{status:?}");
        assert!(detect(&s.g, MIN_AGE, t(1000)).is_empty());
    }
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

#[test]
fn revalidation_drops_the_entry_when_the_target_is_demoted() {
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
    assert_eq!(revalidated(&mut hot, &s.g, 1000), None);
    assert!(hot.is_empty());
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
