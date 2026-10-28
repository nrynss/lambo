//! #198 on the live Postgres family: the shared suspected-supersession edge
//! case (`crate::test_util::suspected`): the `SuspectedSupersedes` edge
//! type's column string round-trips, confers no blast radius, and lapses
//! with a deleted edge or concept.
//!
//! Live only, and run by hand, like `supersession_live`: no live database
//! step is part of CI (owner decision). The Postgres leg is the `#[ignore]`d
//! test below:
//!
//! ```text
//! LAMBO_POSTGRES_DSN=<dsn> cargo test --no-default-features --features store-postgres --lib store::pg::suspected_live -- --ignored
//! ```
//!
//! The Cockroach leg runs inside `cockroach::conformance::conformance_suite`.

use crate::store::GraphStore;

/// Flush the case into `store` and check it.
pub(crate) async fn check_suspected_case(store: &dyn GraphStore) {
    crate::test_util::suspected::check_round_trip(store).await;
}

#[cfg(feature = "store-postgres")]
#[tokio::test]
#[ignore = "live: requires LAMBO_POSTGRES_DSN against pinned pgvector/pgvector:pg17"]
async fn postgres_answers_the_suspected_supersession_case() {
    use super::postgres::{postgres_dsn_or_skip, PostgresStore};
    use crate::store::{StoreConfig, StoreKind};

    let Some(dsn) = postgres_dsn_or_skip("postgres_answers_the_suspected_supersession_case") else {
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
    check_suspected_case(&store).await;
}
