//! Session load for the Postgres-wire family: `load_session` reads the
//! session row and every table in one transaction, replayed whole by
//! `tx_retry` on a serializable conflict, so a load always observes one
//! snapshot. Row decoding is in `codec.rs`; the J3 write-intent reader runs on
//! the same transaction and never begins or commits one.

use chrono::{DateTime, Utc};
use sqlx::Row;

use super::codec::{
    backend, row_to_canonization_event, row_to_concept, row_to_edge, row_to_interaction,
    row_to_reservation, row_to_synonym, session_embedding_from_parts,
};
use super::pool::tx_retry;
use super::sql::SELECT_SYNONYMS_SQL;
use super::{Dialect, PgStore};
use crate::types::{GraphSnapshot, NodeId, SessionId, StoreError};

/// Load a session's write intents (J3), in replay order — (`issued_ms`,
/// `lane_seq`), exact admission order within one issuing process and
/// wall-clock order across processes.
pub(super) async fn load_write_intents(
    tx: &mut sqlx::PgConnection,
    session: &SessionId,
) -> Result<Vec<crate::types::WriteIntent>, StoreError> {
    let rows = sqlx::query(
        "SELECT receipt, agent, interaction_id, lane_seq, issued_ms, payload, created_at, \
                consumed_at, outcome_tag, outcome_summary \
         FROM write_intents WHERE session_id = $1 ORDER BY issued_ms ASC, lane_seq ASC",
    )
    .bind(&session.0)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| backend(format!("load write intents: {e}")))?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let receipt: String = row
            .try_get(0)
            .map_err(|e| backend(format!("load write intents: {e}")))?;
        let agent: String = row
            .try_get(1)
            .map_err(|e| backend(format!("load write intents: {e}")))?;
        let interaction: uuid::Uuid = row
            .try_get(2)
            .map_err(|e| backend(format!("load write intents: {e}")))?;
        let lane_seq: i64 = row
            .try_get(3)
            .map_err(|e| backend(format!("load write intents: {e}")))?;
        let issued_ms: i64 = row
            .try_get(4)
            .map_err(|e| backend(format!("load write intents: {e}")))?;
        let payload: String = row
            .try_get(5)
            .map_err(|e| backend(format!("load write intents: {e}")))?;
        let created_at: DateTime<Utc> = row
            .try_get(6)
            .map_err(|e| backend(format!("load write intents: {e}")))?;
        let consumed_at: Option<DateTime<Utc>> = row
            .try_get(7)
            .map_err(|e| backend(format!("load write intents: {e}")))?;
        let outcome_tag: Option<String> = row
            .try_get(8)
            .map_err(|e| backend(format!("load write intents: {e}")))?;
        let outcome_summary: Option<String> = row
            .try_get(9)
            .map_err(|e| backend(format!("load write intents: {e}")))?;
        let payload: crate::types::WriteIntentPayload = serde_json::from_str(&payload)
            .map_err(|e| backend(format!("parse write intent payload: {e}")))?;
        let outcome = match (consumed_at, outcome_tag, outcome_summary) {
            (Some(consumed_at), Some(tag), Some(summary)) => {
                Some(crate::types::WriteIntentOutcome {
                    tag,
                    summary,
                    consumed_at,
                })
            }
            (None, None, None) => None,
            _ => {
                return Err(StoreError::Invariant(format!(
                    "write intent {receipt}: consumed_at/outcome columns are partially set"
                )))
            }
        };
        out.push(crate::types::WriteIntent {
            session_id: session.clone(),
            receipt,
            agent: crate::types::AgentId::new(&agent),
            interaction: NodeId(interaction),
            lane_seq: u64::try_from(lane_seq).unwrap_or(u64::MAX),
            issued_ms,
            payload,
            created_at,
            outcome,
        });
    }
    Ok(out)
}

