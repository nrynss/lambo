//! #22 PR 2 on the live Postgres family: a concept's `embedding_source`
//! survives flush and load through the shared check every adapter runs, and
//! a stored value this build cannot read fails the load (review M1).
//!
//! Live only. The Postgres leg is the `#[ignore]`d test below, which the
//! `postgres-live` CI job runs by name; the Cockroach leg runs inside
//! `cockroach::conformance::conformance_suite`. The offline decode of the
//! column is tested in `codec::tests`.

use super::{Dialect, PgStore};
use crate::store::GraphStore;
use crate::types::{SessionId, StoreError};

/// After [`crate::store::embedding_source_testkit::check_embedding_source_round_trip`]
/// has planted `sid`, overwrite every stored source in it with a value no
/// build of this version can read (an unknown modality), and assert that
/// `load_session` fails with an `Invariant` naming the column instead of
/// loading the concept as text-embedded. Mirrors SQLite's
/// `an_unreadable_embedding_source_fails_the_load`.
pub(crate) async fn check_unreadable_embedding_source_fails_the_load<D: Dialect>(
    store: &PgStore<D>,
    sid: &SessionId,
) {
    let pool = store.pool().await.expect("pool");
    let corrupted = sqlx::query(
        "UPDATE concepts SET embedding_source = '{\"modality\":\"audio\",\"origin\":\"client\"}' \
         WHERE session_id = $1 AND embedding_source IS NOT NULL",
    )
    .bind(sid.as_str())
    .execute(&pool)
    .await
    .expect("plant an unreadable source")
    .rows_affected();
    assert!(corrupted > 0, "the round-trip check left a sourced concept");
    let err = store
        .load_session(sid)
        .await
        .expect_err("an unreadable source must not load as None");
    assert!(matches!(err, StoreError::Invariant(_)), "{err:?}");
    assert!(err.to_string().contains("embedding_source"), "{err}");
}

#[cfg(feature = "store-postgres")]
#[tokio::test]
#[ignore = "live: requires LAMBO_POSTGRES_DSN against pinned pgvector/pgvector:pg17"]
async fn postgres_round_trips_the_embedding_source() {
    use std::time::Duration;

    use uuid::Uuid;

    use super::postgres::{postgres_dsn_or_skip, PostgresStore};
    use crate::store::lease::{LeaseHolder, LeaseOutcome};
    use crate::store::{StoreConfig, StoreKind};
    use crate::types::AgentId;

    let Some(dsn) = postgres_dsn_or_skip("postgres_round_trips_the_embedding_source") else {
        return;
    };
    let store = PostgresStore::new(StoreConfig {
        kind: StoreKind::Postgres,
        dsn: Some(dsn),
        path: None,
        vector_dim: None,
    })
    .expect("construct");
    store.init_schema().await.expect("init_schema");
    store
        .preflight_schema()
        .await
        .expect("a provisioned store carries concepts.embedding_source");
    let dim = store.vector_dimensions().expect("pg stores carry vectors");
    let sid = SessionId::from(format!("embedding-source-{}", Uuid::new_v4()));
    let holder = LeaseHolder {
        endpoint: None,
        agent: AgentId::new("embedding-source-test"),
        pid: 1,
        host: "test".into(),
    };
    let LeaseOutcome::Acquired(lease) = store
        .acquire_lease(&sid, &holder, Duration::from_secs(60))
        .await
        .expect("acquire")
    else {
        panic!("a fresh session's lease is free");
    };
    crate::store::embedding_source_testkit::check_embedding_source_round_trip(
        &store,
        &sid,
        dim,
        Some(lease.token),
    )
    .await;
    check_unreadable_embedding_source_fails_the_load(&store, &sid).await;
    // Leave only the erase tombstone behind (#23), as the erase tests do.
    store
        .erase_session(&sid, &holder)
        .await
        .expect("erase the test session");
}
