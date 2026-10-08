//! Write transactions for the SQLite adapter. Every `begin`/`commit` on the
//! write path is in this file:
//!
//! * `flush` — one transaction per [`MutationBatch`]: the session-row stamp
//!   (`ensure_sessions`: the #17 mutation epoch and the #29 GC mark, merged
//!   monotonically), the fencing-token gate for every touched session
//!   (including the owners of rows the batch deletes), then the planned
//!   statements, then commit. A stale token or any failed
//!   statement returns before the commit and rolls the batch back.
//! * `record_canonization` — the canon task's immediate write, fenced the same
//!   way inside its own transaction.
//! * `seed` (fixtures) — the full-snapshot path.
//! * `erase` — session erasure (#23): the lease gate, the tombstone and every
//!   session-keyed DELETE in one `IMMEDIATE` transaction.
//! * The writer-published flush stats row (`session_stats`), suppressed for an
//!   erased session.
//!
//! The statements themselves are in `write_rows.rs` and never own a
//! transaction.

use std::collections::HashSet;

use sqlx::Row;

#[cfg(feature = "fixtures")]
use super::codec::enum_to_text;
use super::codec::{db_err, ts_to_text};
use super::leases::{lease_info_from_text, LeaseRowText, LEASE_ROW_SQL};
use super::write_rows::{
    apply_canonization_transition, apply_step, count_session_vectors, delete_session_rows,
    write_erase_tombstone, ERASE_STATEMENTS,
};
#[cfg(feature = "fixtures")]
use super::write_rows::{put_write_intent, upsert_concepts, upsert_edges, upsert_interactions};
use super::SqliteStore;
use crate::store::batch::{
    batch_deleted_ids, plan_flush, BulkLimits, ACCESS_COLUMNS, CONCEPT_COLUMNS, EDGE_COLUMNS,
    INTERACTION_COLUMNS,
};
#[cfg(feature = "fixtures")]
use crate::store::batch::{seed_concept_rows, seed_edge_rows};
use crate::store::erase::{
    check_fence, erase_gate, EraseCounts, EraseGate, EraseOutcome, EraseReport, EraseStepHook,
    PriorLease, ERASED_HOLDER,
};
use crate::store::lease::LeaseHolder;
use crate::store::{map_write_err, SessionFlushStats};
#[cfg(feature = "fixtures")]
use crate::types::GraphSnapshot;
use crate::types::{CanonizationEvent, GcMark, Mutation, MutationBatch, SessionId, StoreError};

/// Rows per multi-row upsert statement (L82-1).
///
/// Chosen against SQLite's *most conservative* `SQLITE_MAX_VARIABLE_NUMBER` of
/// 999 rather than the 32766 a modern build ships: 16 columns × 60 rows = 960
/// and 10 × 99 = 990 both fit either way, and a statement that silently depends
/// on how the library was compiled is not worth the extra rows. The limits exist so
/// the shape matches Cockroach's, not to hit a latency target. `interactions`
/// batches too (100): its self-foreign-key chain is safe under the R1-1
/// first-position dedupe (reference-before-use) with end-of-statement FK checks,
/// mirroring the Cockroach constant (F4).
pub(super) const BULK_LIMITS: BulkLimits = BulkLimits {
    interactions: 100,
    // C2 added a 17th concept column (`human_confirmed`); 60 × 17 = 1020
    // breaches the conservative 999-variable ceiling, so the chunk drops to 58
    // (58 × 17 = 986) to keep the R1-4 assert honest on pre-3.32 SQLite.
    concepts: 58,
    edges: 99,
    // Issue #30: 4 binds per row; 240 × 4 = 960.
    accesses: 240,
};

/// The conservative `SQLITE_MAX_VARIABLE_NUMBER` the limits above are sized
/// against. Pre-3.32 builds ship this; 3.32+ ship 32766.
pub(super) const SQLITE_MAX_VARIABLE_NUMBER: usize = 999;

