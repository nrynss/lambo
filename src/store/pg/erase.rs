//! Session erasure (#23) for every dialect of the family.
//!
//! The coverage check against both shipped migrations runs offline. The rest
//! needs a real engine: the Postgres leg is the `#[ignore]`d test at the end
//! (run by CI's `postgres-live` job), the Cockroach leg runs inside
//! `cockroach::conformance::conformance_suite`. Every live check uses unique
//! session ids and removes its tombstones at the end, so it is safe on a
//! shared cluster.

use std::collections::BTreeSet;
use std::time::Duration;

use chrono::Utc;
use uuid::Uuid;

use super::sql::ERASE_STATEMENTS;
use super::{Dialect, PgStore};
use crate::store::erase::testkit::{planted_batch, planted_counts, FailAt};
use crate::store::erase::{EraseCounts, EraseOutcome, EraseReport, ERASED_HOLDER};
use crate::store::lease::{LeaseHolder, LeaseOutcome};
use crate::store::{tables_in_ddl, GraphStore, SessionFlushStats};
use crate::types::{AgentId, SessionId, StoreError};

/// The schema-drift guard, offline: the erase statement list plus the
/// tombstoned lease table is exactly the set of tables each shipped
/// migration creates. A table added to either without an erase statement
/// fails here, in the ordinary `cargo test` row.
#[test]
fn erase_covers_every_table_in_both_ddls() {
    let mut covered: BTreeSet<&str> = ERASE_STATEMENTS.iter().map(|(t, _)| *t).collect();
    covered.insert("session_leases");
    for (dialect, ddl) in [
        (
            "postgres",
            include_str!("../../../migrations/postgres/001_init.sql"),
        ),
        (
            "cockroach",
            include_str!("../../../migrations/cockroach/001_init.sql"),
        ),
    ] {
        let tables: BTreeSet<&str> = tables_in_ddl(ddl).into_iter().collect();
        assert_eq!(covered, tables, "{dialect}");
    }
    for (table, sql) in ERASE_STATEMENTS {
        assert_eq!(*sql, format!("DELETE FROM {table} WHERE session_id = $1"));
    }
}

fn holder(agent: &str, pid: u32) -> LeaseHolder {
    LeaseHolder {
        endpoint: None,
        agent: AgentId::new(agent),
        pid,
        host: "test".into(),
    }
}

async fn census<D: Dialect>(store: &PgStore<D>, sid: &SessionId) -> Vec<(String, i64)> {
    let pool = store.pool().await.expect("pool");
    let mut out = Vec::new();
    for table in tables_in_ddl(&store.ddl) {
        let n: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {table} WHERE session_id = $1"
        ))
        .bind(sid.as_str())
        .fetch_one(&pool)
        .await
        .unwrap_or_else(|e| panic!("count {table}: {e}"));
        out.push((table.to_string(), n));
    }
    out
}

/// A row in every table for `sid`; `owner` ends up holding its live lease.
async fn plant_everything<D: Dialect>(store: &PgStore<D>, sid: &SessionId, owner: &LeaseHolder) {
    let dim = store.vector_dimensions().expect("pg stores carry vectors");
    let LeaseOutcome::Acquired(lease) = store
        .acquire_lease(sid, owner, Duration::from_secs(60))
        .await
        .expect("acquire")
    else {
        panic!("the owner takes the session's lease");
    };
    store
        .flush(&planted_batch(sid, dim), Some(lease.token))
        .await
        .expect("plant");
    let pool = store.pool().await.expect("pool");
    sqlx::query(
        "INSERT INTO synonyms (session_id, source_key, canonical_key) VALUES ($1, 'navy', 'blue')",
    )
    .bind(sid.as_str())
    .execute(&pool)
    .await
    .expect("synonym");
    sqlx::query(
        "INSERT INTO reservations (session_id, node_id, agent_id, expires_at) \
         VALUES ($1, $2, 'erase-test', $3)",
    )
    .bind(sid.as_str())
    .bind(Uuid::new_v4())
    .bind(Utc::now() + chrono::Duration::hours(1))
    .execute(&pool)
    .await
    .expect("reservation");
    store
        .write_flush_stats(
            sid,
            &SessionFlushStats {
                flush_lag_ms: 5,
                log_depth: 1,
            },
        )
        .await
        .expect("stats");
    store
        .record_lease_refusal(sid, "refused@h#9", &owner.token())
        .await
        .expect("refusal");
}

fn everything_counts() -> EraseCounts {
    EraseCounts {
        synonyms: 1,
        reservations: 1,
        session_stats: 1,
        lease_refusals: 1,
        leases: 1,
        ..planted_counts()
    }
}

fn erased(outcome: EraseOutcome) -> EraseReport {
    match outcome {
        EraseOutcome::Erased(report) => report,
        EraseOutcome::Held { current, .. } => panic!("erase refused: held by {}", current.holder),
    }
}

async fn assert_only_the_tombstone<D: Dialect>(store: &PgStore<D>, sid: &SessionId) {
    for (table, n) in census(store, sid).await {
        assert_eq!(n, i64::from(table == "session_leases"), "{table}");
    }
}

/// Remove a test session's tombstone and anything else it left.
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

