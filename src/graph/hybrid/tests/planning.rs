//! Gather and plan: candidate validation, contracts, parent_of, stale
//! gathers and request limits.

use super::*;

#[derive(Debug)]
struct BarrierEmbedder {
    barrier: Barrier,
    calls: AtomicUsize,
}

impl BarrierEmbedder {
    fn new(parties: usize) -> Self {
        Self {
            barrier: Barrier::new(parties),
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl Embedder for BarrierEmbedder {
    fn dimensions(&self) -> usize {
        1024
    }

    async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call < 2 {
            self.barrier.wait().await;
        }
        Ok(vec![0.0; 1024])
    }
}

#[derive(Debug)]
struct PausingEmbedder {
    started: Notify,
    release: Notify,
    calls: AtomicUsize,
}

#[async_trait]
impl Embedder for PausingEmbedder {
    fn dimensions(&self) -> usize {
        1024
    }

    async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.started.notify_one();
            self.release.notified().await;
        }
        Ok(vec![0.0; 1024])
    }
}

#[test]
fn candidate_validation_and_ties_form_one_tier() {
    let lower = NodeId(Uuid::from_u64_pair(0, 1));
    let higher = NodeId(Uuid::from_u64_pair(0, 2));
    let invalid = NodeId(Uuid::from_u64_pair(0, 3));
    let a = vec![
        hit(higher, 0.9),
        hit(invalid, f64::NAN),
        hit(lower, 0.9),
        hit(invalid, 1.1),
    ];
    let mut b = a.clone();
    b.reverse();
    // The 0.9 pair is the whole valid top tier — both members, in either
    // input order (the gather phase picks none of them: no graph here).
    let tier_ids = |tier: Vec<&Scored<NodeId>>| tier.iter().map(|c| c.item).collect::<Vec<_>>();
    let mut ta = tier_ids(top_tier(&a, 0.85));
    let mut tb = tier_ids(top_tier(&b, 0.85));
    // NodeId is deliberately not Ord (issue #2: the UUID must not be the
    // semantic order) — sort by the raw bytes only to compare sets.
    ta.sort_by_key(|id| id.0);
    tb.sort_by_key(|id| id.0);
    assert_eq!(ta, vec![lower, higher]);
    assert_eq!(tb, vec![lower, higher]);
    // Invalid candidates are filtered, not merely outranked.
    assert!(top_tier(&[hit(invalid, f64::INFINITY)], 0.85).is_empty());
    // A strictly best candidate is a tier of one.
    assert_eq!(
        tier_ids(top_tier(&[hit(higher, 0.9), hit(lower, 0.86)], 0.85)),
        vec![higher]
    );
}