// R1-4: the arithmetic in the doc comment above is prose, and prose does not
// fail a build. Raising `concepts` past the 58-row chunk (see `BULK_LIMITS`)
// passes the whole local suite against a modern bundled SQLite and only breaks
// on an old one, in production. These turn that into a compile error.
const _: () = assert!(
    BULK_LIMITS.interactions * INTERACTION_COLUMNS <= SQLITE_MAX_VARIABLE_NUMBER,
    "interactions chunk exceeds SQLITE_MAX_VARIABLE_NUMBER"
);

const _: () = assert!(
    BULK_LIMITS.concepts * CONCEPT_COLUMNS <= SQLITE_MAX_VARIABLE_NUMBER,
    "concepts chunk exceeds SQLITE_MAX_VARIABLE_NUMBER"
);

const _: () = assert!(
    BULK_LIMITS.edges * EDGE_COLUMNS <= SQLITE_MAX_VARIABLE_NUMBER,
    "edges chunk exceeds SQLITE_MAX_VARIABLE_NUMBER"
);

const _: () = assert!(
    BULK_LIMITS.accesses * ACCESS_COLUMNS <= SQLITE_MAX_VARIABLE_NUMBER,
    "accesses chunk exceeds SQLITE_MAX_VARIABLE_NUMBER"
);

impl SqliteStore {
    /// Ensure every session touched by the batch has a `sessions` row (FK
    /// anchor; `created_at` DB-default) and stamp the batch's absolute
    /// `mutation_epoch` watermark onto it (issue #17). Idempotent, so once per
    /// unique session per batch is enough. The upsert is monotonic
    /// (`max(existing, stamped)`), so a replayed batch converges to the same
    /// final state instead of regressing the counter — the flush-replay
    /// contract — and the stamp commits in the batch's own transaction, so a
    /// crash can never leave durable content ahead of its durable count. A
    /// batch of pure `DeleteNode`/`DeleteEdge` mutations resolves no session
    /// here; its epoch contribution lands with the next batch that names one
    /// (the stamp is absolute, so the counter only ever lags, never rewinds).
    pub(super) async fn ensure_sessions(
        &self,
        tx: &mut sqlx::SqliteConnection,
        sessions: &HashSet<String>,
        mutation_epoch: u64,
        gc_mark: GcMark,
    ) -> Result<(), StoreError> {
        let epoch = i64::try_from(mutation_epoch).unwrap_or(i64::MAX);
        let last_gc_epoch = i64::try_from(gc_mark.last_gc_epoch).unwrap_or(i64::MAX);
        let last_gc_at = gc_mark.last_gc_at.map(ts_to_text);
        for sid in sessions {
            // Issue #29: GC's sweep mark rides the same statement with the
            // store-side merge (`GcMark::apply_to_stored`). `last_gc_at` is
            // fixed-width millisecond UTC text (`ts_to_text`), so the
            // lexicographic MAX is the chronological one; SQLite's two-argument
            // MAX returns NULL if either side is NULL, hence the COALESCE
            // fallbacks. The one exception to the max is a re-anchored mark
            // (`last_gc_at_reset`, the last bind) at least as current as the
            // stored mark (`last_gc_epoch >=`, read from the pre-update row):
            // its time replaces the stored one, so a future time left by a
            // corrected clock jump cannot keep the time trigger off, while a
            // stale reset replayed after a later sweep cannot rewind it
            // (`GcMark::reset_is_current_for`). `last_gc_epoch` is a max either way.
            sqlx::query(
                "INSERT INTO sessions (session_id, mutation_epoch, last_gc_epoch, last_gc_at) \
                 VALUES (?, ?, ?, ?) \
                 ON CONFLICT (session_id) DO UPDATE SET \
                     mutation_epoch = MAX(mutation_epoch, excluded.mutation_epoch), \
                     last_gc_epoch = MAX(last_gc_epoch, excluded.last_gc_epoch), \
                     last_gc_at = CASE WHEN ? AND excluded.last_gc_epoch >= last_gc_epoch \
                         THEN COALESCE(excluded.last_gc_at, last_gc_at) \
                         ELSE COALESCE(MAX(last_gc_at, excluded.last_gc_at), \
                                       last_gc_at, excluded.last_gc_at) END",
            )
            .bind(sid)
            .bind(epoch)
            .bind(last_gc_epoch)
            .bind(last_gc_at.as_deref())
            .bind(gc_mark.last_gc_at_reset)
            .execute(&mut *tx)
            .await
            .map_err(|e| map_write_err(e, |m| format!("ensure session row: {m}")))?;
        }
        Ok(())
    }

