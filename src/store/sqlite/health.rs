//! Read-only store-wide health overview for the SQLite adapter (#248).
//!
//! Runs a single read-only SQL statement over one snapshot and one store-clock
//! instant (`clock` CTE), never acquiring a lease or mutating any row.

use chrono::{DateTime, Utc};
use sqlx::Row;

use super::codec::{db_err, text_to_ts};
use super::SqliteStore;
use crate::store::erase::ERASED_HOLDER;
use crate::store::lease::RELEASED_HOLDER;
use crate::store::{ActiveSessionLease, CanonizationCounts, SessionHealth};
use crate::types::{SessionId, StoreError};

/// Single-statement per-session durable health query for SQLite.
///
/// `clock` captures one `strftime` instant so the active-lease check is
/// evaluated against the store clock (F18) consistently across all columns.
/// `COUNT(CASE WHEN ... THEN 1 END)` keeps every aggregate an integer `i64`.
const STORE_HEALTH_SQL: &str = "\
WITH clock AS ( \
    SELECT strftime('%Y-%m-%dT%H:%M:%fZ','now') AS now_ts \
), \
active_sessions AS ( \
    SELECT s.session_id \
    FROM sessions s \
    WHERE NOT EXISTS ( \
        SELECT 1 FROM session_leases sl \
        WHERE sl.session_id = s.session_id AND sl.holder = ?1 \
    ) \
    UNION \
    SELECT sl.session_id \
    FROM session_leases sl \
    WHERE sl.holder <> ?1 \
), \
concept_agg AS ( \
    SELECT \
        session_id, \
        COUNT(*) AS concept_count, \
        COUNT(CASE WHEN embedding IS NOT NULL THEN 1 END) AS embedded_count, \
        COUNT(CASE WHEN canonization_status = 'None' THEN 1 END) AS canon_none, \
        COUNT(CASE WHEN canonization_status = 'Candidate' THEN 1 END) AS canon_candidate, \
        COUNT(CASE WHEN canonization_status = 'Venerable' THEN 1 END) AS canon_venerable, \
        COUNT(CASE WHEN canonization_status = 'Canonical' THEN 1 END) AS canon_canonical, \
        COUNT(CASE WHEN canonization_status NOT IN ('None', 'Candidate', 'Venerable', 'Canonical') THEN 1 END) AS canon_unknown, \
        MAX(created_at) AS max_concept_at \
    FROM concepts \
    GROUP BY session_id \
), \
edge_agg AS ( \
    SELECT \
        session_id, \
        COUNT(*) AS edge_count, \
        MAX(created_at) AS max_edge_created_at, \
        MAX(last_reinforced) AS max_edge_reinforced_at \
    FROM edges \
    GROUP BY session_id \
), \
interaction_agg AS ( \
    SELECT \
        session_id, \
        MAX(created_at) AS max_interaction_at \
    FROM interactions \
    GROUP BY session_id \
), \
event_agg AS ( \
    SELECT \
        session_id, \
        MAX(occurred_at) AS max_event_at \
    FROM canonization_events \
    GROUP BY session_id \
) \
SELECT \
    a.session_id, \
    COALESCE(c.concept_count, 0) AS concept_count, \
    COALESCE(e.edge_count, 0) AS edge_count, \
    COALESCE(c.embedded_count, 0) AS embedded_count, \
    COALESCE(c.canon_none, 0) AS canon_none, \
    COALESCE(c.canon_candidate, 0) AS canon_candidate, \
    COALESCE(c.canon_venerable, 0) AS canon_venerable, \
    COALESCE(c.canon_canonical, 0) AS canon_canonical, \
    COALESCE(c.canon_unknown, 0) AS canon_unknown, \
    CASE \
        WHEN sl.session_id IS NOT NULL \
         AND sl.holder <> ?1 \
         AND sl.holder <> ?2 \
         AND sl.expires_at > clock.now_ts \
        THEN sl.holder \
        ELSE NULL \
    END AS lease_holder, \
    CASE \
        WHEN sl.session_id IS NOT NULL \
         AND sl.holder <> ?1 \
         AND sl.holder <> ?2 \
         AND sl.expires_at > clock.now_ts \
        THEN sl.acquired_at \
        ELSE NULL \
    END AS lease_acquired_at, \
    CASE \
        WHEN sl.session_id IS NOT NULL \
         AND sl.holder <> ?1 \
         AND sl.holder <> ?2 \
         AND sl.expires_at > clock.now_ts \
        THEN sl.expires_at \
        ELSE NULL \
    END AS lease_expires_at, \
    i.max_interaction_at, \
    c.max_concept_at, \
    e.max_edge_created_at, \
    e.max_edge_reinforced_at, \
    ev.max_event_at \
FROM active_sessions a \
CROSS JOIN clock \
LEFT JOIN concept_agg c ON c.session_id = a.session_id \
LEFT JOIN edge_agg e ON e.session_id = a.session_id \
LEFT JOIN interaction_agg i ON i.session_id = a.session_id \
LEFT JOIN event_agg ev ON ev.session_id = a.session_id \
LEFT JOIN session_leases sl ON sl.session_id = a.session_id \
ORDER BY a.session_id ASC";