impl<D: Dialect> PgStore<D> {
    pub(super) async fn load_snapshot(
        &self,
        session: &SessionId,
    ) -> Result<GraphSnapshot, StoreError> {
        let pool = &self.pool().await?;
        let session_id = session.clone();
        // Copy handle (&SessionId): the FnMut body runs once per retry attempt.
        let sid = &session_id;
        tx_retry(|| async move {
            let mut tx = pool.begin().await.map_err(backend)?;
            let session_row = sqlx::query(&self.sql.select_session)
                .bind(sid.0.as_str())
                .fetch_optional(&mut *tx)
                .await
                .map_err(backend)?;
            let Some(session_row) = session_row else {
                return Err(StoreError::SessionNotFound(sid.0.clone()));
            };
            let root_goal: Option<String> = session_row.try_get("root_goal").map_err(backend)?;
            let root_goal = root_goal
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .map_err(|e| backend(format!("parse root_goal JSONB: {e}")))?;

            // Ordered `SetEmbedding` mutations and full-snapshot seed both persist
            // the nullable kind/model/dim columns. STORE-7: a row with exactly one of
            // embedding_kind / embedding_dim set (kind XOR dim) is a corruption
            // error — mirroring sqlite — never a silent `None` (see
            // `session_embedding_from_parts`).
            let embedding_kind: Option<String> =
                session_row.try_get("embedding_kind").map_err(backend)?;
            let embedding_model: Option<String> =
                session_row.try_get("embedding_model").map_err(backend)?;
            let embedding_dim: Option<i64> =
                session_row.try_get("embedding_dim").map_err(backend)?;
            let embedding = session_embedding_from_parts(
                embedding_kind,
                embedding_model,
                embedding_dim,
                sid.0.as_str(),
            )?;
            // Issue #17: the durable mutation counter, stamped by flush and
            // seeded by `seed`; the loading writer resumes it so GC's
            // `gc_interval` measures deployment-lifetime mutations.
            let mutation_epoch: i64 = session_row.try_get("mutation_epoch").map_err(backend)?;
            // Issue #29: GC's sweep accounting, resumed with the epoch so a
            // restart neither re-sweeps nor resets the `gc_max_interval` clock.
            let last_gc_epoch: i64 = session_row.try_get("last_gc_epoch").map_err(backend)?;
            let gc_mark = crate::types::GcMark {
                last_gc_epoch: u64::try_from(last_gc_epoch).unwrap_or(0),
                last_gc_at: session_row.try_get("last_gc_at").map_err(backend)?,
                last_gc_at_reset: false,
            };

            let interactions = sqlx::query(&self.sql.select_interactions)
                .bind(sid.0.as_str())
                .fetch_all(&mut *tx)
                .await
                .map_err(backend)?
                .iter()
                .map(row_to_interaction)
                .collect::<Result<Vec<_>, _>>()?;

            let concepts = sqlx::query(&self.sql.select_concepts)
                .bind(sid.0.as_str())
                .fetch_all(&mut *tx)
                .await
                .map_err(backend)?
                .iter()
                .map(row_to_concept)
                .collect::<Result<Vec<_>, _>>()?;

            let edges = sqlx::query(&self.sql.select_edges)
                .bind(sid.0.as_str())
                .fetch_all(&mut *tx)
                .await
                .map_err(backend)?
                .iter()
                .map(row_to_edge)
                .collect::<Result<Vec<_>, _>>()?;

            let synonyms = sqlx::query(SELECT_SYNONYMS_SQL)
                .bind(sid.0.as_str())
                .fetch_all(&mut *tx)
                .await
                .map_err(backend)?
                .iter()
                .map(row_to_synonym)
                .collect::<Result<Vec<_>, _>>()?;

            let reservations = sqlx::query(&self.sql.select_reservations)
                .bind(sid.0.as_str())
                .fetch_all(&mut *tx)
                .await
                .map_err(backend)?
                .iter()
                .map(row_to_reservation)
                .collect::<Result<Vec<_>, _>>()?;

            let canonization_events = sqlx::query(&self.sql.select_canonization_events)
                .bind(sid.0.as_str())
                .fetch_all(&mut *tx)
                .await
                .map_err(backend)?
                .iter()
                .map(row_to_canonization_event)
                .collect::<Result<Vec<_>, _>>()?;

            let write_intents = load_write_intents(&mut tx, sid).await?;

            tx.commit().await.map_err(backend)?;
            Ok(GraphSnapshot {
                session_id: sid.clone(),
                root_goal,
                created_at: session_row.try_get("created_at").map_err(backend)?,
                closed_at: session_row.try_get("closed_at").map_err(backend)?,
                interactions,
                concepts,
                edges,
                synonyms,
                reservations,
                canonization_events,
                embedding,
                write_intents,
                mutation_epoch: u64::try_from(mutation_epoch).unwrap_or(u64::MAX),
                gc_mark,
            })
        })
        .await
    }
}
