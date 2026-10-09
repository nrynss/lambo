//! Flush/load round trips, row codecs, batch replay semantics and
//! canonization events.

use super::*;

#[tokio::test]
async fn flush_stats_write_then_read_round_trips_sqlite() {
    // T85-3: a writer publishes flush stats into the durable table; a
    // reader (possibly another process) reads them back. Absent row =
    // honest `None` (n/a).
    let store = test_store();
    store.init_schema().await.unwrap();
    let sid = SessionId::from("stats-roundtrip");

    assert_eq!(store.read_flush_stats(&sid).await.unwrap(), None);

    let stats = SessionFlushStats {
        flush_lag_ms: 42,
        log_depth: 7,
    };
    store.write_flush_stats(&sid, &stats).await.unwrap();
    assert_eq!(store.read_flush_stats(&sid).await.unwrap(), Some(stats));

    // A different session has no row → None (n/a), never fabricated 0.
    assert_eq!(
        store
            .read_flush_stats(&SessionId::from("other"))
            .await
            .unwrap(),
        None
    );

    // Re-publish converges (idempotent upsert), the whole row is replaced.
    let later = SessionFlushStats {
        flush_lag_ms: 99,
        log_depth: 1,
    };
    store.write_flush_stats(&sid, &later).await.unwrap();
    assert_eq!(store.read_flush_stats(&sid).await.unwrap(), Some(later));
}

#[tokio::test]
async fn load_missing_session_errors() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let err = store
        .load_session(&SessionId::from("nope"))
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::SessionNotFound(_)));
}

/// XP-8: `root_goal` survives flush → load through the **mutation path**.
///
/// Before `Mutation::SetRootGoal` the goal reached a store only via the
/// full-snapshot `seed` path, so a session reloaded from the write-behind log
/// came back with no goal — drift detection silently stopped and GC's
/// root-goal exclusion emptied. Covers the array shape (ALGO-6), a
/// last-write-wins replacement, and the explicit clear.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn root_goal_roundtrips_flush_and_load() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let sid = SessionId::from("goal-roundtrip");
    let ts = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
    let goal = serde_json::json!(["launch the product", "ship the API"]);
    let batch = MutationBatch {
        mutation_epoch: 0,
        gc_mark: Default::default(),
        mutations: vec![
            plant_interaction(&sid, NodeId::new(), None, ts),
            Mutation::SetRootGoal {
                session_id: sid.clone(),
                goal: Some(goal.clone()),
            },
        ],
    };
    store.flush(&batch, None).await.unwrap();
    let loaded = store.load_session(&sid).await.unwrap();
    assert_eq!(
        loaded.root_goal.as_ref(),
        Some(&goal),
        "the array goal must survive flush→load (XP-8 / ALGO-6)"
    );

    // Last write wins, and a clear is durable (not "no change").
    let replace = MutationBatch {
        mutation_epoch: 0,
        gc_mark: Default::default(),
        mutations: vec![Mutation::SetRootGoal {
            session_id: sid.clone(),
            goal: Some(serde_json::json!("only this one")),
        }],
    };
    store.flush(&replace, None).await.unwrap();
    assert_eq!(
        store.load_session(&sid).await.unwrap().root_goal,
        Some(serde_json::json!("only this one"))
    );
    let clear = MutationBatch {
        mutation_epoch: 0,
        gc_mark: Default::default(),
        mutations: vec![Mutation::SetRootGoal {
            session_id: sid.clone(),
            goal: None,
        }],
    };
    store.flush(&clear, None).await.unwrap();
    assert_eq!(store.load_session(&sid).await.unwrap().root_goal, None);

    #[cfg(feature = "store-memory")]
    {
        let memory = MemoryStore::new();
        memory.flush(&batch, None).await.unwrap();
        let want = memory.load_session(&sid).await.unwrap();
        assert_eq!(
            want.root_goal.as_ref(),
            Some(&goal),
            "MemoryStore applies SetRootGoal identically"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn embedding_contract_roundtrips_flush_and_load() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let sid = SessionId::from("embedding-roundtrip");
    let contract = crate::types::EmbeddingContract {
        kind: "fixture".into(),
        model: Some("fixture-v1".into()),
        dim: 1024,
    };
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![
                    plant_interaction(
                        &sid,
                        NodeId::new(),
                        None,
                        Utc.timestamp_opt(1_752_000_000, 0).unwrap(),
                    ),
                    Mutation::SetEmbedding {
                        session_id: sid.clone(),
                        embedding: Some(contract.clone()),
                    },
                ],
            },
            None,
        )
        .await
        .unwrap();

    let loaded = store.load_session(&sid).await.unwrap();
    assert_eq!(loaded.embedding, Some(contract.clone()));
    let reloaded = crate::graph::Graph::from_snapshot(loaded).unwrap();
    let incompatible = crate::types::EmbeddingContract {
        kind: "bedrock".into(),
        model: Some("amazon.titan-embed-text-v2:0".into()),
        dim: 1024,
    };
    assert!(reloaded
        .embedding()
        .unwrap()
        .ensure_compatible(&incompatible)
        .is_err());
    let err =
        crate::resolve::assert_session_embedding_compatible(reloaded.embedding(), &incompatible)
            .unwrap_err();
    let text = err.to_string();
    assert!(text.contains("fixture-v1"), "{text}");
    assert!(text.contains("amazon.titan-embed-text-v2:0"), "{text}");
    assert!(text.contains("--allow-embedding-mismatch"), "{text}");

    #[cfg(feature = "store-memory")]
    {
        let memory = MemoryStore::new();
        memory
            .flush(
                &MutationBatch {
                    mutation_epoch: 0,
                    gc_mark: Default::default(),
                    mutations: vec![Mutation::SetEmbedding {
                        session_id: sid.clone(),
                        embedding: Some(contract.clone()),
                    }],
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            memory.load_session(&sid).await.unwrap().embedding,
            Some(contract)
        );
    }
}

/// Acceptance (CON-8): `Concept.embedding` survives flush → load on SQLite
/// (the column is now written and read), and the loaded snapshot deep-equals
/// the MemoryStore oracle on the same batch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concept_embedding_roundtrips_flush_and_load() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let sid = SessionId::from("embed-roundtrip");
    let i1 = NodeId::new();
    let c1 = NodeId::new();
    // Whole-second timestamps: the SQLite round-trip contract truncates to
    // milliseconds, so a fresh Utc::now() would break snapshot equality.
    let ts = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
    let emb = vec![0.25, -0.5, 1.0, 0.0];
    let batch = MutationBatch {
        mutation_epoch: 0,
        gc_mark: Default::default(),
        mutations: vec![
            plant_interaction(&sid, i1, None, ts),
            Mutation::UpsertNode {
                node: NodeKind::Concept(Concept {
                    id: c1,
                    session_id: sid.clone(),
                    content: "embedded concept".into(),
                    canonical_key: "embedded concept".into(),
                    concept_type: ConceptType::Entity,
                    origin_interaction: i1,
                    origin_agent: AgentId::from("a"),
                    created_at: ts,
                    access_count: 0,
                    last_accessed: None,
                    gc_survived: 0,
                    canonization_status: CanonizationStatus::None,
                    blast_radius: None,
                    last_demotion_time: None,
                    embedding: Some(emb.clone()),
                    human_confirmed: 0,
                    embedding_source: None,
                    chunk_group_id: None,
                }),
            },
        ],
    };
    store.flush(&batch, None).await.unwrap();
    let loaded = store.load_session(&sid).await.unwrap();
    assert_eq!(loaded.concepts.len(), 1);
    assert_eq!(
        loaded.concepts[0].embedding.as_deref(),
        Some(emb.as_slice()),
        "Concept.embedding must survive flush→load (CON-8)"
    );
    #[cfg(feature = "store-memory")]
    {
        let memory = MemoryStore::new();
        memory.flush(&batch, None).await.unwrap();
        let want = memory.load_session(&sid).await.unwrap();
        assert_eq!(
            loaded, want,
            "sqlite snapshot deep-equals the MemoryStore oracle (embedding included)"
        );
    }
}

