//! Committed fixture graphs (T1.4) — the swarm unblocker.
//!
//! JSON lives in `fixtures/`. This module loads them into in-RAM structures so
//! P2–P7 tracks start against realistic data with zero network access.
//!
//! Loadable surfaces:
//! * `load_snapshot(name)` — a `GraphSnapshot` (session graphs).
//! * `load_store(name)`    — a `MemoryStore` seeded with that snapshot.
//! * `load_mutation_batch(name)` — a `MutationBatch` (flush/adapter input).
//! * `load_recall_goldens()`, `load_canonicalization_cases()` — P5 / P6 tables.
//!
//! Conventions (see `scripts/gen-fixtures.py`, which regenerates these):
//! * Canonical keys: lowercase -> split `[-_ ]` + camelCase -> drop stopwords ->
//!   Porter stem -> sort -> join `" "`. Synonym lookup on the raw normalized key
//!   BEFORE stemming ("register_user" -> "create_user" -> "creat user").
//! * All timestamps are in the past relative to eval time, so age filters treat
//!   every edge as aged.

use std::fs;
use std::path::PathBuf;

use crate::store::MemoryStore;
use crate::types::{GraphSnapshot, MutationBatch, StoreError};

/// Absolute path to a fixture JSON file.
pub fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(format!("{name}.json"))
}

fn read_json<T: serde::de::DeserializeOwned>(name: &str) -> Result<T, StoreError> {
    let path = fixture_path(name);
    let text = fs::read_to_string(&path)
        .map_err(|e| StoreError::Backend(format!("fixture {path:?} unreadable: {e}")))?;
    serde_json::from_str(&text)
        .map_err(|e| StoreError::Backend(format!("fixture {path:?} invalid JSON: {e}")))
}

/// Load a session graph snapshot by fixture name (e.g. "session-rest-api").
pub fn load_snapshot(name: &str) -> Result<GraphSnapshot, StoreError> {
    read_json(name)
}

/// Load a session graph and seed it into a fresh `MemoryStore`.
pub fn load_store(name: &str) -> Result<MemoryStore, StoreError> {
    let snap = load_snapshot(name)?;
    let store = MemoryStore::new();
    store.seed(snap)?;
    Ok(store)
}

/// Load the ordered write-behind `MutationBatch` exercising every mutation kind.
pub fn load_mutation_batch(name: &str) -> Result<MutationBatch, StoreError> {
    read_json(name)
}

/// Recall goldens: query -> expected phase-1 candidates / phase-2 expanded sets.
pub fn load_recall_goldens() -> Result<serde_json::Value, StoreError> {
    read_json("recall-goldens")
}

