//! #19(a): opposition never changes a canonization verdict.
//!
//! The shared #36 dependents case carries a contradiction pair between `X`
//! (the pillar) and `Q` (`Q -Contradicts-> X`, `X -Contradicts-> Q`). Every
//! promotion input is computed twice, over the case as written and over the
//! same case with the two contradictions removed, and must agree: the daemon
//! scores and Stage 1 candidates on the graph, the Solo score, and Stage 2
//! and Stage 3 on a store at both edge ages.

use chrono::Duration as ChronoDuration;

use crate::config::ScoringWeights;
use crate::graph::Graph;
use crate::store::{GraphStore, MemoryStore};
use crate::test_util::dependents as case;
use crate::types::{EdgeType, Mutation, MutationBatch, Node, NodeId, ScoreTable};

use super::policy::solo_score;
use super::{dependent_interactions, stage1_candidates, stage2_passes, stage3_passes};

fn is_contradiction(m: &Mutation) -> bool {
    matches!(m, Mutation::UpsertEdge { edge } if edge.edge_type == EdgeType::Contradicts)
}

/// The case's graph, every concept past the Stage 1 survival floor.
fn seasoned(mut g: Graph) -> Graph {
    let ids: Vec<NodeId> = g.concepts().map(|c| c.id).collect();
    for _ in 0..3 {
        g.bump_gc_survived(&ids);
    }
    g
}

fn graphs() -> (Graph, Graph) {
    let with = case::graph();
    let mut without = case::graph();
    let ids: Vec<NodeId> = without
        .edges()
        .filter(|e| e.edge_type == EdgeType::Contradicts)
        .map(|e| e.id)
        .collect();
    assert_eq!(ids.len(), 2, "the case carries both directions");
    for id in ids {
        without.remove_edge(id).unwrap();
    }
    (seasoned(with), seasoned(without))
}

fn concept(g: &Graph, name: &str) -> crate::types::Concept {
    match g.node(case::concept_id(name)) {
        Some(Node::Concept(c)) => c.clone(),
        _ => panic!("{name} is a concept of the case"),
    }
}

#[test]
fn contradictions_change_no_graph_side_canonization_input() {
    let (with, without) = graphs();
    let weights = ScoringWeights::default();
    let table = |g: &Graph| ScoreTable {
        epoch: 0,
        ranked: crate::daemon::score::rescore(g, &weights),
    };
    let (t_with, t_without) = (table(&with), table(&without));
    assert_eq!(t_with.ranked, t_without.ranked, "daemon scores");
    let (age, now) = (case::MIN_AGE, case::now());
    let s1_with = stage1_candidates(&with, &t_with, age, now);
    assert!(!s1_with.is_empty(), "Stage 1 admits someone in this case");
    assert_eq!(s1_with, stage1_candidates(&without, &t_without, age, now));
    for name in ["X", "Q"] {
        let id = case::concept_id(name);
        let (d_with, d_without) = (
            dependent_interactions(&with, id, age, now),
            dependent_interactions(&without, id, age, now),
        );
        assert_eq!(d_with, d_without, "{name} dependent interactions");
        assert_eq!(
            solo_score(&with, &concept(&with, name), d_with),
            solo_score(&without, &concept(&without, name), d_without),
            "{name} Solo score"
        );
    }
}

#[tokio::test]
async fn contradictions_change_no_store_side_stage_verdict() {
    let full = case::store_batch();
    let mut stripped = MutationBatch::new();
    for m in full.mutations.iter().filter(|m| !is_contradiction(m)) {
        stripped.push(m.clone());
    }
    assert_eq!(full.mutations.len(), stripped.mutations.len() + 2);
    let with = MemoryStore::new();
    let without = MemoryStore::new();
    with.flush(&full, None).await.unwrap();
    without.flush(&stripped, None).await.unwrap();

    let sid = case::session();
    let now = case::now();
    let cooldown = ChronoDuration::seconds(300).to_std().unwrap();
    let mut x_passes_stage3 = false;
    for name in ["X", "Q"] {
        let node = case::concept_id(name);
        for age in [case::MIN_AGE, std::time::Duration::ZERO] {
            let stores: [&dyn GraphStore; 2] = [&with, &without];
            let mut seen = Vec::new();
            for store in stores {
                seen.push((
                    store.interaction_span(&sid, node, age, now).await.unwrap(),
                    store.blast_radius(&sid, node, age, now).await.unwrap(),
                    stage2_passes(store, &sid, node, age, now).await.unwrap(),
                    stage3_passes(store, &sid, node, None, age, cooldown, now)
                        .await
                        .unwrap(),
                ));
            }
            assert_eq!(seen[0], seen[1], "{name} at {age:?}");
            x_passes_stage3 |= name == "X" && seen[0].3.is_some();
        }
    }
    assert!(x_passes_stage3, "the pillar clears Stage 3 in this case");
}

