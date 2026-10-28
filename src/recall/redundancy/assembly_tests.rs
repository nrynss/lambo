//! #181 through assembly: which members a recall emits with
//! `[selection] skip_redundant`, what the kept hit says, and that nothing
//! changes when the rule is off or finds nothing.
//!
//! Every test runs under the shipped default ([`DEFAULT`], identical
//! content only) unless it says otherwise; one that needs the `Semantic`
//! edge or cosine signal runs under `similar_signals = true` ([`SIMILAR`])
//! and says so.

use std::collections::HashMap;

use chrono::{DateTime, TimeZone, Utc};
use uuid::Uuid;

use super::*;
use crate::config::RecallWeights;
use crate::recall::assemble::{assemble_with_legs, default_token_count};
use crate::recall::detail::DetailedRecall;
use crate::recall::expand::ExpandedSet;
use crate::types::{
    AgentId, CanonizationStatus, Concept, ConceptType, Edge, EmbeddingContract, HotListPayload,
    Interaction, Node, RecallQuery, Reservation, ScoreTable, Scored, SessionId,
};

fn sid() -> SessionId {
    SessionId::from("redundancy-assembly")
}

/// Concept ids whose short forms differ (`a001…`, `a002…`).
fn uid(n: u64) -> NodeId {
    NodeId(Uuid::from_u64_pair((0xa000 + n) << 48, n))
}

fn origin() -> NodeId {
    NodeId(Uuid::from_u64_pair(8, 1))
}

fn ts(minutes: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(1_760_000_000, 0).unwrap() + chrono::Duration::minutes(minutes)
}

const EFFICACY: &str = "Dogfood finding (2026-08-20, DOGFOOD-FINDINGS.md): 92 of 100 concepts \
                        in the lambo-dev store carry no durable embedding";

/// A test graph: `(id, content, type, vector)` concepts under a 2-d
/// contract, each with a distinct canonical key.
struct Fixture {
    g: Graph,
}

impl Fixture {
    fn new(concepts: &[(u64, &str, ConceptType, Option<[f32; 2]>)]) -> Self {
        let mut g = Graph::new(sid());
        g.stamp_embedding(EmbeddingContract {
            kind: "fixture".into(),
            model: None,
            dim: 2,
        })
        .unwrap();
        g.insert_interaction(Interaction {
            event_time: None,
            id: origin(),
            session_id: sid(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: None,
            previous_id: None,
            created_at: ts(0),
        })
        .unwrap();
        for &(n, content, ty, v) in concepts {
            g.insert_concept(
                Concept {
                    id: uid(n),
                    session_id: sid(),
                    content: content.into(),
                    canonical_key: format!("key {n:03}"),
                    concept_type: ty,
                    origin_interaction: origin(),
                    origin_agent: AgentId::from("agent-a"),
                    created_at: ts(0),
                    access_count: 0,
                    last_accessed: None,
                    gc_survived: 0,
                    canonization_status: CanonizationStatus::None,
                    blast_radius: None,
                    last_demotion_time: None,
                    embedding: v.map(|v| v.to_vec()),
                    human_confirmed: 0,
                    embedding_source: None,
                    chunk_group_id: None,
                },
                origin(),
            )
            .unwrap();
        }
        Self { g }
    }

    /// `n` distinct concepts `c1..=cn`, no vectors and no digits (a digit
    /// would make every pair distinct claims).
    fn distinct(n: u64) -> Self {
        const WORDS: [&str; 8] = [
            "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel",
        ];
        let texts: Vec<String> = (1..=n)
            .map(|i| format!("the {} fact", WORDS[i as usize - 1]))
            .collect();
        let rows: Vec<(u64, &str, ConceptType, Option<[f32; 2]>)> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| (i as u64 + 1, t.as_str(), ConceptType::Logic, None))
            .collect();
        Self::new(&rows)
    }

    fn edge(&mut self, n: u64, source: u64, target: u64, edge_type: EdgeType) -> &mut Self {
        self.edge_at(n, source, target, edge_type, ts(1))
    }

    /// [`Fixture::edge`], written at `at`.
    fn edge_at(
        &mut self,
        n: u64,
        source: u64,
        target: u64,
        edge_type: EdgeType,
        at: DateTime<Utc>,
    ) -> &mut Self {
        self.g
            .upsert_edge(Edge {
                event_time: None,
                id: NodeId(Uuid::from_u64_pair(9, n)),
                session_id: sid(),
                source: uid(source),
                target: uid(target),
                edge_type,
                weight: 1.0,
                reinforcements: 1,
                created_at: at,
                last_reinforced: at,
            })
            .unwrap();
        self
    }

    fn status(&mut self, n: u64, status: CanonizationStatus) -> &mut Self {
        let Some(Node::Concept(c)) = self.g.node(uid(n)) else {
            panic!("concept {n}");
        };
        let mut c = c.clone();
        c.canonization_status = status;
        self.g.insert_concept(c, origin()).unwrap();
        self
    }
}

/// `[selection] skip_redundant = true` alone, the shipped default:
/// identical content only.
const DEFAULT: Redundancy = Redundancy {
    threshold: 0.85,
    similar: false,
};

/// `[selection] skip_redundant = true, similar_signals = true` at BGE-M3's
/// merge threshold: every signal.
const SIMILAR: Redundancy = Redundancy {
    threshold: 0.85,
    similar: true,
};

/// One recall's knobs.
struct Run<'a> {
    /// `(id, daemon score)`: the expanded members and their rank.
    ranked: &'a [(u64, f64)],
    top_k: usize,
    max_tokens: usize,
    hot: HashMap<NodeId, Vec<HotListPayload>>,
    redundancy: Option<Redundancy>,
}

impl<'a> Run<'a> {
    /// Under the shipped default ([`DEFAULT`]).
    fn new(ranked: &'a [(u64, f64)], top_k: usize) -> Self {
        Self {
            ranked,
            top_k,
            max_tokens: 10_000,
            hot: HashMap::new(),
            redundancy: Some(DEFAULT),
        }
    }