fn non_negative(val: i64, field: &str, session_id: &str) -> Result<u64, StoreError> {
    u64::try_from(val).map_err(|_| {
        StoreError::Backend(format!(
            "store health: negative {field} ({val}) for session {session_id:?}"
        ))
    })
}

impl SqliteStore {
    pub(super) async fn read_store_health(&self) -> Result<Vec<SessionHealth>, StoreError> {
        let rows = sqlx::query(STORE_HEALTH_SQL)
            .bind(ERASED_HOLDER)
            .bind(RELEASED_HOLDER)
            .fetch_all(self.pool())
            .await
            .map_err(|e| db_err("read store health", e))?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let session_id: String = row
                .try_get("session_id")
                .map_err(|e| db_err("read store health: session_id", e))?;
            let canon_unknown: i64 = row
                .try_get("canon_unknown")
                .map_err(|e| db_err("read store health: canon_unknown", e))?;
            if canon_unknown != 0 {
                return Err(StoreError::Backend(format!(
                    "store health: session {session_id:?} has {canon_unknown} concept(s) with an \
                     unknown canonization_status"
                )));
            }

            let concept_count = non_negative(
                row.try_get("concept_count")
                    .map_err(|e| db_err("read store health: concept_count", e))?,
                "concept_count",
                &session_id,
            )?;
            let edge_count = non_negative(
                row.try_get("edge_count")
                    .map_err(|e| db_err("read store health: edge_count", e))?,
                "edge_count",
                &session_id,
            )?;
            let embedded_count = non_negative(
                row.try_get("embedded_count")
                    .map_err(|e| db_err("read store health: embedded_count", e))?,
                "embedded_count",
                &session_id,
            )?;
            let canonization = CanonizationCounts {
                none: non_negative(
                    row.try_get("canon_none")
                        .map_err(|e| db_err("read store health: canon_none", e))?,
                    "canon_none",
                    &session_id,
                )?,
                candidate: non_negative(
                    row.try_get("canon_candidate")
                        .map_err(|e| db_err("read store health: canon_candidate", e))?,
                    "canon_candidate",
                    &session_id,
                )?,
                venerable: non_negative(
                    row.try_get("canon_venerable")
                        .map_err(|e| db_err("read store health: canon_venerable", e))?,
                    "canon_venerable",
                    &session_id,
                )?,
                canonical: non_negative(
                    row.try_get("canon_canonical")
                        .map_err(|e| db_err("read store health: canon_canonical", e))?,
                    "canon_canonical",
                    &session_id,
                )?,
            };

            let lease_holder: Option<String> = row
                .try_get("lease_holder")
                .map_err(|e| db_err("read store health: lease_holder", e))?;
            let lease_acquired: Option<String> = row
                .try_get("lease_acquired_at")
                .map_err(|e| db_err("read store health: lease_acquired_at", e))?;
            let lease_expires: Option<String> = row
                .try_get("lease_expires_at")
                .map_err(|e| db_err("read store health: lease_expires_at", e))?;
            let lease = match (lease_holder, lease_acquired, lease_expires) {
                (Some(holder), Some(acquired), Some(expires)) => Some(ActiveSessionLease {
                    holder,
                    acquired_at: text_to_ts(&acquired)?,
                    expires_at: text_to_ts(&expires)?,
                }),
                (None, None, None) => None,
                _ => {
                    return Err(StoreError::Backend(format!(
                        "store health: inconsistent active lease columns for session {session_id:?}"
                    )));
                }
            };

            let ts_cols = [
                "max_interaction_at",
                "max_concept_at",
                "max_edge_created_at",
                "max_edge_reinforced_at",
                "max_event_at",
            ];
            let mut last_write_at: Option<DateTime<Utc>> = None;
            for col in ts_cols {
                let raw: Option<String> = row
                    .try_get(col)
                    .map_err(|e| db_err(&format!("read store health: {col}"), e))?;
                if let Some(text) = raw {
                    let parsed = text_to_ts(&text)?;
                    last_write_at = Some(match last_write_at {
                        Some(prev) => prev.max(parsed),
                        None => parsed,
                    });
                }
            }

            out.push(SessionHealth {
                session_id: SessionId(session_id),
                concept_count,
                edge_count,
                embedded_count,
                canonization,
                lease,
                last_write_at,
            });
        }

        Ok(out)
    }
}
