//! Stage 1 — Candidate (T6.1, spec §10, #36 PR 3).
//!
//! Pure predicate: reports which concepts currently clear Stage 1. It does
//! **not** apply a `None → Candidate` transition (T6.4 owns writes).
//!
//! ## Evidence-led admission (#36 PR 3)
//!
//! A concept passes when both hold:
//!
//! 1. **`gc_survived >= 3`** ([`MIN_GC_SURVIVED`]): it has outlived three GC
//!    sweeps.
//! 2. **At least two other dependent interactions**
//!    ([`MIN_DEPENDENT_INTERACTIONS`]): its aged dependents (#36's
//!    `dependents` relation, [`crate::types::Edge::dependence`]) originate in
//!    at least two distinct interactions **other than the concept's own
//!    origin**, each interaction itself older than the age floor. This is the
//!    same measurement as `distinct` in Stage 2's
//!    [`crate::store::GraphStore::interaction_span_with_window`], with a lower
//!    bar (2 here, 3 at Stage 2).
//!
//! Canonical concepts are never candidates. There is **no peer minimum and no
//! percentile cut**: the spec's "20 non-Canonical peers and strictly above
//! P90" was a quota, and a quota moves with whatever else the session holds.
//! Measured on a rig snapshot (design note `design-36-canonization.md`),
//! replaying recall traffic at k = 0/1/2 moved the P90 Canonical count
//! 68/74/78 and flipped which fair-test ruling reached Venerable; the evidence
//! floor gave the same set every time. A young or small session (fewer than
//! twenty concepts) can now qualify on evidence alone.
//!
//! ## The composite score orders, it does not select
//!
//! [`stage1_candidates`] returns the admitted set **ordered** by the daemon
//! composite (from the caller's [`ScoreTable`]), highest first, then canonical
//! key ascending, then `NodeId` ascending ([`tie_break_by_key`], issue #2).
//! The order only decides which members a bounded cycle takes first
//! (`EvalParams::batch_size`); it never decides membership, so access
//! frequency (#30) can reorder the queue but cannot admit or refuse anyone.
//! A missing, non-finite or negative score entry orders as `0.0` (ALGO-10's
//! `sane_weight` convention, see [`sane_score`]).
//!
//! ## Batched input
//!
//! The dependent-interaction count is computed for every concept in **one
//! pass over the edges** ([`dependent_interaction_counts`]), over the in-RAM
//! graph the cycle already holds under its read guard: no per-node store
//! round-trip, so Stage 1 stays I/O-free (the evaluator's R2-4 property). It
//! follows the stores' Stage 2 rules exactly (session-scoped concepts,
//! edge about-time and interaction about-time at or before `now - min_age`,
//! own origin excluded, a dependent reached twice counted once), and a
//! parity test holds it to `MemoryStore` and SQLite on the shared #36
//! dependents case. [`dependent_interactions`] answers the same question for
//! one node in `O(degree)`, for inspect.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::graph::Graph;
use crate::types::{tie_break_by_key, CanonizationStatus, Edge, Node, NodeId, ScoreTable};

/// Stage 1 survival floor (spec §10).
pub(super) const MIN_GC_SURVIVED: i32 = 3;

/// Stage 1 evidence floor (#36 PR 3): distinct dependent origin interactions,
/// other than the concept's own, that a Candidate needs.
pub(super) const MIN_DEPENDENT_INTERACTIONS: usize = 2;

/// Whether Stage 1's two bars hold for these measurements (`>=` on both).
pub(crate) fn stage1_evidence_passes(gc_survived: i32, dependent_interactions: usize) -> bool {
    gc_survived >= MIN_GC_SURVIVED && dependent_interactions >= MIN_DEPENDENT_INTERACTIONS
}

