//! #33 PR 2: `Memory::recall_layered` over real sessions — the unlayered
//! call is the plain recall, a layer's hits merge and name their session,
//! accesses stay per session, a closed or incompatible layer degrades, the
//! query is embedded once, and a layer answers a dependency question whose
//! anchor it holds.

use super::writes::{ContextTolerantEmbedder, VectorSearchStore};
use super::*;
use crate::memory::LayerRef;
use crate::types::RecallHit;

/// The accesses a session noted and has not applied, as `(id, count)`;
/// takes them, so the next call starts from empty.
fn take_accesses(mem: &Memory) -> Vec<(NodeId, u32)> {
    mem.accesses
        .take()
        .into_iter()
        .map(|(id, n, _)| (id, n))
        .collect()
}

/// An embedder that counts `embed_query` calls (recall's embed), and
/// otherwise is the vector tests' fixture embedder.
struct CountingEmbedder {
    inner: ContextTolerantEmbedder,
    queries: Arc<AtomicUsize>,
}

#[async_trait]
impl Embedder for CountingEmbedder {
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.inner.embed(text).await
    }
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.queries.fetch_add(1, Ordering::SeqCst);
        self.inner.embed_query(text).await
    }
    fn modalities(&self) -> crate::embed::Modalities {
        self.inner.modalities()
    }
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        self.inner.as_any()
    }
}

/// A Hybrid session whose store searches vectors (over the holder graph)
/// when `searches`, with a query-counting embedder.
async fn vector_session(session: &str, searches: bool) -> (Memory, Arc<AtomicUsize>) {
    let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let store: Arc<dyn GraphStore> = if searches {
        Arc::new(VectorSearchStore::graph_ranked(inner))
    } else {
        inner
    };
    let queries = Arc::new(AtomicUsize::new(0));
    let mem = Memory::builder()
        .session(session)
        .agent("agent-a")
        .config(Config {
            daemon_tick_interval: Duration::from_secs(3_600),
            ..Config::default()
        })
        .flush_interval(Duration::from_secs(3_600))
        .match_strategy(MatchStrategy::Hybrid)
        .store(store)
        .embedder(Arc::new(CountingEmbedder {
            inner: ContextTolerantEmbedder(FixtureEmbedder::new()),
            queries: queries.clone(),
        }) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .build()
        .await
        .expect("build");
    (mem, queries)
}

/// A session with a one-hour daemon tick, so a settled score table stays
/// put between two recalls (reads never wake the daemon).
async fn slow_session(store: Arc<dyn GraphStore>, session: &str, kind: &str) -> Memory {
    Memory::builder()
        .session(session)
        .agent("agent-a")
        .config(Config {
            daemon_tick_interval: Duration::from_secs(3_600),
            ..Config::default()
        })
        .flush_interval(Duration::from_secs(3_600))
        .store(store)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract(kind, 1024))
        .build()
        .await
        .expect("build")
}

async fn seeded(session: &str, concepts: &[(&str, ConceptType)]) -> Memory {
    let mem = slow_session(Arc::new(MemoryStore::new()), session, "fixture").await;
    mem.derive(concepts, &ParentOf::none()).await.unwrap();
    mem.settle_daemon().await;
    mem
}

