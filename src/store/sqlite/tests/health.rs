//! #248: SQLite store-wide health overview (`GraphStore::store_health`).

use super::*;
use crate::store::lease::LeaseHolder;
use crate::store::CanonizationCounts;
use crate::types::{
    CanonizationEvent, EmbeddingSource, Mutation, MutationBatch, SourceModality, VectorOrigin,
};

#[tokio::test]
async fn store_health_empty_and_multi_session_with_leases_coverage_and_erased_omission() {
    let store = vec_test_store(4);
    store.init_schema().await.unwrap();
    assert_eq!(store.store_health().await.unwrap(), vec![]);

    let sid_a = SessionId::from("alpha");
    let sid_b = SessionId::from("beta");
    let sid_leased_empty = SessionId::from("delta-empty-leased");
    let sid_erased = SessionId::from("gamma-erased");

    let t1 = Utc.timestamp_opt(1_700_000_010, 0).unwrap();
    let t2 = Utc.timestamp_opt(1_700_000_020, 0).unwrap();
    let t3 = Utc.timestamp_opt(1_700_000_050, 0).unwrap();
    let t4 = Utc.timestamp_opt(1_700_000_090, 0).unwrap();

    let ia = NodeId::new();
    let c_none = NodeId::new();
    let c_cand = NodeId::new();
    let c_ven = NodeId::new();
    let c_canon = NodeId::new();
    let c_img_unembedded = NodeId::new();
    let edge_id = NodeId::new();

    let make_concept = |id, key: &str, status, embedded, image, ts| Mutation::UpsertNode {
        node: NodeKind::Concept(Concept {
            id,
            session_id: sid_a.clone(),
            content: key.into(),
            canonical_key: key.into(),
            concept_type: ConceptType::Entity,
            origin_interaction: ia,
            origin_agent: AgentId::from("a"),
            created_at: ts,
            access_count: 0,
            last_accessed: None,
            gc_survived: 0,
            canonization_status: status,
            blast_radius: None,
            last_demotion_time: None,
            embedding: if embedded {
                Some(vec![1.0, 0.0, 0.0, 0.0])
            } else {
                None
            },
            human_confirmed: 0,
            embedding_source: if image {
                Some(EmbeddingSource {
                    modality: SourceModality::Image,
                    origin: VectorOrigin::Client,
                    sha256: None,
                    mime: None,
                })
            } else {
                None
            },
            chunk_group_id: None,
        }),
    };

    store
        .flush(
            &MutationBatch {
                mutations: vec![
                    Mutation::SetEmbedding {
                        session_id: sid_a.clone(),
                        embedding: Some(vec_contract(4)),
                    },
                    plant_interaction(&sid_a, ia, None, t1),
                    make_concept(c_none, "k-none", CanonizationStatus::None, true, false, t1),
                    make_concept(
                        c_cand,
                        "k-cand",
                        CanonizationStatus::Candidate,
                        false,
                        false,
                        t2,
                    ),
                    make_concept(
                        c_ven,
                        "k-ven",
                        CanonizationStatus::Venerable,
                        true,
                        true,
                        t2,
                    ),
                    make_concept(
                        c_canon,
                        "k-canon",
                        CanonizationStatus::Canonical,
                        true,
                        false,
                        t2,
                    ),
                    make_concept(
                        c_img_unembedded,
                        "k-img-miss",
                        CanonizationStatus::None,
                        false,
                        true,
                        t2,
                    ),
                    Mutation::UpsertEdge {
                        edge: Edge {
                            id: edge_id,
                            session_id: sid_a.clone(),
                            source: ia,
                            target: c_none,
                            edge_type: EdgeType::Derives,
                            weight: 1.0,
                            reinforcements: 2,
                            created_at: t1,
                            last_reinforced: t3,
                            event_time: None,
                        },
                    },
                ],
                mutation_epoch: 1,
                gc_mark: Default::default(),
            },
            None,
        )
        .await
        .unwrap();

    // A canonization event newer than all concepts/edges advances last_write_at.
    store
        .record_canonization(
            &CanonizationEvent {
                id: NodeId::new(),
                session_id: sid_a.clone(),
                node_id: c_canon,
                from_status: CanonizationStatus::Venerable,
                to_status: CanonizationStatus::Canonical,
                blast_radius: Some(3),
                last_demotion_time: None,
                occurred_at: t4,
                reason: None,
            },
            None,
        )
        .await
        .unwrap();

    // Session B: one interaction + concept, then a released lease -> "no writer".
    let ib = NodeId::new();
    let cb = NodeId::new();
    store
        .flush(
            &MutationBatch {
                mutations: vec![
                    plant_interaction(&sid_b, ib, None, t1),
                    plant_concept(&sid_b, cb, ib, "beta concept", ConceptType::Logic, t2),
                ],
                mutation_epoch: 1,
                gc_mark: Default::default(),
            },
            None,
        )
        .await
        .unwrap();
    let mk_holder = |agent: &str, host: &str, pid: u32| LeaseHolder {
        agent: AgentId::from(agent),
        pid,
        host: host.into(),
        endpoint: None,
    };
    let holder_b = mk_holder("writer-b", "host-b", 200);
    store
        .acquire_lease(&sid_b, &holder_b, Duration::from_secs(60))
        .await
        .unwrap();
    store.release_lease(&sid_b, &holder_b).await.unwrap();

    // Session A gets an active live lease.
    let holder_a = mk_holder("writer-a", "host-a", 100);
    let LeaseOutcome::Acquired(lease_a) = store
        .acquire_lease(&sid_a, &holder_a, Duration::from_secs(60))
        .await
        .unwrap()
    else {
        panic!("acquire alpha lease");
    };

    // Leased-but-unflushed empty session appears with its live lease and zero counts.
    let holder_d = mk_holder("writer-d", "host-d", 300);
    let LeaseOutcome::Acquired(lease_d) = store
        .acquire_lease(&sid_leased_empty, &holder_d, Duration::from_secs(60))
        .await
        .unwrap()
    else {
        panic!("acquire delta lease");
    };

    // Erased session has a tombstone in session_leases and must be omitted.
    let ig = NodeId::new();
    store
        .flush(
            &MutationBatch {
                mutations: vec![plant_interaction(&sid_erased, ig, None, t1)],
                mutation_epoch: 1,
                gc_mark: Default::default(),
            },
            None,
        )
        .await
        .unwrap();
    let eraser = mk_holder("eraser", "host-e", 400);
    store.erase_session(&sid_erased, &eraser).await.unwrap();

    let health = store.store_health().await.unwrap();
    let names: Vec<&str> = health.iter().map(|h| h.session_id.as_str()).collect();
    assert_eq!(names, vec!["alpha", "beta", "delta-empty-leased"]);

    // Alpha: 5 concepts (3 embedded across text + image), 1 edge, all 4 statuses, live lease, t4.
    let a = &health[0];
    assert_eq!(a.concept_count, 5);
    assert_eq!(a.edge_count, 1);
    assert_eq!(a.embedded_count, 3);
    assert_eq!(
        a.canonization,
        CanonizationCounts {
            none: 2,
            candidate: 1,
            venerable: 1,
            canonical: 1,
        }
    );
    let active_a = a.lease.as_ref().expect("alpha has live lease");
    assert_eq!(active_a.holder, holder_a.token());
    assert_eq!(active_a.acquired_at, lease_a.acquired_at);
    assert_eq!(active_a.expires_at, lease_a.expires_at);
    assert_eq!(a.last_write_at, Some(t4));

    // Beta: released lease reads as None ("no writer"), last_write = t2.
    let b = &health[1];
    assert_eq!(b.concept_count, 1);
    assert_eq!(b.edge_count, 0);
    assert_eq!(b.embedded_count, 0);
    assert_eq!(b.canonization.none, 1);
    assert!(b.lease.is_none(), "released lease must report no writer");
    assert_eq!(b.last_write_at, Some(t2));

    // Delta: leased empty session has 0 counts, live lease, no writes yet.
    let d = &health[2];
    assert_eq!(d.concept_count, 0);
    assert_eq!(d.edge_count, 0);
    assert_eq!(d.embedded_count, 0);
    assert_eq!(d.lease.as_ref().unwrap().holder, holder_d.token());
    assert_eq!(d.lease.as_ref().unwrap().expires_at, lease_d.expires_at);
    assert!(d.last_write_at.is_none());

    // Expired lease reports no writer (checked against store clock).
    sqlx::query(
        "UPDATE session_leases SET expires_at = '2020-01-01T00:00:00.000Z' WHERE session_id = ?1",
    )
    .bind("delta-empty-leased")
    .execute(store.pool())
    .await
    .unwrap();
    let health_after_expiry = store.store_health().await.unwrap();
    let d_expired = health_after_expiry
        .iter()
        .find(|h| h.session_id.as_str() == "delta-empty-leased")
        .unwrap();
    assert!(
        d_expired.lease.is_none(),
        "expired lease must report no writer"
    );
}

