//! #125: a hybrid derive gathers its embeds into batched rounds when the
//! embedder batches, and reads vectors a caller embedded ahead for it.

use super::*;
use crate::test_util::BatchingEmbedder;

const PROMPT: &str = "write queue batching for concurrent agents";

fn items() -> [(&'static str, ConceptType); 3] {
    [
        ("batched alpha", ConceptType::Entity),
        ("batched beta", ConceptType::Logic),
        ("batched gamma", ConceptType::Constraint),
    ]
}

async fn run(
    graph: Arc<RwLock<Graph>>,
    embedder: &dyn Embedder,
    interaction: NodeId,
    concepts: &[(&str, ConceptType)],
    parent_of: &ParentOf<'_>,
) -> Result<DeriveOutcome, LamboError> {
    let store = SpyStore::with_vector(Vec::new());
    derive_with(
        graph,
        VectorCandidates::from_store(&store),
        embedder,
        &contract("fixture", 1024),
        interaction,
        &agent(),
        concepts,
        parent_of,
        10,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        None,
        None,
    )
    .await
}

/// The stored vector of the concept with this content's key.
fn stored(graph: &RwLock<Graph>, content: &str) -> Option<Vec<f32>> {
    let g = graph.read();
    let key = crate::graph::canonical::canonical_key(content, |_| None);
    let id = g.concept_by_key(&key)?;
    match g.node(id) {
        Some(Node::Concept(c)) => c.embedding.clone(),
        _ => None,
    }
}

#[tokio::test]
async fn a_derive_embeds_its_items_and_parent_ends_in_one_round() {
    let (graph, iid) = graph_with_interaction("b125-one-round", 1, 0, PROMPT);
    let e = BatchingEmbedder::new(32);
    let pairs = [("batched parent", "batched alpha")];
    let out = run(
        graph.clone(),
        &e,
        iid,
        &items(),
        &ParentOf::from_pairs(&pairs),
    )
    .await
    .unwrap();
    assert_eq!(out.created.len(), 3 + 1);
    assert_eq!(e.rounds(), 1, "one round for the whole call");
    assert_eq!(e.singles(), 0, "nothing embedded one at a time");
    assert_eq!(
        *e.round_sizes.lock(),
        vec![4],
        "three items and one parent end"
    );
    // The batched vectors are exactly the ones an unbatched write stores.
    let fixture = FixtureEmbedder::new();
    for content in ["batched alpha", "batched beta", "batched parent"] {
        let want = fixture
            .embed(&context_text(content, Some(PROMPT)))
            .await
            .unwrap();
        assert_eq!(stored(&graph, content), Some(want), "{content}");
    }
}

#[tokio::test]
async fn a_non_batching_embedder_keeps_the_one_at_a_time_path() {
    let (graph, iid) = graph_with_interaction("b125-cap1", 1, 0, PROMPT);
    let e = BatchingEmbedder::new(1);
    run(graph, &e, iid, &items(), &ParentOf::none())
        .await
        .unwrap();
    assert_eq!(e.rounds(), 0);
    assert_eq!(e.singles(), 3);
}

#[tokio::test]
async fn a_refused_item_fails_its_write_with_its_own_class_and_nothing_is_written() {
    // The round embeds the good items; the refused one is embedded alone,
    // so the write meets that item's own permanent refusal.
    let (graph, iid) = graph_with_interaction("b125-refused", 1, 0, PROMPT);
    let e = BatchingEmbedder::new(32);
    let bad = format!("batched {}", BatchingEmbedder::REFUSE);
    let concepts = [
        ("batched alpha", ConceptType::Entity),
        (bad.as_str(), ConceptType::Entity),
        ("batched gamma", ConceptType::Entity),
    ];
    let err = run(graph.clone(), &e, iid, &concepts, &ParentOf::none())
        .await
        .unwrap_err();
    assert!(matches!(err, LamboError::Embed(_)), "{err:?}");
    assert_eq!((e.rounds(), e.singles()), (1, 1));
    assert_eq!(graph.read().concepts().count(), 0, "nothing was written");
    assert!(graph.read().embedding().is_none(), "no contract stamped");
}

#[tokio::test]
async fn a_whole_round_outage_falls_back_to_single_embeds() {
    let (graph, iid) = graph_with_interaction("b125-outage", 1, 0, PROMPT);
    let e = BatchingEmbedder::new(32);
    e.fail_rounds.store(true, Ordering::SeqCst);
    let out = run(graph.clone(), &e, iid, &items(), &ParentOf::none())
        .await
        .unwrap();
    assert_eq!(out.created.len(), 3);
    assert_eq!(out.embedded, 3);
    assert_eq!((e.rounds(), e.singles()), (1, 3));
}

#[tokio::test]
async fn record_action_contents_embed_in_one_round() {
    let e = BatchingEmbedder::new(32);
    let produces = ["out.rs"];
    let modifies = ["a.rs", "b.rs"];
    let depends_on = ["a.rs"];
    let act = crate::graph::action::Action {
        event_time: None,
        action: "rewired the lanes",
        produces: &produces,
        modifies: &modifies,
        depends_on: &depends_on,
    };
    let got = crate::graph::action::embed_action_contents(&e, &act)
        .await
        .unwrap();
    assert_eq!(got.len(), 4, "distinct strings only");
    assert_eq!((e.rounds(), e.singles()), (1, 0));
    assert_eq!(*e.round_sizes.lock(), vec![4]);
    let fixture = FixtureEmbedder::new();
    assert_eq!(got["a.rs"], fixture.embed("a.rs").await.unwrap());

    // A refused string still fails the action, with its own class.
    let e = BatchingEmbedder::new(32);
    let bad = [BatchingEmbedder::REFUSE];
    let act = crate::graph::action::Action {
        event_time: None,
        action: "touched a refused path",
        produces: &bad,
        modifies: &[],
        depends_on: &[],
    };
    let err = crate::graph::action::embed_action_contents(&e, &act)
        .await
        .unwrap_err();
    assert!(matches!(err, LamboError::Embed(_)), "{err:?}");
}