/// Upgrade regression: pre-contract durable rows may contain vectors. They
/// remain loadable, but startup quarantines the unknown vectors rather than
/// guessing a model from their width.
#[tokio::test]
async fn legacy_vectors_without_contract_are_quarantined_on_materialization() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let sid = SessionId::from("legacy-vector-upgrade");
    let interaction = NodeId::new();
    let concept = NodeId::new();
    let ts = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
    let mut concept_mutation = plant_concept(
        &sid,
        concept,
        interaction,
        "legacy embedded concept",
        ConceptType::Entity,
        ts,
    );
    let Mutation::UpsertNode {
        node: NodeKind::Concept(ref mut value),
    } = concept_mutation
    else {
        unreachable!()
    };
    value.embedding = Some(vec![0.1, 0.2, 0.3]);
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![
                    plant_interaction(&sid, interaction, None, ts),
                    concept_mutation,
                    Mutation::UpsertEdge {
                        edge: Edge {
                            event_time: None,
                            id: NodeId::new(),
                            session_id: sid.clone(),
                            source: interaction,
                            target: concept,
                            edge_type: EdgeType::Derives,
                            weight: 1.0,
                            reinforcements: 1,
                            created_at: ts,
                            last_reinforced: ts,
                        },
                    },
                ],
            },
            None,
        )
        .await
        .unwrap();

    let raw = store.load_session(&sid).await.unwrap();
    assert!(raw.embedding.is_none());
    assert!(raw.concepts[0].embedding.is_some());
    let loaded = load_session_async(&store, &sid).await.unwrap();
    assert!(loaded.graph.embedding().is_none());
    assert!(loaded.graph.concepts().all(|c| c.embedding.is_none()));
    assert!(loaded.graph.snapshot().concepts[0].embedding.is_none());

    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![Mutation::SetEmbedding {
                    session_id: sid.clone(),
                    embedding: Some(EmbeddingContract {
                        kind: "fixture".into(),
                        model: Some("fixture-v1".into()),
                        dim: 3,
                    }),
                }],
            },
            None,
        )
        .await
        .unwrap();
    let migrated = store.load_session(&sid).await.unwrap();
    assert!(migrated.concepts[0].embedding.is_none());
    assert!(migrated.embedding.is_some());
}

#[cfg(feature = "fixtures")]
#[tokio::test]
async fn oversized_seed_embedding_dimension_fails_before_transaction() {
    let store = test_store();
    let err = store
        .seed(&GraphSnapshot {
            session_id: SessionId::from("oversized-dim"),
            embedding: Some(EmbeddingContract {
                kind: "fixture".into(),
                model: None,
                dim: usize::MAX,
            }),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Invariant(_)));
}

/// Acceptance (STORE-1, offline gate): the full-snapshot `seed` path persists
/// `GraphSnapshot.embedding` into `sessions.embedding_{kind,model,dim}`, and a
/// later flush (which only ensures the session row) does not clobber it — the
/// SQLite twin of the live cockroach conformance check.
#[cfg(feature = "fixtures")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seed_load_preserves_embedding_contract() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let sid = SessionId::from("seed-embed-contract");
    let i1 = NodeId::new();
    let c1 = NodeId::new();
    let ts = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
    // `dim` matches the seeded concept's vector width below. It used to read
    // 1024 against a 3-wide vector — incidental to what this test asserts
    // (STORE-1 contract persistence + CON-8 embedding round-trip), but a
    // snapshot no writer should be able to produce: since F-R1-1 the width gate
    // refuses it on the seed path too, which is the point of the gate.
    let contract = EmbeddingContract {
        kind: "bge_m3".into(),
        model: Some("BAAI/bge-m3".into()),
        dim: 3,
    };
    store
        .seed(&GraphSnapshot {
            session_id: sid.clone(),
            interactions: vec![Interaction {
                event_time: None,
                id: i1,
                session_id: sid.clone(),
                agent_id: AgentId::from("a"),
                prompt_text: Some("seed".into()),
                previous_id: None,
                created_at: ts,
            }],
            concepts: vec![Concept {
                id: c1,
                session_id: sid.clone(),
                content: "seeded".into(),
                canonical_key: "seeded".into(),
                concept_type: ConceptType::Entity,
                origin_interaction: i1,
                origin_agent: AgentId::from("a"),
                created_at: ts,
                access_count: 0,
                last_accessed: None,
                gc_survived: 0,
                canonization_status: CanonizationStatus::None,
                blast_radius: None,
                last_demotion_time: None,
                embedding: Some(vec![0.1, 0.2, 0.3]),
                human_confirmed: 0,
                embedding_source: None,
                chunk_group_id: None,
            }],
            embedding: Some(contract.clone()),
            ..Default::default()
        })
        .await
        .unwrap();
    let loaded = store.load_session(&sid).await.unwrap();
    assert_eq!(
        loaded.embedding.as_ref(),
        Some(&contract),
        "seed must persist the embedding contract (STORE-1)"
    );
    assert_eq!(
        loaded.concepts[0].embedding.as_deref(),
        Some([0.1f32, 0.2, 0.3].as_slice()),
        "seeded concept embedding round-trips through the full-snapshot path"
    );
    // A later flush (session-row ensure only) must not clobber the contract.
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![plant_interaction(
                    &sid,
                    NodeId::new(),
                    None,
                    Utc.timestamp_opt(1_752_003_600, 0).unwrap(),
                )],
            },
            None,
        )
        .await
        .unwrap();
    let loaded = store.load_session(&sid).await.unwrap();
    assert_eq!(
        loaded.embedding.as_ref(),
        Some(&contract),
        "flush must not clobber a seeded embedding contract"
    );
    assert_eq!(loaded.interactions.len(), 2);
}

/// Acceptance: mutations-batch.json flush + load round-trip — the SQLite
/// snapshot deep-equals the MemoryStore oracle on the same batch.
#[cfg(feature = "fixtures")]
#[tokio::test]
async fn mutations_batch_roundtrip_matches_memory() {
    let batch: MutationBatch = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/mutations-batch.json"
    )))
    .unwrap();

    let sqlite = test_store();
    sqlite.init_schema().await.unwrap();
    sqlite.flush(&batch, None).await.unwrap();

    let memory = MemoryStore::new();
    memory.flush(&batch, None).await.unwrap();

    let sid = SessionId::from("session-mutations");
    let got = sqlite.load_session(&sid).await.unwrap();
    let want = memory.load_session(&sid).await.unwrap();
    assert_eq!(got, want);

    // Spot-check the expected shape (fixture: delete_edge 70051 removes
    // the Temporal edge; delete_node 7003 also removes the incident
    // Dependency edge 70053 → target 7003, matching MemoryStore; only the
    // Derives edge 70052 survives).
    assert_eq!(got.interactions.len(), 2);
    assert_eq!(got.concepts.len(), 1, "deleted concept must be gone");
    assert_eq!(got.concepts[0].content, "kept concept");
    assert_eq!(got.edges.len(), 1, "deleted edge must be gone");
    assert_eq!(got.edges[0].edge_type, EdgeType::Derives);
    assert_eq!(
        got.canonization_events[0].to_status,
        CanonizationStatus::Candidate
    );
}