    /// Under `similar_signals = true` ([`SIMILAR`]).
    fn similar(mut self) -> Self {
        self.redundancy = Some(SIMILAR);
        self
    }

    fn off(mut self) -> Self {
        self.redundancy = None;
        self
    }

    fn go(&self, g: &Graph) -> DetailedRecall {
        let scores = ScoreTable {
            epoch: 0,
            ranked: self
                .ranked
                .iter()
                .map(|&(n, s)| Scored::new(uid(n), s))
                .collect(),
        };
        let expanded = ExpandedSet {
            required: self
                .ranked
                .iter()
                .map(|&(n, _)| Scored::new(uid(n), 0.0))
                .collect(),
            siblings: Vec::new(),
        };
        assemble_with_legs(
            g,
            &expanded,
            &[],
            None,
            &scores,
            &self.hot,
            &RecallQuery {
                query: "irrelevant".into(),
                top_k: self.top_k,
                max_tokens: self.max_tokens,
                traversal_depth: 2,
            },
            RecallWeights::default(),
            crate::Config::default().canonization_edge_min_age,
            true,
            self.redundancy,
            ts(60),
            default_token_count,
        )
    }
}

fn ids(r: &DetailedRecall) -> Vec<NodeId> {
    r.hits.iter().map(|h| h.node_id).collect()
}

fn skipped_for(r: &DetailedRecall, n: u64) -> Vec<RedundantSkip> {
    r.detailed
        .iter()
        .find(|d| d.node_id == uid(n))
        .map(|d| d.skipped_redundant.clone())
        .unwrap_or_default()
}

fn skip(n: u64, signal: RedundancySignal) -> RedundantSkip {
    RedundantSkip {
        node_id: uid(n),
        signal,
    }
}

/// Everything a caller can observe of a recall, as one comparable value.
fn observed(r: &DetailedRecall) -> (String, String, Vec<String>, String) {
    (
        serde_json::to_string(r).unwrap(),
        r.context.clone(),
        r.warnings.clone(),
        format!("{:?}", r.hits),
    )
}

const RANKED4: [(u64, f64); 4] = [(1, 0.9), (2, 0.8), (3, 0.7), (4, 0.6)];

/// (1) Under `similar_signals = true`, two members joined by a `Semantic`
/// edge: only the higher-scored is emitted, and the next distinct member
/// takes the freed slot. Under the default the edge is not a signal, so
/// both are emitted, as with the rule off.
#[test]
fn a_semantic_edge_pair_keeps_the_higher_and_the_next_distinct_fills_the_slot() {
    let mut f = Fixture::distinct(4);
    f.edge(1, 2, 1, EdgeType::Semantic);
    let on = Run::new(&RANKED4, 2).similar().go(&f.g);
    assert_eq!(ids(&on), vec![uid(1), uid(3)]);
    assert_eq!(
        skipped_for(&on, 1),
        vec![skip(2, RedundancySignal::SemanticEdge)]
    );
    let off = Run::new(&RANKED4, 2).off().go(&f.g);
    assert_eq!(ids(&off), vec![uid(1), uid(2)]);
    let default = Run::new(&RANKED4, 2).go(&f.g);
    assert_eq!(observed(&default), observed(&off));
}

/// (2) The efficacy run's shape: one text stored as an Entity and as an
/// Observation, with no edge and no vector. One copy is skipped.
#[test]
fn identical_content_as_entity_and_observation_with_no_edge_is_skipped() {
    let f = Fixture::new(&[
        (1, EFFICACY, ConceptType::Entity, None),
        (2, EFFICACY, ConceptType::Observation, None),
        (
            3,
            "the J3 latency claim was verified",
            ConceptType::Logic,
            None,
        ),
        (4, "a fourth fact", ConceptType::Logic, None),
    ]);
    let on = Run::new(&RANKED4, 2).go(&f.g);
    assert_eq!(ids(&on), vec![uid(1), uid(3)]);
    assert_eq!(
        skipped_for(&on, 1),
        vec![skip(2, RedundancySignal::SameContent)]
    );
    // Whichever type ranks first is the one kept.
    let swapped = [(2, 0.9), (1, 0.8), (3, 0.7), (4, 0.6)];
    let on = Run::new(&swapped, 2).go(&f.g);
    assert_eq!(ids(&on), vec![uid(2), uid(3)]);
}

/// (3) A forced member is never skipped and keeps its place; an ordinary
/// duplicate of a forced hit is skipped in its favour.
#[test]
fn a_forced_member_is_never_skipped() {
    let f = Fixture::new(&[
        (1, EFFICACY, ConceptType::Entity, None),
        (2, EFFICACY, ConceptType::Observation, None),
        (3, "a third fact", ConceptType::Logic, None),
        (4, "a fourth fact", ConceptType::Logic, None),
    ]);
    let conflict = HotListPayload::Conflict {
        agents: vec![AgentId::from("agent-a"), AgentId::from("agent-b")],
        writer: AgentId::from("agent-b"),
        seconds_ago: 5,
    };
    // The duplicate is hot: forced, so it stays (past top_k).
    let mut run = Run::new(&RANKED4, 2);
    run.hot.insert(uid(2), vec![conflict.clone()]);
    let on = run.go(&f.g);
    assert_eq!(ids(&on), vec![uid(1), uid(2), uid(3)]);
    assert!(on.detailed.iter().all(|d| d.skipped_redundant.is_empty()));

    // The original is hot: the ordinary duplicate is skipped for it.
    let mut run = Run::new(&RANKED4, 2);
    run.hot.insert(uid(1), vec![conflict]);
    let on = run.go(&f.g);
    assert_eq!(ids(&on), vec![uid(1), uid(3), uid(4)]);
    assert_eq!(
        skipped_for(&on, 1),
        vec![skip(2, RedundancySignal::SameContent)]
    );
}