    /// Seed a prebuilt snapshot directly (fixtures track, MemoryStore/Cockroach
    /// parity). Writes all seven tables in one transaction — the full-snapshot
    /// path that carries synonyms and reservations (they have no `Mutation` kind,
    /// S5 contract). Persists `GraphSnapshot.embedding` into
    /// `sessions.embedding_{kind,model,dim}` (STORE-1), so a seeded contract
    /// survives `load_session` instead of being dropped.
    #[cfg(feature = "fixtures")]
    pub async fn seed(&self, snapshot: &GraphSnapshot) -> Result<(), StoreError> {
        let embedding_dim = snapshot
            .embedding
            .as_ref()
            .map(|contract| i64::try_from(contract.dim))
            .transpose()
            .map_err(|_| StoreError::Invariant("embedding dimension does not fit i64".into()))?;
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(|e| map_write_err(e, |m| format!("begin seed transaction: {m}")))?;
        let root_goal = snapshot
            .root_goal
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| StoreError::Backend(format!("serialize root_goal: {e}")))?;
        let embedding = snapshot.embedding.as_ref();
        let (embedding_kind, embedding_model) = match embedding {
            Some(c) => (Some(c.kind.as_str()), c.model.as_deref()),
            None => (None, None),
        };
        sqlx::query(
            "INSERT INTO sessions (\
                 session_id, root_goal, created_at, closed_at, \
                 embedding_kind, embedding_model, embedding_dim, mutation_epoch, \
                 last_gc_epoch, last_gc_at) \
             VALUES (?, ?, COALESCE(?, strftime('%Y-%m-%dT%H:%M:%fZ','now')), ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (session_id) DO UPDATE SET \
                 root_goal = excluded.root_goal, \
                 created_at = excluded.created_at, \
                 closed_at = excluded.closed_at, \
                 embedding_kind = excluded.embedding_kind, \
                 embedding_model = excluded.embedding_model, \
                 embedding_dim = excluded.embedding_dim, \
                 mutation_epoch = excluded.mutation_epoch, \
                 last_gc_epoch = excluded.last_gc_epoch, \
                 last_gc_at = excluded.last_gc_at",
        )
        .bind(&snapshot.session_id.0)
        .bind(root_goal.as_deref())
        .bind(snapshot.created_at.map(ts_to_text))
        .bind(snapshot.closed_at.map(ts_to_text))
        .bind(embedding_kind)
        .bind(embedding_model)
        .bind(embedding_dim)
        .bind(i64::try_from(snapshot.mutation_epoch).unwrap_or(i64::MAX))
        .bind(i64::try_from(snapshot.gc_mark.last_gc_epoch).unwrap_or(i64::MAX))
        .bind(snapshot.gc_mark.last_gc_at.map(ts_to_text))
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("upsert session row: {m}")))?;
        // Interactions before concepts (`concepts.origin_interaction`
        // REFERENCES interactions(id)); chunked exactly as a flush is, so the
        // seed path runs the same statements (L82-1).
        for i in &snapshot.interactions {
            upsert_interactions(&mut *tx, &[i]).await?;
        }
        // Deduplicated first (R1-6): a multi-row statement rejects colliding
        // input rows outright, where the row-at-a-time seed this replaced simply
        // last-wins'd them.
        for chunk in seed_concept_rows(&snapshot.concepts).chunks(BULK_LIMITS.concepts) {
            upsert_concepts(&mut *tx, chunk).await?;
        }
        for chunk in seed_edge_rows(&snapshot.edges).chunks(BULK_LIMITS.edges) {
            upsert_edges(&mut *tx, chunk).await?;
        }
        for s in &snapshot.synonyms {
            sqlx::query(
                "INSERT INTO synonyms (session_id, source_key, canonical_key) \
                 VALUES (?, ?, ?) \
                 ON CONFLICT (session_id, source_key) DO UPDATE SET \
                     canonical_key = excluded.canonical_key",
            )
            .bind(&s.session_id.0)
            .bind(&s.source_key)
            .bind(&s.canonical_key)
            .execute(&mut *tx)
            .await
            .map_err(|e| map_write_err(e, |m| format!("upsert synonym: {m}")))?;
        }
        for r in &snapshot.reservations {
            sqlx::query(
                "INSERT INTO reservations (session_id, node_id, agent_id, expires_at) \
                 VALUES (?, ?, ?, ?) \
                 ON CONFLICT (session_id, node_id) DO UPDATE SET \
                     agent_id = excluded.agent_id, \
                     expires_at = excluded.expires_at",
            )
            .bind(&r.session_id.0)
            .bind(r.node_id.0.to_string())
            .bind(&r.agent_id.0)
            .bind(ts_to_text(r.expires_at))
            .execute(&mut *tx)
            .await
            .map_err(|e| map_write_err(e, |m| format!("upsert reservation: {m}")))?;
        }
        for ev in &snapshot.canonization_events {
            let from_status = enum_to_text(&ev.from_status, "from_status")?;
            let to_status = enum_to_text(&ev.to_status, "to_status")?;
            sqlx::query(
                "INSERT INTO canonization_events (\
                     id, session_id, node_id, from_status, to_status, blast_radius, \
                     last_demotion_time, occurred_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT (id) DO NOTHING",
            )
            .bind(ev.id.0.to_string())
            .bind(&ev.session_id.0)
            .bind(ev.node_id.0.to_string())
            .bind(from_status)
            .bind(to_status)
            .bind(ev.blast_radius)
            .bind(ev.last_demotion_time.map(ts_to_text))
            .bind(ts_to_text(ev.occurred_at))
            .execute(&mut *tx)
            .await
            .map_err(|e| map_write_err(e, |m| format!("append canonization event: {m}")))?;
        }
        // J3 write intents ride the seed for adapter parity: MemoryStore's
        // seed stores the whole snapshot, so dropping them here would make a
        // seeded-then-loaded session differ by adapter.
        for intent in &snapshot.write_intents {
            put_write_intent(&mut *tx, intent).await?;
        }
        tx.commit()
            .await
            .map_err(|e| map_write_err(e, |m| format!("commit seed transaction: {m}")))?;
        Ok(())
    }

    pub(super) async fn upsert_flush_stats(
        &self,
        session: &SessionId,
        stats: &SessionFlushStats,
    ) -> Result<(), StoreError> {
        // Upsert the whole row so re-publishes converge (idempotency, same
        // contract as `flush`). Only the writer's FlushTask calls this;
        // readers only read. `updated_at` is stamped from the store clock
        // (strftime), matching the SQLite TIMESTAMPTZ-as-TEXT convention.
        //
        // #23: not for an erased session. A fenced writer's flush task can
        // still publish in the window before its heartbeat sees the tombstone,
        // and an unguarded upsert would put a `session_stats` row back after
        // the erase. Stats carry no content, but "everything keyed to the
        // session goes" is the contract, so the tombstone suppresses it.
        sqlx::query(
            "INSERT INTO session_stats (session_id, flush_lag_ms, log_depth, updated_at) \
             SELECT ?1, ?2, ?3, strftime('%Y-%m-%dT%H:%M:%fZ','now') \
             WHERE NOT EXISTS (SELECT 1 FROM session_leases \
                               WHERE session_id = ?1 AND holder = ?4) \
             ON CONFLICT (session_id) DO UPDATE SET \
               flush_lag_ms = excluded.flush_lag_ms, \
               log_depth = excluded.log_depth, \
               updated_at = excluded.updated_at",
        )
        .bind(&session.0)
        .bind(stats.flush_lag_ms as i64)
        .bind(stats.log_depth as i64)
        .bind(ERASED_HOLDER)
        .execute(self.pool())
        .await
        .map_err(|e| db_err("write flush stats", e))?;
        Ok(())
    }

    pub(super) async fn select_flush_stats(
        &self,
        session: &SessionId,
    ) -> Result<Option<SessionFlushStats>, StoreError> {
        let row =
            sqlx::query("SELECT flush_lag_ms, log_depth FROM session_stats WHERE session_id = ?1")
                .bind(&session.0)
                .fetch_optional(self.pool())
                .await
                .map_err(|e| db_err("read flush stats", e))?;
        let Some(row) = row else {
            return Ok(None);
        };
        let flush_lag_ms: i64 = row
            .try_get("flush_lag_ms")
            .map_err(|e| db_err("read flush stats: flush_lag_ms", e))?;
        let log_depth: i64 = row
            .try_get("log_depth")
            .map_err(|e| db_err("read flush stats: log_depth", e))?;
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
        if batch.mutations.is_empty() {
            return Ok(());
        }
        // Replay in batch order — the graph contract (§2.4 / drain_log) says
        // chronological order is the order, and stores MUST NOT re-sort.
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(|e| map_write_err(e, |m| format!("begin flush transaction: {m}")))?;

        let mut sessions: HashSet<String> = HashSet::new();
        for m in &batch.mutations {
            match m {
                Mutation::UpsertNode { node } => {
                    sessions.insert(node.session_id().0.clone());
                }
                Mutation::UpsertEdge { edge } => {
                    sessions.insert(edge.session_id.0.clone());
                }
                Mutation::CanonizationTransition { event } => {
                    sessions.insert(event.session_id.0.clone());
                }
                Mutation::SetRootGoal { session_id, .. } => {
                    sessions.insert(session_id.0.clone());
                }
                Mutation::SetEmbedding { session_id, .. } => {
                    sessions.insert(session_id.0.clone());
                }
                Mutation::PutWriteIntent { intent } => {
                    sessions.insert(intent.session_id.0.clone());
                }
                Mutation::ConsumeWriteIntent { session_id, .. }
                | Mutation::RecordAccess { session_id, .. } => {
                    sessions.insert(session_id.0.clone());
                }
                Mutation::DeleteNode { .. } | Mutation::DeleteEdge { .. } => {}
            }
        }
        self.ensure_sessions(&mut *tx, &sessions, batch.mutation_epoch, batch.gc_mark)
            .await?;

        // The fenced set is the stamped set plus the owning session of every
        // row a `DeleteNode`/`DeleteEdge` will remove. Those mutations name no
        // session, so without this a delete-only batch (a GC sweep) skipped
        // the gate entirely and a writer that had lost its lease could delete
        // rows in a session another writer now holds. Resolved here, inside
        // the transaction and before any delete runs; a row that is already
        // gone resolves nothing and its delete is a no-op. The stamp set above
        // is deliberately unchanged (see `ensure_sessions`).
        let mut fenced = sessions.clone();
        fenced.extend(deleted_row_sessions(&mut *tx, &batch.mutations).await?);

        // Fencing-token gate (#1): reject a stale/missing token for every
        // session the batch touches, INSIDE the same transaction as the writes
        // (atomic with them — a takeover cannot slip between the check and the
        // commit; on rejection the `?` drops `tx`, rolling the batch back). A
        // session with a lease row (current_token >= 1) must present a token
        // that is current; an unleased session has no row and passes (seed /
        // fixture parity).
        for sid in &fenced {
            let lease: Option<(i64, String)> = sqlx::query_as(LEASE_FENCE_SQL)
                .bind(sid)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| db_err("flush: read lease token", e))?;
            if let Some((cur, holder)) = lease {
                let cur = u64::try_from(cur).map_err(|_| {
                    StoreError::Invariant(format!("session {sid}: negative lease current_token"))
                })?;
                check_fence(sid, token, cur, &holder)?;
            }
        }

        // Same planned-statement replay as the Cockroach adapter (L82-1). There
        // is no network here, so this is not the latency fix it is there — it is
        // kept identical on purpose. `store::batch`'s deduplication and
        // canonization-column rules are subtle enough that they need a real SQL
        // engine executing them in CI, and this is the adapter that can.
        for step in plan_flush(&batch.mutations, BULK_LIMITS) {
            apply_step(&mut *tx, &step).await?;
        }

        tx.commit()
            .await
            .map_err(|e| map_write_err(e, |m| format!("commit flush transaction: {m}")))?;
        Ok(())
    }

    pub(super) async fn record_canonization_event(
        &self,
        event: &CanonizationEvent,
        token: Option<u64>,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool().begin().await.map_err(|e| {
            map_write_err(e, |m| format!("begin record_canonization transaction: {m}"))
        })?;
        // Fencing-token gate (#1): this durable write path HAD no lease check
        // at all — the canon task bypassed `lease_lost`. Check the token inside
        // this transaction, atomically with the write (rolls back on `?`).
        let lease: Option<(i64, String)> = sqlx::query_as(LEASE_FENCE_SQL)
            .bind(&event.session_id.0)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| db_err("record_canonization: read lease token", e))?;
        if let Some((cur, holder)) = lease {
            let cur = u64::try_from(cur).map_err(|_| {
                StoreError::Invariant(format!(
                    "session {}: negative lease current_token",
                    event.session_id
                ))
            })?;
            check_fence(&event.session_id.0, token, cur, &holder)?;
        }
        apply_canonization_transition(&mut *tx, event).await?;
        tx.commit().await.map_err(|e| {
            map_write_err(e, |m| {
                format!("commit record_canonization transaction: {m}")
            })
        })?;
        Ok(())
    }
}

