//! Connection setup: file-backed and in-memory databases, WAL, the
//! registry, and concurrent flushes.

use super::*;

#[tokio::test]
async fn concurrent_flushes_across_sessions_do_not_fail() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let store = std::sync::Arc::new(store);

    let mut handles = Vec::new();
    for n in 0..8 {
        let s = std::sync::Arc::clone(&store);
        handles.push(tokio::spawn(async move {
            let sid = SessionId::from(format!("c{n}"));
            let i1 = NodeId::new();
            let c1 = NodeId::new();
            let ts = Utc::now();
            s.flush(
                &MutationBatch {
                    mutation_epoch: 0,
                    gc_mark: Default::default(),
                    mutations: vec![
                        plant_interaction(&sid, i1, None, ts),
                        plant_concept(&sid, c1, i1, &format!("n{n}"), ConceptType::Entity, ts),
                    ],
                },
                None,
            )
            .await
            .unwrap();
            s.load_session(&sid).await.unwrap();
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
}

/// Acceptance: build_store(kind = sqlite) returns a working adapter and
/// is_ready() is true under the feature.
#[tokio::test]
async fn build_store_registry_returns_working_sqlite_adapter() {
    let store = crate::store::build_store(crate::store::StoreConfig {
        kind: crate::store::StoreKind::Sqlite,
        dsn: None,
        path: Some("sqlite::memory:".into()),
        vector_dim: None,
    })
    .unwrap();
    assert!(crate::store::StoreKind::Sqlite.is_ready());
    // F2: the registry hands back a vector-capable adapter that reports a width.
    assert_eq!(store.capabilities(), Capabilities::VECTOR_SEARCH);
    assert_eq!(
        store.vector_dimensions(),
        Some(crate::embed::EmbedderConfig::default().dim),
        "a registry build with no configured width falls back to the `[embedder] dim` \
             default, never a width constant of its own"
    );

    store.init_schema().await.unwrap();
    let sid = SessionId::from("registry");
    let i1 = NodeId::new();
    let c1 = NodeId::new();
    let ts = Utc::now();
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![
                    plant_interaction(&sid, i1, None, ts),
                    plant_concept(&sid, c1, i1, "registry concept", ConceptType::Entity, ts),
                ],
            },
            None,
        )
        .await
        .unwrap();
    let snap = store.load_session(&sid).await.unwrap();
    assert_eq!(snap.concepts.len(), 1);
    assert_eq!(snap.concepts[0].content, "registry concept");
}

/// RAII cleanup for a file-backed test database: removes the db file and
/// any WAL/SHM sidecars on drop (the pool is dropped with the store, so
/// WAL sidecars are normally already checkpointed away).
struct TempDb(std::path::PathBuf);

impl TempDb {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("lambo-sqlite-test-{}.db", uuid::Uuid::new_v4())))
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for sidecar in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", self.0.display(), sidecar));
        }
    }
}

/// CON-1: a fresh file-backed database must bootstrap (create_if_missing)
/// and round-trip a flush → load, surviving a reopen. Pre-fix, connect on
/// a fresh path failed with `(code: 14) unable to open database file`.
#[tokio::test]
async fn file_backed_roundtrip_survives_reopen() {
    let db = TempDb::new();
    let path = db.path().to_str().unwrap();
    {
        let store = SqliteStore::connect(path).unwrap();
        store.init_schema().await.unwrap();
        let sid = SessionId::from("file-backed");
        let i1 = NodeId::new();
        let c1 = NodeId::new();
        let ts = Utc::now();
        store
            .flush(
                &MutationBatch {
                    mutation_epoch: 0,
                    gc_mark: Default::default(),
                    mutations: vec![
                        plant_interaction(&sid, i1, None, ts),
                        plant_concept(&sid, c1, i1, "file-backed concept", ConceptType::Entity, ts),
                    ],
                },
                None,
            )
            .await
            .unwrap();
        let snap = store.load_session(&sid).await.unwrap();
        assert_eq!(snap.concepts.len(), 1);
        assert_eq!(snap.concepts[0].content, "file-backed concept");
    }
    // Reopen from disk: the data must be there (durability, not just
    // in-process memory).
    let store = SqliteStore::connect(path).unwrap();
    store.init_schema().await.unwrap();
    let snap = store
        .load_session(&SessionId::from("file-backed"))
        .await
        .unwrap();
    assert_eq!(snap.concepts.len(), 1);
    assert_eq!(snap.concepts[0].content, "file-backed concept");
    drop(store);
}

/// STORE-9: a file-backed database is opened with WAL journal mode and an
/// 8s busy_timeout (deliberately non-default — sqlx's default is 5s, so
/// the assertion below fails if the `.busy_timeout()` wiring is removed),
/// so a concurrent external reader (spec §2.2) can't turn a flush into a
/// SQLITE_BUSY failure.
#[tokio::test]
async fn file_backed_wal_and_busy_timeout_applied() {
    let db = TempDb::new();
    let store = SqliteStore::connect(db.path().to_str().unwrap()).unwrap();
    store.init_schema().await.unwrap();
    let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(mode, "wal");
    let busy: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert!(busy >= 8_000, "busy_timeout was {busy} ms");
    drop(store);
}

/// STORE-9 guard contract: `is_in_memory_uri` must classify exactly the
/// spellings sqlx's `FromStr` treats as in-memory (database part
/// `:memory:` or a `mode=memory` query param, position-independent) —
/// including exotic spellings the old four-literal guard missed, such as
/// `:memory:?cache=shared` and `mode=memory` in a non-first query slot.
#[test]
fn is_in_memory_uri_matches_sqlx_grammar() {
    for mem in [
        ":memory:",
        ":memory:?cache=shared",
        "sqlite::memory:",
        "sqlite://:memory:",
        "sqlite://?mode=memory",
        "sqlite://db.db?cache=shared&mode=memory",
        "sqlite://db.db?mode=memory&cache=private",
    ] {
        assert!(
            SqliteStore::is_in_memory_uri(mem),
            "sqlx treats {mem:?} as in-memory; the guard must too"
        );
    }
    for file in [
        "db.db",
        "sqlite://db.db",
        "sqlite://db.db?mode=rwc",
        "sqlite://db.db?cache=shared",
        "sqlite://db.db?mode=rw&cache=private",
    ] {
        assert!(
            !SqliteStore::is_in_memory_uri(file),
            "sqlx treats {file:?} as file-backed; the guard must too"
        );
    }
}

/// STORE-9 guard behavior: in-memory databases must NOT receive the
/// file-backed WAL / busy_timeout tuning. Opened via the exotic
/// `:memory:?cache=shared` spelling so a guard regression would route this
/// through the file branch. `PRAGMA journal_mode` alone cannot detect that
/// (WAL on an in-memory DB is a silent no-op — SQLite reports `memory`
/// either way), so also assert busy_timeout stayed at sqlx's 5s default
/// rather than the file branch's 8s; that check fails on a guard miss.
#[tokio::test]
async fn memory_database_does_not_get_wal() {
    let store = SqliteStore::connect(":memory:?cache=shared").unwrap();
    store.init_schema().await.unwrap();
    let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(mode, "memory");
    let busy: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert!(
        busy < 8_000,
        "file-backed tuning leaked into in-memory DB: busy_timeout={busy} ms \
             (sqlx default is 5000, file branch sets 8000)"
    );
}
