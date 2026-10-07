//! Issue #30: the narrow, monotonic access-count update.

use super::*;

// -----------------------------------------------------------------------
// Issue #30 — the narrow, monotonic access update
// -----------------------------------------------------------------------

fn concept_with_access(m: Mutation, count: i32, at: Option<DateTime<Utc>>) -> Mutation {
    let Mutation::UpsertNode {
        node: NodeKind::Concept(mut c),
    } = m
    else {
        panic!("a concept upsert")
    };
    c.access_count = count;
    c.last_accessed = at;
    Mutation::UpsertNode {
        node: NodeKind::Concept(c),
    }
}

fn record_access(sid: &SessionId, id: NodeId, count: i32, at: DateTime<Utc>) -> Mutation {
    Mutation::RecordAccess {
        session_id: sid.clone(),
        id,
        access_count: count,
        last_accessed: at,
    }
}

fn stored_access(snap: &GraphSnapshot, id: NodeId) -> (i32, Option<DateTime<Utc>>, &Concept) {
    let c = snap
        .concepts
        .iter()
        .find(|c| c.id == id)
        .expect("concept row");
    (c.access_count, c.last_accessed, c)
}

/// The access update writes the two access columns and nothing else, keeps
/// the larger of stored and presented on both (so a replayed or older
/// batch never lowers them), counts only against an existing row of the
/// presenting session, and leaves the mutation watermark to the batch
/// stamp exactly like every other kind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn record_access_is_a_narrow_monotonic_update_of_existing_rows() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let sid = SessionId::from("issue-30-sqlite");
    let (i1, c1) = (NodeId::new(), NodeId::new());
    let t0 = Utc.with_ymd_and_hms(2026, 10, 7, 12, 0, 0).unwrap();
    let t = |s: i64| t0 + chrono::Duration::seconds(s);
    let mut planted = plant_concept(&sid, c1, i1, "user schema", ConceptType::Entity, t0);
    if let Mutation::UpsertNode {
        node: NodeKind::Concept(c),
    } = &mut planted
    {
        c.gc_survived = 2;
    }
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 7,
                gc_mark: Default::default(),
                mutations: vec![plant_interaction(&sid, i1, None, t0), planted],
            },
            None,
        )
        .await
        .unwrap();

    let accesses = MutationBatch {
        mutation_epoch: 7,
        gc_mark: Default::default(),
        mutations: vec![record_access(&sid, c1, 5, t(20))],
    };
    store.flush(&accesses, None).await.unwrap();
    let snap = store.load_session(&sid).await.unwrap();
    let (count, last, c) = stored_access(&snap, c1);
    assert_eq!((count, last), (5, Some(t(20))));
    assert_eq!(
        (c.content.as_str(), c.gc_survived, c.created_at),
        ("user schema", 2, t0),
        "no other column moves"
    );
    assert_eq!(snap.mutation_epoch, 7);

    // Replay converges; an older presentation lowers nothing.
    store.flush(&accesses, None).await.unwrap();
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 7,
                gc_mark: Default::default(),
                mutations: vec![record_access(&sid, c1, 3, t(10))],
            },
            None,
        )
        .await
        .unwrap();
    let snap = store.load_session(&sid).await.unwrap();
    assert_eq!(stored_access(&snap, c1).0, 5);
    assert_eq!(stored_access(&snap, c1).1, Some(t(20)));
    // Each column takes its own max: a higher count with an older instant
    // raises the count and keeps the later instant.
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 7,
                gc_mark: Default::default(),
                mutations: vec![record_access(&sid, c1, 9, t(15))],
            },
            None,
        )
        .await
        .unwrap();
    let snap = store.load_session(&sid).await.unwrap();
    assert_eq!(
        (stored_access(&snap, c1).0, stored_access(&snap, c1).1),
        (9, Some(t(20)))
    );

    // A missing row is a no-op, not an error and not an insert; another
    // session's access never counts against this session's row.
    let ghost = NodeId::new();
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 7,
                gc_mark: Default::default(),
                mutations: vec![
                    record_access(&sid, ghost, 4, t(30)),
                    record_access(&SessionId::from("someone-else"), c1, 99, t(40)),
                ],
            },
            None,
        )
        .await
        .unwrap();
    let snap = store.load_session(&sid).await.unwrap();
    assert!(snap.concepts.iter().all(|c| c.id != ghost));
    assert_eq!(
        (stored_access(&snap, c1).0, stored_access(&snap, c1).1),
        (9, Some(t(20)))
    );
}

