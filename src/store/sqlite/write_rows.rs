//! Write statements for the SQLite adapter: one function per planned flush
//! step or mutation kind (multi-row upserts, deletes, session-column writes,
//! the vector-width write gate and its quarantine, canonization transitions,
//! read-access updates, durable write intents).
//!
//! **Transaction rule.** Every function here runs on a connection the caller
//! hands it and never begins, commits or rolls back. The transaction owners
//! are in `persistence.rs` (`flush`, `seed`, `record_canonization`); an error
//! returned from here propagates with `?` and drops the caller's transaction,
//! rolling the whole batch back.

use std::collections::HashMap;

use sqlx::Row;

use super::codec::{cutoff_text, db_err, enum_to_text, session_embedding_from_parts, ts_to_text};
use crate::store::batch::{AccessUpdate, ConceptRow, FlushStep};
use crate::store::map_write_err;
use crate::store::vector::encode_vector_blob;
use crate::types::{
    CanonizationEvent, Edge, Interaction, Mutation, Node, NodeId, SessionId, StoreError,
};

/// Apply one planned [`FlushStep`].
pub(super) async fn apply_step(
    tx: &mut sqlx::SqliteConnection,
    step: &FlushStep<'_>,
) -> Result<(), StoreError> {
    match step {
        FlushStep::Interactions(rows) => upsert_interactions(&mut *tx, rows).await,
        FlushStep::Concepts(rows) => upsert_concepts(&mut *tx, rows).await,
        FlushStep::Edges(rows) => upsert_edges(&mut *tx, rows).await,
        FlushStep::Single(m) => apply_single(&mut *tx, m).await,
        // Durable intents (J3/F4). SQLite is a local file — no network round-trip
        // per statement — so there is nothing to batch for and the existing
        // per-intent statements are both simpler and exactly the old behaviour.
        // The F4 win is Cockroach-specific; the planner's steps are the same here.
        FlushStep::PutIntents(intents) => {
            for intent in intents {
                put_write_intent(&mut *tx, intent).await?;
            }
            Ok(())
        }
        FlushStep::ConsumeIntents(consumes) => {
            for (session_id, receipt, outcome) in consumes {
                consume_write_intent(&mut *tx, session_id, receipt, outcome).await?;
            }
            Ok(())
        }
        FlushStep::Accesses(rows) => update_accesses(&mut *tx, rows).await,
    }
}

/// Apply one mutation the planner could not bulk. See the Cockroach adapter's
/// `apply_single` for why the upsert arms are handled rather than
/// `unreachable!()`d.
pub(super) async fn apply_single(
    tx: &mut sqlx::SqliteConnection,
    m: &Mutation,
) -> Result<(), StoreError> {
    match m {
        Mutation::UpsertNode {
            node: Node::Interaction(i),
        } => upsert_interactions(&mut *tx, &[i]).await?,
        Mutation::UpsertNode {
            node: Node::Concept(c),
        } => upsert_concepts(&mut *tx, &[ConceptRow::new(c)]).await?,
        Mutation::UpsertEdge { edge } => upsert_edges(&mut *tx, &[edge]).await?,
        Mutation::DeleteNode { id } => {
            delete_node(&mut *tx, *id).await?;
        }
        Mutation::DeleteEdge { id } => {
            sqlx::query("DELETE FROM edges WHERE id = ?")
                .bind(id.0.to_string())
                .execute(&mut *tx)
                .await
                .map_err(|e| map_write_err(e, |m| format!("delete edge: {m}")))?;
        }
        Mutation::CanonizationTransition { event } => {
            apply_canonization_transition(&mut *tx, event).await?;
        }
        Mutation::SetRootGoal { session_id, goal } => {
            set_root_goal(&mut *tx, session_id, goal.as_ref()).await?;
        }
        Mutation::SetEmbedding {
            session_id,
            embedding,
        } => {
            set_embedding(&mut *tx, session_id, embedding.as_ref()).await?;
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
            update_accesses(&mut *tx, &[row]).await?;
        }
    }
    Ok(())
}

/// The batched access update (issue #30) up to the `VALUES` list: SQLite names
/// a bare `VALUES` table's columns `column1..4`, so they are renamed in a
/// subquery. `push_values` emits the `VALUES` keyword itself.
pub(super) const UPDATE_ACCESSES_PREFIX_SQL: &str = "UPDATE concepts SET \
     access_count = MAX(concepts.access_count, v.access_count), \
     last_accessed = MAX(COALESCE(concepts.last_accessed, v.last_accessed), v.last_accessed) \
     FROM (SELECT column1 AS id, column2 AS session_id, column3 AS access_count, \
     column4 AS last_accessed FROM (";

/// Closes [`UPDATE_ACCESSES_PREFIX_SQL`]. The join names the session as well
/// as the id, so a row can only ever count against its own session.
pub(super) const UPDATE_ACCESSES_SUFFIX_SQL: &str =
    ")) AS v WHERE concepts.id = v.id AND concepts.session_id = v.session_id";

