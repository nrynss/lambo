//! Session erasure (#23) on SQLite: coverage of every table, the tombstone
//! fence, idempotency and the crash-midway rerun.

use std::collections::BTreeSet;
use std::time::Duration as StdDuration;

use super::*;
use crate::store::erase::testkit::{planted_batch, planted_counts, FailAt};
use crate::store::erase::{no_fault, EraseCounts, EraseOutcome, ERASED_HOLDER};
use crate::store::lease::{LeaseHolder, LeaseOutcome};
use crate::store::tables_in_ddl;
use crate::types::StoreError;

const DIM: usize = 8;

fn holder(agent: &str, pid: u32) -> LeaseHolder {
    LeaseHolder {
        agent: AgentId::new(agent),
        pid,
        host: "test".into(),
        endpoint: None,
    }
}

async fn rows(store: &SqliteStore, table: &str, sid: &SessionId) -> i64 {
    sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM {table} WHERE session_id = ?1"
    ))
    .bind(&sid.0)
    .fetch_one(store.pool())
    .await
    .unwrap_or_else(|e| panic!("count {table}: {e}"))
}

/// Row count of every table in the shipped DDL for `sid`.
async fn census(store: &SqliteStore, sid: &SessionId) -> Vec<(String, i64)> {
    let mut out = Vec::new();
    for t in tables_in_ddl(INIT_SQL) {
        out.push((t.to_string(), rows(store, t, sid).await));
    }
    out
}

/// Write a row into **every** table of the schema for `sid`: the mutation
/// kinds through `flush`, the rest the way production writes them (the lease,
/// a refusal, published stats) or, for the two snapshot-only tables, by SQL.
/// `eraser` ends up holding the session's live lease.
async fn plant_everything(store: &SqliteStore, sid: &SessionId, eraser: &LeaseHolder) {
    let LeaseOutcome::Acquired(lease) = store
        .acquire_lease(sid, eraser, StdDuration::from_secs(60))
        .await
        .unwrap()
    else {
        panic!("the eraser takes the session's lease");
    };
    store
        .flush(&planted_batch(sid, DIM), Some(lease.token))
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO synonyms (session_id, source_key, canonical_key) VALUES (?1, 'navy', 'blue')",
    )
    .bind(&sid.0)
    .execute(store.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO reservations (session_id, node_id, agent_id, expires_at) \
         VALUES (?1, ?2, 'erase-test', '2099-01-01T00:00:00.000Z')",
    )
    .bind(&sid.0)
    .bind(NodeId::new().0.to_string())
    .execute(store.pool())
    .await
    .unwrap();
    store
        .write_flush_stats(
            sid,
            &SessionFlushStats {
                flush_lag_ms: 5,
                log_depth: 1,
            },
        )
        .await
        .unwrap();
    store
        .record_lease_refusal(sid, "refused@h#9", &eraser.token())
        .await
        .unwrap();
}

/// What erasing one [`plant_everything`] removes.
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

fn erased(outcome: EraseOutcome) -> crate::store::EraseReport {
    match outcome {
        EraseOutcome::Erased(report) => report,
        EraseOutcome::Held { current, .. } => panic!("erase refused: held by {}", current.holder),
    }
}

/// The schema-drift guard: the erase statement list plus the tombstoned lease
/// table is exactly the set of tables the shipped DDL creates. A table added to
/// the migration without an erase statement fails here.
#[test]
fn erase_covers_every_table_in_the_ddl() {
    let ddl: BTreeSet<&str> = tables_in_ddl(INIT_SQL).into_iter().collect();
    let mut covered: BTreeSet<&str> = write_rows::ERASE_STATEMENTS
        .iter()
        .map(|(t, _)| *t)
        .collect();
    covered.insert("session_leases");
    assert_eq!(covered, ddl);
    for (table, sql) in write_rows::ERASE_STATEMENTS {
        assert_eq!(
            *sql,
            format!("DELETE FROM {table} WHERE session_id = ?1"),
            "each statement deletes from the table it is filed under"
        );
    }
}

