//! Session info: no secrets, live contract changes and cache headers.

use super::*;

// ---- no secrets in the page ----------------------------------------

#[tokio::test]
async fn session_info_never_leaks_the_dsn_path_or_embedder_url() {
    let store = Arc::new(MemoryStore::new());
    let mut backends = backends_on(store);
    backends.store_cfg.dsn = Some("postgresql://demo:hunter2@crdb.internal:26257/lambo".into());
    backends.store_cfg.path = Some("/var/lib/lambo/private.sqlite".into());
    backends.embedder_cfg.llama_url = Some("http://embed.internal:8080".into());
    let state = Arc::new(AppState {
        session: SessionId::new("t85-secrets"),
        backends,
        exposed: false,
        auth: None,
        freshness: Mutex::new(Freshness {
            fingerprint: 0,
            observed_at: Instant::now(),
        }),
    });
    let (addr, handle) = spawn(state).await;

    let raw = request(addr, "GET", "/api/session").await;
    assert_eq!(raw.status, 200);
    for secret in [
        "hunter2",
        "crdb.internal",
        "postgresql://",
        "/var/lib/lambo",
        "embed.internal",
    ] {
        assert!(
            !raw.body.contains(secret),
            "/api/session leaked '{secret}': {}",
            raw.body
        );
    }

    let info: serde_json::Value = serde_json::from_str(&raw.body).expect("json");
    assert_eq!(info["store"], "memory");
    assert_eq!(info["embedder"], "fixture");
    assert_eq!(info["read_only"], true);
    assert_eq!(info["mode"], "reader");

    handle.abort();
}

#[tokio::test]
async fn h1_live_contract_changes_update_session_pulse_and_keep_recall_fail_closed() {
    let store = Arc::new(MemoryStore::new());
    let stored = EmbeddingContract {
        kind: "fixture".into(),
        model: Some("fixture-model-v1".into()),
        dim: 1024,
    };
    let mem = crate::Memory::builder()
        .session("h1-web-mismatch")
        .agent("writer")
        .store(store.clone() as Arc<dyn GraphStore>)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn crate::embed::Embedder>)
        .embedding_contract(stored.clone())
        .build()
        .await
        .unwrap();
    mem.close().await.unwrap();

    let mut backends = backends_on(store.clone());
    backends.embedding.model = Some("fixture-model-v1".into());
    backends.embedder_cfg.llama_model = backends.embedding.model.clone();
    let state = Arc::new(AppState {
        session: SessionId::new("h1-web-mismatch"),
        backends,
        exposed: false,
        auth: None,
        freshness: Mutex::new(Freshness {
            fingerprint: 0,
            observed_at: Instant::now(),
        }),
    });
    let (addr, handle) = spawn(state).await;

    let raw = request(addr, "GET", "/api/session").await;
    assert_eq!(raw.status, 200);
    let info: serde_json::Value = serde_json::from_str(&raw.body).unwrap();
    assert_eq!(info["embedding_contract"]["status"], "compatible");

    let changed = EmbeddingContract {
        kind: "fixture".into(),
        model: Some("fixture-model-v2".into()),
        dim: 1024,
    };
    // The live writer that changes the contract writes under its own lease:
    // the session was leased when it was seeded, and a lease's token is never
    // reset, so an unleased write would be refused.
    let writer = crate::store::lease::LeaseHolder {
        endpoint: None,
        agent: AgentId::from("contract-writer"),
        pid: 1,
        host: "test".into(),
    };
    let sid = SessionId::new("h1-web-mismatch");
    let crate::store::lease::LeaseOutcome::Acquired(lease) = store
        .acquire_lease(&sid, &writer, std::time::Duration::from_secs(60))
        .await
        .unwrap()
    else {
        panic!("the writer must take the session");
    };
    store
        .flush(
            &crate::types::MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![crate::types::Mutation::SetEmbedding {
                    session_id: sid.clone(),
                    embedding: Some(changed),
                }],
            },
            Some(lease.token),
        )
        .await
        .unwrap();

    let raw = request(addr, "GET", "/api/session").await;
    assert_eq!(raw.status, 200);
    let info: serde_json::Value = serde_json::from_str(&raw.body).unwrap();
    assert_eq!(info["embedding_contract"]["status"], "mismatch");
    assert_eq!(
        info["embedding_contract"]["stored"]["model"],
        "fixture-model-v2"
    );
    assert_eq!(
        info["embedding_contract"]["configured"]["model"],
        "fixture-model-v1"
    );
    let message = info["embedding_contract"]["message"].as_str().unwrap();
    assert!(message.contains("fixture-model-v2"), "{message}");
    assert!(message.contains("fixture-model-v1"), "{message}");
    assert_eq!(info["vector_search"], false);

    let pulse = request(addr, "GET", "/api/pulse").await;
    assert_eq!(pulse.status, 200);
    let pulse: serde_json::Value = serde_json::from_str(&pulse.body).unwrap();
    assert_eq!(pulse["embedding_contract"]["status"], "mismatch");

    for path in ["/api/stats", "/api/graph", "/api/inspect?focus=missing"] {
        let response = request(addr, "GET", path).await;
        assert_eq!(
            response.status, 200,
            "structural route {path} remains available: {}",
            response.body
        );
    }
    let recall = request(addr, "GET", "/api/recall?q=anything").await;
    assert_eq!(recall.status, 502, "mismatched vector recall must refuse");
    assert!(recall.body.contains("fixture-model-v2"), "{}", recall.body);
    assert!(recall.body.contains("fixture-model-v1"), "{}", recall.body);
    // H3: the fail-closed response carries no success payload — no
    // `context`, no `hits`, no `response_annotations` (the error shape
    // renders `error`, never the additive fields).
    for absent in [
        "\"hits\"",
        "\"response_annotations\"",
        "\"included_in_context\"",
        "\"context\":",
    ] {
        assert!(
            !recall.body.contains(absent),
            "mismatch response must not carry '{absent}': {}",
            recall.body
        );
    }

    store
        .flush(
            &crate::types::MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![crate::types::Mutation::SetEmbedding {
                    session_id: sid.clone(),
                    embedding: Some(stored),
                }],
            },
            Some(lease.token),
        )
        .await
        .unwrap();
    store.release_lease(&sid, &writer).await.unwrap();
    let pulse = request(addr, "GET", "/api/pulse").await;
    let pulse: serde_json::Value = serde_json::from_str(&pulse.body).unwrap();
    assert_eq!(pulse["embedding_contract"]["status"], "compatible");
    // The page was rebuilt after this test was written, so these assert the
    // same three properties against the current implementation rather than
    // the previous one's identifiers: it reads the contract off the poll,
    // it says so on a mismatch, and it clears the banner when the contract
    // becomes compatible again instead of leaving a stale warning up.
    assert!(
        APP_JS.contains("applyEmbeddingStatus(p.embedding_contract"),
        "the page must re-read the embedding contract on every poll"
    );
    assert!(
        APP_JS.contains("Search by meaning is off"),
        "the page must state that ranking by meaning is disabled"
    );
    assert!(
        APP_JS.contains("show(box, !!mismatch)"),
        "the banner must clear in the compatible direction, not only appear"
    );

    handle.abort();
}