/// (3) The #19 forced path: a contradicted Venerable duplicate is forced
/// with its companion, so neither is skipped.
#[test]
fn a_contradicted_venerable_duplicate_and_its_companion_are_kept() {
    let mut f = Fixture::new(&[
        (1, EFFICACY, ConceptType::Entity, None),
        (2, EFFICACY, ConceptType::Observation, None),
        (3, "a third fact", ConceptType::Logic, None),
        (
            4,
            "the backfill landed, coverage is complete",
            ConceptType::Logic,
            None,
        ),
    ]);
    f.status(2, CanonizationStatus::Venerable);
    let mut run = Run::new(&RANKED4, 1);
    run.hot.insert(
        uid(2),
        vec![HotListPayload::Contradiction {
            by: uid(4),
            writer: AgentId::from("agent-b"),
            at: ts(30),
        }],
    );
    let on = run.go(&f.g);
    assert!(ids(&on).contains(&uid(1)) && ids(&on).contains(&uid(2)));
    assert!(ids(&on).contains(&uid(4)), "the companion is forced too");
}

/// A member that carries a warning of its own is not skipped, even when
/// it is not forced: an ordinary contradicted duplicate (#179) and a
/// reserved one. Its warning is not on the copy that was kept.
#[test]
fn a_duplicate_with_a_warning_of_its_own_is_not_skipped() {
    let f = Fixture::new(&[
        (1, EFFICACY, ConceptType::Entity, None),
        (2, EFFICACY, ConceptType::Observation, None),
        (3, "a third fact", ConceptType::Logic, None),
        (
            4,
            "the backfill landed, coverage is complete",
            ConceptType::Logic,
            None,
        ),
    ]);
    let mut run = Run::new(&RANKED4, 2);
    run.hot.insert(
        uid(2),
        vec![HotListPayload::Contradiction {
            by: uid(4),
            writer: AgentId::from("agent-b"),
            at: ts(30),
        }],
    );
    let on = run.go(&f.g);
    assert_eq!(&ids(&on)[..2], &[uid(1), uid(2)]);
    assert!(on.detailed.iter().all(|d| d.skipped_redundant.is_empty()));

    let mut reserved = Fixture::new(&[
        (1, EFFICACY, ConceptType::Entity, None),
        (2, EFFICACY, ConceptType::Observation, None),
        (3, "a third fact", ConceptType::Logic, None),
    ]);
    reserved.g.set_reservation(Reservation {
        session_id: sid(),
        node_id: uid(2),
        agent_id: AgentId::from("agent-c"),
        expires_at: ts(120),
    });
    let on = Run::new(&[(1, 0.9), (2, 0.8), (3, 0.7)], 2).go(&reserved.g);
    assert_eq!(ids(&on), vec![uid(1), uid(2)]);
}

/// A skipped copy that contradicts a shown hit comes back as that hit's
/// companion (#179). It is then a hit of its own, so the kept copy does not
/// also name it: no skip list holds a member of the result.
#[test]
fn a_skipped_copy_brought_back_as_a_companion_is_not_also_named() {
    let f = Fixture::new(&[
        (1, EFFICACY, ConceptType::Entity, None),
        (2, EFFICACY, ConceptType::Observation, None),
        (
            3,
            "the backfill landed, coverage is complete",
            ConceptType::Logic,
            None,
        ),
        (4, "a fourth fact", ConceptType::Logic, None),
    ]);
    let mut run = Run::new(&RANKED4, 2);
    run.hot.insert(
        uid(3),
        vec![HotListPayload::Contradiction {
            by: uid(2),
            writer: AgentId::from("agent-b"),
            at: ts(30),
        }],
    );
    let on = run.go(&f.g);
    assert!(ids(&on).contains(&uid(2)), "the companion: {:?}", ids(&on));
    assert!(ids(&on).contains(&uid(1)) && ids(&on).contains(&uid(3)));
    let shown: Vec<NodeId> = ids(&on);
    for d in &on.detailed {
        for s in &d.skipped_redundant {
            assert!(
                !shown.contains(&s.node_id),
                "{:?} is a hit and a skip",
                s.node_id
            );
        }
    }
    assert!(
        !on.context
            .contains(&format!("also {}", crate::recall::format::short_id(uid(2)))),
        "{}",
        on.context
    );
}

/// A forced member with no warning line of its own is never skipped: the
/// #19 companion of a contradicted Venerable concept is forced but carries
/// no hot-list payload, so only forcing keeps it when it repeats a hit
/// already taken.
#[test]
fn a_forced_member_without_a_warning_is_never_skipped() {
    let mut f = Fixture::new(&[
        (1, EFFICACY, ConceptType::Entity, None),
        (2, "the pillar fact", ConceptType::Logic, None),
        (3, "a third fact", ConceptType::Logic, None),
        (4, EFFICACY, ConceptType::Observation, None),
    ]);
    f.status(2, CanonizationStatus::Venerable);
    let mut run = Run::new(&RANKED4, 1);
    run.hot.insert(
        uid(2),
        vec![HotListPayload::Contradiction {
            by: uid(4),
            writer: AgentId::from("agent-b"),
            at: ts(30),
        }],
    );
    assert!(
        !run.hot.contains_key(&uid(4)),
        "premise: no payload of its own"
    );
    let on = run.go(&f.g);
    assert_eq!(ids(&on), vec![uid(1), uid(2), uid(4)]);
    assert!(on.detailed.iter().all(|d| d.skipped_redundant.is_empty()));
}

