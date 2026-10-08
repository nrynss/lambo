//! Embedder and backend failures, and the no-capability path.

use super::*;

#[tokio::test]
async fn no_capability_is_byte_identical_to_canonical() {
    // Run the SAME interaction twice: once through the sync `derive`
    // (MatchStrategy::Canonical) and once through `hybrid::derive` against a
    // store without VECTOR_SEARCH. The graph shapes must be identical
    // (fresh concept + Derives, no Semantic edge, zero embed / vector calls).
    use crate::graph::derive::derive as canonical_derive;

    let sess = "hybrid-nocap";
    let (graph_h, i1) = graph_with_interaction(sess, 1, 0, "auth flow for users");
    let recorder = RecordingEmbedder::new();
    let out_h = derive(
        graph_h.clone(),
        &SpyStore::without_vector(),
        &recorder,
        &contract("fixture", 1024),
        i1,
        &agent(),
        &[
            ("register user", ConceptType::Entity),
            ("create account", ConceptType::Entity),
        ],
        &ParentOf::none(),
        10,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        None,
    )
    .await
    .unwrap();

    // No embedding was attempted (capability gated before any I/O) and no
    // contract was stamped — byte-identical to Canonical.
    assert!(
        recorder.embedded_texts().is_empty(),
        "no embed when no VECTOR_SEARCH"
    );
    assert!(
        graph_h.read().embedding().is_none(),
        "no contract stamp on degraded path"
    );
    assert!(!out_h.matched.iter().any(|n| n == &i1));

    // Sync canonical twin for the same inputs.
    let (mut g_c, i1c) = {
        let mut g = Graph::new(sid(sess));
        let mut i = interaction(1, None, 0, "auth flow for users");
        i.session_id = sid(sess);
        let id = i.id;
        g.insert_interaction(i).unwrap();
        (g, id)
    };
    let out_c = canonical_derive(
        &mut g_c,
        i1c,
        &agent(),
        &[
            ("register user", ConceptType::Entity),
            ("create account", ConceptType::Entity),
        ],
        &ParentOf::none(),
        10,
    )
    .unwrap();

    fn shape(
        g: &Graph,
    ) -> (
        Vec<(String, String)>,
        std::collections::HashMap<EdgeType, usize>,
    ) {
        let mut nodes: Vec<(String, String)> = g
            .concepts()
            .map(|c| (c.canonical_key.clone(), format!("{:?}", c.concept_type)))
            .collect();
        nodes.sort();
        let mut edges = std::collections::HashMap::new();
        for e in g.edges() {
            *edges.entry(e.edge_type).or_insert(0) += 1;
        }
        (nodes, edges)
    }

    assert_eq!(out_h.created.len(), out_c.created.len());
    assert_eq!(shape(&graph_h.read()), shape(&g_c));
}

#[tokio::test]
async fn mid_session_kind_swap_refused_without_reembed() {
    let sess = "hybrid-swap";
    // Session already stamped with fixture (kind "fixture"). A hybrid write
    // arriving with a different live kind (bedrock) must be refused BEFORE
    // embedding — the embedder must never be called.
    let (graph, i1) = graph_with_interaction(sess, 1, 0, "auth flow");
    graph
        .write()
        .stamp_embedding(contract("fixture", 1024))
        .unwrap();
    let failing = FailingEmbedder::new(); // would panic the test if called via embed -> no, it fails; use a panic embedder

    let err = derive(
        graph.clone(),
        &SpyStore::with_vector(vec![]),
        &failing,
        &contract("bedrock", 1024), // same dim, different kind — the trap
        i1,
        &agent(),
        &[("create account", ConceptType::Entity)],
        &ParentOf::none(),
        10,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        None,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, LamboError::Config(_)),
        "unexpected error: {err:?}"
    );
    // Refused WITHOUT re-embed: embedder never invoked, graph unchanged.
    assert_eq!(failing.calls(), 0);
    let g = graph.read();
    assert_eq!(g.node_count(), 1, "interaction only — no concept written");
    assert_eq!(g.embedding().unwrap().kind, "fixture", "stamp preserved");
}

