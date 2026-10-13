//! #20 on the live Postgres family: the shared supersession case
//! (`crate::test_util::supersession`): `latest_structural_evidence_at`,
//! `interaction_span_since` and the `reason` / `superseded_by` audit
//! columns, against the case's expected answers.
//!
//! Live only, and run by hand, like `dependents_live`: no live database step
//! is part of CI (owner decision). The Postgres leg is the `#[ignore]`d test
//! below:
//!
//! ```text
//! LAMBO_POSTGRES_DSN=<dsn> cargo test --no-default-features --features store-postgres --lib store::pg::supersession_live -- --ignored
//! ```
//!
//! The Cockroach leg runs inside `cockroach::conformance::conformance_suite`:
//!
//! ```text
//! LAMBO_COCKROACH_DSN=<dsn> cargo test --features store-cockroach,fixtures --lib conformance_suite -- --ignored
//! ```
//!
//! The case uses fixed ids and the sessions `supersede` /
//! `supersede-other`, so a re-run against a persistent database upserts the
//! same rows; its audit events have fixed ids too, so a re-run's records are
//! deduplicated and the reloaded rows are the first run's. A database
//! provisioned before #20 converges through `init_schema`'s
//! `ADD COLUMN IF NOT EXISTS`; its existing rows keep a `NULL` pair, which
//! reads as a budget demotion (or no reason, on a promotion).

use crate::store::GraphStore;
use crate::test_util::supersession as case;

/// Flush the case into `store` and check both accessors and the audit
/// columns.
pub(crate) async fn check_supersession_case(store: &dyn GraphStore) {
    case::check_accessors(store).await;
    case::check_reason_persistence(store).await;
}

#[cfg(feature = "store-postgres")]
#[tokio::test]
#[ignore = "live: requires LAMBO_POSTGRES_DSN against pinned pgvector/pgvector:pg17"]
async fn postgres_answers_the_supersession_case() {
    use super::postgres::{postgres_dsn_or_skip, PostgresStore};
    use crate::store::{StoreConfig, StoreKind};

    let Some(dsn) = postgres_dsn_or_skip("postgres_answers_the_supersession_case") else {
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
    check_supersession_case(&store).await;
}