/// Concepts that currently clear Stage 1, ordered by composite score
/// (highest first), then canonical key, then `NodeId` (see the module docs).
///
/// `min_age` is the age floor Stage 2 uses (`canonization_edge_min_age`):
/// a dependence edge or a dependent's origin interaction younger than
/// `now - min_age` does not count yet.
pub fn stage1_candidates(
    graph: &Graph,
    scores: &ScoreTable,
    min_age: Duration,
    now: DateTime<Utc>,
) -> Vec<NodeId> {
    let counts = dependent_interaction_counts(graph, min_age, now);
    let mut passed: Vec<&crate::types::Concept> = graph
        .concepts()
        .filter(|c| c.canonization_status != CanonizationStatus::Canonical)
        .filter(|c| stage1_evidence_passes(c.gc_survived, counts.get(&c.id).copied().unwrap_or(0)))
        .collect();
    let score_of: HashMap<NodeId, f64> = scores.ranked.iter().map(|s| (s.item, s.score)).collect();
    let daemon_score = |id: NodeId| sane_score(score_of.get(&id).copied().unwrap_or(0.0));
    passed.sort_by(|a, b| {
        daemon_score(b.id)
            .total_cmp(&daemon_score(a.id))
            .then_with(|| {
                tie_break_by_key(Some(&a.canonical_key), &a.id, Some(&b.canonical_key), &b.id)
            })
    });
    passed.into_iter().map(|c| c.id).collect()
}

/// `now - min_age`, or `None` when that instant is not representable (an
/// absurd floor): then nothing is old enough and nothing counts, which is
/// the conservative answer.
fn age_cutoff(min_age: Duration, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let age = chrono::Duration::from_std(min_age).ok()?;
    now.checked_sub_signed(age)
}

/// The `(pillar, interaction)` this edge contributes, if any: the edge
/// declares a dependence ([`Edge::dependence`]), is aged, both ends are
/// concepts of `graph`, and the dependent's origin interaction is in `graph`,
/// aged, and not the pillar's own origin.
fn dependent_origin(graph: &Graph, e: &Edge, cutoff: DateTime<Utc>) -> Option<(NodeId, NodeId)> {
    if e.about_time() > cutoff {
        return None;
    }
    let (pillar, dependent) = e.dependence()?;
    let (Some(Node::Concept(p)), Some(Node::Concept(d))) =
        (graph.node(pillar), graph.node(dependent))
    else {
        return None;
    };
    if d.origin_interaction == p.origin_interaction {
        return None;
    }
    match graph.node(d.origin_interaction) {
        Some(Node::Interaction(i)) if i.about_time() <= cutoff => Some((pillar, i.id)),
        _ => None,
    }
}

/// For every concept with at least one, the number of distinct interactions
/// its aged dependents originate in, other than its own origin (see the
/// module docs). One pass over the edges.
pub fn dependent_interaction_counts(
    graph: &Graph,
    min_age: Duration,
    now: DateTime<Utc>,
) -> HashMap<NodeId, usize> {
    let Some(cutoff) = age_cutoff(min_age, now) else {
        return HashMap::new();
    };
    let mut pairs: HashSet<(NodeId, NodeId)> = HashSet::new();
    for e in graph.edges() {
        if let Some(pair) = dependent_origin(graph, e, cutoff) {
            pairs.insert(pair);
        }
    }
    let mut out: HashMap<NodeId, usize> = HashMap::new();
    for (pillar, _) in pairs {
        *out.entry(pillar).or_insert(0) += 1;
    }
    out
}

/// [`dependent_interaction_counts`] for one node, through the adjacency
/// index (`O(degree)`). Agrees with the batched map for every node.
pub fn dependent_interactions(
    graph: &Graph,
    node: NodeId,
    min_age: Duration,
    now: DateTime<Utc>,
) -> usize {
    let Some(cutoff) = age_cutoff(min_age, now) else {
        return 0;
    };
    graph
        .incident_edges(node)
        .into_iter()
        .filter_map(|e| dependent_origin(graph, e, cutoff))
        .filter(|(pillar, _)| *pillar == node)
        .map(|(_, interaction)| interaction)
        .collect::<HashSet<NodeId>>()
        .len()
}