/// Acceptance: flush -> load round-trip deep-equals the graph (T3.5 shape
/// reused against SqliteStore). The graph is built through the real write
/// path (derive / demote / transition), drained, flushed, and loaded back
/// via `load_session`; the loaded session must deep-equal the pre-flush
/// snapshot (minus RAM-local synonyms/reservations — S5), including the
/// demoted observations' `chunk_group_id` (T5.2 contract).
// Multi-thread flavor: load_session runs the store future on a worker
// thread with its own current-thread runtime (see load.rs). sqlx returns
// pool connections via a spawned task; a current-thread runtime that is
// blocked joining that worker never runs the return task, so a cross-
// runtime acquire would time out. Multi-thread keeps other workers polling.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flush_load_roundtrip_deep_equals_graph() {
    let store = test_store();
    store.init_schema().await.unwrap();

    let sid = SessionId::from("roundtrip-sqlite");
    let mut g = crate::graph::Graph::new(sid.clone());

    let ts = |minutes: i64| {
        let base = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
        base + chrono::Duration::minutes(minutes)
    };
    let i1 = NodeId::new();
    g.insert_interaction(Interaction {
        event_time: None,
        id: i1,
        session_id: sid.clone(),
        agent_id: AgentId::from("agent-a"),
        prompt_text: Some("prompt 1".into()),
        previous_id: None,
        created_at: ts(0),
    })
    .unwrap();
    let i2 = NodeId::new();
    g.insert_interaction(Interaction {
        event_time: None,
        id: i2,
        session_id: sid.clone(),
        agent_id: AgentId::from("agent-a"),
        prompt_text: Some("prompt 2".into()),
        previous_id: Some(i1),
        created_at: ts(5),
    })
    .unwrap();
    derive(
        &mut g,
        i2,
        &AgentId::from("agent-a"),
        &[
            ("user schema", ConceptType::Entity),
            ("api layer", ConceptType::Logic),
        ],
        &ParentOf::from_pairs(&[("user schema", "api layer")]),
        10,
    )
    .unwrap();
    let observations = demote(
        &mut g,
        i2,
        &AgentId::from("agent-a"),
        "Drift note. Second drift note.",
        "chunk-1",
    )
    .unwrap();
    assert_eq!(observations.len(), 2);
    let user_schema_id = g
        .concepts()
        .find(|c| c.content == "user schema")
        .expect("derive created it")
        .id;
    g.apply_canonization_transition(CanonizationEvent {
        id: NodeId::new(),
        session_id: sid.clone(),
        node_id: user_schema_id,
        from_status: CanonizationStatus::None,
        to_status: CanonizationStatus::Candidate,
        blast_radius: Some(2),
        last_demotion_time: None,
        occurred_at: ts(12),
    })
    .unwrap();
    // RAM-local metadata that has no Mutation kind (S5).
    g.declare_synonym("us", "user schema");
    reserve(
        &mut g,
        user_schema_id,
        &AgentId::from("agent-b"),
        Duration::from_secs(3600),
        ts(10),
    )
    .unwrap();

    g.assert_invariants().unwrap();
    let mut expected = g.snapshot();
    expected.synonyms.clear();
    expected.reservations.clear();
    let epoch_at_drain = g.epoch();
    let batch = g.drain_log();
    assert!(!batch.is_empty());

    store.flush(&batch, None).await.unwrap();
    let loaded = load_session(&store, &sid).unwrap();

    assert_eq!(loaded.graph.snapshot(), expected);
    assert_eq!(loaded.graph.log_len(), 0, "load must not seed mutations");
    // Issue #17: the batch's stamp is durable, so the load resumes the
    // mutation accounting instead of restarting it (the log stays empty —
    // a loaded session's history is already durable).
    assert_eq!(loaded.graph.epoch(), epoch_at_drain);
    loaded.graph.assert_invariants().unwrap();
    assert_eq!(loaded.graph.synonyms().count(), 0);
    assert_eq!(loaded.graph.reservations().len(), 0);
    // T5.2 contract: the demote chunk id SURVIVES the flush→load round-trip
    // (the P3 wave 2 schema remediation added concepts.chunk_group_id).
    for c in loaded.graph.concepts() {
        if c.concept_type == ConceptType::Observation {
            assert_eq!(
                c.chunk_group_id.as_deref(),
                Some("chunk-1"),
                "demoted observation must keep its chunk_group_id across flush→load"
            );
        } else {
            assert_eq!(
                c.chunk_group_id, None,
                "non-Observation concepts carry no chunk group"
            );
        }
    }
    // Index rebuilt from the snapshot agrees with a reference and finds
    // both observations.
    let reference = crate::graph::index::InvertedIndex::from_snapshot(&expected);
    for q in ["user schema", "api layer", "drift"] {
        assert_eq!(loaded.index.search(q, 10), reference.search(q, 10));
    }
    let drift: Vec<NodeId> = loaded
        .index
        .search("drift", 10)
        .into_iter()
        .map(|s| s.item)
        .collect();
    assert_eq!(drift.len(), 2, "both observations indexed");
}

/// Issue #17: the `sessions` row carries the durable mutation counter.
/// flush stamps the batch's absolute watermark, `load_session` reads it
/// back (that is what a writer restart resumes from), and a stale stamp —
/// a replayed or retained-then-retried older batch — must never rewind the
/// counter (the flush-replay contract, applied to the stamp).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flush_stamps_and_load_resumes_the_mutation_epoch() {
    let store = test_store();
    store.init_schema().await.unwrap();

    let sid = SessionId::from("mutation-epoch");
    let i1 = NodeId::new();
    let c1 = NodeId::new();
    let ts = Utc.with_ymd_and_hms(2026, 8, 20, 12, 0, 0).unwrap();
    let batch = MutationBatch {
        mutation_epoch: 42,
        gc_mark: Default::default(),
        mutations: vec![
            plant_interaction(&sid, i1, None, ts),
            plant_concept(&sid, c1, i1, "user schema", ConceptType::Entity, ts),
        ],
    };
    store.flush(&batch, None).await.unwrap();

    let snap = store.load_session(&sid).await.unwrap();
    assert_eq!(snap.mutation_epoch, 42, "the stamp must be durable");

    // An older batch (a replay, or a retained batch retried after a newer
    // flush) carries a lower watermark: monotonic max keeps the counter.
    let older = MutationBatch {
        mutation_epoch: 7,
        gc_mark: Default::default(),
        mutations: vec![plant_interaction(&sid, NodeId::new(), Some(i1), ts)],
    };
    store.flush(&older, None).await.unwrap();
    let snap = store.load_session(&sid).await.unwrap();
    assert_eq!(
        snap.mutation_epoch, 42,
        "a stale stamp must not rewind the durable counter"
    );
}

