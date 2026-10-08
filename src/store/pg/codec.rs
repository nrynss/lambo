//! Value and row codecs for the Postgres-wire family: the backend error
//! wrapper, the session embedding-contract classifier, enum <-> STRING column
//! spellings, `load_session`'s row -> graph-type decoders, the shared
//! candidate order, and small pure checks. No statement runs here.
//!
//! `encode_vector` / `decode_vector` live in the shared `crate::store::vector`
//! module (CON-8: SQLite stores the same text form as a BLOB, so every adapter
//! shares one codec).

use chrono::{DateTime, Utc};
use sqlx::postgres::PgRow;
use sqlx::Row;
use std::time::Duration;
use uuid::Uuid;

use crate::store::vector::decode_vector;
use crate::types::{
    tie_break_by_key, CanonizationEvent, CanonizationStatus, Concept, ConceptType, Edge, EdgeType,
    EmbeddingContract, Interaction, NodeId, Reservation, Scored, SessionId, StoreError, Synonym,
};

/// Parse pgvector `format_type` output (`vector(768)`). Rejects Cockroach
/// `VECTOR(n)` so a live probe cannot silently accept the wrong dialect.
pub(crate) fn parse_pgvector_format_type(formatted: &str) -> Option<usize> {
    let rest = formatted.trim().strip_prefix("vector(")?;
    let inner = rest.strip_suffix(')')?;
    inner.trim().parse().ok()
}

pub(super) fn backend<E: std::fmt::Display>(e: E) -> StoreError {
    StoreError::Backend(e.to_string())
}

/// STORE-7 — session-row embedding-contract parsing. A row with exactly one
/// of `embedding_kind` / `embedding_dim` set (as direct SQL can manufacture)
/// is a corruption error, never a silent `None`; `embedding_model` alone is
/// inert (model without a kind has nothing to label). The kind-XOR-dim arms
/// classify as [`StoreError::Invariant`] (E2E-2): the corruption is
/// deterministic, so `tx_retry` must not replay it — both consumers (the
/// checked vector read and the load path) go through this helper.
pub(super) fn session_embedding_from_parts(
    kind: Option<String>,
    model: Option<String>,
    dim: Option<i64>,
    session_id: &str,
) -> Result<Option<EmbeddingContract>, StoreError> {
    match (kind, dim) {
        (Some(kind), Some(dim)) => Ok(Some(EmbeddingContract {
            kind,
            model,
            dim: usize::try_from(dim).map_err(|_| {
                StoreError::Backend(format!(
                    "sessions row for {session_id} has negative embedding_dim"
                ))
            })?,
        })),
        (None, None) => Ok(None),
        // E2E-2: a kind-XOR-dim row is DETERMINISTIC corruption — replaying
        // the transaction cannot change the parse, so classifying it as
        // `Backend` made `tx_retry` replay it 5× with backoff (~500 ms)
        // before surfacing (STORE-4: deterministic failures are never
        // replayed). `Invariant` returns on the first attempt. Same class on
        // the load path: both consumers (load_session and the checked read)
        // go through this helper.
        (Some(_), None) => Err(StoreError::Invariant(format!(
            "sessions row for {session_id} has embedding_kind without embedding_dim"
        ))),
        (None, Some(_)) => Err(StoreError::Invariant(format!(
            "sessions row for {session_id} has embedding_dim without embedding_kind"
        ))),
    }
}

/// The pg-family candidate order: score descending, then canonical key
/// ascending, then node id ascending ([`tie_break_by_key`], issue #2). One
/// implementation for the global-fetch filter and the exact session query, so
/// the two paths can never order equal-score rows differently.
pub(super) fn order_candidates(mut scored: Vec<(Scored<NodeId>, String)>) -> Vec<Scored<NodeId>> {
    scored.sort_by(|(a, a_key), (b, b_key)| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| tie_break_by_key(Some(a_key), &a.item, Some(b_key), &b.item))
    });
    scored.into_iter().map(|(s, _)| s).collect()
}

/// Embeddings must match the schema column width before they ever reach SQL.
pub(super) fn check_embedding_dim(v: &[f32], dim: usize) -> Result<(), StoreError> {
    if v.len() != dim {
        return Err(StoreError::Invariant(format!(
            "embedding dimension {} does not match store vector width {dim} (see vector_dimensions())",
            v.len()
        )));
    }
    Ok(())
}

