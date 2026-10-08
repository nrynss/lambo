//! Single-writer leases for the SQLite adapter (T8.6, J2, J4): the atomic
//! acquire/refresh upsert, lease reads and holder-scoped release, and the
//! lease-refusal log with its lazy retention purge. Every timestamp comes from
//! SQLite's own `strftime(...,'now')`, never from a caller (F18).
//!
//! The fencing token these rows carry is *checked* elsewhere, inside each write
//! transaction (`persistence.rs`); this module only issues and reports it.

use chrono::{DateTime, Utc};
use sqlx::SqlitePool;
use std::time::Duration;

use super::codec::{db_err, text_to_ts, ts_to_text};
use super::SqliteStore;
use crate::store::lease::{LeaseHolder, LeaseInfo, LeaseOutcome};
use crate::types::{SessionId, StoreError};

/// Atomic single-writer lease acquire / refresh (T8.6).
///
/// ONE statement — `INSERT ... ON CONFLICT DO UPDATE ... WHERE ... RETURNING` —
/// so the decision is made under SQLite's write lock with no read-then-write
/// race. The update fires only when the existing lease is expired or is already
/// ours; on a refresh we keep the original `acquired_at`. All timestamps come
/// from SQLite's own `strftime(...,'now')` — never a caller argument (F18).
///
/// * A returned row whose holder is ours ⇒ [`LeaseOutcome::Acquired`] (fresh
///   insert, expired steal, or our refresh).
/// * An empty RETURNING ⇒ the guard was false: a live lease is held by someone
///   else. We read it back to report the holder + age ([`LeaseOutcome::Held`]).
///   If the row vanished in between (released concurrently) we retry a bounded
///   number of times.
pub(super) async fn acquire_or_refresh(
    pool: &SqlitePool,
    session: &SessionId,
    holder: &LeaseHolder,
    ttl: Duration,
) -> Result<LeaseOutcome, StoreError> {
    // Fractional seconds keep sub-second TTLs (tests) honest; whole seconds for
    // the production 45s. Bound as a strftime modifier, e.g. "+45 seconds".
    let ttl_modifier = format!("+{} seconds", ttl.as_secs_f64());
    let token = holder.token();
    const ACQUIRE_SQL: &str = "\
        INSERT INTO session_leases \
            (session_id, holder, acquired_at, expires_at, current_token, endpoint) \
        VALUES (?1, ?2, \
                strftime('%Y-%m-%dT%H:%M:%fZ','now'), \
                strftime('%Y-%m-%dT%H:%M:%fZ','now', ?3), \
                1, ?4) \
        ON CONFLICT (session_id) DO UPDATE SET \
            holder = excluded.holder, \
            acquired_at = CASE WHEN session_leases.holder = excluded.holder \
                               THEN session_leases.acquired_at ELSE excluded.acquired_at END, \
            expires_at = excluded.expires_at, \
            current_token = CASE WHEN session_leases.holder = excluded.holder \
                                 THEN session_leases.current_token \
                                 ELSE session_leases.current_token + 1 END, \
            endpoint = excluded.endpoint \
        WHERE session_leases.expires_at <= strftime('%Y-%m-%dT%H:%M:%fZ','now') \
           OR session_leases.holder = excluded.holder \
        RETURNING holder, acquired_at, expires_at, current_token, endpoint";

    for _ in 0..3 {
        let won: Option<LeaseRowText> = sqlx::query_as(ACQUIRE_SQL)
            .bind(&session.0)
            .bind(&token)
            .bind(&ttl_modifier)
            .bind(holder.endpoint.as_deref())
            .fetch_optional(pool)
            .await
            .map_err(|e| db_err("acquire lease", e))?;
        if let Some(row) = won {
            return Ok(LeaseOutcome::Acquired(lease_info_from_text(row)?));
        }
        // Guard was false — someone else holds a live lease. Read it back.
        let current: Option<LeaseRowText> = sqlx::query_as(LEASE_ROW_SQL)
            .bind(&session.0)
            .fetch_optional(pool)
            .await
            .map_err(|e| db_err("read current lease", e))?;
        match current {
            Some(row) => {
                let info = lease_info_from_text(row)?;
                let age = (Utc::now() - info.acquired_at)
                    .to_std()
                    .unwrap_or(Duration::ZERO);
                return Ok(LeaseOutcome::Held { current: info, age });
            }
            // Released between our upsert and this read: retry the acquire.
            None => continue,
        }
    }
    Err(StoreError::Backend(
        "acquire lease: contended row kept changing under us (retries exhausted)".into(),
    ))
}

/// The lease row as SQLite hands it back — see [`LEASE_ROW_SQL`] for the
/// column order this tuple mirrors. Named so the acquire's `RETURNING` and the
/// standalone read cannot drift apart in shape (J2 added a sixth column and the
/// two lists were already duplicated).
pub(super) type LeaseRowText = (String, String, String, i64, Option<String>);

/// Every column [`LeaseInfo`] needs, in [`LeaseRowText`] order.
pub(super) const LEASE_ROW_SQL: &str = "\
    SELECT holder, acquired_at, expires_at, current_token, endpoint \
    FROM session_leases WHERE session_id = ?1";