/// Issue #29: the `sessions` row carries GC's sweep mark beside the
/// epoch. flush stamps it with a field-wise monotonic merge (an older
/// stamp, or one with no sweep time, never rewinds either field — the
/// NULL handling is the part SQLite's two-argument `MAX` gets wrong on its
/// own), `seed` overwrites it, and `load_session` returns it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flush_stamps_and_load_resumes_the_gc_mark() {
    let store = test_store();
    store.init_schema().await.unwrap();

    let sid = SessionId::from("gc-mark");
    let i1 = NodeId::new();
    let ts = Utc.with_ymd_and_hms(2026, 8, 20, 12, 0, 0).unwrap();
    let swept = Utc.with_ymd_and_hms(2026, 10, 7, 9, 30, 0).unwrap();
    let flush = |mark: GcMark, prev: Option<NodeId>| MutationBatch {
        mutation_epoch: 1,
        gc_mark: mark,
        mutations: vec![plant_interaction(&sid, NodeId::new(), prev, ts)],
    };

    // A never-swept session loads the unset mark.
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 1,
                gc_mark: GcMark::default(),
                mutations: vec![plant_interaction(&sid, i1, None, ts)],
            },
            None,
        )
        .await
        .unwrap();
    assert!(store.load_session(&sid).await.unwrap().gc_mark.is_unset());

    let mark = GcMark {
        last_gc_epoch: 40,
        last_gc_at: Some(swept),
        last_gc_at_reset: false,
    };
    store.flush(&flush(mark, Some(i1)), None).await.unwrap();
    assert_eq!(store.load_session(&sid).await.unwrap().gc_mark, mark);

    // Older stamps — a lower epoch, an earlier time, no time at all —
    // change nothing.
    for stale in [
        GcMark::default(),
        GcMark {
            last_gc_epoch: 7,
            last_gc_at: None,
            last_gc_at_reset: false,
        },
        GcMark {
            last_gc_epoch: 39,
            last_gc_at: Some(swept - chrono::Duration::days(1)),
            last_gc_at_reset: false,
        },
    ] {
        store.flush(&flush(stale, Some(i1)), None).await.unwrap();
        assert_eq!(
            store.load_session(&sid).await.unwrap().gc_mark,
            mark,
            "stale stamp {stale:?} must not rewind the mark"
        );
    }

    // A drain advances the epoch without a new time; a later sweep moves
    // both.
    let drained = GcMark {
        last_gc_epoch: 45,
        last_gc_at: None,
        last_gc_at_reset: false,
    };
    store.flush(&flush(drained, Some(i1)), None).await.unwrap();
    assert_eq!(
        store.load_session(&sid).await.unwrap().gc_mark,
        GcMark {
            last_gc_epoch: 45,
            last_gc_at: Some(swept),
            last_gc_at_reset: false,
        }
    );
    let later = GcMark {
        last_gc_epoch: 90,
        last_gc_at: Some(swept + chrono::Duration::days(1)),
        last_gc_at_reset: false,
    };
    store.flush(&flush(later, Some(i1)), None).await.unwrap();
    assert_eq!(store.load_session(&sid).await.unwrap().gc_mark, later);

    // Issue #29: a forward clock jump persisted a future sweep time. A
    // plain (max-merged) earlier stamp cannot correct it; a re-anchored
    // one replaces it — the one regression the merge allows — while the
    // epoch stays a max, and the stored mark never carries the flag.
    let future = GcMark {
        last_gc_epoch: 95,
        last_gc_at: Some(swept + chrono::Duration::days(400)),
        last_gc_at_reset: false,
    };
    store.flush(&flush(future, Some(i1)), None).await.unwrap();
    let corrected = swept + chrono::Duration::days(2);
    let plain = GcMark {
        last_gc_epoch: 95,
        last_gc_at: Some(corrected),
        last_gc_at_reset: false,
    };
    store.flush(&flush(plain, Some(i1)), None).await.unwrap();
    assert_eq!(store.load_session(&sid).await.unwrap().gc_mark, future);
    // A re-anchor keeps the writer's current epoch (it is not a sweep).
    let reset = GcMark {
        last_gc_epoch: 95,
        last_gc_at: Some(corrected),
        last_gc_at_reset: true,
    };
    store.flush(&flush(reset, Some(i1)), None).await.unwrap();
    let stored = store.load_session(&sid).await.unwrap().gc_mark;
    assert_eq!(
        stored,
        GcMark {
            last_gc_epoch: 95,
            last_gc_at: Some(corrected),
            last_gc_at_reset: false,
        },
        "a re-anchor replaces the time, never rewinds the epoch"
    );
    assert_eq!(stored, future.apply_to_stored(reset), "same rule as memory");
    // A re-anchored stamp with no time cannot erase one.
    let no_time = GcMark {
        last_gc_epoch: 95,
        last_gc_at: None,
        last_gc_at_reset: true,
    };
    store.flush(&flush(no_time, Some(i1)), None).await.unwrap();
    assert_eq!(
        store.load_session(&sid).await.unwrap().gc_mark.last_gc_at,
        Some(corrected)
    );

    // A reset replayed after a later sweep (its epoch is older than the
    // stored mark's) must not rewind that sweep's time: it falls back to
    // the max-merge, exactly as `GcMark::apply_to_stored` does.
    let newer_sweep = GcMark {
        last_gc_epoch: 120,
        last_gc_at: Some(corrected + chrono::Duration::days(1)),
        last_gc_at_reset: false,
    };
    store
        .flush(&flush(newer_sweep, Some(i1)), None)
        .await
        .unwrap();
    let replayed_reset = GcMark {
        last_gc_epoch: 95,
        last_gc_at: Some(corrected),
        last_gc_at_reset: true,
    };
    store
        .flush(&flush(replayed_reset, Some(i1)), None)
        .await
        .unwrap();
    let stored = store.load_session(&sid).await.unwrap().gc_mark;
    assert_eq!(
        stored, newer_sweep,
        "a stale reset cannot rewind a later sweep"
    );
    assert_eq!(
        stored,
        newer_sweep.apply_to_stored(replayed_reset),
        "same rule as memory"
    );
}

/// Acceptance: canonization_events append + concept status update, via
/// both the mutation and `record_canonization`.
#[tokio::test]
async fn canonization_events_append_and_update_concept() {
    let store = test_store();
    store.init_schema().await.unwrap();

    let sid = SessionId::from("canon");
    let i1 = NodeId::new();
    let c1 = NodeId::new();
    // Whole-second clock: the store persists ms precision (T3.1 fixed
    // format), so Utc::now()'s nanoseconds would not round-trip.
    let ts = Utc.with_ymd_and_hms(2026, 8, 10, 12, 0, 0).unwrap();
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![
                    plant_interaction(&sid, i1, None, ts),
                    plant_concept(&sid, c1, i1, "pillar", ConceptType::Entity, ts),
                ],
            },
            None,
        )
        .await
        .unwrap();

    let ev1 = CanonizationEvent {
        id: NodeId::new(),
        session_id: sid.clone(),
        node_id: c1,
        from_status: CanonizationStatus::None,
        to_status: CanonizationStatus::Candidate,
        blast_radius: Some(2),
        last_demotion_time: None,
        occurred_at: ts,
    };
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![Mutation::CanonizationTransition { event: ev1.clone() }],
            },
            None,
        )
        .await
        .unwrap();

    let snap = store.load_session(&sid).await.unwrap();
    assert_eq!(
        snap.concepts[0].canonization_status,
        CanonizationStatus::Candidate
    );
    assert_eq!(snap.concepts[0].blast_radius, Some(2));
    assert_eq!(snap.canonization_events.len(), 1);
    assert_eq!(snap.canonization_events[0], ev1);

    // record_canonization appends a second event and re-updates the node.
    let ev2 = CanonizationEvent {
        id: NodeId::new(),
        session_id: sid.clone(),
        node_id: c1,
        from_status: CanonizationStatus::Candidate,
        to_status: CanonizationStatus::Venerable,
        blast_radius: Some(4),
        last_demotion_time: None,
        occurred_at: ts + chrono::Duration::minutes(1),
    };
    store.record_canonization(&ev2, None).await.unwrap();
    let snap = store.load_session(&sid).await.unwrap();
    assert_eq!(
        snap.concepts[0].canonization_status,
        CanonizationStatus::Venerable
    );
    assert_eq!(snap.canonization_events.len(), 2);
    assert_eq!(snap.canonization_events[1], ev2);

    // Transition on a missing concept is a typed NotFound.
    let ghost = CanonizationEvent {
        id: NodeId::new(),
        session_id: sid.clone(),
        node_id: NodeId::new(),
        from_status: CanonizationStatus::None,
        to_status: CanonizationStatus::Candidate,
        blast_radius: None,
        last_demotion_time: None,
        occurred_at: ts,
    };
    let err = store.record_canonization(&ghost, None).await.unwrap_err();
    assert!(matches!(err, StoreError::NotFound(_)));

    // F12: the write-behind log now replays ev1, which was already
    // recorded. The replay must be a no-op — not a status rollback to
    // Candidate, and not a duplicate audit row.
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![Mutation::CanonizationTransition { event: ev1.clone() }],
            },
            None,
        )
        .await
        .unwrap();
    let snap = store.load_session(&sid).await.unwrap();
    assert_eq!(
        snap.concepts[0].canonization_status,
        CanonizationStatus::Venerable,
        "a replayed hop must not roll the durable status back (F12)"
    );
    assert_eq!(snap.concepts[0].blast_radius, Some(4));
    assert_eq!(snap.canonization_events.len(), 2);
}