/// `now - age`, mirroring `MemoryStore::cutoff` (error vocabulary included).
pub(super) fn cutoff(now: DateTime<Utc>, age: Duration) -> Result<DateTime<Utc>, StoreError> {
    let d = chrono::Duration::from_std(age)
        .map_err(|e| StoreError::Backend(format!("age duration out of range: {e}")))?;
    Ok(now - d)
}

pub(super) fn concept_type_sql(ct: ConceptType) -> &'static str {
    match ct {
        ConceptType::Entity => "Entity",
        ConceptType::Logic => "Logic",
        ConceptType::Constraint => "Constraint",
        ConceptType::Resource => "Resource",
        ConceptType::Observation => "Observation",
    }
}

pub(super) fn parse_concept_type(s: &str) -> Result<ConceptType, StoreError> {
    Ok(match s {
        "Entity" => ConceptType::Entity,
        "Logic" => ConceptType::Logic,
        "Constraint" => ConceptType::Constraint,
        "Resource" => ConceptType::Resource,
        "Observation" => ConceptType::Observation,
        other => return Err(backend(format!("unknown concept_type {other:?} in store"))),
    })
}

pub(super) fn edge_type_sql(et: EdgeType) -> &'static str {
    match et {
        EdgeType::Temporal => "Temporal",
        EdgeType::Derives => "Derives",
        EdgeType::CoOccurrence => "CoOccurrence",
        EdgeType::Causal => "Causal",
        EdgeType::Dependency => "Dependency",
        EdgeType::Hierarchical => "Hierarchical",
        EdgeType::Semantic => "Semantic",
    }
}

pub(super) fn parse_edge_type(s: &str) -> Result<EdgeType, StoreError> {
    Ok(match s {
        "Temporal" => EdgeType::Temporal,
        "Derives" => EdgeType::Derives,
        "CoOccurrence" => EdgeType::CoOccurrence,
        "Causal" => EdgeType::Causal,
        "Dependency" => EdgeType::Dependency,
        "Hierarchical" => EdgeType::Hierarchical,
        "Semantic" => EdgeType::Semantic,
        other => return Err(backend(format!("unknown edge_type {other:?} in store"))),
    })
}

pub(super) fn canonization_status_sql(cs: CanonizationStatus) -> &'static str {
    match cs {
        CanonizationStatus::None => "None",
        CanonizationStatus::Candidate => "Candidate",
        CanonizationStatus::Venerable => "Venerable",
        CanonizationStatus::Canonical => "Canonical",
    }
}

pub(super) fn parse_canonization_status(s: &str) -> Result<CanonizationStatus, StoreError> {
    Ok(match s {
        "None" => CanonizationStatus::None,
        "Candidate" => CanonizationStatus::Candidate,
        "Venerable" => CanonizationStatus::Venerable,
        "Canonical" => CanonizationStatus::Canonical,
        other => {
            return Err(backend(format!(
                "unknown canonization_status {other:?} in store"
            )))
        }
    })
}

pub(super) fn parse_node_id(s: &str) -> Result<NodeId, StoreError> {
    Uuid::parse_str(s)
        .map(NodeId)
        .map_err(|e| backend(format!("invalid node id {s:?}: {e}")))
}

pub(super) fn row_to_interaction(row: &PgRow) -> Result<Interaction, StoreError> {
    let id: String = row.try_get("id").map_err(backend)?;
    let previous: Option<String> = row.try_get("previous_id").map_err(backend)?;
    Ok(Interaction {
        id: parse_node_id(&id)?,
        session_id: SessionId(row.try_get("session_id").map_err(backend)?),
        agent_id: crate::types::AgentId(row.try_get("agent_id").map_err(backend)?),
        prompt_text: row.try_get("prompt_text").map_err(backend)?,
        previous_id: previous.as_deref().map(parse_node_id).transpose()?,
        created_at: row.try_get("created_at").map_err(backend)?,
        event_time: row.try_get("event_time").map_err(backend)?,
    })
}

