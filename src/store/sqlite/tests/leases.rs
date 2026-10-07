//! Single-writer lease (T8.6).

use super::*;

/// **JE2E-1.** `lease_refusals` had no retention at all — the DDL said so
/// out loud ("rows are read by the poller and need no retention") — so the
/// founding scenario, a client auto-respawning a losing serve, inserted one
/// row per respawn forever. The purge now rides the insert, the lazy shape
/// `consume_write_intent` uses.
///
/// The old row is planted with an explicit stamp rather than by waiting out
/// the hour: the assertion is about the retention *boundary*, not about a
/// clock. Both sides of it are asserted in one call, so a purge that swept
/// everything would fail as loudly as one that swept nothing.
#[tokio::test]
async fn recording_a_lease_refusal_purges_this_sessions_expired_ones() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let session = SessionId::new("purge-me");
    let other = SessionId::new("leave-me-alone");
    let retention = crate::store::lease::LEASE_REFUSAL_RETENTION.as_secs() as i64;

    // Two rows past the window and one inside it, plus one belonging to a
    // different session (the purge is session-scoped, like every other
    // statement on this path).
    for (sid, ago, by) in [
        (&session, retention + 60, "ancient-a@h#1"),
        (&session, retention + 1, "ancient-b@h#1"),
        (&session, retention - 60, "recent@h#1"),
        (&other, retention + 60, "other-session@h#1"),
    ] {
        sqlx::query(
            "INSERT INTO lease_refusals (session_id, refused_at, refused_by, current_holder) \
                 VALUES (?1, strftime('%Y-%m-%dT%H:%M:%fZ','now',?2), ?3, 'holder@h#1')",
        )
        .bind(&sid.0)
        .bind(format!("-{ago} seconds"))
        .bind(by)
        .execute(store.pool())
        .await
        .unwrap();
    }

    store
        .record_lease_refusal(&session, "fresh@h#1", "holder@h#1")
        .await
        .unwrap();

    let long_ago = Utc.timestamp_opt(0, 0).unwrap();
    let ours: Vec<String> = store
        .pending_lease_refusals(&session, long_ago)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.refused_by)
        .collect();
    assert!(
        !ours.iter().any(|b| b.starts_with("ancient")),
        "rows past the retention window must be gone: {ours:?}"
    );
    assert!(
        ours.iter().any(|b| b == "recent@h#1"),
        "a row inside the window must survive — a purge that swept everything \
             would break the holder-side line this table exists for: {ours:?}"
    );
    assert!(
        ours.iter().any(|b| b == "fresh@h#1"),
        "and the row just recorded is there: {ours:?}"
    );
    let theirs: Vec<String> = store
        .pending_lease_refusals(&other, long_ago)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.refused_by)
        .collect();
    assert_eq!(
        theirs,
        vec!["other-session@h#1".to_string()],
        "another session's expired row is not this session's to sweep"
    );
}

// -----------------------------------------------------------------------
// Single-writer lease (T8.6)
// -----------------------------------------------------------------------

fn lease_holder(agent: &str, pid: u32) -> LeaseHolder {
    LeaseHolder {
        endpoint: None,
        agent: AgentId::new(agent),
        pid,
        host: "test-host".into(),
    }
}

/// T8.6: the acquire/Held/release/expiry contract on a single connection.
#[tokio::test]
async fn lease_lifecycle_on_one_connection() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let sid = SessionId::from("s");
    let a = lease_holder("agent-a", 100);
    let b = lease_holder("agent-b", 200);
    let ttl = Duration::from_secs(30);

    assert!(store
        .acquire_lease(&sid, &a, ttl)
        .await
        .unwrap()
        .is_acquired());
    match store.acquire_lease(&sid, &b, ttl).await.unwrap() {
        LeaseOutcome::Held { current, .. } => assert_eq!(current.holder, a.token()),
        other => panic!("expected Held, got {other:?}"),
    }
    // Refresh keeps acquired_at.
    let LeaseOutcome::Acquired(first) = store.acquire_lease(&sid, &a, ttl).await.unwrap() else {
        panic!("A refresh must succeed");
    };
    let LeaseOutcome::Acquired(refreshed) = store.refresh_lease(&sid, &a, ttl).await.unwrap()
    else {
        panic!("refresh must succeed");
    };
    assert_eq!(first.acquired_at, refreshed.acquired_at);

    store.release_lease(&sid, &a).await.unwrap();
    assert!(store
        .acquire_lease(&sid, &b, ttl)
        .await
        .unwrap()
        .is_acquired());
}

