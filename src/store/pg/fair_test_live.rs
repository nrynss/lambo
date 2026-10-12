//! #36 PR 3 on the live Postgres family: the canonization fair test
//! (`crate::test_util::fair_test`) through the real evaluator, with Stage 2
//! and Stage 3 measured by the live store's SQL.
//!
//! Live only, and run by hand, like `dependents_live`: no live database
//! step is part of CI (owner decision). The Postgres leg is the
//! `#[ignore]`d test below:
//!
//! ```text
//! LAMBO_POSTGRES_DSN=<dsn> cargo test --no-default-features --features store-postgres --lib store::pg::fair_test_live -- --ignored
//! ```
//!
//! The Cockroach leg runs inside `cockroach::conformance::conformance_suite`:
//!
//! ```text
//! LAMBO_COCKROACH_DSN=<dsn> cargo test --features store-cockroach,fixtures --lib conformance_suite -- --ignored
//! ```
//!
//! The fixture is built in memory (`fair_test::snapshot_in`), so neither leg
//! needs the `fixtures` feature to read a JSON file. Each run uses a fresh
//! session id, `session-fair-test-<uuid>`, with every node and edge id seeded
//! by that name, so a re-run against a persistent database meets none of a
//! previous run's rows and nothing has to be erased first. (Erasing is not an
//! option: `erase_session` tombstones the session id permanently, and every
//! later flush to it is refused.)

use crate::canon::{EvalParams, PromotionPolicy};
use crate::store::GraphStore;
use crate::test_util::fair_test as fair;

/// Flush the fixture under a fresh session id and run both policies to their
/// fixed points on `store`, asserting the statuses `fair_test::expected`
/// lists. Each policy gets its own fresh session, so the second run starts
/// from the fixture's all-`None` state too.
pub(crate) async fn check_fair_test(store: &dyn GraphStore) {
    for policy in [PromotionPolicy::Swarm, PromotionPolicy::Solo] {
        let sid = format!("{}-{}", fair::NAME, uuid::Uuid::new_v4());
        let snap = fair::snapshot_in(&sid, true);
        store
            .flush(&fair::batch_of(&snap), None)
            .await
            .expect("flush the fixture");
        let params = EvalParams {
            promotion_policy: policy,
            ..EvalParams::default()
        };
        let g = fair::fixed_point(store, snap, &params, 3).await;
        fair::assert_expected(
            &fair::statuses(&g),
            policy,
            &format!("the live store ({sid})"),
        );
    }
}

#[cfg(feature = "store-postgres")]
#[tokio::test]
#[ignore = "live: requires LAMBO_POSTGRES_DSN against pinned pgvector/pgvector:pg17"]
async fn postgres_runs_the_fair_test() {
    use super::postgres::{postgres_dsn_or_skip, PostgresStore};
    use crate::store::{StoreConfig, StoreKind};

    let Some(dsn) = postgres_dsn_or_skip("postgres_runs_the_fair_test") else {
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
    check_fair_test(&store).await;
}