/// Apply one chunk of read accesses (issue #30) as ONE narrow `UPDATE`: the
/// two access columns only — no embedding rewrite — and **monotonic**, so a
/// replayed batch, or an access landing after a concept upsert that already
/// carried a higher count, can never lower what is stored. Existing rows only:
/// it never inserts.
///
/// `last_accessed` is TEXT in the fixed-width `ts_to_text` form (UTC, millis,
/// `Z`), so the string `MAX` is the chronological one. SQLite's multi-argument
/// `MAX` is NULL if any argument is, hence the `COALESCE` for a row never read.
pub(super) async fn update_accesses(
    tx: &mut sqlx::SqliteConnection,
    rows: &[AccessUpdate<'_>],
) -> Result<(), StoreError> {
    if rows.is_empty() {
        return Ok(());
    }
    update_accesses_query(rows)
        .build()
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("record accesses: {m}")))?;
    Ok(())
}

/// The statement [`update_accesses`] runs, built but not executed.
pub(super) fn update_accesses_query<'a>(
    rows: &'a [AccessUpdate<'a>],
) -> sqlx::QueryBuilder<'a, sqlx::Sqlite> {
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(UPDATE_ACCESSES_PREFIX_SQL);
    qb.push_values(rows.iter(), |mut b, r| {
        b.push_bind(r.id.0.to_string())
            .push_bind(r.session_id.0.as_str())
            .push_bind(r.access_count)
            .push_bind(ts_to_text(r.last_accessed));
    });
    qb.push(UPDATE_ACCESSES_SUFFIX_SQL);
    qb
}

/// Upsert one durable write intent (J3). Keyed by (session, receipt); a re-put
/// replaces the row, matching the memory adapter.
pub(super) async fn put_write_intent(
    tx: &mut sqlx::SqliteConnection,
    intent: &crate::types::WriteIntent,
) -> Result<(), StoreError> {
    let payload = serde_json::to_string(&intent.payload)
        .map_err(|e| StoreError::Backend(format!("serialize write intent payload: {e}")))?;
    sqlx::query(
        "INSERT INTO write_intents \
             (session_id, receipt, agent, interaction_id, lane_seq, issued_ms, payload, \
              created_at, consumed_at, outcome_tag, outcome_summary) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
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
    .bind(intent.interaction.0.to_string())
    .bind(i64::try_from(intent.lane_seq).unwrap_or(i64::MAX))
    .bind(intent.issued_ms)
    .bind(payload)
    .bind(ts_to_text(intent.created_at))
    .bind(intent.outcome.as_ref().map(|o| ts_to_text(o.consumed_at)))
    .bind(intent.outcome.as_ref().map(|o| o.tag.clone()))
    .bind(intent.outcome.as_ref().map(|o| o.summary.clone()))
    .execute(&mut *tx)
    .await
    .map_err(|e| map_write_err(e, |m| format!("put write intent: {m}")))?;
    Ok(())
}

/// Mark one intent consumed with its outcome, then purge consumed rows older
/// than [`crate::types::WRITE_INTENT_RETENTION`] — clocked by the mutation's
/// own `consumed_at`, so the adapter needs no clock. Consuming an absent
/// receipt is a no-op (the put may already be purged; replay is idempotent).
pub(super) async fn consume_write_intent(
    tx: &mut sqlx::SqliteConnection,
    session_id: &SessionId,
    receipt: &str,
    outcome: &crate::types::WriteIntentOutcome,
) -> Result<(), StoreError> {
    sqlx::query(
        "UPDATE write_intents SET consumed_at = ?, outcome_tag = ?, outcome_summary = ? \
         WHERE session_id = ? AND receipt = ?",
    )
    .bind(ts_to_text(outcome.consumed_at))
    .bind(&outcome.tag)
    .bind(&outcome.summary)
    .bind(session_id.as_str())
    .bind(receipt)
    .execute(&mut *tx)
    .await
    .map_err(|e| map_write_err(e, |m| format!("consume write intent: {m}")))?;
    let cutoff = cutoff_text(outcome.consumed_at, crate::types::WRITE_INTENT_RETENTION)?;
    sqlx::query(
        "DELETE FROM write_intents \
         WHERE session_id = ? AND consumed_at IS NOT NULL AND consumed_at < ?",
    )
    .bind(session_id.as_str())
    .bind(cutoff)
    .execute(&mut *tx)
    .await
    .map_err(|e| map_write_err(e, |m| format!("purge consumed write intents: {m}")))?;
    Ok(())
}

pub(super) async fn upsert_interactions(
    tx: &mut sqlx::SqliteConnection,
    rows: &[&Interaction],
) -> Result<(), StoreError> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "INSERT INTO interactions (id, session_id, agent_id, prompt_text, previous_id, created_at, event_time) ",
    );
    qb.push_values(rows.iter(), |mut b, i| {
        b.push_bind(i.id.0.to_string())
            .push_bind(i.session_id.0.clone())
            .push_bind(i.agent_id.0.clone())
            .push_bind(i.prompt_text.clone())
            .push_bind(i.previous_id.map(|id| id.0.to_string()))
            .push_bind(ts_to_text(i.created_at))
            .push_bind(i.event_time.map(ts_to_text));
    });
    qb.push(
        " ON CONFLICT (id) DO UPDATE SET \
             session_id = excluded.session_id, \
             agent_id = excluded.agent_id, \
             prompt_text = excluded.prompt_text, \
             previous_id = excluded.previous_id, \
             created_at = excluded.created_at, \
             event_time = excluded.event_time",
    );
    qb.build()
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("upsert interaction: {m}")))?;
    Ok(())
}