/// A duplicate whose only warning is a #207 stale citation (it cites a
/// concept another contradicts; the kept copy does not) is not skipped.
#[test]
fn a_duplicate_with_only_a_stale_citation_is_not_skipped() {
    let mut f = Fixture::new(&[
        (1, EFFICACY, ConceptType::Entity, None),
        (2, EFFICACY, ConceptType::Observation, None),
        (3, "the cited fact", ConceptType::Logic, None),
        (4, "the contradicting fact", ConceptType::Logic, None),
        (5, "a fifth fact", ConceptType::Logic, None),
    ]);
    f.edge_at(1, 2, 3, EdgeType::Dependency, ts(0))
        .edge_at(2, 4, 3, EdgeType::Contradicts, ts(10));
    let ranked = [(1, 0.9), (2, 0.8), (5, 0.7)];
    let on = Run::new(&ranked, 2).go(&f.g);
    assert_eq!(ids(&on), vec![uid(1), uid(2)]);
    let kinds: Vec<_> = on.detailed[1].annotations.iter().map(|a| a.kind).collect();
    assert_eq!(
        kinds,
        vec![crate::recall::detail::AnnotationKind::RestsOnContradicted],
        "premise: the copy's own warning"
    );
    assert!(on.detailed.iter().all(|d| d.skipped_redundant.is_empty()));
}

