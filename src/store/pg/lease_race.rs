//! #1 fencing: the flush gate's lease read must hold the lease row to commit.
//!
//! PostgreSQL runs the flush transaction at READ COMMITTED, so a plain
//! `SELECT current_token` takes no lock and a takeover that commits between
//! the check and the flush's commit is neither seen nor blocked. The gate reads
//! with [`LEASE_TOKEN_FOR_SHARE_SQL`] (`FOR SHARE`), which makes the takeover's
//! `INSERT ... ON CONFLICT DO UPDATE` wait for our commit.
//!
//! Live only (a real engine and its lease clock). The test drives the same
//! statement constant the flush uses, inside its own open transaction, because
//! `flush_batch` owns its transaction end to end and offers no point between
//! the gate and the commit to interleave at. It therefore proves the lock
//! semantics of the shipped statement, not the call site: that the flush and
//! the canonization write both use this constant is covered by review and by
//! the compiler (no other lease-token read remains in `persistence.rs`). A
//! control leg runs the old plain SELECT and shows the takeover is NOT blocked,
//! so the test would fail if the clause were dropped.

use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;

use super::sql::LEASE_TOKEN_FOR_SHARE_SQL;
use super::{Dialect, PgStore};
use crate::store::lease::{LeaseHolder, LeaseOutcome};
use crate::store::GraphStore;
use crate::types::{AgentId, SessionId};

fn holder(agent: &str, pid: u32) -> LeaseHolder {
    LeaseHolder {
        endpoint: None,
        agent: AgentId::new(agent),
        pid,
        host: "test".into(),
    }
}

/// Acquires a 1 s lease, waits for it to lapse (the row stays, so the stale
/// holder's token is still the current one: exactly the window a takeover
/// races into), and returns the first holder's token.
async fn lapsed_lease<D: Dialect>(store: &PgStore<D>, sid: &SessionId) -> u64 {
    let LeaseOutcome::Acquired(first) = store
        .acquire_lease(sid, &holder("first", 1), Duration::from_secs(1))
        .await
        .expect("acquire")
    else {
        panic!("the first holder must acquire");
    };
    tokio::time::sleep(Duration::from_millis(1_300)).await;
    first.token
}

/// Opens a transaction, reads the lease token with `read_sql`, then starts a
/// takeover from a second connection. Returns the token the transaction saw,
/// whether the takeover finished while the transaction was still open, and the
/// takeover's token after the transaction ended.
async fn takeover_during_open_tx<D: Dialect>(
    store: &Arc<PgStore<D>>,
    sid: &SessionId,
    read_sql: &'static str,
) -> (i64, bool, u64) {
    let pool = store.pool().await.expect("pool");
    let mut tx = pool.begin().await.expect("begin");
    let seen: i64 = sqlx::query_scalar(read_sql)
        .bind(sid.as_str())
        .fetch_one(&mut *tx)
        .await
        .expect("read token");

    let racer = {
        let (store, sid) = (Arc::clone(store), sid.clone());
        tokio::spawn(async move {
            store
                .acquire_lease(&sid, &holder("successor", 2), Duration::from_secs(60))
                .await
        })
    };
    tokio::pin!(racer);
    // Long enough for a takeover that is not blocked to finish many times over.
    let finished_early = tokio::time::timeout(Duration::from_millis(1_500), &mut racer)
        .await
        .is_ok();
    tx.commit().await.expect("commit");
    let outcome = racer.await.expect("join").expect("takeover");
    let LeaseOutcome::Acquired(second) = outcome else {
        panic!("the lapsed lease must be taken over once the reader is done");
    };
    (seen, finished_early, second.token)
}

async fn cleanup<D: Dialect>(store: &PgStore<D>, sid: &SessionId) {
    let pool = store.pool().await.expect("pool");
    for table in ["session_leases", "lease_refusals"] {
        let _ = sqlx::query(&format!("DELETE FROM {table} WHERE session_id = $1"))
            .bind(sid.as_str())
            .execute(&pool)
            .await;
    }
}

pub(crate) async fn check_takeover_waits_for_the_fence_holder<D: Dialect>(store: Arc<PgStore<D>>) {
    // Locked read: the takeover must wait for the transaction.
    let locked = SessionId::from(format!("lease-race-locked-{}", Uuid::new_v4()));
    let first = lapsed_lease(&store, &locked).await;
    let (seen, finished_early, second) =
        takeover_during_open_tx(&store, &locked, LEASE_TOKEN_FOR_SHARE_SQL).await;
    assert_eq!(
        seen,
        i64::try_from(first).unwrap(),
        "the gate read the lapsed holder's token"
    );
    assert!(
        !finished_early,
        "a takeover must block while a flush holds the lease row share-locked"
    );
    assert!(
        second > first,
        "the takeover completes after the commit with a higher token"
    );
    cleanup(&store, &locked).await;

    // Control: the old unlocked read does not hold the takeover back, so the
    // leg above is discriminating.
    let plain = SessionId::from(format!("lease-race-plain-{}", Uuid::new_v4()));
    let first = lapsed_lease(&store, &plain).await;
    let (seen, finished_early, _) = takeover_during_open_tx(
        &store,
        &plain,
        "SELECT current_token FROM session_leases WHERE session_id = $1",
    )
    .await;
    assert_eq!(seen, i64::try_from(first).unwrap());
    assert!(
        finished_early,
        "control: an unlocked read must not block the takeover (else the test proves nothing)"
    );
    cleanup(&store, &plain).await;
}

#[tokio::test]
#[ignore = "live: requires LAMBO_POSTGRES_DSN against pinned pgvector/pgvector:pg17"]
async fn postgres_takeover_waits_for_the_fence_check() {
    let Some(dsn) =
        super::postgres::postgres_dsn_or_skip("postgres_takeover_waits_for_the_fence_check")
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
    check_takeover_waits_for_the_fence_holder(Arc::new(store)).await;
}