impl SqliteStore {
    /// Erase every row keyed to `session` and leave the tombstone (#23; see
    /// `store::erase`). One transaction, begun `IMMEDIATE` so the write lock
    /// is held from the lease read on: the gate's decision, the tombstone and
    /// the deletes cannot interleave with a flush, an acquire or a refresh
    /// from another connection or process. A failure at any step (`hook`
    /// included) drops the transaction and leaves the session exactly as it
    /// was, so a rerun starts from the same state and completes.
    pub(super) async fn erase(
        &self,
        session: &SessionId,
        eraser: &LeaseHolder,
        hook: EraseStepHook<'_>,
    ) -> Result<EraseOutcome, StoreError> {
        crate::store::lease::refuse_reserved_holder(&eraser.token())?;
        let mut tx = self
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|e| map_write_err(e, |m| format!("begin erase transaction: {m}")))?;
        let prior: Option<(String, bool)> = sqlx::query_as(
            "SELECT holder, expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now') \
             FROM session_leases WHERE session_id = ?1",
        )
        .bind(&session.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| db_err("erase_session: read lease", e))?;
        let eraser_token = eraser.token();
        let gate = erase_gate(
            prior.as_ref().map(|(holder, live)| PriorLease {
                holder,
                live: *live,
            }),
            &eraser_token,
        );
        let EraseGate::Proceed { replaces_lease } = gate else {
            return held(&mut tx, session).await;
        };
        // The write lock is ours, so the guard cannot disagree with the gate;
        // if it ever did, report the holder rather than delete under it.
        let Some(fence_token) = write_erase_tombstone(&mut tx, session, &eraser_token).await?
        else {
            return held(&mut tx, session).await;
        };

