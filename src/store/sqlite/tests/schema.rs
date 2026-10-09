//! Schema init, migration convergence and preflight.

use super::*;

#[tokio::test]
async fn init_schema_runs_twice_cleanly() {
    // Acceptance: init_schema twice on a fresh target (T3.1 idempotency).
    let store = test_store();
    store.init_schema().await.unwrap();
    store.init_schema().await.unwrap();
}

/// J3 F5, at the adapter. An un-provisioned target fails the preflight; a
/// provisioned one passes it; and dropping the one table this branch added
/// (a pre-J3 store, exactly) fails it by name.
#[tokio::test]
async fn preflight_schema_refuses_an_unprovisioned_or_unmigrated_target() {
    let store = test_store();
    let err = store
        .preflight_schema()
        .await
        .expect_err("an empty database must not pass the preflight");
    assert!(
        matches!(err, StoreError::Capability(_)),
        "an un-migrated store is a missing capability, not a backend fault: {err:?}"
    );

    store.init_schema().await.unwrap();
    store
        .preflight_schema()
        .await
        .expect("a provisioned store must pass");

    sqlx::query("DROP TABLE write_intents")
        .execute(store.pool())
        .await
        .unwrap();
    let err = store
        .preflight_schema()
        .await
        .expect_err("a pre-J3 store must not pass")
        .to_string();
    assert!(err.contains("write_intents"), "{err}");
    assert!(err.contains("lambo provision"), "{err}");
}

/// J3-R2R-3, at the adapter: F5's uncovered half. A store whose *tables*
/// are all present but one *column* is missing — the exact shape round 2
/// measured attaching, acking and losing everything, loud only at close —
/// must now be refused by the preflight, by table and column name.
#[tokio::test]
async fn preflight_schema_refuses_a_missing_column() {
    let store = test_store();
    store.init_schema().await.unwrap();
    store
        .preflight_schema()
        .await
        .expect("a provisioned store must pass the column preflight");
    // Render `concepts.chunk_group_id` absent under the required name the
    // way an older build's store would have it. RENAME is the clean
    // analogue of round-2's measured "one column missing" — all ten tables
    // present, one column gone under the name the DDL requires.
    sqlx::query("ALTER TABLE concepts RENAME COLUMN chunk_group_id TO chunk_group_id_old")
        .execute(store.pool())
        .await
        .unwrap();
    let err = store
        .preflight_schema()
        .await
        .expect_err("a store missing a column the build requires must not pass")
        .to_string();
    assert!(err.contains("concepts"), "names the table: {err}");
    assert!(err.contains("chunk_group_id"), "names the column: {err}");
    assert!(err.contains("lambo provision"), "actionable: {err}");
}

#[test]
fn columns_in_ddl_parses_the_shipped_migration() {
    // The parser and the shipped DDL must agree, or the column preflight
    // silently checks nothing. Assert a column from each idiom: an inline
    // post-T3.1 column, a plain column, and that table-level constraints
    // (PRIMARY KEY / UNIQUE) are never read as columns.
    let cols = super::columns_in_ddl(INIT_SQL);
    assert!(
        cols.contains(&("concepts", "chunk_group_id")),
        "the J3-R2R-3 missing-column case must be in the parsed set"
    );
    assert!(cols.contains(&("concepts", "embedding")));
    assert!(cols.contains(&("sessions", "embedding_kind")));
    assert!(cols.contains(&("write_intents", "outcome_summary")));
    // The parse must not manufacture a column out of a table-level clause.
    assert!(!cols.contains(&("edges", "UNIQUE")));
    assert!(!cols.contains(&("synonyms", "PRIMARY")));
    assert!(!cols.contains(&("write_intents", "PRIMARY")));
}

