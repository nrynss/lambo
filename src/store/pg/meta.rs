//! #176: the store-wide `store_meta` key/value rows other features keep
//! (the embedder canary fingerprint), read and written outside any session.

use super::codec::backend;
use super::dialect::Dialect;
use super::PgStore;
use crate::types::StoreError;

impl<D: Dialect> PgStore<D> {
    pub(super) async fn read_meta(&self, key: &str) -> Result<Option<String>, StoreError> {
        let pool = self.pool().await?;
        sqlx::query_scalar("SELECT value FROM store_meta WHERE key = $1")
            .bind(key)
            .fetch_optional(&pool)
            .await
            .map_err(backend)
    }

    pub(super) async fn write_meta(&self, key: &str, value: &str) -> Result<(), StoreError> {
        let pool = self.pool().await?;
        sqlx::query(
            "INSERT INTO store_meta (key, value) VALUES ($1, $2) \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        )
        .bind(key)
        .bind(value)
        .execute(&pool)
        .await
        .map_err(backend)?;
        Ok(())
    }
}
