//! Write statements for the Postgres-wire family: one function per planned
//! flush step or mutation kind (multi-row upserts, write intents, read
//! accesses, deletes, session-column writes with the legacy-embedding
//! quarantine, canonization transitions). Statement text and query builders
//! are in `sql.rs`.
//!
//! **Transaction rule.** Every function here runs on a connection the caller
//! hands it and never begins, commits or rolls back. The transaction owners
//! are in `persistence.rs` (`flush`, `seed`, `record_canonization`), each
//! inside a `tx_retry` body; an error returned from here propagates with `?`,
//! drops the caller's transaction and rolls the whole batch back.

use super::codec::{backend, canonization_status_sql};
use super::sql::{
    access_update_query, concept_upsert_query, consume_write_intents_update, edge_upsert_query,
    interaction_upsert_query, put_write_intents_upsert, DialectSql, DELETE_EDGE_SQL,
    DELETE_NODE_CONCEPTS_SQL, DELETE_NODE_EDGES_SQL, DELETE_NODE_INTERACTIONS_SQL,
    INSERT_CANONIZATION_EVENT_SQL, QUARANTINE_LEGACY_EMBEDDINGS_SQL, SET_ROOT_GOAL_SQL,
    UPDATE_CONCEPT_STATUS_SQL,
};
use super::sql::{COUNT_SESSION_VECTORS_SQL, ERASE_TOMBSTONE_SQL};
use crate::store::batch::{AccessUpdate, ConceptRow, FlushStep};
use crate::store::map_write_err;
use crate::store::vector::encode_vector;
use crate::types::{
    CanonizationEvent, DemotionReason, Edge, Interaction, Mutation, Node, SessionId, StoreError,
};

/// Multi-row upsert of one planned [`FlushStep::Interactions`] chunk (L82-1).
///
/// `rows` is already deduplicated on `id` and capped at
/// [`BULK_LIMITS`](super::persistence::BULK_LIMITS)`.interactions`.
pub(super) async fn bulk_upsert_interactions(
    tx: &mut sqlx::PgConnection,
    rows: &[&Interaction],
) -> Result<(), StoreError> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut check = sqlx::QueryBuilder::<sqlx::Postgres>::new(
        "SELECT 1 FROM retired_aliases WHERE alias_id IN (",
    );
    let mut ids = check.separated(", ");
    for row in rows {
        ids.push_bind(row.id.0);
    }
    ids.push_unseparated(") UNION ALL SELECT 1 FROM concept_names WHERE node_id IN (");
    let mut named_ids = check.separated(", ");
    for row in rows {
        named_ids.push_bind(row.id.0);
    }
    named_ids.push_unseparated(") LIMIT 1");
    if check
        .build()
        .fetch_optional(&mut *tx)
        .await
        .map_err(backend)?
        .is_some()
    {
        return Err(StoreError::Invariant(
            "a retired alias id cannot be reused".into(),
        ));
    }
    interaction_upsert_query(rows)
        .build()
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("upsert interaction: {m}")))?;
    Ok(())
}

/// Multi-row upsert of one planned [`FlushStep::Concepts`] chunk (L82-1).
///
/// Each row's three canonization columns come from its
/// [`ConceptRow::canonization`], **not** from `row.concept` — see [`ConceptRow`]
/// for why a deduplicated row splits them.
///
/// Vectors are encoded up front because `push_values`' closure cannot fail.
pub(super) async fn bulk_upsert_concepts(
    tx: &mut sqlx::PgConnection,
    rows: &[ConceptRow<'_>],
    sql: &DialectSql,
) -> Result<(), StoreError> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut embeddings: Vec<Option<String>> = Vec::with_capacity(rows.len());
    check_named_concept_keys(&mut *tx, rows).await?;
    for r in rows {
        if let Some(source) = &r.concept.embedding_source {
            source.check_writable(r.concept.id)?;
        }
        embeddings.push(match &r.concept.embedding {
            Some(v) => Some(encode_vector(v)?),
            None => None,
        });
    }

    concept_upsert_query(rows, &embeddings, sql.vector_cast)
        .build()
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("upsert concept: {m}")))?;
    Ok(())
}

