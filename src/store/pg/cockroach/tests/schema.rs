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