/// E2E-8: a legacy (unrecorded) session reports `vector_search: false`
/// even on a store that advertises `VECTOR_SEARCH` — its vectors were
/// quarantined at load and the checked read returns an empty pool for an
/// unstamped durable contract. The `embedding_contract.status` field
/// stays `unrecorded` (H1's banner semantics are unchanged); only the
/// derived flag tightens. A compatible session on the same store still
/// reports the leg on.
#[tokio::test]
async fn h1_legacy_unrecorded_sessions_report_vector_search_false() {
    let store = Arc::new(MemoryStore::new());
    // The legacy shape: concept vectors present, no embedding contract
    // stamped — exactly what `load_session` quarantines (store/load.rs).
    let sid = SessionId::new("e2e8-legacy");
    let iid = NodeId::new();
    let cid = NodeId::new();
    let now = Utc::now();
    let mut batch = MutationBatch::new();
    batch.push(Mutation::UpsertNode {
        node: Node::Interaction(Interaction {
            event_time: None,
            id: iid,
            session_id: sid.clone(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: Some("legacy".to_string()),
            previous_id: None,
            created_at: now,
        }),
    });
    let mut c = concept(sid.clone(), cid, iid, "legacy", now);
    c.embedding = Some(vec![0.0; 1024]);
    batch.push(Mutation::UpsertNode {
        node: Node::Concept(c),
    });
    batch.push(Mutation::UpsertEdge {
        edge: edge(NodeId::new(), sid.clone(), iid, cid, EdgeType::Derives, now),
    });
    store
        .flush(&batch, None)
        .await
        .expect("seed legacy session");

    let (addr, handle) = spawn(state_from_backends(
        backends_with_store(Box::new(VectorSearch(Shared(store.clone())))),
        "e2e8-legacy",
        None,
    ))
    .await;

    let info = get_json(addr, "/api/session").await;
    assert_eq!(info["embedding_contract"]["status"], "unrecorded", "{info}");
    assert_eq!(
        info["vector_search"], false,
        "unrecorded session on a VECTOR_SEARCH store must report the leg off: {info}"
    );

    let pulse = get_json(addr, "/api/pulse").await;
    assert_eq!(
        pulse["embedding_contract"]["status"], "unrecorded",
        "{pulse}"
    );
    assert_eq!(
        pulse["vector_search"], false,
        "/api/pulse must agree with /api/session: {pulse}"
    );
    handle.abort();

    // The same store, now stamped compatible: the leg is genuinely on.
    store
        .flush(
            &crate::types::MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![crate::types::Mutation::SetEmbedding {
                    session_id: sid.clone(),
                    embedding: Some(EmbeddingContract {
                        kind: "fixture".into(),
                        model: None,
                        dim: 1024,
                    }),
                }],
            },
            None,
        )
        .await
        .unwrap();
    let (addr, handle) = spawn(state_from_backends(
        backends_with_store(Box::new(VectorSearch(Shared(store.clone())))),
        "e2e8-legacy",
        None,
    ))
    .await;
    let info = get_json(addr, "/api/session").await;
    assert_eq!(info["embedding_contract"]["status"], "compatible", "{info}");
    assert_eq!(
        info["vector_search"], true,
        "a compatible session on a VECTOR_SEARCH store keeps the leg on: {info}"
    );
    handle.abort();
}

#[tokio::test]
async fn api_responses_are_not_cacheable() {
    let store = seed("t85-cache").await;
    let (addr, handle) = spawn(state_on(store, "t85-cache")).await;

    for path in ["/api/session", "/api/stats", "/api/events", "/api/pulse"] {
        let r = request(addr, "GET", path).await;
        assert!(
            r.headers.to_lowercase().contains("cache-control: no-store"),
            "{path} must not be cacheable — it is session memory: {}",
            r.headers
        );
    }

    handle.abort();
}