/// Migration path for pre-existing databases (P3 wave 2): a database built
/// from the T3.1 DDL (no chunk_group_id / embedding columns) converges on
/// `init_schema` — the guarded ALTERs add the columns — a second
/// `init_schema` is a no-op, and chunk_group_id then round-trips. The
/// regular tests always start from a fresh schema, so this is the only
/// place the ALTER convergence is exercised. Multi-thread flavor:
/// `load_session` runs on a worker thread (see module doc pool quirk).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_schema_converges_preexisting_database() {
    let store = test_store();
    let old = r#"
            CREATE TABLE sessions (
                session_id TEXT PRIMARY KEY,
                root_goal TEXT,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
                closed_at TEXT
            );
            CREATE TABLE concepts (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL REFERENCES sessions(session_id),
                content TEXT NOT NULL,
                canonical_key TEXT NOT NULL,
                concept_type TEXT NOT NULL,
                origin_interaction TEXT NOT NULL REFERENCES interactions(id),
                origin_agent TEXT NOT NULL,
                created_at TEXT NOT NULL,
                access_count INTEGER NOT NULL DEFAULT 0,
                last_accessed TEXT,
                gc_survived INTEGER NOT NULL DEFAULT 0,
                canonization_status TEXT NOT NULL DEFAULT 'None',
                blast_radius INTEGER,
                last_demotion_time TEXT,
                embedding BLOB
            );
            CREATE TABLE canonization_events (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                node_id TEXT NOT NULL,
                from_status TEXT NOT NULL,
                to_status TEXT NOT NULL,
                blast_radius INTEGER,
                occurred_at TEXT NOT NULL
            );
        "#;
    sqlx::query(old).execute(store.pool()).await.unwrap();

    // Convergence + idempotency: columns appear, second init is a no-op.
    store.init_schema().await.unwrap();
    store.init_schema().await.unwrap();
    let concept_cols: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('concepts')")
            .fetch_all(store.pool())
            .await
            .unwrap();
    assert!(
        concept_cols.iter().any(|c| c == "chunk_group_id"),
        "chunk_group_id must be added to a pre-existing concepts table"
    );
    let session_cols: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('sessions')")
            .fetch_all(store.pool())
            .await
            .unwrap();
    for want in [
        "embedding_kind",
        "embedding_model",
        "embedding_dim",
        "mutation_epoch",
        // Issue #29: GC's sweep accounting converges the same way.
        "last_gc_epoch",
        "last_gc_at",
    ] {
        assert!(
            session_cols.iter().any(|c| c == want),
            "{want} must be added to a pre-existing sessions table"
        );
    }
    let ce_cols: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('canonization_events')")
            .fetch_all(store.pool())
            .await
            .unwrap();
    assert!(
        ce_cols.iter().any(|c| c == "last_demotion_time"),
        "last_demotion_time must be added to a pre-existing canonization_events table"
    );

    // The converged column actually round-trips a demoted observation.
    let sid = SessionId::from("legacy-session");
    let i1 = NodeId::new();
    let o1 = NodeId::new();
    let ts = Utc::now();
    let batch = MutationBatch {
        mutation_epoch: 0,
        gc_mark: Default::default(),
        mutations: vec![
            plant_interaction(&sid, i1, None, ts),
            Mutation::UpsertNode {
                node: NodeKind::Concept(Concept {
                    id: o1,
                    session_id: sid.clone(),
                    content: "legacy drift note".into(),
                    canonical_key: "legacy drift note".into(),
                    concept_type: ConceptType::Observation,
                    origin_interaction: i1,
                    origin_agent: AgentId::from("a"),
                    created_at: ts,
                    access_count: 0,
                    last_accessed: None,
                    gc_survived: 0,
                    canonization_status: CanonizationStatus::None,
                    blast_radius: None,
                    last_demotion_time: None,
                    embedding: None,
                    human_confirmed: 0,
                    embedding_source: None,
                    chunk_group_id: Some("legacy-chunk".into()),
                }),
            },
            // The rebuilt graph requires the Derives edge (invariant),
            // exactly as demote would create it.
            Mutation::UpsertEdge {
                edge: crate::types::Edge {
                    event_time: None,
                    id: NodeId::new(),
                    session_id: sid.clone(),
                    source: i1,
                    target: o1,
                    edge_type: EdgeType::Derives,
                    weight: 1.0,
                    reinforcements: 1,
                    created_at: ts,
                    last_reinforced: ts,
                },
            },
        ],
    };
    store.flush(&batch, None).await.unwrap();
    let loaded = load_session(&store, &sid).unwrap();
    let obs = loaded
        .graph
        .concepts()
        .find(|c| c.concept_type == ConceptType::Observation)
        .expect("observation loaded from converged database");
    assert_eq!(obs.chunk_group_id.as_deref(), Some("legacy-chunk"));
}