#[tokio::test]
async fn concurrent_first_writers_cannot_mix_embedding_contracts() {
    let (graph, interaction) =
        graph_with_interaction("hybrid-contract-race", 1, 0, "concurrent contract race");
    let embedder = Arc::new(BarrierEmbedder::new(2));
    let store = Arc::new(SpyStore::with_vector(Vec::new()));

    let spawn = |kind: &'static str, content: &'static str| {
        let graph = graph.clone();
        let embedder = embedder.clone();
        let store = store.clone();
        tokio::spawn(async move {
            derive(
                graph,
                store.as_ref(),
                embedder.as_ref(),
                &contract(kind, 1024),
                interaction,
                &agent(),
                &[(content, ConceptType::Entity)],
                &ParentOf::none(),
                10,
                SEMANTIC_MATCH_THRESHOLD_DEFAULT,
                None,
            )
            .await
        })
    };
    let (a, b) = tokio::join!(spawn("fixture-a", "alpha"), spawn("fixture-b", "beta"));
    let outcomes = [a.unwrap(), b.unwrap()];
    assert_eq!(outcomes.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(outcomes.iter().filter(|r| r.is_err()).count(), 1);
    let g = graph.read();
    assert_eq!(
        g.concepts().count(),
        1,
        "losing vector space writes nothing"
    );
    g.assert_invariants().unwrap();
}

/// C4: one derive naming the same content in BOTH `concepts` and a
/// `parent_of` pair must produce ONE node, under the product's strategy.
///
/// The regression this pins is an identity split, not a missing edge:
/// before the fix the call produced two nodes for one content — the
/// declared `Observation` (embedded, typed, carrying CoOccurrence /
/// Derives / Semantic) and a bare `Entity` (unembedded, carrying only
/// Derives / Hierarchical) — because `canonicalize` never matches an
/// `Observation` (GRAPH-1). On Mooshik's bootstrap graph that was 170
/// contents existing as such pairs, holding embedding coverage near 50%
/// and splitting each fact's supporting interactions across two nodes.
///
/// **The count is the assertion.** A test that checked only "a
/// Hierarchical edge exists" passes on the duplicate — the split satisfies
/// the edge — which is precisely why this went unnoticed.
///
/// `Observation` is the type that reproduces it, so it is the type under
/// test; `Entity` is carried alongside as the control that was already
/// correct, so a regression that breaks matching generally is
/// distinguishable from one that breaks only the Observation path.
#[tokio::test]
async fn parent_of_child_resolves_to_a_concept_declared_in_the_same_call() {
    for concept_type in [ConceptType::Observation, ConceptType::Entity] {
        let (graph, interaction) =
            graph_with_interaction("hybrid-same-call", 1, 0, "ingest context");
        let store = SpyStore::with_vector(Vec::new());
        let embedder = FixtureEmbedder::new();
        let pairs = [("document:src.md", "shared content")];
        derive(
            graph.clone(),
            &store,
            &embedder,
            &contract("fixture", 1024),
            interaction,
            &agent(),
            &[("shared content", concept_type)],
            &ParentOf::from_pairs(&pairs),
            10,
            SEMANTIC_MATCH_THRESHOLD_DEFAULT,
            None,
        )
        .await
        .unwrap();

        let g = graph.read();
        let mine: Vec<_> = g
            .concepts()
            .filter(|c| c.content == "shared content")
            .collect();
        assert_eq!(
            mine.len(),
            1,
            "{concept_type:?}: one content must be one node, got {:?}",
            mine.iter()
                .map(|c| (c.concept_type, c.embedding.is_some()))
                .collect::<Vec<_>>()
        );
        let node = mine[0];
        assert_eq!(
            node.concept_type, concept_type,
            "the surviving node must keep the declared type, not become the \
                 parent_of default"
        );
        assert!(
            node.embedding.is_some(),
            "the surviving node must keep its embedding — losing it is what held \
                 coverage near 50%"
        );

        let id = node.id;
        let parent = g
            .concepts()
            .find(|c| c.content == "document:src.md")
            .expect("the parent end is still created")
            .id;
        assert!(
            g.edge_between(interaction, id, EdgeType::Derives).is_some(),
            "{concept_type:?}: the concept keeps its Derives edge"
        );
        assert!(
            g.edge_between(parent, id, EdgeType::Hierarchical).is_some(),
            "{concept_type:?}: the pair's Hierarchical edge lands on that same node \
                 — M9 samples targets of Hierarchical edges, so it is load-bearing"
        );
    }
}

/// C4 control: the fix is scoped to THIS call's own writes, so a
/// `parent_of` end the graph has never seen is still created fresh as
/// `PARENT_OF_CONCEPT_TYPE` (`Entity`). Without this, a fix that made
/// `canonicalize` match Observations generally would pass the test above
/// while quietly reattaching agent content to demoted context-overflow
/// records (GRAPH-1).
#[tokio::test]
async fn a_brand_new_parent_of_end_is_still_created_as_entity() {
    let (graph, interaction) = graph_with_interaction("hybrid-fresh-end", 1, 0, "ingest context");
    let store = SpyStore::with_vector(Vec::new());
    let embedder = FixtureEmbedder::new();
    let pairs = [("document:src.md", "never mentioned elsewhere")];
    derive(
        graph.clone(),
        &store,
        &embedder,
        &contract("fixture", 1024),
        interaction,
        &agent(),
        &[("an unrelated concept", ConceptType::Observation)],
        &ParentOf::from_pairs(&pairs),
        10,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        None,
    )
    .await
    .unwrap();

    let g = graph.read();
    let fresh: Vec<_> = g
        .concepts()
        .filter(|c| c.content == "never mentioned elsewhere")
        .collect();
    assert_eq!(fresh.len(), 1, "the fresh end is created exactly once");
    assert_eq!(
        fresh[0].concept_type, PARENT_OF_CONCEPT_TYPE,
        "an end this call did not declare is still an Entity"
    );
}

/// Issue #16 §2: a `parent_of` end that is in neither `concepts` nor the
/// graph is created fresh, and before this fix it was created with
/// `embedding: None` — permanently invisible to recall's vector leg, while
/// the receipt read "2 created (1 embedded)". Both rigs accumulated these
/// (every one an `Entity`-typed `parent_of` end) until `re-embed
/// --missing-only` backfilled them.
///
/// The end must carry a vector in the session's space, the embed must use the
/// same origin-framed context as the call's own concepts, and the outcome's
/// `embedded` count must cover it. The merge leg stays with `concepts`: the
/// end is embedded but never queried for candidates, so `vector_calls` stays
/// at one (the declared concept's).
#[tokio::test]
async fn a_parent_of_only_end_is_embedded_and_counted() {
    let (graph, interaction) =
        graph_with_interaction("hybrid-parent-embed", 1, 0, "ingest context");
    let store = SpyStore::with_vector(Vec::new());
    let embedder = RecordingEmbedder::new();
    let pairs = [("document:src.md", "an unrelated concept")];
    let out = derive(
        graph.clone(),
        &store,
        &embedder,
        &contract("fixture", 1024),
        interaction,
        &agent(),
        &[("an unrelated concept", ConceptType::Observation)],
        &ParentOf::from_pairs(&pairs),
        10,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        None,
    )
    .await
    .unwrap();

    let g = graph.read();
    let parent = g
        .concepts()
        .find(|c| c.content == "document:src.md")
        .expect("the parent end is created");
    assert_eq!(parent.concept_type, PARENT_OF_CONCEPT_TYPE);
    assert!(
        parent.embedding.as_ref().is_some_and(|v| v.len() == 1024),
        "a parent_of-only end must carry a vector (issue #16 §2)"
    );
    assert_eq!(out.created.len(), 2);
    assert_eq!(
        out.embedded, 2,
        "the receipt's \"N created (M embedded)\" must count the parent_of end"
    );
    assert!(
        embedder
            .embedded_texts()
            .contains(&"document:src.md — ingest context".to_string()),
        "the end is embedded with the same origin framing as the call's concepts: {:?}",
        embedder.embedded_texts()
    );
    assert_eq!(
        store.vector_calls(),
        1,
        "a parent_of end is embedded, never sent through the merge leg"
    );
    assert_eq!(g.embedding(), Some(&contract("fixture", 1024)));
    assert!(
        g.concepts().all(|c| c.embedding.is_some()),
        "every concept this call created carries a vector"
    );
    g.assert_invariants().unwrap();
}

/// The fix embeds only ends the call will *create*. An end that resolves to a
/// concept already in the graph, or to one of this call's own `concepts`, is
/// not embedded again (its vector belongs to the write that created it), and
/// two pairs naming one new end embed it once.
#[tokio::test]
async fn parent_of_ends_that_resolve_to_existing_concepts_are_not_re_embedded() {
    let (graph, interaction) =
        graph_with_interaction("hybrid-parent-reuse", 1, 0, "ingest context");
    let store = SpyStore::with_vector(Vec::new());
    let embedder = RecordingEmbedder::new();
    derive(
        graph.clone(),
        &store,
        &embedder,
        &contract("fixture", 1024),
        interaction,
        &agent(),
        &[("existing parent", ConceptType::Entity)],
        &ParentOf::none(),
        10,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        None,
    )
    .await
    .unwrap();
    let before = embedder.embedded_texts().len();

    let pairs = [
        ("existing parent", "declared child"),
        ("new parent", "declared child"),
        ("new parent", "other child"),
    ];
    let out = derive(
        graph.clone(),
        &store,
        &embedder,
        &contract("fixture", 1024),
        interaction,
        &agent(),
        &[("declared child", ConceptType::Entity)],
        &ParentOf::from_pairs(&pairs),
        10,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        None,
    )
    .await
    .unwrap();

    let texts = embedder.embedded_texts()[before..].to_vec();
    let mut subjects: Vec<&str> = texts
        .iter()
        .map(|t| t.split(" — ").next().unwrap())
        .collect();
    subjects.sort_unstable();
    assert_eq!(
        subjects,
        ["declared child", "new parent", "other child"],
        "one embed per created content; the existing parent and the declared \
         child's second mention are not re-embedded"
    );
    assert_eq!(out.created.len(), 3);
    assert_eq!(out.embedded, 3);
    let g = graph.read();
    assert!(g.concepts().all(|c| c.embedding.is_some()));
    g.assert_invariants().unwrap();
}

/// The degradation rules are the `concepts` rules. Capability absent: the end
/// stays keyword-only and nothing is embedded (byte-identical to Canonical).
/// Embedder failure on the end: the whole write fails with nothing written,
/// exactly as it does for a declared concept (J3-R3-1).
#[tokio::test]
async fn parent_of_end_embedding_follows_the_concepts_degrade_rules() {
    let (graph, interaction) =
        graph_with_interaction("hybrid-parent-nocap", 1, 0, "ingest context");
    let embedder = RecordingEmbedder::new();
    let pairs = [("document:src.md", "a child")];
    let out = derive(
        graph.clone(),
        &SpyStore::without_vector(),
        &embedder,
        &contract("fixture", 1024),
        interaction,
        &agent(),
        &[("a child", ConceptType::Entity)],
        &ParentOf::from_pairs(&pairs),
        10,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        None,
    )
    .await
    .unwrap();
    assert!(embedder.embedded_texts().is_empty());
    assert_eq!(out.embedded, 0);
    assert!(graph.read().embedding().is_none());
    assert!(graph.read().concepts().all(|c| c.embedding.is_none()));

    /// Embeds everything except the parent_of end.
    struct RefusesParent;
    #[async_trait]
    impl Embedder for RefusesParent {
        fn dimensions(&self) -> usize {
            1024
        }
        async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
            if text.starts_with("document:src.md") {
                return Err(EmbedError::Unavailable("server down".into()));
            }
            FixtureEmbedder::new().embed(text).await
        }
    }
    let (graph, interaction) = graph_with_interaction("hybrid-parent-fail", 1, 0, "ingest context");
    let err = derive(
        graph.clone(),
        &SpyStore::with_vector(Vec::new()),
        &RefusesParent,
        &contract("fixture", 1024),
        interaction,
        &agent(),
        &[("a child", ConceptType::Entity)],
        &ParentOf::from_pairs(&pairs),
        10,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        None,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, LamboError::EmbedUnavailable(_)),
        "a failed parent_of embed fails the write like a failed concept embed: {err:?}"
    );
    let g = graph.read();
    assert_eq!(g.node_count(), 1, "interaction only — nothing was written");
    assert!(g.embedding().is_none());
}