/// Multi-row upsert of one planned [`FlushStep::Edges`] chunk (L82-1).
///
/// `rows` is already deduplicated on the **natural** key
/// `(source, target, edge_type)` — the conflict target below — because two rows
/// colliding there in one statement is an error, not a last-write-wins.
pub(super) async fn bulk_upsert_edges(
    tx: &mut sqlx::PgConnection,
    rows: &[&Edge],
) -> Result<(), StoreError> {
    if rows.is_empty() {
        return Ok(());
    }
    edge_upsert_query(rows)
        .build()
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("upsert edge: {m}")))?;
    Ok(())
}

/// Multi-row upsert of one planned [`FlushStep::PutIntents`] chunk (J3/F4).
///
/// `intents` is already deduplicated on `(session_id, receipt)` and capped at
/// [`crate::store::batch::INTENT_BATCH`].
pub(super) async fn bulk_put_write_intents(
    tx: &mut sqlx::PgConnection,
    intents: &[&crate::types::WriteIntent],
) -> Result<(), StoreError> {
    if intents.is_empty() {
        return Ok(());
    }
    let payloads: Vec<String> = intents
        .iter()
        .map(|i| {
            i.stored_payload()
                .map_err(|e| backend(format!("serialize write intent payload: {e}")))
        })
        .collect::<Result<_, _>>()?;
    put_write_intents_upsert(intents, &payloads)
        .build()
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("put write intent: {m}")))?;
    Ok(())
}

/// Multi-row `write_intents` consume of one planned [`FlushStep::ConsumeIntents`]
/// chunk (J3/F4): one UPDATE marking each receipt consumed, then the lazy
/// retention purge — one DELETE per distinct session in the chunk, clocked by
/// the chunk's oldest `consumed_at` minus the retention window.
pub(super) async fn bulk_consume_write_intents(
    tx: &mut sqlx::PgConnection,
    consumes: &[(
        &crate::types::SessionId,
        &str,
        &crate::types::WriteIntentOutcome,
    )],
) -> Result<(), StoreError> {
    if consumes.is_empty() {
        return Ok(());
    }
    consume_write_intents_update(consumes)
        .build()
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("consume write intent: {m}")))?;

    // Lazy retention purge (J3-R2R-5), clocked by the chunk's oldest
    // `consumed_at` minus the retention window, one DELETE per distinct session.
    let cutoff = consumes
        .iter()
        .map(|c| c.2.consumed_at)
        .min()
        .expect("consume batch is non-empty")
        - chrono::Duration::from_std(crate::types::WRITE_INTENT_RETENTION)
            .map_err(|e| backend(format!("retention duration out of range: {e}")))?;
    let sessions: std::collections::HashSet<&crate::types::SessionId> =
        consumes.iter().map(|c| c.0).collect();
    for session in sessions {
        sqlx::query(
            "DELETE FROM write_intents \
             WHERE session_id = $1 AND consumed_at IS NOT NULL AND consumed_at < $2",
        )
        .bind(session.0.as_str())
        .bind(cutoff)
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("purge consumed write intents: {m}")))?;
    }
    Ok(())
}

/// Apply one planned [`FlushStep`].
pub(super) async fn apply_step(
    tx: &mut sqlx::PgConnection,
    step: &FlushStep<'_>,
    sql: &DialectSql,
) -> Result<(), StoreError> {
    match step {
        FlushStep::Interactions(rows) => bulk_upsert_interactions(&mut *tx, rows).await,
        FlushStep::Concepts(rows) => bulk_upsert_concepts(&mut *tx, rows, sql).await,
        FlushStep::Edges(rows) => bulk_upsert_edges(&mut *tx, rows).await,
        FlushStep::Single(m) => apply_single(&mut *tx, m, sql).await,
        FlushStep::PutIntents(intents) => bulk_put_write_intents(&mut *tx, intents).await,
        FlushStep::ConsumeIntents(consumes) => bulk_consume_write_intents(&mut *tx, consumes).await,
        FlushStep::Accesses(rows) => bulk_update_accesses(&mut *tx, rows).await,
    }
}