/// #19(b): the same, for contradictions written through the product write
/// path (`against`, pinned by the pre-pass and applied by the derive's
/// commit) instead of hand-built rows. Both copies re-derive `Q` and `X` in
/// their own origin interactions, so they differ only by the two
/// `Contradicts` edges `against` wrote; every graph-side and store-side
/// canonization input must agree.
#[tokio::test]
async fn contradictions_written_through_against_change_no_score_or_verdict() {
    use crate::graph::cite;
    use crate::graph::derive::{derive_cited, ParentOf};
    use crate::types::{AgentId, ConceptCitations, PinnedCitations};

    let stripped = || {
        let mut g = case::graph();
        let ids: Vec<NodeId> = g
            .edges()
            .filter(|e| e.edge_type == EdgeType::Contradicts)
            .map(|e| e.id)
            .collect();
        for id in ids {
            g.remove_edge(id).unwrap();
        }
        g
    };
    let rederive = |g: &mut Graph, name: &str, other: &str, contradict: bool| {
        let c = concept(g, name);
        let concepts = [(c.content.as_str(), c.concept_type)];
        let pinned = if contradict {
            let refs = [ConceptCitations {
                content: c.content.clone(),
                depends_on: Vec::new(),
                against: vec![case::concept_id(other).0.to_string()],
            }];
            cite::validate(g, &concepts, &refs).unwrap()
        } else {
            PinnedCitations::default()
        };
        let out = derive_cited(
            g,
            c.origin_interaction,
            &AgentId::from("agent-36"),
            &concepts,
            &ParentOf::none(),
            10,
            &pinned,
        )
        .unwrap();
        assert_eq!(out.matched, vec![c.id], "{name} is re-derived, not created");
        assert_eq!(out.contradicted, usize::from(contradict));
    };
    let mut with = stripped();
    let mut without = stripped();
    for (name, other) in [("Q", "X"), ("X", "Q")] {
        rederive(&mut with, name, other, true);
        rederive(&mut without, name, other, false);
    }
    let pair: Vec<(NodeId, NodeId)> = with
        .edges()
        .filter(|e| e.edge_type == EdgeType::Contradicts)
        .map(|e| (e.source, e.target))
        .collect();
    assert_eq!(pair.len(), 2, "Q <-> X, written by `against`");
    assert!(without
        .edges()
        .all(|e| e.edge_type != EdgeType::Contradicts));

    // Graph side.
    let (with, without) = (seasoned(with), seasoned(without));
    let weights = ScoringWeights::default();
    let table = |g: &Graph| ScoreTable {
        epoch: 0,
        ranked: crate::daemon::score::rescore(g, &weights),
    };
    let (t_with, t_without) = (table(&with), table(&without));
    assert_eq!(t_with.ranked, t_without.ranked, "daemon scores");
    let (age, now) = (case::MIN_AGE, case::now());
    assert_eq!(
        stage1_candidates(&with, &t_with, age, now),
        stage1_candidates(&without, &t_without, age, now)
    );
    for name in ["X", "Q"] {
        let id = case::concept_id(name);
        let (d_with, d_without) = (
            dependent_interactions(&with, id, age, now),
            dependent_interactions(&without, id, age, now),
        );
        assert_eq!(d_with, d_without, "{name} dependent interactions");
        assert_eq!(
            solo_score(&with, &concept(&with, name), d_with),
            solo_score(&without, &concept(&without, name), d_without),
            "{name} Solo score"
        );
        assert_eq!(
            crate::recall::format::blast_radius(&with, id),
            crate::recall::format::blast_radius(&without, id),
            "{name} blast radius"
        );
    }

    // Store side: each graph's whole write log, flushed.
    let (mut with, mut without) = (with, without);
    let with_store = MemoryStore::new();
    let without_store = MemoryStore::new();
    with_store.flush(&with.drain_log(), None).await.unwrap();
    without_store
        .flush(&without.drain_log(), None)
        .await
        .unwrap();
    let sid = case::session();
    let cooldown = ChronoDuration::seconds(300).to_std().unwrap();
    for name in ["X", "Q"] {
        let node = case::concept_id(name);
        for age in [case::MIN_AGE, std::time::Duration::ZERO] {
            let stores: [&dyn GraphStore; 2] = [&with_store, &without_store];
            let mut seen = Vec::new();
            for store in stores {
                seen.push((
                    store.interaction_span(&sid, node, age, now).await.unwrap(),
                    store.blast_radius(&sid, node, age, now).await.unwrap(),
                    stage2_passes(store, &sid, node, age, now).await.unwrap(),
                    stage3_passes(store, &sid, node, None, age, cooldown, now)
                        .await
                        .unwrap(),
                ));
            }
            assert_eq!(seen[0], seen[1], "{name} at {age:?}");
        }
    }
}