/// Canonicalization cases: input text -> expected canonical key (T6 contract).
pub fn load_canonicalization_cases() -> Result<serde_json::Value, StoreError> {
    read_json("canonicalization-cases")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::GraphStore;
    use crate::types::{CanonizationStatus, NodeId, SessionId};
    use std::time::Duration;

    fn node_id(s: &str) -> NodeId {
        NodeId(s.parse().unwrap())
    }

    #[test]
    fn loads_rest_api_snapshot() {
        let snap = load_snapshot("session-rest-api").unwrap();
        assert_eq!(snap.session_id, SessionId::from("session-rest-api"));
        assert_eq!(snap.interactions.len(), 12);
        assert_eq!(snap.concepts.len(), 20);
        assert!(!snap.edges.is_empty());
    }

    #[tokio::test]
    async fn loads_into_memory_store_without_invariant_violations() {
        let store = load_store("session-rest-api").unwrap();
        let snap = store
            .load_session(&SessionId::from("session-rest-api"))
            .await
            .unwrap();
        assert_eq!(snap.concepts.len(), 20);
        // User schema present and already Canonical with a blast radius.
        let us = snap
            .concepts
            .iter()
            .find(|c| c.content == "user schema")
            .expect("user schema present");
        assert_eq!(us.canonization_status, CanonizationStatus::Canonical);
        assert_eq!(us.blast_radius, Some(8));
    }

    #[tokio::test]
    async fn rest_api_user_schema_passes_all_three_stages() {
        let store = load_store("session-rest-api").unwrap();
        let sid = SessionId::from("session-rest-api");
        let us = node_id("f0000000-0000-4000-8000-000000001001");

        // Stage 1: gc_survived >= 3
        let snap = store.load_session(&sid).await.unwrap();
        let g = snap.concepts.iter().find(|c| c.id == us).unwrap();
        assert!(g.gc_survived >= 3);

        // Stage 2: interaction_span >= 3 distinct interactions, coverage >= 0.3
        let span = store
            .interaction_span(&sid, us, Duration::from_secs(0))
            .await
            .unwrap();
        assert!(span.distinct >= 6, "distinct={}", span.distinct);
        assert!(span.coverage >= 0.3, "coverage={}", span.coverage);

        // Stage 3: blast_radius > 5
        let br = store
            .blast_radius(&sid, us, Duration::from_secs(0))
            .await
            .unwrap();
        assert!(br > 5, "blast_radius={br}");
    }

    #[tokio::test]
    async fn api_layer_passes_stage2_but_fails_stage3() {
        let store = load_store("session-rest-api").unwrap();
        let sid = SessionId::from("session-rest-api");
        let api = node_id("f0000000-0000-4000-8000-000000001012");

        let span = store
            .interaction_span(&sid, api, Duration::from_secs(0))
            .await
            .unwrap();
        assert!(span.distinct >= 3, "distinct={}", span.distinct);
        assert!(span.coverage >= 0.3, "coverage={}", span.coverage);

        let br = store
            .blast_radius(&sid, api, Duration::from_secs(0))
            .await
            .unwrap();
        assert!(br <= 5, "blast_radius={br} (should fail Stage 3)");
    }

    #[tokio::test]
    async fn rest_api_has_recent_agent_a_write() {
        let store = load_store("session-rest-api").unwrap();
        let sid = SessionId::from("session-rest-api");
        let snap = store.load_session(&sid).await.unwrap();
        // Caching layer (id 1010) is authored by agent-a near session end.
        let cache_id = node_id("f0000000-0000-4000-8000-000000001010");
        let cache = snap.concepts.iter().find(|c| c.id == cache_id).unwrap();
        assert_eq!(cache.origin_agent.as_str(), "agent-a");
        assert_eq!(cache.content, "caching layer");
    }

    /// All edges must reference nodes that actually exist (spec §5.7 invariant).
    #[test]
    fn edges_reference_existing_nodes() {
        for name in ["session-rest-api", "session-drift"] {
            let snap = load_snapshot(name).unwrap();
            let mut ids = std::collections::HashSet::new();
            for i in &snap.interactions {
                ids.insert(i.id);
            }
            for c in &snap.concepts {
                ids.insert(c.id);
            }
            for e in &snap.edges {
                assert!(
                    ids.contains(&e.source),
                    "{name}: edge {} source missing",
                    e.id
                );
                assert!(
                    ids.contains(&e.target),
                    "{name}: edge {} target missing",
                    e.id
                );
            }
        }
    }

    #[test]
    fn drift_has_goal_onenpath_far_and_disconnected() {
        let snap = load_snapshot("session-drift").unwrap();
        let contents: Vec<&str> = snap.concepts.iter().map(|c| c.content.as_str()).collect();
        assert!(contents.contains(&"launch the product"));
        assert!(contents.contains(&"on path step one"));
        assert!(contents.contains(&"far budget concept"));
        assert!(contents.contains(&"isolated widget"));
        // Root goal is marked Venerable.
        let goal = snap
            .concepts
            .iter()
            .find(|c| c.content == "launch the product")
            .unwrap();
        assert_eq!(goal.canonization_status, CanonizationStatus::Venerable);
        // 9 concepts, 7 edges, 2 interactions.
        assert_eq!(snap.concepts.len(), 9);
        assert_eq!(snap.edges.len(), 7);
    }

    #[test]
    fn mutations_batch_loads_and_applies() {
        let batch = load_mutation_batch("mutations-batch").unwrap();
        assert_eq!(batch.mutations.len(), 8); // all five kinds exercised
        let mut kinds = std::collections::HashSet::new();
        use crate::types::Mutation;
        for m in &batch.mutations {
            let k = match m {
                Mutation::UpsertNode { .. } => "upsert_node",
                Mutation::UpsertEdge { .. } => "upsert_edge",
                Mutation::DeleteNode { .. } => "delete_node",
                Mutation::DeleteEdge { .. } => "delete_edge",
                Mutation::CanonizationTransition { .. } => "canonization_transition",
            };
            kinds.insert(k);
        }
        assert_eq!(kinds.len(), 5);
    }

    #[tokio::test]
    async fn mutations_batch_apply_semantics() {
        let store = MemoryStore::new();
        let batch = load_mutation_batch("mutations-batch").unwrap();
        store.flush(&batch).await.unwrap();
        let sid = SessionId::from("session-mutations");
        let snap = store.load_session(&sid).await.unwrap();
        // kept concept survives; deleted concept gone; transition applied.
        let kept = node_id("f0000000-0000-4000-8000-000000007002");
        let deleted = node_id("f0000000-0000-4000-8000-000000007003");
        assert!(snap.concepts.iter().any(|c| c.id == kept));
        assert!(!snap.concepts.iter().any(|c| c.id == deleted));
        let k = snap.concepts.iter().find(|c| c.id == kept).unwrap();
        assert_eq!(k.canonization_status, CanonizationStatus::Candidate);
        assert_eq!(snap.canonization_events.len(), 1);
    }

    #[test]
    fn recall_goldens_parse() {
        let g = load_recall_goldens().unwrap();
        let cases = g["cases"].as_array().unwrap();
        assert!(cases.len() >= 2);
        for c in cases {
            assert!(!c["phase1_candidates"].as_array().unwrap().is_empty());
            assert!(!c["phase2_expanded"].as_array().unwrap().is_empty());
        }
    }

    #[test]
    fn canonicalization_cases_parse_and_consistent() {
        let cases = load_canonicalization_cases().unwrap();
        let arr = cases.as_array().unwrap();
        assert!(arr.len() >= 10);
        for c in arr {
            assert!(!c["input"].as_str().unwrap().is_empty());
            assert!(!c["expected_key"].as_str().unwrap().is_empty());
            assert!(!c["category"].as_str().unwrap().is_empty());
        }
        // The two semantic near-pair cases must have DISTINCT canonical keys
        // (normalization must NOT merge them; hybrid step 6 does).
        let a = cases
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["category"] == "semantic-near-pair-A")
            .unwrap();
        let b = cases
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["category"] == "semantic-near-pair-B")
            .unwrap();
        assert_ne!(a["expected_key"], b["expected_key"]);
    }
}