/// Acceptance: rows in every table, erased; only the tombstone remains; a
/// second session in the same store is untouched; a repeat is
/// `already_absent`.
#[tokio::test]
async fn erase_removes_every_row_and_leaves_only_the_tombstone() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let (sid, other) = (SessionId::new("erase-me"), SessionId::new("keep-me"));
    let eraser = holder("eraser", 1);
    plant_everything(&store, &sid, &eraser).await;
    plant_everything(&store, &other, &holder("other-writer", 2)).await;

    let before = census(&store, &sid).await;
    for (table, n) in &before {
        assert!(
            *n >= 1,
            "the fixture must plant a row in {table}: {before:?}"
        );
    }
    let other_before = census(&store, &other).await;

    let report = erased(store.erase_session(&sid, &eraser).await.unwrap());
    assert_eq!(report.removed, everything_counts());
    assert!(!report.already_absent);

    for (table, n) in census(&store, &sid).await {
        let want = i64::from(table == "session_leases");
        assert_eq!(n, want, "{table} after erase");
    }
    let tomb = store.read_lease(&sid).await.unwrap().expect("tombstone");
    assert_eq!(tomb.holder, ERASED_HOLDER);
    assert_eq!(tomb.endpoint, None);
    assert_eq!(tomb.token, report.fence_token);
    assert!(matches!(
        store.load_session(&sid).await,
        Err(StoreError::SessionNotFound(_))
    ));
    assert_eq!(census(&store, &other).await, other_before, "other session");

    let again = erased(store.erase_session(&sid, &eraser).await.unwrap());
    assert!(again.already_absent);
    assert_eq!(again.removed, EraseCounts::default());
    assert_eq!(
        again.fence_token, report.fence_token,
        "a repeat mints nothing"
    );
}

/// Acceptance: a write holding a pre-erase fencing token is refused after the
/// erase, with the erased message, and does not recreate the session. Neither
/// can an unleased write, a canonization, a takeover, a stats publish or a
/// lease refusal put a row back.
#[tokio::test]
async fn a_pre_erase_token_cannot_write_or_recreate_the_session() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let sid = SessionId::new("zombie");
    let zombie = holder("zombie-serve", 7);
    let LeaseOutcome::Acquired(lease) = store
        .acquire_lease(&sid, &zombie, StdDuration::from_millis(1))
        .await
        .unwrap()
    else {
        panic!("acquire");
    };
    store
        .flush(&planted_batch(&sid, DIM), Some(lease.token))
        .await
        .unwrap();
    tokio::time::sleep(StdDuration::from_millis(20)).await;

    // The zombie's lease lapsed, so the erase takes the session over.
    let report = erased(
        store
            .erase_session(&sid, &holder("eraser", 1))
            .await
            .unwrap(),
    );
    assert!(report.fence_token > lease.token);
    assert_eq!(report.removed.leases, 1);

    for token in [Some(lease.token), None] {
        let err = store
            .flush(&planted_batch(&sid, DIM), token)
            .await
            .expect_err("a write to an erased session is refused");
        assert!(matches!(err, StoreError::StaleWrite(_)), "{err}");
        assert!(err.to_string().contains("was erased"), "{err}");
    }
    let event = CanonizationEvent {
        id: NodeId::new(),
        session_id: sid.clone(),
        node_id: NodeId::new(),
        from_status: CanonizationStatus::None,
        to_status: CanonizationStatus::Candidate,
        blast_radius: None,
        last_demotion_time: None,
        occurred_at: Utc::now(),
    };
    let err = store
        .record_canonization(&event, Some(lease.token))
        .await
        .expect_err("canonization refused");
    assert!(err.to_string().contains("was erased"), "{err}");

    let held = store
        .acquire_lease(&sid, &holder("new-writer", 3), StdDuration::from_secs(60))
        .await
        .unwrap();
    assert!(
        matches!(&held, LeaseOutcome::Held { current, .. } if current.holder == ERASED_HOLDER),
        "no acquire takes an erased session over: {held:?}"
    );
    // The zombie's heartbeat sees the same thing, which is what fences it.
    let refreshed = store
        .refresh_lease(&sid, &zombie, StdDuration::from_secs(60))
        .await
        .unwrap();
    assert!(!refreshed.is_acquired());

    store
        .write_flush_stats(
            &sid,
            &SessionFlushStats {
                flush_lag_ms: 1,
                log_depth: 1,
            },
        )
        .await
        .unwrap();
    store
        .record_lease_refusal(&sid, "new-writer@test#3", ERASED_HOLDER)
        .await
        .unwrap();

    for (table, n) in census(&store, &sid).await {
        assert_eq!(n, i64::from(table == "session_leases"), "{table}");
    }
    assert!(matches!(
        store.load_session(&sid).await,
        Err(StoreError::SessionNotFound(_))
    ));
}