/// Apply one chunk of read accesses (issue #30) as ONE narrow `UPDATE` of the
/// two access columns: no embedding rewrite, so on PostgreSQL the row update
/// changes no indexed column and is eligible for a HOT update, and neither
/// engine re-touches the vector index for a read. Existing rows only.
pub(super) async fn bulk_update_accesses(
    tx: &mut sqlx::PgConnection,
    rows: &[AccessUpdate<'_>],
) -> Result<(), StoreError> {
    if rows.is_empty() {
        return Ok(());
    }
    access_update_query(rows)
        .build()
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("record accesses: {m}")))?;
    Ok(())
}

/// Apply one mutation the planner could not bulk — a deletion, a canonization
/// transition, or a session-column write.
///
/// Every one of these can *observe* a row an upsert may have written, which is
/// exactly why [`plan_flush`](crate::store::batch::plan_flush) emits them alone and in place (see
/// `store::batch`). The upsert arms are unreachable for the same reason, but
/// they are handled rather than `unreachable!()`d: a planner change must not be
/// able to turn into a panic inside a flush.
pub(super) async fn apply_single(
    tx: &mut sqlx::PgConnection,
    m: &Mutation,
    sql: &DialectSql,
) -> Result<(), StoreError> {
    match m {
        Mutation::UpsertNode {
            node: Node::Interaction(i),
        } => bulk_upsert_interactions(&mut *tx, &[i]).await?,
        Mutation::UpsertNode {
            node: Node::Concept(c),
        } => bulk_upsert_concepts(&mut *tx, &[ConceptRow::new(c)], sql).await?,
        Mutation::UpsertEdge { edge } => bulk_upsert_edges(&mut *tx, &[edge]).await?,
        Mutation::SetConceptName { name } => set_concept_name(&mut *tx, name).await?,
        Mutation::RetireAlias { retired } => retire_alias(&mut *tx, retired).await?,
        Mutation::DeleteNode { id } => {
            // Explicit incident-edge cleanup: edges carry no FK on source/target
            // (spec §4); delete the node row from both node tables (interaction
            // deletes are unreachable under the graph contract — see module doc).
            sqlx::query(DELETE_NODE_EDGES_SQL)
                .bind(id.0)
                .execute(&mut *tx)
                .await
                .map_err(|e| map_write_err(e, |m| format!("delete node edges: {m}")))?;
            sqlx::query(DELETE_NODE_CONCEPTS_SQL)
                .bind(id.0)
                .execute(&mut *tx)
                .await
                .map_err(|e| map_write_err(e, |m| format!("delete node concepts: {m}")))?;
            sqlx::query(DELETE_NODE_INTERACTIONS_SQL)
                .bind(id.0)
                .execute(&mut *tx)
                .await
                .map_err(|e| map_write_err(e, |m| format!("delete node interactions: {m}")))?;
        }
        Mutation::DeleteEdge { id } => {
            sqlx::query(DELETE_EDGE_SQL)
                .bind(id.0)
                .execute(&mut *tx)
                .await
                .map_err(|e| map_write_err(e, |m| format!("delete edge: {m}")))?;
        }
        Mutation::CanonizationTransition { event } => {
            apply_canonization(&mut *tx, event).await?;
        }
        Mutation::SetRootGoal { session_id, goal } => {
            let encoded = goal
                .as_ref()
                .map(serde_json::to_string)
                .transpose()
                .map_err(|e| backend(format!("serialize root_goal: {e}")))?;
            let res = sqlx::query(SET_ROOT_GOAL_SQL)
                .bind(session_id.as_str())
                .bind(encoded.as_deref())
                .execute(&mut *tx)
                .await
                .map_err(|e| map_write_err(e, |m| format!("set root_goal: {m}")))?;
            if res.rows_affected() == 0 {
                return Err(StoreError::NotFound(format!(
                    "sessions row for {session_id} while setting root_goal"
                )));
            }
        }
        Mutation::SetEmbedding {
            session_id,
            embedding,
        } => {
            if embedding.is_some() {
                sqlx::query(QUARANTINE_LEGACY_EMBEDDINGS_SQL)
                    .bind(session_id.as_str())
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| {
                        map_write_err(e, |m| format!("quarantine legacy embeddings: {m}"))
                    })?;
            }
            let res = sqlx::query(sql.set_embedding.clone())
                .bind(session_id.as_str())
                .bind(embedding.as_ref().map(|e| e.kind.as_str()))
                .bind(embedding.as_ref().and_then(|e| e.model.as_deref()))
                .bind(
                    embedding
                        .as_ref()
                        .map(|e| i64::try_from(e.dim))
                        .transpose()
                        .map_err(|_| {
                            StoreError::Invariant(format!(
                                "embedding dimension does not fit i64 for {session_id}"
                            ))
                        })?,
                )
                .execute(&mut *tx)
                .await
                .map_err(|e| map_write_err(e, |m| format!("set embedding: {m}")))?;
            if res.rows_affected() == 0 {
                return Err(StoreError::NotFound(format!(
                    "sessions row for {session_id} while setting embedding"
                )));
            }
        }
        Mutation::PutWriteIntent { intent } => {
            put_write_intent(&mut *tx, intent).await?;
        }
        Mutation::ConsumeWriteIntent {
            session_id,
            receipt,
            outcome,
        } => {
            consume_write_intent(&mut *tx, session_id, receipt, outcome).await?;
        }
        Mutation::RecordAccess {
            session_id,
            id,
            access_count,
            last_accessed,
        } => {
            let row = AccessUpdate {
                session_id,
                id: *id,
                access_count: *access_count,
                last_accessed: *last_accessed,
            };
            bulk_update_accesses(&mut *tx, &[row]).await?;
        }
    }
    Ok(())
}

