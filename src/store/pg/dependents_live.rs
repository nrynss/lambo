//! #36 on the live Postgres family: the shared dependents case
//! (`crate::test_util::dependents`) through `blast_radius` and
//! `interaction_span`, against the case's expected answers and, when the
//! build carries `MemoryStore`, against MemoryStore on every concept.
//!
//! Live only, and run by hand: by the owner's decision this check is not part
//! of CI. The Postgres leg is the `#[ignore]`d test below:
//!
//! ```text
//! LAMBO_POSTGRES_DSN=<dsn> cargo test --no-default-features --features store-postgres --lib store::pg::dependents_live -- --ignored
//! ```
//!
//! The Cockroach leg runs inside `cockroach::conformance::conformance_suite`:
//!
//! ```text
//! LAMBO_COCKROACH_DSN=<dsn> cargo test --features store-cockroach,fixtures --lib conformance_suite -- --ignored
//! ```
//!
//! The SQL text itself is checked offline in `cockroach::tests::sql_shapes`,
//! and the same case runs offline on MemoryStore and SQLite.
//!
//! The case uses fixed node ids and the session ids `deps` / `deps-other`, so
//! a re-run against a persistent database upserts the same rows and expects
//! the same answers.

use crate::store::GraphStore;
use crate::test_util::dependents as case;

/// Flush the case into `store` and assert every expected answer, plus
/// MemoryStore parity for every concept at both ages when `store-memory` is
/// compiled in.
pub(crate) async fn check_dependents_case(store: &dyn GraphStore) {
    let batch = case::store_batch();
    store.flush(&batch, None).await.expect("flush the case");
    let sid = case::session();
    for (name, age, blast, distinct, coverage) in case::expected_store() {
        let node = case::concept_id(name);
        let got = store
            .blast_radius(&sid, node, age, case::now())
            .await
            .expect("blast_radius");
        assert_eq!(got, blast, "blast_radius({name}, {age:?})");
        let span = store
            .interaction_span(&sid, node, age, case::now())
            .await
            .expect("interaction_span");
        assert_eq!(span.distinct, distinct, "span distinct({name}, {age:?})");
        assert!(
            (span.coverage - coverage).abs() < 1e-9,
            "span coverage({name}, {age:?}) = {} want {coverage}",
            span.coverage
        );
    }

    #[cfg(feature = "store-memory")]
    {
        let memory = crate::store::MemoryStore::new();
        memory.flush(&batch, None).await.expect("memory flush");
        let snap = memory.load_session(&sid).await.expect("memory load");
        for c in &snap.concepts {
            for age in [std::time::Duration::ZERO, case::MIN_AGE] {
                assert_eq!(
                    store
                        .blast_radius(&sid, c.id, age, case::now())
                        .await
                        .unwrap(),
                    memory
                        .blast_radius(&sid, c.id, age, case::now())
                        .await
                        .unwrap(),
                    "blast_radius {} {age:?}",
                    c.content
                );
                assert_eq!(
                    store
                        .interaction_span(&sid, c.id, age, case::now())
                        .await
                        .unwrap(),
                    memory
                        .interaction_span(&sid, c.id, age, case::now())
                        .await
                        .unwrap(),
                    "interaction_span {} {age:?}",
                    c.content
                );
            }
        }
    }
}

#[cfg(feature = "store-postgres")]
#[tokio::test]
#[ignore = "live: requires LAMBO_POSTGRES_DSN against pinned pgvector/pgvector:pg17"]
async fn postgres_answers_the_dependents_case() {
    use super::postgres::{postgres_dsn_or_skip, PostgresStore};
    use crate::store::{StoreConfig, StoreKind};

    let Some(dsn) = postgres_dsn_or_skip("postgres_answers_the_dependents_case") else {
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
    check_dependents_case(&store).await;
}