/// D/C upgrade path (the dogfood-rig gap): a store provisioned by a pre-D/C
/// build carries interactions/edges WITHOUT event_time and concepts WITHOUT
/// human_confirmed — the exact shape that failed the column preflight with
/// "table edges is missing a column ... event_time" and no self-repair.
/// `init_schema` must converge it: the guarded ALTERs add all three columns,
/// a second init is a no-op, and a flush→load round-trip then preserves
/// Some(event_time) and a nonzero human_confirmed. Companion to
/// `init_schema_converges_preexisting_database` (the earlier waves' columns).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_schema_converges_pre_d_event_time_and_human_confirmed() {
    let store = test_store();
    // The pre-D/C shapes: every column the flush/load paths write EXCEPT
    // the three this build converges (concepts is created by init_schema's
    // CREATE TABLE IF NOT EXISTS — but as an EXISTING table it keeps its
    // old shape, so build it here without human_confirmed).
    let old = r#"
            CREATE TABLE sessions (
                session_id TEXT PRIMARY KEY,
                root_goal TEXT,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
            );
            CREATE TABLE concepts (
                id                  TEXT PRIMARY KEY,
                session_id          TEXT NOT NULL REFERENCES sessions(session_id),
                content             TEXT NOT NULL,
                canonical_key       TEXT NOT NULL,
                concept_type        TEXT NOT NULL,
                origin_interaction  TEXT NOT NULL REFERENCES interactions(id),
                origin_agent        TEXT NOT NULL,
                created_at          TEXT NOT NULL,
                access_count        INTEGER NOT NULL DEFAULT 0,
                last_accessed       TEXT,
                gc_survived         INTEGER NOT NULL DEFAULT 0,
                canonization_status TEXT NOT NULL DEFAULT 'None',
                blast_radius        INTEGER,
                last_demotion_time  TEXT,
                embedding           BLOB,
                chunk_group_id      TEXT
            );
            CREATE TABLE interactions (
                id          TEXT PRIMARY KEY,
                session_id  TEXT NOT NULL REFERENCES sessions(session_id),
                agent_id    TEXT NOT NULL,
                prompt_text TEXT,
                previous_id TEXT REFERENCES interactions(id),
                created_at  TEXT NOT NULL
            );
            CREATE TABLE edges (
                id              TEXT PRIMARY KEY,
                session_id      TEXT NOT NULL REFERENCES sessions(session_id),
                source          TEXT NOT NULL,
                target          TEXT NOT NULL,
                edge_type       TEXT NOT NULL,
                weight          REAL NOT NULL,
                reinforcements INTEGER NOT NULL DEFAULT 0,
                created_at      TEXT NOT NULL,
                last_reinforced TEXT NOT NULL,
                UNIQUE (source, target, edge_type)
            );
        "#;
    sqlx::query(old).execute(store.pool()).await.unwrap();

    // Convergence + idempotency: columns appear, second init is a no-op.
    store.init_schema().await.unwrap();
    store.init_schema().await.unwrap();
    for (table, want) in [
        ("interactions", "event_time"),
        ("edges", "event_time"),
        ("concepts", "human_confirmed"),
    ] {
        let cols: Vec<String> =
            sqlx::query_scalar(&format!("SELECT name FROM pragma_table_info('{table}')"))
                .fetch_all(store.pool())
                .await
                .unwrap();
        assert!(
            cols.iter().any(|c| c == want),
            "{want} must be added to a pre-existing {table} table"
        );
    }

    // And the converged columns actually round-trip D/C payloads.
    let sid = SessionId::from("pre-d-upgrade");
    let i1 = NodeId::new();
    let c1 = NodeId::new();
    let ts = Utc.with_ymd_and_hms(2026, 8, 20, 12, 0, 0).unwrap();
    let batch = MutationBatch {
        mutation_epoch: 0,
        gc_mark: Default::default(),
        mutations: vec![
            Mutation::UpsertNode {
                node: NodeKind::Interaction(Interaction {
                    event_time: Some(ts),
                    id: i1,
                    session_id: sid.clone(),
                    agent_id: AgentId::from("a"),
                    prompt_text: Some("prompt".into()),
                    previous_id: None,
                    created_at: ts,
                }),
            },
            Mutation::UpsertNode {
                node: NodeKind::Concept(Concept {
                    id: c1,
                    session_id: sid.clone(),
                    content: "confirmed fact".into(),
                    canonical_key: "confirmed fact".into(),
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
                    embedding: None,
                    human_confirmed: 3,
                    embedding_source: None,
                    chunk_group_id: None,
                }),
            },
            Mutation::UpsertEdge {
                edge: Edge {
                    event_time: Some(ts),
                    id: NodeId::new(),
                    session_id: sid.clone(),
                    source: i1,
                    target: c1,
                    edge_type: EdgeType::Derives,
                    weight: 1.0,
                    reinforcements: 1,
                    created_at: ts,
                    last_reinforced: ts,
                },
            },
        ],
    };
    store.flush(&batch, None).await.unwrap();
    let loaded = load_session(&store, &sid).unwrap();
    assert_eq!(
        loaded
            .graph
            .interactions()
            .find(|i| i.id == i1)
            .unwrap()
            .event_time,
        Some(ts),
        "converged interactions.event_time must round-trip"
    );
    assert_eq!(
        loaded
            .graph
            .edges()
            .find(|e| e.source == i1)
            .unwrap()
            .event_time,
        Some(ts),
        "converged edges.event_time must round-trip"
    );
    assert_eq!(
        loaded
            .graph
            .concepts()
            .find(|c| c.id == c1)
            .unwrap()
            .human_confirmed,
        3,
        "converged concepts.human_confirmed must round-trip"
    );
}

