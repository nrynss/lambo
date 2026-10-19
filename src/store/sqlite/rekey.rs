//! #148 on SQLite: read, plan ([`crate::store::rekey::plan_rekey`]) and write
//! the re-key in one `BEGIN IMMEDIATE` transaction.

use std::collections::HashMap;

use sqlx::SqliteConnection;

use super::codec::db_err;
use super::SqliteStore;
use crate::store::rekey::{
    placeholder_key, plan_rekey, KeyRow, RekeyPlan, REKEY_MARKER_KEY, REKEY_MARKER_VALUE,
};
use crate::types::StoreError;

impl SqliteStore {
    pub(super) async fn rekey_keys(&self, force: bool) -> Result<RekeyPlan, StoreError> {
        let mut conn = self
            .pool()
            .acquire()
            .await
            .map_err(|e| db_err("rekey: acquire", e))?;
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *conn)
            .await
            .map_err(|e| db_err("rekey: begin", e))?;
        let outcome = rekey_in(&mut conn, force).await;
        let end = if outcome.is_ok() {
            "COMMIT"
        } else {
            "ROLLBACK"
        };
        let ended = sqlx::query(end).execute(&mut *conn).await;
        let plan = outcome?;
        ended.map_err(|e| db_err("rekey: commit", e))?;
        Ok(plan)
    }
}

async fn rekey_in(conn: &mut SqliteConnection, force: bool) -> Result<RekeyPlan, StoreError> {
    // #171: read under the same `BEGIN IMMEDIATE` as the re-key, so two
    // provisions cannot both see "not done" and both scan.
    if !force {
        let marker: Option<String> =
            sqlx::query_scalar("SELECT value FROM store_meta WHERE key = ?")
                .bind(REKEY_MARKER_KEY)
                .fetch_optional(&mut *conn)
                .await
                .map_err(|e| db_err("rekey: read store_meta marker", e))?;
        if marker.as_deref() == Some(REKEY_MARKER_VALUE) {
            return Ok(RekeyPlan {
                skipped: true,
                ..RekeyPlan::default()
            });
        }
    }
    let plan = plan_and_apply(&mut *conn).await?;
    record_marker(&mut *conn, &plan).await?;
    Ok(plan)
}

/// Record the clean re-key, or withdraw a stale record when this pass left a
/// collision (#171, [`REKEY_MARKER_KEY`]).
async fn record_marker(conn: &mut SqliteConnection, plan: &RekeyPlan) -> Result<(), StoreError> {
    let query = if plan.collisions.is_empty() {
        sqlx::query(
            "INSERT INTO store_meta (key, value) VALUES (?, ?) \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        )
        .bind(REKEY_MARKER_KEY)
        .bind(REKEY_MARKER_VALUE)
    } else {
        sqlx::query("DELETE FROM store_meta WHERE key = ?").bind(REKEY_MARKER_KEY)
    };
    query
        .execute(&mut *conn)
        .await
        .map_err(|e| db_err("rekey: record store_meta marker", e))?;
    Ok(())
}

async fn plan_and_apply(conn: &mut SqliteConnection) -> Result<RekeyPlan, StoreError> {
    let rows: Vec<(String, String, String, String, String)> = sqlx::query_as(
        "SELECT id, session_id, content, canonical_key, concept_type FROM concepts ORDER BY id",
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| db_err("rekey: read concepts", e))?;
    let rows: Vec<KeyRow> = rows
        .into_iter()
        .map(|(id, session_id, content, canonical_key, ty)| KeyRow {
            id,
            session_id,
            content,
            canonical_key,
            observation: ty == "Observation",
        })
        .collect();
    let synonyms: Vec<(String, String, String)> =
        sqlx::query_as("SELECT session_id, source_key, canonical_key FROM synonyms")
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| db_err("rekey: read synonyms", e))?;
    let synonyms: HashMap<(String, String), String> =
        synonyms.into_iter().map(|(s, k, v)| ((s, k), v)).collect();
    let plan = plan_rekey(&rows, &synonyms);
    if plan.changes.is_empty() {
        return Ok(plan);
    }
    let live: Vec<(String, String)> = sqlx::query_as(
        "SELECT session_id, holder FROM session_leases \
         WHERE expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now')",
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| db_err("rekey: read live leases", e))?;
    if let Some((session, holder)) = crate::store::rekey::first_live_holder(&live) {
        return Err(crate::store::rekey::live_lease_err(
            session,
            holder,
            plan.changes.len(),
        ));
    }
    for last in [false, true] {
        for change in &plan.changes {
            let (from, to) = if last {
                (placeholder_key(&change.id), change.new_key.clone())
            } else {
                (change.old_key.clone(), placeholder_key(&change.id))
            };
            let done = sqlx::query(
                "UPDATE concepts SET canonical_key = ? WHERE id = ? AND canonical_key = ?",
            )
            .bind(&to)
            .bind(&change.id)
            .bind(&from)
            .execute(&mut *conn)
            .await
            .map_err(|e| db_err("rekey: update key", e))?;
            if done.rows_affected() != 1 {
                return Err(StoreError::Backend(format!(
                    "rekey: concept {} changed under the re-key",
                    change.id
                )));
            }
        }
    }
    Ok(plan)
}
