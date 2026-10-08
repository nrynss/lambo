//! Single-writer leases for the Postgres-wire family (T8.6, J2, J4): the
//! atomic acquire/refresh upsert (wrapped in `tx_retry` for serializable
//! conflicts), lease reads and holder-scoped release, and the lease-refusal log
//! with its lazy retention purge. Every timestamp is the cluster's `now()`,
//! never a caller instant (F18).
//!
//! The fencing token these rows carry is *checked* inside each write
//! transaction (`persistence.rs`); this module only issues and reports it.

use chrono::{DateTime, Utc};
use std::time::Duration;

use super::codec::backend;
use super::pool::tx_retry;
use super::{Dialect, PgStore};
use crate::store::lease::{LeaseHolder, LeaseInfo, LeaseOutcome};
use crate::store::map_write_err;
use crate::types::{SessionId, StoreError};

/// The lease row as Cockroach hands it back — the column order
/// [`LEASE_ROW_SQL`] and the acquire's `RETURNING` both use. Named so the two
/// duplicated column lists cannot drift in shape (J2 added a sixth column).
pub(super) type LeaseRowTs = (String, DateTime<Utc>, DateTime<Utc>, i64, Option<String>);

/// Every column [`LeaseInfo`] needs, in [`LeaseRowTs`] order.
pub(super) const LEASE_ROW_SQL: &str = "\
    SELECT holder, acquired_at, expires_at, current_token, endpoint \
    FROM session_leases WHERE session_id = $1";

pub(super) fn lease_info_from_ts(row: LeaseRowTs) -> Result<LeaseInfo, StoreError> {
    let (holder, acquired_at, expires_at, current_token, endpoint) = row;
    Ok(LeaseInfo {
        holder,
        token: u64::try_from(current_token)
            .map_err(|_| StoreError::Backend("lease row has a negative current_token".into()))?,
        acquired_at,
        expires_at,
        endpoint,
    })
}

impl<D: Dialect> PgStore<D> {
    /// Atomic single-writer lease acquire / refresh (T8.6).
    ///
    /// ONE statement — `INSERT ... ON CONFLICT DO UPDATE ... WHERE ... RETURNING`
    /// — so two processes acquiring on the same session serialize under
    /// Cockroach's concurrency control with no read-then-write race. The update
    /// fires only when the current lease is expired or already ours; a refresh
    /// keeps the original `acquired_at`. Every timestamp comes from the cluster's
    /// `now()` (the clock two processes share) — never a caller argument (F18).
    /// `ttl` is a duration multiplied into an INTERVAL, so no client instant is
    /// ever stored.
    ///
    /// An empty RETURNING means the guard was false — a live lease is held by
    /// someone else — so we read it back and report [`LeaseOutcome::Held`] with
    /// the holder and its age. A row released in the gap is retried a bounded
    /// number of times.
    pub(super) async fn acquire_or_refresh_lease(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
        ttl: Duration,
    ) -> Result<LeaseOutcome, StoreError> {
        const ACQUIRE_SQL: &str = "\
            INSERT INTO session_leases \
                (session_id, holder, acquired_at, expires_at, current_token, endpoint) \
            VALUES ($1, $2, now(), now() + ($3 * INTERVAL '1 second'), 1, $4) \
            ON CONFLICT (session_id) DO UPDATE SET \
                holder = excluded.holder, \
                acquired_at = CASE WHEN session_leases.holder = excluded.holder \
                                   THEN session_leases.acquired_at ELSE excluded.acquired_at END, \
                expires_at = excluded.expires_at, \
                current_token = CASE WHEN session_leases.holder = excluded.holder \
                                     THEN session_leases.current_token \
                                     ELSE session_leases.current_token + 1 END, \
                endpoint = excluded.endpoint \
            WHERE session_leases.expires_at <= now() \
               OR session_leases.holder = excluded.holder \
            RETURNING holder, acquired_at, expires_at, current_token, endpoint";
        let pool = &self.pool().await?;
        let token = holder.token();
        let ttl_secs = ttl.as_secs_f64();
        // T86-3: wrap the acquire in `tx_retry`, exactly like every other
        // contended write in this file. sqlx does not auto-retry a SQLSTATE 40001
        // `RETRY_SERIALIZABLE` abort, which `map_write_err` maps to a retryable
        // `StoreError::Backend`; without this wrapper a genuine cross-node acquire
        // conflict surfaced as an opaque `Backend` error (→ `LamboError::Store`)
        // instead of transparently replaying — diverging from the SQLite backend
        // (which absorbs contention via `busy_timeout`) and from the rest of
        // `cockroach.rs`. The inner `for 0..3` still handles the orthogonal
        // vanished-row case (empty RETURNING then empty read-back).
        let session_id = &session.0;
        let token_ref = token.as_str();
        let endpoint_ref = holder.endpoint.as_deref();
        tx_retry(|| async move {
            for _ in 0..3 {
                let won: Option<LeaseRowTs> = sqlx::query_as(ACQUIRE_SQL)
                    .bind(session_id)
                    .bind(token_ref)
                    .bind(ttl_secs)
                    .bind(endpoint_ref)
                    .fetch_optional(pool)
                    .await
                    .map_err(|e| map_write_err(e, |m| format!("acquire lease: {m}")))?;
                if let Some(row) = won {
                    return Ok(LeaseOutcome::Acquired(lease_info_from_ts(row)?));
                }
                let current: Option<LeaseRowTs> = sqlx::query_as(LEASE_ROW_SQL)
                    .bind(session_id)
                    .fetch_optional(pool)
                    .await
                    .map_err(backend)?;
                match current {
                    Some(row) => {
                        let current = lease_info_from_ts(row)?;
                        let age = (Utc::now() - current.acquired_at)
                            .to_std()
                            .unwrap_or(Duration::ZERO);
                        return Ok(LeaseOutcome::Held { current, age });
                    }
                    None => continue,
                }
            }
            Err(StoreError::Backend(
                "acquire lease: contended row kept changing under us (retries exhausted)".into(),
            ))
        })
        .await
    }

