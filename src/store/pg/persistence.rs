//! Write transactions for the Postgres-wire family. Every `begin`/`commit`
//! on the write path is in this file, each inside a `tx_retry` body so a
//! serializable conflict replays the whole transaction:
//!
//! * `flush` — one transaction per [`MutationBatch`]: the session-row upsert
//!   that stamps the #17 mutation epoch and merges the #29 GC mark, the
//!   fencing-token gate for every touched session (including the owners of
//!   rows the batch deletes), then the planned statements, then commit. A stale token or any failed statement returns
//!   before the commit and rolls the batch back.
//! * `record_canonization` — the canon task's immediate write, fenced the same
//!   way inside its own transaction.
//! * `seed` (fixtures) — the full-snapshot path.
//! * The writer-published flush stats row (`session_stats`).
//!
//! The statements themselves are in `write_rows.rs` and never own a
//! transaction.

use sqlx::Row;

use super::codec::backend;
use super::pool::tx_retry;
use super::sql::{DELETED_ROW_SESSIONS_SQL, UPSERT_SESSION_ROW_SQL};
#[cfg(feature = "fixtures")]
use super::sql::{UPSERT_RESERVATION_SQL, UPSERT_SYNONYM_SQL};
use super::write_rows::{apply_canonization, apply_step};
#[cfg(feature = "fixtures")]
use super::write_rows::{
    bulk_upsert_concepts, bulk_upsert_edges, bulk_upsert_interactions, insert_canonization_event,
    put_write_intent,
};
use super::{Dialect, PgStore};
use crate::store::batch::{
    batch_deleted_ids, batch_session_ids, plan_flush, BulkLimits, ACCESS_COLUMNS, CONCEPT_COLUMNS,
    EDGE_COLUMNS, INTERACTION_COLUMNS,
};
#[cfg(feature = "fixtures")]
use crate::store::batch::{seed_concept_rows, seed_edge_rows};
use crate::store::lease::lease_permits_write;
use crate::store::{map_write_err, SessionFlushStats};
#[cfg(feature = "fixtures")]
use crate::types::GraphSnapshot;
use crate::types::{CanonizationEvent, MutationBatch, SessionId, StoreError};

/// Rows per multi-row upsert statement (L82-1).
///
/// The hard ceiling is PostgreSQL's wire-protocol limit of 65535 bind
/// parameters per statement: 16 columns caps `concepts` at 4095 rows and 9
/// columns caps `edges` at 7281. These sit an order of magnitude below that —
/// enough that the live finding's 784-mutation tail plans into single-digit
/// statements, while keeping any one statement small enough that CockroachDB
/// plans it without trouble and a retry re-sends little.
///
/// **`interactions` batches now too.** `interactions.previous_id REFERENCES
/// interactions(id)` is a *self* foreign key, and a batch's interactions form a
/// chain — which is why this used to be 1 ("row-at-a-time costs nothing").
/// F4 disproved the "costs nothing": `record_action` emits one interaction per
/// call and concepts/edges batch, so the *un-batched* interactions became the
/// largest per-statement contributor to the close-flush round-trip count (30 of
/// ~37 at a K=30 deferred tail). Batching them is safe because
/// `dedupe_last_at_first_position` (R1-1) emits each interaction at its first
/// occurrence — reference-before-use — and both engines check the self-FK at
/// end-of-statement, so a chain inside one multi-row statement is satisfied; a
/// chain split across statements still passes because the reference is written
/// in an earlier statement of the same transaction. Verified against the live
/// Cockroach cluster (F4 re-measurement).
pub(super) const BULK_LIMITS: BulkLimits = BulkLimits {
    interactions: 256,
    concepts: 256,
    edges: 512,
    // Issue #30: 4 binds per row; a whole realistic tick's accesses in one
    // round-trip on a serverless cluster.
    accesses: 1024,
};

/// PostgreSQL's wire-protocol ceiling on bind parameters per statement, which
/// CockroachDB inherits by speaking the same protocol.
pub(super) const PG_MAX_BIND_PARAMETERS: usize = 65535;

// R1-4: the ceiling arithmetic above is prose, and prose does not fail a build.
// A column added to `concepts` without revisiting the row limit would push a
// chunk over the wire limit and only be discovered against a real server; these
// turn it into a compile error.
const _: () = assert!(
    BULK_LIMITS.interactions * INTERACTION_COLUMNS <= PG_MAX_BIND_PARAMETERS,
    "interactions chunk exceeds the PostgreSQL bind-parameter limit"
);