/// **The write gate for vector width** (F-R1-1).
///
/// Refuse a concept whose vector width disagrees with the session's durable
/// embedding contract, in the same transaction as the upsert that would store it.
/// This is what Cockroach's `VECTOR(1024)` DDL does for free
/// (`migrations/cockroach/001_init.sql`); SQLite's `concepts.embedding` is a
/// width-agnostic `BLOB`, so the adapter has to do it by hand or not at all.
///
/// # Why the write gate and not only the read check
///
/// [`super::vector_candidates::select_session_vectors`] detects a width-mismatched row, but detection is
/// terminal and session-wide: it returns on the first bad row, so **one** corrupt
/// concept makes the whole session's vector leg — and therefore
/// `recall::candidates::gather` and `hybrid::derive`, both of which propagate a
/// `Backend` error rather than degrading — fail permanently, until someone edits
/// the row by hand. `Concept::embedding`'s own contract is *"width = session
/// `EmbeddingContract::dim`"*, and before this gate nothing on the SQLite write
/// path enforced it: one public `GraphStore::flush` could durably poison a
/// session. Refusing the write costs the caller one batch; accepting it costs the
/// session its recall.
///
/// # What the contract is at the moment a concept is validated
///
/// The contract read here is the one **visible in `sessions` when this step
/// executes**, which makes the intra-batch ordering well defined rather than
/// incidental: [`crate::store::batch::plan_flush`] treats [`Mutation::SetEmbedding`] as a
/// barrier that drains every open bucket before it and is then emitted alone. So
/// within one [`MutationBatch`](crate::types::MutationBatch):
///
/// * concepts submitted **after** a `SetEmbedding` are validated against the width
///   that `SetEmbedding` just stamped — a batch that stamps `dim` and then upserts
///   concepts of that `dim` **passes** (this is the shape `seed_vectors` and every
///   real `hybrid::derive` flush use);
/// * concepts submitted **before** it are validated against the contract that was
///   durable when they were written — which is **not** necessarily the contract a
///   reader will interpret them under, because the later `SetEmbedding` can move
///   it. That gap is closed on the other side, in [`set_embedding`]: stamping a
///   contract NULLs every vector of a different width, so a concept validated
///   against a contract that has since changed width is erased rather than
///   orphaned (F-R2-1 — round 2 reproduced the orphan through one public `flush`
///   when the quarantine only fired over a NULL contract).
///
/// So the two halves together, and neither alone, give the property this gate is
/// for: **no vector whose width disagrees with the session contract can survive a
/// write through this adapter's `GraphStore` surface.** The gate refuses a mismatch
/// against a contract that already exists; the quarantine erases one that a contract
/// change would otherwise leave behind.
///
/// The scoping to the trait is deliberate, and it leaves **two** residuals (F-R3-1):
/// a hand-edited database, which no write-side rule can cover and which the read
/// path's per-row width check is the defence against; and `SqliteStore::seed`, the
/// adapter's other `sessions.embedding_dim` writer, which restamps the contract with
/// no quarantine at all — `#[cfg(feature = "fixtures")]`, absent from the trait, and
/// reached by no in-tree caller outside tests. Because it *upserts* where
/// `MemoryStore::seed` *replaces*, a second seed over a live session can leave the
/// first seed's vectors orphaned under the new width. Named rather than closed
/// because it is fixtures scaffolding, not a shipped path.
///
/// # A vector arriving with no contract stamped is accepted
///
/// Deliberate, and the one place SQLite cannot mirror Cockroach: Cockroach's DDL
/// width is a property of the *table*, so it refuses a wrong-width insert even
/// with no session contract. SQLite has no such number — with `embedding_kind` /
/// `embedding_dim` still NULL there is no authority to check against, and the
/// process-configured `vector_dim` is explicitly not one (it is a resolution-time
/// pin, see the module doc's "Width authority"). Accepting is safe because such a
/// vector is unreachable *and* cannot survive to become the fatal mismatch above:
/// the read path returns an empty pool while the contract is NULL, and
/// [`set_embedding`] NULLs every vector of a different width when it stamps —
/// which from a NULL contract means all of them. The width becomes enforceable
/// exactly when it becomes meaningful.
pub(super) async fn enforce_concept_vector_widths(
    tx: &mut sqlx::SqliteConnection,
    rows: &[ConceptRow<'_>],
) -> Result<(), StoreError> {
    // Cache per session: a batch normally touches one, and only vector-bearing
    // rows can violate anything, so a vector-free flush costs zero extra reads.
    let mut widths: HashMap<&str, Option<usize>> = HashMap::new();
    for r in rows {
        let Some(vector) = r.concept.embedding.as_ref() else {
            continue;
        };
        let sid = r.concept.session_id.0.as_str();
        if !widths.contains_key(sid) {
            let row = sqlx::query(
                "SELECT embedding_kind, embedding_model, embedding_dim \
                 FROM sessions WHERE session_id = ?",
            )
            .bind(sid)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| db_err("upsert concept: session contract", e))?;
            // Same classifier as `load_session` and the checked read, so a
            // kind-XOR-dim corrupt row is reported identically on all three paths
            // instead of being silently treated as unstamped here.
            let contract = match row {
                Some(row) => session_embedding_from_parts(
                    row.try_get(0)
                        .map_err(|e| db_err("upsert concept: session contract", e))?,
                    row.try_get(1)
                        .map_err(|e| db_err("upsert concept: session contract", e))?,
                    row.try_get(2)
                        .map_err(|e| db_err("upsert concept: session contract", e))?,
                    sid,
                )?,
                // `ensure_sessions` runs before every flush step, so a missing row
                // here means the session vanished mid-batch; treat it as unstamped
                // and let the upsert itself produce the real error.
                None => None,
            };
            widths.insert(sid, contract.map(|c| c.dim));
        }
        let Some(dim) = widths[sid] else {
            continue;
        };
        if vector.len() != dim {
            return Err(StoreError::Invariant(format!(
                "concept {} carries a {}-dimensional embedding but session {} stores \
                 vectors of {} — refusing the write: one width-mismatched row makes the \
                 session's entire vector leg fail on every read (re-embed the session or \
                 start a new one; `Concept::embedding` must match the session contract)",
                r.concept.id,
                vector.len(),
                sid,
                dim
            )));
        }
    }
    Ok(())
}