/// Acceptance: a legal demote (duplicate Observation canonical keys) must
/// not fail the flush (partial-UNIQUE semantics); a duplicate
/// non-Observation key is a real conflict and must fail.
#[tokio::test]
async fn partial_unique_demote_duplicates_pass_but_entity_duplicates_fail() {
    let store = test_store();
    store.init_schema().await.unwrap();

    let sid = SessionId::from("uniq");
    let i1 = NodeId::new();
    let o1 = NodeId::new();
    let o2 = NodeId::new();
    let e1 = NodeId::new();
    let ts = Utc::now();

    // Two Observations sharing a canonical key + one Entity sharing that
    // same key: all legal (the partial index only constrains
    // concept_type <> 'Observation').
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![
                    plant_interaction(&sid, i1, None, ts),
                    plant_concept(
                        &sid,
                        o1,
                        i1,
                        "duplicate sentence",
                        ConceptType::Observation,
                        ts,
                    ),
                    plant_concept(
                        &sid,
                        o2,
                        i1,
                        "duplicate sentence",
                        ConceptType::Observation,
                        ts,
                    ),
                    plant_concept(&sid, e1, i1, "duplicate sentence", ConceptType::Entity, ts),
                ],
            },
            None,
        )
        .await
        .unwrap();
    let snap = store.load_session(&sid).await.unwrap();
    assert_eq!(snap.concepts.len(), 3, "all three rows must persist");

    // A second Entity with the same canonical key violates the partial
    // unique index and must fail the flush (transaction rolled back).
    let e2 = NodeId::new();
    let err = store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![plant_concept(
                    &sid,
                    e2,
                    i1,
                    "duplicate sentence",
                    ConceptType::Entity,
                    ts,
                )],
            },
            None,
        )
        .await
        .unwrap_err();
    // STORE-4: constraint violations are classified (never flattened
    // into Backend) so the flush loop can dead-letter them.
    assert!(
        matches!(err, StoreError::Constraint(_)),
        "expected Constraint, got {err:?}"
    );
    let snap = store.load_session(&sid).await.unwrap();
    assert_eq!(snap.concepts.len(), 3, "failed flush must not persist rows");
}