/// Finite, non-negative score, else `0.0` — ALGO-10's `sane_weight`
/// convention, applied at the point of use, so a NaN can neither jump the
/// queue (`total_cmp` sorts NaN above every real score) nor be dropped.
fn sane_score(score: f64) -> f64 {
    if score.is_finite() && score >= 0.0 {
        score
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "fixtures")]
    use crate::config::ScoringWeights;
    #[cfg(feature = "fixtures")]
    use crate::daemon::score::rescore;
    use crate::types::{AgentId, Concept, ConceptType, EdgeType, Interaction, Scored, SessionId};
    use chrono::{TimeZone, Utc};
    use uuid::Uuid;

    fn now() -> DateTime<Utc> {
        Utc.timestamp_opt(1_752_000_000, 0).unwrap()
    }

    const MIN_AGE: Duration = Duration::from_secs(60);

    fn sid() -> SessionId {
        SessionId::from("test-session")
    }

    fn nid(id: u64) -> NodeId {
        NodeId(Uuid::from_u64_pair(2, id))
    }

    fn iid(id: u64) -> NodeId {
        NodeId(Uuid::from_u64_pair(1, id))
    }

    /// Interaction `id`, `mins` minutes before [`now`].
    fn interaction(id: u64, mins: i64) -> Interaction {
        Interaction {
            event_time: None,
            id: iid(id),
            session_id: sid(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: Some("prompt".into()),
            previous_id: None,
            created_at: now() - chrono::Duration::minutes(mins),
        }
    }

    fn concept(id: u64, origin: u64, gc: i32, status: CanonizationStatus) -> Concept {
        Concept {
            id: nid(id),
            session_id: sid(),
            content: format!("c{id}"),
            canonical_key: format!("c{id}"),
            concept_type: ConceptType::Logic,
            origin_interaction: iid(origin),
            origin_agent: AgentId::from("agent-a"),
            created_at: now() - chrono::Duration::minutes(100),
            access_count: 0,
            last_accessed: None,
            gc_survived: gc,
            canonization_status: status,
            blast_radius: None,
            last_demotion_time: None,
            embedding: None,
            human_confirmed: 0,
            embedding_source: None,
            chunk_group_id: None,
        }
    }

    fn edge(src: u64, tgt: u64, ty: EdgeType, mins: i64) -> Edge {
        let at = now() - chrono::Duration::minutes(mins);
        Edge {
            event_time: None,
            id: NodeId::new(),
            session_id: sid(),
            source: nid(src),
            target: nid(tgt),
            edge_type: ty,
            weight: 1.0,
            reinforcements: 1,
            created_at: at,
            last_reinforced: at,
        }
    }

    /// Five interactions 100..60 minutes back (`i1`..`i5`) and one at `now`
    /// (`i9`, young under [`MIN_AGE`]), plus `concepts`.
    fn graph_with(concepts: impl IntoIterator<Item = Concept>) -> Graph {
        let mut g = Graph::new(sid());
        let mut previous = None;
        for (id, mins) in [(1, 100), (2, 90), (3, 80), (4, 70), (5, 60), (9, 0)] {
            let mut i = interaction(id, mins);
            i.previous_id = previous;
            previous = Some(i.id);
            g.insert_interaction(i).unwrap();
        }
        for c in concepts {
            let origin = c.origin_interaction;
            g.insert_concept(c, origin).unwrap();
        }
        g
    }

    fn table(pairs: &[(u64, f64)]) -> ScoreTable {
        ScoreTable {
            epoch: 0,
            ranked: pairs
                .iter()
                .map(|&(id, score)| Scored::new(nid(id), score))
                .collect(),
        }
    }

    /// Pillar `1` (origin `i1`) with dependents `10 + k` originating in the
    /// given interactions, each citing it with an aged `Dependency` edge.
    fn pillar_with(gc: i32, dependent_origins: &[u64]) -> Graph {
        let mut concepts = vec![concept(1, 1, gc, CanonizationStatus::None)];
        for (k, &origin) in dependent_origins.iter().enumerate() {
            concepts.push(concept(10 + k as u64, origin, 0, CanonizationStatus::None));
        }
        let mut g = graph_with(concepts);
        for k in 0..dependent_origins.len() as u64 {
            g.upsert_edge(edge(10 + k, 1, EdgeType::Dependency, 30))
                .unwrap();
        }
        g
    }

    /// Design test 1: a graph far below the old 20-peer minimum (three
    /// concepts) admits a Candidate on evidence alone. Fails if any peer
    /// minimum survives.
    #[test]
    fn a_graph_with_fewer_than_twenty_peers_qualifies_on_evidence() {
        let g = pillar_with(3, &[2, 3]);
        assert_eq!(g.concepts().count(), 3);
        assert_eq!(
            stage1_candidates(&g, &table(&[]), MIN_AGE, now()),
            vec![nid(1)]
        );
    }

    /// Design test 2, survival half: two other dependent interactions but
    /// only two GC survivals is refused; three passes (`>=`, not `>`).
    #[test]
    fn fewer_than_three_survivals_is_refused() {
        let g = pillar_with(2, &[2, 3]);
        assert!(stage1_candidates(&g, &table(&[]), MIN_AGE, now()).is_empty());
        let g = pillar_with(3, &[2, 3]);
        assert_eq!(
            stage1_candidates(&g, &table(&[]), MIN_AGE, now()),
            vec![nid(1)]
        );
    }

    /// Design test 2, origin half: one other dependent interaction is
    /// refused, however many dependents share it; a dependent minted in the
    /// pillar's own interaction is not another interaction.
    #[test]
    fn fewer_than_two_other_dependent_interactions_is_refused() {
        // Three dependents, all from i2: one interaction.
        let g = pillar_with(9, &[2, 2, 2]);
        assert_eq!(dependent_interactions(&g, nid(1), MIN_AGE, now()), 1);
        assert!(stage1_candidates(&g, &table(&[]), MIN_AGE, now()).is_empty());
        // i2 plus the pillar's own i1: still one *other* interaction.
        let g = pillar_with(9, &[2, 1]);
        assert_eq!(dependent_interactions(&g, nid(1), MIN_AGE, now()), 1);
        assert!(stage1_candidates(&g, &table(&[]), MIN_AGE, now()).is_empty());
        // A young origin (i9, at now) does not count under the floor...
        let g = pillar_with(9, &[2, 9]);
        assert_eq!(dependent_interactions(&g, nid(1), MIN_AGE, now()), 1);
        assert!(stage1_candidates(&g, &table(&[]), MIN_AGE, now()).is_empty());
        // ...and does with no floor.
        assert_eq!(
            stage1_candidates(&g, &table(&[]), Duration::ZERO, now()),
            vec![nid(1)]
        );
    }

    /// A young dependence edge does not count until it ages; neither do
    /// edges that declare no dependence (`Causal`, `Contradicts`,
    /// `CoOccurrence`), nor the wrong direction of `Dependency` (the pillar
    /// depending on something) or of `Hierarchical` (the pillar's parent).
    #[test]
    fn only_aged_declared_dependence_counts() {
        let mut concepts = vec![concept(1, 1, 9, CanonizationStatus::None)];
        for (id, origin) in [(10, 2), (11, 3), (12, 4), (13, 5), (14, 2), (15, 3)] {
            concepts.push(concept(id, origin, 0, CanonizationStatus::None));
        }
        let mut g = graph_with(concepts);
        g.upsert_edge(edge(10, 1, EdgeType::Dependency, 0)).unwrap(); // young
        g.upsert_edge(edge(11, 1, EdgeType::Causal, 30)).unwrap();
        g.upsert_edge(edge(12, 1, EdgeType::Contradicts, 30))
            .unwrap();
        g.upsert_edge(edge(13, 1, EdgeType::CoOccurrence, 30))
            .unwrap();
        g.upsert_edge(edge(1, 14, EdgeType::Dependency, 30))
            .unwrap(); // 1 depends on 14
        g.upsert_edge(edge(15, 1, EdgeType::Hierarchical, 30))
            .unwrap(); // 15 is 1's parent
        assert_eq!(dependent_interactions(&g, nid(1), MIN_AGE, now()), 0);
        assert!(stage1_candidates(&g, &table(&[]), MIN_AGE, now()).is_empty());
        // The young edge counts with no floor, alone: one interaction.
        assert_eq!(dependent_interactions(&g, nid(1), Duration::ZERO, now()), 1);
        // The reversed edges make 14 and 15 the pillars instead.
        let counts = dependent_interaction_counts(&g, MIN_AGE, now());
        assert_eq!(counts.get(&nid(14)), Some(&1));
        assert_eq!(counts.get(&nid(15)), Some(&1));
    }

    /// `Hierarchical` children count like `Dependency` sources, and a
    /// dependent reached by both is one dependent.
    #[test]
    fn hierarchical_children_count_and_double_declarations_count_once() {
        let mut g = pillar_with(3, &[2]);
        g.insert_concept(concept(20, 3, 0, CanonizationStatus::None), iid(3))
            .unwrap();
        g.upsert_edge(edge(1, 20, EdgeType::Hierarchical, 30))
            .unwrap();
        g.upsert_edge(edge(1, 10, EdgeType::Hierarchical, 30))
            .unwrap();
        assert_eq!(dependent_interactions(&g, nid(1), MIN_AGE, now()), 2);
        assert_eq!(
            stage1_candidates(&g, &table(&[]), MIN_AGE, now()),
            vec![nid(1)]
        );
    }

    /// Canonical concepts are never candidates; Candidates and Venerables
    /// that still hold the evidence are reported (the cycle keeps only
    /// still-`None` ones).
    #[test]
    fn canonical_concepts_are_not_candidates() {
        for (status, admitted) in [
            (CanonizationStatus::None, true),
            (CanonizationStatus::Candidate, true),
            (CanonizationStatus::Venerable, true),
            (CanonizationStatus::Canonical, false),
        ] {
            let mut g = pillar_with(3, &[2, 3]);
            if status != CanonizationStatus::None {
                let c = match g.node(nid(1)) {
                    Some(Node::Concept(c)) => c.clone(),
                    _ => unreachable!(),
                };
                g.insert_concept(
                    Concept {
                        canonization_status: status,
                        ..c
                    },
                    iid(1),
                )
                .unwrap();
            }
            let got = stage1_candidates(&g, &table(&[]), MIN_AGE, now());
            assert_eq!(got.contains(&nid(1)), admitted, "{status:?}");
        }
    }

    /// The composite orders, highest first, then canonical key, then id;
    /// a missing or non-finite score orders as zero and never drops a
    /// member.
    #[test]
    fn the_composite_orders_the_admitted_set_and_never_filters_it() {
        let mut concepts = Vec::new();
        for p in 1..=4u64 {
            concepts.push(concept(p, 1, 3, CanonizationStatus::None));
            concepts.push(concept(10 + p, 2, 0, CanonizationStatus::None));
            concepts.push(concept(20 + p, 3, 0, CanonizationStatus::None));
        }
        let mut g = graph_with(concepts);
        for p in 1..=4u64 {
            g.upsert_edge(edge(10 + p, p, EdgeType::Dependency, 30))
                .unwrap();
            g.upsert_edge(edge(20 + p, p, EdgeType::Dependency, 30))
                .unwrap();
        }
        // 4 high, 2 NaN (orders as 0.0), 1 and 3 absent (0.0): ties go to
        // canonical key c1 < c2 < c3.
        let got = stage1_candidates(&g, &table(&[(4, 0.9), (2, f64::NAN)]), MIN_AGE, now());
        assert_eq!(got, vec![nid(4), nid(1), nid(2), nid(3)]);
    }

    /// Design test 3: replayed reads (access traffic, #30) move the composite
    /// and so the order, never the admitted set. Each pillar's score is
    /// recomputed after k replayed accesses on a rotating subset.
    #[cfg(feature = "fixtures")]
    #[test]
    fn replayed_reads_preserve_eligibility() {
        let mut concepts = Vec::new();
        for p in 1..=6u64 {
            concepts.push(concept(p, 1, 3, CanonizationStatus::None));
            // Pillars 1..=3 have two other interactions, 4..=6 only one.
            concepts.push(concept(10 + p, 2, 0, CanonizationStatus::None));
            let second = if p <= 3 { 3 } else { 2 };
            concepts.push(concept(20 + p, second, 0, CanonizationStatus::None));
        }
        let mut g = graph_with(concepts);
        for p in 1..=6u64 {
            g.upsert_edge(edge(10 + p, p, EdgeType::Dependency, 30))
                .unwrap();
            g.upsert_edge(edge(20 + p, p, EdgeType::Dependency, 30))
                .unwrap();
        }
        let weights = ScoringWeights::default();
        let baseline: HashSet<NodeId> = {
            let t = ScoreTable {
                epoch: 0,
                ranked: rescore(&g, &weights),
            };
            stage1_candidates(&g, &t, MIN_AGE, now())
                .into_iter()
                .collect()
        };
        assert_eq!(baseline, [nid(1), nid(2), nid(3)].into_iter().collect());
        for k in 1..=3u32 {
            // Replay k reads per interaction on the refused pillars and on
            // one admitted pillar, the traffic that used to move a P90 cut.
            for p in [2u64, 4, 5, 6] {
                let c = match g.node(nid(p)) {
                    Some(Node::Concept(c)) => c.clone(),
                    _ => unreachable!(),
                };
                g.insert_concept(
                    Concept {
                        access_count: c.access_count + k as i32 * 5,
                        last_accessed: Some(now()),
                        ..c
                    },
                    iid(1),
                )
                .unwrap();
            }
            let t = ScoreTable {
                epoch: 0,
                ranked: rescore(&g, &weights),
            };
            let got: HashSet<NodeId> = stage1_candidates(&g, &t, MIN_AGE, now())
                .into_iter()
                .collect();
            assert_eq!(got, baseline, "k={k}: reads changed eligibility");
        }
    }

    /// The batched map and the single-node walk agree on every node of the
    /// shared #36 dependents case, at both ages.
    #[test]
    fn batched_and_single_node_counts_agree_on_the_dependents_case() {
        let g = crate::test_util::dependents::graph();
        let at = crate::test_util::dependents::now();
        for age in [crate::test_util::dependents::MIN_AGE, Duration::ZERO] {
            let counts = dependent_interaction_counts(&g, age, at);
            for c in g.concepts() {
                assert_eq!(
                    counts.get(&c.id).copied().unwrap_or(0),
                    dependent_interactions(&g, c.id, age, at),
                    "{} at {age:?}",
                    c.content
                );
            }
        }
    }

    /// The graph-side count is the stores' Stage 2 `distinct` (#36 PR 3
    /// parity): every concept of the shared dependents case, at both ages,
    /// on `MemoryStore`. SQLite runs the same comparison in
    /// `store::sqlite::tests::structural`.
    #[cfg(feature = "store-memory")]
    #[tokio::test]
    async fn graph_counts_equal_memory_store_span_distinct() {
        use crate::store::{GraphStore, MemoryStore};
        use crate::test_util::dependents as case;
        let store = MemoryStore::new();
        store.flush(&case::store_batch(), None).await.unwrap();
        let g = case::graph();
        for age in [case::MIN_AGE, Duration::ZERO] {
            let counts = dependent_interaction_counts(&g, age, case::now());
            for c in g.concepts() {
                let span = store
                    .interaction_span(&case::session(), c.id, age, case::now())
                    .await
                    .unwrap();
                assert_eq!(
                    counts.get(&c.id).copied().unwrap_or(0) as u64,
                    span.distinct,
                    "{} at {age:?}",
                    c.content
                );
            }
        }
    }

    /// Review L9: an edge and an interaction exactly at `now - min_age`
    /// count, one second younger do not, on the graph and on MemoryStore.
    #[cfg(feature = "store-memory")]
    #[tokio::test]
    async fn rows_exactly_at_the_cutoff_count_on_graph_and_memory_store() {
        use crate::store::{GraphStore, MemoryStore};
        use crate::test_util::dependents as case;
        let g = case::cutoff_graph();
        let pillar = case::cutoff_pillar();
        let store = MemoryStore::new();
        store.flush(&case::cutoff_batch(), None).await.unwrap();
        for (age, want) in [(case::MIN_AGE, 2usize), (Duration::ZERO, 4)] {
            assert_eq!(dependent_interactions(&g, pillar, age, case::now()), want);
            assert_eq!(
                dependent_interaction_counts(&g, age, case::now()).get(&pillar),
                Some(&want)
            );
            let span = store
                .interaction_span(g.session_id(), pillar, age, case::now())
                .await
                .unwrap();
            assert_eq!(span.distinct, want as u64, "{age:?}");
        }
    }

    /// An unrepresentable age floor counts nothing rather than everything.
    #[test]
    fn an_absurd_age_floor_counts_nothing() {
        let g = pillar_with(3, &[2, 3]);
        let huge = Duration::from_secs(u64::MAX);
        assert!(dependent_interaction_counts(&g, huge, now()).is_empty());
        assert_eq!(dependent_interactions(&g, nid(1), huge, now()), 0);
    }

    /// Fixture composition smoke: planted `user schema` is Canonical, so it
    /// is not a candidate until rewound; rewound, it qualifies on evidence.
    #[cfg(feature = "fixtures")]
    #[test]
    fn rest_api_user_schema_passes_stage1_after_rewind() {
        let mut snap = crate::fixtures::load_snapshot("session-rest-api").unwrap();
        let at = chrono::Utc.with_ymd_and_hms(2026, 8, 10, 10, 0, 0).unwrap();
        let us_id = snap
            .concepts
            .iter()
            .find(|c| c.content == "user schema")
            .expect("user schema present")
            .id;
        {
            let g = Graph::from_snapshot(snap.clone()).unwrap();
            let scores = ScoreTable {
                epoch: g.epoch(),
                ranked: rescore(&g, &ScoringWeights::default()),
            };
            let passed = stage1_candidates(&g, &scores, MIN_AGE, at);
            assert!(
                !passed.contains(&us_id),
                "Canonical user schema: {passed:?}"
            );
        }
        let us = snap
            .concepts
            .iter_mut()
            .find(|c| c.id == us_id)
            .expect("user schema present");
        assert_eq!(us.gc_survived, 4);
        us.canonization_status = CanonizationStatus::None;
        us.blast_radius = None;
        let g = Graph::from_snapshot(snap).unwrap();
        assert_eq!(dependent_interactions(&g, us_id, MIN_AGE, at), 7);
        let scores = ScoreTable {
            epoch: g.epoch(),
            ranked: rescore(&g, &ScoringWeights::default()),
        };
        assert!(stage1_candidates(&g, &scores, MIN_AGE, at).contains(&us_id));
    }
}