/// A duplicate whose only warning is a #20 supersession line (it was a
/// Canonical concept another superseded; the kept copy was not) is not
/// skipped.
#[test]
fn a_duplicate_with_only_a_supersession_line_is_not_skipped() {
    use crate::types::{CanonizationEvent, DemotionReason};
    let mut f = Fixture::new(&[
        (1, EFFICACY, ConceptType::Entity, None),
        (2, EFFICACY, ConceptType::Observation, None),
        (3, "the newer fact", ConceptType::Logic, None),
        (5, "a fifth fact", ConceptType::Logic, None),
    ]);
    for (n, (from, to, reason)) in [
        (
            CanonizationStatus::None,
            CanonizationStatus::Venerable,
            None,
        ),
        (
            CanonizationStatus::Venerable,
            CanonizationStatus::Canonical,
            None,
        ),
        (
            CanonizationStatus::Canonical,
            CanonizationStatus::None,
            Some(DemotionReason::Superseded { by: uid(3) }),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let at = ts(2 + n as i64);
        f.g.apply_canonization_transition(CanonizationEvent {
            id: NodeId(Uuid::from_u64_pair(7, n as u64)),
            session_id: sid(),
            node_id: uid(2),
            from_status: from,
            to_status: to,
            blast_radius: None,
            last_demotion_time: (to == CanonizationStatus::None).then_some(at),
            occurred_at: at,
            reason,
        })
        .unwrap();
    }
    let ranked = [(1, 0.9), (2, 0.8), (5, 0.7)];
    let on = Run::new(&ranked, 2).go(&f.g);
    assert_eq!(ids(&on), vec![uid(1), uid(2)]);
    let kinds: Vec<_> = on.detailed[1].annotations.iter().map(|a| a.kind).collect();
    assert_eq!(
        kinds,
        vec![crate::recall::detail::AnnotationKind::Superseded],
        "premise: the copy's own line"
    );
    assert!(on.detailed.iter().all(|d| d.skipped_redundant.is_empty()));
}

/// A duplicate that a newer concept is suspected of replacing (#198) is
/// not skipped: skipping it would drop its `possibly_superseded_by` field
/// and line, which the kept copy does not carry.
#[test]
fn a_duplicate_that_may_be_superseded_is_not_skipped() {
    let mut f = Fixture::new(&[
        (1, EFFICACY, ConceptType::Entity, None),
        (2, EFFICACY, ConceptType::Observation, None),
        (3, "the newer fact", ConceptType::Logic, None),
        (5, "a fifth fact", ConceptType::Logic, None),
    ]);
    f.edge(1, 3, 2, EdgeType::SuspectedSupersedes);
    let ranked = [(1, 0.9), (2, 0.8), (3, 0.7), (5, 0.6)];
    let on = Run::new(&ranked, 3).go(&f.g);
    assert_eq!(ids(&on), vec![uid(1), uid(2), uid(3)]);
    assert_eq!(on.detailed[1].possibly_superseded_by, Some(uid(3)));
    let kinds: Vec<_> = on.detailed[1].annotations.iter().map(|a| a.kind).collect();
    assert_eq!(
        kinds,
        vec![crate::recall::detail::AnnotationKind::PossiblySuperseded],
        "premise: the copy's own line"
    );
    assert!(on.detailed.iter().all(|d| d.skipped_redundant.is_empty()));
}

/// A recall over `daemon` scores and `phase1` relevance (a member with
/// relevance and no daemon score is fresh, which runs the reservation and
/// cold-start passes), at `top_k`, under the shipped default.
fn go_with_phase1(
    g: &Graph,
    daemon: &[(u64, f64)],
    phase1: &[(u64, f64)],
    top_k: usize,
) -> DetailedRecall {
    let mut members: Vec<u64> = daemon.iter().chain(phase1).map(|&(n, _)| n).collect();
    members.sort_unstable();
    members.dedup();
    let phase1: Vec<Scored<NodeId>> = phase1
        .iter()
        .map(|&(n, s)| Scored::new(uid(n), s))
        .collect();
    assemble_with_legs(
        g,
        &ExpandedSet {
            required: members.iter().map(|&n| Scored::new(uid(n), 0.0)).collect(),
            siblings: Vec::new(),
        },
        &phase1,
        None,
        &ScoreTable {
            epoch: 0,
            ranked: daemon
                .iter()
                .map(|&(n, s)| Scored::new(uid(n), s))
                .collect(),
        },
        &HashMap::new(),
        &RecallQuery {
            query: "irrelevant".into(),
            top_k,
            max_tokens: 10_000,
            traversal_depth: 0,
        },
        RecallWeights::default(),
        crate::Config::default().canonization_edge_min_age,
        true,
        Some(DEFAULT),
        ts(60),
        default_token_count,
    )
}

/// The cold-start check applies the rule too: in query order the fresh
/// hit comes third, behind two copies of one fact; skipping the second copy
/// brings it within `top_k = 2`, so the recall runs cold, as the final
/// pass (which skips the copy too) would show it.
#[test]
fn the_cold_start_check_applies_the_rule() {
    let f = Fixture::new(&[
        (1, EFFICACY, ConceptType::Entity, None),
        (2, EFFICACY, ConceptType::Observation, None),
        (3, "the fresh fact", ConceptType::Logic, None),
    ]);
    let r = go_with_phase1(
        &f.g,
        &[(1, 1.0), (2, 0.9)],
        &[(1, 0.9), (2, 0.9), (3, 0.8)],
        2,
    );
    assert!(r.cold_start, "{:?}", ids(&r));
    assert_eq!(ids(&r), vec![uid(1), uid(3)]);
    assert_eq!(
        skipped_for(&r, 1),
        vec![skip(2, RedundancySignal::SameContent)]
    );
}

/// The reservation pass applies the rule too: in blend order a reserved
/// hit comes third, behind two copies of one fact; skipping the second copy
/// gives it a slot there, so cold mode keeps it (its reservation warning
/// with it) although query order ranks it last.
#[test]
fn the_reservation_pass_applies_the_rule() {
    let mut f = Fixture::new(&[
        (1, EFFICACY, ConceptType::Entity, None),
        (2, EFFICACY, ConceptType::Observation, None),
        (3, "the reserved fact", ConceptType::Logic, None),
        (4, "the fresh fact", ConceptType::Logic, None),
        (5, "a fifth fact", ConceptType::Logic, None),
    ]);
    f.g.set_reservation(Reservation {
        session_id: sid(),
        node_id: uid(3),
        agent_id: AgentId::from("agent-c"),
        expires_at: ts(120),
    });
    let r = go_with_phase1(
        &f.g,
        &[(1, 1.0), (2, 0.99), (3, 0.98), (5, 0.0)],
        &[(4, 1.0), (1, 0.5), (2, 0.5), (5, 0.3), (3, 0.1)],
        2,
    );
    assert!(r.cold_start);
    assert_eq!(ids(&r), vec![uid(4), uid(1), uid(3)], "the holder is kept");
}

/// (4) Under `similar_signals = true` at a -1 threshold: a member without
/// a vector gives no cosine evidence; with a vector the same pair is
/// redundant. Under the default the cosine pair is not redundant either.
#[test]
fn a_member_without_a_vector_is_not_redundant_by_cosine() {
    let rows = |second: Option<[f32; 2]>| {
        Fixture::new(&[
            (1, "lease ttl is long", ConceptType::Logic, Some([1.0, 0.0])),
            (2, "leases last long", ConceptType::Logic, second),
            (3, "a third fact", ConceptType::Logic, None),
        ])
    };
    let ranked = [(1, 0.9), (2, 0.8), (3, 0.7)];
    let mut run = Run::new(&ranked, 2);
    run.redundancy = Some(Redundancy {
        threshold: -1.0,
        similar: true,
    });
    assert_eq!(ids(&run.go(&rows(None).g)), vec![uid(1), uid(2)]);
    let with = run.go(&rows(Some([1.0, 0.0])).g);
    assert_eq!(ids(&with), vec![uid(1), uid(3)]);
    assert_eq!(
        skipped_for(&with, 1),
        vec![skip(2, RedundancySignal::Cosine)]
    );
    run.redundancy = Some(Redundancy {
        threshold: -1.0,
        similar: false,
    });
    let default = run.go(&rows(Some([1.0, 0.0])).g);
    assert_eq!(ids(&default), vec![uid(1), uid(2)], "cosine only");
}

/// (5) A `Contradicts` edge in either direction keeps both, under the
/// default (the same content) and under `similar_signals = true` (the
/// same content, edge and cosine signals at once).
#[test]
fn a_contradicts_pair_is_never_skipped() {
    for (source, target) in [(1, 2), (2, 1)] {
        let mut f = Fixture::new(&[
            (1, EFFICACY, ConceptType::Entity, Some([1.0, 0.0])),
            (2, EFFICACY, ConceptType::Observation, Some([1.0, 0.0])),
            (3, "a third fact", ConceptType::Logic, None),
        ]);
        f.edge(1, 2, 1, EdgeType::Semantic)
            .edge(2, source, target, EdgeType::Contradicts);
        let ranked = [(1, 0.9), (2, 0.8), (3, 0.7)];
        for run in [Run::new(&ranked, 2), Run::new(&ranked, 2).similar()] {
            let on = run.go(&f.g);
            assert_eq!(ids(&on), vec![uid(1), uid(2)], "{source} -> {target}");
        }
    }
}

/// #198: under the default, two members with the same content joined by a
/// `SuspectedSupersedes` edge in either direction are both kept. With the
/// edge newer -> older pointing from the second-ranked member, only the
/// veto keeps it; the other way the older one would also be kept as a
/// member that may carry its own `possibly_superseded` line.
#[test]
fn a_suspected_supersession_pair_with_the_same_content_is_never_skipped() {
    let rows = [
        (1, EFFICACY, ConceptType::Entity, None),
        (2, EFFICACY, ConceptType::Observation, None),
        (3, "a third fact", ConceptType::Logic, None),
    ];
    let ranked = [(1, 0.9), (2, 0.8), (3, 0.7)];
    let premise = Run::new(&ranked, 2).go(&Fixture::new(&rows).g);
    assert_eq!(ids(&premise), vec![uid(1), uid(3)], "premise: a duplicate");
    for (source, target) in [(1, 2), (2, 1)] {
        let mut f = Fixture::new(&rows);
        f.edge(1, source, target, EdgeType::SuspectedSupersedes);
        let on = Run::new(&ranked, 2).go(&f.g);
        assert_eq!(ids(&on), vec![uid(1), uid(2)], "{source} -> {target}");
        assert!(on.detailed.iter().all(|d| d.skipped_redundant.is_empty()));
    }
}

/// #198: under `similar_signals = true`, two members joined by both a
/// `Semantic` edge and a `SuspectedSupersedes` edge in either direction
/// are both kept, though the `Semantic` edge alone finds them redundant.
#[test]
fn a_suspected_supersession_pair_with_a_semantic_edge_is_never_skipped() {
    let rows = [
        (
            1,
            "the default recall fusion is max",
            ConceptType::Logic,
            None,
        ),
        (
            2,
            "the default recall fusion is rrf",
            ConceptType::Logic,
            None,
        ),
        (3, "a third fact", ConceptType::Logic, None),
    ];
    let ranked = [(1, 0.9), (2, 0.8), (3, 0.7)];
    let mut semantic = Fixture::new(&rows);
    semantic.edge(1, 1, 2, EdgeType::Semantic);
    let premise = Run::new(&ranked, 2).similar().go(&semantic.g);
    assert_eq!(ids(&premise), vec![uid(1), uid(3)], "premise: redundant");
    assert_eq!(
        skipped_for(&premise, 1),
        vec![skip(2, RedundancySignal::SemanticEdge)]
    );
    for (source, target) in [(1, 2), (2, 1)] {
        let mut f = Fixture::new(&rows);
        f.edge(1, 1, 2, EdgeType::Semantic)
            .edge(2, source, target, EdgeType::SuspectedSupersedes);
        let on = Run::new(&ranked, 2).similar().go(&f.g);
        assert_eq!(ids(&on), vec![uid(1), uid(2)], "{source} -> {target}");
        assert!(on.detailed.iter().all(|d| d.skipped_redundant.is_empty()));
    }
}

/// (6) Under `similar_signals = true`: texts that differ in an identifier,
/// a number or a negation are never redundant, even with a `Semantic` edge
/// and equal vectors.
#[test]
fn differing_identifiers_digits_or_negation_are_never_skipped() {
    for (a, b) in [
        ("release v0.5.0 is current", "release v0.6.0 is current"),
        (
            "the gc collection cap is 280",
            "the gc collection cap is 290",
        ),
        ("see src/mcp/serve.rs", "see src/mcp/server.rs"),
        (
            "use the candle embedder on Metal",
            "do not use the candle embedder on Metal",
        ),
    ] {
        let mut f = Fixture::new(&[
            (1, a, ConceptType::Logic, Some([1.0, 0.0])),
            (2, b, ConceptType::Logic, Some([1.0, 0.0])),
            (3, "a third fact", ConceptType::Logic, None),
        ]);
        f.edge(1, 2, 1, EdgeType::Semantic);
        let on = Run::new(&[(1, 0.9), (2, 0.8), (3, 0.7)], 2)
            .similar()
            .go(&f.g);
        assert_eq!(ids(&on), vec![uid(1), uid(2)], "{a:?} / {b:?}");
    }
}

/// (7) Off, or on (under either configuration) with nothing redundant in
/// the window, the result is byte-identical: hits, detail (wire shape),
/// context and warnings.
#[test]
fn off_or_with_nothing_redundant_the_result_is_byte_identical() {
    let mut f = Fixture::new(&[
        (1, "alpha fact one", ConceptType::Logic, Some([1.0, 0.0])),
        (2, "beta fact two", ConceptType::Entity, Some([0.0, 1.0])),
        (3, "gamma fact three", ConceptType::Observation, None),
        (4, "delta fact four", ConceptType::Logic, Some([0.6, 0.8])),
    ]);
    f.edge(1, 1, 2, EdgeType::Dependency);
    let mut off = Run::new(&RANKED4, 3).off();
    off.max_tokens = 30;
    for mut on in [Run::new(&RANKED4, 3), Run::new(&RANKED4, 3).similar()] {
        on.max_tokens = 30;
        assert_eq!(observed(&on.go(&f.g)), observed(&off.go(&f.g)));
    }
    // A redundant pair outside the window changes nothing either: top_k 1
    // checks four members, and the duplicate is the fifth.
    let mut g = Fixture::distinct(5);
    g.edge(1, 5, 1, EdgeType::Semantic);
    let ranked = [(1, 0.9), (2, 0.8), (3, 0.7), (4, 0.6), (5, 0.5)];
    let on = Run::new(&ranked, 1).similar().go(&g.g);
    let off = Run::new(&ranked, 1).off().go(&g.g);
    assert_eq!(observed(&on), observed(&off));
}

/// (7) The shipped demo golden renders byte-for-byte with the rule on,
/// under either configuration: the fixture session has no redundant pair
/// in its window.
#[cfg(feature = "fixtures")]
#[test]
fn the_demo_golden_is_byte_identical_with_the_rule_on() {
    use crate::config::ScoringWeights;
    use crate::daemon::score::rescore;
    use crate::fixtures;
    use crate::graph::index::InvertedIndex;
    use crate::recall::candidates::{candidates, Phase1Input};
    use crate::recall::expand::expand;

    let snap = fixtures::load_snapshot("session-rest-api").unwrap();
    let graph = Graph::from_snapshot(snap.clone()).unwrap();
    let index = InvertedIndex::from_snapshot(&snap);
    let scores = ScoreTable {
        epoch: graph.epoch(),
        ranked: rescore(&graph, &ScoringWeights::default()),
    };
    let golden = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/recall-context-golden.txt"
    ))
    .unwrap();
    for (text, top_k) in [
        ("update user schema", 5),
        ("pagination", 5),
        ("create", 5),
        ("user", 10),
        ("schema field migration", 20),
    ] {
        let query = RecallQuery {
            query: text.into(),
            top_k,
            max_tokens: 500,
            traversal_depth: 2,
        };
        let phase1 = candidates(&graph, &index, Phase1Input::default(), text, top_k);
        let expanded = expand(&graph, phase1.clone(), 2);
        // The golden's planted conflict on `user schema`, 11 s old.
        let us = NodeId("f0000000-0000-4000-8000-000000001001".parse().unwrap());
        let hot = HashMap::from([(
            us,
            vec![HotListPayload::Conflict {
                agents: vec![AgentId::from("agent-a"), AgentId::from("agent-b")],
                writer: AgentId::from("agent-a"),
                seconds_ago: 11,
            }],
        )]);
        let run = |redundancy| {
            assemble_with_legs(
                &graph,
                &expanded,
                &phase1,
                None,
                &scores,
                &hot,
                &query,
                RecallWeights::default(),
                crate::Config::default().canonization_edge_min_age,
                true,
                redundancy,
                ts(60),
                default_token_count,
            )
        };
        for config in [DEFAULT, SIMILAR] {
            let on = run(Some(config));
            assert!(on.detailed.iter().all(|d| d.skipped_redundant.is_empty()));
            assert_eq!(observed(&on), observed(&run(None)), "{text}");
            if text == "update user schema" {
                assert_eq!(on.context, golden);
            }
        }
    }
}

