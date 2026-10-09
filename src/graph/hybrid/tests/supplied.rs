//! #22 PR 3: a derive whose image item carries a supplied vector (design
//! sections 3.3 and 4.4), and the text merge tier that excludes image
//! concepts.

use super::*;
use crate::store::vector_source::VectorCandidates;
use crate::types::{SourceModality, VectorOrigin};

fn live() -> EmbeddingContract {
    contract("fixture", 1024)
}

fn image_source() -> EmbeddingSource {
    EmbeddingSource {
        modality: SourceModality::Image,
        origin: VectorOrigin::Client,
        sha256: None,
        mime: None,
    }
}

/// A supplied vector for `content`, pointing where a text query for `label`
/// points (the fixture's image rule).
fn supplied(content: &str, label: &str) -> SuppliedVector {
    SuppliedVector {
        content: content.into(),
        vector: FixtureEmbedder::new().embed_sync(label),
        contract: live(),
        source: image_source(),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run(
    graph: &Arc<RwLock<Graph>>,
    store: &SpyStore,
    embedder: &dyn Embedder,
    embedding: &EmbeddingContract,
    interaction: NodeId,
    concepts: &[(&str, ConceptType)],
    parent_of: &ParentOf<'_>,
    supplied: Option<&SuppliedVector>,
) -> Result<DeriveOutcome, LamboError> {
    derive_with(
        graph.clone(),
        VectorCandidates::from_store(store),
        embedder,
        embedding,
        interaction,
        &agent(),
        concepts,
        parent_of,
        10,
        SEMANTIC_MATCH_THRESHOLD_DEFAULT,
        supplied,
        None,
    )
    .await
}

/// A concept already in the graph with a vector: `source` makes it an image
/// concept.
fn vectored(
    sess: &str,
    n: u64,
    content: &str,
    owner: NodeId,
    vector: Vec<f32>,
    source: Option<EmbeddingSource>,
) -> Concept {
    Concept {
        id: NodeId(Uuid::from_u64_pair(7, n)),
        session_id: sid(sess),
        content: content.into(),
        canonical_key: crate::graph::canonical::canonical_key(content, |_| None),
        concept_type: ConceptType::Entity,
        origin_interaction: owner,
        origin_agent: agent(),
        created_at: ts(0),
        access_count: 0,
        last_accessed: None,
        gc_survived: 0,
        canonization_status: CanonizationStatus::None,
        blast_radius: None,
        last_demotion_time: None,
        embedding: Some(vector),
        human_confirmed: 0,
        embedding_source: source,
        chunk_group_id: None,
    }
}

#[tokio::test]
async fn an_image_item_takes_its_supplied_vector_and_never_merges() {
    let sess = "supplied-no-merge";
    let (graph, iid) = graph_with_interaction(sess, 1, 0, "outfits");
    graph.write().stamp_embedding(live()).unwrap();
    let text = vectored(
        sess,
        1,
        "red silk saree",
        iid,
        FixtureEmbedder::new().embed_sync("red silk saree"),
        None,
    );
    let text_id = text.id;
    graph.write().insert_concept(text, iid).unwrap();

    // The store would offer a perfect merge target. The image item must not
    // ask for it, let alone take it.
    let store = SpyStore::with_vector(vec![hit(text_id, 1.0)]);
    let embedder = RecordingEmbedder::new();
    let image = supplied("render 17 [image:r17]", "red silk saree");
    let out = run(
        &graph,
        &store,
        &embedder,
        &live(),
        iid,
        &[(image.content.as_str(), ConceptType::Resource)],
        &ParentOf::none(),
        Some(&image),
    )
    .await
    .unwrap();

    assert_eq!(out.created.len(), 1);
    assert!(out.semantic_merged.is_empty(), "an image never merges");
    assert_eq!(out.embedded, 1, "the supplied vector counts as embedded");
    assert_eq!(store.vector_calls(), 0, "no candidate lookup for an image");
    assert!(
        embedder.embedded_texts().is_empty(),
        "the image item is never text-embedded"
    );
    let g = graph.read();
    let Some(Node::Concept(c)) = g.node(out.created[0]) else {
        panic!("created a concept");
    };
    assert_eq!(c.embedding.as_ref(), Some(&image.vector));
    assert_eq!(c.embedding_source, Some(image_source()));
    assert_eq!(c.concept_type, ConceptType::Resource);
    assert!(g.edge_between(text_id, c.id, EdgeType::Semantic).is_none());
    assert!(g.edge_between(c.id, text_id, EdgeType::Semantic).is_none());
    g.assert_invariants().unwrap();
}

#[tokio::test]
async fn a_text_item_never_merges_into_an_image_concept() {
    let sess = "supplied-text-excludes-images";
    let (graph, iid) = graph_with_interaction(sess, 1, 0, "outfits");
    graph.write().stamp_embedding(live()).unwrap();
    let f = FixtureEmbedder::new();
    let picture = vectored(
        sess,
        1,
        "render 17 [image:r17]",
        iid,
        f.embed_sync("x"),
        Some(image_source()),
    );
    let words = vectored(sess, 2, "alpha api", iid, f.embed_sync("y"), None);
    let (picture_id, words_id) = (picture.id, words.id);
    {
        let mut g = graph.write();
        g.insert_concept(picture, iid).unwrap();
        g.insert_concept(words, iid).unwrap();
    }

    // The image outranks the text candidate: the text one is still the merge.
    let out = run(
        &graph,
        &SpyStore::with_vector(vec![hit(picture_id, 0.99), hit(words_id, 0.9)]),
        &RecordingEmbedder::new(),
        &live(),
        iid,
        &[("gamma queries", ConceptType::Entity)],
        &ParentOf::none(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(out.semantic_merged, vec![words_id]);

    // With only the image above the bar, the text concept is fresh and
    // unmerged: a refused merge, never a merge into a picture.
    let out = run(
        &graph,
        &SpyStore::with_vector(vec![hit(picture_id, 0.99)]),
        &RecordingEmbedder::new(),
        &live(),
        iid,
        &[("delta queries", ConceptType::Entity)],
        &ParentOf::none(),
        None,
    )
    .await
    .unwrap();
    assert!(out.semantic_merged.is_empty());
    assert_eq!(out.created.len(), 1);
    let g = graph.read();
    let fresh = out.created[0];
    assert!(g
        .edge_between(picture_id, fresh, EdgeType::Semantic)
        .is_none());
    assert!(g
        .edge_between(fresh, picture_id, EdgeType::Semantic)
        .is_none());
    assert!(
        matches!(g.node(fresh), Some(Node::Concept(c)) if c.embedding.is_some()),
        "the refused merge still keeps its own vector (L82-4)"
    );
}

#[tokio::test]
async fn the_same_image_content_canonical_matches_and_keeps_its_first_vector() {
    let sess = "supplied-dedupe";
    let (graph, iid) = graph_with_interaction(sess, 1, 0, "outfits");
    let first = supplied("render 17 [image:r17]", "red silk saree");
    let concepts = [(first.content.as_str(), ConceptType::Resource)];
    let store = SpyStore::with_vector(Vec::new());
    let embedder = RecordingEmbedder::new();
    let out = run(
        &graph,
        &store,
        &embedder,
        &live(),
        iid,
        &concepts,
        &ParentOf::none(),
        Some(&first),
    )
    .await
    .unwrap();
    let id = out.created[0];
    assert_eq!(
        graph.read().embedding(),
        Some(&live()),
        "the first image stamps the session's space"
    );

    let mut again = supplied("render 17 [image:r17]", "something else");
    again.source.origin = VectorOrigin::Server;
    let out = run(
        &graph,
        &store,
        &embedder,
        &live(),
        iid,
        &concepts,
        &ParentOf::none(),
        Some(&again),
    )
    .await
    .unwrap();
    assert!(out.created.is_empty());
    assert_eq!(out.matched, vec![id]);
    assert_eq!(out.embedded, 0, "a match with a vector keeps it");
    let g = graph.read();
    let Some(Node::Concept(c)) = g.node(id) else {
        panic!("still a concept");
    };
    assert_eq!(c.embedding.as_ref(), Some(&first.vector), "first wins");
    assert_eq!(c.embedding_source, Some(image_source()));
    assert_eq!(g.concepts().count(), 1);
}

#[tokio::test]
async fn a_re_derived_image_repairs_its_missing_vector() {
    let sess = "supplied-repair";
    let (graph, iid) = graph_with_interaction(sess, 1, 0, "outfits");
    graph.write().stamp_embedding(live()).unwrap();
    // An image concept whose vector a quarantine or a
    // `re-embed --drop-image-vectors` nulled, keeping its source.
    let mut quarantined = vectored(
        sess,
        1,
        "render 17 [image:r17]",
        iid,
        vec![0.0; 1024],
        Some(image_source()),
    );
    quarantined.embedding = None;
    let id = quarantined.id;
    graph.write().insert_concept(quarantined, iid).unwrap();

    let image = supplied("render 17 [image:r17]", "red silk saree");
    let out = run(
        &graph,
        &SpyStore::with_vector(Vec::new()),
        &RecordingEmbedder::new(),
        &live(),
        iid,
        &[(image.content.as_str(), ConceptType::Resource)],
        &ParentOf::none(),
        Some(&image),
    )
    .await
    .unwrap();
    assert_eq!(out.matched, vec![id]);
    assert_eq!(out.embedded, 1);
    assert!(matches!(
        graph.read().node(id),
        Some(Node::Concept(c)) if c.embedding.as_ref() == Some(&image.vector)
    ));

    // A TEXT concept that happens to carry the same content is left as it
    // is: only an image concept missing its vector is repaired.
    let sess = "supplied-no-text-repair";
    let (graph, iid) = graph_with_interaction(sess, 1, 0, "outfits");
    graph.write().stamp_embedding(live()).unwrap();
    let mut text = vectored(sess, 1, "render 17 [image:r17]", iid, vec![0.0; 1024], None);
    text.embedding = None;
    let id = text.id;
    graph.write().insert_concept(text, iid).unwrap();
    let out = run(
        &graph,
        &SpyStore::with_vector(Vec::new()),
        &RecordingEmbedder::new(),
        &live(),
        iid,
        &[(image.content.as_str(), ConceptType::Resource)],
        &ParentOf::none(),
        Some(&image),
    )
    .await
    .unwrap();
    assert_eq!((out.matched, out.embedded), (vec![id], 0));
    assert!(matches!(
        graph.read().node(id),
        Some(Node::Concept(c)) if c.embedding.is_none() && c.embedding_source.is_none()
    ));
}

/// AC4 at apply: a supplied vector that does not fit the live contract is an
/// `Embed` refusal (so a replayed intent settles `failed`), and nothing is
/// written.
#[tokio::test]
async fn a_supplied_vector_that_does_not_fit_the_live_contract_is_refused() {
    let content = "render 17 [image:r17]";
    let good = supplied(content, "red silk saree");
    let mut cases: Vec<(&str, SuppliedVector)> = Vec::new();
    for (what, contract) in [
        ("kind", contract("other", 1024)),
        (
            "model",
            EmbeddingContract {
                model: Some("v2".into()),
                ..live()
            },
        ),
        ("dim", contract("fixture", 512)),
    ] {
        cases.push((
            what,
            SuppliedVector {
                contract,
                ..good.clone()
            },
        ));
    }
    cases.push((
        "width",
        SuppliedVector {
            vector: vec![0.5; 512],
            ..good.clone()
        },
    ));
    let mut nan = good.clone();
    nan.vector[3] = f32::NAN;
    cases.push(("non-finite", nan));
    cases.push((
        "zero norm",
        SuppliedVector {
            vector: vec![0.0; 1024],
            ..good.clone()
        },
    ));

    for (what, bad) in cases {
        let (graph, iid) = graph_with_interaction("supplied-refused", 1, 0, "outfits");
        let store = SpyStore::with_vector(Vec::new());
        let err = run(
            &graph,
            &store,
            &RecordingEmbedder::new(),
            &live(),
            iid,
            &[(content, ConceptType::Resource)],
            &ParentOf::none(),
            Some(&bad),
        )
        .await
        .expect_err(what);
        assert!(matches!(err, LamboError::Embed(_)), "{what}: {err:?}");
        assert!(
            !err.to_string().contains("0.5"),
            "{what}: never quotes the vector: {err}"
        );
        let g = graph.read();
        assert_eq!(g.concepts().count(), 0, "{what}: nothing written");
        assert!(g.embedding().is_none(), "{what}: nothing stamped");
    }
}

/// Design section 3.3 (2): the session's stamped space must be the supplied
/// vector's. A session stamped under another contract refuses the image and
/// writes nothing, whichever phase sees it.
#[tokio::test]
async fn a_session_stamped_in_another_space_refuses_the_image() {
    let (graph, iid) = graph_with_interaction("supplied-stamped", 1, 0, "outfits");
    graph
        .write()
        .stamp_embedding(contract("legacy", 1024))
        .unwrap();
    let image = supplied("render 17 [image:r17]", "red silk saree");
    let err = run(
        &graph,
        &SpyStore::with_vector(Vec::new()),
        &RecordingEmbedder::new(),
        &live(),
        iid,
        &[(image.content.as_str(), ConceptType::Resource)],
        &ParentOf::none(),
        Some(&image),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, LamboError::Config(_)), "{err:?}");
    assert_eq!(graph.read().concepts().count(), 0);
}

/// The race the commit-lock check exists for: an image derive plans against
/// an unstamped session, and while it awaits (here, the embed of a
/// `parent_of` end it will create) a text derive under another space
/// commits first. The image derive must not write into that space.
#[tokio::test]
async fn an_image_derive_racing_a_first_writer_in_another_space_writes_nothing() {
    let (graph, iid) = graph_with_interaction("supplied-race", 1, 0, "outfits");
    let pausing = Arc::new(PausingEmbedderForRace::default());
    let image = supplied("render 17 [image:r17]", "red silk saree");

    let image_task = {
        let graph = graph.clone();
        let pausing = pausing.clone();
        let image = image.clone();
        tokio::spawn(async move {
            let pairs = [("wardrobe", image.content.as_str())];
            run(
                &graph,
                &SpyStore::with_vector(Vec::new()),
                pausing.as_ref(),
                &live(),
                iid,
                &[(image.content.as_str(), ConceptType::Resource)],
                &ParentOf::from_pairs(&pairs),
                Some(&image),
            )
            .await
        })
    };
    pausing.started.notified().await;
    let rival = run(
        &graph,
        &SpyStore::with_vector(Vec::new()),
        &RecordingEmbedder::new(),
        &contract("legacy", 1024),
        iid,
        &[("alpha api", ConceptType::Entity)],
        &ParentOf::none(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(rival.created.len(), 1);
    pausing.release.notify_one();

    let err = image_task.await.unwrap().unwrap_err();
    assert!(matches!(err, LamboError::Config(_)), "{err:?}");
    let g = graph.read();
    assert_eq!(g.concepts().count(), 1, "only the rival's concept");
    assert_eq!(g.embedding(), Some(&contract("legacy", 1024)));
    g.assert_invariants().unwrap();
}

/// Pauses its first embed until released (the race above).
#[derive(Debug, Default)]
struct PausingEmbedderForRace {
    started: Notify,
    release: Notify,
    calls: AtomicUsize,
}

#[async_trait]
impl Embedder for PausingEmbedderForRace {
    fn dimensions(&self) -> usize {
        1024
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.started.notify_one();
            self.release.notified().await;
        }
        Ok(FixtureEmbedder::new().embed_sync(text))
    }
}

#[tokio::test]
async fn an_image_derive_needs_vector_search_and_exactly_its_own_concept() {
    let image = supplied("render 17 [image:r17]", "red silk saree");
    let (graph, iid) = graph_with_interaction("supplied-shape", 1, 0, "outfits");
    let err = run(
        &graph,
        &SpyStore::without_vector(),
        &RecordingEmbedder::new(),
        &live(),
        iid,
        &[(image.content.as_str(), ConceptType::Resource)],
        &ParentOf::none(),
        Some(&image),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, LamboError::Config(_)), "{err:?}");
    assert!(err.to_string().contains("VECTOR_SEARCH"), "{err}");

    for concepts in [
        vec![("something else", ConceptType::Entity)],
        vec![
            (image.content.as_str(), ConceptType::Resource),
            ("another", ConceptType::Entity),
        ],
    ] {
        let err = run(
            &graph,
            &SpyStore::with_vector(Vec::new()),
            &RecordingEmbedder::new(),
            &live(),
            iid,
            &concepts,
            &ParentOf::none(),
            Some(&image),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, LamboError::Store(StoreError::Invariant(_))),
            "{err:?}"
        );
    }
    assert_eq!(graph.read().concepts().count(), 0);
}

/// A `parent_of` pair naming the image concept resolves to it, and the
/// pair's other, new end is text-embedded as usual (issue #16 section 2).
#[tokio::test]
async fn an_image_derive_parent_of_end_is_text_embedded_and_links_to_the_image() {
    let (graph, iid) = graph_with_interaction("supplied-parent", 1, 0, "outfits");
    let image = supplied("render 17 [image:r17]", "red silk saree");
    let pairs = [("wardrobe", image.content.as_str())];
    let embedder = RecordingEmbedder::new();
    let out = run(
        &graph,
        &SpyStore::with_vector(Vec::new()),
        &embedder,
        &live(),
        iid,
        &[(image.content.as_str(), ConceptType::Resource)],
        &ParentOf::from_pairs(&pairs),
        Some(&image),
    )
    .await
    .unwrap();
    assert_eq!(out.created.len(), 2);
    assert_eq!(out.embedded, 2);
    let texts = embedder.embedded_texts();
    assert_eq!(texts.len(), 1, "only the parent end is embedded: {texts:?}");
    assert!(texts[0].starts_with("wardrobe"), "{texts:?}");
    let g = graph.read();
    let image_id = g
        .concepts()
        .find(|c| c.embedding_source.is_some())
        .map(|c| c.id)
        .unwrap();
    let parent = g
        .concepts()
        .find(|c| c.content == "wardrobe")
        .map(|c| c.id)
        .unwrap();
    assert!(g
        .edge_between(parent, image_id, EdgeType::Hierarchical)
        .is_some());
    g.assert_invariants().unwrap();
}