/// A store that advertises `VECTOR_SEARCH` but refuses the checked lookup
/// with a capability miss serves no vectors, so the ends stay keyword-only,
/// exactly as the call's own concepts do. That must not depend on what else
/// is in the call: with an unmatched concept, the concept's lookup sees the
/// refusal first; in a call with no unmatched concept, the ends ask the store
/// themselves (once, with the first end's vector) and stop there.
#[tokio::test]
async fn parent_of_ends_stay_keyword_only_when_the_store_refuses_vectors() {
    let pairs = [
        ("document:src.md", "a child"),
        ("document:other.md", "a child"),
    ];

    // With an unmatched concept in the call.
    let (graph, interaction) =
        graph_with_interaction("hybrid-parent-refused", 1, 0, "ingest context");
    let store = SpyStore::refusing();
    let embedder = RecordingEmbedder::new();
    let out = derive(
        graph.clone(),
        &store,
        &embedder,
        &contract("fixture", 1024),
        interaction,
        &agent(),
        &[("a child", ConceptType::Entity)],
        &ParentOf::from_pairs(&pairs),
        10,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        None,
    )
    .await
    .unwrap();
    assert_eq!(out.created.len(), 3);
    assert_eq!(out.embedded, 0);
    assert_eq!(
        store.vector_calls(),
        1,
        "the concept's lookup saw the refusal"
    );
    assert_eq!(
        embedder.embedded_texts(),
        ["a child — ingest context"],
        "after the refusal no end is embedded"
    );
    assert!(graph.read().concepts().all(|c| c.embedding.is_none()));

    // Ends only: no concept asked the store, so the first end does.
    let (graph, interaction) =
        graph_with_interaction("hybrid-parent-refused-ends", 1, 0, "ingest context");
    let store = SpyStore::refusing();
    let embedder = RecordingEmbedder::new();
    let out = derive(
        graph.clone(),
        &store,
        &embedder,
        &contract("fixture", 1024),
        interaction,
        &agent(),
        &[],
        &ParentOf::from_pairs(&pairs),
        10,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        out.created.len(),
        3,
        "both parents and the child are created"
    );
    assert_eq!(
        out.embedded, 0,
        "a store that refuses vectors gets none, whatever else the call carries"
    );
    assert_eq!(
        store.vector_calls(),
        1,
        "one probe, with the first end's vector"
    );
    assert_eq!(
        embedder.embedded_texts().len(),
        1,
        "the refusal stops the ends' embeds: {:?}",
        embedder.embedded_texts()
    );
    let g = graph.read();
    assert!(g.concepts().all(|c| c.embedding.is_none()));
    g.assert_invariants().unwrap();
}