/// (8) Deterministic under the existing tie-break: at equal scores the
/// lower canonical key ranks first and is the copy kept, whatever order
/// the members arrive in.
#[test]
fn equal_scores_keep_the_copy_the_tie_break_ranks_first() {
    let f = Fixture::new(&[
        (1, "a third fact", ConceptType::Logic, None),
        (5, EFFICACY, ConceptType::Observation, None),
        (6, EFFICACY, ConceptType::Entity, None),
    ]);
    for ranked in [
        [(6, 0.8), (5, 0.8), (1, 0.1)],
        [(5, 0.8), (6, 0.8), (1, 0.1)],
        [(1, 0.1), (6, 0.8), (5, 0.8)],
    ] {
        let on = Run::new(&ranked, 2).go(&f.g);
        // key 005 < key 006.
        assert_eq!(ids(&on), vec![uid(5), uid(1)], "{ranked:?}");
        assert_eq!(
            skipped_for(&on, 5),
            vec![skip(6, RedundancySignal::SameContent)]
        );
        assert_eq!(observed(&on), observed(&Run::new(&ranked, 2).go(&f.g)));
    }
}

/// (9) Under `similar_signals = true`: a member that came in through
/// phase-2 expansion (over the `Semantic` edge, with no phase-1 relevance)
/// is treated like a direct hit: skipped when it duplicates a higher hit,
/// kept when it ranks first.
#[test]
fn expanded_members_are_treated_like_direct_hits() {
    let mut f = Fixture::new(&[
        (1, "lease ttl is 45 seconds", ConceptType::Logic, None),
        (2, "the lease lasts 45 seconds", ConceptType::Logic, None),
        (3, "a third fact", ConceptType::Logic, None),
    ]);
    f.edge(1, 1, 2, EdgeType::Semantic);
    let phase1 = vec![Scored::new(uid(1), 1.0), Scored::new(uid(3), 0.9)];
    let expanded = crate::recall::expand::expand(&f.g, phase1.clone(), 2);
    assert!(
        expanded.required.iter().any(|s| s.item == uid(2)),
        "c2 is reached over the Semantic edge"
    );
    let run = |daemon: [f64; 3]| {
        let scores = ScoreTable {
            epoch: 0,
            ranked: (1..=3)
                .map(|n| Scored::new(uid(n), daemon[n as usize - 1]))
                .collect(),
        };
        assemble_with_legs(
            &f.g,
            &expanded,
            &phase1,
            None,
            &scores,
            &HashMap::new(),
            &RecallQuery {
                query: "lease".into(),
                top_k: 2,
                max_tokens: 10_000,
                traversal_depth: 2,
            },
            RecallWeights::default(),
            crate::Config::default().canonization_edge_min_age,
            true,
            Some(SIMILAR),
            ts(60),
            default_token_count,
        )
    };
    // c1 (direct) ranks first; the expanded c2 would be second.
    let on = run([0.5, 1.5, 0.0]);
    assert_eq!(ids(&on), vec![uid(1), uid(3)]);
    assert_eq!(
        skipped_for(&on, 1),
        vec![skip(2, RedundancySignal::SemanticEdge)]
    );
    // The expanded c2 ranks first: the direct c1 is the one skipped.
    let on = run([0.0, 4.0, 0.0]);
    assert_eq!(ids(&on), vec![uid(2), uid(3)]);
    assert_eq!(
        skipped_for(&on, 2),
        vec![skip(1, RedundancySignal::SemanticEdge)]
    );
}

