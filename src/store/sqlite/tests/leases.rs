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

/// Rows of `table` with this id, read straight from the store.
async fn rows_with_id(store: &SqliteStore, table: &str, id: NodeId) -> i64 {
    sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table} WHERE id = ?1"))
        .bind(id.0.to_string())
        .fetch_one(store.pool())
        .await
        .unwrap()
}

/// #1 fencing, delete-only batches. `DeleteNode`/`DeleteEdge` carry no
/// session, so the fenced set used to be built from the other mutations only,
/// and a batch of nothing but deletes (what a GC sweep drains) committed with
/// no token check at all. This is the zombie-writer shape: the first holder's
/// lease lapses, a second writer takes the session over (token 2), and the
/// first one's stale token must not be able to delete the second one's rows,
/// through either delete kind.
#[tokio::test]
async fn a_stale_token_cannot_flush_a_delete_only_batch() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let sid = SessionId::from("fenced-deletes");
    let ts = Utc::now();
    let (origin, concept, derives) = (NodeId::new(), NodeId::new(), NodeId::new());
    let planted = MutationBatch {
        mutation_epoch: 1,
        gc_mark: Default::default(),
        mutations: vec![
            plant_interaction(&sid, origin, None, ts),
            plant_concept(&sid, concept, origin, "doomed", ConceptType::Entity, ts),
            Mutation::UpsertEdge {
                edge: crate::types::Edge {
                    id: derives,
                    session_id: sid.clone(),
                    source: origin,
                    target: concept,
                    edge_type: EdgeType::Derives,
                    weight: 1.0,
                    reinforcements: 0,
                    created_at: ts,
                    last_reinforced: ts,
                    event_time: None,
                },
            },
        ],
    };
    store.flush(&planted, None).await.unwrap();

    // Holder 1 lapses; holder 2 takes over and mints token 2.
    let ttl = Duration::from_secs(1);
    let LeaseOutcome::Acquired(first) = store
        .acquire_lease(&sid, &lease_holder("zombie", 1), ttl)
        .await
        .unwrap()
    else {
        panic!("the first holder must acquire");
    };
    tokio::time::sleep(Duration::from_millis(1_300)).await;
    let LeaseOutcome::Acquired(second) = store
        .acquire_lease(&sid, &lease_holder("successor", 2), ttl)
        .await
        .unwrap()
    else {
        panic!("the successor must take the lapsed lease over");
    };
    assert!(second.token > first.token);

    let deletes = |mutations: Vec<Mutation>| MutationBatch {
        mutation_epoch: 2,
        gc_mark: Default::default(),
        mutations,
    };
    let edge_only = deletes(vec![Mutation::DeleteEdge { id: derives }]);
    let node_only = deletes(vec![Mutation::DeleteNode { id: concept }]);
    for (what, batch) in [("DeleteEdge", &edge_only), ("DeleteNode", &node_only)] {
        for token in [Some(first.token), None] {
            let got = store.flush(batch, token).await;
            assert!(
                matches!(got, Err(StoreError::StaleWrite(_))),
                "a {what}-only batch under token {token:?} must be fenced, got {got:?}"
            );
        }
    }
    assert_eq!(rows_with_id(&store, "concepts", concept).await, 1);
    assert_eq!(rows_with_id(&store, "edges", derives).await, 1);

    // The current holder's token passes, and the deletes land.
    store.flush(&edge_only, Some(second.token)).await.unwrap();
    store.flush(&node_only, Some(second.token)).await.unwrap();
    assert_eq!(rows_with_id(&store, "concepts", concept).await, 0);
    assert_eq!(rows_with_id(&store, "edges", derives).await, 0);

    // A delete of a row that is already gone resolves no session: a no-op,
    // not an error, whatever the token.
    store.flush(&node_only, Some(first.token)).await.unwrap();
}