/// COH-3 acceptance: a demotion event (Canonical -> None) carries
/// `last_demotion_time`; it lands on the concept and round-trips through the
/// event table; a later non-demotion transition leaves it untouched.
#[tokio::test]
async fn demotion_event_sets_and_roundtrips_last_demotion_time() {
    let store = test_store();
    store.init_schema().await.unwrap();

    let sid = SessionId::from("demote-canon");
    let i1 = NodeId::new();
    let c1 = NodeId::new();
    let ts = Utc.with_ymd_and_hms(2026, 8, 10, 12, 0, 0).unwrap();
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![
                    plant_interaction(&sid, i1, None, ts),
                    plant_concept(&sid, c1, i1, "pillar", ConceptType::Entity, ts),
                ],
            },
            None,
        )
        .await
        .unwrap();

    let demote_at = ts + chrono::Duration::minutes(5);
    let ev = CanonizationEvent {
        id: NodeId::new(),
        session_id: sid.clone(),
        node_id: c1,
        from_status: CanonizationStatus::Canonical,
        to_status: CanonizationStatus::None,
        blast_radius: None,
        last_demotion_time: Some(demote_at),
        occurred_at: demote_at,
    };
    store.record_canonization(&ev, None).await.unwrap();

    let snap = store.load_session(&sid).await.unwrap();
    assert_eq!(
        snap.concepts[0].canonization_status,
        CanonizationStatus::None
    );
    assert_eq!(snap.concepts[0].blast_radius, None);
    assert_eq!(snap.concepts[0].last_demotion_time, Some(demote_at));
    assert_eq!(snap.canonization_events.len(), 1);
    assert_eq!(snap.canonization_events[0], ev);

    // A promotion after the demotion must NOT clobber the field (COALESCE).
    let promo = CanonizationEvent {
        id: NodeId::new(),
        session_id: sid.clone(),
        node_id: c1,
        from_status: CanonizationStatus::None,
        to_status: CanonizationStatus::Candidate,
        blast_radius: Some(2),
        last_demotion_time: None,
        occurred_at: demote_at + chrono::Duration::minutes(1),
    };
    store.record_canonization(&promo, None).await.unwrap();
    let snap = store.load_session(&sid).await.unwrap();
    assert_eq!(
        snap.concepts[0].last_demotion_time,
        Some(demote_at),
        "non-demotion transitions leave last_demotion_time untouched"
    );
    assert_eq!(
        snap.concepts[0].canonization_status,
        CanonizationStatus::Candidate
    );
    assert_eq!(snap.canonization_events.len(), 2);
    assert_eq!(snap.canonization_events[1], promo);
}

/// Re-stamp a planted `UpsertNode` with a **stale** canonization snapshot
/// and a bumped `gc_survived` — exactly the shape T4.5's
/// `bump_gc_survived` appends: the concept as it stood when the mutation
/// was queued, not as it stands now (R2-1).
fn with_stale_canonization(
    m: Mutation,
    status: CanonizationStatus,
    blast: Option<i32>,
    last_demotion_time: Option<DateTime<Utc>>,
) -> Mutation {
    match m {
        Mutation::UpsertNode {
            node: NodeKind::Concept(mut c),
        } => {
            c.canonization_status = status;
            c.blast_radius = blast;
            c.last_demotion_time = last_demotion_time;
            c.gc_survived += 1;
            Mutation::UpsertNode {
                node: NodeKind::Concept(c),
            }
        }
        other => panic!("expected a concept upsert, got {other:?}"),
    }
}

/// R2-1 on the durable tier: a stale `UpsertNode` flushed **ahead of** an
/// already-recorded transition must not regress the concept row.
///
/// `apply_canonization_transition` returns before the UPDATE when its
/// audit INSERT dedupes ("the effect is already in the row"). That premise
/// only holds while the canonization path owns the three columns —
/// `upsert_concept`'s `ON CONFLICT` list used to write them from a
/// snapshot queued before the hop, so this batch left the row wrong with
/// no repair. `gc_survived` still lands: the fix is column ownership, not
/// a blanket skip.
#[tokio::test]
async fn stale_upsert_before_a_recorded_transition_does_not_regress_the_status() {
    let store = test_store();
    store.init_schema().await.unwrap();

    let sid = SessionId::from("r2-1-status");
    let i1 = NodeId::new();
    let c1 = NodeId::new();
    let ts = Utc.with_ymd_and_hms(2026, 8, 10, 12, 0, 0).unwrap();
    let plant = plant_concept(&sid, c1, i1, "pillar", ConceptType::Entity, ts);
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![plant_interaction(&sid, i1, None, ts), plant.clone()],
            },
            None,
        )
        .await
        .unwrap();

    let hop = CanonizationEvent {
        id: NodeId::new(),
        session_id: sid.clone(),
        node_id: c1,
        from_status: CanonizationStatus::None,
        to_status: CanonizationStatus::Candidate,
        blast_radius: None,
        last_demotion_time: None,
        occurred_at: ts,
    };
    store.record_canonization(&hop, None).await.unwrap();

    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![
                    with_stale_canonization(plant, CanonizationStatus::None, None, None),
                    Mutation::CanonizationTransition { event: hop },
                ],
            },
            None,
        )
        .await
        .unwrap();

    let snap = store.load_session(&sid).await.unwrap();
    assert_eq!(
        snap.concepts[0].canonization_status,
        CanonizationStatus::Candidate,
        "a stale upsert must not take a recorded hop back out of the row"
    );
    assert_eq!(
        snap.concepts[0].gc_survived, 1,
        "the upsert's own columns must still land — only the canonization \
             columns are excluded"
    );
    assert_eq!(snap.canonization_events.len(), 1, "no duplicate audit row");
}