/// A `parent_of` end the call will create is embedded with the origin
/// framing, so its framed context is held to `MAX_HYBRID_CONTEXT_BYTES` like
/// a concept's. The end itself is under the per-string cap that
/// `validate_limits` checks; only the framed context is over. The refusal is
/// a `Config` error before any embed or store call, with nothing written.
#[tokio::test]
async fn an_oversized_parent_of_end_context_is_refused_before_any_embed() {
    let (graph, interaction) =
        graph_with_interaction("hybrid-parent-oversized", 1, 0, "ingest context");
    let store = SpyStore::with_vector(Vec::new());
    let embedder = RecordingEmbedder::new();
    let long_end = "p".repeat(MAX_HYBRID_CONTEXT_BYTES);
    let pairs = [(long_end.as_str(), "a child")];
    let before = graph.read().snapshot();
    let err = derive(
        graph.clone(),
        &store,
        &embedder,
        &contract("fixture", 1024),
        interaction,
        &agent(),
        &[("a child", ConceptType::Entity)],
        &ParentOf::from_pairs(&pairs),
        10,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        None,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&err, LamboError::Config(m) if m.contains("embedding context")),
        "unexpected error: {err:?}"
    );
    assert!(embedder.embedded_texts().is_empty());
    assert_eq!(store.vector_calls(), 0);
    assert_eq!(graph.read().snapshot(), before, "nothing was written");
}

