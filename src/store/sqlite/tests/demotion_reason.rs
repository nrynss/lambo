//! #20: the `reason` / `superseded_by` audit columns. Legacy rows, the
//! guarded upgrade, round-trips and malformed-row refusal.

use super::*;
use crate::types::DemotionReason;

fn ts() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 10, 12, 0, 0).unwrap()
}

async fn seeded(store: &SqliteStore, sid: &SessionId, ids: &[NodeId]) -> NodeId {
    let i1 = NodeId::new();
    let mut mutations = vec![plant_interaction(sid, i1, None, ts())];
    for (n, &id) in ids.iter().enumerate() {
        mutations.push(plant_concept(
            sid,
            id,
            i1,
            &format!("concept {n}"),
            ConceptType::Entity,
            ts(),
        ));
    }
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations,
            },
            None,
        )
        .await
        .unwrap();
    i1
}

fn event(
    sid: &SessionId,
    node: NodeId,
    from: CanonizationStatus,
    to: CanonizationStatus,
    reason: Option<DemotionReason>,
) -> CanonizationEvent {
    let demotion = from == CanonizationStatus::Canonical && to == CanonizationStatus::None;
    CanonizationEvent {
        id: NodeId::new(),
        session_id: sid.clone(),
        node_id: node,
        from_status: from,
        to_status: to,
        blast_radius: None,
        last_demotion_time: demotion.then(ts),
        occurred_at: ts(),
        reason,
    }
}

async fn raw_event(
    store: &SqliteStore,
    sid: &SessionId,
    node: NodeId,
    from: &str,
    to: &str,
    reason: Option<&str>,
    by: Option<String>,
) {
    sqlx::query(
        "INSERT INTO canonization_events (id, session_id, node_id, from_status, to_status, \
         blast_radius, last_demotion_time, occurred_at, reason, superseded_by) \
         VALUES (?, ?, ?, ?, ?, NULL, NULL, '2026-08-10T12:00:00.000Z', ?, ?)",
    )
    .bind(NodeId::new().0.to_string())
    .bind(&sid.0)
    .bind(node.0.to_string())
    .bind(from)
    .bind(to)
    .bind(reason)
    .bind(by)
    .execute(store.pool())
    .await
    .unwrap();
}

/// A store provisioned before #20 has `canonization_events` without the two
/// columns. `init_schema` adds them through the PRAGMA guard (SQLite has no
/// `ADD COLUMN IF NOT EXISTS`), a second init is a no-op, and the
/// historical rows read back as a Budget demotion and a reasonless promotion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pre_20_audit_table_converges_and_its_rows_read_as_budget() {
    let store = test_store();
    sqlx::query(
        "CREATE TABLE canonization_events (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            node_id TEXT NOT NULL,
            from_status TEXT NOT NULL,
            to_status TEXT NOT NULL,
            blast_radius INTEGER,
            last_demotion_time TEXT,
            occurred_at TEXT NOT NULL
        )",
    )
    .execute(store.pool())
    .await
    .unwrap();
    let sid = SessionId::from("legacy-20");
    let c = NodeId::new();
    for (id, from, to, at) in [
        (
            NodeId::new(),
            "Venerable",
            "Canonical",
            "2026-08-10T12:00:00.000Z",
        ),
        (
            NodeId::new(),
            "Canonical",
            "None",
            "2026-08-10T12:01:00.000Z",
        ),
    ] {
        sqlx::query(
            "INSERT INTO canonization_events (id, session_id, node_id, from_status, \
             to_status, blast_radius, last_demotion_time, occurred_at) \
             VALUES (?, ?, ?, ?, ?, NULL, NULL, ?)",
        )
        .bind(id.0.to_string())
        .bind(&sid.0)
        .bind(c.0.to_string())
        .bind(from)
        .bind(to)
        .bind(at)
        .execute(store.pool())
        .await
        .unwrap();
    }

    store.init_schema().await.unwrap();
    store.init_schema().await.unwrap();
    let cols: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('canonization_events')")
            .fetch_all(store.pool())
            .await
            .unwrap();
    for want in ["reason", "superseded_by"] {
        assert!(cols.iter().any(|c| c == want), "{want} must be added");
    }
    store.preflight_schema().await.unwrap();

    seeded(&store, &sid, &[c]).await;
    let snap = store.load_session(&sid).await.unwrap();
    assert_eq!(snap.canonization_events.len(), 2);
    let promotion = &snap.canonization_events[0];
    let demotion = &snap.canonization_events[1];
    assert_eq!(promotion.reason, None);
    assert_eq!(
        promotion.demotion_reason(),
        None,
        "a promotion has no reason"
    );
    assert_eq!(demotion.reason, None);
    assert_eq!(demotion.demotion_reason(), Some(DemotionReason::Budget));
}

