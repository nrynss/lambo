//! #22 PR 2 on live PostgreSQL: a concept's `embedding_source` survives
//! flush and load through the shared check every adapter runs. `#[ignore]`d
//! like the other live tests; the `postgres-live` CI job runs it by name.

use std::time::Duration;

use uuid::Uuid;

use super::postgres::{postgres_dsn_or_skip, PostgresStore};
use crate::store::lease::{LeaseHolder, LeaseOutcome};
use crate::store::{GraphStore, StoreConfig, StoreKind};
use crate::types::{AgentId, SessionId};

#[tokio::test]
#[ignore = "live: requires LAMBO_POSTGRES_DSN against pinned pgvector/pgvector:pg17"]
async fn postgres_round_trips_the_embedding_source() {
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
    // Leave only the erase tombstone behind (#23), as the erase tests do.
    store
        .erase_session(&sid, &holder)
        .await
        .expect("erase the test session");
}