/// A call whose only new concepts are `parent_of` ends still embeds, so a
/// mid-session contract swap is refused before the first embed, as it is for
/// a call with a new concept. Without the check the ends would be embedded in
/// the wrong space and only the commit-phase recheck would refuse.
#[tokio::test]
async fn an_ends_only_call_checks_the_contract_before_any_embed() {
    let (graph, interaction) = graph_with_interaction("hybrid-parent-swap", 1, 0, "ingest context");
    graph
        .write()
        .stamp_embedding(contract("fixture", 1024))
        .unwrap();
    let store = SpyStore::with_vector(Vec::new());
    let embedder = RecordingEmbedder::new();
    let pairs = [("new parent", "new child")];
    let err = derive(
        graph.clone(),
        &store,
        &embedder,
        &contract("bedrock", 1024),
        interaction,
        &agent(),
        &[],
        &ParentOf::from_pairs(&pairs),
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
    assert!(
        embedder.embedded_texts().is_empty(),
        "refused before any embed: {:?}",
        embedder.embedded_texts()
    );
    assert_eq!(store.vector_calls(), 0);
    let g = graph.read();
    assert_eq!(g.node_count(), 1, "interaction only — nothing was written");
    assert_eq!(g.embedding().unwrap().kind, "fixture", "stamp preserved");
}

#[test]
fn context_len_is_the_length_of_the_context_text() {
    for (content, origin) in [
        ("user schema", Some("ingest context")),
        ("user schema", Some("  padded  ")),
        ("user schema", Some("   ")),
        ("user schema", None),
        ("", None),
    ] {
        assert_eq!(
            context_len(content, origin),
            context_text(content, origin).len(),
            "{content:?} / {origin:?}"
        );
    }
}

/// The context cap is on the context actually embedded: `"{content} —
/// {origin}"` adds 5 bytes, `"Concept: {content}"` adds 9. The check used
/// to add 3 in both cases, so a context up to 6 bytes over the cap passed.
/// A context of exactly the cap is embedded; one byte over is refused before
/// any embed, in both arms.
#[tokio::test]
async fn the_context_cap_counts_the_real_framing_bytes() {
    async fn run(prompt: &str, content_len: usize) -> (Result<DeriveOutcome, LamboError>, usize) {
        let (graph, interaction) = graph_with_interaction("hybrid-context-cap", 1, 0, prompt);
        let embedder = RecordingEmbedder::new();
        let content = "c".repeat(content_len);
        let out = derive(
            graph,
            &SpyStore::with_vector(Vec::new()),
            &embedder,
            &contract("fixture", 1024),
            interaction,
            &agent(),
            &[(content.as_str(), ConceptType::Entity)],
            &ParentOf::none(),
            10,
            SEMANTIC_MATCH_THRESHOLD_DEFAULT,
            None,
        )
        .await;
        (out, embedder.embedded_texts().len())
    }
    // With an origin: content + " — " (5) + origin.
    let at_cap = MAX_HYBRID_CONTEXT_BYTES - 5 - "ctx".len();
    let (out, embeds) = run("ctx", at_cap).await;
    assert_eq!(out.unwrap().embedded, 1);
    assert_eq!(embeds, 1);
    let (out, embeds) = run("ctx", at_cap + 1).await;
    assert!(matches!(out, Err(LamboError::Config(_))), "{out:?}");
    assert_eq!(embeds, 0);

    // No origin (an empty prompt): "Concept: " (9) + content.
    let at_cap = MAX_HYBRID_CONTEXT_BYTES - 9;
    let (out, embeds) = run("", at_cap).await;
    assert_eq!(out.unwrap().embedded, 1);
    assert_eq!(embeds, 1);
    let (out, embeds) = run("", at_cap + 1).await;
    assert!(matches!(out, Err(LamboError::Config(_))), "{out:?}");
    assert_eq!(embeds, 0);
}

#[tokio::test]
async fn first_use_empty_candidates_still_commits_contract() {
    // Cockroach returns this safe empty shape for a missing/unstamped
    // session before the first SetEmbedding commit.
    let (graph, interaction) =
        graph_with_interaction("hybrid-first-use", 1, 0, "first use context");
    let store = SpyStore::with_vector(Vec::new());
    let embedder = FixtureEmbedder::new();
    derive(
        graph.clone(),
        &store,
        &embedder,
        &contract("fixture", 1024),
        interaction,
        &agent(),
        &[("first embedded concept", ConceptType::Entity)],
        &ParentOf::none(),
        10,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        None,
    )
    .await
    .unwrap();

    assert_eq!(store.vector_calls(), 1);
    let mut g = graph.write();
    assert_eq!(g.embedding(), Some(&contract("fixture", 1024)));
    assert_eq!(g.concepts().count(), 1);
    // L82-4 (product decision 2026-08-14): the store had no trusted match,
    // so no merge is recorded — but the vector that was successfully
    // computed IS persisted, which is what makes an organically-derived
    // concept vector-recallable. This assertion used to be `is_none()`;
    // the contract is still committed so every vector lives in one known
    // space.
    assert!(
        g.concepts()
            .all(|concept| concept.embedding.as_ref().is_some_and(|v| v.len() == 1024)),
        "a successfully embedded fresh concept persists its vector (L82-4)"
    );
    assert_eq!(
        g.edges()
            .filter(|e| e.edge_type == EdgeType::Semantic)
            .count(),
        0,
        "no candidate cleared the threshold, so no merge is endorsed"
    );
    assert!(g.drain_log().mutations.iter().any(|mutation| matches!(
        mutation,
        crate::types::Mutation::SetEmbedding {
            embedding: Some(_),
            ..
        }
    )));
}

#[tokio::test]
async fn intervening_graph_mutation_discards_stale_gather_and_replans() {
    let (graph, interaction) = graph_with_interaction("hybrid-epoch-race", 1, 0, "epoch race");
    let embedder = Arc::new(PausingEmbedder {
        started: Notify::new(),
        release: Notify::new(),
        calls: AtomicUsize::new(0),
    });
    let store = Arc::new(SpyStore::with_vector(Vec::new()));
    let task = {
        let graph = graph.clone();
        let embedder = embedder.clone();
        let store = store.clone();
        tokio::spawn(async move {
            derive(
                graph,
                store.as_ref(),
                embedder.as_ref(),
                &contract("fixture", 1024),
                interaction,
                &agent(),
                &[("stale plan", ConceptType::Entity)],
                &ParentOf::none(),
                10,
                SEMANTIC_MATCH_THRESHOLD_DEFAULT,
                None,
            )
            .await
        })
    };
    embedder.started.notified().await;
    graph
        .write()
        .set_root_goal(Some(serde_json::json!("concurrent daemon mutation")));
    embedder.release.notify_one();
    task.await.unwrap().unwrap();

    assert_eq!(embedder.calls.load(Ordering::SeqCst), 2);
    assert_eq!(store.vector_calls(), 2);
    let g = graph.read();
    assert_eq!(g.concepts().count(), 1, "stale attempt wrote nothing");
    g.assert_invariants().unwrap();
}

#[tokio::test]
async fn intervening_synonym_change_discards_stale_gather_and_replans() {
    let (graph, interaction) = graph_with_interaction("hybrid-synonym-race", 1, 0, "synonym race");
    let embedder = Arc::new(PausingEmbedder {
        started: Notify::new(),
        release: Notify::new(),
        calls: AtomicUsize::new(0),
    });
    let store = Arc::new(SpyStore::with_vector(Vec::new()));
    let task = {
        let graph = graph.clone();
        let embedder = embedder.clone();
        let store = store.clone();
        tokio::spawn(async move {
            derive(
                graph,
                store.as_ref(),
                embedder.as_ref(),
                &contract("fixture", 1024),
                interaction,
                &agent(),
                &[("alias", ConceptType::Entity)],
                &ParentOf::none(),
                10,
                SEMANTIC_MATCH_THRESHOLD_DEFAULT,
                None,
            )
            .await
        })
    };
    embedder.started.notified().await;
    graph.write().declare_synonym("alias", "canonical target");
    embedder.release.notify_one();
    task.await.unwrap().unwrap();

    assert_eq!(embedder.calls.load(Ordering::SeqCst), 2);
    assert_eq!(store.vector_calls(), 2);
    let concept = graph.read().concepts().next().unwrap().clone();
    assert_eq!(concept.canonical_key, "canon target");
}

#[tokio::test]
async fn invalid_or_oversized_requests_do_no_external_work() {
    let (graph, interaction) = graph_with_interaction("hybrid-bounds", 1, 0, "bounded");
    let embedder = FailingEmbedder::new();
    let store = SpyStore::with_vector(Vec::new());
    let concepts = vec![("x", ConceptType::Entity); MAX_HYBRID_CONCEPTS + 1];
    let err = derive(
        graph.clone(),
        &store,
        &embedder,
        &contract("fixture", 1024),
        interaction,
        &agent(),
        &concepts,
        &ParentOf::none(),
        10,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, LamboError::Config(_)));
    assert_eq!(embedder.calls(), 0);
    assert_eq!(store.vector_calls(), 0);

    let parents: Vec<(String, String)> = (0..=MAX_HYBRID_PARENT_PAIRS)
        .map(|n| (format!("parent-{n}"), format!("child-{n}")))
        .collect();
    let parent_refs: Vec<(&str, &str)> = parents
        .iter()
        .map(|(parent, child)| (parent.as_str(), child.as_str()))
        .collect();
    let before = graph.read().snapshot();
    let err = derive(
        graph.clone(),
        &store,
        &embedder,
        &contract("fixture", 1024),
        interaction,
        &agent(),
        &[("valid", ConceptType::Entity)],
        &ParentOf::from_pairs(&parent_refs),
        10,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, LamboError::Config(_)));
    assert_eq!(embedder.calls(), 0);
    assert_eq!(store.vector_calls(), 0);
    assert_eq!(graph.read().snapshot(), before, "rejection mutates nothing");

    let err = derive(
        graph,
        &store,
        &embedder,
        &contract("fixture", 1024),
        interaction,
        &agent(),
        &[("valid", ConceptType::Entity)],
        &ParentOf::none(),
        10,
        f64::NAN,
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, LamboError::Config(_)));
    assert_eq!(embedder.calls(), 0);
    assert_eq!(store.vector_calls(), 0);
}
