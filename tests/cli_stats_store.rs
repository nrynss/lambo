//! #248: End-to-end CLI tests for `lambo stats --store` (and `--json`) on SQLite.
//!
//! Verifies read-only multi-session overview without constructing an embedder
//! or acquiring a writer lease, live lease reporting while a writer holds a
//! session, released/erased session handling, raw vector coverage across text
//! and image concepts, and unprovisioned-store preflight refusal.
#![cfg(all(feature = "store-sqlite", feature = "embed-fixture"))]

use std::time::Duration;

use chrono::{TimeZone, Utc};
use lambo::store::lease::LeaseHolder;
use lambo::store::{GraphStore, SqliteStore};
use lambo::types::{
    AgentId, CanonizationStatus, Concept, ConceptType, Edge, EdgeType, EmbeddingContract,
    EmbeddingSource, Interaction, Mutation, MutationBatch, Node, NodeId, SessionId, SourceModality,
    VectorOrigin,
};

mod common;
use common::ScratchDir;

#[tokio::test]
async fn stats_store_reports_multi_session_health_without_embedder_or_lease() {
    let dir = ScratchDir::new("lambo-cli-stats-store");
    let db_path = dir.join("store.db");
    let cfg_path = dir.join("lambo.toml");

    // Deliberately point `[embedder]` at an unconstructible config (dim = 0)
    // to prove `lambo stats --store` never builds an embedder.
    let toml = format!(
        "[store]\nkind = \"sqlite\"\npath = \"{}\"\n\n[embedder]\nkind = \"fixture\"\ndim = 0\n",
        db_path.to_str().unwrap()
    );
    std::fs::write(&cfg_path, toml).unwrap();

    // Unprovisioned / nonexistent store fails cleanly without creating a file on disk.
    let pre = common::lambo_command()
        .args(["--config", cfg_path.to_str().unwrap(), "stats", "--store"])
        .output()
        .unwrap();
    assert!(!pre.status.success(), "nonexistent store must refuse");
    let pre_stderr = String::from_utf8_lossy(&pre.stderr);
    assert!(
        pre_stderr.contains("does not exist") && pre_stderr.contains("lambo provision"),
        "expected helpful missing-file error, got: {pre_stderr}"
    );
    assert!(
        !db_path.exists(),
        "lambo stats --store must not create a nonexistent SQLite file"
    );

    // Provision and seed two active sessions + one erased session.
    let store = SqliteStore::connect(db_path.to_str().unwrap())
        .unwrap()
        .with_vector_dim(4)
        .unwrap();
    store.init_schema().await.unwrap();

    let t1 = Utc.timestamp_opt(1_700_000_010, 0).unwrap();
    let t2 = Utc.timestamp_opt(1_700_000_080, 0).unwrap();

    let sid_a = SessionId::from("alpha");
    let sid_b = SessionId::from("beta");
    let sid_gone = SessionId::from("erased");

    let ia = NodeId::new();
    let c1 = NodeId::new();
    let c2 = NodeId::new();
    store
        .flush(
            &MutationBatch {
                mutations: vec![
                    Mutation::SetEmbedding {
                        session_id: sid_a.clone(),
                        embedding: Some(EmbeddingContract {
                            kind: "fixture".into(),
                            model: None,
                            dim: 4,
                        }),
                    },
                    Mutation::UpsertNode {
                        node: Node::Interaction(Interaction {
                            id: ia,
                            session_id: sid_a.clone(),
                            agent_id: AgentId::from("agent-a"),
                            prompt_text: None,
                            previous_id: None,
                            created_at: t1,
                            event_time: None,
                        }),
                    },
                    Mutation::UpsertNode {
                        node: Node::Concept(Concept {
                            id: c1,
                            session_id: sid_a.clone(),
                            content: "embedded fact".into(),
                            canonical_key: "embedded fact".into(),
                            concept_type: ConceptType::Logic,
                            origin_interaction: ia,
                            origin_agent: AgentId::from("agent-a"),
                            created_at: t1,
                            access_count: 0,
                            last_accessed: None,
                            gc_survived: 0,
                            canonization_status: CanonizationStatus::Canonical,
                            blast_radius: None,
                            last_demotion_time: None,
                            embedding: Some(vec![1.0, 0.0, 0.0, 0.0]),
                            human_confirmed: 0,
                            embedding_source: None,
                            chunk_group_id: None,
                        }),
                    },
                    Mutation::UpsertNode {
                        node: Node::Concept(Concept {
                            id: c2,
                            session_id: sid_a.clone(),
                            content: "unembedded image".into(),
                            canonical_key: "unembedded image".into(),
                            concept_type: ConceptType::Resource,
                            origin_interaction: ia,
                            origin_agent: AgentId::from("agent-a"),
                            created_at: t1,
                            access_count: 0,
                            last_accessed: None,
                            gc_survived: 0,
                            canonization_status: CanonizationStatus::None,
                            blast_radius: None,
                            last_demotion_time: None,
                            embedding: None,
                            human_confirmed: 0,
                            embedding_source: Some(EmbeddingSource {
                                modality: SourceModality::Image,
                                origin: VectorOrigin::Client,
                                sha256: None,
                                mime: None,
                            }),
                            chunk_group_id: None,
                        }),
                    },
                    Mutation::UpsertEdge {
                        edge: Edge {
                            id: NodeId::new(),
                            session_id: sid_a.clone(),
                            source: ia,
                            target: c1,
                            edge_type: EdgeType::Derives,
                            weight: 1.0,
                            reinforcements: 1,
                            created_at: t1,
                            last_reinforced: t2,
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

    let ib = NodeId::new();
    store
        .flush(
            &MutationBatch {
                mutations: vec![Mutation::UpsertNode {
                    node: Node::Interaction(Interaction {
                        id: ib,
                        session_id: sid_b.clone(),
                        agent_id: AgentId::from("agent-b"),
                        prompt_text: None,
                        previous_id: None,
                        created_at: t1,
                        event_time: None,
                    }),
                }],
                mutation_epoch: 1,
                gc_mark: Default::default(),
            },
            None,
        )
        .await
        .unwrap();

    let holder_a = LeaseHolder {
        agent: AgentId::from("live-serve"),
        pid: 4242,
        host: "rig-host".into(),
        endpoint: None,
    };
    store
        .acquire_lease(&sid_a, &holder_a, Duration::from_secs(60))
        .await
        .unwrap();
    let eraser = LeaseHolder {
        agent: AgentId::from("eraser"),
        pid: 4243,
        host: "rig-host".into(),
        endpoint: None,
    };
    store.erase_session(&sid_gone, &eraser).await.unwrap();

    // Text output
    let out = common::lambo_command()
        .args(["--config", cfg_path.to_str().unwrap(), "stats", "--store"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stats --store failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("store health (2 sessions, read-only)"),
        "{stdout}"
    );
    assert!(stdout.contains("session 'alpha'"), "{stdout}");
    assert!(
        stdout.contains("concepts=2 edges=1 embedded=1/2"),
        "{stdout}"
    );
    assert!(
        stdout.contains("canonization: none=1 candidate=0 venerable=0 canonical=1"),
        "{stdout}"
    );
    assert!(stdout.contains(&holder_a.token()), "{stdout}");
    assert!(stdout.contains("session 'beta'"), "{stdout}");
    assert!(stdout.contains("lease: no writer"), "{stdout}");
    assert!(!stdout.contains("erased"), "{stdout}");

    // JSON output
    let out_json = common::lambo_command()
        .args([
            "--config",
            cfg_path.to_str().unwrap(),
            "stats",
            "--store",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        out_json.status.success(),
        "stats --store --json failed: {}",
        String::from_utf8_lossy(&out_json.stderr)
    );
    let parsed: serde_json::Value = serde_json::from_slice(&out_json.stdout).unwrap();
    assert_eq!(parsed["session_count"], 2);
    assert_eq!(parsed["sessions"][0]["session_id"], "alpha");
    assert_eq!(parsed["sessions"][0]["concept_count"], 2);
    assert_eq!(parsed["sessions"][0]["edge_count"], 1);
    assert_eq!(parsed["sessions"][0]["embedded_count"], 1);
    assert_eq!(parsed["sessions"][0]["lease"]["holder"], holder_a.token());
    assert_eq!(parsed["sessions"][1]["session_id"], "beta");
    assert!(parsed["sessions"][1]["lease"].is_null());

    // Confirm the active lease on `alpha` was untouched and `beta` acquired none.
    let lease_after_a = store.read_lease(&sid_a).await.unwrap().unwrap();
    assert_eq!(lease_after_a.holder, holder_a.token());
    assert!(store.read_lease(&sid_b).await.unwrap().is_none());
}

#[test]
fn stats_store_refuses_preexisting_empty_file_without_wal_mutation() {
    let dir = ScratchDir::new("lambo-cli-stats-store-empty");
    let db_path = dir.join("empty.db");
    let cfg_path = dir.join("lambo.toml");
    std::fs::File::create(&db_path).unwrap();

    let toml = format!(
        "[store]\nkind = \"sqlite\"\npath = \"{}\"\n\n[embedder]\nkind = \"fixture\"\ndim = 0\n",
        db_path.to_str().unwrap()
    );
    std::fs::write(&cfg_path, toml).unwrap();

    let out = common::lambo_command()
        .args(["--config", cfg_path.to_str().unwrap(), "stats", "--store"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "pre-existing empty unprovisioned file must be refused"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no schema yet") && stderr.contains("lambo provision"),
        "expected preflight refusal on empty file, got: {stderr}"
    );
    assert_eq!(
        std::fs::metadata(&db_path).unwrap().len(),
        0,
        "lambo stats --store must not mutate a 0-byte unprovisioned file into WAL mode"
    );
    assert!(!dir.join("empty.db-wal").exists());
    assert!(!dir.join("empty.db-shm").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn stats_store_reads_chmod_0444_db_without_mutation() {
    use std::os::unix::fs::PermissionsExt;
    use std::str::FromStr;

    // Root ignores 0444 write restrictions.
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

    let dir = ScratchDir::new("lambo-cli-stats-store-0444");
    let db_path = dir.join("readonly.db");
    let seed_cfg_path = dir.join("lambo-seed.toml");
    let ro_cfg_path = dir.join("lambo-ro.toml");
    std::fs::write(
        &seed_cfg_path,
        format!(
            "[store]\nkind = \"sqlite\"\npath = \"{}\"\n\n[embedder]\nkind = \"fixture\"\ndim = 1024\n",
            db_path.to_str().unwrap()
        ),
    )
    .unwrap();
    // Deliberately use dim = 0 on the stats --store config to prove it never builds an embedder.
    std::fs::write(
        &ro_cfg_path,
        format!(
            "[store]\nkind = \"sqlite\"\npath = \"{}\"\n\n[embedder]\nkind = \"fixture\"\ndim = 0\n",
            db_path.to_str().unwrap()
        ),
    )
    .unwrap();

    // Provision and seed via separate CLI subprocesses so no writer handle remains open.
    let prov = common::lambo_command()
        .args(["--config", seed_cfg_path.to_str().unwrap(), "provision"])
        .output()
        .unwrap();
    assert!(
        prov.status.success(),
        "provision failed: {}",
        String::from_utf8_lossy(&prov.stderr)
    );
    let derived = common::lambo_command()
        .args([
            "--config",
            seed_cfg_path.to_str().unwrap(),
            "derive",
            "--session",
            "ro-alpha",
            "--agent",
            "agent-a",
            "--content",
            "read-only fact",
            "--kind",
            "logic",
        ])
        .output()
        .unwrap();
    assert!(
        derived.status.success(),
        "derive failed: {}",
        String::from_utf8_lossy(&derived.stderr)
    );

    // Checkpoint WAL and close cleanly so no writer or sidecar holds locks before chmod 0444.
    let opts = sqlx::sqlite::SqliteConnectOptions::from_str(db_path.to_str().unwrap()).unwrap();
    let pool = sqlx::SqlitePool::connect_with(opts).await.unwrap();
    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let bytes_before = std::fs::read(&db_path).unwrap();
    std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o444)).unwrap();
    let _restore = RestoreMode(db_path.clone());

    let out = common::lambo_command()
        .args([
            "--config",
            ro_cfg_path.to_str().unwrap(),
            "stats",
            "--store",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stats --store on chmod 0444 DB must succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("session 'ro-alpha'"), "{stdout}");
    assert!(
        stdout.contains("concepts=1 edges=1 embedded=1/1"),
        "{stdout}"
    );
    assert!(
        stdout.contains("canonization: none=1 candidate=0 venerable=0 canonical=0"),
        "{stdout}"
    );
    assert!(stdout.contains("lease: no writer"), "{stdout}");
    assert_eq!(
        std::fs::read(&db_path).unwrap(),
        bytes_before,
        "lambo stats --store must not mutate a read-only database file"
    );
}

#[test]
fn session_health_and_active_lease_constructors_work_from_external_crate() {
    let t1 = Utc.timestamp_opt(1_700_000_010, 0).unwrap();
    let t2 = Utc.timestamp_opt(1_700_000_070, 0).unwrap();
    let lease = lambo::ActiveSessionLease::new("agent@host#1".into(), t1, t2);
    assert_eq!(lease.holder, "agent@host#1");
    assert_eq!(lease.acquired_at, t1);
    assert_eq!(lease.expires_at, t2);

    let counts = lambo::CanonizationCounts {
        none: 2,
        candidate: 1,
        venerable: 1,
        canonical: 3,
    };
    let health = lambo::SessionHealth::new(
        SessionId::from("ext-session"),
        7,
        4,
        5,
        counts,
        Some(lease.clone()),
        Some(t2),
    );
    assert_eq!(health.session_id, SessionId::from("ext-session"));
    assert_eq!(health.concept_count, 7);
    assert_eq!(health.edge_count, 4);
    assert_eq!(health.embedded_count, 5);
    assert_eq!(health.canonization, counts);
    assert_eq!(health.lease, Some(lease));
    assert_eq!(health.last_write_at, Some(t2));
}