#[tokio::test]
async fn an_unlayered_call_is_byte_identical_to_recall() {
    // Every concept is long, so whichever ranks first is cut at 40 tokens.
    let pad = " and more detail".repeat(20);
    let texts = [
        format!("auth middleware checks tokens{pad}"),
        format!("auth tokens rotate weekly{pad}"),
        format!("user schema{pad}"),
    ];
    let mem = seeded(
        "layered-plain",
        &[
            (texts[0].as_str(), ConceptType::Entity),
            (texts[1].as_str(), ConceptType::Logic),
            (texts[2].as_str(), ConceptType::Entity),
        ],
    )
    .await;
    // 500 fits every block, 40 truncates the long first one (premise
    // checked below), 0 renders nothing.
    let mut truncated = false;
    for max_tokens in [500, 40, 0] {
        let q = RecallQuery {
            max_tokens,
            ..query("auth tokens")
        };
        let plain = mem.recall_detailed(q.clone()).await.unwrap();
        let plain_accesses = take_accesses(&mem);
        let layered = mem.recall_layered(&[], q).await.unwrap();
        let layered_accesses = take_accesses(&mem);
        // Field by field: hits, context, truncation, warnings, legs, cold
        // start, presentation hits (with both context flags) and response
        // annotations.
        assert_eq!(layered.detail, plain);
        assert_eq!(layered.context, plain.context);
        assert_eq!(layered.warnings, plain.warnings);
        let hits: Vec<RecallHit> = layered.hits.iter().map(|h| h.hit.clone()).collect();
        assert_eq!(hits, plain.hits);
        for (h, d) in layered.hits.iter().zip(&plain.detailed) {
            assert_eq!(h.included_in_context, d.included_in_context);
            assert_eq!(h.truncated_in_context, d.truncated_in_context);
            assert_eq!(h.session, *mem.session());
            assert!(h.also.is_empty());
        }
        let legs: Vec<_> = plain
            .hits
            .iter()
            .map(|h| plain.legs.get(&h.node_id).copied())
            .collect();
        assert_eq!(layered.hit_legs, legs);
        assert!(layered.unavailable_layers.is_empty());
        assert_eq!(layered_accesses, plain_accesses);
        truncated |= plain.truncation.is_some();
    }
    assert!(truncated, "premise: one budget truncated the first block");
    mem.close().await.unwrap();
}

