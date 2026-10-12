//! #33 PR 2: `Memory::recall_layered` over real sessions — the unlayered
//! call is the plain recall, a layer's hits merge and name their session,
//! accesses stay per session, and a closed or incompatible layer degrades.

use super::*;
use crate::memory::LayerRef;
use crate::types::RecallHit;

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
    let mem = seeded(
        "layered-plain",
        &[
            ("auth middleware checks tokens", ConceptType::Entity),
            ("auth tokens rotate weekly", ConceptType::Logic),
            ("user schema", ConceptType::Entity),
        ],
    )
    .await;
    for max_tokens in [500, 12, 0] {
        let q = RecallQuery {
            max_tokens,
            ..query("auth tokens")
        };
        let plain = mem.recall(q.clone()).await.unwrap();
        let layered = mem.recall_layered(&[], q).await.unwrap();
        assert_eq!(layered.context, plain.context);
        assert_eq!(layered.warnings, plain.warnings);
        let hits: Vec<RecallHit> = layered.hits.iter().map(|h| h.hit.clone()).collect();
        assert_eq!(hits, plain.hits);
        assert!(layered.hits.iter().all(|h| h.session == *mem.session()));
        assert!(layered.unavailable_layers.is_empty());
    }
    // A `top_k` the pipeline refuses answers as the plain recall does, even
    // with layers.
    let other = seeded("layered-plain-other", &[("auth", ConceptType::Entity)]).await;
    let q = RecallQuery {
        top_k: 0,
        ..query("auth")
    };
    let plain = mem.recall(q.clone()).await.unwrap();
    let layered = mem
        .recall_layered(&[LayerRef::new(&other)], q)
        .await
        .unwrap();
    assert_eq!(layered.warnings, plain.warnings);
    assert!(layered.hits.is_empty());
    mem.close().await.unwrap();
    other.close().await.unwrap();
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
        .recall_as_layer(query("auth tokens"), None, general.embedding_contract())
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
    for w in [f64::NAN, -0.1, f64::INFINITY] {
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
            &[LayerRef::new(&b).with_weight(0.0).with_min_similarity(1.0)],
            query("x")
        )
        .await
        .is_ok());
    a.close().await.unwrap();
    b.close().await.unwrap();
}
