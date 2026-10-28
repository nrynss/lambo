//! #198 part c at hybrid write time: a created concept's vector hits at or
//! above the floor (the merge threshold) that pass the candidate predicate
//! are listed on the outcome, ranked and capped. The list is
//! informational: the write's graph is the same whether or not it lists
//! anything.

use super::*;
use crate::types::PinnedReference;

const SESS: &str = "hybrid-replacement";

fn graph_with_interactions() -> (Arc<RwLock<Graph>>, [NodeId; 3]) {
    graph_with_about_times([None; 3])
}

/// Three interactions written at minutes 0, 10 and 20, each with the given
/// about-time (`event_time`, in minutes) when one is set.
fn graph_with_about_times(about: [Option<i64>; 3]) -> (Arc<RwLock<Graph>>, [NodeId; 3]) {
    let mut g = Graph::new(sid(SESS));
    let mut prev = None;
    let mut ids = [NodeId::default(); 3];
    for (k, at) in [0, 10, 20].into_iter().enumerate() {
        let mut i = interaction(k as u64 + 1, prev, at, "notes");
        i.session_id = sid(SESS);
        i.event_time = about[k].map(ts);
        ids[k] = i.id;
        prev = Some(i.id);
        g.insert_interaction(i).unwrap();
    }
    (Arc::new(RwLock::new(g)), ids)
}

async fn derive_many(
    graph: &Arc<RwLock<Graph>>,
    interaction: NodeId,
    concepts: &[(&str, ConceptType)],
    hits: Vec<Scored<NodeId>>,
    pinned: &PinnedCitations,
) -> DeriveOutcome {
    derive_with(
        graph.clone(),
        VectorCandidates::from_store(&SpyStore::with_vector(hits)),
        &RecordingEmbedder::new(),
        &contract("fixture", 1024),
        interaction,
        &agent(),
        concepts,
        &ParentOf::none(),
        10,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        None,
        pinned,
        None,
    )
    .await
    .unwrap()
}

async fn derive_at(
    graph: &Arc<RwLock<Graph>>,
    interaction: NodeId,
    content: &str,
    ty: ConceptType,
    hits: Vec<Scored<NodeId>>,
) -> DeriveOutcome {
    derive_many(
        graph,
        interaction,
        &[(content, ty)],
        hits,
        &PinnedCitations::default(),
    )
    .await
}

fn listed(out: &DeriveOutcome) -> Vec<(NodeId, f64)> {
    out.replacement_candidates
        .iter()
        .map(|c| (c.id, c.cosine))
        .collect()
}

/// Edges between `a` and `b`, either way, of any type.
fn edges_between(graph: &Arc<RwLock<Graph>>, a: NodeId, b: NodeId) -> usize {
    graph
        .read()
        .edges()
        .filter(|e| (e.source == a && e.target == b) || (e.source == b && e.target == a))
        .count()
}

/// A reworded predecessor (other identifiers, so the guard refuses the
/// merge; more than one span, so the trigger records nothing) is listed,
/// and nothing about the write changes: no edge to it, no merge.
const OLD: &str = "the embedder listens on port 8080";
const REWORDED: &str = "after the clash the embedder moved to port 9090 behind the proxy";

#[tokio::test]
async fn a_reworded_predecessor_is_listed_and_nothing_is_recorded() {
    let (graph, [i0, i1, _]) = graph_with_interactions();
    let old = derive_at(&graph, i0, OLD, ConceptType::Logic, Vec::new())
        .await
        .created[0];
    let out = derive_at(
        &graph,
        i1,
        REWORDED,
        ConceptType::Logic,
        vec![hit(old, 0.88)],
    )
    .await;
    let new = out.created[0];
    assert_eq!(listed(&out), vec![(old, 0.88)]);
    let c = &out.replacement_candidates[0];
    assert_eq!(c.new, new);
    assert_eq!(c.excerpt, OLD);
    assert!(!c.newer, "the predecessor is dated before the write");
    assert_eq!(out.suspected, 0);
    assert!(out.semantic_merged.is_empty());
    assert_eq!(out.merge_guarded.identifiers, 1);
    assert_eq!(edges_between(&graph, new, old), 0, "no edge is recorded");
    graph.read().assert_invariants().unwrap();
}

/// The same write below the floor lists nothing, and its graph is the
/// same shape as the listed one: the list changes nothing it writes.
#[tokio::test]
async fn below_the_floor_lists_nothing_and_writes_the_same() {
    let mut shapes = Vec::new();
    for score in [0.88, 0.84] {
        let (graph, [i0, i1, _]) = graph_with_interactions();
        let old = derive_at(&graph, i0, OLD, ConceptType::Logic, Vec::new())
            .await
            .created[0];
        let _ = graph.write().drain_log();
        let out = derive_at(
            &graph,
            i1,
            REWORDED,
            ConceptType::Logic,
            vec![hit(old, score)],
        )
        .await;
        assert_eq!(out.replacement_candidates.is_empty(), score < 0.85);
        let batch = graph.write().drain_log();
        let kinds: Vec<String> = batch
            .mutations
            .iter()
            .map(|m| format!("{:?}", std::mem::discriminant(m)))
            .collect();
        let mut out = out;
        out.replacement_candidates.clear();
        out.created.clear();
        out.merge_guarded = Default::default();
        shapes.push((kinds, out));
    }
    assert_eq!(shapes[0], shapes[1]);
}