pub(super) fn row_to_concept(row: &PgRow) -> Result<Concept, StoreError> {
    let id: String = row.try_get("id").map_err(backend)?;
    let origin: String = row.try_get("origin_interaction").map_err(backend)?;
    let embedding: Option<String> = row.try_get("embedding").map_err(backend)?;
    // Cockroach `INT` is INT8 on the wire (all integer columns); Lambo types are i32.
    let access_count: i64 = row.try_get("access_count").map_err(backend)?;
    let gc_survived: i64 = row.try_get("gc_survived").map_err(backend)?;
    let blast_radius: Option<i64> = row.try_get("blast_radius").map_err(backend)?;
    let human_confirmed: i64 = row.try_get("human_confirmed").map_err(backend)?;
    Ok(Concept {
        id: parse_node_id(&id)?,
        session_id: SessionId(row.try_get("session_id").map_err(backend)?),
        content: row.try_get("content").map_err(backend)?,
        canonical_key: row.try_get("canonical_key").map_err(backend)?,
        concept_type: parse_concept_type(
            &row.try_get::<String, _>("concept_type").map_err(backend)?,
        )?,
        origin_interaction: parse_node_id(&origin)?,
        origin_agent: crate::types::AgentId(row.try_get("origin_agent").map_err(backend)?),
        created_at: row.try_get("created_at").map_err(backend)?,
        access_count: access_count as i32,
        last_accessed: row.try_get("last_accessed").map_err(backend)?,
        gc_survived: gc_survived as i32,
        canonization_status: parse_canonization_status(
            &row.try_get::<String, _>("canonization_status")
                .map_err(backend)?,
        )?,
        blast_radius: blast_radius.map(|v| v as i32),
        last_demotion_time: row.try_get("last_demotion_time").map_err(backend)?,
        embedding: embedding.as_deref().map(decode_vector).transpose()?,
        human_confirmed: human_confirmed as i32,
        chunk_group_id: row.try_get("chunk_group_id").map_err(backend)?,
    })
}

pub(super) fn row_to_edge(row: &PgRow) -> Result<Edge, StoreError> {
    let id: String = row.try_get("id").map_err(backend)?;
    let source: String = row.try_get("source").map_err(backend)?;
    let target: String = row.try_get("target").map_err(backend)?;
    let reinforcements: i64 = row.try_get("reinforcements").map_err(backend)?;
    Ok(Edge {
        id: parse_node_id(&id)?,
        session_id: SessionId(row.try_get("session_id").map_err(backend)?),
        source: parse_node_id(&source)?,
        target: parse_node_id(&target)?,
        edge_type: parse_edge_type(&row.try_get::<String, _>("edge_type").map_err(backend)?)?,
        weight: row.try_get("weight").map_err(backend)?,
        reinforcements: reinforcements as i32,
        created_at: row.try_get("created_at").map_err(backend)?,
        last_reinforced: row.try_get("last_reinforced").map_err(backend)?,
        event_time: row.try_get("event_time").map_err(backend)?,
    })
}

pub(super) fn row_to_synonym(row: &PgRow) -> Result<Synonym, StoreError> {
    Ok(Synonym {
        session_id: SessionId(row.try_get("session_id").map_err(backend)?),
        source_key: row.try_get("source_key").map_err(backend)?,
        canonical_key: row.try_get("canonical_key").map_err(backend)?,
    })
}

pub(super) fn row_to_reservation(row: &PgRow) -> Result<Reservation, StoreError> {
    let node_id: String = row.try_get("node_id").map_err(backend)?;
    Ok(Reservation {
        session_id: SessionId(row.try_get("session_id").map_err(backend)?),
        node_id: parse_node_id(&node_id)?,
        agent_id: crate::types::AgentId(row.try_get("agent_id").map_err(backend)?),
        expires_at: row.try_get("expires_at").map_err(backend)?,
    })
}

pub(super) fn row_to_canonization_event(row: &PgRow) -> Result<CanonizationEvent, StoreError> {
    let id: String = row.try_get("id").map_err(backend)?;
    let node_id: String = row.try_get("node_id").map_err(backend)?;
    let blast_radius: Option<i64> = row.try_get("blast_radius").map_err(backend)?;
    Ok(CanonizationEvent {
        id: parse_node_id(&id)?,
        session_id: SessionId(row.try_get("session_id").map_err(backend)?),
        node_id: parse_node_id(&node_id)?,
        from_status: parse_canonization_status(
            &row.try_get::<String, _>("from_status").map_err(backend)?,
        )?,
        to_status: parse_canonization_status(
            &row.try_get::<String, _>("to_status").map_err(backend)?,
        )?,
        blast_radius: blast_radius.map(|v| v as i32),
        last_demotion_time: row.try_get("last_demotion_time").map_err(backend)?,
        occurred_at: row.try_get("occurred_at").map_err(backend)?,
    })
}