pub(super) async fn upsert_concepts(
    tx: &mut sqlx::SqliteConnection,
    rows: &[ConceptRow<'_>],
) -> Result<(), StoreError> {
    if rows.is_empty() {
        return Ok(());
    }
    // Before any encoding: a refusal must leave the transaction with nothing
    // written, and `?` here rolls the whole batch back (F-R1-1).
    enforce_concept_vector_widths(&mut *tx, rows).await?;
    let mut encoded: Vec<ConceptBinds> = Vec::with_capacity(rows.len());
    for r in rows {
        encoded.push(concept_binds(r)?);
    }
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "INSERT INTO concepts (\
             id, session_id, content, canonical_key, concept_type, origin_interaction, \
             origin_agent, created_at, access_count, last_accessed, gc_survived, \
             canonization_status, blast_radius, last_demotion_time, embedding, \
             chunk_group_id, human_confirmed) ",
    );
    qb.push_values(rows.iter().zip(encoded.iter()), |mut b, (r, enc)| {
        let c = r.concept;
        b.push_bind(c.id.0.to_string())
            .push_bind(c.session_id.0.clone())
            .push_bind(c.content.clone())
            .push_bind(c.canonical_key.clone())
            .push_bind(enc.concept_type.clone())
            .push_bind(c.origin_interaction.0.to_string())
            .push_bind(c.origin_agent.0.clone())
            .push_bind(ts_to_text(c.created_at))
            .push_bind(c.access_count)
            .push_bind(c.last_accessed.map(ts_to_text))
            .push_bind(c.gc_survived)
            .push_bind(enc.status.clone())
            .push_bind(r.canonization.blast_radius)
            .push_bind(r.canonization.last_demotion_time.map(ts_to_text))
            .push_bind(enc.embedding.clone())
            .push_bind(c.chunk_group_id.clone())
            .push_bind(c.human_confirmed);
    });
    // Conflict target is the `id` PRIMARY KEY. The partial unique index
    // (session_id, canonical_key) WHERE concept_type <> 'Observation' is NOT a
    // valid target (bare ON CONFLICT errors); legal duplicate Observation keys
    // (demote) never conflict with it, and a genuine duplicate non-Observation
    // key surfaces as an error (the graph tier already forbids it in RAM).
    //
    // R2-1: `canonization_status` / `blast_radius` / `last_demotion_time` are
    // in the INSERT column list (a brand-new row must carry them) but
    // deliberately **absent from the DO UPDATE SET list** — on an existing row
    // the canonization path is their only writer. Rationale on
    // `Mutation::UpsertNode`; the *values* bound above come from
    // `ConceptRow::canonization`, not from the concept, for the deduplication
    // reason spelled out on `store::batch::ConceptRow`.
    qb.push(
        " ON CONFLICT (id) DO UPDATE SET \
             session_id = excluded.session_id, \
             content = excluded.content, \
             canonical_key = excluded.canonical_key, \
             concept_type = excluded.concept_type, \
             origin_interaction = excluded.origin_interaction, \
             origin_agent = excluded.origin_agent, \
             created_at = excluded.created_at, \
             access_count = excluded.access_count, \
             last_accessed = excluded.last_accessed, \
             gc_survived = excluded.gc_survived, \
             embedding = excluded.embedding, \
             chunk_group_id = excluded.chunk_group_id, \
             human_confirmed = excluded.human_confirmed",
    );
    qb.build()
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("upsert concept: {m}")))?;
    Ok(())
}