/// Upsert one durable write intent (J3). Keyed by (session, receipt); a re-put
/// replaces the row, matching the SQLite and memory adapters.
pub(super) async fn put_write_intent(
    tx: &mut sqlx::PgConnection,
    intent: &crate::types::WriteIntent,
) -> Result<(), StoreError> {
    let payload = intent
        .stored_payload()
        .map_err(|e| backend(format!("serialize write intent payload: {e}")))?;
    sqlx::query(
        "INSERT INTO write_intents \
             (session_id, receipt, agent, interaction_id, lane_seq, issued_ms, payload, \
              created_at, consumed_at, outcome_tag, outcome_summary) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) \
         ON CONFLICT (session_id, receipt) DO UPDATE SET \
             agent = excluded.agent, \
             interaction_id = excluded.interaction_id, \
             lane_seq = excluded.lane_seq, \
             issued_ms = excluded.issued_ms, \
             payload = excluded.payload, \
             created_at = excluded.created_at, \
             consumed_at = excluded.consumed_at, \
             outcome_tag = excluded.outcome_tag, \
             outcome_summary = excluded.outcome_summary",
    )
    .bind(intent.session_id.as_str())
    .bind(&intent.receipt)
    .bind(intent.agent.0.as_str())
    .bind(intent.interaction.0)
    .bind(i64::try_from(intent.lane_seq).unwrap_or(i64::MAX))
    .bind(intent.issued_ms)
    .bind(payload)
    .bind(intent.created_at)
    .bind(intent.outcome.as_ref().map(|o| o.consumed_at))
    .bind(intent.outcome.as_ref().map(|o| o.tag.clone()))
    .bind(intent.outcome.as_ref().map(|o| o.summary.clone()))
    .execute(&mut *tx)
    .await
    .map_err(|e| map_write_err(e, |m| format!("put write intent: {m}")))?;
    Ok(())
}

