//! #1 fencing across a clean release, for every dialect of the family (#23
//! review H2).
//!
//! A release used to delete the lease row, so the next acquire minted token 1
//! again and a writer still holding an older token passed the fence against
//! the new holder. A release now expires the row and keeps `current_token`.
//! The check itself is store-agnostic
//! ([`crate::store::lease::testkit::check_release_keeps_the_fencing_token`]);
//! this module gives it unique session ids and cleans up after it, so it is
//! safe on a shared cluster.
//!
//! Live only. The Postgres leg is the `#[ignore]`d test below (run by CI's
//! `postgres-live` job); the Cockroach leg runs inside
//! `cockroach::conformance::conformance_suite`.

use uuid::Uuid;

use super::sql::ERASE_STATEMENTS;
use super::{Dialect, PgStore};
use crate::store::lease::testkit::check_release_keeps_the_fencing_token;
use crate::types::SessionId;

/// Remove every row the check wrote, the lease rows included (a test
/// session's whole lifetime ends here, so no token needs to outlive it).
async fn cleanup<D: Dialect>(store: &PgStore<D>, sid: &SessionId) {
    let pool = store.pool().await.expect("pool");
    for (_, sql) in ERASE_STATEMENTS {
        let _ = sqlx::query(sql).bind(sid.as_str()).execute(&pool).await;
    }
    let _ = sqlx::query("DELETE FROM session_leases WHERE session_id = $1")
        .bind(sid.as_str())
        .execute(&pool)
        .await;
}

pub(crate) async fn check_release_keeps_the_token<D: Dialect>(store: &PgStore<D>) {
    let run = Uuid::new_v4();
    let sid = SessionId::from(format!("release-{run}"));
    let sid2 = SessionId::from(format!("release-zombie-{run}"));
    check_release_keeps_the_fencing_token(store, &sid, &sid2).await;
    cleanup(store, &sid).await;
    cleanup(store, &sid2).await;
}

#[cfg(feature = "store-postgres")]
#[tokio::test]
#[ignore = "live: requires LAMBO_POSTGRES_DSN against pinned pgvector/pgvector:pg17"]
async fn postgres_release_keeps_the_fencing_token() {
    use crate::store::GraphStore;

    let Some(dsn) =
        super::postgres::postgres_dsn_or_skip("postgres_release_keeps_the_fencing_token")
    else {
        return;
    };
    let store = super::postgres::PostgresStore::new(crate::store::StoreConfig {
        kind: crate::store::StoreKind::Postgres,
        dsn: Some(dsn),
        path: None,
        vector_dim: None,
    })
    .expect("construct");
    store.init_schema().await.expect("init_schema");
    check_release_keeps_the_token(&store).await;
}