/// Acceptance: a failure part way through (at every step in turn) leaves the
/// session exactly as it was, lease included, and a rerun completes with no
/// rows left.
#[tokio::test]
async fn a_failure_at_any_step_rolls_back_and_a_rerun_completes() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let sid = SessionId::new("crash-midway");
    let eraser = holder("eraser", 1);
    plant_everything(&store, &sid, &eraser).await;
    let before = census(&store, &sid).await;
    let lease_before = store.read_lease(&sid).await.unwrap();

    let steps = write_rows::ERASE_STATEMENTS.len() + 1; // + the vector count
    for n in 0..steps {
        let fail = FailAt::new(n);
        let hook = |step: &str| fail.step(step);
        let err = store
            .erase(&sid, &eraser, &hook)
            .await
            .expect_err("the injected failure surfaces");
        assert!(err.to_string().contains("injected failure"), "{err}");
        assert_eq!(fail.seen(), n + 1, "failed at step {n}");
        assert_eq!(
            census(&store, &sid).await,
            before,
            "rolled back at step {n}"
        );
        assert_eq!(store.read_lease(&sid).await.unwrap(), lease_before);
    }

    let report = erased(store.erase(&sid, &eraser, &no_fault).await.unwrap());
    assert_eq!(report.removed, everything_counts());
    for (table, n) in census(&store, &sid).await {
        assert_eq!(n, i64::from(table == "session_leases"), "{table}");
    }
}

/// Erasure never preempts a live writer: a lease held by someone else is
/// reported and nothing is touched.
#[tokio::test]
async fn a_live_lease_held_by_another_writer_refuses_the_erase() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let sid = SessionId::new("in-use");
    let writer = holder("serve", 4);
    plant_everything(&store, &sid, &writer).await;
    let before = census(&store, &sid).await;

    match store
        .erase_session(&sid, &holder("eraser", 1))
        .await
        .unwrap()
    {
        EraseOutcome::Held { current, .. } => assert_eq!(current.holder, writer.token()),
        EraseOutcome::Erased(r) => panic!("erased under a live writer: {r:?}"),
    }
    assert_eq!(census(&store, &sid).await, before);
    assert_eq!(
        store.read_lease(&sid).await.unwrap().unwrap().holder,
        writer.token()
    );
}

/// An id that never held data is erased (tombstoned) and reports
/// `already_absent`, so a fan-out over accounts with no history completes.
#[tokio::test]
async fn erasing_an_unknown_session_tombstones_it_and_reports_already_absent() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let sid = SessionId::new("never-used");
    let report = erased(
        store
            .erase_session(&sid, &holder("eraser", 1))
            .await
            .unwrap(),
    );
    assert!(report.already_absent);
    assert_eq!(report.fence_token, 1);
    let err = store
        .flush(&planted_batch(&sid, DIM), None)
        .await
        .expect_err("the tombstone fences even a never-used id");
    assert!(err.to_string().contains("was erased"), "{err}");
}

/// #23 review H1: release, then erase, then a zombie's write under any token
/// is refused and recreates nothing.
#[tokio::test]
async fn an_erase_after_a_release_fences_every_token_on_sqlite() {
    let store = test_store();
    store.init_schema().await.unwrap();
    crate::store::erase::testkit::check_erase_after_release_fences(
        &store,
        &SessionId::from("erase-after-release"),
    )
    .await;
}

