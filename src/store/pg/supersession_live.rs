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

use super::{Dialect, PgStore};
use crate::store::GraphStore;
use crate::test_util::supersession as case;

/// Flush the case into `store` and check both accessors and the audit
/// columns.
pub(crate) async fn check_supersession_case(store: &dyn GraphStore) {
    case::check_accessors(store).await;
    case::check_reason_persistence(store).await;
}

/// Review M3 on the Postgres family: rows whose reason pair is unreadable
/// (inserted by hand, as an older or newer build might leave them) load as
/// an unknown reason, never Budget, and do not stop the session's load. The
/// event ids are fixed, so a re-run inserts nothing new.
pub(crate) async fn check_unreadable_reason<D: Dialect>(store: &PgStore<D>) {
    use crate::types::{CanonizationStatus, DemotionReason, NodeId};
    let sid = case::session();
    let node = case::concept_id("R1");
    let pool = store.pool().await.expect("pool");
    let rows: [(u64, &str, &str, Option<&str>, Option<uuid::Uuid>); 3] = [
        (90, "Canonical", "None", Some("evicted"), None),
        (91, "Canonical", "None", Some("budget"), Some(node.0)),
        (92, "Venerable", "Canonical", Some("budget"), None),
    ];
    for (n, from, to, reason, by) in rows {
        sqlx::query(
            "INSERT INTO canonization_events (id, session_id, node_id, from_status, \
             to_status, blast_radius, last_demotion_time, occurred_at, reason, superseded_by) \
             VALUES ($1, $2, $3, $4, $5, NULL, NULL, now(), $6, $7) ON CONFLICT (id) DO NOTHING",
        )
        .bind(uuid::Uuid::from_u64_pair(0x20_05, n))
        .bind(&sid.0)
        .bind(node.0)
        .bind(from)
        .bind(to)
        .bind(reason)
        .bind(by)
        .execute(&pool)
        .await
        .expect("insert a raw row");
    }
    let snap = store
        .load_session(&sid)
        .await
        .expect("the load must not fail");
    for (n, ..) in rows {
        let id = NodeId(uuid::Uuid::from_u64_pair(0x20_05, n));
        let ev = snap
            .canonization_events
            .iter()
            .find(|e| e.id == id)
            .expect("the row loads");
        assert!(
            matches!(ev.reason, Some(DemotionReason::Unknown { .. })),
            "{ev:?}"
        );
        assert_ne!(ev.demotion_reason(), Some(DemotionReason::Budget));
    }
    assert!(snap
        .concepts
        .iter()
        .any(|c| c.id == node && c.canonization_status == CanonizationStatus::None));
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
    check_unreadable_reason(&store).await;
}
