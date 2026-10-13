//! #20: evidence-driven supersession through the real cycle.
//!
//! Every scene is minutes after [`at`]`(0)`; the cycle runs at
//! [`at`]`(300)` with the shipped age floor (60 s) and cooldown (300 s), so
//! the eligibility and cooldown guards are the production ones.

use super::*;
use crate::store::{GraphStore, MemoryStore};
use crate::types::{AgentId, DemotionReason, Mutation, MutationBatch};
use std::sync::Mutex;

fn at(minutes: i64) -> DateTime<Utc> {
    ts() + chrono::Duration::minutes(minutes)
}

fn at_secs(seconds: i64) -> DateTime<Utc> {
    ts() + chrono::Duration::seconds(seconds)
}

const NOW: i64 = 300;
const C: u64 = 1;
const S: u64 = 2;

fn on() -> EvalParams {
    EvalParams {
        supersede: true,
        ..EvalParams::default()
    }
}

/// A session under construction: interactions are collected first and
/// chained in time order at [`Scene::build`].
struct Scene {
    interactions: Vec<(u64, DateTime<Utc>, &'static str)>,
    concepts: Vec<(Concept, u64)>,
    edges: Vec<Edge>,
    next: u64,
}

impl Scene {
    /// `C` (Canonical, origin at minute 0 by `a`) and `S` (None, origin at
    /// minute 10 by `b`).
    fn new() -> Self {
        let mut s = Self {
            interactions: Vec::new(),
            concepts: Vec::new(),
            edges: Vec::new(),
            next: 100,
        };
        let i_c = s.interaction(at(0), "a");
        let i_s = s.interaction(at(10), "b");
        s.concept(C, i_c, CanonizationStatus::Canonical);
        s.concept(S, i_s, CanonizationStatus::None);
        s
    }

