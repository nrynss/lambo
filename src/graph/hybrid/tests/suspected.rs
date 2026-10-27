//! #198: the suspected-supersession trigger at hybrid write time. A vector
//! hit at or above the threshold that the identifier guard refused becomes
//! a `SuspectedSupersedes` edge, newer -> older, when the two are the same
//! eligible type, differ by one value-slot span and are dated apart. Every
//! negative leaves the graph without the edge and changes nothing else.

use super::*;
use crate::types::PinnedReference;

const SESS: &str = "hybrid-suspected";

/// A graph whose interactions are at minutes 0, 10 and 20 (ids 1..=3),
/// chained.
fn graph_with_interactions() -> (Arc<RwLock<Graph>>, [NodeId; 3]) {
    let mut g = Graph::new(sid(SESS));
    let mut prev = None;
    let mut ids = [NodeId::default(); 3];
    for (k, at) in [0, 10, 20].into_iter().enumerate() {
        let mut i = interaction(k as u64 + 1, prev, at, "notes");
        i.session_id = sid(SESS);
        ids[k] = i.id;
        prev = Some(i.id);
        g.insert_interaction(i).unwrap();
    }
    (Arc::new(RwLock::new(g)), ids)
}

#[allow(clippy::too_many_arguments)]
async fn derive_typed(
    graph: &Arc<RwLock<Graph>>,
    interaction: NodeId,
    content: &str,
    ty: ConceptType,
    hits: Vec<Scored<NodeId>>,
    pinned: &PinnedCitations,
    threshold: f64,
) -> DeriveOutcome {
    derive_with(
        graph.clone(),
        VectorCandidates::from_store(&SpyStore::with_vector(hits)),
        &RecordingEmbedder::new(),
        &contract("fixture", 1024),
        interaction,
        &agent(),
        &[(content, ty)],
        &ParentOf::none(),
        10,
        threshold,
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
    hits: Vec<Scored<NodeId>>,
) -> DeriveOutcome {
    derive_typed(
        graph,
        interaction,
        content,
        ConceptType::Logic,
        hits,
        &PinnedCitations::default(),
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
    )
    .await
}

fn suspected(graph: &Arc<RwLock<Graph>>, newer: NodeId, older: NodeId) -> bool {
    graph
        .read()
        .edge_between(newer, older, EdgeType::SuspectedSupersedes)
        .is_some()
}

fn suspected_edges(graph: &Arc<RwLock<Graph>>) -> usize {
    graph
        .read()
        .edges()
        .filter(|e| e.edge_type == EdgeType::SuspectedSupersedes)
        .count()
}

const OLD: &str = "the embedder listens on port 8080";
const NEW: &str = "the embedder listens on port 8081";

/// The positive case: a later write that changes the value after the
/// predicate gets the edge newer -> older, counted on the outcome, and the
/// write is otherwise the guard's fresh arm (no merge, vector kept).
#[tokio::test]
async fn a_new_value_of_an_older_fact_is_suspected() {
    let (graph, [i0, i1, _]) = graph_with_interactions();
    let old = derive_at(&graph, i0, OLD, Vec::new()).await.created[0];
    let out = derive_at(&graph, i1, NEW, vec![hit(old, 0.95)]).await;
    let new = out.created[0];
    assert!(suspected(&graph, new, old));
    assert!(!suspected(&graph, old, new));
    assert_eq!(out.suspected, 1);
    assert_eq!(out.merge_guarded.identifiers, 1);
    assert!(out.semantic_merged.is_empty());
    assert_eq!(out.embedded, 1);
    graph.read().assert_invariants().unwrap();
    // Re-deriving the same text matches the concept and flags nothing new.
    let again = derive_at(&graph, i1, NEW, vec![hit(old, 0.95)]).await;
    assert_eq!(again.suspected, 0);
    assert_eq!(suspected_edges(&graph), 1);
}

/// The edge rides the write's own mutation batch: it is in the drained log
/// after both concepts, with the write's timestamps.
#[tokio::test]
async fn the_edge_is_written_with_the_write() {
    let (graph, [i0, i1, _]) = graph_with_interactions();
    let old = derive_at(&graph, i0, OLD, Vec::new()).await.created[0];
    let _ = graph.write().drain_log();
    let new = derive_at(&graph, i1, NEW, vec![hit(old, 0.95)])
        .await
        .created[0];
    let batch = graph.write().drain_log();
    let edge = batch
        .mutations
        .iter()
        .find_map(|m| match m {
            crate::types::Mutation::UpsertEdge { edge }
                if edge.edge_type == EdgeType::SuspectedSupersedes =>
            {
                Some(edge.clone())
            }
            _ => None,
        })
        .expect("the edge is in the write's batch");
    assert_eq!((edge.source, edge.target), (new, old));
    assert_eq!(edge.created_at, ts(10));
    assert_eq!(edge.event_time, None);
}

/// Negatives. Each is a pair the trigger must leave alone; the write still
/// takes the guard's fresh arm.
#[tokio::test]
async fn pairs_the_trigger_leaves_alone() {
    // (old, new, why)
    let cases = [
        // The change is in the subject position: two different things.
        (
            "J1 round-1 verdict: approve with two findings",
            "J2 round-1 verdict: approve with two findings",
            "subject slot",
        ),
        (
            "J3-round1.md review: two findings",
            "J3-round3.md review: two findings",
            "subject slot (path)",
        ),
        // Two numbers change at once: two spans.
        (
            "The #190 queries split 239 dev / 129 held-out.",
            "The #190 queries split 199 dev / 111 held-out.",
            "two spans",
        ),
        // No predicate cue at all.
        ("release v0.5.0 notes", "release v0.6.0 notes", "no cue"),
    ];
    for (old_text, new_text, why) in cases {
        let (graph, [i0, i1, _]) = graph_with_interactions();
        let old = derive_at(&graph, i0, old_text, Vec::new()).await.created[0];
        let out = derive_at(&graph, i1, new_text, vec![hit(old, 0.95)]).await;
        assert_eq!(out.merge_guarded.identifiers, 1, "{why}: guard refused");
        assert_eq!(out.suspected, 0, "{why}");
        assert_eq!(suspected_edges(&graph), 0, "{why}");
    }
}

/// Different types, and the excluded types (Resource, Entity) even when
/// both ends share one.
#[tokio::test]
async fn type_conditions() {
    for (old_ty, new_ty, flagged) in [
        (ConceptType::Logic, ConceptType::Observation, false),
        (ConceptType::Resource, ConceptType::Resource, false),
        (ConceptType::Entity, ConceptType::Entity, false),
        (ConceptType::Constraint, ConceptType::Constraint, true),
        (ConceptType::Observation, ConceptType::Observation, true),
    ] {
        let (graph, [i0, i1, _]) = graph_with_interactions();
        let pinned = PinnedCitations::default();
        let t = SEMANTIC_MATCH_THRESHOLD_DEFAULT;
        let old = derive_typed(&graph, i0, OLD, old_ty, Vec::new(), &pinned, t)
            .await
            .created[0];
        let out = derive_typed(&graph, i1, NEW, new_ty, vec![hit(old, 0.95)], &pinned, t).await;
        assert_eq!(out.suspected, usize::from(flagged), "{old_ty:?}/{new_ty:?}");
        assert_eq!(
            suspected_edges(&graph),
            usize::from(flagged),
            "{old_ty:?}/{new_ty:?}"
        );
    }
}

/// Below the threshold the hit is no merge candidate and no suspicion; at
/// exactly the threshold it is. The threshold is the one the write ran
/// under (a calibrated threshold is just a different value here).
#[tokio::test]
async fn only_hits_at_or_above_the_threshold() {
    for (score, threshold, flagged) in [
        (0.84, 0.85, false),
        (0.85, 0.85, true),
        (0.92, 0.93, false),
        (0.93, 0.93, true),
    ] {
        let (graph, [i0, i1, _]) = graph_with_interactions();
        let pinned = PinnedCitations::default();
        let ty = ConceptType::Logic;
        let old = derive_typed(&graph, i0, OLD, ty, Vec::new(), &pinned, threshold)
            .await
            .created[0];
        let out = derive_typed(
            &graph,
            i1,
            NEW,
            ty,
            vec![hit(old, score)],
            &pinned,
            threshold,
        )
        .await;
        assert_eq!(
            out.suspected,
            usize::from(flagged),
            "{score} at {threshold}"
        );
    }
}

/// A hit the identifier guard does not refuse is a paraphrase: it merges,
/// and nothing is suspected.
#[tokio::test]
async fn a_hit_the_guard_accepts_is_not_suspected() {
    let (graph, [i0, i1, _]) = graph_with_interactions();
    let old = derive_at(&graph, i0, OLD, Vec::new()).await.created[0];
    let out = derive_at(
        &graph,
        i1,
        "port 8080 is where the embedder listens",
        vec![hit(old, 0.95)],
    )
    .await;
    assert_eq!(out.semantic_merged, vec![old]);
    assert_eq!(out.suspected, 0);
    assert_eq!(suspected_edges(&graph), 0);
}

/// A hit the content is declared `against` is the other guard: the
/// contradiction is declared, so there is nothing to suspect.
#[tokio::test]
async fn a_declared_contradiction_is_not_suspected() {
    let (graph, [i0, i1, _]) = graph_with_interactions();
    let old = derive_at(&graph, i0, OLD, Vec::new()).await.created[0];
    let pinned = PinnedCitations {
        references: Vec::new(),
        against: vec![PinnedReference {
            content: NEW.to_string(),
            reference: old.0.to_string(),
            target: old,
        }],
    };
    let out = derive_typed(
        &graph,
        i1,
        NEW,
        ConceptType::Logic,
        vec![hit(old, 0.95)],
        &pinned,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
    )
    .await;
    assert_eq!(out.merge_guarded.contradicts, 1);
    assert_eq!(out.contradicted, 1);
    assert_eq!(out.suspected, 0);
    assert_eq!(suspected_edges(&graph), 0);
}

/// A write that cites the older concept in `depends_on` declares the
/// relation: the identifier guard still refuses the merge, but the
/// citation settles it, so no edge is left.
#[tokio::test]
async fn a_declared_citation_is_not_suspected() {
    let (graph, [i0, i1, _]) = graph_with_interactions();
    let old = derive_at(&graph, i0, OLD, Vec::new()).await.created[0];
    let pinned = PinnedCitations {
        references: vec![PinnedReference {
            content: NEW.to_string(),
            reference: old.0.to_string(),
            target: old,
        }],
        against: Vec::new(),
    };
    let out = derive_typed(
        &graph,
        i1,
        NEW,
        ConceptType::Logic,
        vec![hit(old, 0.95)],
        &pinned,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
    )
    .await;
    assert_eq!(out.merge_guarded.identifiers, 1);
    assert_eq!(out.cited, 1);
    assert_eq!(out.suspected, 0);
    assert_eq!(suspected_edges(&graph), 0);
}

/// Two concepts written by one interaction (or at one instant) tie in time:
/// nothing is recorded, either way.
#[tokio::test]
async fn a_tie_in_time_records_nothing() {
    let (graph, [i0, _, _]) = graph_with_interactions();
    let old = derive_at(&graph, i0, OLD, Vec::new()).await.created[0];
    let out = derive_at(&graph, i0, NEW, vec![hit(old, 0.95)]).await;
    assert_eq!(out.merge_guarded.identifiers, 1);
    assert_eq!(out.suspected, 0);
    assert_eq!(suspected_edges(&graph), 0);
}

/// A backfilled write: written later, but its interaction's `event_time`
/// dates it before the older concept, so the existing concept is the
/// suspected replacement and the edge points at the new one.
#[tokio::test]
async fn a_backfilled_write_inverts_the_direction() {
    let (graph, [_, i1, _]) = graph_with_interactions();
    let current = derive_at(&graph, i1, NEW, Vec::new()).await.created[0];
    // Written at minute 30, about minute 5.
    let backfill = {
        let mut g = graph.write();
        let tail = *g.temporal_chain().last().unwrap();
        let mut i = interaction(9, Some(tail), 30, "backfill");
        i.session_id = sid(SESS);
        i.event_time = Some(ts(5));
        let id = i.id;
        g.insert_interaction(i).unwrap();
        id
    };
    let out = derive_at(&graph, backfill, OLD, vec![hit(current, 0.95)]).await;
    let old = out.created[0];
    assert_eq!(out.suspected, 1);
    assert!(
        suspected(&graph, current, old),
        "the later-dated concept replaces"
    );
    assert!(!suspected(&graph, old, current));
}

/// Several refused hits: each qualifying one gets its own edge, in id
/// order, and a non-qualifying one in the same call does not.
#[tokio::test]
async fn several_refused_hits_are_judged_each() {
    let (graph, [i0, i1, i2]) = graph_with_interactions();
    let a = derive_at(&graph, i0, "the embedder listens on port 8080", Vec::new())
        .await
        .created[0];
    let b = derive_at(&graph, i1, "the embedder listens on port 8082", Vec::new())
        .await
        .created[0];
    let c = derive_at(&graph, i1, "the embedder2 listens on port 8081", Vec::new())
        .await
        .created[0];
    let out = derive_at(
        &graph,
        i2,
        NEW,
        vec![hit(a, 0.95), hit(b, 0.94), hit(c, 0.96)],
    )
    .await;
    let new = out.created[0];
    assert_eq!(out.suspected, 2);
    assert!(suspected(&graph, new, a));
    assert!(suspected(&graph, new, b));
    assert!(
        !suspected(&graph, new, c),
        "the change is in the subject slot"
    );
}

/// Lapse: a later write that declares the relation, `against` or
/// `depends_on`, from either end, removes the suspicion.
#[tokio::test]
async fn a_later_declaration_settles_the_suspicion() {
    for (against, from_newer) in [(true, true), (false, true), (true, false), (false, false)] {
        let (graph, [i0, i1, i2]) = graph_with_interactions();
        let old = derive_at(&graph, i0, OLD, Vec::new()).await.created[0];
        let new = derive_at(&graph, i1, NEW, vec![hit(old, 0.95)])
            .await
            .created[0];
        assert!(suspected(&graph, new, old));
        let (content, target) = if from_newer { (NEW, old) } else { (OLD, new) };
        let reference = PinnedReference {
            content: content.to_string(),
            reference: target.0.to_string(),
            target,
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
        let _ = graph.write().drain_log();
        derive_typed(
            &graph,
            i2,
            content,
            ConceptType::Logic,
            Vec::new(),
            &pinned,
            SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        )
        .await;
        assert_eq!(
            suspected_edges(&graph),
            0,
            "against={against} from_newer={from_newer}"
        );
        // The removal is in the flush batch, so every store drops it too.
        let batch = graph.write().drain_log();
        assert!(batch
            .mutations
            .iter()
            .any(|m| matches!(m, crate::types::Mutation::DeleteEdge { .. })));
        graph.read().assert_invariants().unwrap();
    }
}