/// **L82-1 on a real SQL engine.** Two upserts of the same concept in ONE
/// batch must collapse to the durable row row-by-row replay produced.
///
/// This is the case the multi-row rewrite could silently get wrong, and the
/// one no reasoning-by-inspection settles: both engines *reject* a
/// statement whose input rows collide on the conflict target, so the rows
/// must be deduplicated — and a naive "last wins" would take the second
/// snapshot's canonization columns, which the row-by-row `ON CONFLICT DO
/// UPDATE` (R2-1: canonization columns are INSERT-only) would have
/// discarded. `store::batch::ConceptRow` keeps the first occurrence's
/// canonization and the last occurrence's everything-else; this executes
/// that against SQLite and reads the row back.
#[tokio::test]
async fn a_repeated_concept_in_one_batch_collapses_like_row_by_row_replay() {
    let store = test_store();
    store.init_schema().await.unwrap();

    let sid = SessionId::from("l82-1-dedupe");
    let i1 = NodeId::new();
    let c1 = NodeId::new();
    let ts = Utc.with_ymd_and_hms(2026, 8, 14, 12, 0, 0).unwrap();

    // The concept is born mid-progression: its first appearance in the
    // batch already carries a status, and a later appearance (a GC
    // `bump_gc_survived` snapshot taken before the hop) does not.
    let born = with_stale_canonization(
        plant_concept(&sid, c1, i1, "pillar", ConceptType::Entity, ts),
        CanonizationStatus::Canonical,
        Some(9),
        Some(ts),
    );
    let Mutation::UpsertNode {
        node: NodeKind::Concept(mut later),
    } = plant_concept(&sid, c1, i1, "pillar", ConceptType::Entity, ts)
    else {
        unreachable!("plant_concept builds a concept upsert")
    };
    later.gc_survived = 4;
    later.content = "pillar (revised)".into();

    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![
                    plant_interaction(&sid, i1, None, ts),
                    born,
                    Mutation::UpsertNode {
                        node: NodeKind::Concept(later),
                    },
                ],
            },
            None,
        )
        .await
        .expect("a batch with a repeated id must not be rejected as a duplicate conflict");

    let snap = store.load_session(&sid).await.unwrap();
    assert_eq!(snap.concepts.len(), 1, "one row, not two");
    let row = &snap.concepts[0];
    assert_eq!(
        row.content, "pillar (revised)",
        "ordinary columns: last wins"
    );
    assert_eq!(row.gc_survived, 4, "ordinary columns: last wins");
    assert_eq!(
        row.canonization_status,
        CanonizationStatus::Canonical,
        "the INSERT carries the FIRST occurrence's canonization columns — row-by-row \
             replay would have inserted them and then skipped them on the DO UPDATE (R2-1)"
    );
    assert_eq!(row.blast_radius, Some(9));
    assert_eq!(row.last_demotion_time, Some(ts));
}

/// **R1-1, the pin — against a real SQL engine with foreign keys on.**
///
/// `interactions.previous_id REFERENCES interactions(id)` is a *self* FK.
/// A planner that collapses a repeated interaction at its LAST position
/// re-emits `i1` after `i2(prev=i1)`; with `BULK_LIMITS.interactions == 1`
/// each is its own statement, SQLite checks the FK at end-of-statement, and
/// `i2` fails with `SQLITE_CONSTRAINT_FOREIGNKEY` (787). `Constraint` is
/// terminal, so the flush loop dead-letters the whole batch — the same loss
/// class L82-1 was raised for.
///
/// The second half is the control the reviewer used to prove *relocation* is
/// the cause and not the duplicate: with the repeat adjacent, nothing moves
/// past `i2`, and even the broken planner returns `Ok`.
#[tokio::test]
async fn a_repeated_interaction_does_not_outrun_the_row_that_chains_onto_it() {
    let store = test_store();
    store.init_schema().await.unwrap();

    let ts = Utc.with_ymd_and_hms(2026, 8, 15, 12, 0, 0).unwrap();

    // CONTROL FIRST, so that it still executes when the assertion below
    // fails: the same duplicate, adjacent. Nothing is relocated past i2
    // under either rule, so this passes with or without the fix — which is
    // what makes the second half evidence about *relocation* specifically
    // rather than about duplicates.
    let sid = SessionId::from("r1-1-adjacent-control");
    let i1 = NodeId::new();
    let i2 = NodeId::new();
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![
                    plant_interaction(&sid, i1, None, ts),
                    plant_interaction(&sid, i1, None, ts),
                    plant_interaction(&sid, i2, Some(i1), ts),
                ],
            },
            None,
        )
        .await
        .expect("the adjacent-repeat control must always pass");
    assert_eq!(
        store.load_session(&sid).await.unwrap().interactions.len(),
        2
    );

    // Non-adjacent re-upsert: i1, then i2 chaining onto i1, then i1 again.
    // `Graph::insert_interaction` permits a re-upsert that does not move the
    // interaction within the temporal chain, so `previous_id` is the same on
    // both occurrences.
    let sid = SessionId::from("r1-1-self-fk");
    let i1 = NodeId::new();
    let i2 = NodeId::new();
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![
                    plant_interaction(&sid, i1, None, ts),
                    plant_interaction(&sid, i2, Some(i1), ts),
                    plant_interaction(&sid, i1, None, ts),
                ],
            },
            None,
        )
        .await
        .expect(
            "the repeated interaction must not be relocated past the row that references \
                 it — a self-FK violation here dead-letters the whole batch (R1-1)",
        );

    let snap = store.load_session(&sid).await.unwrap();
    assert_eq!(snap.interactions.len(), 2, "one row per id");
    let chained = snap
        .interactions
        .iter()
        .find(|i| i.id == i2)
        .expect("i2 must be durable");
    assert_eq!(
        chained.previous_id,
        Some(i1),
        "the chain must survive the collapse"
    );
}

/// **L82-1.** A batch far larger than the per-statement row limit must
/// round-trip whole — the chunking is what keeps a statement inside the
/// backend's bind-parameter cap, and an off-by-one there loses rows
/// silently rather than loudly.
#[tokio::test]
async fn a_batch_larger_than_the_chunk_limit_round_trips_whole() {
    let store = test_store();
    store.init_schema().await.unwrap();

    let sid = SessionId::from("l82-1-chunking");
    let i1 = NodeId::new();
    let ts = Utc.with_ymd_and_hms(2026, 8, 14, 12, 0, 0).unwrap();
    // Several chunks' worth on both buckets.
    let concepts = BULK_LIMITS.concepts * 3 + 7;
    let mut mutations = vec![plant_interaction(&sid, i1, None, ts)];
    let mut ids = Vec::with_capacity(concepts);
    for n in 0..concepts {
        let id = NodeId::new();
        ids.push(id);
        mutations.push(plant_concept(
            &sid,
            id,
            i1,
            &format!("concept {n}"),
            ConceptType::Entity,
            ts,
        ));
    }
    for n in 0..ids.len() - 1 {
        mutations.push(Mutation::UpsertEdge {
            edge: Edge {
                event_time: None,
                id: NodeId::new(),
                session_id: sid.clone(),
                source: ids[n],
                target: ids[n + 1],
                edge_type: EdgeType::Causal,
                weight: 1.0,
                reinforcements: 1,
                created_at: ts,
                last_reinforced: ts,
            },
        });
    }
    assert!(
        concepts > BULK_LIMITS.concepts && ids.len() - 1 > BULK_LIMITS.edges,
        "both buckets must actually span multiple statements"
    );

    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations,
            },
            None,
        )
        .await
        .unwrap();

    let snap = store.load_session(&sid).await.unwrap();
    assert_eq!(
        snap.concepts.len(),
        concepts,
        "every concept must be durable"
    );
    assert_eq!(
        snap.edges.len(),
        ids.len() - 1,
        "every edge must be durable"
    );
}