/// #1 fencing, delete-only batches, cross-session edge. A node delete also
/// removes every edge incident to the node, and edges are session-scoped while
/// node ids are global, so the deleted node can sit in session A (whose lease
/// the caller still holds) with an incident edge in session B (whose lease has
/// moved on). The delete must be fenced on B's lease too; a node with no
/// foreign edge is deleted normally, so only the incident-edge lookup can be
/// what refuses the first batch.
#[tokio::test]
async fn a_node_delete_is_fenced_on_the_session_of_an_incident_edge() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let (a, b) = (SessionId::from("fence-a"), SessionId::from("fence-b"));
    let ts = Utc::now();
    let (origin_a, shared, plain, origin_b, cross) = (
        NodeId::new(),
        NodeId::new(),
        NodeId::new(),
        NodeId::new(),
        NodeId::new(),
    );
    let planted = MutationBatch {
        mutation_epoch: 1,
        gc_mark: Default::default(),
        mutations: vec![
            plant_interaction(&a, origin_a, None, ts),
            plant_concept(&a, shared, origin_a, "shared", ConceptType::Entity, ts),
            plant_concept(&a, plain, origin_a, "plain", ConceptType::Entity, ts),
            plant_interaction(&b, origin_b, None, ts),
            // Session B's edge points at session A's concept.
            Mutation::UpsertEdge {
                edge: crate::types::Edge {
                    id: cross,
                    session_id: b.clone(),
                    source: origin_b,
                    target: shared,
                    edge_type: EdgeType::Derives,
                    weight: 1.0,
                    reinforcements: 0,
                    created_at: ts,
                    last_reinforced: ts,
                    event_time: None,
                },
            },
        ],
    };
    store.flush(&planted, None).await.unwrap();

    // A is held by its first holder (token 1). B lapses and is taken over, so
    // its current token is 2 and token 1 is stale for B only.
    let long = Duration::from_secs(60);
    let LeaseOutcome::Acquired(held_a) = store
        .acquire_lease(&a, &lease_holder("holder-a", 1), long)
        .await
        .unwrap()
    else {
        panic!("A must acquire");
    };
    let short = Duration::from_secs(1);
    store
        .acquire_lease(&b, &lease_holder("zombie-b", 2), short)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1_300)).await;
    let LeaseOutcome::Acquired(taken_b) = store
        .acquire_lease(&b, &lease_holder("successor-b", 3), short)
        .await
        .unwrap()
    else {
        panic!("the successor must take B over");
    };
    assert!(taken_b.token > held_a.token);

    let deletes = |mutations: Vec<Mutation>| MutationBatch {
        mutation_epoch: 2,
        gc_mark: Default::default(),
        mutations,
    };
    // The node lives in A (token current) but its incident edge is B's.
    let shared_only = deletes(vec![Mutation::DeleteNode { id: shared }]);
    let got = store.flush(&shared_only, Some(held_a.token)).await;
    assert!(
        matches!(got, Err(StoreError::StaleWrite(_))),
        "deleting a node whose incident edge is in a taken-over session must be fenced, got {got:?}"
    );
    assert_eq!(rows_with_id(&store, "concepts", shared).await, 1);
    assert_eq!(rows_with_id(&store, "edges", cross).await, 1);

    // Control: a node of A with no foreign edge is deleted under the same token.
    let plain_only = deletes(vec![Mutation::DeleteNode { id: plain }]);
    store.flush(&plain_only, Some(held_a.token)).await.unwrap();
    assert_eq!(rows_with_id(&store, "concepts", plain).await, 0);

    // B's current token covers both sessions (>= each lease), so it deletes.
    store
        .flush(&shared_only, Some(taken_b.token))
        .await
        .unwrap();
    assert_eq!(rows_with_id(&store, "concepts", shared).await, 0);
    assert_eq!(rows_with_id(&store, "edges", cross).await, 0);
}