pub(super) fn lease_info_from_text(row: LeaseRowText) -> Result<LeaseInfo, StoreError> {
    let (holder, acquired_at, expires_at, current_token, endpoint) = row;
    Ok(LeaseInfo {
        holder,
        token: u64::try_from(current_token)
            .map_err(|_| StoreError::Backend("lease row has a negative current_token".into()))?,
        acquired_at: text_to_ts(&acquired_at)?,
        expires_at: text_to_ts(&expires_at)?,
        endpoint,
    })
}

impl SqliteStore {
    pub(super) async fn read_lease_row(
        &self,
        session: &SessionId,
    ) -> Result<Option<LeaseInfo>, StoreError> {
        let row: Option<LeaseRowText> = sqlx::query_as(LEASE_ROW_SQL)
            .bind(&session.0)
            .fetch_optional(self.pool())
            .await
            .map_err(|e| db_err("read lease", e))?;
        row.map(lease_info_from_text).transpose()
    }

    pub(super) async fn expire_lease_row(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
    ) -> Result<(), StoreError> {
        // Holder-scoped: only our own row (a stale release after our lease was
        // stolen must not evict the new holder). An UPDATE, not a DELETE: the
        // row keeps `current_token`, so the next acquire mints above it (#23
        // review H2; see `store::lease`).
        sqlx::query(
            "UPDATE session_leases SET holder = ?3, \
                 expires_at = strftime('%Y-%m-%dT%H:%M:%fZ','now'), endpoint = NULL \
             WHERE session_id = ?1 AND holder = ?2",
        )
        .bind(&session.0)
        .bind(holder.token())
        .bind(crate::store::lease::RELEASED_HOLDER)
        .execute(self.pool())
        .await
        .map_err(|e| db_err("release lease", e))?;
        Ok(())
    }

    // J4. Record a lease refusal against this session at the store's clock,
    // then purge this session's refusals older than
    // `LEASE_REFUSAL_RETENTION` (JE2E-1).
    //
    // The purge is **lazy, on the write path**, mirroring `write_intents`'
    // `consume_write_intent`: the retention sweep rides the one statement that
    // was going to touch this table anyway, so no adapter grows a clock, no
    // task grows a timer, and a session nobody contends keeps its rows (which
    // is free — nothing is being appended to sweep them out of the way of).
    // The cutoff is the store's own clock, the same `strftime` that stamps the
    // row, so the comparison is between two store instants and never a
    // caller's (F18).
    pub(super) async fn insert_lease_refusal(
        &self,
        session: &SessionId,
        refused_by: &str,
        current_holder: &str,
    ) -> Result<(), StoreError> {
        //
        // #23: never against an erased session's tombstone. A refusal row is
        // keyed to the session, and the erase removed them all; a writer
        // turned away by the tombstone must not start a new set.
        sqlx::query(
            "INSERT INTO lease_refusals \
                 (session_id, refused_at, refused_by, current_holder) \
             SELECT ?1, strftime('%Y-%m-%dT%H:%M:%fZ','now'), ?2, ?3 \
             WHERE NOT EXISTS (SELECT 1 FROM session_leases \
                               WHERE session_id = ?1 AND holder = ?4)",
        )
        .bind(&session.0)
        .bind(refused_by)
        .bind(current_holder)
        .bind(crate::store::erase::ERASED_HOLDER)
        .execute(self.pool())
        .await
        .map_err(|e| db_err("record lease refusal", e))?;
        sqlx::query(
            "DELETE FROM lease_refusals \
             WHERE session_id = ?1 \
               AND refused_at < strftime('%Y-%m-%dT%H:%M:%fZ','now',?2)",
        )
        .bind(&session.0)
        .bind(format!(
            "-{} seconds",
            crate::store::lease::LEASE_REFUSAL_RETENTION.as_secs()
        ))
        .execute(self.pool())
        .await
        .map_err(|e| db_err("purge expired lease refusals", e))?;
        Ok(())
    }

    // J4. Refusals recorded against this session at/after `since`, newest first.
    pub(super) async fn select_lease_refusals(
        &self,
        session: &SessionId,
        since: DateTime<Utc>,
    ) -> Result<Vec<crate::store::lease::LeaseRefusal>, StoreError> {
        type Row = (String, String, String); // refused_at, refused_by, current_holder
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT refused_at, refused_by, current_holder FROM lease_refusals \
             WHERE session_id = ?1 AND refused_at >= ?2 ORDER BY refused_at DESC",
        )
        .bind(&session.0)
        .bind(ts_to_text(since))
        .fetch_all(self.pool())
        .await
        .map_err(|e| db_err("pending lease refusals", e))?;
        rows.into_iter()
            .map(|(refused_at, refused_by, current_holder)| {
                Ok(crate::store::lease::LeaseRefusal {
                    session: session.clone(),
                    at: text_to_ts(&refused_at)?,
                    refused_by,
                    current_holder,
                })
            })
            .collect()
    }
}
