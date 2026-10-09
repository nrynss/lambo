//! Session load for the SQLite adapter: `load_session` materializes one
//! session from a single read transaction (so a concurrent writer process
//! cannot interleave a commit between the SELECTs), and the per-table row
//! readers decode each row into its graph type. Ordering rules are in the
//! adapter's module doc ("Load ordering").
//!
//! The row readers run on the caller's transaction and never begin or commit
//! one; `load_snapshot` owns the transaction.

use sqlx::Row;

use super::codec::{
    db_err, node_id, node_id_str, session_embedding_from_parts, text_to_enum, text_to_ts,
};
use super::SqliteStore;
use crate::store::vector::decode_vector_blob;
use crate::types::{
    CanonizationEvent, Concept, Edge, GcMark, GraphSnapshot, Interaction, SessionId, StoreError,
};

/// Load a session's write intents (J3), in replay order — (`issued_ms`,
/// `lane_seq`), which is exact admission order within one issuing process and
/// wall-clock order across processes.
pub(super) async fn load_write_intents(
    tx: &mut sqlx::SqliteConnection,
    session: &SessionId,
) -> Result<Vec<crate::types::WriteIntent>, StoreError> {
    let rows = sqlx::query(
        "SELECT receipt, agent, interaction_id, lane_seq, issued_ms, payload, created_at, \
                consumed_at, outcome_tag, outcome_summary \
         FROM write_intents WHERE session_id = ? ORDER BY issued_ms ASC, lane_seq ASC",
    )
    .bind(&session.0)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| db_err("load write intents", e))?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let receipt: String = row
            .try_get(0)
            .map_err(|e| db_err("load write intents", e))?;
        let agent: String = row
            .try_get(1)
            .map_err(|e| db_err("load write intents", e))?;
        let interaction: String = row
            .try_get(2)
            .map_err(|e| db_err("load write intents", e))?;
        let lane_seq: i64 = row
            .try_get(3)
            .map_err(|e| db_err("load write intents", e))?;
        let issued_ms: i64 = row
            .try_get(4)
            .map_err(|e| db_err("load write intents", e))?;
        let payload: String = row
            .try_get(5)
            .map_err(|e| db_err("load write intents", e))?;
        let created_at: String = row
            .try_get(6)
            .map_err(|e| db_err("load write intents", e))?;
        let consumed_at: Option<String> = row
            .try_get(7)
            .map_err(|e| db_err("load write intents", e))?;
        let outcome_tag: Option<String> = row
            .try_get(8)
            .map_err(|e| db_err("load write intents", e))?;
        let outcome_summary: Option<String> = row
            .try_get(9)
            .map_err(|e| db_err("load write intents", e))?;
        let payload: crate::types::WriteIntentPayload = serde_json::from_str(&payload)
            .map_err(|e| StoreError::Backend(format!("parse write intent payload: {e}")))?;
        let outcome = match (consumed_at, outcome_tag, outcome_summary) {
            (Some(at), Some(tag), Some(summary)) => Some(crate::types::WriteIntentOutcome {
                tag,
                summary,
                consumed_at: text_to_ts(&at)?,
            }),
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
            interaction: node_id(&interaction, "write intent interaction")?,
            lane_seq: u64::try_from(lane_seq).unwrap_or(u64::MAX),
            issued_ms,
            payload,
            created_at: text_to_ts(&created_at)?,
            outcome,
        });
    }
    Ok(out)
}

pub(super) async fn load_interactions(
    tx: &mut sqlx::SqliteConnection,
    session: &SessionId,
) -> Result<Vec<Interaction>, StoreError> {
    let rows = sqlx::query(
        "SELECT id, session_id, agent_id, prompt_text, previous_id, created_at, event_time \
         FROM interactions WHERE session_id = ? ORDER BY created_at ASC, id ASC",
    )
    .bind(&session.0)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| db_err("load interactions", e))?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let id: String = row.try_get(0).map_err(|e| db_err("load interactions", e))?;
        let sid: String = row.try_get(1).map_err(|e| db_err("load interactions", e))?;
        let agent: String = row.try_get(2).map_err(|e| db_err("load interactions", e))?;
        let prompt: Option<String> = row.try_get(3).map_err(|e| db_err("load interactions", e))?;
        let prev: Option<String> = row.try_get(4).map_err(|e| db_err("load interactions", e))?;
        let created: String = row.try_get(5).map_err(|e| db_err("load interactions", e))?;
        let event_time: Option<String> =
            row.try_get(6).map_err(|e| db_err("load interactions", e))?;
        out.push(Interaction {
            id: node_id(&id, "interaction id")?,
            session_id: SessionId::from(sid),
            agent_id: crate::types::AgentId::new(agent),
            prompt_text: prompt,
            previous_id: prev.as_deref().map(node_id_str).transpose()?,
            created_at: text_to_ts(&created)?,
            event_time: event_time.as_deref().map(text_to_ts).transpose()?,
        });
    }
    Ok(out)
}

