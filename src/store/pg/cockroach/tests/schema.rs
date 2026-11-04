//! DDL width, migration convergence and the dimension check.

use super::*;

// Vector codec tests (roundtrip / rendering / non-finite) live in the shared
// `crate::store::vector` module — SQLite stores the same text form, so the codec's
// coverage must run under either store feature (CON-8).

#[test]
fn schema_vector_dim_reads_ddl_width() {
    assert_eq!(schema_vector_dim(INIT_SQL), Some(1024));
    assert_eq!(schema_vector_dim("embedding VECTOR(768)"), Some(768));
    assert_eq!(schema_vector_dim("no vector here"), None);
    assert_eq!(schema_vector_dim("VECTOR(x)"), None);
    // The DDL is the authority: a schema change flows into vector_dimensions().
    assert_eq!(schema_vector_dim(INIT_SQL).unwrap(), 1024);
}

/// D/C upgrade path: the served migration must carry idempotent ALTERs for
/// every column a later wave shipped inline-only in the CREATE TABLEs.
/// `init_schema` executes INIT_SQL verbatim on every provision, so these
/// ALTER statements ARE the convergence path for a cluster provisioned by
/// an older build; without them the column preflight refuses the store with
/// no self-repair ("table edges is missing a column ... event_time").
/// Text-level contract — needs no live cluster (live convergence is the
/// ignored suite's job).
#[test]
fn served_migration_converges_event_time_and_human_confirmed() {
    assert!(
        INIT_SQL
            .contains("ALTER TABLE interactions ADD COLUMN IF NOT EXISTS event_time TIMESTAMPTZ",),
        "pre-D interactions rows have no convergence path"
    );
    assert!(
        INIT_SQL.contains("ALTER TABLE edges ADD COLUMN IF NOT EXISTS event_time TIMESTAMPTZ"),
        "pre-D edges rows have no convergence path"
    );
    assert!(
        INIT_SQL.contains(
            "ALTER TABLE concepts ADD COLUMN IF NOT EXISTS human_confirmed INT NOT NULL DEFAULT 0",
        ),
        "pre-C concepts rows have no convergence path"
    );
    assert_eq!(
        CockroachDialect::post_init_statements(),
        [
            "ALTER TABLE session_leases \
                 ADD COLUMN IF NOT EXISTS current_token INT NOT NULL DEFAULT 0",
            "ALTER TABLE session_leases ADD COLUMN IF NOT EXISTS endpoint STRING",
        ]
    );
}

/// #222 review: the fresh-vs-stale branch reads "every required table
/// missing" as a fresh store. That reading is sound only because the shipped
/// DDL names a non-empty required set — an empty parse (0 == 0) must never
/// occur, and the `!required.is_empty()` guard in `verify_schema` is its
/// backstop. The live behaviour is covered by the preflight tests (sqlite in
/// process, postgres live); this pins the precondition on the shipped DDL.
#[test]
fn shipped_ddl_names_a_nonempty_required_table_set() {
    let required = crate::store::tables_in_ddl(INIT_SQL);
    assert!(
        !required.is_empty(),
        "an empty parse must never read as fresh"
    );
    for table in [
        "sessions",
        "concepts",
        "interactions",
        "edges",
        "session_leases",
        "store_meta",
        "write_intents",
    ] {
        assert!(
            required.contains(&table),
            "{table} must be required: {required:?}"
        );
    }
}

#[test]
fn embedding_dim_check() {
    assert!(check_embedding_dim(&[0.0; 1024], 1024).is_ok());
    let err = check_embedding_dim(&[0.0; 8], 1024).unwrap_err();
    assert!(matches!(err, StoreError::Invariant(_)));
}

#[cfg(feature = "fixtures")]
#[tokio::test]
async fn oversized_seed_embedding_dimension_fails_before_pool_use() {
    let store = CockroachStore::new(StoreConfig {
        kind: crate::store::StoreKind::Cockroach,
        dsn: Some("postgresql://localhost:26257/defaultdb?sslmode=disable".into()),
        path: None,
        vector_dim: None,
    })
    .unwrap();
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
    assert!(
        store.pool.get().is_none(),
        "dimension check precedes pool use"
    );
}