/// A pair the trigger flags is listed too: the writer confirms it.
#[tokio::test]
async fn a_suspected_pair_is_listed_too() {
    let (graph, [i0, i1, _]) = graph_with_interactions();
    let old = derive_at(&graph, i0, OLD, ConceptType::Logic, Vec::new())
        .await
        .created[0];
    let out = derive_at(
        &graph,
        i1,
        "the embedder listens on port 8081",
        ConceptType::Logic,
        vec![hit(old, 0.95)],
    )
    .await;
    assert_eq!(out.suspected, 1);
    assert_eq!(listed(&out), vec![(old, 0.95)]);
}

/// M2: the concept the write merged with is listed, marked merged (a
/// similarity merge declares no replacement: a reworded reversal with no
/// conflicting identifiers merges); another close hit below the merge tier
/// is listed unmarked.
#[tokio::test]
async fn the_merged_with_concept_is_listed_and_marked() {
    let (graph, [i0, i1, i2]) = graph_with_interactions();
    let a = derive_at(
        &graph,
        i0,
        "the retry policy uses exponential backoff",
        ConceptType::Logic,
        Vec::new(),
    )
    .await
    .created[0];
    let b = derive_at(
        &graph,
        i1,
        "the retry policy gives up after the budget",
        ConceptType::Logic,
        Vec::new(),
    )
    .await
    .created[0];
    let out = derive_at(
        &graph,
        i2,
        "the retry policy no longer uses exponential backoff",
        ConceptType::Logic,
        vec![hit(a, 0.95), hit(b, 0.9)],
    )
    .await;
    assert_eq!(out.semantic_merged, vec![a]);
    assert_eq!(listed(&out), vec![(a, 0.95), (b, 0.9)]);
    let merged: Vec<bool> = out
        .replacement_candidates
        .iter()
        .map(|c| c.merged)
        .collect();
    assert_eq!(merged, vec![true, false]);
    graph.read().assert_invariants().unwrap();
}

/// m3: with a tied merge tier, every member is listed and marked merged,
/// not only the one the stable tie-break sent the edge to.
#[tokio::test]
async fn every_tied_merge_target_is_marked_merged() {
    let (graph, [i0, i1, i2]) = graph_with_interactions();
    let mut olds = Vec::new();
    for (i, content) in [
        (i0, "the retry policy uses exponential backoff"),
        (i0, "the retry policy uses linear backoff"),
        (i1, "the retry policy gives up after the budget"),
    ] {
        olds.push(
            derive_at(&graph, i, content, ConceptType::Logic, Vec::new())
                .await
                .created[0],
        );
    }
    let out = derive_at(
        &graph,
        i2,
        "the retry policy now uses capped backoff",
        ConceptType::Logic,
        vec![hit(olds[0], 0.95), hit(olds[1], 0.95), hit(olds[2], 0.9)],
    )
    .await;
    assert_eq!(out.semantic_merged.len(), 1);
    assert!(olds[..2].contains(&out.semantic_merged[0]));
    let mut marks: Vec<(NodeId, bool)> = out
        .replacement_candidates
        .iter()
        .map(|c| (c.id, c.merged))
        .collect();
    marks[..2].sort_by_key(|(id, _)| id.0);
    let mut tier = vec![(olds[0], true), (olds[1], true)];
    tier.sort_by_key(|(id, _)| id.0);
    tier.push((olds[2], false));
    assert_eq!(marks, tier);
}

/// Another type, or an ineligible one, is never listed.
#[tokio::test]
async fn type_conditions_hold_on_the_write_path() {
    use ConceptType::*;
    for (old_ty, new_ty, want) in [
        (Logic, Logic, true),
        (Constraint, Constraint, true),
        (Observation, Observation, true),
        (Constraint, Logic, false),
        (Entity, Entity, false),
        (Resource, Resource, false),
    ] {
        let (graph, [i0, i1, _]) = graph_with_interactions();
        let old = derive_at(&graph, i0, OLD, old_ty, Vec::new()).await.created[0];
        let out = derive_at(&graph, i1, REWORDED, new_ty, vec![hit(old, 0.9)]).await;
        assert_eq!(
            !out.replacement_candidates.is_empty(),
            want,
            "{old_ty:?}/{new_ty:?}"
        );
    }
}