#[tokio::test]
async fn a_refused_top_k_is_the_plain_answer_and_a_bad_stack_is_still_refused() {
    let mem = seeded("layered-refused", &[("auth tokens", ConceptType::Entity)]).await;
    let other = seeded("layered-refused-other", &[("auth", ConceptType::Entity)]).await;
    let q = RecallQuery {
        top_k: crate::store::MAX_VECTOR_CANDIDATE_LIMIT + 1,
        ..query("auth")
    };
    let plain = mem.recall_detailed(q.clone()).await.unwrap();
    assert!(
        plain.hits.is_empty(),
        "premise: the pipeline refuses this top_k"
    );
    let layered = mem
        .recall_layered(&[LayerRef::new(&other)], q.clone())
        .await
        .unwrap();
    assert_eq!(layered.detail, plain);
    assert_eq!(layered.warnings, plain.warnings);
    assert!(layered.hits.is_empty());
    // The stack is checked first: a bad stack is refused whatever top_k is.
    assert!(matches!(
        mem.recall_layered(&[LayerRef::new(&mem)], q).await,
        Err(LamboError::Config(_))
    ));
    mem.close().await.unwrap();
    other.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_query_is_embedded_once_for_every_layer() {
    let (project, p_queries) = vector_session("layered-embed-p", true).await;
    let (team, t_queries) = vector_session("layered-embed-t", true).await;
    let (general, g_queries) = vector_session("layered-embed-g", true).await;
    for (mem, text) in [
        (&project, "auth middleware checks tokens"),
        (&team, "auth tokens rotate weekly"),
        (&general, "auth rulings live in general"),
    ] {
        mem.derive(&[(text, ConceptType::Logic)], &ParentOf::none())
            .await
            .unwrap();
        mem.settle_daemon().await;
    }
    for q in [&p_queries, &t_queries, &g_queries] {
        q.store(0, Ordering::SeqCst);
    }
    let r = project
        .recall_layered(
            &[LayerRef::new(&team), LayerRef::new(&general)],
            query("auth tokens"),
        )
        .await
        .unwrap();
    assert_eq!(
        p_queries.load(Ordering::SeqCst),
        1,
        "embedded once, by the caller"
    );
    assert_eq!(t_queries.load(Ordering::SeqCst), 0);
    assert_eq!(g_queries.load(Ordering::SeqCst), 0);
    // Both layers searched with the caller's vector.
    for (h, legs) in r.hits.iter().zip(&r.hit_legs) {
        assert!(legs.is_some_and(|l| l.vector.is_some()), "{h:?}");
    }
    assert_eq!(r.hits.len(), 3, "{:?}", r.hits);
    for mem in [&project, &team, &general] {
        mem.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_caller_without_vector_search_still_embeds_once_for_its_layers() {
    let (project, p_queries) = vector_session("layered-novec-p", false).await;
    let (general, g_queries) = vector_session("layered-novec-g", true).await;
    project
        .derive(
            &[("auth middleware checks tokens", ConceptType::Logic)],
            &ParentOf::none(),
        )
        .await
        .unwrap();
    general
        .derive(
            &[("auth tokens rotate weekly", ConceptType::Logic)],
            &ParentOf::none(),
        )
        .await
        .unwrap();
    project.settle_daemon().await;
    general.settle_daemon().await;
    p_queries.store(0, Ordering::SeqCst);
    g_queries.store(0, Ordering::SeqCst);
    let plain = project.recall_detailed(query("auth tokens")).await.unwrap();
    assert_eq!(
        p_queries.load(Ordering::SeqCst),
        0,
        "premise: no own vector leg"
    );
    let r = project
        .recall_layered(&[LayerRef::new(&general)], query("auth tokens"))
        .await
        .unwrap();
    assert_eq!(p_queries.load(Ordering::SeqCst), 1);
    assert_eq!(g_queries.load(Ordering::SeqCst), 0);
    for (h, legs) in r.hits.iter().zip(&r.hit_legs) {
        let has_vector = legs.is_some_and(|l| l.vector.is_some());
        // The caller's side ran without a vector, as its plain recall does;
        // the layer's side searched with the caller's.
        assert_eq!(has_vector, h.session == *general.session(), "{h:?}");
    }
    let own: Vec<RecallHit> = r
        .hits
        .iter()
        .filter(|h| h.session == *project.session())
        .map(|h| h.hit.clone())
        .collect();
    assert_eq!(own, plain.hits);
    project.close().await.unwrap();
    general.close().await.unwrap();
}

#[tokio::test]
async fn a_layer_answers_a_dependency_question_whose_anchor_it_holds() {
    let project = seeded(
        "layered-dep-project",
        &[("auth middleware checks tokens", ConceptType::Entity)],
    )
    .await;
    let general = slow_session(
        Arc::new(MemoryStore::new()),
        "layered-dep-general",
        "fixture",
    )
    .await;
    // `rotated the prod tokens` depends on the ruling (a Hierarchical edge
    // from it): the ruling is the anchor, the action its dependent.
    general
        .derive(
            &[
                ("weekly token rotation ruling", ConceptType::Constraint),
                ("rotated the prod tokens", ConceptType::Resource),
            ],
            &ParentOf::from_pairs(&[("weekly token rotation ruling", "rotated the prod tokens")]),
        )
        .await
        .unwrap();
    general.settle_daemon().await;
    let q = RecallQuery {
        traversal_depth: 1,
        ..query("what depends on weekly token rotation ruling")
    };
    let r = project
        .recall_layered(&[LayerRef::new(&general)], q)
        .await
        .unwrap();
    let dependent = r
        .hits
        .iter()
        .find(|h| h.hit.content == "rotated the prod tokens")
        .unwrap_or_else(|| panic!("the layer's dependent is admitted: {:?}", r.hits));
    assert_eq!(dependent.session, *general.session());
    let headline = format!(
        "[layered-dep-general] {}",
        crate::recall::dispatch::traversal_headline(1)
    );
    assert!(r.warnings.contains(&headline), "{:?}", r.warnings);
    assert!(r
        .detail
        .response_annotations
        .iter()
        .any(|a| a.kind == crate::recall::detail::AnnotationKind::Traversal && a.text == headline));
    project.close().await.unwrap();
    general.close().await.unwrap();
}

#[tokio::test]
async fn a_layer_hit_merges_names_its_session_and_counts_its_own_access() {
    let project = seeded(
        "layered-project",
        &[("auth middleware checks tokens", ConceptType::Entity)],
    )
    .await;
    let general = seeded(
        "layered-general",
        &[
            ("auth tokens rotate weekly", ConceptType::Logic),
            // No keyword evidence for the query: kept out by the floor even
            // though it is the layer's most recent write (no recent leg).
            ("zebra crossing", ConceptType::Observation),
        ],
    )
    .await;
    let r = project
        .recall_layered(&[LayerRef::new(&general)], query("auth tokens"))
        .await
        .unwrap();
    let sessions: Vec<&str> = r.hits.iter().map(|h| h.session.as_str()).collect();
    assert!(sessions.contains(&"layered-project"), "{sessions:?}");
    assert!(sessions.contains(&"layered-general"), "{sessions:?}");
    assert!(
        r.hits.iter().all(|h| h.hit.content != "zebra crossing"),
        "{:?}",
        r.hits
    );
    assert!(
        r.context.contains(", from layered-general)"),
        "{}",
        r.context
    );
    assert!(r.unavailable_layers.is_empty());
    // #30: each session noted only its own returned hits.
    let own = |s: &str| r.hits.iter().filter(|h| h.session.as_str() == s).count();
    assert_eq!(project.unapplied_accesses(), own("layered-project"));
    assert_eq!(general.unapplied_accesses(), own("layered-general"));

    // Route::Layer runs no recent leg: the plain recall finds the recent
    // `zebra crossing`, the layer's side of a layered recall does not.
    let plain = general.recall(query("auth tokens")).await.unwrap();
    assert!(plain.hits.iter().any(|h| h.content == "zebra crossing"));
    let side = general
        .recall_as_layer(
            query("auth tokens"),
            None,
            general.embedding_contract(),
            None,
        )
        .await
        .unwrap();
    assert!(side
        .recall
        .hits
        .iter()
        .all(|h| h.content != "zebra crossing"));
    assert_eq!(side.identities.len(), side.recall.hits.len());

    project.close().await.unwrap();
    general.close().await.unwrap();
}

#[tokio::test]
async fn a_closed_or_incompatible_layer_degrades_to_the_plain_answer() {
    let project = seeded(
        "layered-degrade",
        &[
            ("auth middleware checks tokens", ConceptType::Entity),
            ("auth tokens rotate weekly", ConceptType::Logic),
        ],
    )
    .await;
    let closed = seeded("layered-closed", &[("auth closed", ConceptType::Entity)]).await;
    closed.close().await.unwrap();
    let foreign = slow_session(Arc::new(MemoryStore::new()), "layered-foreign", "fixture-b").await;
    foreign
        .derive(&[("auth foreign", ConceptType::Entity)], &ParentOf::none())
        .await
        .unwrap();

    let plain = project.recall(query("auth tokens")).await.unwrap();
    for layer in [&closed, &foreign] {
        let r = project
            .recall_layered(&[LayerRef::new(layer)], query("auth tokens"))
            .await
            .unwrap();
        let name = layer.session().to_string();
        assert_eq!(r.unavailable_layers, [layer.session().clone()]);
        assert_eq!(r.context, plain.context);
        let hits: Vec<RecallHit> = r.hits.iter().map(|h| h.hit.clone()).collect();
        assert_eq!(hits, plain.hits);
        let mut expected = plain.warnings.clone();
        expected.push(format!(
            "recall: layer {name} is unavailable; results are from layered-degrade only"
        ));
        assert_eq!(r.warnings, expected);
    }
    project.close().await.unwrap();
    foreign.close().await.unwrap();
}

#[tokio::test]
async fn bad_layer_stacks_are_refused() {
    let a = seeded("layered-a", &[("x", ConceptType::Entity)]).await;
    let b = seeded("layered-b", &[("x", ConceptType::Entity)]).await;
    let refused = |r: Result<LayeredRecall, LamboError>| matches!(r, Err(LamboError::Config(_)));
    assert!(refused(
        a.recall_layered(&[LayerRef::new(&a)], query("x")).await
    ));
    assert!(refused(
        a.recall_layered(&[LayerRef::new(&b), LayerRef::new(&b)], query("x"))
            .await
    ));
    for w in [f64::NAN, -0.1, 0.0, f64::INFINITY] {
        assert!(refused(
            a.recall_layered(&[LayerRef::new(&b).with_weight(w)], query("x"))
                .await
        ));
    }
    for s in [f64::NAN, -0.1, 1.1] {
        assert!(refused(
            a.recall_layered(&[LayerRef::new(&b).with_min_similarity(s)], query("x"))
                .await
        ));
    }
    assert!(a
        .recall_layered(
            &[LayerRef::new(&b)
                .with_weight(f64::MIN_POSITIVE)
                .with_min_similarity(1.0)],
            query("x")
        )
        .await
        .is_ok());
    a.close().await.unwrap();
    b.close().await.unwrap();
}