/// Every #23 acceptance check against one live store: a row in every table
/// erased with only the tombstone left and another session untouched; a
/// repeat is `already_absent`; a pre-erase token and an unleased write are
/// refused and recreate nothing; no acquire takes the session over; a live
/// writer refuses the erase; a failure at every step rolls back and the rerun
/// completes.
pub(crate) async fn check_erase_session<D: Dialect>(store: &PgStore<D>) {
    let run = Uuid::new_v4();
    let sid = SessionId::from(format!("erase-{run}"));
    let other = SessionId::from(format!("erase-keep-{run}"));
    let eraser = holder("eraser", 1);
    plant_everything(store, &sid, &eraser).await;
    plant_everything(store, &other, &holder("other-writer", 2)).await;
    let dim = store.vector_dimensions().expect("dim");

    let before = census(store, &sid).await;
    for (table, n) in &before {
        assert!(
            *n >= 1,
            "the fixture must plant a row in {table}: {before:?}"
        );
    }
    let other_before = census(store, &other).await;

    // Crash midway, at every step: rolled back, lease untouched.
    let lease_before = store.read_lease(&sid).await.expect("lease");
    for n in 0..=ERASE_STATEMENTS.len() {
        let fail = FailAt::new(n);
        let hook = |step: &str| fail.step(step);
        let err = store
            .erase(&sid, &eraser, &hook)
            .await
            .expect_err("injected failure surfaces");
        assert!(err.to_string().contains("injected failure"), "{err}");
        assert_eq!(fail.seen(), n + 1, "failed once, at step {n}");
        assert_eq!(census(store, &sid).await, before, "rolled back at step {n}");
        assert_eq!(store.read_lease(&sid).await.expect("lease"), lease_before);
    }

    // A live writer that is not the eraser refuses it.
    match store
        .erase_session(&sid, &holder("someone-else", 3))
        .await
        .expect("erase")
    {
        EraseOutcome::Held { current, .. } => assert_eq!(current.holder, eraser.token()),
        EraseOutcome::Erased(r) => panic!("erased under a live writer: {r:?}"),
    }
    assert_eq!(census(store, &sid).await, before);

    // The real erase, by the lease's own holder, then a repeat.
    let report = erased(store.erase_session(&sid, &eraser).await.expect("erase"));
    assert_eq!(report.removed, everything_counts());
    assert!(!report.already_absent);
    assert!(report.fence_token > lease_before.as_ref().expect("lease").token);
    assert_only_the_tombstone(store, &sid).await;
    let tomb = store.read_lease(&sid).await.expect("read").expect("tomb");
    assert_eq!(tomb.holder, ERASED_HOLDER);
    assert!(matches!(
        store.load_session(&sid).await,
        Err(StoreError::SessionNotFound(_))
    ));
    assert_eq!(census(store, &other).await, other_before, "other session");
    let again = erased(store.erase_session(&sid, &eraser).await.expect("repeat"));
    assert!(again.already_absent);
    assert_eq!(again.fence_token, report.fence_token);

    // Fenced: the old token and an unleased write are refused, recreate
    // nothing, and neither a takeover nor stats nor a refusal writes a row.
    let old = lease_before.expect("lease").token;
    for token in [Some(old), None] {
        let err = store
            .flush(&planted_batch(&sid, dim), token)
            .await
            .expect_err("refused");
        assert!(matches!(err, StoreError::StaleWrite(_)), "{err}");
        assert!(err.to_string().contains("was erased"), "{err}");
    }
    let taken = store
        .acquire_lease(&sid, &holder("new-writer", 4), Duration::from_secs(60))
        .await
        .expect("acquire");
    assert!(
        matches!(&taken, LeaseOutcome::Held { current, .. } if current.holder == ERASED_HOLDER),
        "{taken:?}"
    );
    store
        .write_flush_stats(
            &sid,
            &SessionFlushStats {
                flush_lag_ms: 1,
                log_depth: 1,
            },
        )
        .await
        .expect("stats call");
    store
        .record_lease_refusal(&sid, "new-writer@test#4", ERASED_HOLDER)
        .await
        .expect("refusal call");
    assert_only_the_tombstone(store, &sid).await;

    cleanup(store, &sid).await;
    let _ = store
        .erase_session(&other, &holder("other-writer", 2))
        .await;
    cleanup(store, &other).await;
}

/// #23 review H1 on a live engine: release, erase, then a zombie's write under
/// any token is refused and recreates nothing. Also L4: another session's
/// edges onto the erased nodes go with them.
pub(crate) async fn check_erase_after_release<D: Dialect>(store: &PgStore<D>) {
    let sid = SessionId::from(format!("erase-after-release-{}", Uuid::new_v4()));
    crate::store::erase::testkit::check_erase_after_release_fences(store, &sid).await;
    cleanup(store, &sid).await;

    let run = Uuid::new_v4();
    let (a, b) = (
        SessionId::from(format!("erase-cross-a-{run}")),
        SessionId::from(format!("erase-cross-b-{run}")),
    );
    crate::store::erase::testkit::check_erase_removes_cross_session_edges(store, &a, &b).await;
    cleanup(store, &a).await;
    cleanup(store, &b).await;
}

#[cfg(feature = "store-postgres")]
#[tokio::test]
#[ignore = "live: requires LAMBO_POSTGRES_DSN against pinned pgvector/pgvector:pg17"]
async fn postgres_erase_after_a_release_fences_every_token() {
    let Some(dsn) =
        super::postgres::postgres_dsn_or_skip("postgres_erase_after_a_release_fences_every_token")
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
    check_erase_after_release(&store).await;
}

#[cfg(feature = "store-postgres")]
#[tokio::test]
#[ignore = "live: requires LAMBO_POSTGRES_DSN against pinned pgvector/pgvector:pg17"]
async fn postgres_erases_a_session() {
    let Some(dsn) = super::postgres::postgres_dsn_or_skip("postgres_erases_a_session") else {
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
    check_erase_session(&store).await;
}
