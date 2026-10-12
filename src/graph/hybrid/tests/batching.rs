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
    gather: Gather<'_>,
) -> Result<DeriveOutcome, LamboError> {
    let store = SpyStore::with_vector(Vec::new());
    derive_with_gather(
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
        gather,
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
        Gather::default(),
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
    run(
        graph,
        &e,
        iid,
        &items(),
        &ParentOf::none(),
        Gather::default(),
    )
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
    let err = run(
        graph.clone(),
        &e,
        iid,
        &concepts,
        &ParentOf::none(),
        Gather::default(),
    )
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
    let out = run(
        graph.clone(),
        &e,
        iid,
        &items(),
        &ParentOf::none(),
        Gather::default(),
    )
    .await
    .unwrap();
    assert_eq!(out.created.len(), 3);
    assert_eq!(out.embedded, 3);
    assert_eq!((e.rounds(), e.singles()), (1, 3));
}

#[tokio::test]
async fn seeded_vectors_are_used_and_only_the_rest_is_embedded() {
    let (graph, iid) = graph_with_interaction("b125-seed", 1, 0, PROMPT);
    let e = BatchingEmbedder::new(32);
    let fixture = FixtureEmbedder::new();
    let mut seed = EmbedSeed::default();
    for content in ["batched alpha", "batched beta"] {
        let text = context_text(content, Some(PROMPT));
        let vector = fixture.embed(&text).await.unwrap();
        seed.extend([(text, vector)]);
    }
    let out = run(
        graph.clone(),
        &e,
        iid,
        &items(),
        &ParentOf::none(),
        Gather {
            seed: Some(&seed),
            io_started: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(out.created.len(), 3);
    // One text is left, and one text is never a round.
    assert_eq!((e.rounds(), e.singles()), (0, 1));
}

#[tokio::test]
async fn a_spent_io_budget_refuses_as_unavailable_before_any_embed_lands() {
    // The lane prefetch passes the instant its round started; a call whose
    // budget is already spent times out rather than overrunning it. The
    // embedder is held shut, so only the deadline can end the call.
    let (graph, iid) = graph_with_interaction("b125-budget", 1, 0, PROMPT);
    let mut e = BatchingEmbedder::new(32);
    e.gate = Some(Arc::new(tokio::sync::Semaphore::new(0)));
    let started = tokio::time::Instant::now() - HYBRID_IO_TIMEOUT - Duration::from_secs(1);
    let err = run(
        graph.clone(),
        &e,
        iid,
        &items(),
        &ParentOf::none(),
        Gather {
            seed: None,
            io_started: Some(started),
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(err, LamboError::EmbedUnavailable(_)), "{err:?}");
    assert_eq!(graph.read().concepts().count(), 0);
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

#[tokio::test]
async fn planned_embed_texts_predicts_what_the_call_embeds_and_nothing_it_refuses() {
    let (graph, iid) = graph_with_interaction("b125-predict", 1, 0, PROMPT);
    let e = BatchingEmbedder::new(32);
    run(
        graph.clone(),
        &e,
        iid,
        &[("batched alpha", ConceptType::Entity)],
        &ParentOf::none(),
        Gather::default(),
    )
    .await
    .unwrap();
    let g = graph.read();
    let pairs = [("batched parent", "batched beta")];
    let predicted = planned_embed_texts(
        &g,
        iid,
        &[
            ("batched alpha", ConceptType::Entity),
            ("batched beta", ConceptType::Entity),
        ],
        &ParentOf::from_pairs(&pairs),
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
    );
    // "batched alpha" now matches canonically, so only beta and the parent.
    assert_eq!(
        predicted,
        vec![
            context_text("batched beta", Some(PROMPT)),
            context_text("batched parent", Some(PROMPT)),
        ]
    );
    // A missing interaction, or a context over the cap, predicts nothing.
    let none = |iid, content: &str| {
        planned_embed_texts(
            &g,
            iid,
            &[(content, ConceptType::Entity)],
            &ParentOf::none(),
            SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        )
    };
    assert!(none(NodeId::new(), "batched gamma").is_empty());
    let huge = "x".repeat(MAX_HYBRID_CONTEXT_BYTES);
    assert!(none(iid, &huge).is_empty());
}