/// R2-1, demotion variant — the worse half. The stale snapshot carries
/// `last_demotion_time: None` and the pre-demotion blast, so the demoted
/// node used to reload `Canonical` with the re-promotion cooldown erased
/// (COH-3, "cooldown survives restart").
#[tokio::test]
async fn stale_upsert_before_a_recorded_demotion_does_not_erase_the_cooldown() {
    let store = test_store();
    store.init_schema().await.unwrap();

    let sid = SessionId::from("r2-1-cooldown");
    let i1 = NodeId::new();
    let c1 = NodeId::new();
    let ts = Utc.with_ymd_and_hms(2026, 8, 10, 12, 0, 0).unwrap();
    let plant = plant_concept(&sid, c1, i1, "pillar", ConceptType::Entity, ts);
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![plant_interaction(&sid, i1, None, ts), plant.clone()],
            },
            None,
        )
        .await
        .unwrap();

    store
        .record_canonization(
            &CanonizationEvent {
                id: NodeId::new(),
                session_id: sid.clone(),
                node_id: c1,
                from_status: CanonizationStatus::Venerable,
                to_status: CanonizationStatus::Canonical,
                blast_radius: Some(8),
                last_demotion_time: None,
                occurred_at: ts,
            },
            None,
        )
        .await
        .unwrap();

    let demote_at = ts + chrono::Duration::minutes(5);
    let demote = CanonizationEvent {
        id: NodeId::new(),
        session_id: sid.clone(),
        node_id: c1,
        from_status: CanonizationStatus::Canonical,
        to_status: CanonizationStatus::None,
        blast_radius: None,
        last_demotion_time: Some(demote_at),
        occurred_at: demote_at,
    };
    store.record_canonization(&demote, None).await.unwrap();

    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![
                    with_stale_canonization(plant, CanonizationStatus::Canonical, Some(8), None),
                    Mutation::CanonizationTransition { event: demote },
                ],
            },
            None,
        )
        .await
        .unwrap();

    let snap = store.load_session(&sid).await.unwrap();
    assert_eq!(
        snap.concepts[0].canonization_status,
        CanonizationStatus::None,
        "a demoted node must not reload as Canonical"
    );
    assert_eq!(snap.concepts[0].blast_radius, None);
    assert_eq!(
        snap.concepts[0].last_demotion_time,
        Some(demote_at),
        "the re-promotion cooldown must survive the stale upsert"
    );
}

/// Acceptance: ISO-8601 timestamps use the FIXED 24-char ms format and
/// lex ordering == time ordering (T3.1 contract).
#[tokio::test]
async fn timestamps_fixed_format_and_lex_ordering() {
    let store = test_store();
    store.init_schema().await.unwrap();

    let sid = SessionId::from("ts");
    let t1 = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
    let t2 = t1 + chrono::Duration::milliseconds(250);
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![
                    plant_interaction(&sid, NodeId::new(), None, t1),
                    plant_interaction(&sid, NodeId::new(), None, t2),
                ],
            },
            None,
        )
        .await
        .unwrap();

    // Raw TEXT round-trips exactly and in the fixed 24-char form.
    let raw: String = sqlx::query_scalar(
        "SELECT created_at FROM interactions WHERE session_id = ? ORDER BY created_at ASC",
    )
    .bind(&sid.0)
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(raw, "2026-01-01T12:00:00.000Z");
    assert_eq!(raw.len(), 24);
    assert!(raw.ends_with('Z'));

    // Lex order of the stored TEXT equals time order (contract that makes
    // SQL age comparisons valid).
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT created_at FROM interactions WHERE session_id = ? ORDER BY created_at ASC",
    )
    .bind(&sid.0)
    .fetch_all(store.pool())
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec!["2026-01-01T12:00:00.000Z", "2026-01-01T12:00:00.250Z"]
    );
    assert!(rows[0] < rows[1]);

    // Load parses back to the exact instants.
    let snap = store.load_session(&sid).await.unwrap();
    let times: Vec<_> = snap.interactions.iter().map(|i| i.created_at).collect();
    assert_eq!(times, vec![t1, t2]);
}

/// D-R1-2: the flush→load round-trip claim had no test that actually wrote
/// a **non-NULL** event_time — every existing adapter row is NULL on both
/// sides, so NULL ≡ NULL passes while a mis-bind of `created_at` (or the
/// positional `try_get(9)` edge read drifting) would re-age every
/// historical fact onto flush time invisibly. Both stamps here are
/// distinct instants, so a mis-bind cannot pass; companion rows stay None.
#[tokio::test]
async fn event_time_survives_the_flush_load_round_trip() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let sid = SessionId::from("event-time-roundtrip");
    let about = Utc.with_ymd_and_hms(1999, 12, 31, 23, 59, 59).unwrap();
    let flushed = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();

    let i_stamped = NodeId::new();
    let i_plain = NodeId::new();
    let e_stamped = NodeId::new();
    let e_plain = NodeId::new();

    let interaction =
        |id: NodeId, prev: Option<NodeId>, et: Option<DateTime<Utc>>| Mutation::UpsertNode {
            node: NodeKind::Interaction(Interaction {
                event_time: et,
                id,
                session_id: sid.clone(),
                agent_id: AgentId::from("a"),
                prompt_text: Some("prompt".into()),
                previous_id: prev,
                created_at: flushed,
            }),
        };
    let edge = |id: NodeId, et: Option<DateTime<Utc>>| Mutation::UpsertEdge {
        edge: Edge {
            event_time: et,
            id,
            session_id: sid.clone(),
            source: NodeId::new(),
            target: NodeId::new(),
            edge_type: EdgeType::Dependency,
            weight: 1.0,
            reinforcements: 1,
            created_at: flushed,
            last_reinforced: flushed,
        },
    };

    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![
                    interaction(i_stamped, None, Some(about)),
                    interaction(i_plain, Some(i_stamped), None),
                    edge(e_stamped, Some(about)),
                    edge(e_plain, None),
                ],
            },
            None,
        )
        .await
        .unwrap();

    let loaded = store.load_session(&sid).await.unwrap();
    let stamped_i = loaded
        .interactions
        .iter()
        .find(|i| i.id == i_stamped)
        .expect("stamped interaction loaded");
    assert_eq!(stamped_i.event_time, Some(about));
    // Distinct from created_at by construction: a bind that swaps them fails.
    assert_eq!(stamped_i.created_at, flushed);
    let plain_i = loaded
        .interactions
        .iter()
        .find(|i| i.id == i_plain)
        .expect("plain interaction loaded");
    assert_eq!(plain_i.event_time, None);

    let stamped_e = loaded.edges.iter().find(|e| e.id == e_stamped).unwrap();
    assert_eq!(stamped_e.event_time, Some(about));
    assert_eq!(stamped_e.created_at, flushed);
    let plain_e = loaded.edges.iter().find(|e| e.id == e_plain).unwrap();
    assert_eq!(plain_e.event_time, None);
}

