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