/// The batch planner's ordering rule against a real engine: an access and
/// a concept upsert for the same concept in one batch leave the higher
/// values whichever came first in the log, and an access to a concept born
/// in the same batch lands on the new row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn record_access_and_a_concept_upsert_in_one_batch_keep_the_later_values() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let sid = SessionId::from("issue-30-order");
    let i1 = NodeId::new();
    let (a, b, born) = (NodeId::new(), NodeId::new(), NodeId::new());
    let t0 = Utc.with_ymd_and_hms(2026, 10, 7, 12, 0, 0).unwrap();
    let t = |s: i64| t0 + chrono::Duration::seconds(s);
    let concept = |id, name: &str| plant_concept(&sid, id, i1, name, ConceptType::Entity, t0);
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 1,
                gc_mark: Default::default(),
                mutations: vec![
                    plant_interaction(&sid, i1, None, t0),
                    concept(a, "a"),
                    concept(b, "b"),
                ],
            },
            None,
        )
        .await
        .unwrap();

    // The graph only raises the two fields, so the later mutation carries
    // the higher values. a: upsert(2) then access(4). b: access(4) then
    // upsert(6). born: inserted, then accessed, in one batch.
    let batch = MutationBatch {
        mutation_epoch: 2,
        gc_mark: Default::default(),
        mutations: vec![
            concept_with_access(concept(a, "a"), 2, Some(t(2))),
            record_access(&sid, a, 4, t(4)),
            record_access(&sid, b, 4, t(4)),
            concept_with_access(concept(b, "b"), 6, Some(t(6))),
            concept(born, "born"),
            record_access(&sid, born, 1, t(8)),
        ],
    };
    store.flush(&batch, None).await.unwrap();
    let snap = store.load_session(&sid).await.unwrap();
    let got = |id| {
        let (n, at, _) = stored_access(&snap, id);
        (n, at)
    };
    assert_eq!(got(a), (4, Some(t(4))), "a later access is not undone");
    assert_eq!(got(b), (6, Some(t(6))), "a later upsert is not undone");
    assert_eq!(got(born), (1, Some(t(8))), "a same-batch birth is counted");
}

/// SQL text: the statement sets the two access columns only, both through
/// `MAX`, joins on id **and** session, and binds four values per row.
#[test]
fn the_access_update_sql_is_narrow_and_monotonic() {
    let sid = SessionId::from("s");
    let at = Utc.with_ymd_and_hms(2026, 10, 7, 12, 0, 0).unwrap();
    let rows = [
        AccessUpdate {
            session_id: &sid,
            id: NodeId::new(),
            access_count: 1,
            last_accessed: at,
        },
        AccessUpdate {
            session_id: &sid,
            id: NodeId::new(),
            access_count: 2,
            last_accessed: at,
        },
    ];
    let sql = update_accesses_query(&rows).sql().to_string();
    let set = &sql[sql.find(" SET ").unwrap()..sql.find(" FROM ").unwrap()];
    assert_eq!(set.matches(" = ").count(), 2, "{sql}");
    assert!(
        set.contains("access_count = MAX(concepts.access_count, v.access_count)"),
        "{sql}"
    );
    assert!(
        set.contains(
            "last_accessed = MAX(COALESCE(concepts.last_accessed, v.last_accessed), v.last_accessed)"
        ),
        "{sql}"
    );
    assert!(
        !sql.contains("embedding") && !sql.contains("INSERT"),
        "{sql}"
    );
    assert!(
        sql.ends_with("WHERE concepts.id = v.id AND concepts.session_id = v.session_id"),
        "{sql}"
    );
    assert_eq!(
        sql.matches('?').count(),
        rows.len() * ACCESS_COLUMNS,
        "{sql}"
    );
}