    fn id(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    fn interaction(&mut self, when: DateTime<Utc>, agent: &'static str) -> u64 {
        let id = self.id();
        self.interactions.push((id, when, agent));
        id
    }

    fn concept(&mut self, id: u64, origin: u64, status: CanonizationStatus) {
        let when = self
            .interactions
            .iter()
            .find(|(i, _, _)| *i == origin)
            .map(|(_, w, _)| *w)
            .unwrap();
        let mut c = super::concept(id, origin, 0, status);
        c.created_at = when;
        self.concepts.push((c, origin));
    }

    fn concept_mut(&mut self, id: u64) -> &mut Concept {
        &mut self
            .concepts
            .iter_mut()
            .find(|(c, _)| c.id == nid(id))
            .unwrap()
            .0
    }

    fn edge(&mut self, src: u64, tgt: u64, ty: EdgeType, when: DateTime<Utc>) -> NodeId {
        let id = eid(self.id());
        self.edges.push(Edge {
            event_time: None,
            id,
            session_id: sid(),
            source: nid(src),
            target: nid(tgt),
            edge_type: ty,
            weight: 1.0,
            reinforcements: 1,
            created_at: when,
            last_reinforced: when,
        });
        id
    }

    /// `src -Contradicts-> tgt`, declared by `agent` at `when`.
    fn contradicts(
        &mut self,
        src: u64,
        tgt: u64,
        when: DateTime<Utc>,
        agent: &'static str,
    ) -> NodeId {
        self.interaction(when, agent);
        self.edge(src, tgt, EdgeType::Contradicts, when)
    }

    /// A new concept written by `agent` at `when` that depends on `pillar`.
    fn support(&mut self, pillar: u64, when: DateTime<Utc>, agent: &'static str) -> u64 {
        let origin = self.interaction(when, agent);
        let dep = self.id();
        self.concept(dep, origin, CanonizationStatus::None);
        self.edge(dep, pillar, EdgeType::Dependency, when);
        dep
    }

    fn build(mut self) -> Graph {
        let mut g = Graph::new(sid());
        self.interactions.sort_by_key(|(id, w, _)| (*w, *id));
        let mut prev = None;
        for (id, when, agent) in &self.interactions {
            g.insert_interaction(Interaction {
                event_time: None,
                id: iid(*id),
                session_id: sid(),
                agent_id: AgentId::from(*agent),
                prompt_text: Some(format!("i{id}")),
                previous_id: prev,
                created_at: *when,
            })
            .unwrap();
            prev = Some(iid(*id));
        }
        for (c, origin) in self.concepts {
            g.insert_concept(c, iid(origin)).unwrap();
        }
        for e in self.edges {
            g.upsert_edge(e).unwrap();
        }
        g
    }
}

/// The base scene: `C` has evidence at minute 50, `S` contradicts it at 60,
/// and `S` then earns three supports at 100, 150 and 200: newer, from three
/// other interactions, spread over half the session.
fn superseding() -> Scene {
    let mut s = Scene::new();
    s.support(C, at(50), "c");
    s.contradicts(S, C, at(60), "x");
    for (m, agent) in [(100, "d"), (150, "e"), (200, "f")] {
        s.support(S, at(m), agent);
    }
    s
}

async fn store_of(g: &Graph) -> MemoryStore {
    let store = MemoryStore::new();
    let snap = g.snapshot();
    let mut batch = MutationBatch::new();
    for i in snap.interactions {
        batch.push(Mutation::UpsertNode {
            node: Node::Interaction(i),
        });
    }
    for c in snap.concepts {
        batch.push(Mutation::UpsertNode {
            node: Node::Concept(c),
        });
    }
    for e in snap.edges {
        batch.push(Mutation::UpsertEdge { edge: e });
    }
    store.flush(&batch, None).await.unwrap();
    store
}

async fn cycle(
    ev: &mut Evaluator,
    g: &RwLock<Graph>,
    store: &dyn GraphStore,
    params: &EvalParams,
    now: DateTime<Utc>,
) -> EvalOutcome {
    let (tx, _rx) = crate::daemon::events::event_channel();
    eval_cycle(ev, g, store, &table(&[]), &tx, params, now)
        .await
        .unwrap()
}

/// Run one cycle on a fresh store of `scene` and return the graph and outcome.
async fn run(scene: Scene, params: &EvalParams) -> (Graph, EvalOutcome) {
    let g = scene.build();
    let store = store_of(&g).await;
    let g = RwLock::new(g);
    let outcome = cycle(&mut Evaluator::new(), &g, &store, params, at(NOW)).await;
    (g.into_inner(), outcome)
}

fn superseded_by(outcome: &EvalOutcome) -> Vec<(NodeId, NodeId)> {
    outcome
        .supersessions
        .iter()
        .map(|e| match e.reason.clone() {
            Some(DemotionReason::Superseded { by }) => (e.node_id, by),
            other => panic!("a supersession without its reason: {other:?}"),
        })
        .collect()
}

#[tokio::test]
async fn newer_support_for_the_contradicting_concept_demotes_the_canonical_one() {
    let (g, outcome) = run(superseding(), &on()).await;
    assert_eq!(superseded_by(&outcome), vec![(nid(C), nid(S))]);
    let event = &outcome.supersessions[0];
    assert_eq!(event.from_status, CanonizationStatus::Canonical);
    assert_eq!(event.to_status, CanonizationStatus::None);
    assert_eq!(event.last_demotion_time, Some(at(NOW)));
    assert_eq!(event.blast_radius, None);
    assert_eq!(status_of(&g, nid(C)), CanonizationStatus::None);
    assert_eq!(
        stage3::last_demotion_time(&g, nid(C)),
        Some(at(NOW)),
        "the cooldown applies unchanged"
    );
    // S is not promoted by the supersession: it climbs on its own merits.
    assert_eq!(status_of(&g, nid(S)), CanonizationStatus::None);
    assert!(outcome.promotions.iter().all(|e| e.node_id != nid(S)));
    assert_eq!(g.canonization_events().last(), Some(event));
}

#[tokio::test]
async fn default_off_changes_nothing() {
    let (g, outcome) = run(superseding(), &EvalParams::default()).await;
    assert!(outcome.is_empty(), "{outcome:?}");
    assert_eq!(status_of(&g, nid(C)), CanonizationStatus::Canonical);
}

#[tokio::test]
async fn an_assertion_alone_never_demotes() {
    let mut s = Scene::new();
    s.support(C, at(50), "c");
    s.contradicts(S, C, at(260), "x");
    let (g, outcome) = run(s, &on()).await;
    assert!(outcome.supersessions.is_empty());
    assert_eq!(status_of(&g, nid(C)), CanonizationStatus::Canonical);
}

/// S's three supports are older than C's latest evidence (250), so none of
/// them counts, however strong it is in total.
#[tokio::test]
async fn older_support_does_not_count() {
    let mut s = superseding();
    s.support(C, at(250), "g");
    let (g, outcome) = run(s, &on()).await;
    assert!(outcome.supersessions.is_empty());
    assert_eq!(status_of(&g, nid(C)), CanonizationStatus::Canonical);
}

/// The cutoff filters before counting: with C's evidence at 120, only two
/// of S's three supports are newer, and two origins fail Stage 2 although
/// all three would pass it.
#[tokio::test]
async fn the_cutoff_applies_before_origins_are_counted() {
    let mut s = superseding();
    s.support(C, at(120), "g");
    let (g, outcome) = run(s, &on()).await;
    assert!(outcome.supersessions.is_empty());
    assert_eq!(status_of(&g, nid(C)), CanonizationStatus::Canonical);
}

/// Strictly after: support at the watermark's own instant does not count.
#[tokio::test]
async fn support_at_the_watermark_instant_does_not_count() {
    let mut s = superseding();
    s.support(C, at(100), "g");
    let (g, outcome) = run(s, &on()).await;
    assert!(outcome.supersessions.is_empty());
    assert_eq!(status_of(&g, nid(C)), CanonizationStatus::Canonical);
}

/// Support younger than the age floor does not count until it ages in.
#[tokio::test]
async fn fresh_support_counts_only_once_it_is_old_enough() {
    let mut s = Scene::new();
    s.support(C, at(50), "c");
    s.contradicts(S, C, at(60), "x");
    s.support(S, at(100), "d");
    s.support(S, at(200), "e");
    s.support(S, at_secs(NOW * 60 - 30), "f");
    let g = s.build();
    let store = store_of(&g).await;
    let g = RwLock::new(g);
    let mut ev = Evaluator::new();
    let first = cycle(&mut ev, &g, &store, &on(), at(NOW)).await;
    assert!(first.supersessions.is_empty(), "30 s old: not yet evidence");
    let later = cycle(&mut ev, &g, &store, &on(), at(NOW + 1)).await;
    assert_eq!(superseded_by(&later), vec![(nid(C), nid(S))]);
}

/// #19(c) self-suppression: the contradiction's own declarer citing C
/// afterwards does not defend C against it; anyone else's citation does.
#[tokio::test]
async fn the_declarer_cannot_defend_against_its_own_contradiction() {
    let mut s = superseding();
    s.support(C, at(220), "x");
    let (g, outcome) = run(s, &on()).await;
    assert_eq!(superseded_by(&outcome), vec![(nid(C), nid(S))]);
    assert_eq!(status_of(&g, nid(C)), CanonizationStatus::None);

    let mut s = superseding();
    s.support(C, at(220), "y");
    let (g, outcome) = run(s, &on()).await;
    assert!(outcome.supersessions.is_empty());
    assert_eq!(status_of(&g, nid(C)), CanonizationStatus::Canonical);
}

/// S citing C is not C's evidence against S either.
#[tokio::test]
async fn the_contradicting_concept_citing_its_target_is_not_evidence() {
    let mut s = superseding();
    s.edge(S, C, EdgeType::Dependency, at(230));
    s.interaction(at(230), "y");
    let (_, outcome) = run(s, &on()).await;
    assert_eq!(superseded_by(&outcome), vec![(nid(C), nid(S))]);
}

/// Competing sources: one reason per target, the newest eligible evidence
/// first, then the smaller canonical key.
#[tokio::test]
async fn the_source_with_the_newest_evidence_wins() {
    const S2: u64 = 3;
    let scene = |s2_last: i64| {
        let mut s = superseding();
        let origin = s.interaction(at(20), "b2");
        s.concept(S2, origin, CanonizationStatus::None);
        s.contradicts(S2, C, at(70), "x2");
        for (m, agent) in [(110, "d2"), (160, "e2"), (s2_last, "f2")] {
            s.support(S2, at(m), agent);
        }
        s
    };
    let (_, outcome) = run(scene(210), &on()).await;
    assert_eq!(superseded_by(&outcome), vec![(nid(C), nid(S2))]);
    // Tied on the newest evidence: the canonical key decides ("c2" < "c3").
    let (_, outcome) = run(scene(200), &on()).await;
    assert_eq!(superseded_by(&outcome), vec![(nid(C), nid(S))]);
}

#[tokio::test]
async fn a_target_inside_its_cooldown_is_not_superseded_again() {
    let mut s = superseding();
    s.concept_mut(C).last_demotion_time = Some(at(NOW - 2));
    let (g, outcome) = run(s, &on()).await;
    assert!(outcome.supersessions.is_empty());
    assert_eq!(status_of(&g, nid(C)), CanonizationStatus::Canonical);
}

#[tokio::test]
async fn a_source_inside_its_cooldown_cannot_supersede() {
    let mut s = superseding();
    s.concept_mut(S).last_demotion_time = Some(at(NOW - 2));
    let (g, outcome) = run(s, &on()).await;
    assert!(outcome.supersessions.is_empty());
    assert_eq!(status_of(&g, nid(C)), CanonizationStatus::Canonical);
}

/// Review H1/M1: two Canonical concepts contradicting each other, each with
/// newer support the other's declarer did not write, so each qualifies
/// against the other. The pair rule lets only the one with the newest
/// counted evidence act (C's 205 beats S's 200), whatever their ids: C
/// supersedes S, nothing flips inside the cooldown, and S re-climbs and is
/// held at Venerable while C's evidence is newer (inspect says so). Neither
/// is stuck there forever: once S earns newer support from another agent,
/// S is released, promoted, and supersedes C, which is then held in turn.
#[tokio::test]
async fn reciprocal_contradictions_flip_at_most_once_per_cooldown() {
    let mut s = Scene::new();
    s.concept_mut(S).canonization_status = CanonizationStatus::Canonical;
    s.concept_mut(S).gc_survived = 3;
    s.concept_mut(C).gc_survived = 3;
    s.contradicts(S, C, at(60), "q");
    s.contradicts(C, S, at(61), "p");
    // S's support is written by p, C's declarer, so it is not S's evidence
    // against C's contradiction; and the other way round. Six each: blast 6.
    for m in [100, 120, 140, 160, 180, 200] {
        s.support(S, at(m), "p");
        s.support(C, at(m + 5), "q");
    }
    let g = s.build();
    let store = store_of(&g).await;
    let g = RwLock::new(g);
    let mut ev = Evaluator::new();

    let first = cycle(&mut ev, &g, &store, &on(), at(NOW)).await;
    assert_eq!(superseded_by(&first), vec![(nid(S), nid(C))]);
    assert_eq!(status_of(&g.read(), nid(C)), CanonizationStatus::Canonical);

    let inside = cycle(&mut ev, &g, &store, &on(), at(NOW + 1)).await;
    assert!(inside.supersessions.is_empty(), "{inside:?}");

    // S climbs back, and is held at Venerable: C's evidence is newer.
    for m in 2..20 {
        let out = cycle(&mut ev, &g, &store, &on(), at(NOW + m)).await;
        assert!(out.supersessions.is_empty(), "minute {m}: {out:?}");
    }
    assert_eq!(status_of(&g.read(), nid(S)), CanonizationStatus::Venerable);
    assert_eq!(status_of(&g.read(), nid(C)), CanonizationStatus::Canonical);
    let why = crate::canon::explain::explain_node(&g, &store, nid(S), &on(), at(NOW + 20))
        .await
        .unwrap()
        .unwrap();
    assert!(!why.eligible, "{why:?}");
    let held = format!(
        "held: contradicted by {} with newer evidence",
        crate::recall::format::short_id(nid(C))
    );
    assert!(why.reasons.contains(&held), "{:?}", why.reasons);

    // S earns newer support from another agent: released, promoted, and it
    // supersedes C, which is held in turn.
    for (k, m) in [30, 31, 32].into_iter().enumerate() {
        add_support(&g, &store, S, at(NOW + m), 7_000 + k as u64).await;
    }
    let mut flips = Vec::new();
    for m in 34..60 {
        let out = cycle(&mut ev, &g, &store, &on(), at(NOW + m)).await;
        flips.extend(superseded_by(&out));
    }
    assert_eq!(flips, vec![(nid(C), nid(S))]);
    assert_eq!(status_of(&g.read(), nid(S)), CanonizationStatus::Canonical);
    assert_eq!(status_of(&g.read(), nid(C)), CanonizationStatus::Venerable);
}

/// The pair rule decides by evidence, not by id: mirror the scene so the
/// higher-id concept has the newer counted evidence, and it wins.
#[tokio::test]
async fn the_pair_rule_does_not_depend_on_ids() {
    let mut s = Scene::new();
    s.concept_mut(S).canonization_status = CanonizationStatus::Canonical;
    s.contradicts(S, C, at(60), "q");
    s.contradicts(C, S, at(61), "p");
    for m in [100, 150, 200] {
        s.support(S, at(m + 5), "p");
        s.support(C, at(m), "q");
    }
    let (g, out) = run(s, &on()).await;
    assert_eq!(superseded_by(&out), vec![(nid(C), nid(S))]);
    assert_eq!(status_of(&g, nid(S)), CanonizationStatus::Canonical);
}

/// Add, to the graph and the store, a concept written by agent `r` at `when`
/// that depends on `pillar`.
async fn add_support(
    g: &RwLock<Graph>,
    store: &dyn GraphStore,
    pillar: u64,
    when: DateTime<Utc>,
    id: u64,
) {
    let mut batch = MutationBatch::new();
    {
        let mut g = g.write();
        let interaction = Interaction {
            event_time: None,
            id: iid(id),
            session_id: sid(),
            agent_id: AgentId::from("r"),
            prompt_text: Some(format!("i{id}")),
            previous_id: g.temporal_chain().last().copied(),
            created_at: when,
        };
        g.insert_interaction(interaction.clone()).unwrap();
        let mut dep = super::concept(id + 1, id, 0, CanonizationStatus::None);
        dep.created_at = when;
        g.insert_concept(dep.clone(), iid(id)).unwrap();
        let edge = Edge {
            event_time: None,
            id: eid(id + 2),
            session_id: sid(),
            source: nid(id + 1),
            target: nid(pillar),
            edge_type: EdgeType::Dependency,
            weight: 1.0,
            reinforcements: 1,
            created_at: when,
            last_reinforced: when,
        };
        g.upsert_edge(edge.clone()).unwrap();
        batch.push(Mutation::UpsertNode {
            node: Node::Interaction(interaction),
        });
        batch.push(Mutation::UpsertNode {
            node: Node::Concept(dep),
        });
        batch.push(Mutation::UpsertEdge { edge });
    }
    store.flush(&batch, None).await.unwrap();
}

/// A full budget: supersession frees C's slot, the same cycle's Stage 3
/// spends it on V, and the outcome and the audit follow the commit order.
/// With supersession off, Stage 3 is not even gathered.
#[tokio::test]
async fn a_freed_slot_is_spent_in_the_same_cycle_in_commit_order() {
    const V: u64 = 4;
    let scene = || {
        let mut s = superseding();
        let origin = s.interaction(at(5), "v");
        s.concept(V, origin, CanonizationStatus::Venerable);
        // Six children from three other interactions: blast 6 and Stage 2.
        for (k, m) in [30, 130, 230, 31, 131, 231].into_iter().enumerate() {
            let child_origin = s.interaction(at(m), "w");
            let child = 500 + k as u64;
            s.concept(child, child_origin, CanonizationStatus::None);
            s.edge(V, child, EdgeType::Hierarchical, at(m));
        }
        s
    };
    let full = EvalParams {
        max_canonical_nodes: 1,
        ..on()
    };
    let g = scene().build();
    let store = Recording::new(store_of(&g).await);
    let g = RwLock::new(g);
    let outcome = cycle(&mut Evaluator::new(), &g, &store, &full, at(NOW)).await;
    assert_eq!(superseded_by(&outcome), vec![(nid(C), nid(S))]);
    assert_eq!(outcome.stage3_batch, vec![nid(V)]);
    let promoted: Vec<_> = outcome.promotions.iter().map(|e| e.node_id).collect();
    assert_eq!(promoted, vec![nid(V)]);
    assert!(outcome.demotions.is_empty());
    let order: Vec<_> = outcome.transitions().map(|e| e.node_id).collect();
    assert_eq!(order, vec![nid(C), nid(V)]);
    // Review M5: the ids sort in commit order, so a reload, which orders one
    // instant's rows by id, keeps it.
    let ids: Vec<_> = outcome.transitions().map(|e| e.id.0).collect();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted);
    let mut reloaded = store
        .inner
        .load_session(&sid())
        .await
        .unwrap()
        .canonization_events;
    reloaded.sort_by(|a, b| (a.occurred_at, a.id.0).cmp(&(b.occurred_at, b.id.0)));
    let reloaded: Vec<_> = reloaded
        .iter()
        .filter(|e| e.occurred_at == at(NOW))
        .map(|e| e.node_id)
        .collect();
    assert_eq!(reloaded, order);
    assert_eq!(
        store.recorded(),
        order,
        "the audit is written in commit order"
    );
    let audit: Vec<_> = g
        .read()
        .canonization_events()
        .iter()
        .map(|e| e.node_id)
        .collect();
    assert_eq!(audit, order);