        let mut removed = EraseCounts {
            vectors: count_session_vectors(&mut tx, session).await?,
            leases: u64::from(replaces_lease),
            ..Default::default()
        };
        hook("vectors")?;
        for (table, sql) in ERASE_STATEMENTS {
            let n = delete_session_rows(&mut tx, table, sql, session).await?;
            removed.add_table(table, n)?;
            hook(table)?;
        }
        tx.commit()
            .await
            .map_err(|e| map_write_err(e, |m| format!("commit erase transaction: {m}")))?;
        Ok(EraseOutcome::Erased(EraseReport::new(
            session.clone(),
            removed,
            fence_token,
        )))
    }
}

/// The live lease that refused an erase, read inside the erase transaction.
/// The caller returns it; dropping the transaction rolls back nothing, since
/// nothing was written.
async fn held(
    tx: &mut sqlx::SqliteConnection,
    session: &SessionId,
) -> Result<EraseOutcome, StoreError> {
    let row: LeaseRowText = sqlx::query_as(LEASE_ROW_SQL)
        .bind(&session.0)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| db_err("erase_session: read live lease", e))?;
    let current = lease_info_from_text(row)?;
    let age = (chrono::Utc::now() - current.acquired_at)
        .to_std()
        .unwrap_or(std::time::Duration::ZERO);
    Ok(EraseOutcome::Held { current, age })
}