/// (11) The kept hit's label names what was skipped (`also <id>`, three at
/// most, then `+N more`), the detail carries each skip with its signal,
/// and the CLI re-render of the detail matches the pipeline's block.
#[test]
fn the_kept_hit_names_the_skipped_members() {
    let rows: Vec<(u64, &str, ConceptType, Option<[f32; 2]>)> = (1..=6)
        .map(|n| (n, EFFICACY, ConceptType::Observation, None))
        .chain([(7, "a seventh fact", ConceptType::Logic, None)])
        .collect();
    let f = Fixture::new(&rows);
    let ranked: Vec<(u64, f64)> = (1..=7).map(|n| (n, 1.0 - n as f64 / 10.0)).collect();
    let on = Run::new(&ranked, 2).go(&f.g);
    assert_eq!(ids(&on), vec![uid(1), uid(7)]);
    let skipped = skipped_for(&on, 1);
    assert_eq!(
        skipped,
        (2..=6)
            .map(|n| skip(n, RedundancySignal::SameContent))
            .collect::<Vec<_>>()
    );
    let short = |n: u64| crate::recall::format::short_id(uid(n));
    let label = format!(
        "{EFFICACY} [Observation] (score 0.45, id {}, also {}, also {}, also {}, +2 more)",
        short(1),
        short(2),
        short(3),
        short(4)
    );
    assert!(on.context.starts_with(&label), "{}", on.context);
    assert!(
        !on.context.contains(&short(5)),
        "two are counted, not named"
    );
    // The other hit skipped nothing and renders as before.
    assert!(on.context.contains(&format!(
        "a seventh fact [Logic] (score 0.15, id {})",
        short(7)
    )));
    // The CLI/portal re-render of the detail is the pipeline's block.
    assert_eq!(
        crate::recall::format::render_detailed_block(&on.detailed[0]),
        on.context.split("\n\n").next().unwrap()
    );
    // On the wire: snake_case, only on the hit that skipped.
    let wire = serde_json::to_value(&on).unwrap();
    assert_eq!(
        wire["hits"][0]["skipped_redundant"][0],
        serde_json::json!({"node_id": uid(2).0.to_string(), "signal": "same_content"})
    );
    assert!(wire["hits"][1].get("skipped_redundant").is_none());
    assert_eq!(
        skips_json(&skipped)[4],
        serde_json::json!({"node_id": uid(6).0.to_string(), "signal": "same_content"})
    );
}