pub(super) async fn upsert_edges(
    tx: &mut sqlx::SqliteConnection,
    rows: &[&Edge],
) -> Result<(), StoreError> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut types: Vec<String> = Vec::with_capacity(rows.len());
    for e in rows {
        types.push(enum_to_text(&e.edge_type, "edge_type")?);
    }
    let mut qb = sqlx::QueryBuilder::<sqlx::Sqlite>::new(
        "INSERT INTO edges (\
             id, session_id, source, target, edge_type, weight, reinforcements, \
             created_at, last_reinforced, event_time) ",
    );
    qb.push_values(rows.iter().zip(types.iter()), |mut b, (e, edge_type)| {
        b.push_bind(e.id.0.to_string())
            .push_bind(e.session_id.0.clone())
            .push_bind(e.source.0.to_string())
            .push_bind(e.target.0.to_string())
            .push_bind(edge_type.clone())
            .push_bind(e.weight)
            .push_bind(e.reinforcements)
            .push_bind(ts_to_text(e.created_at))
            .push_bind(ts_to_text(e.last_reinforced))
            .push_bind(e.event_time.map(ts_to_text));
    });
    // Natural-key preference (MemoryStore parity): the table-level
    // UNIQUE (source, target, edge_type) autoindexes and is a legal target.
    qb.push(
        " ON CONFLICT (source, target, edge_type) DO UPDATE SET \
             id = excluded.id, \
             session_id = excluded.session_id, \
             weight = excluded.weight, \
             reinforcements = excluded.reinforcements, \
             created_at = excluded.created_at, \
             last_reinforced = excluded.last_reinforced, \
             event_time = excluded.event_time",
    );
    qb.build()
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("upsert edge: {m}")))?;
    Ok(())
}

/// The fallible per-row encodings, done before `push_values`' infallible closure.
pub(super) struct ConceptBinds {
    concept_type: String,
    status: String,
    /// CON-8: the embedding is written for flush→load round-trip parity. Same
    /// wire form as Cockroach's VECTOR text literal (shared store::vector
    /// codec), stored in the BLOB column; never NULL for a present vector, NULL
    /// otherwise.
    embedding: Option<Vec<u8>>,
}

pub(super) fn concept_binds(r: &ConceptRow<'_>) -> Result<ConceptBinds, StoreError> {
    Ok(ConceptBinds {
        concept_type: enum_to_text(&r.concept.concept_type, "concept_type")?,
        status: enum_to_text(&r.canonization.status, "canonization_status")?,
        embedding: r
            .concept
            .embedding
            .as_ref()
            .map(|v| encode_vector_blob(v))
            .transpose()?,
    })
}

pub(super) async fn delete_node(
    tx: &mut sqlx::SqliteConnection,
    id: NodeId,
) -> Result<(), StoreError> {
    // MemoryStore parity: a node delete removes the node plus every incident
    // edge (edges carry no FK, so dangling edges would otherwise survive).
    let id_text = id.0.to_string();
    sqlx::query("DELETE FROM interactions WHERE id = ?")
        .bind(&id_text)
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("delete interaction: {m}")))?;
    sqlx::query("DELETE FROM concepts WHERE id = ?")
        .bind(&id_text)
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("delete concept: {m}")))?;
    sqlx::query("DELETE FROM edges WHERE source = ? OR target = ? OR id = ?")
        .bind(&id_text)
        .bind(&id_text)
        .bind(&id_text)
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("delete incident edges: {m}")))?;
    Ok(())
}

/// XP-8: persist a session's `root_goal`. The column already exists (both
/// schemas carry it, `seed` writes it) and the JSON encoding is `seed`'s
/// exactly, so a goal set through the mutation path and one seeded from a
/// snapshot are indistinguishable on reload. `ensure_sessions` has already
/// created the row, so a zero-row update means the session vanished mid-batch.
pub(super) async fn set_root_goal(
    tx: &mut sqlx::SqliteConnection,
    session: &SessionId,
    goal: Option<&serde_json::Value>,
) -> Result<(), StoreError> {
    let encoded = goal
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| StoreError::Backend(format!("serialize root_goal: {e}")))?;
    let res = sqlx::query("UPDATE sessions SET root_goal = ? WHERE session_id = ?")
        .bind(encoded.as_deref())
        .bind(&session.0)
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("set root_goal: {m}")))?;
    if res.rows_affected() == 0 {
        return Err(StoreError::NotFound(format!(
            "sessions row for {session} while setting root_goal"
        )));
    }
    Ok(())
}