#[tokio::test]
async fn embed_failure_fails_the_write_and_writes_nothing() {
    // J3-R3-1 (reverses this test's previous pin, which asserted the L82-4
    // era behaviour "a dead embedder degrades the write, it does not fail
    // it"). The degrade arm was the located mechanism behind the dogfood
    // store's 92/100 unembedded concepts and behind round 3's estimator
    // poisoning: the concept was applied with `embedding: NULL`, the caller
    // was told an unqualified success, and the write queue sampled a ~3 ms
    // non-embed as a fast write. An embedder failure is now an `Err`, and
    // nothing — no concept, no edge, no contract stamp — is written.
    let sess = "hybrid-embedfail";
    let (graph, i1) = graph_with_interaction(sess, 1, 0, "auth flow");
    let failing = FailingEmbedder::new();
    let err = derive(
        graph.clone(),
        &SpyStore::with_vector(vec![]),
        &failing,
        &contract("fixture", 1024),
        i1,
        &agent(),
        &[("create account", ConceptType::Entity)],
        &ParentOf::none(),
        10,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(failing.calls(), 1, "embed attempted once, then refused");
    // J3 round-1 N1 sharpens this assertion without weakening it. The
    // failure this test drives is `EmbedError::Unavailable("server down")`
    // — a dead embedder, not a rejected input — so the class it must
    // produce is `EmbedUnavailable`. The write still fails and still writes
    // nothing (the whole point of the reversal above); what is new is that
    // the durable-intent replay can now tell this failure from a content
    // refusal instead of destroying an acked write over it. The refusal
    // class has its own pin below.
    assert!(
        matches!(err, LamboError::EmbedUnavailable(_)),
        "a dead embedder is EmbedUnavailable, not a degrade and not a content \
             refusal: {err:?}"
    );
    assert!(
        err.to_string().contains("nothing was written"),
        "the refusal must say nothing was written: {err}"
    );
    let g = graph.read();
    assert_eq!(
        g.node_count(),
        1,
        "interaction only — the refused write must not create a concept"
    );
    // MINOR-2 still holds: no embed ever returned a vector, so this is not
    // a "first embed" and the contract must not be stamped.
    assert!(
        g.embedding().is_none(),
        "a failed embed must not bind the session to an embedding contract"
    );
    g.assert_invariants().unwrap();
}

/// **J3 round-1 N2** — the deviation's own arm, finally pinned.
///
/// The design of record's honesty clause named the embedder *refusal*; this
/// branch also made an embed **timeout** an `Err`, and that deviation shipped
/// with an argument but no test — while being the commoner field condition of
/// the two (a slow or wedged llama.cpp is likelier than a refusing one). The
/// refusal arm was pinned thoroughly; the added arm was not.
///
/// The clock is paused, so `timeout_at` fires the moment every task is idle:
/// tokio auto-advances a paused clock to the next deadline, which makes a
/// 30-second `HYBRID_IO_TIMEOUT` cost no wall time and keeps the test out of
/// the flaky-stopwatch class. The embedder never resolves, so the *only* way
/// out of `derive` is the timeout arm.
#[tokio::test(start_paused = true)]
async fn an_embed_timeout_fails_the_write_and_writes_nothing() {
    struct HangingEmbedder;
    #[async_trait]
    impl Embedder for HangingEmbedder {
        fn dimensions(&self) -> usize {
            1024
        }
        async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
            std::future::pending().await
        }
    }
    let sess = "hybrid-embedtimeout";
    let (graph, i1) = graph_with_interaction(sess, 1, 0, "auth flow");
    let err = derive(
        graph.clone(),
        &SpyStore::with_vector(vec![]),
        &HangingEmbedder,
        &contract("fixture", 1024),
        i1,
        &agent(),
        &[("create account", ConceptType::Entity)],
        &ParentOf::none(),
        10,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        None,
    )
    .await
    .unwrap_err();
    // A timeout is an unreachable embedder, not a rejected input: the
    // durable-intent replay must be able to tell them apart (N1).
    assert!(
        matches!(err, LamboError::EmbedUnavailable(_)),
        "an embed timeout is EmbedUnavailable: {err:?}"
    );
    assert!(
        err.to_string().contains("timed out"),
        "the message must name the timeout, not a generic failure: {err}"
    );
    assert!(
        err.to_string().contains("nothing was written"),
        "the timeout arm owes the same honesty as the refusal arm: {err}"
    );
    let g = graph.read();
    assert_eq!(
        g.node_count(),
        1,
        "interaction only — a timed-out write must not create a concept"
    );
    // MINOR-2: no vector ever came back, so the session must not be bound to
    // an embedding contract by a write that failed.
    assert!(
        g.embedding().is_none(),
        "a timed-out embed must not bind the session to an embedding contract"
    );
    g.assert_invariants().unwrap();
}

/// **J3 round-1 N1, the other side of the classification.** A refusal the
/// embedder *answers* — `EmbedError::Backend`, which is what the shipped
/// adapter reports for a non-success HTTP status — must stay
/// `LamboError::Embed`, because that is the class the durable-intent replay
/// consumes on. If this collapsed into `EmbedUnavailable` the fix would have
/// traded "never retry" for "retry forever": a record no embedder will
/// accept would be re-attempted at every attach for the life of the session.
#[tokio::test]
async fn a_content_refusal_stays_an_embed_error_not_an_unavailable_one() {
    struct RefusingEmbedder;
    #[async_trait]
    impl Embedder for RefusingEmbedder {
        fn dimensions(&self) -> usize {
            1024
        }
        async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
            Err(EmbedError::Backend(
                "llama.cpp returned 500 Internal Server Error for model \"bge-m3\": \
                     input is too long"
                    .into(),
            ))
        }
    }
    let sess = "hybrid-embedrefuse";
    let (graph, i1) = graph_with_interaction(sess, 1, 0, "auth flow");
    let err = derive(
        graph.clone(),
        &SpyStore::with_vector(vec![]),
        &RefusingEmbedder,
        &contract("fixture", 1024),
        i1,
        &agent(),
        &[("create account", ConceptType::Entity)],
        &ParentOf::none(),
        10,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        None,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, LamboError::Embed(_)),
        "an answered refusal is a content-level failure: {err:?}"
    );
    assert!(
        err.to_string().contains("nothing was written"),
        "still refused, still nothing written: {err}"
    );
    let g = graph.read();
    assert_eq!(g.node_count(), 1, "interaction only");
    assert!(g.embedding().is_none());
    g.assert_invariants().unwrap();
}

#[tokio::test]
async fn real_backend_error_propagates_not_degrade() {
    let sess = "hybrid-backend";
    let (graph, i1) = graph_with_interaction(sess, 1, 0, "auth flow");
    // A genuine (non-Capability) store error must NOT be swallowed into a
    // fresh concept — it propagates, so a broken backend is visible.
    let err = derive(
        graph.clone(),
        &SpyStore::failing(),
        &RecordingEmbedder::new(),
        &contract("fixture", 1024),
        i1,
        &agent(),
        &[("create account", ConceptType::Entity)],
        &ParentOf::none(),
        10,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        None,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, LamboError::Store(StoreError::Backend(_))),
        "unexpected: {err:?}"
    );
}