/// Mark one intent consumed with its outcome, then purge consumed rows older
/// than [`crate::types::WRITE_INTENT_RETENTION`] — clocked by the mutation's
/// own `consumed_at`. Consuming an absent receipt is a no-op (idempotent
/// replay, same as the canonization dedupe). A settled image intent's payload
/// is overwritten with [`crate::types::SETTLED_IMAGE_INTENT_PAYLOAD`] in the
/// same statement (#22 review L1), and so is a settled cited derive's (#36
/// PR 4 review H2) and a settled cited action's (#19(b)).
pub(super) async fn consume_write_intent(
    tx: &mut sqlx::PgConnection,
    session_id: &SessionId,
    receipt: &str,
    outcome: &crate::types::WriteIntentOutcome,
) -> Result<(), StoreError> {
    sqlx::query(
        "UPDATE write_intents SET consumed_at = $1, outcome_tag = $2, outcome_summary = $3, \
             payload = CASE WHEN payload LIKE CAST($6 AS TEXT) \
                 OR payload LIKE CAST($8 AS TEXT) \
                 OR payload LIKE CAST($9 AS TEXT) \
                 OR payload LIKE CAST($10 AS TEXT) \
                 OR payload LIKE CAST($11 AS TEXT) \
                 OR payload LIKE CAST($12 AS TEXT) THEN CAST($7 AS TEXT) \
                 ELSE payload END \
         WHERE session_id = $4 AND receipt = $5",
    )
    .bind(outcome.consumed_at)
    .bind(&outcome.tag)
    .bind(&outcome.summary)
    .bind(session_id.as_str())
    .bind(receipt)
    .bind(crate::types::DERIVE_IMAGE_PAYLOAD_LIKE)
    .bind(crate::types::SETTLED_IMAGE_INTENT_PAYLOAD)
    .bind(crate::types::DERIVE_CITED_PAYLOAD_LIKE)
    .bind(crate::types::ACTION_CITED_PAYLOAD_LIKE)
    .bind(crate::types::REPO_SCOPED_PAYLOAD_LIKE)
    .bind(crate::types::DERIVE_NAMED_PAYLOAD_LIKE)
    .bind(crate::types::ACTION_NAMED_PAYLOAD_LIKE)
    .execute(&mut *tx)
    .await
    .map_err(|e| map_write_err(e, |m| format!("consume write intent: {m}")))?;
    let cutoff = outcome.consumed_at
        - chrono::Duration::from_std(crate::types::WRITE_INTENT_RETENTION)
            .map_err(|e| backend(format!("retention duration out of range: {e}")))?;
    sqlx::query(
        "DELETE FROM write_intents \
         WHERE session_id = $1 AND consumed_at IS NOT NULL AND consumed_at < $2",
    )
    .bind(session_id.as_str())
    .bind(cutoff)
    .execute(&mut *tx)
    .await
    .map_err(|e| map_write_err(e, |m| format!("purge consumed write intents: {m}")))?;
    Ok(())
}

/// Append the audit row; `false` when the id was already there (the
/// `ON CONFLICT (id) DO NOTHING` dedupe fired).
pub(super) async fn insert_canonization_event(
    tx: &mut sqlx::PgConnection,
    ev: &CanonizationEvent,
) -> Result<bool, StoreError> {
    ev.check_reason()?;
    let (reason, superseded_by) = DemotionReason::to_columns(ev.reason.as_ref());
    let res = sqlx::query(INSERT_CANONIZATION_EVENT_SQL)
        .bind(ev.id.0)
        .bind(&ev.session_id.0)
        .bind(ev.node_id.0)
        .bind(canonization_status_sql(ev.from_status))
        .bind(canonization_status_sql(ev.to_status))
        .bind(ev.blast_radius)
        .bind(ev.last_demotion_time)
        .bind(ev.occurred_at)
        .bind(reason)
        .bind(superseded_by.map(|by| by.0))
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("insert canonization event: {m}")))?;
    Ok(res.rows_affected() > 0)
}