/// The delete lookup is chunked over an `IN` list. A batch with more ids than
/// one chunk still fences on a single leased row wherever it falls, and the
/// same batch minus that row deletes everything.
#[tokio::test]
async fn a_delete_batch_larger_than_one_lookup_chunk_is_still_fenced() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let (free, leased) = (SessionId::from("free"), SessionId::from("leased"));
    let ts = Utc::now();
    let origin_free = NodeId::new();
    let origin_leased = NodeId::new();
    let mut mutations = vec![
        plant_interaction(&free, origin_free, None, ts),
        plant_interaction(&leased, origin_leased, None, ts),
    ];
    let free_ids: Vec<NodeId> = (0..450).map(|_| NodeId::new()).collect();
    for (i, id) in free_ids.iter().enumerate() {
        mutations.push(plant_concept(
            &free,
            *id,
            origin_free,
            &format!("free-{i}"),
            ConceptType::Entity,
            ts,
        ));
    }
    let fenced_node = NodeId::new();
    mutations.push(plant_concept(
        &leased,
        fenced_node,
        origin_leased,
        "leased",
        ConceptType::Entity,
        ts,
    ));
    let planted = MutationBatch {
        mutation_epoch: 1,
        gc_mark: Default::default(),
        mutations,
    };
    store.flush(&planted, None).await.unwrap();
    store
        .acquire_lease(&leased, &lease_holder("holder", 1), Duration::from_secs(60))
        .await
        .unwrap();

    let delete = |ids: &[NodeId]| MutationBatch {
        mutation_epoch: 2,
        gc_mark: Default::default(),
        mutations: ids
            .iter()
            .map(|id| Mutation::DeleteNode { id: *id })
            .collect(),
    };
    let mut all = free_ids.clone();
    all.push(fenced_node);
    let got = store.flush(&delete(&all), None).await;
    assert!(
        matches!(got, Err(StoreError::StaleWrite(_))),
        "a leased row among 451 deletes must fence the batch, got {got:?}"
    );
    assert_eq!(rows_with_id(&store, "concepts", fenced_node).await, 1);
    assert_eq!(rows_with_id(&store, "concepts", free_ids[0]).await, 1);

    store.flush(&delete(&free_ids), None).await.unwrap();
    assert_eq!(rows_with_id(&store, "concepts", free_ids[449]).await, 0);
    assert_eq!(rows_with_id(&store, "concepts", fenced_node).await, 1);
}

/// #23 review H2: a release keeps the fencing token, so a writer holding a
/// token from before the release is refused against the next holder.
#[tokio::test]
async fn a_release_keeps_the_fencing_token_on_sqlite() {
    let store = test_store();
    store.init_schema().await.unwrap();
    crate::store::lease::testkit::check_release_keeps_the_fencing_token(
        &store,
        &SessionId::from("release-keeps-token"),
        &SessionId::from("release-zombie"),
    )
    .await;
}

/// #23 review H2: the documented operator override is an UPDATE that keeps
/// the fencing token (so the next acquire mints above the wedged holder's)
/// and never lifts an erasure tombstone. Runs the shipped statement itself.
#[tokio::test]
async fn the_operator_override_keeps_the_token_and_spares_a_tombstone() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let override_for = |sid: &str| crate::store::lease::OPERATOR_OVERRIDE.replace("<session>", sid);
    let ttl = Duration::from_secs(60);

    let sid = SessionId::from("wedged");
    let wedged = lease_holder("wedged", 1);
    let LeaseOutcome::Acquired(w) = store.acquire_lease(&sid, &wedged, ttl).await.unwrap() else {
        panic!("fresh acquire");
    };
    sqlx::query(&override_for("wedged"))
        .execute(store.pool())
        .await
        .unwrap();
    let row = store.read_lease(&sid).await.unwrap().expect("row kept");
    assert_eq!(row.holder, crate::store::lease::RELEASED_HOLDER);
    assert_eq!(row.token, w.token);
    let LeaseOutcome::Acquired(next) = store
        .acquire_lease(&sid, &lease_holder("next", 2), ttl)
        .await
        .unwrap()
    else {
        panic!("the override must let the next writer in at once");
    };
    assert!(next.token > w.token);
    assert!(matches!(
        store
            .flush(
                &crate::store::lease::testkit::interaction_batch(&sid, "wedged"),
                Some(w.token)
            )
            .await,
        Err(StoreError::StaleWrite(_))
    ));

    let erased = SessionId::from("erased");
    let eraser = lease_holder("eraser", 3);
    store.erase_session(&erased, &eraser).await.unwrap();
    let tomb = store.read_lease(&erased).await.unwrap().expect("tombstone");
    sqlx::query(&override_for("erased"))
        .execute(store.pool())
        .await
        .unwrap();
    assert_eq!(
        store.read_lease(&erased).await.unwrap(),
        Some(tomb),
        "the override must not lift a tombstone"
    );
}