    pub(super) async fn read_lease_row(
        &self,
        session: &SessionId,
    ) -> Result<Option<LeaseInfo>, StoreError> {
        let pool = &self.pool().await?;
        let row: Option<LeaseRowTs> = sqlx::query_as(LEASE_ROW_SQL)
            .bind(&session.0)
            .fetch_optional(pool)
            .await
            .map_err(backend)?;
        row.map(lease_info_from_ts).transpose()
    }

    pub(super) async fn delete_lease_row(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
    ) -> Result<(), StoreError> {
        let pool = &self.pool().await?;
        // Holder-scoped so a stale release cannot evict the writer that took
        // over after our lease lapsed.
        sqlx::query("DELETE FROM session_leases WHERE session_id = $1 AND holder = $2")
            .bind(&session.0)
            .bind(holder.token())
            .execute(pool)
            .await
            .map_err(|e| map_write_err(e, |m| format!("release lease: {m}")))?;
        Ok(())
    }

    // J4. Record a lease refusal against this session at the store's clock,
    // then purge this session's refusals older than
    // `LEASE_REFUSAL_RETENTION` (JE2E-1).
    //
    // The purge is **lazy, on the write path**, mirroring `write_intents`'
    // `consume_write_intent`: it rides the one statement that was going to
    // touch this table anyway, so no adapter grows a clock and no task grows a
    // timer. Two statements here rather than one, and each is a round trip on
    // Cockroach — the same cost shape F4 records for the intent consume, and
    // acceptable for the same reason: this path runs once per *refused start*,
    // not once per write.
    pub(super) async fn insert_lease_refusal(
        &self,
        session: &SessionId,
        refused_by: &str,
        current_holder: &str,
    ) -> Result<(), StoreError> {
        let pool = &self.pool().await?;
        // #23: never against an erased session's tombstone (see the SQLite
        // adapter's twin).
        sqlx::query(
            "INSERT INTO lease_refusals (session_id, refused_at, refused_by, current_holder) \
             SELECT $1, now(), $2, $3 \
             WHERE NOT EXISTS (SELECT 1 FROM session_leases \
                               WHERE session_id = $1 AND holder = $4)",
        )
        .bind(&session.0)
        .bind(refused_by)
        .bind(current_holder)
        .bind(crate::store::erase::ERASED_HOLDER)
        .execute(pool)
        .await
        .map_err(|e| map_write_err(e, |m| format!("record lease refusal: {m}")))?;
        sqlx::query(
            "DELETE FROM lease_refusals \
             WHERE session_id = $1 AND refused_at < now() - $2::INTERVAL",
        )
        .bind(&session.0)
        .bind(format!(
            "{} seconds",
            crate::store::lease::LEASE_REFUSAL_RETENTION.as_secs()
        ))
        .execute(pool)
        .await
        .map_err(|e| map_write_err(e, |m| format!("purge expired lease refusals: {m}")))?;
        Ok(())
    }

    // J4. Refusals recorded against this session at/after `since`, newest first.
    pub(super) async fn select_lease_refusals(
        &self,
        session: &SessionId,
        since: DateTime<Utc>,
    ) -> Result<Vec<crate::store::lease::LeaseRefusal>, StoreError> {
        let pool = &self.pool().await?;
        type Row = (DateTime<Utc>, String, String);
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT refused_at, refused_by, current_holder FROM lease_refusals \
             WHERE session_id = $1 AND refused_at >= $2 ORDER BY refused_at DESC",
        )
        .bind(&session.0)
        .bind(since)
        .fetch_all(pool)
        .await
        .map_err(backend)?;
        Ok(rows
            .into_iter()
            .map(
                |(at, refused_by, current_holder)| crate::store::lease::LeaseRefusal {
                    session: session.clone(),
                    at,
                    refused_by,
                    current_holder,
                },
            )
            .collect())
    }
}
