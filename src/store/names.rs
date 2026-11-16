//! Shared validation for durable citation-name records (#146).
#[cfg(feature = "fixtures")]
use crate::types::GraphSnapshot;
use crate::types::{ConceptName, RetiredAlias, StoreError};

pub(crate) fn validate_name(name: &ConceptName) -> Result<(), StoreError> {
    let expected = crate::graph::names::name_key(&name.name)
        .map_err(|error| StoreError::Invariant(error.to_string()))?;
    if name.name_key != expected {
        return Err(StoreError::Invariant(
            "concept name has an invalid normalized key".into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_retired_alias(retired: &RetiredAlias) -> Result<(), StoreError> {
    if retired.session_id != retired.concept.session_id
        || retired.target == retired.concept.id
        || retired.edges.iter().any(|e| {
            e.session_id != retired.session_id
                || (e.source != retired.concept.id && e.target != retired.concept.id)
        })
    {
        return Err(StoreError::Invariant(
            "retired alias has an invalid session or target".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(any(
    feature = "store-sqlite",
    feature = "store-postgres",
    feature = "store-cockroach"
))]
pub(crate) fn decode_retired_alias(payload: &str) -> Result<RetiredAlias, StoreError> {
    let retired: RetiredAlias = serde_json::from_str(payload)
        .map_err(|e| StoreError::Invariant(format!("invalid retired alias payload: {e}")))?;
    validate_retired_alias(&retired)?;
    Ok(retired)
}

/// Validate durable identity metadata before importing an arbitrary snapshot.
#[cfg(feature = "fixtures")]
pub(crate) fn validate_snapshot(snapshot: &GraphSnapshot) -> Result<(), StoreError> {
    let mut keys = std::collections::HashSet::new();
    let mut retired_ids = std::collections::HashSet::new();
    for retired in &snapshot.retired_aliases {
        validate_retired_alias(retired)?;
        if retired.session_id != snapshot.session_id
            || !retired_ids.insert(retired.concept.id)
            || snapshot.concepts.iter().any(|c| c.id == retired.concept.id)
            || snapshot
                .interactions
                .iter()
                .any(|i| i.id == retired.concept.id)
        {
            return Err(StoreError::Invariant(
                "invalid or active retired alias in snapshot".into(),
            ));
        }
    }
    for name in &snapshot.names {
        validate_name(name)?;
        if name.session_id != snapshot.session_id
            || !keys.insert(&name.name_key)
            || retired_ids.contains(&name.node_id)
            || snapshot.interactions.iter().any(|i| i.id == name.node_id)
            || snapshot
                .concepts
                .iter()
                .any(|c| c.canonical_key == name.name_key && c.id != name.node_id)
        {
            return Err(StoreError::Invariant(
                "invalid or conflicting concept name in snapshot".into(),
            ));
        }
    }
    Ok(())
}