/// (11) The mention costs tokens: under a budget that fits two plain
/// blocks, the kept hit's longer label can push the second one out of the
/// rendered context, while both stay in the hits.
#[test]
fn the_mention_counts_against_max_tokens() {
    let f = Fixture::new(&[
        (1, "alpha fact", ConceptType::Logic, None),
        (2, "alpha fact", ConceptType::Observation, None),
        (3, "beta fact", ConceptType::Logic, None),
    ]);
    let ranked = [(1, 0.9), (2, 0.8), (3, 0.7)];
    let off = Run::new(&[(1, 0.9), (3, 0.7)], 2).off().go(&f.g);
    let budget = default_token_count(&off.context);
    let mut run = Run::new(&ranked, 2);
    run.max_tokens = budget;
    let on = run.go(&f.g);
    assert_eq!(ids(&on), vec![uid(1), uid(3)]);
    assert!(on.detailed[0].included_in_context);
    assert!(!on.detailed[1].included_in_context, "{}", on.context);
}

/// (12) Only the first `window(top_k)` ordinary members are checked: with
/// top_k 2 that is eight, so a ninth duplicate takes the second slot.
#[test]
fn only_the_window_is_checked() {
    let rows: Vec<(u64, &str, ConceptType, Option<[f32; 2]>)> = (1..=9)
        .map(|n| (n, EFFICACY, ConceptType::Observation, None))
        .chain([(10, "a distinct fact", ConceptType::Logic, None)])
        .collect();
    let f = Fixture::new(&rows);
    let ranked: Vec<(u64, f64)> = (1..=10).map(|n| (n, 2.0 - n as f64 / 10.0)).collect();
    assert_eq!(window(2), 8);
    let on = Run::new(&ranked, 2).go(&f.g);
    assert_eq!(ids(&on), vec![uid(1), uid(9)]);
    assert_eq!(skipped_for(&on, 1).len(), 7, "members 2..=8");
    // A wider window reaches the distinct member instead.
    let on = Run::new(&ranked, 3).go(&f.g);
    assert_eq!(ids(&on), vec![uid(1), uid(10)], "window(3) = 12");
}

/// The pair bound is per recall, not per pass: a recall with a fresh,
/// unscored phase-1 hit runs `emitted` three times (the reservation pass,
/// the cold-start check and the final pass), and each pair is judged once.
/// Twenty distinct members at `top_k = 20` make 190 pairs a pass, 570 over
/// three passes without the memo.
#[test]
fn the_pair_bound_holds_per_recall_across_passes() {
    let texts: Vec<String> = (0..21)
        .map(|i| format!("distinct fact number {i}"))
        .collect();
    let rows: Vec<(u64, &str, ConceptType, Option<[f32; 2]>)> = texts
        .iter()
        .enumerate()
        .map(|(i, t)| (i as u64 + 1, t.as_str(), ConceptType::Logic, None))
        .collect();
    let f = Fixture::new(&rows);
    // Member 21 is fresh: a phase-1 hit the daemon has not scored yet.
    let scores = ScoreTable {
        epoch: 0,
        ranked: (1..=20)
            .map(|n| Scored::new(uid(n), 1.0 - n as f64 / 100.0))
            .collect(),
    };
    let phase1 = vec![Scored::new(uid(21), 1.0)];
    let expanded = ExpandedSet {
        required: (1..=21).map(|n| Scored::new(uid(n), 0.0)).collect(),
        siblings: Vec::new(),
    };
    let before = SIGNAL_CALLS.with(std::cell::Cell::get);
    let r = assemble_with_legs(
        &f.g,
        &expanded,
        &phase1,
        None,
        &scores,
        &HashMap::new(),
        &RecallQuery {
            query: "distinct fact".into(),
            top_k: 20,
            max_tokens: 100_000,
            traversal_depth: 0,
        },
        RecallWeights::default(),
        crate::Config::default().canonization_edge_min_age,
        true,
        Some(DEFAULT),
        ts(60),
        default_token_count,
    );
    let calls = SIGNAL_CALLS.with(std::cell::Cell::get) - before;
    assert!(r.cold_start, "premise: all three passes ran");
    assert_eq!(r.hits.len(), 20);
    assert!(calls <= PAIR_BUDGET, "{calls} verdicts");
    assert!(calls <= 21 * 20 / 2, "each pair once: {calls} verdicts");
}

/// Canonical-first order is kept: a Canonical member still ranks first,
/// and a duplicate below it is skipped in favour of the higher copy.
#[test]
fn canonical_first_order_is_kept() {
    let mut f = Fixture::new(&[
        (1, EFFICACY, ConceptType::Entity, None),
        (2, EFFICACY, ConceptType::Observation, None),
        (3, "the pillar", ConceptType::Entity, None),
        (4, "a fourth fact", ConceptType::Logic, None),
    ]);
    f.status(3, CanonizationStatus::Canonical);
    let on = Run::new(&[(1, 0.9), (2, 0.8), (3, 0.1), (4, 0.05)], 3).go(&f.g);
    assert_eq!(ids(&on), vec![uid(3), uid(1), uid(4)]);
}