pub(super) async fn load_concepts(
    tx: &mut sqlx::SqliteConnection,
    session: &SessionId,
) -> Result<Vec<Concept>, StoreError> {
    let rows = sqlx::query(
        "SELECT id, session_id, content, canonical_key, concept_type, origin_interaction, \
                origin_agent, created_at, access_count, last_accessed, gc_survived, \
                canonization_status, blast_radius, last_demotion_time, embedding, \
                chunk_group_id, human_confirmed \
         FROM concepts WHERE session_id = ? ORDER BY id ASC",
    )
    .bind(&session.0)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| db_err("load concepts", e))?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let id: String = row.try_get(0).map_err(|e| db_err("load concepts", e))?;
        let sid: String = row.try_get(1).map_err(|e| db_err("load concepts", e))?;
        let content: String = row.try_get(2).map_err(|e| db_err("load concepts", e))?;
        let key: String = row.try_get(3).map_err(|e| db_err("load concepts", e))?;
        let ctype: String = row.try_get(4).map_err(|e| db_err("load concepts", e))?;
        let origin: String = row.try_get(5).map_err(|e| db_err("load concepts", e))?;
        let agent: String = row.try_get(6).map_err(|e| db_err("load concepts", e))?;
        let created: String = row.try_get(7).map_err(|e| db_err("load concepts", e))?;
        let access_count: i32 = row.try_get(8).map_err(|e| db_err("load concepts", e))?;
        let last_accessed: Option<String> =
            row.try_get(9).map_err(|e| db_err("load concepts", e))?;
        let gc_survived: i32 = row.try_get(10).map_err(|e| db_err("load concepts", e))?;
        let status: String = row.try_get(11).map_err(|e| db_err("load concepts", e))?;
        let blast_radius: Option<i32> = row.try_get(12).map_err(|e| db_err("load concepts", e))?;
        let last_demotion: Option<String> =
            row.try_get(13).map_err(|e| db_err("load concepts", e))?;
        // CON-8: decode the BLOB back to the shared text form. A corrupt blob
        // (invalid UTF-8 / unparseable elements) is a backend error, not a panic.
        let embedding: Option<Vec<u8>> = row.try_get(14).map_err(|e| db_err("load concepts", e))?;
        let embedding = embedding
            .map(|bytes| decode_vector_blob(&id, &bytes))
            .transpose()?;
        let chunk_group_id: Option<String> =
            row.try_get(15).map_err(|e| db_err("load concepts", e))?;
        let human_confirmed: i32 = row.try_get(16).map_err(|e| db_err("load concepts", e))?;
        out.push(Concept {
            id: node_id(&id, "concept id")?,
            session_id: SessionId::from(sid),
            content,
            canonical_key: key,
            concept_type: text_to_enum(&ctype, "concept_type")?,
            origin_interaction: node_id(&origin, "origin_interaction")?,
            origin_agent: crate::types::AgentId::new(agent),
            created_at: text_to_ts(&created)?,
            access_count,
            last_accessed: last_accessed.as_deref().map(text_to_ts).transpose()?,
            gc_survived,
            canonization_status: text_to_enum(&status, "canonization_status")?,
            blast_radius,
            last_demotion_time: last_demotion.as_deref().map(text_to_ts).transpose()?,
            embedding,
            chunk_group_id,
            human_confirmed,
            embedding_source: None,
        });
    }
    Ok(out)
}

