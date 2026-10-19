//! #176: the store-wide `store_meta` key/value rows other features keep
//! (the embedder canary fingerprint), read and written outside any session.

use super::codec::db_err;
use super::SqliteStore;
use crate::types::StoreError;

impl SqliteStore {
    pub(super) async fn read_meta(&self, key: &str) -> Result<Option<String>, StoreError> {
        sqlx::query_scalar("SELECT value FROM store_meta WHERE key = ?")
            .bind(key)
            .fetch_optional(self.pool())
            .await
            .map_err(|e| db_err("read store_meta", e))
    }

    pub(super) async fn write_meta(&self, key: &str, value: &str) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO store_meta (key, value) VALUES (?, ?) \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        )
        .bind(key)
        .bind(value)
        .execute(self.pool())
        .await
        .map_err(|e| db_err("write store_meta", e))?;
        Ok(())
    }
}