/// #23 review L4: another session's edges onto the erased nodes go.
#[tokio::test]
async fn erase_removes_cross_session_edges_on_sqlite() {
    let store = test_store();
    store.init_schema().await.unwrap();
    crate::store::erase::testkit::check_erase_removes_cross_session_edges(
        &store,
        &SessionId::from("erased-a"),
        &SessionId::from("kept-b"),
    )
    .await;
}

/// #23 review L5: a fixture seed refuses an erased session id rather than
/// recreating it under the tombstone.
#[cfg(feature = "fixtures")]
#[tokio::test]
async fn seed_refuses_an_erased_session_on_sqlite() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let sid = SessionId::from("erased-then-seeded");
    store
        .erase_session(&sid, &holder("eraser", 1))
        .await
        .unwrap();
    let err = store
        .seed(&GraphSnapshot {
            session_id: sid.clone(),
            ..Default::default()
        })
        .await
        .expect_err("an erased id is not seeded");
    assert!(err.to_string().contains("was erased"), "{err}");
    assert!(store.load_session(&sid).await.is_err());
}

/// #22 PR 3 acceptance: everything Lambo keeps about an image (the concept,
/// its vector, its `embedding_source` and digest, the applied image derive's
/// intent row) is session-keyed, so erasing the session leaves zero rows in
/// every table but the tombstone. The concepts here are written by the real
/// image derive, synchronously and through the write queue.
#[cfg(feature = "embed-fixture")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn erase_leaves_nothing_of_a_derived_image() {
    use crate::embed::{png_with_label, Embedder, FixtureEmbedder};
    use crate::graph::image::{ImageDerive, ImagePayload};
    use crate::memory::Memory;
    use crate::types::MatchStrategy;
    use std::sync::Arc;

    let _quiet = crate::test_util::quiet_logs();
    let (_dir, path) = scratch_db();
    let store = Arc::new(SqliteStore::connect(&path).unwrap());
    store.init_schema().await.unwrap();
    let sid = SessionId::new("erase-an-image");
    let mem = Memory::builder()
        .session(sid.0.clone())
        .agent("agent-a")
        .flush_interval(StdDuration::from_secs(3_600))
        .match_strategy(MatchStrategy::Hybrid)
        .store(store.clone() as Arc<dyn crate::store::GraphStore>)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(EmbeddingContract {
            kind: "fixture".into(),
            model: None,
            dim: FixtureEmbedder::new().dimensions(),
        })
        .build()
        .await
        .unwrap();
    let agent = AgentId::new("agent-a");
    let png = png_with_label("red silk saree");
    let derive = |id| ImageDerive {
        caption: "render 17",
        concept_type: ConceptType::Resource,
        image_id: Some(id),
        payload: ImagePayload::Bytes(crate::surface::image::validate(&png, "image/png").unwrap()),
        parent_of: &[],
        event_time: None,
    };
    mem.derive_image_as(&agent, derive("r17")).await.unwrap();
    let submitted = mem
        .derive_image_async_as(&agent, derive("r18"))
        .await
        .unwrap();
    let answer = mem
        .pipeline()
        .wait(&agent, submitted.receipt, crate::writeq::RECEIPT_WAIT_MAX)
        .await;
    assert_eq!(answer.tag(), "applied", "{answer:?}");
    mem.close().await.unwrap();

    let before = census(&store, &sid).await;
    for table in ["concepts", "write_intents", "sessions"] {
        assert!(
            before.iter().any(|(t, n)| t == table && *n >= 1),
            "{table} holds the image's rows before the erase: {before:?}"
        );
    }
    let sources: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM concepts WHERE session_id = ?1 AND embedding_source IS NOT NULL",
    )
    .bind(&sid.0)
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(sources, 2, "both image concepts carry their source");

    let report = erased(
        store
            .erase_session(&sid, &holder("eraser", 1))
            .await
            .unwrap(),
    );
    assert_eq!(report.removed.concepts, 2);
    assert_eq!(report.removed.vectors, 2);
    for (table, n) in census(&store, &sid).await {
        let want = i64::from(table == "session_leases");
        assert_eq!(n, want, "{table} after erase");
    }
}