/// Apply a canonization transition: append the audit event, then update the
/// concept row. Missing concept → `NotFound` (MemoryStore parity).
///
/// **F12 — the audit row is the idempotency key.** The evaluator dual-writes
/// (`record_canonization` immediately, the same transition again when the
/// write-behind log flushes), and the two are not ordered against each other:
/// a lagging flush of hop 1 landing after hop 2's immediate write would
/// otherwise *regress* the durable status, and a crash before hop 2's own
/// flush would leave the reload showing a status the audit already moved past
/// — after which the evaluator re-promotes under a fresh event id and the same
/// hop appears twice in the on-screen audit. So the INSERT goes first: if its
/// `ON CONFLICT (id) DO NOTHING` fires, this transition's effect is already in
/// the row and the UPDATE is skipped. Both statements share the caller's
/// transaction, so the ordering swap costs nothing on the first write.
///
/// **R2-1 — what makes "already in the row" true.** Skipping the UPDATE is
/// only sound while nothing else writes those three columns.
/// `UPSERT_CONCEPT_SQL` used to, from a possibly stale
/// `Mutation::UpsertNode` snapshot, so a batch shaped
/// `[UpsertNode(stale), CanonizationTransition(already recorded)]` left the
/// row regressed *and* the repair skipped. It no longer does — see
/// `UPSERT_CONCEPT_SQL` and `Mutation::UpsertNode`.
pub(super) async fn apply_canonization(
    tx: &mut sqlx::PgConnection,
    ev: &CanonizationEvent,
) -> Result<(), StoreError> {
    if !insert_canonization_event(tx, ev).await? {
        return Ok(());
    }
    let res = sqlx::query(UPDATE_CONCEPT_STATUS_SQL)
        .bind(ev.node_id.0)
        .bind(canonization_status_sql(ev.to_status))
        .bind(ev.blast_radius)
        .bind(&ev.session_id.0)
        .bind(ev.last_demotion_time)
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("apply canonization transition: {m}")))?;
    if res.rows_affected() == 0 {
        return Err(StoreError::NotFound(format!(
            "concept {} for canonization",
            ev.node_id
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Session erasure (#23). The transaction is `persistence.rs`'s `erase`.
// ---------------------------------------------------------------------------

/// Concepts of the session that carry an embedding, counted before they go.
pub(super) async fn count_session_vectors(
    tx: &mut sqlx::PgConnection,
    session: &SessionId,
) -> Result<u64, StoreError> {
    let n: i64 = sqlx::query_scalar(COUNT_SESSION_VECTORS_SQL)
        .bind(session.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("erase_session: count vectors: {m}")))?;
    Ok(u64::try_from(n).unwrap_or(0))
}

/// Run one [`ERASE_STATEMENTS`](super::sql::ERASE_STATEMENTS) DELETE; the
/// number of rows it removed.
pub(super) async fn delete_session_rows(
    tx: &mut sqlx::PgConnection,
    table: &str,
    sql: &'static str,
    session: &SessionId,
) -> Result<u64, StoreError> {
    let done = sqlx::query(sql)
        .bind(session.as_str())
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("erase_session: delete {table}: {m}")))?;
    Ok(done.rows_affected())
}

/// Write the erasure tombstone ([`ERASE_TOMBSTONE_SQL`]). `None` means the
/// guard was false: a live lease belongs to someone else.
pub(super) async fn write_erase_tombstone(
    tx: &mut sqlx::PgConnection,
    session: &SessionId,
    eraser: &str,
) -> Result<Option<u64>, StoreError> {
    let token: Option<i64> = sqlx::query_scalar(ERASE_TOMBSTONE_SQL)
        .bind(session.as_str())
        .bind(crate::store::erase::ERASED_HOLDER)
        .bind(crate::store::erase::tombstone_expires_at())
        .bind(eraser)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("erase_session: write tombstone: {m}")))?;
    token
        .map(|t| {
            u64::try_from(t).map_err(|_| {
                StoreError::Invariant(format!("session {session}: negative lease current_token"))
            })
        })
        .transpose()
}

/// Register a fixed citation identity; retries may repeat but never retarget it.
pub(super) async fn set_concept_name(
    tx: &mut sqlx::PgConnection,
    name: &crate::types::ConceptName,
) -> Result<(), StoreError> {
    write_concept_name(tx, name, false).await
}

/// Snapshot restore may carry a binding whose original target was collected.
#[cfg(feature = "fixtures")]
pub(super) async fn seed_concept_name(
    tx: &mut sqlx::PgConnection,
    name: &crate::types::ConceptName,
) -> Result<(), StoreError> {
    write_concept_name(tx, name, true).await
}