pub(super) async fn load_edges(
    tx: &mut sqlx::SqliteConnection,
    session: &SessionId,
) -> Result<Vec<Edge>, StoreError> {
    let rows = sqlx::query(
        "SELECT id, session_id, source, target, edge_type, weight, reinforcements, \
                created_at, last_reinforced, event_time \
         FROM edges WHERE session_id = ? ORDER BY id ASC",
    )
    .bind(&session.0)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| db_err("load edges", e))?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let id: String = row.try_get(0).map_err(|e| db_err("load edges", e))?;
        let sid: String = row.try_get(1).map_err(|e| db_err("load edges", e))?;
        let source: String = row.try_get(2).map_err(|e| db_err("load edges", e))?;
        let target: String = row.try_get(3).map_err(|e| db_err("load edges", e))?;
        let etype: String = row.try_get(4).map_err(|e| db_err("load edges", e))?;
        let weight: f64 = row.try_get(5).map_err(|e| db_err("load edges", e))?;
        let reinforcements: i32 = row.try_get(6).map_err(|e| db_err("load edges", e))?;
        let created: String = row.try_get(7).map_err(|e| db_err("load edges", e))?;
        let last_reinforced: String = row.try_get(8).map_err(|e| db_err("load edges", e))?;
        let event_time: Option<String> = row.try_get(9).map_err(|e| db_err("load edges", e))?;
        out.push(Edge {
            id: node_id(&id, "edge id")?,
            session_id: SessionId::from(sid),
            source: node_id(&source, "edge source")?,
            target: node_id(&target, "edge target")?,
            edge_type: text_to_enum(&etype, "edge_type")?,
            weight,
            reinforcements,
            created_at: text_to_ts(&created)?,
            last_reinforced: text_to_ts(&last_reinforced)?,
            event_time: event_time.as_deref().map(text_to_ts).transpose()?,
        });
    }
    Ok(out)
}

pub(super) async fn load_synonyms(
    tx: &mut sqlx::SqliteConnection,
    session: &SessionId,
) -> Result<Vec<crate::types::Synonym>, StoreError> {
    let rows = sqlx::query(
        "SELECT session_id, source_key, canonical_key \
         FROM synonyms WHERE session_id = ? ORDER BY source_key ASC",
    )
    .bind(&session.0)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| db_err("load synonyms", e))?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let sid: String = row.try_get(0).map_err(|e| db_err("load synonyms", e))?;
        let src: String = row.try_get(1).map_err(|e| db_err("load synonyms", e))?;
        let canon: String = row.try_get(2).map_err(|e| db_err("load synonyms", e))?;
        out.push(crate::types::Synonym {
            session_id: SessionId::from(sid),
            source_key: src,
            canonical_key: canon,
        });
    }
    Ok(out)
}

pub(super) async fn load_reservations(
    tx: &mut sqlx::SqliteConnection,
    session: &SessionId,
) -> Result<Vec<crate::types::Reservation>, StoreError> {
    let rows = sqlx::query(
        "SELECT session_id, node_id, agent_id, expires_at \
         FROM reservations WHERE session_id = ?",
    )
    .bind(&session.0)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| db_err("load reservations", e))?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let sid: String = row.try_get(0).map_err(|e| db_err("load reservations", e))?;
        let node: String = row.try_get(1).map_err(|e| db_err("load reservations", e))?;
        let agent: String = row.try_get(2).map_err(|e| db_err("load reservations", e))?;
        let expires: String = row.try_get(3).map_err(|e| db_err("load reservations", e))?;
        out.push(crate::types::Reservation {
            session_id: SessionId::from(sid),
            node_id: node_id(&node, "reservation node")?,
            agent_id: crate::types::AgentId::new(agent),
            expires_at: text_to_ts(&expires)?,
        });
    }
    Ok(out)
}

pub(super) async fn load_canonization_events(
    tx: &mut sqlx::SqliteConnection,
    session: &SessionId,
) -> Result<Vec<CanonizationEvent>, StoreError> {
    let rows = sqlx::query(
        "SELECT id, session_id, node_id, from_status, to_status, blast_radius, \
             last_demotion_time, occurred_at \
         FROM canonization_events WHERE session_id = ? ORDER BY occurred_at ASC, id ASC",
    )
    .bind(&session.0)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| db_err("load canonization events", e))?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let id: String = row
            .try_get(0)
            .map_err(|e| db_err("load canonization events", e))?;
        let sid: String = row
            .try_get(1)
            .map_err(|e| db_err("load canonization events", e))?;
        let node: String = row
            .try_get(2)
            .map_err(|e| db_err("load canonization events", e))?;
        let from: String = row
            .try_get(3)
            .map_err(|e| db_err("load canonization events", e))?;
        let to: String = row
            .try_get(4)
            .map_err(|e| db_err("load canonization events", e))?;
        let blast_radius: Option<i32> = row
            .try_get(5)
            .map_err(|e| db_err("load canonization events", e))?;
        let last_demotion: Option<String> = row
            .try_get(6)
            .map_err(|e| db_err("load canonization events", e))?;
        let occurred: String = row
            .try_get(7)
            .map_err(|e| db_err("load canonization events", e))?;
        out.push(CanonizationEvent {
            id: node_id(&id, "canonization event id")?,
            session_id: SessionId::from(sid),
            node_id: node_id(&node, "canonization node")?,
            from_status: text_to_enum(&from, "from_status")?,
            to_status: text_to_enum(&to, "to_status")?,
            blast_radius,
            last_demotion_time: last_demotion.as_deref().map(text_to_ts).transpose()?,
            occurred_at: text_to_ts(&occurred)?,
        });
    }
    Ok(out)
}