/// A predecessor the write already cites, by `against` or `depends_on`,
/// is not listed: the writer has said what it is.
#[tokio::test]
async fn an_already_cited_predecessor_is_not_listed() {
    for against in [true, false] {
        let (graph, [i0, i1, _]) = graph_with_interactions();
        let old = derive_at(&graph, i0, OLD, ConceptType::Logic, Vec::new())
            .await
            .created[0];
        let reference = PinnedReference {
            content: REWORDED.to_string(),
            reference: old.0.to_string(),
            target: old,
        };
        let pinned = if against {
            PinnedCitations {
                references: Vec::new(),
                against: vec![reference],
            }
        } else {
            PinnedCitations {
                references: vec![reference],
                against: Vec::new(),
            }
        };
        let out = derive_many(
            &graph,
            i1,
            &[(REWORDED, ConceptType::Logic)],
            vec![hit(old, 0.9)],
            &pinned,
        )
        .await;
        assert!(
            out.replacement_candidates.is_empty(),
            "against={against}: {:?}",
            out.replacement_candidates
        );
    }
}

/// Ranked by cosine and capped at three for one concept; each created
/// concept of a call gets its own list, and a concept the same call
/// re-derived is never one.
#[tokio::test]
async fn ranking_cap_and_the_calls_own_concepts() {
    let (graph, [i0, i1, _]) = graph_with_interactions();
    let mut olds = Vec::new();
    for n in 0..5 {
        let content = format!("the embedder listens on port 80{n}0 behind proxy {n}");
        olds.push(
            derive_at(&graph, i0, &content, ConceptType::Logic, Vec::new())
                .await
                .created[0],
        );
    }
    let hits = vec![
        hit(olds[0], 0.86),
        hit(olds[1], 0.93),
        hit(olds[2], 0.88),
        hit(olds[3], 0.91),
        hit(olds[4], 0.87),
    ];
    let out = derive_at(&graph, i1, REWORDED, ConceptType::Logic, hits.clone()).await;
    assert_eq!(
        listed(&out),
        vec![(olds[1], 0.93), (olds[3], 0.91), (olds[2], 0.88)]
    );

    // One call re-deriving olds[1] (a canonical match) beside a new
    // concept: the new one never lists what the same call restated.
    let (graph2, [j0, j1, _]) = graph_with_interactions();
    let mut olds2 = Vec::new();
    for n in 0..2 {
        let content = format!("the embedder listens on port 80{n}0 behind proxy {n}");
        olds2.push(
            derive_at(&graph2, j0, &content, ConceptType::Logic, Vec::new())
                .await
                .created[0],
        );
    }
    let restated = "the embedder listens on port 8000 behind proxy 0";
    let out = derive_many(
        &graph2,
        j1,
        &[
            (restated, ConceptType::Logic),
            (REWORDED, ConceptType::Logic),
        ],
        vec![hit(olds2[0], 0.95), hit(olds2[1], 0.9)],
        &PinnedCitations::default(),
    )
    .await;
    assert_eq!(out.matched, vec![olds2[0]]);
    assert_eq!(listed(&out), vec![(olds2[1], 0.9)]);
}

/// A re-derive of an existing text (a canonical match) writes nothing new
/// and lists nothing.
#[tokio::test]
async fn a_canonical_match_lists_nothing() {
    let (graph, [i0, i1, _]) = graph_with_interactions();
    let old = derive_at(&graph, i0, OLD, ConceptType::Logic, Vec::new())
        .await
        .created[0];
    let out = derive_at(&graph, i1, OLD, ConceptType::Logic, vec![hit(old, 1.0)]).await;
    assert_eq!(out.matched, vec![old]);
    assert!(out.replacement_candidates.is_empty());
}

/// A sync (canonical-strategy) derive has no vectors, so no list.
#[test]
fn a_sync_derive_lists_nothing() {
    let (graph, [i0, i1, _]) = graph_with_interactions();
    let mut g = graph.write();
    for (i, content) in [(i0, OLD), (i1, REWORDED)] {
        let out = crate::graph::derive::derive(
            &mut g,
            i,
            &agent(),
            &[(content, ConceptType::Logic)],
            &ParentOf::none(),
            10,
        )
        .unwrap();
        assert!(out.replacement_candidates.is_empty());
    }
}

/// M1 on the write path: a write backfilled (`event_time`) to before an
/// existing concept still lists it, marked `newer`, since that concept may
/// replace this write; a write at the same about-time is not marked.
#[tokio::test]
async fn a_backfilled_write_marks_the_later_concept_newer() {
    for (about, want) in [(Some(-30), true), (Some(0), false), (None, false)] {
        let (graph, [i0, i1, _]) = graph_with_about_times([None, about, None]);
        let old = derive_at(&graph, i0, OLD, ConceptType::Logic, Vec::new())
            .await
            .created[0];
        let out = derive_at(
            &graph,
            i1,
            REWORDED,
            ConceptType::Logic,
            vec![hit(old, 0.88)],
        )
        .await;
        assert_eq!(listed(&out), vec![(old, 0.88)], "{about:?}");
        assert_eq!(out.replacement_candidates[0].newer, want, "{about:?}");
    }
}
