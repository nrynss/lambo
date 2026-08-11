//! T3.5 — `load_session()` / startup materialization (spec §2.5).
//!
//! Turns a durable [`crate::types::GraphSnapshot`] into a live session in RAM:
//! the [`Graph`]
//! (via [`Graph::from_snapshot`], which re-verifies every §5.7 invariant before
//! returning) and the session's [`InvertedIndex`] rebuilt from the **same**
//! snapshot (T2.6's rebuild-from-graph path). A missing session is a first use,
//! not an error — see [`load_session`].
//!
//! ## Daemon warm-up rescore seam (spec §2.5)
//!
//! The daemon's warm-up rescore over a freshly loaded session is **not** this
//! module's job and is deliberately not implemented or stubbed here. The seam:
//! the session owner (P3's `Memory` type, which owns `Arc<RwLock<Graph>>` plus
//! the [`InvertedIndex`]) calls [`load_session`] at startup and, when the store
//! returned an **existing** session (never for a fresh empty one), emits the
//! rescore signal. That signal is P4's event channel (T4.6,
//! `src/daemon/events.rs`, a `tokio::sync::broadcast` of `DaemonEvent`); the
//! daemon task skeleton (T4.1, `src/daemon/mod.rs`) already lists "warm-up
//! rescore on load (T3.5's signal)" as its wake source. Once the channel
//! exists, T4.1/T4.6 wires it — this module only provides the materialization.
//!
//! ## Lock discipline (spec §6.4)
//!
//! [`load_session`] is a pure constructor — there is no graph to lock yet, so it
//! never takes one. The owner installs the returned [`LoadedSession`] under its
//! `Arc<RwLock<…>>` and must not hold that lock across any `.await` afterwards.
//!
//! ## Determinism
//!
//! No `Utc::now` anywhere in this module: every timestamp in the loaded session
//! comes from the snapshot. [`load_session`] is synchronous (pinned API) and
//! bridges to the store's async trait method on a private worker thread with
//! its own current-thread runtime, so it is callable from sync startup code and
//! from inside a tokio task alike (a direct `Handle::block_on` would panic in
//! the latter case).

use std::future::Future;

use crate::graph::index::InvertedIndex;
use crate::graph::Graph;
use crate::types::{LamboError, SessionId, StoreError};

use super::GraphStore;

/// A session materialized into RAM: the graph plus its rebuilt inverted index.
#[derive(Debug)]
pub struct LoadedSession {
    pub graph: Graph,
    pub index: InvertedIndex,
}

/// Load a session from a durable store into RAM (spec §2.5).
///
/// * `store.load_session(session)` -> `Ok(snap)`: materialize the graph
///   ([`Graph::from_snapshot`], which re-verifies every §5.7 invariant) and
///   rebuild the inverted index ([`InvertedIndex::from_snapshot`]) from the
///   same snapshot.
/// * `Err(StoreError::SessionNotFound)`: return a fresh **empty** session
///   (`Graph::new(session)` + `InvertedIndex::new()`) — a missing session is a
///   first use, not an error.
/// * Any other store error propagates unchanged. A corrupted snapshot (an
///   invariant violation) surfaces from [`Graph::from_snapshot`] as a typed
///   [`StoreError::Invariant`] — never a panic.
pub fn load_session(
    store: &dyn GraphStore,
    session: &SessionId,
) -> Result<LoadedSession, StoreError> {
    let snap = match block_on(store.load_session(session))? {
        Ok(snap) => snap,
        Err(StoreError::SessionNotFound(_)) => {
            return Ok(LoadedSession {
                graph: Graph::new(session.clone()),
                index: InvertedIndex::new(),
            })
        }
        Err(e) => return Err(e),
    };
    // Index first (borrows the snapshot), then move the snapshot into the graph.
    let index = InvertedIndex::from_snapshot(&snap);
    let graph = Graph::from_snapshot(snap).map_err(lambo_to_store)?;
    Ok(LoadedSession { graph, index })
}

/// Flatten a [`LamboError`] from the graph tier into store vocabulary.
///
/// [`Graph::from_snapshot`] only produces `LamboError::Store` variants
/// (invariant / not-found); the fallback keeps any future variant typed rather
/// than losing it to a string.
fn lambo_to_store(err: LamboError) -> StoreError {
    match err {
        LamboError::Store(e) => e,
        other => StoreError::Other(other.into()),
    }
}