/// A supersession demotion round-trips through both write paths (the
/// immediate `record_canonization` and the write-behind flush), and its
/// `superseded_by` survives the superseding concept being collected: the
/// column is a bare id, no foreign key, no cascade.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_supersession_round_trips_and_outlives_its_source() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let sid = SessionId::from("superseded");
    let (c, s, d) = (NodeId::new(), NodeId::new(), NodeId::new());
    seeded(&store, &sid, &[c, s, d]).await;

    let up = |node| {
        [
            event(
                &sid,
                node,
                CanonizationStatus::None,
                CanonizationStatus::Venerable,
                None,
            ),
            event(
                &sid,
                node,
                CanonizationStatus::Venerable,
                CanonizationStatus::Canonical,
                None,
            ),
        ]
    };
    for ev in up(c).iter().chain(up(d).iter()) {
        store.record_canonization(ev, None).await.unwrap();
    }
    let recorded = event(
        &sid,
        c,
        CanonizationStatus::Canonical,
        CanonizationStatus::None,
        Some(DemotionReason::Superseded { by: s }),
    );
    store.record_canonization(&recorded, None).await.unwrap();
    let flushed = event(
        &sid,
        d,
        CanonizationStatus::Canonical,
        CanonizationStatus::None,
        Some(DemotionReason::Budget),
    );
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![
                    Mutation::CanonizationTransition {
                        event: flushed.clone(),
                    },
                    // The superseding concept is collected afterwards.
                    Mutation::DeleteNode { id: s },
                ],
            },
            None,
        )
        .await
        .unwrap();

    let snap = store.load_session(&sid).await.unwrap();
    assert!(snap.concepts.iter().all(|k| k.id != s), "s was deleted");
    let got = |id| {
        snap.canonization_events
            .iter()
            .find(|e| e.id == id)
            .cloned()
            .unwrap()
    };
    assert_eq!(got(recorded.id), recorded);
    assert_eq!(
        got(recorded.id).demotion_reason(),
        Some(DemotionReason::Superseded { by: s })
    );
    assert_eq!(
        got(flushed.id),
        flushed,
        "an explicit Budget round-trips too"
    );
}

/// Every write gate refuses a reason on a promotion and a self-supersession,
/// before anything is written.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reason_on_a_promotion_or_a_self_supersession_is_refused() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let sid = SessionId::from("refused");
    let c = NodeId::new();
    seeded(&store, &sid, &[c]).await;
    let bad = [
        event(
            &sid,
            c,
            CanonizationStatus::None,
            CanonizationStatus::Candidate,
            Some(DemotionReason::Budget),
        ),
        event(
            &sid,
            c,
            CanonizationStatus::Canonical,
            CanonizationStatus::None,
            Some(DemotionReason::Superseded { by: c }),
        ),
    ];
    for ev in &bad {
        let err = store.record_canonization(ev, None).await.unwrap_err();
        assert!(matches!(err, StoreError::Invariant(_)), "{err:?}");
        let err = store
            .flush(
                &MutationBatch {
                    mutation_epoch: 0,
                    gc_mark: Default::default(),
                    mutations: vec![Mutation::CanonizationTransition { event: ev.clone() }],
                },
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Invariant(_)), "{err:?}");
    }
    let snap = store.load_session(&sid).await.unwrap();
    assert!(snap.canonization_events.is_empty());
    assert_eq!(
        snap.concepts[0].canonization_status,
        CanonizationStatus::None
    );
}

/// A stored row whose pair does not name exactly one reason is refused on
/// load, never read as Budget: `budget` with an id, `superseded` without
/// one, an id without a reason, unknown text, and a reason on a promotion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_malformed_reason_row_is_refused_on_load() {
    let by = || Some(NodeId::new().0.to_string());
    let cases: [(&str, &str, Option<&str>, Option<String>); 5] = [
        ("Canonical", "None", Some("budget"), by()),
        ("Canonical", "None", Some("superseded"), None),
        ("Canonical", "None", None, by()),
        ("Canonical", "None", Some("evicted"), None),
        ("Venerable", "Canonical", Some("budget"), None),
    ];
    for (n, (from, to, reason, by)) in cases.into_iter().enumerate() {
        let store = test_store();
        store.init_schema().await.unwrap();
        let sid = SessionId::from(format!("malformed-{n}"));
        let c = NodeId::new();
        seeded(&store, &sid, &[c]).await;
        raw_event(&store, &sid, c, from, to, reason, by).await;
        let err = store.load_session(&sid).await.unwrap_err();
        assert!(matches!(err, StoreError::Invariant(_)), "case {n}: {err:?}");
    }
}