/// J2: the endpoint a holder publishes round-trips through the real SQL —
/// out of the acquire's `RETURNING`, out of the loser's `Held` read-back,
/// and out of the standalone `read_lease` the proxy path uses — and a
/// refresh republishes it rather than dropping it.
///
/// The NULL half is asserted too, and it is the load-bearing one: every
/// writer that is not a `serve` process leaves the column NULL, and a
/// refused serve must be able to tell "no hub here" from "a hub at <path>".
#[tokio::test]
async fn the_lease_endpoint_round_trips_and_a_refresh_republishes_it() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let sid = SessionId::from("s");
    let hub = lease_holder("agent-a", 100).reachable_at("/run/lambo/s-abc.sock");
    let loser = lease_holder("agent-b", 200);
    let ttl = Duration::from_secs(30);

    let LeaseOutcome::Acquired(taken) = store.acquire_lease(&sid, &hub, ttl).await.unwrap() else {
        panic!("the hub must win a fresh lease");
    };
    assert_eq!(taken.endpoint.as_deref(), Some("/run/lambo/s-abc.sock"));

    // The refusal a proxying serve reads: the endpoint arrives with the
    // holder, so one round trip tells the loser where to forward.
    match store.acquire_lease(&sid, &loser, ttl).await.unwrap() {
        LeaseOutcome::Held { current, .. } => {
            assert_eq!(current.holder, hub.token());
            assert_eq!(current.endpoint.as_deref(), Some("/run/lambo/s-abc.sock"));
        }
        other => panic!("expected Held, got {other:?}"),
    }

    // The read the proxy repeats on every reconnect attempt.
    let row = store.read_lease(&sid).await.unwrap().expect("a live row");
    assert_eq!(row.endpoint.as_deref(), Some("/run/lambo/s-abc.sock"));
    assert_eq!(row.holder, hub.token());

    // A refresh is the heartbeat; it must not blank the address.
    let LeaseOutcome::Acquired(refreshed) = store.refresh_lease(&sid, &hub, ttl).await.unwrap()
    else {
        panic!("the hub's own refresh must succeed");
    };
    assert_eq!(refreshed.endpoint.as_deref(), Some("/run/lambo/s-abc.sock"));

    // A writer that is not a serve process publishes nothing, and the row
    // says so — "no hub here" is a fact, not missing data.
    store.release_lease(&sid, &hub).await.unwrap();
    let LeaseOutcome::Acquired(cli) = store.acquire_lease(&sid, &loser, ttl).await.unwrap() else {
        panic!("a released lease must be re-acquirable");
    };
    assert_eq!(cli.endpoint, None);
    assert_eq!(
        store.read_lease(&sid).await.unwrap().unwrap().endpoint,
        None
    );
}

/// J2: `read_lease` on a session no writer has ever leased is `None`, not an
/// error — a proxy must be able to distinguish "nobody holds this" from a
/// store failure, because only the first one is worth retrying.
#[tokio::test]
async fn read_lease_is_none_for_an_unleased_session() {
    let store = test_store();
    store.init_schema().await.unwrap();
    assert!(store
        .read_lease(&SessionId::from("never-leased"))
        .await
        .unwrap()
        .is_none());
}