/// Persist the embedding-space identity in the same ordered transaction as
/// concept vectors. A reload must never observe vectors without their contract.
///
/// # Stamping a contract quarantines every vector of a different width (F-R2-1)
///
/// The `UPDATE concepts SET embedding = NULL` below fires whenever the width
/// being stamped differs from the width durable *before* this statement —
/// `embedding_dim IS NOT ?` is SQLite's null-safe comparison, so it is true both
/// for an unstamped session (`embedding_dim IS NULL`, the original case) and for
/// a **restamp** from one width to another. Round 2 reproduced why the narrower
/// NULL-only predicate was not enough: a batch of
/// `SetEmbedding{4}`, `Concept{4-wide}`, `SetEmbedding{3}`, `Concept{3-wide}`
/// passes [`enforce_concept_vector_widths`] at every step — each concept really
/// does match the contract of its own moment — and still commits a 4-wide vector
/// under a `dim = 3` contract, which is the durable, session-wide,
/// permanent-until-hand-edited recall failure the gate exists to prevent. With
/// this predicate the second stamp NULLs the earlier vector, so the batch
/// self-heals instead: it is accepted, and the terminal state is a `dim = 3`
/// contract beside only 3-wide vectors. Same for the two-flush shape.
///
/// Together with the gate this closes the property across the trait: **no vector
/// whose width disagrees with the session contract can survive a write through this
/// adapter's `GraphStore` surface.** The gate refuses a mismatch against an existing
/// contract; this statement erases one that a contract change would otherwise
/// orphan.
///
/// Two residuals sit outside that surface (F-R3-1). The read path's per-row width
/// check remains the defence against an externally edited database, which no
/// write-side rule can cover. And `SqliteStore::seed` is a second
/// `sessions.embedding_dim` writer that this quarantine does not run: it restamps
/// the contract through `INSERT … ON CONFLICT (session_id) DO UPDATE SET …
/// embedding_dim = excluded.embedding_dim` with no quarantine, and because it
/// *upserts* where `MemoryStore::seed` *replaces*, concepts already in the session
/// but absent from the new snapshot are never revisited — so a second seed over a
/// live session can leave the first seed's vectors orphaned under the new width
/// (round-3 PROBE G reproduced exactly this terminal state through two `seed` calls
/// and no direct SQL). It is named rather than closed because the surface is
/// fixtures scaffolding: `seed` is `#[cfg(feature = "fixtures")]`, `fixtures` is off
/// both `default` and `ship`, `seed` is not on the `GraphStore` trait, and no
/// in-tree caller outside tests reaches it.
///
/// # Why *width*, and not any contract change
///
/// A kind or model change at the **same** width does **not** quarantine, and that
/// is deliberate rather than an omission:
///
/// * The graph tier already treats those cases differently on purpose.
///   `Graph::replace_embedding_with_operator_override` — the
///   `--allow-embedding-mismatch` writer attach path — *requires* equal widths,
///   refuses a `kind` change while any vector remains, and explicitly permits a
///   same-kind **model identifier rename** with the vectors left in place. Erasing
///   them here would destroy data on the one migration path built to keep it.
/// * Width is the only contract property this storage can enforce. A same-width
///   relabel leaves every BLOB decodable and every read correct; a width change
///   makes the stored bytes uninterpretable. Semantic space identity (kind/model)
///   is checked where it is knowable — `EmbeddingContract::ensure_compatible`, at
///   the graph tier and in `vector_candidates_checked` against the caller's
///   expected contract.
///
/// # Cockroach parity: deliberate divergence, with the reason
///
/// `cockroach.rs`'s `QUARANTINE_LEGACY_EMBEDDINGS_SQL` keeps the NULL-contract-only
/// predicate. That is a divergence, and it is sound because the shape it would
/// close cannot arise there: `concepts.embedding` is `VECTOR(1024)` in the DDL, so
/// every stored vector is exactly that wide or NULL, and a restamp to any other
/// width cannot produce a row that decodes to an unexpected width — it instead
/// makes the whole session refuse loudly at `check_embedding_dim` against the
/// DDL-parsed authority, before any row is read. SQLite's `BLOB` has no such
/// authority, which is why the adapter has to hold this line by hand. (The second
/// reason is honest rather than structural: this worktree has no Cockroach DSN, so
/// a change to that statement could not be executed, and an unrun SQL edit is
/// worse than a documented asymmetry.)
pub(super) async fn set_embedding(
    tx: &mut sqlx::SqliteConnection,
    session: &SessionId,
    embedding: Option<&crate::types::EmbeddingContract>,
) -> Result<(), StoreError> {
    let dim = embedding
        .map(|e| i64::try_from(e.dim))
        .transpose()
        .map_err(|_| {
            StoreError::Invariant(format!(
                "embedding dimension does not fit i64 for {session}"
            ))
        })?;
    if embedding.is_some() {
        // F-R2-1: `IS NOT` is SQLite's null-safe inequality, so this covers both
        // "no contract yet" (embedding_dim IS NULL) and "a different width was
        // durable a moment ago" — a restamp. Equal widths quarantine nothing, which
        // is what keeps a same-width model rename non-destructive. See the doc above.
        sqlx::query(
            "UPDATE concepts SET embedding = NULL WHERE session_id = ? AND EXISTS (\
             SELECT 1 FROM sessions WHERE session_id = ? \
             AND embedding_dim IS NOT ?)",
        )
        .bind(&session.0)
        .bind(&session.0)
        .bind(dim)
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("quarantine legacy embeddings: {m}")))?;
    }
    let res = sqlx::query(
        "UPDATE sessions SET embedding_kind = ?, embedding_model = ?, embedding_dim = ? \
         WHERE session_id = ?",
    )
    .bind(embedding.map(|e| e.kind.as_str()))
    .bind(embedding.and_then(|e| e.model.as_deref()))
    .bind(dim)
    .bind(&session.0)
    .execute(&mut *tx)
    .await
    .map_err(|e| map_write_err(e, |m| format!("set embedding: {m}")))?;
    if res.rows_affected() == 0 {
        return Err(StoreError::NotFound(format!(
            "sessions row for {session} while setting embedding"
        )));
    }
    Ok(())
}