#[tokio::test]
async fn store_health_fails_closed_on_unknown_canonization_status() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let sid = SessionId::from("corrupt");
    let i = NodeId::new();
    let c = NodeId::new();
    let ts = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
    store
        .flush(
            &MutationBatch {
                mutations: vec![
                    plant_interaction(&sid, i, None, ts),
                    plant_concept(&sid, c, i, "fact", ConceptType::Entity, ts),
                ],
                mutation_epoch: 1,
                gc_mark: Default::default(),
            },
            None,
        )
        .await
        .unwrap();
    sqlx::query("UPDATE concepts SET canonization_status = 'Bogus' WHERE id = ?1")
        .bind(c.0.to_string())
        .execute(store.pool())
        .await
        .unwrap();

    let err = store.store_health().await.unwrap_err().to_string();
    assert!(err.contains("unknown canonization_status"), "{err}");
}

#[tokio::test]
async fn connect_existing_read_only_refuses_missing_and_empty_files_without_mutation() {
    let dir = crate::test_util::ScratchDir::new("lambo-sqlite-health-ro");
    let missing = dir.join("missing.db");

    let err = match SqliteStore::connect_existing_read_only(missing.to_str().unwrap()) {
        Ok(_) => panic!("missing file must fail"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("does not exist"), "{err}");
    assert!(
        !missing.exists(),
        "connect_existing_read_only must never create a missing file"
    );

    // Pre-existing 0-byte unprovisioned file must be refused by preflight without
    // being mutated (e.g., no WAL header written, no sidecars created).
    let empty = dir.join("empty.db");
    std::fs::File::create(&empty).unwrap();
    let ro = SqliteStore::connect_existing_read_only(empty.to_str().unwrap()).unwrap();
    let err = ro.preflight_schema().await.unwrap_err().to_string();
    assert!(err.contains("no schema yet"), "{err}");
    ro.pool().close().await;
    assert_eq!(
        std::fs::metadata(&empty).unwrap().len(),
        0,
        "read-only preflight must not mutate a 0-byte unprovisioned file"
    );
    assert!(!dir.join("empty.db-wal").exists());
    assert!(!dir.join("empty.db-shm").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn connect_existing_read_only_reads_chmod_0444_wal_db_without_mutation() {
    use std::os::unix::fs::PermissionsExt;

    // Root bypasses POSIX 0444 write restrictions.
    // SAFETY: geteuid has no preconditions and cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }

    struct RestoreMode(std::path::PathBuf);
    impl Drop for RestoreMode {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o644));
        }
    }

    let dir = crate::test_util::ScratchDir::new("lambo-sqlite-health-0444");
    let path = dir.join("store.db");
    let sid = SessionId::from("ro-session");
    let ts = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
    let i1 = NodeId::new();
    let c1 = NodeId::new();
    {
        let writer = SqliteStore::connect(path.to_str().unwrap()).unwrap();
        writer.init_schema().await.unwrap();
        writer
            .flush(
                &MutationBatch {
                    mutations: vec![
                        plant_interaction(&sid, i1, None, ts),
                        plant_concept(&sid, c1, i1, "readonly concept", ConceptType::Logic, ts),
                    ],
                    mutation_epoch: 1,
                    gc_mark: Default::default(),
                },
                None,
            )
            .await
            .unwrap();
        writer.pool().close().await;
    }

    let bytes_before = std::fs::read(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
    let _restore = RestoreMode(path.clone());

    let reader = SqliteStore::connect_existing_read_only(path.to_str().unwrap()).unwrap();
    reader.preflight_schema().await.unwrap();
    let health = reader.store_health().await.unwrap();
    assert_eq!(health.len(), 1);
    assert_eq!(health[0].session_id, sid);
    assert_eq!(health[0].concept_count, 1);
    // Write attempt on a read-only handle must fail.
    let write_res = reader
        .flush(
            &MutationBatch {
                mutations: vec![plant_interaction(&sid, NodeId::new(), None, ts)],
                mutation_epoch: 2,
                gc_mark: Default::default(),
            },
            None,
        )
        .await;
    assert!(write_res.is_err());
    reader.pool().close().await;
    assert_eq!(
        std::fs::read(&path).unwrap(),
        bytes_before,
        "read-only inspection must not mutate the database file"
    );
}