const _: () = assert!(
    BULK_LIMITS.concepts * CONCEPT_COLUMNS <= PG_MAX_BIND_PARAMETERS,
    "concepts chunk exceeds the PostgreSQL bind-parameter limit"
);

const _: () = assert!(
    BULK_LIMITS.edges * EDGE_COLUMNS <= PG_MAX_BIND_PARAMETERS,
    "edges chunk exceeds the PostgreSQL bind-parameter limit"
);

const _: () = assert!(
    BULK_LIMITS.accesses * ACCESS_COLUMNS <= PG_MAX_BIND_PARAMETERS,
    "accesses chunk exceeds the PostgreSQL bind-parameter limit"
);

impl<D: Dialect> PgStore<D> {
    /// Seed a prebuilt snapshot directly (fixtures track, MemoryStore parity). Writes all
    /// seven tables in one transaction — the full-snapshot path that carries synonyms and
    /// reservations (they have no `Mutation` kind, S5 contract). Also persists
    /// `GraphSnapshot.embedding` into `sessions.embedding_{kind,model,dim}` (STORE-1),
    /// so a seeded contract survives restarts instead of being dropped.
    #[cfg(feature = "fixtures")]
    pub async fn seed(&self, snapshot: &GraphSnapshot) -> Result<(), StoreError> {
        let sid = &snapshot.session_id.0;
        let embedding_dim = snapshot
            .embedding
            .as_ref()
            .map(|contract| i64::try_from(contract.dim))
            .transpose()
            .map_err(|_| StoreError::Invariant("embedding dimension does not fit i64".into()))?;
        let pool = &self.pool().await?;
        let root_goal = snapshot
            .root_goal
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| backend(format!("serialize root_goal: {e}")))?;
        // Copy handle (Option<&str>): the FnMut body runs once per retry attempt and
        // must not move the owned String into the first attempt's future.
        let root_goal = root_goal.as_deref();
        let embedding = snapshot.embedding.as_ref();
        // Copy handles for the same FnMut-reborrow reason as root_goal.
        let embedding_kind = embedding.map(|c| c.kind.as_str());
        let embedding_model = embedding.and_then(|c| c.model.as_deref());
        // Issue #17: the seeded snapshot carries the mutation accounting.
        let mutation_epoch = i64::try_from(snapshot.mutation_epoch).unwrap_or(i64::MAX);
        // Issue #29: and GC's sweep mark.
        let last_gc_epoch = i64::try_from(snapshot.gc_mark.last_gc_epoch).unwrap_or(i64::MAX);
        let last_gc_at = snapshot.gc_mark.last_gc_at;
        tx_retry(|| async move {
            let mut tx = pool
                .begin()
                .await
                .map_err(|e| map_write_err(e, |m| format!("begin seed transaction: {m}")))?;
            sqlx::query(&self.sql.upsert_session)
                .bind(sid)
                .bind(root_goal)
                .bind(snapshot.created_at)
                .bind(snapshot.closed_at)
                .bind(embedding_kind)
                .bind(embedding_model)
                .bind(embedding_dim)
                .bind(mutation_epoch)
                .bind(last_gc_epoch)
                .bind(last_gc_at)
                .execute(&mut *tx)
                .await
                .map_err(|e| map_write_err(e, |m| format!("upsert session row: {m}")))?;
            // Interactions before concepts (`concepts.origin_interaction`
            // REFERENCES interactions(id)); each chunked the same way a flush
            // is, so the seed path exercises the same statements (L82-1).
            for i in &snapshot.interactions {
                bulk_upsert_interactions(&mut *tx, &[i]).await?;
            }
            // Deduplicated first (R1-6): a multi-row statement rejects colliding
            // input rows outright, where the row-at-a-time seed this replaced
            // simply last-wins'd them.
            for chunk in seed_concept_rows(&snapshot.concepts).chunks(BULK_LIMITS.concepts) {
                bulk_upsert_concepts(&mut *tx, chunk, &self.sql).await?;
            }
            for chunk in seed_edge_rows(&snapshot.edges).chunks(BULK_LIMITS.edges) {
                bulk_upsert_edges(&mut *tx, chunk).await?;
            }
            for s in &snapshot.synonyms {
                sqlx::query(UPSERT_SYNONYM_SQL)
                    .bind(&s.session_id.0)
                    .bind(&s.source_key)
                    .bind(&s.canonical_key)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| map_write_err(e, |m| format!("upsert synonym: {m}")))?;
            }
            for r in &snapshot.reservations {
                sqlx::query(UPSERT_RESERVATION_SQL)
                    .bind(&r.session_id.0)
                    .bind(r.node_id.0)
                    .bind(&r.agent_id.0)
                    .bind(r.expires_at)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| map_write_err(e, |m| format!("upsert reservation: {m}")))?;
            }
            for ev in &snapshot.canonization_events {
                insert_canonization_event(&mut *tx, ev).await?;
            }
            // J3 write intents ride the seed for adapter parity (see the
            // SQLite seed).
            for intent in &snapshot.write_intents {
                put_write_intent(&mut *tx, intent).await?;
            }
            tx.commit()
                .await
                .map_err(|e| map_write_err(e, |m| format!("commit seed transaction: {m}")))?;
            Ok(())
        })
        .await
    }

    pub(super) async fn upsert_flush_stats(
        &self,
        session: &SessionId,
        stats: &SessionFlushStats,
    ) -> Result<(), StoreError> {
        let pool = &self.pool().await?;
        // Upsert the whole row so re-publishes converge (idempotency, same
        // contract as `flush`). Only the writer's FlushTask calls this;
        // readers only read. `updated_at` is stamped from the cluster clock
        // (now()).
        sqlx::query(
            "INSERT INTO session_stats (session_id, flush_lag_ms, log_depth, updated_at) \
             VALUES ($1, $2, $3, now()) \
             ON CONFLICT (session_id) DO UPDATE SET \
               flush_lag_ms = excluded.flush_lag_ms, \
               log_depth = excluded.log_depth, \
               updated_at = excluded.updated_at",
        )
        .bind(&session.0)
        .bind(stats.flush_lag_ms as i64)
        .bind(stats.log_depth as i64)
        .execute(pool)
        .await
        .map_err(|e| map_write_err(e, |m| format!("write flush stats: {m}")))?;
        Ok(())
    }

    pub(super) async fn select_flush_stats(
        &self,
        session: &SessionId,
    ) -> Result<Option<SessionFlushStats>, StoreError> {
        let pool = &self.pool().await?;
        let row =
            sqlx::query("SELECT flush_lag_ms, log_depth FROM session_stats WHERE session_id = $1")
                .bind(&session.0)
                .fetch_optional(pool)
                .await
                .map_err(backend)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let flush_lag_ms: i64 = row
            .try_get("flush_lag_ms")
            .map_err(|e| backend(format!("read flush stats: flush_lag_ms: {e}")))?;
        let log_depth: i64 = row
            .try_get("log_depth")
            .map_err(|e| backend(format!("read flush stats: log_depth: {e}")))?;
        Ok(Some(SessionFlushStats {
            flush_lag_ms: u64::try_from(flush_lag_ms).unwrap_or(u64::MAX),
            log_depth: u64::try_from(log_depth).unwrap_or(u64::MAX),
        }))
    }

    pub(super) async fn flush_batch(
        &self,
        batch: &MutationBatch,
        token: Option<u64>,
    ) -> Result<(), StoreError> {
        let pool = &self.pool().await?;
        tx_retry(|| async move {
            let mut tx = pool
                .begin()
                .await
                .map_err(|e| map_write_err(e, |m| format!("begin flush transaction: {m}")))?;
            // Ensure a sessions row for every session the batch writes into — the DDL
            // enforces `REFERENCES sessions(session_id)` on interactions/concepts, and the
            // graph tier creates sessions implicitly (MemoryStore::ensure_session parity).
            // Issue #17: the same statement stamps the batch's absolute mutation-epoch
            // watermark, monotonically, in this transaction.
            for sid in batch_session_ids(&batch.mutations) {
                sqlx::query(UPSERT_SESSION_ROW_SQL)
                    .bind(sid)
                    .bind(i64::try_from(batch.mutation_epoch).unwrap_or(i64::MAX))
                    .bind(i64::try_from(batch.gc_mark.last_gc_epoch).unwrap_or(i64::MAX))
                    .bind(batch.gc_mark.last_gc_at)
                    .bind(batch.gc_mark.last_gc_at_reset)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| map_write_err(e, |m| format!("upsert session row: {m}")))?;
            }

            // The fenced set is the stamped set plus the owning session of
            // every row a `DeleteNode`/`DeleteEdge` will remove. Those
            // mutations name no session, so without this a delete-only batch
            // (a GC sweep) skipped the gate entirely and a writer that had
            // lost its lease could delete rows in a session another writer now
            // holds. One statement, inside this transaction and before any
            // delete runs, and only when the batch deletes anything; a row
            // that is already gone resolves nothing and its delete is a no-op.
            // The stamp loop above is deliberately unchanged.
            let mut fenced: Vec<String> = batch_session_ids(&batch.mutations)
                .into_iter()
                .map(str::to_owned)
                .collect();
            let (deleted_nodes, deleted_edges) = batch_deleted_ids(&batch.mutations);
            if !deleted_nodes.is_empty() || !deleted_edges.is_empty() {
                let nodes: Vec<uuid::Uuid> = deleted_nodes.iter().map(|id| id.0).collect();
                let edges: Vec<uuid::Uuid> = deleted_edges.iter().map(|id| id.0).collect();
                let owners: Vec<String> = sqlx::query_scalar(DELETED_ROW_SESSIONS_SQL)
                    .bind(&nodes)
                    .bind(&edges)
                    .fetch_all(&mut *tx)
                    .await
                    .map_err(backend)?;
                for sid in owners {
                    if !fenced.contains(&sid) {
                        fenced.push(sid);
                    }
                }
            }

            // Fencing-token gate (#1): reject a stale/missing token for every
            // session the batch touches, INSIDE the same transaction as the
            // writes (atomic with them; a takeover cannot slip between the
            // check and the commit — on rejection `?` drops `tx`, rolling back).
            // An unleased session (no row / current_token 0) passes — seed /
            // fixture parity.
            for sid in &fenced {
                let current: Option<i64> = sqlx::query_scalar(
                    "SELECT current_token FROM session_leases WHERE session_id = $1",
                )
                .bind(sid)
                .fetch_optional(&mut *tx)
                .await
                .map_err(backend)?;
                if let Some(cur) = current {
                    let cur = u64::try_from(cur).map_err(|_| {
                        StoreError::Invariant(format!("session {sid}: negative lease current_token"))
                    })?;
                    if !lease_permits_write(cur, token) {
                        return Err(StoreError::StaleWrite(format!(
                            "session {sid}: presented token {token:?} is stale (lease token {cur}) — \
                             single-writer fence (GitHub issue #1)"
                        )));
                    }
                }
            }

            // Replay the batch as planned statements rather than one statement
            // per mutation (L82-1). Order is still the batch's own — see
            // `store::batch` for why bucketing upserts by table preserves it,
            // and why every mutation that could *observe* a row is a barrier.
            //
            // This is the fix for the live finding: against a serverless
            // cluster the old loop cost one network round-trip per mutation, so
            // a 784-mutation shutdown tail could not drain inside `close()`'s
            // 10 s grace window and was discarded.
            for step in plan_flush(&batch.mutations, BULK_LIMITS) {
                apply_step(&mut *tx, &step, &self.sql).await?;
            }
            tx.commit()
                .await
                .map_err(|e| map_write_err(e, |m| format!("commit flush transaction: {m}")))?;
            Ok(())
        })
        .await
    }

    pub(super) async fn record_canonization_event(
        &self,
        event: &CanonizationEvent,
        token: Option<u64>,
    ) -> Result<(), StoreError> {
        let pool = &self.pool().await?;
        tx_retry(|| async move {
            let mut tx = pool.begin().await.map_err(|e| {
                map_write_err(e, |m| format!("begin record_canonization transaction: {m}"))
            })?;
            // Fencing-token gate (#1): this durable write path HAD no lease
            // check at all — the canon task bypassed `lease_lost`. Check the
            // token inside this transaction, atomically with the write
            // (rolls back on `?`).
            let current: Option<i64> = sqlx::query_scalar(
                "SELECT current_token FROM session_leases WHERE session_id = $1",
            )
            .bind(event.session_id.as_str())
            .fetch_optional(&mut *tx)
            .await
            .map_err(backend)?;
            if let Some(cur) = current {
                let cur = u64::try_from(cur).map_err(|_| {
                    StoreError::Invariant(format!(
                        "session {}: negative lease current_token",
                        event.session_id
                    ))
                })?;
                if !lease_permits_write(cur, token) {
                    return Err(StoreError::StaleWrite(format!(
                        "session {}: presented token {token:?} is stale (lease token {cur}) — \
                         single-writer fence (GitHub issue #1)",
                        event.session_id,
                    )));
                }
            }
            apply_canonization(&mut *tx, event).await?;
            tx.commit().await.map_err(|e| {
                map_write_err(e, |m| {
                    format!("commit record_canonization transaction: {m}")
                })
            })?;
            Ok(())
        })
        .await
    }
}
