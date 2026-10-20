//! #193: the merge guards. A vector hit at or above the threshold is not a
//! merge candidate when its text names other identifiers (digit runs or
//! identifier tokens differ), or when the written content is declared
//! `against` it. A guarded hit falls to the fresh arm (vector kept, no
//! `Semantic` edge) unless another, unguarded hit takes the merge.

use super::*;
use crate::types::PinnedReference;

const SESS: &str = "hybrid-guards";

/// Derive `content` into `graph` from `interaction` with the store handing
/// back `hits` and `pinned` accepted.
async fn derive_one(
    graph: &Arc<RwLock<Graph>>,
    interaction: NodeId,
    content: &str,
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
        &[(content, ConceptType::Logic)],
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

/// A graph holding `existing` as concepts (each derived with no near hit),
/// with their ids in order.
async fn seeded(existing: &[&str]) -> (Arc<RwLock<Graph>>, NodeId, Vec<NodeId>) {
    let (graph, interaction) = graph_with_interaction(SESS, 1, 0, "release notes");
    let mut ids = Vec::new();
    for content in existing {
        let out = derive_one(
            &graph,
            interaction,
            content,
            Vec::new(),
            &PinnedCitations::default(),
        )
        .await;
        ids.push(out.created[0]);
    }
    (graph, interaction, ids)
}

fn semantic(graph: &Arc<RwLock<Graph>>, a: NodeId, b: NodeId) -> bool {
    let g = graph.read();
    g.edge_between(a, b, EdgeType::Semantic).is_some()
        || g.edge_between(b, a, EdgeType::Semantic).is_some()
}

fn against(content: &str, target: NodeId) -> PinnedCitations {
    PinnedCitations {
        references: Vec::new(),
        against: vec![PinnedReference {
            content: content.to_string(),
            reference: target.0.to_string(),
            target,
        }],
    }
}

/// The identifier pairs of the issue: each is near (0.95, over the 0.85
/// default) and each is refused, leaving two concepts and no `Semantic`
/// edge. The new concept keeps its vector (the fresh arm).
#[tokio::test]
async fn differing_identifiers_never_merge() {
    for (old, new) in [
        ("release v0.5.0 is current", "release v0.6.0 is current"),
        (
            "src/mcp/serve.rs owns the stdio transport",
            "src/mcp/server.rs owns the stdio transport",
        ),
        (
            "review notes for J3-round1.md: two findings",
            "review notes for J3-round3.md: two findings",
        ),
        (
            "the embedder listens on port 8080",
            "the embedder listens on port 8081",
        ),
    ] {
        let (graph, interaction, ids) = seeded(&[old]).await;
        let out = derive_one(
            &graph,
            interaction,
            new,
            vec![hit(ids[0], 0.95)],
            &PinnedCitations::default(),
        )
        .await;
        assert_eq!(out.created.len(), 1, "{new}");
        assert!(out.semantic_merged.is_empty(), "{new}: merged");
        assert!(!semantic(&graph, ids[0], out.created[0]), "{new}");
        assert_eq!(
            out.merge_guarded,
            MergeGuarded {
                identifiers: 1,
                contradicts: 0
            },
            "{new}"
        );
        assert_eq!(out.embedded, 1, "{new}: the fresh arm keeps its vector");
        graph.read().assert_invariants().unwrap();
    }
}

/// The regression guard: a paraphrase naming the same identifiers still
/// merges, and nothing is counted.
#[tokio::test]
async fn a_paraphrase_with_the_same_identifiers_still_merges() {
    let (graph, interaction, ids) = seeded(&["release v0.5.0 is current"]).await;
    let out = derive_one(
        &graph,
        interaction,
        "v0.5.0 is the latest release",
        vec![hit(ids[0], 0.95)],
        &PinnedCitations::default(),
    )
    .await;
    assert_eq!(out.semantic_merged, vec![ids[0]]);
    assert!(semantic(&graph, ids[0], out.created[0]));
    assert!(out.merge_guarded.is_empty());
}

/// A hit below the threshold is not counted when guarded: it would not
/// have merged anyway.
#[tokio::test]
async fn a_guarded_hit_below_the_threshold_is_not_counted() {
    let (graph, interaction, ids) = seeded(&["release v0.5.0 is current"]).await;
    let out = derive_one(
        &graph,
        interaction,
        "release v0.6.0 is current",
        vec![hit(ids[0], 0.5)],
        &PinnedCitations::default(),
    )
    .await;
    assert!(out.semantic_merged.is_empty());
    assert!(out.merge_guarded.is_empty());
}

/// The #179 repro: the newer fact is a close rewording of the one it is
/// declared `against`. It does not merge into it; the `Contradicts` edge is
/// written; the receipt counts the vetoed hit.
#[tokio::test]
async fn a_write_never_merges_into_what_it_is_against() {
    const OLD: &str = "the dogfood embedder is llama.cpp bge-m3";
    const NEW: &str = "the dogfood embedder is candle bge-m3";
    let (graph, interaction, ids) = seeded(&[OLD]).await;
    let old = ids[0];
    let out = derive_one(
        &graph,
        interaction,
        NEW,
        vec![hit(old, 0.97)],
        &against(NEW, old),
    )
    .await;
    let new = out.created[0];
    assert!(out.semantic_merged.is_empty());
    assert!(!semantic(&graph, old, new));
    assert_eq!(out.contradicted, 1);
    assert!(graph
        .read()
        .edge_between(new, old, EdgeType::Contradicts)
        .is_some());
    assert_eq!(
        out.merge_guarded,
        MergeGuarded {
            identifiers: 0,
            contradicts: 1
        }
    );
    graph.read().assert_invariants().unwrap();
}

/// A hit vetoed for being `against` does not stop a merge into another,
/// unguarded hit at or above the threshold.
#[tokio::test]
async fn a_vetoed_hit_leaves_the_next_one_to_merge() {
    const OLD: &str = "the dogfood embedder is llama.cpp bge-m3";
    const OTHER: &str = "dogfood embeds with candle bge-m3";
    const NEW: &str = "the dogfood embedder is candle bge-m3";
    let (graph, interaction, ids) = seeded(&[OLD, OTHER]).await;
    let (old, other) = (ids[0], ids[1]);
    let out = derive_one(
        &graph,
        interaction,
        NEW,
        vec![hit(old, 0.97), hit(other, 0.9)],
        &against(NEW, old),
    )
    .await;
    assert_eq!(out.semantic_merged, vec![other]);
    assert!(!semantic(&graph, old, out.created[0]));
    assert_eq!(out.merge_guarded.contradicts, 1);
}

/// `against` vetoes only for the content that declares it: another
/// concept of the same call may still merge into the target.
#[tokio::test]
async fn the_veto_is_per_content() {
    let pinned = against("some other content", NodeId(Uuid::from_u64_pair(9, 9)));
    let (graph, _, ids) = seeded(&["release v0.5.0 is current"]).await;
    let g = graph.read();
    assert_eq!(
        merge_guard(&g, "v0.5.0 is the latest release", &pinned, ids[0]),
        None
    );
    assert_eq!(
        merge_guard(
            &g,
            "v0.5.0 is the latest release",
            &against("v0.5.0 is the latest release", ids[0]),
            ids[0]
        ),
        Some(MergeGuard::Contradicts)
    );
    assert_eq!(
        merge_guard(&g, "release v0.6.0 is current", &pinned, ids[0]),
        Some(MergeGuard::Identifiers)
    );
    // Not a concept of the graph: left to the commit's own validation.
    assert_eq!(
        merge_guard(&g, "anything", &pinned, NodeId(Uuid::from_u64_pair(7, 7))),
        None
    );
}

/// Re-plan check (`plan_still_holds`): a concept written since the plan,
/// near the planned item, makes the plan stale only when the guards would
/// let the item merge into it. A rival they refuse changes no choice.
#[test]
fn a_guarded_rival_does_not_make_a_plan_stale() {
    const ITEM: &str = "release v0.6.0 is current";
    for (rival, pinned_against, stale) in [
        ("release v0.5.0 is current", false, false),
        ("v0.6.0 is the latest release", true, false),
        ("v0.6.0 is the latest release", false, true),
    ] {
        let (graph, interaction) = graph_with_interaction(SESS, 1, 0, "release notes");
        let concepts = [(ITEM, ConceptType::Logic)];
        let mut g = graph.write();
        g.stamp_embedding(contract("fixture", 4)).unwrap();
        let planned_epoch = g.epoch();
        let (items, parent_ends) = plan_inputs(&g, &concepts, &ParentOf::none()).unwrap();
        let probe = vec![1.0f32, 0.0, 0.0, 0.0];
        let resolutions = vec![Resolution::Fresh {
            key: items[0].2.clone(),
            embedding: Some(probe.clone()),
            source: None,
        }];
        let session_id = g.session_id().clone();
        let c = new_concept(
            &session_id,
            rival,
            ConceptType::Logic,
            crate::graph::canonical::canonical_key(rival, |_| None),
            interaction,
            &agent(),
            ts(0),
            Some(probe.clone()),
            None,
        );
        let rival_id = c.id;
        g.insert_concept(c, interaction).unwrap();
        let pinned = if pinned_against {
            against(ITEM, rival_id)
        } else {
            PinnedCitations::default()
        };
        let stamped = g.embedding().cloned();
        let (created_at, event_time, prompt) = match g.node(interaction) {
            Some(Node::Interaction(i)) => (i.created_at, i.event_time, i.prompt_text.clone()),
            _ => unreachable!(),
        };
        let plan = PlanRead {
            planned_epoch,
            interaction,
            interaction_read: (prompt.as_deref(), created_at, event_time),
            stamped: stamped.as_ref(),
            concepts: &concepts,
            parent_of: &ParentOf::none(),
            items: &items,
            parent_ends: &parent_ends,
            resolutions: &resolutions,
            pinned: &pinned,
            semantic_match_threshold: SEMANTIC_MATCH_THRESHOLD_DEFAULT,
            rescore_cap: MAX_PLAN_RESCORES,
        };
        let want = if stale {
            Verdict::Stale
        } else {
            Verdict::Holds
        };
        assert_eq!(
            plan_still_holds(&g, &plan).unwrap(),
            want,
            "{rival} (against: {pinned_against})"
        );
    }
}