/// Run a store future to completion on a private worker thread.
///
/// A fresh current-thread runtime per call is fine here: `load_session` is a
/// startup-time operation (once per session), not a hot path. The dedicated
/// thread is what makes the call safe from *both* sync contexts (no runtime in
/// scope) and async contexts (where `Handle::block_on` / `Runtime::block_on`
/// panic with "Cannot block the current thread from within a runtime").
fn block_on<F>(fut: F) -> Result<F::Output, StoreError>
where
    F: Future + Send,
    F::Output: Send,
{
    std::thread::scope(|s| {
        s.spawn(move || -> Result<F::Output, StoreError> {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| StoreError::Backend(format!("load_session: build tokio runtime: {e}")))?;
            Ok(rt.block_on(fut))
        })
        .join()
        .map_err(|_| StoreError::Backend("load_session: worker thread panicked".to_string()))?
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::{DateTime, TimeZone, Utc};
    use uuid::Uuid;

    use super::*;
    use crate::graph::derive::{derive, ParentOf};
    use crate::graph::demote::demote;
    use crate::graph::reserve::reserve;
    use crate::store::memory::MemoryStore;
    use crate::types::{
        AgentId, CanonizationEvent, CanonizationStatus, Concept, ConceptType, EdgeType,
        Interaction, Mutation, MutationBatch, Node, NodeId, Reservation, SessionId,
    };

    /// Fixed clock base (mirrors the graph-tier test convention).
    fn ts(minutes: i64) -> DateTime<Utc> {
        let base = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
        base + chrono::Duration::minutes(minutes)
    }

    fn agent_a() -> AgentId {
        AgentId::from("agent-a")
    }

    fn agent_b() -> AgentId {
        AgentId::from("agent-b")
    }

    fn interaction(sid: &SessionId, id: u64, prev: Option<NodeId>, at_min: i64) -> Interaction {
        Interaction {
            id: NodeId(Uuid::from_u64_pair(1, id)),
            session_id: sid.clone(),
            agent_id: agent_a(),
            prompt_text: Some(format!("prompt {id}")),
            previous_id: prev,
            created_at: ts(at_min),
        }
    }

    fn concept(sid: &SessionId, id: u64, origin: NodeId, content: &str) -> Concept {
        Concept {
            id: NodeId(Uuid::from_u64_pair(2, id)),
            session_id: sid.clone(),
            content: content.into(),
            canonical_key: "orphan concept".into(),
            concept_type: ConceptType::Entity,
            origin_interaction: origin,
            origin_agent: agent_a(),
            created_at: ts(0),
            access_count: 0,
            last_accessed: None,
            gc_survived: 0,
            canonization_status: CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: None,
            embedding: None,
            chunk_group_id: None,
        }
    }

    /// Build the fixture-like session exercised by the round-trip test:
    /// interactions (chain of two), a derive (concepts + Derives +
    /// CoOccurrence + Hierarchical edges), a demote (Observations), a synonym,
    /// a canonization transition, and a reservation with an explicit clock.
    /// Returns the graph, the reserved concept's id, and the reservation.
    fn build_session() -> (Graph, NodeId, Reservation) {
        let sid = SessionId::from("roundtrip");
        let mut g = Graph::new(sid.clone());

        let i1 = interaction(&sid, 1, None, 0);
        let i1_id = i1.id;
        g.insert_interaction(i1).unwrap();
        let i2 = interaction(&sid, 2, Some(i1_id), 5);
        let i2_id = i2.id;
        g.insert_interaction(i2).unwrap();

        // Concepts via derive: two Entity/Logic concepts + a Hierarchical pair
        // + a pairwise CoOccurrence edge.
        derive(
            &mut g,
            i2_id,
            &agent_a(),
            &[("user schema", ConceptType::Entity), ("api layer", ConceptType::Logic)],
            &ParentOf::from_pairs(&[("user schema", "api layer")]),
            10,
        )
        .unwrap();

        // Observations via demote (one per sentence, chunk-grouped).
        let observations = demote(&mut g, i2_id, &agent_a(), "Drift note. Second drift note.", "chunk-1").unwrap();
        assert_eq!(observations.len(), 2);

        let user_schema_id = g
            .concepts()
            .find(|c| c.content == "user schema")
            .expect("derive created it")
            .id;

        // RAM-local metadata: synonym + canonization transition (persist via
        // the snapshot only — no Mutation kind for either).
        g.declare_synonym("us", "user schema");
        g.apply_canonization_transition(CanonizationEvent {
            id: NodeId(Uuid::from_u64_pair(4, 1)),
            session_id: g.session_id().clone(),
            node_id: user_schema_id,
            from_status: CanonizationStatus::None,
            to_status: CanonizationStatus::Candidate,
            blast_radius: Some(2),
            occurred_at: ts(12),
        })
        .unwrap();

        // Reservation via reserve with an explicit clock (no Utc::now).
        let reservation = reserve(
            &mut g,
            user_schema_id,
            &agent_b(),
            Duration::from_secs(3600),
            ts(10),
        )
        .unwrap();

        (g, user_schema_id, reservation)
    }

    /// Round-trip through the write-behind path: mutate -> assert invariants ->
    /// drain -> flush -> load, then deep-compare graph and index. Written
    /// generically against `&dyn GraphStore` (a `MemoryStore` instance) so the
    /// same shape runs against `SqliteStore` unchanged once T3.3 lands.
    ///
    /// RAM-local metadata (synonyms, reservations — S5) has no `Mutation` kind,
    /// so the write-behind log cannot carry it: flush -> load restores those as
    /// empty (asserted below); their preservation is the full-snapshot path,
    /// covered by `full_snapshot_round_trip_preserves_ram_local_metadata`.
    #[tokio::test]
    async fn round_trip_via_flush_materializes_mutation_carried_state() {
        let (mut g, user_schema_id, _reservation) = build_session();
        g.assert_invariants().unwrap();

        let mut expected = g.snapshot();
        // The mutation log carries nodes/edges/transitions only (spec §2.4,
        // S5): drop the RAM-local metadata to get the store-faithful oracle.
        expected.synonyms.clear();
        expected.reservations.clear();
        let batch = g.drain_log();
        assert!(!batch.is_empty());

        let store: &dyn GraphStore = &MemoryStore::new();
        store.flush(&batch).await.unwrap();

        let loaded = load_session(store, &SessionId::from("roundtrip")).unwrap();

        // Deep equality with the pre-flush snapshot (interactions in temporal
        // chain order, concepts, edges, canonization trail, metadata fields).
        assert_eq!(loaded.graph.snapshot(), expected);
        assert_eq!(loaded.graph.log_len(), 0, "load must not seed mutations");
        assert_eq!(loaded.graph.epoch(), 0);
        loaded.graph.assert_invariants().unwrap();

        // RAM-local metadata is NOT in the write-behind materialization — the
        // S5 contract, asserted explicitly so nobody "fixes" it by inventing
        // mutations for synonyms/reservations.
        assert_eq!(loaded.graph.synonyms().count(), 0);
        assert_eq!(loaded.graph.reservations().len(), 0);

        // Index: rebuilt from the snapshot, agrees with an independently built
        // reference on a fixture-like query, and finds the expected concepts.
        let reference = InvertedIndex::from_snapshot(&expected);
        for q in ["user schema", "api layer", "drift"] {
            let got = loaded.index.search(q, 10);
            assert!(!got.is_empty(), "query {q:?} must hit the loaded index");
            assert_eq!(got, reference.search(q, 10));
        }
        assert_eq!(
            loaded.index.search("user schema", 10)[0].item,
            user_schema_id,
            "top hit for the fixture query"
        );
        let drift_ids: Vec<NodeId> = loaded
            .index
            .search("drift", 10)
            .into_iter()
            .map(|s| s.item)
            .collect();
        assert_eq!(drift_ids.len(), 2, "both observations indexed");
        assert!(loaded.index.search("zzzz-nothing", 10).is_empty());
    }

    /// Full-snapshot round-trip: seed (the full `GraphSnapshot` save path, S5)
    /// -> load_session -> the loaded session deep-equals the original graph
    /// **including** synonyms and the reservation, which only the snapshot
    /// carries. Fixtures-gated because `MemoryStore::seed` is.
    #[cfg(feature = "fixtures")]
    #[tokio::test]
    async fn full_snapshot_round_trip_preserves_ram_local_metadata() {
        let (g, user_schema_id, reservation) = build_session();
        g.assert_invariants().unwrap();
        let original = g.snapshot();

        let store = MemoryStore::new();
        store.seed(original.clone()).unwrap();

        let loaded = load_session(&store, &SessionId::from("roundtrip")).unwrap();

        // Deep equality of the full snapshot, RAM-local metadata included.
        assert_eq!(loaded.graph.snapshot(), original);
        assert_eq!(loaded.graph.log_len(), 0);
        loaded.graph.assert_invariants().unwrap();

        // Reservation preserved through the full-snapshot round-trip.
        assert_eq!(loaded.graph.reservation(user_schema_id), Some(&reservation));
        assert_eq!(loaded.graph.synonym("us"), Some("user schema"));
        assert_eq!(loaded.graph.canonization_events().len(), 1);

        // Index agrees with an independently rebuilt reference.
        let reference = InvertedIndex::from_snapshot(&original);
        for q in ["user schema", "api layer", "drift"] {
            assert_eq!(loaded.index.search(q, 10), reference.search(q, 10));
        }
    }

    /// Missing session -> Ok(LoadedSession) with an empty graph and an empty
    /// index (first use, not an error).
    #[tokio::test]
    async fn missing_session_returns_fresh_empty_session() {
        let store: &dyn GraphStore = &MemoryStore::new();
        let ghost = SessionId::from("never-written");
        let loaded = load_session(store, &ghost).unwrap();

        assert_eq!(loaded.graph.session_id(), &ghost);
        assert_eq!(loaded.graph.node_count(), 0);
        assert_eq!(loaded.graph.edge_count(), 0);
        assert_eq!(loaded.graph.log_len(), 0);
        loaded.graph.assert_invariants().unwrap();
        assert!(loaded.index.search("anything", 10).is_empty());
    }

    /// Corrupted snapshot -> typed error from load_session, never a panic.
    /// Corruptions are injected through the store's own flush path (MemoryStore
    /// does not validate chain shape / Derives edges), so the test needs no
    /// fixtures feature.
    #[tokio::test]
    async fn corrupted_snapshot_returns_typed_error_not_panic() {
        let store: &dyn GraphStore = &MemoryStore::new();

        // Case 1: interaction whose temporal predecessor is missing — the
        // chain rebuild in from_snapshot rejects it.
        let chain_sid = SessionId::from("corrupt-chain");
        let mut batch = MutationBatch::new();
        batch.push(Mutation::UpsertNode {
            node: Node::Interaction(interaction(&chain_sid, 1, Some(NodeId::nil()), 0)),
        });
        store.flush(&batch).await.unwrap();
        let err = load_session(store, &chain_sid).unwrap_err();
        assert!(
            matches!(err, StoreError::Invariant(_)),
            "expected typed Invariant error, got {err:?}"
        );

        // Case 2: concept with no Derives edge — assert_invariants rejects it.
        let concept_sid = SessionId::from("corrupt-concept");
        let mut batch = MutationBatch::new();
        batch.push(Mutation::UpsertNode {
            node: Node::Concept(concept(&concept_sid, 1, NodeId::nil(), "orphan")),
        });
        store.flush(&batch).await.unwrap();
        let err = load_session(store, &concept_sid).unwrap_err();
        assert!(
            matches!(err, StoreError::Invariant(_)),
            "expected typed Invariant error, got {err:?}"
        );
    }

    /// load_session is a sync function and must work even when the calling
    /// thread has no tokio runtime (plain `#[test]` thread): it runs the
    /// store's async future on its own worker thread.
    #[test]
    fn load_session_works_without_a_tokio_runtime() {
        let store = MemoryStore::new();
        let sid = SessionId::from("sync");
        let mut batch = MutationBatch::new();
        batch.push(Mutation::UpsertNode {
            node: Node::Interaction(interaction(&sid, 1, None, 0)),
        });
        block_on(store.flush(&batch))
            .expect("load_session test: worker thread")
            .expect("flush failed");

        let loaded = load_session(&store, &sid).unwrap();
        assert_eq!(loaded.graph.node_count(), 1);
        assert_eq!(
            loaded.graph.temporal_chain(),
            &[NodeId(Uuid::from_u64_pair(1, 1))]
        );
        loaded.graph.assert_invariants().unwrap();
    }

    /// Sanity: the round-trip batch's mutation kinds match what the store
    /// replays (the write-behind contract the flush task relies on).
    #[test]
    fn build_session_produces_a_well_formed_log() {
        let (mut g, _, _) = build_session();
        g.assert_invariants().unwrap();
        let batch = g.drain_log();
        let has_node = batch
            .mutations
            .iter()
            .any(|m| matches!(m, Mutation::UpsertNode { .. }));
        let has_edge = batch
            .mutations
            .iter()
            .any(|m| matches!(m, Mutation::UpsertEdge { .. }));
        assert!(has_node && has_edge);
        assert!(batch
            .mutations
            .iter()
            .any(|m| matches!(m, Mutation::CanonizationTransition { .. })));
        assert_eq!(g.edge_between(
            g.temporal_chain()[1],
            g.temporal_chain()[0],
            EdgeType::Temporal,
        ).map(|e| e.weight), Some(1.0));
    }
}
