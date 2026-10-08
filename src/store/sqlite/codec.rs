//! Value codecs for the SQLite adapter: the fixed-width timestamp text form,
//! node ids, serde-spelled enums, the session embedding-contract columns and the
//! backend error wrapper. Pure functions shared by every SQLite submodule; no
//! statement runs here.
//!
//! The timestamp form is the round-trip contract described in the adapter's
//! module doc ("Dialect notes"): `YYYY-MM-DDTHH:MM:SS.SSSZ`, so RFC 3339
//! ordering equals lexicographic ordering in SQL.

use chrono::{DateTime, SecondsFormat, Utc};
use std::time::Duration;

use crate::types::{EmbeddingContract, NodeId, StoreError};

pub(super) fn db_err(context: &str, e: sqlx::Error) -> StoreError {
    StoreError::Backend(format!("{context}: {e}"))
}

/// Rebuild the session's [`EmbeddingContract`] from the three nullable columns.
///
/// Shared by `load_session` and the checked candidate read so both classify a corrupt
/// row identically (STORE-7 parity with Cockroach's `session_embedding_from_parts`): a
/// row with `embedding_kind` XOR `embedding_dim` set — which direct SQL can manufacture —
/// is a corruption error, never a silent `None`. `embedding_model` alone is legal (an
/// embedder with no model identifier).
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
        (Some(_), None) => Err(StoreError::Backend(format!(
            "sessions row for {session_id} has embedding_kind without embedding_dim"
        ))),
        (None, Some(_)) => Err(StoreError::Backend(format!(
            "sessions row for {session_id} has embedding_dim without embedding_kind"
        ))),
    }
}

/// Fixed ISO-8601 UTC serialization (T3.1 contract):
/// `YYYY-MM-DDTHH:MM:SS.SSSZ` — 24 chars, ms always present, `Z` suffix.
pub(super) fn ts_to_text(ts: DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub(super) fn text_to_ts(s: &str) -> Result<DateTime<Utc>, StoreError> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| StoreError::Backend(format!("invalid stored timestamp {s:?}: {e}")))
}

/// Cutoff timestamp for age filters, computed in Rust (SQLite has no INTERVAL)
/// and bound as the fixed TEXT so lex comparison in SQL is valid.
pub(super) fn cutoff_text(now: DateTime<Utc>, age: Duration) -> Result<String, StoreError> {
    let d = chrono::Duration::from_std(age)
        .map_err(|e| StoreError::Backend(format!("age duration out of range: {e}")))?;
    Ok(ts_to_text(now - d))
}

pub(super) fn node_id(s: &str, what: &str) -> Result<NodeId, StoreError> {
    uuid::Uuid::parse_str(s)
        .map(NodeId)
        .map_err(|e| StoreError::Backend(format!("invalid stored {what} {s:?}: {e}")))
}

pub(super) fn node_id_str(s: &str) -> Result<NodeId, StoreError> {
    node_id(s, "node id")
}

pub(super) fn enum_to_text<T: serde::Serialize>(v: &T, what: &str) -> Result<String, StoreError> {
    let value = serde_json::to_value(v)
        .map_err(|e| StoreError::Backend(format!("serialize {what}: {e}")))?;
    match value {
        serde_json::Value::String(s) => Ok(s),
        other => Err(StoreError::Backend(format!(
            "serialize {what}: expected string, got {other:?}"
        ))),
    }
}

pub(super) fn text_to_enum<T: serde::de::DeserializeOwned>(
    s: &str,
    what: &str,
) -> Result<T, StoreError> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|e| StoreError::Backend(format!("invalid stored {what} {s:?}: {e}")))
}