    let (g, outcome) = run(
        scene(),
        &EvalParams {
            supersede: false,
            ..full
        },
    )
    .await;
    assert!(outcome.is_empty(), "{outcome:?}");
    assert_eq!(status_of(&g, nid(V)), CanonizationStatus::Venerable);
}

/// The hold: a Venerable that a contradicting concept would supersede at
/// once is not promoted (it would flap: promoted, superseded, back after
/// its cooldown, superseded again). Off, it promotes as before; with newer
/// evidence of its own than the contradiction's support, it promotes too.
#[tokio::test]
async fn a_venerable_that_would_be_superseded_is_held_at_venerable() {
    let scene = |late_own_evidence: bool| {
        let mut s = superseding();
        s.concept_mut(C).canonization_status = CanonizationStatus::Venerable;
        // Six children from six interactions, 75 minutes of spread: Stage 2
        // and blast 6 for C on their own.
        for (k, m) in [5, 15, 25, 35, 70, 80].into_iter().enumerate() {
            let origin = s.interaction(at(m), "w");
            let child = 600 + k as u64;
            s.concept(child, origin, CanonizationStatus::None);
            s.edge(C, child, EdgeType::Hierarchical, at(m));
        }
        if late_own_evidence {
            s.support(C, at(210), "z");
        }
        s
    };
    let (g, outcome) = run(scene(false), &on()).await;
    assert_eq!(outcome.stage3_batch, vec![nid(C)], "evaluated");
    assert!(outcome.promotions.is_empty(), "{outcome:?}");
    assert_eq!(status_of(&g, nid(C)), CanonizationStatus::Venerable);

    let (g, _) = run(scene(false), &EvalParams::default()).await;
    assert_eq!(status_of(&g, nid(C)), CanonizationStatus::Canonical);

    let (g, _) = run(scene(true), &on()).await;
    assert_eq!(status_of(&g, nid(C)), CanonizationStatus::Canonical);
}

/// A just-superseded concept takes no other hop in the same cycle, even
/// when Stage 1 would admit it (it was Canonical when the plan was read).
#[tokio::test]
async fn a_superseded_concept_is_not_re_promoted_in_the_same_cycle() {
    let mut s = superseding();
    s.concept_mut(C).gc_survived = 5;
    s.support(C, at(20), "h");
    let (g, outcome) = run(s, &on()).await;
    assert_eq!(superseded_by(&outcome), vec![(nid(C), nid(S))]);
    assert!(outcome.promotions.iter().all(|e| e.node_id != nid(C)));
    assert_eq!(status_of(&g, nid(C)), CanonizationStatus::None);
}

/// The store was probed with no lock held: a graph that changed meanwhile
/// is re-checked under the write guard, and the supersession is skipped
/// when C gained newer evidence, the contradiction was removed, S was
/// collected, or C already left Canonical.
#[tokio::test]
async fn apply_rechecks_a_graph_that_changed_since_the_probe() {
    type Change = fn(&mut Graph);
    let changes: [(&str, Change); 4] = [
        ("newer evidence for C", |g| {
            g.insert_interaction(Interaction {
                event_time: None,
                id: iid(9_001),
                session_id: sid(),
                agent_id: AgentId::from("late"),
                prompt_text: None,
                previous_id: g.temporal_chain().last().copied(),
                created_at: at(250),
            })
            .unwrap();
            let mut dep = super::concept(9_002, 9_001, 0, CanonizationStatus::None);
            dep.created_at = at(250);
            g.insert_concept(dep, iid(9_001)).unwrap();
            g.upsert_edge(Edge {
                event_time: None,
                id: eid(9_003),
                session_id: sid(),
                source: nid(9_002),
                target: nid(C),
                edge_type: EdgeType::Dependency,
                weight: 1.0,
                reinforcements: 1,
                created_at: at(250),
                last_reinforced: at(250),
            })
            .unwrap();
        }),
        ("the contradiction removed", |g| {
            let id = g
                .edge_between(nid(S), nid(C), EdgeType::Contradicts)
                .unwrap()
                .id;
            g.remove_edge(id).unwrap();
        }),
        ("S collected", |g| g.remove_node(nid(S)).unwrap()),
        ("C demoted meanwhile", |g| {
            g.apply_canonization_transition(CanonizationEvent {
                id: NodeId::new(),
                session_id: sid(),
                node_id: nid(C),
                from_status: CanonizationStatus::Canonical,
                to_status: CanonizationStatus::None,
                blast_radius: None,
                last_demotion_time: Some(at(NOW)),
                occurred_at: at(NOW),
                reason: None,
            })
            .unwrap();
        }),
    ];
    for (what, change) in changes {
        let g = superseding().build();
        let store = store_of(&g).await;
        let mut ev = Evaluator::new();
        let plan = ev.gather(&g, &table(&[]), &on(), at(NOW));
        assert_eq!(plan.supersede.list.len(), 1, "{what}");
        let verdicts = verdicts(&store, &plan, &on(), at(NOW)).await.unwrap();
        assert_eq!(verdicts.supersede.len(), 1, "{what}: the probe passed");
        let mut g = g;
        change(&mut g);
        let (tx, _rx) = crate::daemon::events::event_channel();
        let mut outcome = EvalOutcome::default();
        apply(
            &mut g,
            &tx,
            &plan,
            &verdicts,
            &on(),
            at(NOW),
            &Default::default(),
            &mut outcome,
        )
        .unwrap();
        assert!(outcome.supersessions.is_empty(), "{what}: {outcome:?}");
    }
}

/// A store without the two accessors skips supersession for the cycle with
/// one warning; the cycle itself succeeds.
#[tokio::test]
async fn a_store_without_the_accessors_skips_supersession_with_one_warning() {
    let (logs, _guard) = crate::test_util::capture_logs(tracing::Level::WARN);
    let g = superseding().build();
    let store = Recording::without_accessors(store_of(&g).await);
    let g = RwLock::new(g);
    let outcome = cycle(&mut Evaluator::new(), &g, &store, &on(), at(NOW)).await;
    assert!(outcome.supersessions.is_empty());
    assert_eq!(status_of(&g.read(), nid(C)), CanonizationStatus::Canonical);
    let out = logs.contents();
    assert_eq!(
        out.matches("CanonizationSupersessionUnsupported").count(),
        1,
        "{out}"
    );
}

/// Fencing and retry: the audit write carries the holder's token; a failed
/// write still hands back the committed supersession, whose transition is
/// in the write-behind log, and replaying that log (as the flush task does)
/// records it once; a reload then reads the reason back.
#[tokio::test]
async fn a_failed_audit_write_keeps_the_supersession_and_the_flush_records_it() {
    let g = superseding().build();
    let inner = store_of(&g).await;
    let store = Recording::failing(inner);
    let g = RwLock::new(g);
    g.write().drain_log();
    let (tx, _rx) = crate::daemon::events::event_channel();
    let err = Evaluator::new()
        .eval_cycle(&g, &store, &table(&[]), &tx, &on(), at(NOW), Some(7))
        .await
        .expect_err("the audit write fails");
    assert_eq!(superseded_by(&err.outcome), vec![(nid(C), nid(S))]);
    assert_eq!(
        store.tokens(),
        vec![Some(7)],
        "the holder's token is presented"
    );
    assert_eq!(status_of(&g.read(), nid(C)), CanonizationStatus::None);

    let log = g.write().drain_log();
    for _ in 0..2 {
        store.inner.flush(&log, None).await.unwrap();
    }
    let snap = store.inner.load_session(&sid()).await.unwrap();
    let events: Vec<_> = snap
        .canonization_events
        .iter()
        .filter(|e| e.node_id == nid(C))
        .collect();
    assert_eq!(events.len(), 1, "replayed once");
    assert_eq!(
        events[0].demotion_reason(),
        Some(DemotionReason::Superseded { by: nid(S) })
    );
    let c = snap.concepts.iter().find(|c| c.id == nid(C)).unwrap();
    assert_eq!(c.canonization_status, CanonizationStatus::None);
    assert_eq!(c.last_demotion_time, Some(at(NOW)));
}

/// `MemoryStore` that records the audit writes and can fail them or answer
/// the supersession accessors as a store that never implemented them.
struct Recording {
    inner: MemoryStore,
    fail_record: bool,
    accessors: bool,
    recorded: Mutex<Vec<(NodeId, Option<u64>)>>,
    /// `dependent_evidence` calls per node (review M4).
    evidence_calls: Mutex<std::collections::HashMap<NodeId, usize>>,
    /// Stage 2/3 reads: window-aware span and blast radius calls.
    stage_reads: std::sync::atomic::AtomicUsize,
}

impl Recording {
    fn new(inner: MemoryStore) -> Self {
        Self {
            inner,
            fail_record: false,
            accessors: true,
            recorded: Mutex::new(Vec::new()),
            evidence_calls: Mutex::new(std::collections::HashMap::new()),
            stage_reads: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn failing(inner: MemoryStore) -> Self {
        Self {
            fail_record: true,
            ..Self::new(inner)
        }
    }

    fn without_accessors(inner: MemoryStore) -> Self {
        Self {
            accessors: false,
            ..Self::new(inner)
        }
    }

    fn recorded(&self) -> Vec<NodeId> {
        self.recorded.lock().unwrap().iter().map(|r| r.0).collect()
    }

    fn tokens(&self) -> Vec<Option<u64>> {
        self.recorded.lock().unwrap().iter().map(|r| r.1).collect()
    }
}

#[async_trait::async_trait]
impl GraphStore for Recording {
    async fn init_schema(&self) -> Result<(), StoreError> {
        self.inner.init_schema().await
    }
    fn capabilities(&self) -> crate::store::Capabilities {
        self.inner.capabilities()
    }
    async fn flush(&self, batch: &MutationBatch, token: Option<u64>) -> Result<(), StoreError> {
        self.inner.flush(batch, token).await
    }
    async fn load_session(
        &self,
        session: &SessionId,
    ) -> Result<crate::types::GraphSnapshot, StoreError> {
        self.inner.load_session(session).await
    }
    async fn keyword_candidates(
        &self,
        session: &SessionId,
        tokens: &[String],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.inner.keyword_candidates(session, tokens, limit).await
    }
    async fn vector_candidates(
        &self,
        session: &SessionId,
        embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.inner
            .vector_candidates(session, embedding, limit)
            .await
    }
    async fn blast_radius(
        &self,
        session: &SessionId,
        node: NodeId,
        min_edge_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<u64, StoreError> {
        self.stage_reads
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner
            .blast_radius(session, node, min_edge_age, now)
            .await
    }
    async fn interaction_span(
        &self,
        session: &SessionId,
        node: NodeId,
        min_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<crate::types::InteractionSpan, StoreError> {
        self.inner
            .interaction_span(session, node, min_age, now)
            .await
    }
    async fn interaction_span_with_window(
        &self,
        session: &SessionId,
        node: NodeId,
        min_age: Duration,
        now: DateTime<Utc>,
        window: Duration,
    ) -> Result<crate::types::InteractionSpan, StoreError> {
        self.stage_reads
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner
            .interaction_span_with_window(session, node, min_age, now, window)
            .await
    }
    async fn dependent_evidence(
        &self,
        session: &SessionId,
        node: NodeId,
    ) -> Result<crate::store::DependentEvidence, StoreError> {
        *self.evidence_calls.lock().unwrap().entry(node).or_default() += 1;
        if !self.accessors {
            return Err(StoreError::Capability("dependent_evidence".into()));
        }
        self.inner.dependent_evidence(session, node).await
    }
    async fn record_canonization(
        &self,
        event: &CanonizationEvent,
        token: Option<u64>,
    ) -> Result<(), StoreError> {
        self.recorded.lock().unwrap().push((event.node_id, token));
        if self.fail_record {
            return Err(StoreError::Backend("record_canonization is down".into()));
        }
        self.inner.record_canonization(event, token).await
    }
}

/// Review M5: event ids are time-ordered and sequenced within a cycle, past
/// the 12-bit sequence too.
#[test]
fn event_ids_sort_in_mint_order() {
    let mut ids = EventIds::new(at(NOW));
    let minted: Vec<_> = (0..5_000).map(|_| ids.next().0).collect();
    let mut sorted = minted.clone();
    sorted.sort();
    assert_eq!(minted, sorted);
    assert_eq!(
        minted
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        5_000
    );
    let later = EventIds::new(at(NOW + 1)).next().0;
    assert!(later > *minted.last().unwrap());
}

/// Review M4: at a full budget with a contradiction that does not pass,
/// supersession is decided first and Stage 3's window is not read at all
/// (no span or blast read, nothing evaluated, the ring does not move); and
/// each node's evidence rows are read once per cycle, even when two sources
/// share a target.
#[tokio::test]
async fn store_reads_are_bounded_and_skipped_when_nothing_frees_a_slot() {
    const S2: u64 = 3;
    const V: u64 = 4;
    let mut s = Scene::new();
    s.support(C, at(50), "c");
    s.contradicts(S, C, at(60), "x");
    let origin = s.interaction(at(20), "b2");
    s.concept(S2, origin, CanonizationStatus::None);
    s.contradicts(S2, C, at(70), "x2");
    let origin = s.interaction(at(5), "v");
    s.concept(V, origin, CanonizationStatus::Venerable);
    for (k, m) in [30, 130, 230, 31, 131, 231].into_iter().enumerate() {
        let child_origin = s.interaction(at(m), "w");
        let child = 500 + k as u64;
        s.concept(child, child_origin, CanonizationStatus::None);
        s.edge(V, child, EdgeType::Hierarchical, at(m));
    }
    let g = s.build();
    let store = Recording::new(store_of(&g).await);
    let g = RwLock::new(g);
    let full = EvalParams {
        max_canonical_nodes: 1,
        ..on()
    };
    let mut ev = Evaluator::new();
    let cursor = ev.stage3_cursor();
    let out = cycle(&mut ev, &g, &store, &full, at(NOW)).await;
    assert!(out.is_empty(), "{out:?}");
    assert!(
        out.stage3_batch.is_empty(),
        "nothing in the window was evaluated"
    );
    assert_eq!(ev.stage3_cursor(), cursor, "the ring did not move");
    assert_eq!(
        store.stage_reads.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no Stage 2/3 store read"
    );
    let calls = store.evidence_calls.lock().unwrap().clone();
    assert_eq!(calls.get(&nid(C)), Some(&1), "{calls:?}");
    assert!(calls.values().all(|&n| n == 1), "{calls:?}");
    assert_eq!(calls.len(), 3, "C, S and S2: {calls:?}");
}

/// Review L4: when the contradiction's declarer cannot be told apart from
/// another agent acting at the same instant, the pair is not probed, so an
/// ambiguity never demotes; the same scene with one agent at that instant
/// supersedes.
#[tokio::test]
async fn an_ambiguous_declarer_is_not_probed() {
    let mut s = superseding();
    s.interaction(at(60), "y");
    let (g, out) = run(s, &on()).await;
    assert!(out.supersessions.is_empty(), "{out:?}");
    assert_eq!(status_of(&g, nid(C)), CanonizationStatus::Canonical);
    // A piece of C's evidence that may or may not be the declarer's (x and
    // y both acting at 220): not excluded, not counted, not probed.
    let mut s = superseding();
    s.support(C, at(220), "x");
    s.interaction(at(220), "y");
    let (g, out) = run(s, &on()).await;
    assert!(out.supersessions.is_empty(), "{out:?}");
    assert_eq!(status_of(&g, nid(C)), CanonizationStatus::Canonical);
}