impl SqliteStore {
    pub(super) async fn load_snapshot(
        &self,
        session: &SessionId,
    ) -> Result<GraphSnapshot, StoreError> {
        // One read transaction so the session materializes from a consistent
        // view (startup path; single-connection pool would otherwise interleave
        // with a concurrent flush between SELECTs).
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(|e| db_err("begin load transaction", e))?;

        // The existence probe doubles as the embedding-contract read. Both
        // snapshot seed and `SetEmbedding` flush write these columns; root_goal
        // likewise has its ordered mutation path (XP-8). `mutation_epoch` is
        // the durable mutation counter (issue #17): flush stamps it, and this
        // read is what a writer restart resumes it from.
        let row = sqlx::query(
            "SELECT embedding_kind, embedding_model, embedding_dim, root_goal, mutation_epoch, \
                    last_gc_epoch, last_gc_at \
             FROM sessions WHERE session_id = ?",
        )
        .bind(&session.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| db_err("lookup session", e))?;
        let row = match row {
            Some(row) => row,
            None => return Err(StoreError::SessionNotFound(session.0.clone())),
        };
        let embedding_kind: Option<String> =
            row.try_get(0).map_err(|e| db_err("lookup session", e))?;
        let embedding_model: Option<String> =
            row.try_get(1).map_err(|e| db_err("lookup session", e))?;
        let embedding_dim: Option<i64> = row.try_get(2).map_err(|e| db_err("lookup session", e))?;
        // XP-8: `root_goal` survives a reload — `Mutation::SetRootGoal` writes
        // it, so replaying the log no longer silently clears the drift anchor.
        let root_goal: Option<String> = row.try_get(3).map_err(|e| db_err("lookup session", e))?;
        let root_goal = root_goal
            .as_deref()
            .map(serde_json::from_str::<serde_json::Value>)
            .transpose()
            .map_err(|e| StoreError::Backend(format!("parse root_goal JSON: {e}")))?;
        let mutation_epoch: i64 = row.try_get(4).map_err(|e| db_err("lookup session", e))?;
        // Issue #29: GC's sweep accounting, resumed with the epoch.
        let last_gc_epoch: i64 = row.try_get(5).map_err(|e| db_err("lookup session", e))?;
        let last_gc_at: Option<String> = row.try_get(6).map_err(|e| db_err("lookup session", e))?;
        let gc_mark = GcMark {
            last_gc_epoch: u64::try_from(last_gc_epoch).unwrap_or(0),
            last_gc_at: last_gc_at.as_deref().map(text_to_ts).transpose()?,
            last_gc_at_reset: false,
        };
        let embedding = session_embedding_from_parts(
            embedding_kind,
            embedding_model,
            embedding_dim,
            &session.0,
        )?;

        let interactions = load_interactions(&mut *tx, session).await?;
        let concepts = load_concepts(&mut *tx, session).await?;
        let edges = load_edges(&mut *tx, session).await?;
        let synonyms = load_synonyms(&mut *tx, session).await?;
        let reservations = load_reservations(&mut *tx, session).await?;
        let canonization_events = load_canonization_events(&mut *tx, session).await?;
        let write_intents = load_write_intents(&mut *tx, session).await?;

        tx.commit()
            .await
            .map_err(|e| db_err("commit load transaction", e))?;

        Ok(GraphSnapshot {
            session_id: session.clone(),
            root_goal,
            // `created_at`/`closed_at` are still snapshot-only (no Mutation
            // kind) — None, matching MemoryStore (see module doc).
            created_at: None,
            closed_at: None,
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
    }
}
