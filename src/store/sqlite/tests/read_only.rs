//! Classification opens a finished backup with SQLite's read-only flags.

use super::*;

#[tokio::test]
async fn completed_backup_read_creates_no_sidecars_and_cannot_write() {
    let dir = std::env::temp_dir().join(format!("lambo-readonly-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("source.db");
    let writer = SqliteStore::connect(path.to_str().unwrap()).unwrap();
    writer.init_schema().await.unwrap();
    writer
        .flush(&crate::test_util::dependents::store_batch(), None)
        .await
        .unwrap();
    writer.pool().close().await;
    let before = std::fs::read(&path).unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&path, permissions).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let reader = SqliteStore::connect_read_only(&path).unwrap();
    reader.preflight_schema().await.unwrap();
    assert!(!reader
        .load_session(&SessionId::from("deps"))
        .await
        .unwrap()
        .concepts
        .is_empty());
    assert!(reader
        .flush(&crate::test_util::dependents::store_batch(), None)
        .await
        .is_err());
    reader.pool().close().await;
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(!dir.join("source.db-wal").exists());
    assert!(!dir.join("source.db-shm").exists());
}

#[tokio::test]
async fn nonempty_wal_missing_input_and_old_schema_refuse_without_repair() {
    let dir = std::env::temp_dir().join(format!("lambo-readonly-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("source.db");
    assert!(SqliteStore::connect_read_only(&path).is_err());
    assert!(!path.exists());
    let writer = SqliteStore::connect(path.to_str().unwrap()).unwrap();
    sqlx::query("CREATE TABLE sessions(session_id TEXT PRIMARY KEY)")
        .execute(writer.pool())
        .await
        .unwrap();
    writer.pool().close().await;
    let before = std::fs::read(&path).unwrap();
    std::fs::write(dir.join("source.db-wal"), b"not a finished backup").unwrap();
    assert!(SqliteStore::connect_read_only(&path).is_err());
    std::fs::remove_file(dir.join("source.db-wal")).unwrap();
    let reader = SqliteStore::connect_read_only(&path).unwrap();
    assert!(reader.preflight_schema().await.is_err());
    reader.pool().close().await;
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[cfg(feature = "fixtures")]
#[tokio::test]
async fn session_metadata_roundtrips_and_invalidates_classification_binding() {
    let store = test_store();
    store.init_schema().await.unwrap();
    let (mut source, rules) = crate::session::tests::fixture();
    let at = source.interactions[0].created_at;
    source.created_at = Some(at);
    source.closed_at = Some(at + chrono::Duration::days(1));
    store.seed(&source).await.unwrap();
    let loaded = store.load_session(&source.session_id).await.unwrap();
    assert_eq!(loaded.created_at, source.created_at);
    assert_eq!(loaded.closed_at, source.closed_at);
    let plan = crate::session::classify(&loaded, &rules, &[]).unwrap();
    sqlx::query("UPDATE sessions SET closed_at = ? WHERE session_id = ?")
        .bind((at + chrono::Duration::days(2)).to_rfc3339())
        .bind(&source.session_id.0)
        .execute(store.pool())
        .await
        .unwrap();
    let changed = store.load_session(&source.session_id).await.unwrap();
    assert!(crate::session::validate_plan(&plan, &changed).is_err());
}