/// C2: the same D-R1-2 discipline for `human_confirmed` — a **non-zero**
/// count must survive flush→load on the real adapter. Every pre-existing
/// row carries 0 on both sides, so NULL/0 ≡ 0 passes while a mis-bind or
/// an index-drifted `try_get(16)` would silently reset every confirmed
/// concept to never-confirmed. Companion rows stay 0.
#[tokio::test]
async fn human_confirmed_survives_the_flush_load_round_trip() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let sid = SessionId::from("human-confirmed-roundtrip");
    let flushed = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();

    let iid = NodeId::new();
    let c_confirmed = NodeId::new();
    let c_plain = NodeId::new();
    let concept = |id: NodeId, content: &str, confirmed: i32| Mutation::UpsertNode {
        node: NodeKind::Concept(Concept {
            id,
            session_id: sid.clone(),
            content: content.into(),
            canonical_key: content.into(),
            concept_type: ConceptType::Constraint,
            origin_interaction: iid,
            origin_agent: AgentId::from("a"),
            created_at: flushed,
            access_count: 0,
            last_accessed: None,
            gc_survived: 0,
            canonization_status: CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: None,
            embedding: None,
            human_confirmed: confirmed,
            embedding_source: None,
            chunk_group_id: None,
        }),
    };

    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![
                    Mutation::UpsertNode {
                        node: NodeKind::Interaction(Interaction {
                            event_time: None,
                            id: iid,
                            session_id: sid.clone(),
                            agent_id: AgentId::from("a"),
                            prompt_text: Some("prompt".into()),
                            previous_id: None,
                            created_at: flushed,
                        }),
                    },
                    concept(c_confirmed, "load-bearing warning", 7),
                    concept(c_plain, "ordinary note", 0),
                ],
            },
            None,
        )
        .await
        .unwrap();

    let loaded = store.load_session(&sid).await.unwrap();
    let confirmed = loaded
        .concepts
        .iter()
        .find(|c| c.id == c_confirmed)
        .expect("confirmed concept loaded");
    assert_eq!(confirmed.human_confirmed, 7);
    let plain = loaded
        .concepts
        .iter()
        .find(|c| c.id == c_plain)
        .expect("plain concept loaded");
    assert_eq!(plain.human_confirmed, 0);
}

/// #22 PR 2: a concept's `embedding_source` survives flush→load on the real
/// adapter, a read access leaves it alone, and an upsert that clears it
/// clears the column. The shared check is the one every adapter runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn embedding_source_survives_the_flush_load_round_trip() {
    let store = test_store();
    store.init_schema().await.unwrap();
    crate::store::embedding_source_testkit::check_embedding_source_round_trip(
        &store,
        &SessionId::from("embedding-source"),
        4,
        None,
    )
    .await;
}

/// #22 review L1: a settled image intent keeps no vector, through the shared
/// check every adapter runs, and no trace of it is left in the raw payload
/// column either.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_settled_image_intent_keeps_no_vector() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let sid = SessionId::from("settled-image-intent");
    crate::store::embedding_source_testkit::check_a_settled_image_intent_keeps_no_vector(
        &store, &sid, 4, None,
    )
    .await;
    let payloads: Vec<String> =
        sqlx::query_scalar("SELECT payload FROM write_intents WHERE session_id = ?")
            .bind(sid.as_str())
            .fetch_all(store.pool())
            .await
            .unwrap();
    let with_vector = payloads.iter().filter(|p| p.contains("\"vector\"")).count();
    assert_eq!(
        (payloads.len(), with_vector),
        (3, 0),
        "three intents, none of the settled ones carries a vector: {payloads:?}"
    );
}

/// #22 PR 2: a stored `embedding_source` this build cannot read fails the
/// load by concept id. Reading it as `None` instead would make an image
/// concept look text-embedded, and a re-embed would then replace its image
/// vector with a vector of its caption.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreadable_embedding_source_fails_the_load() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let sid = SessionId::from("embedding-source-corrupt");
    crate::store::embedding_source_testkit::check_embedding_source_round_trip(
        &store, &sid, 4, None,
    )
    .await;
    sqlx::query(
        "UPDATE concepts SET embedding_source = '{\"modality\":\"audio\",\"origin\":\"client\"}' \
         WHERE session_id = ? AND embedding_source IS NOT NULL",
    )
    .bind(sid.as_str())
    .execute(store.pool())
    .await
    .unwrap();
    let err = store
        .load_session(&sid)
        .await
        .expect_err("an unreadable source must not load as None");
    assert!(matches!(err, StoreError::Invariant(_)), "{err:?}");
    assert!(err.to_string().contains("embedding_source"), "{err}");
}

/// #22 review round 2, L1: a malformed digest is refused at the flush.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_malformed_embedding_source_digest_is_refused_on_write() {
    let store = test_store();
    store.init_schema().await.unwrap();
    crate::store::embedding_source_testkit::check_a_malformed_digest_is_refused_on_write(
        &store,
        &SessionId::from("embedding-source-bad-digest"),
        4,
        None,
    )
    .await;
}

/// #22 review L1 (decided): SQLite also quarantines on a width restamp, and
/// like the first-stamp quarantine the shared check covers, it nulls the
/// vector and keeps the source, so the concept stays an image concept.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_width_restamp_nulls_the_vector_and_keeps_its_source() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let sid = SessionId::from("embedding-source-width");
    crate::store::embedding_source_testkit::check_embedding_source_round_trip(
        &store, &sid, 4, None,
    )
    .await;
    let sourced = |snap: &GraphSnapshot| {
        snap.concepts
            .iter()
            .find(|c| c.embedding_source.is_some())
            .cloned()
            .expect("the shared check leaves a sourced concept")
    };
    let mut concept = sourced(&store.load_session(&sid).await.unwrap());
    let source = concept.embedding_source.clone();
    concept.embedding = Some(vec![1.0, 2.0, 3.0, 4.0]);
    let contract = |dim| EmbeddingContract {
        kind: "fixture".into(),
        model: Some("embedding-source-test".into()),
        dim,
    };
    let batch = |mutations| MutationBatch {
        mutations,
        ..Default::default()
    };
    store
        .flush(
            &batch(vec![Mutation::UpsertNode {
                node: crate::types::Node::Concept(concept.clone()),
            }]),
            None,
        )
        .await
        .unwrap();
    let restamped = sourced(&store.load_session(&sid).await.unwrap());
    assert!(restamped.embedding.is_some(), "the 4-wide vector landed");
    store
        .flush(
            &batch(vec![Mutation::SetEmbedding {
                session_id: sid.clone(),
                embedding: Some(contract(5)),
            }]),
            None,
        )
        .await
        .unwrap();
    let loaded = store.load_session(&sid).await.unwrap();
    let after = loaded
        .concepts
        .iter()
        .find(|c| c.id == concept.id)
        .expect("still there");
    assert_eq!(after.embedding, None, "the width restamp quarantined it");
    assert_eq!(after.embedding_source, source, "and kept its source");
}

/// #22 PR 3: an unconsumed image derive's durable intent carries its
/// vector, contract and source through SQLite bit for bit, so a replay
/// applies exactly the vector that was acked (or refuses it).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_image_derive_intent_survives_the_flush_load_round_trip() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let sid = SessionId::from("image-intent");
    let batch = crate::store::erase::testkit::planted_batch(&sid, 8);
    let planted = batch
        .mutations
        .iter()
        .find_map(|m| match m {
            Mutation::PutWriteIntent { intent } => Some(intent.clone()),
            _ => None,
        })
        .expect("the batch plants an intent");
    assert!(matches!(
        planted.payload,
        crate::types::WriteIntentPayload::DeriveImage { .. }
    ));
    store.flush(&batch, None).await.unwrap();
    let loaded = store.load_session(&sid).await.unwrap();
    let [intent] = loaded.write_intents.as_slice() else {
        panic!("one intent: {:?}", loaded.write_intents);
    };
    // The payload exactly (timestamps are stored at millisecond precision).
    assert_eq!(intent.payload, planted.payload);
    assert_eq!(intent.outcome, None);
}