/// The fence's read of a session's lease row: the token and the holder, so
/// [`check_fence`] can refuse an erasure tombstone whatever the token (#23
/// review H1). Runs inside the write transaction.
const LEASE_FENCE_SQL: &str =
    "SELECT current_token, holder FROM session_leases WHERE session_id = ?1";

/// Ids per lookup statement. Each id is bound once and referenced by number in
/// every arm, so this is also the bind count; well under SQLite's historical
/// 999-variable limit.
const DELETE_LOOKUP_CHUNK: usize = 400;

/// `?1, ?2, ..., ?n`, for an `IN (...)` list over numbered binds.
fn numbered_placeholders(n: usize) -> String {
    (1..=n)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Owning sessions of the rows the batch's deletes will remove, read before
/// any of them runs. Mirrors the delete statements exactly: a `DeleteNode`
/// removes the interaction or concept with that id plus every edge incident
/// to it (`write_rows::delete_node`); a `DeleteEdge` removes the edge row.
/// One query per chunk of ids rather than one per id.
async fn deleted_row_sessions(
    tx: &mut sqlx::SqliteConnection,
    mutations: &[Mutation],
) -> Result<HashSet<String>, StoreError> {
    let (nodes, edges) = batch_deleted_ids(mutations);
    let nodes: Vec<String> = nodes.iter().map(|id| id.0.to_string()).collect();
    let edges: Vec<String> = edges.iter().map(|id| id.0.to_string()).collect();
    let mut out = HashSet::new();
    for chunk in nodes.chunks(DELETE_LOOKUP_CHUNK) {
        let ids = numbered_placeholders(chunk.len());
        let sql = format!(
            "SELECT session_id FROM interactions WHERE id IN ({ids}) \
             UNION SELECT session_id FROM concepts WHERE id IN ({ids}) \
             UNION SELECT session_id FROM edges \
                 WHERE source IN ({ids}) OR target IN ({ids}) OR id IN ({ids})"
        );
        let mut query = sqlx::query_scalar::<_, String>(&sql);
        for id in chunk {
            query = query.bind(id);
        }
        let rows = query
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| db_err("flush: resolve deleted node session", e))?;
        out.extend(rows);
    }
    for chunk in edges.chunks(DELETE_LOOKUP_CHUNK) {
        let sql = format!(
            "SELECT session_id FROM edges WHERE id IN ({})",
            numbered_placeholders(chunk.len())
        );
        let mut query = sqlx::query_scalar::<_, String>(&sql);
        for id in chunk {
            query = query.bind(id);
        }
        let rows = query
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| db_err("flush: resolve deleted edge session", e))?;
        out.extend(rows);
    }
    Ok(out)
}
