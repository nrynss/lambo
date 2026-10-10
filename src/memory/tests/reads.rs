//! Read paths: recall, canonical memories, stats, events and reload.

use super::*;

// -- canonical_memories / stats / events --------------------------------

#[tokio::test]
async fn canonical_memories_lists_only_canonical_concepts() {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let mem = memory_on(store, "saints").await;

    mem.derive(
        &[
            ("user schema", ConceptType::Entity),
            ("auth middleware", ConceptType::Entity),
        ],
        &ParentOf::none(),
    )
    .await
    .unwrap();
    assert!(mem.canonical_memories().is_empty());

    // Walk `user schema` up the spec §10 state machine through the audited
    // transition path (the same one the canonization task uses).
    let target = {
        let g = mem.graph().read();
        let found = g
            .concepts()
            .find(|c| c.content == "user schema")
            .map(|c| c.id);
        found.unwrap()
    };
    for (from, to) in [
        (CanonizationStatus::None, CanonizationStatus::Candidate),
        (CanonizationStatus::Candidate, CanonizationStatus::Venerable),
        (CanonizationStatus::Venerable, CanonizationStatus::Canonical),
    ] {
        let event = CanonizationEvent {
            id: NodeId::new(),
            session_id: SessionId::new("saints"),
            node_id: target,
            from_status: from,
            to_status: to,
            blast_radius: Some(0),
            occurred_at: Utc::now(),
            last_demotion_time: None,
        };
        mem.graph()
            .write()
            .apply_canonization_transition(event)
            .unwrap();
    }

    let saints = mem.canonical_memories();
    assert_eq!(saints.len(), 1);
    assert_eq!(saints[0].content, "user schema");
    assert_eq!(saints[0].node_id, target);
    assert_eq!(mem.stats().canonical_count, 1);

    mem.close().await.unwrap();
}

#[tokio::test]
async fn stats_expose_flush_lag_and_log_depth() {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let mem = memory_on(store, "stats").await;

    // A fresh session already carries exactly one mutation: `build()`
    // stamped the embedding contract, and that stamp is durable state.
    let baseline = mem.stats().log_depth;
    assert_eq!(baseline, 1, "the embedding-contract stamp");

    mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
        .await
        .unwrap();

    let stats = mem.stats();
    assert!(stats.log_depth > baseline);
    assert!(stats.log_depth > 0, "unflushed mutations must be visible");
    assert!(!stats.degraded);
    assert_eq!(stats.dead_lettered, 0);
    assert_eq!(stats.session, SessionId::new("stats"));
    assert_eq!(stats.concept_count, 1);

    mem.close().await.unwrap();
    // The graph log is empty after the final drain.
    assert_eq!(mem.graph().read().log_len(), 0);
}

#[tokio::test]
async fn events_hands_out_the_pre_spawn_receiver_first() {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let mem = memory_on(store, "events").await;
    // Two subscriptions, both usable; the first is the pre-spawn one.
    let _first = mem.events();
    let _second = mem.events();
    mem.close().await.unwrap();
}

// -- recall / synonyms / reservations -----------------------------------

#[tokio::test]
async fn recall_returns_a_context_block_for_a_derived_concept() {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let mem = memory_on(store, "recall").await;

    mem.derive(
        &[
            ("user schema", ConceptType::Entity),
            ("auth middleware", ConceptType::Entity),
        ],
        &ParentOf::none(),
    )
    .await
    .unwrap();

    let result = mem
        .recall(RecallQuery {
            query: "update user schema".into(),
            top_k: 5,
            max_tokens: 500,
            traversal_depth: 2,
        })
        .await
        .unwrap();

    assert!(
        result.hits.iter().any(|h| h.content == "user schema"),
        "keyword leg must find the derived concept: {result:?}"
    );
    assert!(result.context.contains("user schema"));

    mem.close().await.unwrap();
}

// -- reload -------------------------------------------------------------

#[tokio::test]
async fn a_closed_session_reloads_with_its_graph_and_index() {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());

    let mem = memory_on(store.clone(), "reload").await;
    mem.derive(
        &[
            ("user schema", ConceptType::Entity),
            ("auth middleware", ConceptType::Entity),
        ],
        &ParentOf::none(),
    )
    .await
    .unwrap();
    mem.demote("The caching layer was the bottleneck.", "chunk-1")
        .unwrap();
    mem.close().await.unwrap();

    let reloaded = memory_on(store, "reload").await;
    assert_eq!(reloaded.stats().concept_count, 3);
    assert!(
        !reloaded.index().read().search("caching", 10).is_empty(),
        "load_session rebuilds the index from the same snapshot"
    );
    // A re-derive of an existing concept matches instead of creating.
    let out = reloaded
        .derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
        .await
        .unwrap();
    assert!(out.created.is_empty());
    assert_eq!(out.matched.len(), 1);

    reloaded.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unrelated_recent_only_unscored_hit_keeps_keyword_recall_blended() {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let mem = memory_on(store, "issue-79-recent-only").await;
    let old = mem
        .derive(
            &[("needle keyword", ConceptType::Entity)],
            &ParentOf::none(),
        )
        .await
        .unwrap()
        .created[0];
    mem.settle_daemon().await;
    stop_daemon_for_cold_start(&mem).await;
    let scores = mem.daemon.scores();
    let old_daemon = scores
        .ranked
        .iter()
        .find(|hit| hit.item == old)
        .unwrap()
        .score;
    assert!(old_daemon > 0.0);
    let recent = mem
        .derive(
            &[("distant galaxy", ConceptType::Entity)],
            &ParentOf::none(),
        )
        .await
        .unwrap()
        .created[0];
    let query = RecallQuery {
        query: "needle".into(),
        top_k: 3,
        max_tokens: 10_000,
        traversal_depth: 0,
    };
    for _ in 0..2 {
        let result = mem.recall_detailed(query.clone()).await.unwrap();
        let old_leg = result.legs.get(&old).unwrap();
        let recent_leg = result.legs.get(&recent).unwrap();
        assert!(old_leg.keyword.is_some());
        assert_eq!(recent_leg.keyword, None);
        assert_eq!(recent_leg.vector, None);
        assert!(recent_leg.recent.is_some());
        let old_hit = result.hits.iter().find(|hit| hit.node_id == old).unwrap();
        let expected = 0.5 * (old_daemon + old_leg.keyword.unwrap());
        assert!((old_hit.score - expected).abs() < 1e-9);
    }
    // A lagged score epoch deliberately prevents pipeline cache insertion;
    // the repeated public read above still cannot misclassify recent-only.
    mem.close().await.unwrap();
}