/// J2: an ALREADY-PROVISIONED store converges on the next attach — the
/// dogfood rig's `lambo-dev.db` must not need a re-provision. Built by
/// creating the pre-J2 five-column table by hand, then running the real
/// `init_schema` over it.
#[tokio::test]
async fn a_pre_j2_lease_table_gains_the_endpoint_column_on_init() {
    let store = test_store();
    // The exact DDL that shipped before J2 (five columns).
    sqlx::query(
        "CREATE TABLE session_leases (\
                 session_id  TEXT PRIMARY KEY, \
                 holder      TEXT NOT NULL, \
                 acquired_at TEXT NOT NULL, \
                 expires_at  TEXT NOT NULL, \
                 current_token INTEGER NOT NULL DEFAULT 0)",
    )
    .execute(store.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO session_leases VALUES \
                 ('legacy', 'old@host#1', '2026-01-01T00:00:00.000Z', \
                  '2026-01-01T00:00:01.000Z', 7)",
    )
    .execute(store.pool())
    .await
    .unwrap();

    store.init_schema().await.unwrap();

    // The pre-existing row survives and reads as "published no endpoint",
    // which is exactly what a pre-J2 holder did.
    let legacy = store
        .read_lease(&SessionId::from("legacy"))
        .await
        .unwrap()
        .expect("the pre-J2 row must survive the ALTER");
    assert_eq!(legacy.endpoint, None);
    assert_eq!(legacy.token, 7);
    // And the column is now writable.
    let sid = SessionId::from("fresh");
    let hub = lease_holder("a", 1).reachable_at("/run/lambo/fresh.sock");
    assert!(store
        .acquire_lease(&sid, &hub, Duration::from_secs(30))
        .await
        .unwrap()
        .is_acquired());
    assert_eq!(
        store
            .read_lease(&sid)
            .await
            .unwrap()
            .unwrap()
            .endpoint
            .as_deref(),
        Some("/run/lambo/fresh.sock")
    );
}

/// T8.6: expiry-after-crash on sqlite — an unreleased lease blocks before the
/// TTL and is reclaimable after it. Uses a 1s TTL to stay well clear of any
/// SQLite fractional-second rounding.
#[tokio::test]
async fn an_unreleased_lease_expires_on_sqlite() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let sid = SessionId::from("s");
    let dead = lease_holder("dead", 1);
    let live = lease_holder("live", 2);
    let ttl = Duration::from_secs(1);

    store.acquire_lease(&sid, &dead, ttl).await.unwrap();
    assert!(matches!(
        store.acquire_lease(&sid, &live, ttl).await.unwrap(),
        LeaseOutcome::Held { .. }
    ));
    tokio::time::sleep(Duration::from_millis(1_300)).await;
    assert!(store
        .acquire_lease(&sid, &live, ttl)
        .await
        .unwrap()
        .is_acquired());
}

/// T8.6: **two independent connections to one DB file** — the cross-process
/// shape in miniature (a subprocess variant lives in
/// `tests/serve_single_writer_lease.rs`). One acquires, the other is refused
/// and told the holder; after a release the second wins.
#[tokio::test]
async fn two_connections_on_one_file_serialize_on_the_lease() {
    let (_dir, path) = scratch_db();
    let sid = SessionId::from("shared");
    let a = lease_holder("proc-a", 111);
    let b = lease_holder("proc-b", 222);
    let ttl = Duration::from_secs(30);

    let store_a = SqliteStore::connect(&path).unwrap();
    store_a.init_schema().await.unwrap();
    let store_b = SqliteStore::connect(&path).unwrap();

    assert!(store_a
        .acquire_lease(&sid, &a, ttl)
        .await
        .unwrap()
        .is_acquired());
    match store_b.acquire_lease(&sid, &b, ttl).await.unwrap() {
        LeaseOutcome::Held { current, .. } => assert_eq!(current.holder, a.token()),
        other => panic!("the second connection must be refused, got {other:?}"),
    }
    store_a.release_lease(&sid, &a).await.unwrap();
    assert!(store_b
        .acquire_lease(&sid, &b, ttl)
        .await
        .unwrap()
        .is_acquired());

    drop(store_a);
    drop(store_b);
}