async fn write_concept_name(
    tx: &mut sqlx::PgConnection,
    name: &crate::types::ConceptName,
    allow_missing: bool,
) -> Result<(), StoreError> {
    crate::store::names::validate_name(name)?;
    if !allow_missing {
        let target_ok: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM concepts WHERE session_id = $1 AND id = $2) OR EXISTS(SELECT 1 FROM concept_names WHERE session_id = $1 AND name_key = $3 AND node_id = $2)")
            .bind(name.session_id.as_str()).bind(name.node_id.0).bind(&name.name_key)
            .fetch_one(&mut *tx).await.map_err(backend)?;
        if !target_ok {
            return Err(StoreError::Invariant(
                "new concept name must target an active concept in its own session".into(),
            ));
        }
    }
    let collision: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM concepts WHERE session_id = $1 AND canonical_key = $2 AND id <> $3)")
        .bind(name.session_id.as_str()).bind(&name.name_key).bind(name.node_id.0)
        .fetch_one(&mut *tx).await.map_err(backend)?;
    if collision {
        return Err(StoreError::Invariant(
            "concept name collides with an existing concept key".into(),
        ));
    }
    let result = sqlx::query(
        "INSERT INTO concept_names (session_id, name, name_key, node_id) VALUES ($1, $2, $3, $4) \
        ON CONFLICT (session_id, name_key) DO UPDATE SET name = concept_names.name \
        WHERE concept_names.node_id = excluded.node_id",
    )
    .bind(name.session_id.as_str())
    .bind(&name.name)
    .bind(&name.name_key)
    .bind(name.node_id.0)
    .execute(&mut *tx)
    .await
    .map_err(|e| map_write_err(e, |m| format!("register concept name: {m}")))?;
    if result.rows_affected() == 0 {
        return Err(StoreError::Invariant(
            "concept name target is immutable".into(),
        ));
    }
    Ok(())
}

/// One batched namespace check, including all rows before any are written.
async fn check_named_concept_keys(
    tx: &mut sqlx::PgConnection,
    rows: &[ConceptRow<'_>],
) -> Result<(), StoreError> {
    let mut qb =
        sqlx::QueryBuilder::<sqlx::Postgres>::new("WITH incoming(session_id, key, id) AS (");
    qb.push_values(rows, |mut b, row| {
        b.push_bind(row.concept.session_id.as_str())
            .push_unseparated("::TEXT")
            .push_bind(&row.concept.canonical_key)
            .push_unseparated("::TEXT")
            .push_bind(row.concept.id.0)
            .push_unseparated("::UUID");
    });
    qb.push(") SELECT 1 FROM incoming i JOIN concept_names n ON n.session_id = i.session_id AND n.name_key = i.key WHERE n.node_id <> i.id UNION ALL SELECT 1 FROM incoming i JOIN retired_aliases r ON r.alias_id = i.id LIMIT 1");
    if qb
        .build()
        .fetch_optional(&mut *tx)
        .await
        .map_err(backend)?
        .is_some()
    {
        return Err(StoreError::Invariant(
            "concept conflicts with a durable name or retired alias id".into(),
        ));
    }
    Ok(())
}

/// Preserve the complete retired alias record, idempotently and without rewriting history.
pub(super) async fn retire_alias(
    tx: &mut sqlx::PgConnection,
    retired: &crate::types::RetiredAlias,
) -> Result<(), StoreError> {
    crate::store::names::validate_retired_alias(retired)?;
    let payload = serde_json::to_string(retired)
        .map_err(|e| StoreError::Backend(format!("serialize retired alias: {e}")))?;
    let result = sqlx::query(
        "INSERT INTO retired_aliases (session_id, alias_id, payload) VALUES ($1, $2, $3) \
        ON CONFLICT (session_id, alias_id) DO NOTHING",
    )
    .bind(retired.session_id.as_str())
    .bind(retired.concept.id.0)
    .bind(&payload)
    .execute(&mut *tx)
    .await
    .map_err(|e| map_write_err(e, |m| format!("retire alias: {m}")))?;
    if result.rows_affected() == 0 {
        let old: String = sqlx::query_scalar(
            "SELECT payload FROM retired_aliases WHERE session_id = $1 AND alias_id = $2",
        )
        .bind(retired.session_id.as_str())
        .bind(retired.concept.id.0)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| StoreError::Backend(format!("read retired alias: {e}")))?;
        if serde_json::from_str::<serde_json::Value>(&old).ok()
            != serde_json::from_str::<serde_json::Value>(&payload).ok()
        {
            return Err(StoreError::Invariant(
                "retired alias history is immutable".into(),
            ));
        }
    }
    Ok(())
}