/// Shared by the `CanonizationTransition` mutation and `record_canonization`:
/// append the event row — the demo's on-screen artifact — then update the
/// concept's status/blast_radius (NotFound if absent, like MemoryStore).
///
/// **F12 — the audit row is the idempotency key.** The evaluator dual-writes
/// (`record_canonization` immediately, the same transition again when the
/// write-behind log flushes), and the two are not ordered against each other:
/// a lagging flush of hop 1 landing after hop 2's immediate write would
/// otherwise *regress* the durable status, and a crash before hop 2's own
/// flush would leave the reload showing a status the audit already moved past
/// — after which the evaluator re-promotes under a fresh event id and the same
/// hop appears twice on screen. So the INSERT goes first: if its
/// `ON CONFLICT (id) DO NOTHING` fires, this transition's effect is already in
/// the row and the UPDATE is skipped. Both statements share the caller's
/// transaction, so the ordering swap costs nothing on the first write.
///
/// **R2-1 — what makes "already in the row" true.** Skipping the UPDATE is
/// only sound while nothing else writes those three columns. `upsert_concept`
/// used to, from a possibly stale `Mutation::UpsertNode` snapshot, so a batch
/// shaped `[UpsertNode(stale), CanonizationTransition(already recorded)]` left
/// the row regressed *and* the repair skipped. It no longer does — see
/// `upsert_concept` and `Mutation::UpsertNode`.
pub(super) async fn apply_canonization_transition(
    tx: &mut sqlx::SqliteConnection,
    event: &CanonizationEvent,
) -> Result<(), StoreError> {
    let to_status = enum_to_text(&event.to_status, "to_status")?;
    let from_status = enum_to_text(&event.from_status, "from_status")?;
    let appended = sqlx::query(
        "INSERT INTO canonization_events (\
             id, session_id, node_id, from_status, to_status, blast_radius, \
             last_demotion_time, occurred_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(event.id.0.to_string())
    .bind(&event.session_id.0)
    .bind(event.node_id.0.to_string())
    .bind(from_status)
    .bind(&to_status)
    .bind(event.blast_radius)
    .bind(event.last_demotion_time.map(ts_to_text))
    .bind(ts_to_text(event.occurred_at))
    .execute(&mut *tx)
    .await
    .map_err(|e| map_write_err(e, |m| format!("append canonization event: {m}")))?;
    if appended.rows_affected() == 0 {
        return Ok(());
    }

    // COH-3: last_demotion_time = COALESCE(?, last_demotion_time) — a demotion
    // event (Some) stamps the concept; non-demotion events (None) leave a
    // previously demoted value untouched (spec §10).
    let res = sqlx::query(
        "UPDATE concepts SET canonization_status = ?, blast_radius = ?, \
         last_demotion_time = COALESCE(?, last_demotion_time) \
         WHERE id = ? AND session_id = ?",
    )
    .bind(&to_status)
    .bind(event.blast_radius)
    .bind(event.last_demotion_time.map(ts_to_text))
    .bind(event.node_id.0.to_string())
    .bind(&event.session_id.0)
    .execute(&mut *tx)
    .await
    .map_err(|e| map_write_err(e, |m| format!("apply canonization transition: {m}")))?;
    if res.rows_affected() == 0 {
        return Err(StoreError::NotFound(format!(
            "concept {} for canonization",
            event.node_id
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Session erasure (#23). The transaction is `persistence.rs`'s `erase`.
// ---------------------------------------------------------------------------

/// Every session-keyed table erasure empties, with its DELETE, in dependency
/// order: rows that reference others go first (`write_intents`, `synonyms`,
/// `edges` and `concepts` reference `sessions`; `concepts` references
/// `interactions`; `interactions` references itself, which one statement over
/// the whole session satisfies), and `sessions` goes last. `session_leases`
/// is deliberately absent: its row becomes the tombstone instead.
///
/// `erase_covers_every_table_in_the_ddl` diffs this list against the shipped
/// migration, so a table added to the schema without an entry here fails a
/// test rather than surviving an account deletion.
pub(super) const ERASE_STATEMENTS: &[(&str, &str)] = &[
    (
        "write_intents",
        "DELETE FROM write_intents WHERE session_id = ?1",
    ),
    (
        "canonization_events",
        "DELETE FROM canonization_events WHERE session_id = ?1",
    ),
    (
        "reservations",
        "DELETE FROM reservations WHERE session_id = ?1",
    ),
    ("synonyms", "DELETE FROM synonyms WHERE session_id = ?1"),
    ("edges", "DELETE FROM edges WHERE session_id = ?1"),
    ("concepts", "DELETE FROM concepts WHERE session_id = ?1"),
    (
        "interactions",
        "DELETE FROM interactions WHERE session_id = ?1",
    ),
    (
        "session_stats",
        "DELETE FROM session_stats WHERE session_id = ?1",
    ),
    (
        "lease_refusals",
        "DELETE FROM lease_refusals WHERE session_id = ?1",
    ),
    ("sessions", "DELETE FROM sessions WHERE session_id = ?1"),
];

/// Edges in **other** sessions incident to the erased session's nodes (#23
/// review L4). Node ids are global and edges session-scoped, so an edge in
/// session B can point at a node of session A; `DeleteNode` removes such
/// edges with the node, and erasure does the same so no row keeps a deleted
/// account's node id. Run with the `edges` step, before `concepts` and
/// `interactions` go (the subqueries read them); counted in `edges`.
pub(super) const ERASE_CROSS_SESSION_EDGES_SQL: &str = "\
    DELETE FROM edges WHERE session_id <> ?1 AND ( \
        source IN (SELECT id FROM concepts WHERE session_id = ?1) \
     OR source IN (SELECT id FROM interactions WHERE session_id = ?1) \
     OR target IN (SELECT id FROM concepts WHERE session_id = ?1) \
     OR target IN (SELECT id FROM interactions WHERE session_id = ?1))";

/// Concepts of the session that carry an embedding, counted before they go.
pub(super) async fn count_session_vectors(
    tx: &mut sqlx::SqliteConnection,
    session: &SessionId,
) -> Result<u64, StoreError> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM concepts WHERE session_id = ?1 AND embedding IS NOT NULL",
    )
    .bind(&session.0)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| db_err("erase_session: count vectors", e))?;
    Ok(u64::try_from(n).unwrap_or(0))
}

/// Run one [`ERASE_STATEMENTS`] DELETE; the number of rows it removed.
pub(super) async fn delete_session_rows(
    tx: &mut sqlx::SqliteConnection,
    table: &str,
    sql: &str,
    session: &SessionId,
) -> Result<u64, StoreError> {
    let done = sqlx::query(sql)
        .bind(&session.0)
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("erase_session: delete {table}: {m}")))?;
    Ok(done.rows_affected())
}

/// Write the erasure tombstone over the session's lease row (see
/// `store::erase`). Guarded exactly like an acquire: it fires on no row, an
/// expired row, an earlier tombstone or the eraser's own lease. The token is
/// bumped unless the row already is a tombstone. `None` means the guard was
/// false — a live lease belongs to someone else.
pub(super) async fn write_erase_tombstone(
    tx: &mut sqlx::SqliteConnection,
    session: &SessionId,
    eraser: &str,
) -> Result<Option<u64>, StoreError> {
    let token: Option<i64> = sqlx::query_scalar(
        "INSERT INTO session_leases \
             (session_id, holder, acquired_at, expires_at, current_token, endpoint) \
         VALUES (?1, ?2, strftime('%Y-%m-%dT%H:%M:%fZ','now'), ?3, 1, NULL) \
         ON CONFLICT (session_id) DO UPDATE SET \
             holder = excluded.holder, \
             acquired_at = CASE WHEN session_leases.holder = excluded.holder \
                                THEN session_leases.acquired_at ELSE excluded.acquired_at END, \
             expires_at = excluded.expires_at, \
             current_token = CASE WHEN session_leases.holder = excluded.holder \
                                  THEN session_leases.current_token \
                                  ELSE session_leases.current_token + 1 END, \
             endpoint = NULL \
         WHERE session_leases.expires_at <= strftime('%Y-%m-%dT%H:%M:%fZ','now') \
            OR session_leases.holder = excluded.holder \
            OR session_leases.holder = ?4 \
         RETURNING current_token",
    )
    .bind(&session.0)
    .bind(crate::store::erase::ERASED_HOLDER)
    .bind(ts_to_text(crate::store::erase::tombstone_expires_at()))
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
