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
//! rescore signal. That signal is the warm-up rescore the T4.1 daemon task
//! skeleton is *intended* to wake on (planned, not yet present), transported
//! via the T4.6 event channel — today the channel exists only as
//! [`crate::types::DaemonEvent`]; neither the transport nor the skeleton is in
//! the tree yet. Once T4.1/T4.6 land, they wire it up — this module only
//! provides the materialization.
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
//! comes from the snapshot. The async core is [`load_session_async`]; the
//! synchronous (pinned API) [`load_session`] is a thin wrapper that bridges it
//! to a private worker thread with its own current-thread runtime, so it is
//! callable from sync startup code and from inside a tokio task alike (a direct
//! `Handle::block_on` would panic in the latter case). The bridge bounds the
//! store call with `LOAD_SESSION_TIMEOUT` (F2) so a hung store cannot block
//! the sync caller forever.

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

/// Default timeout for the sync bridge's store call (F2): without it the
/// worker thread would block forever on a hung store.
const LOAD_SESSION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Load a session from a durable store into RAM (spec §2.5) — **async core**.
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
///
/// This is the async CORE (F4): the sync [`load_session`] is a thin wrapper
/// running it on a private worker thread via the bridge (with
/// `load_session_with_timeout`'s timeout).
pub async fn load_session_async(
    store: &dyn GraphStore,
    session: &SessionId,
) -> Result<LoadedSession, StoreError> {
    let mut snap = match store.load_session(session).await {
        Ok(snap) => snap,
        Err(StoreError::SessionNotFound(_)) => {
            return Ok(LoadedSession {
                graph: Graph::new(session.clone()),
                index: InvertedIndex::new(),
            })
        }
        Err(e) => return Err(e),
    };
    quarantine_legacy_vectors(&mut snap);
    // Index first (borrows the snapshot), then move the snapshot into the graph.
    let index = InvertedIndex::from_snapshot(&snap);
    let graph = Graph::from_snapshot(snap).map_err(lambo_to_store)?;
    Ok(LoadedSession { graph, index })
}

/// Safely upgrade pre-contract snapshots. Width alone cannot identify the
/// model that produced a legacy vector, so materialization strips such vectors
/// rather than inventing a compatible contract. The durable rows remain
/// available for an explicit re-embedding migration.
fn quarantine_legacy_vectors(snap: &mut crate::types::GraphSnapshot) {
    if snap.embedding.is_some() {
        return;
    }
    let mut stripped = 0;
    for concept in &mut snap.concepts {
        if concept.embedding.take().is_some() {
            stripped += 1;
        }
    }
    if stripped > 0 {
        tracing::warn!(
            target: "lambo::store::load",
            session_id = %snap.session_id,
            stripped_vectors = stripped,
            "quarantined legacy vectors with no embedding contract; explicit re-embedding is required"
        );
    }
}

/// [`load_session_async`] bounded by a store-call timeout (F2): a hung store
/// must surface as a typed [`StoreError::Backend`] instead of blocking the
/// sync caller forever. Runs inside the worker-thread runtime (the bridge),
/// where `tokio::time::timeout` can actually fire.
async fn load_session_with_timeout(
    store: &dyn GraphStore,
    session: &SessionId,
    timeout: std::time::Duration,
) -> Result<LoadedSession, StoreError> {
    match tokio::time::timeout(timeout, load_session_async(store, session)).await {
        Ok(result) => result,
        Err(_elapsed) => Err(StoreError::Backend(format!(
            "load_session timed out after {timeout:?}"
        ))),
    }
}

/// Load a session from a durable store into RAM (spec §2.5) — **sync
/// wrapper** over [`load_session_async`] (F4), with the same semantics.
///
/// Runs the async core on a private worker thread (see `block_on`) so it is
/// callable from sync startup code and from inside a tokio task alike. The
/// store call is bounded by `LOAD_SESSION_TIMEOUT` (F2): a hung store yields
/// `StoreError::Backend("load_session timed out after 30s")` instead of a
/// permanent block.
pub fn load_session(
    store: &dyn GraphStore,
    session: &SessionId,
) -> Result<LoadedSession, StoreError> {
    block_on(load_session_with_timeout(
        store,
        session,
        LOAD_SESSION_TIMEOUT,
    ))?
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
                .map_err(|e| {
                    StoreError::Backend(format!("load_session: build tokio runtime: {e}"))
                })?;
            Ok(rt.block_on(fut))
        })
        .join()
        .map_err(|payload| {
            let detail = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("unknown panic");
            StoreError::Backend(format!("load_session: worker thread panicked: {detail}"))
        })?
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::{DateTime, TimeZone, Utc};
    use uuid::Uuid;

    use super::*;
    use crate::graph::demote::demote;
    use crate::graph::derive::{derive, ParentOf};
    use crate::graph::reserve::reserve;
    #[cfg(feature = "store-memory")]
    use crate::store::memory::MemoryStore;
    #[cfg(feature = "store-memory")]
    use crate::store::Capabilities;
    use crate::types::{
        AgentId, CanonizationEvent, CanonizationStatus, ConceptType, EdgeType, Interaction,
        Mutation, NodeId, Reservation, SessionId,
    };
    #[cfg(feature = "store-memory")]
    use crate::types::{Concept, GraphSnapshot, InteractionSpan, MutationBatch, Node, Scored};
    #[cfg(feature = "store-memory")]
    use async_trait::async_trait;

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

    #[cfg(feature = "store-memory")]
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
            &[
                ("user schema", ConceptType::Entity),
                ("api layer", ConceptType::Logic),
            ],
            &ParentOf::from_pairs(&[("user schema", "api layer")]),
            10,
        )
        .unwrap();

        // Observations via demote (one per sentence, chunk-grouped).
        let observations = demote(
            &mut g,
            i2_id,
            &agent_a(),
            "Drift note. Second drift note.",
            "chunk-1",
        )
        .unwrap();
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
            last_demotion_time: None,
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
    #[cfg(feature = "store-memory")]
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
        store.flush(&batch, None).await.unwrap();

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
    #[cfg(feature = "store-memory")]
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
    #[cfg(feature = "store-memory")]
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
        store.flush(&batch, None).await.unwrap();
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
        store.flush(&batch, None).await.unwrap();
        let err = load_session(store, &concept_sid).unwrap_err();
        assert!(
            matches!(err, StoreError::Invariant(_)),
            "expected typed Invariant error, got {err:?}"
        );
    }

    /// load_session is a sync function and must work even when the calling
    /// thread has no tokio runtime (plain `#[test]` thread): it runs the
    /// store's async future on its own worker thread.
    #[cfg(feature = "store-memory")]
    #[test]
    fn load_session_works_without_a_tokio_runtime() {
        let store = MemoryStore::new();
        let sid = SessionId::from("sync");
        let mut batch = MutationBatch::new();
        batch.push(Mutation::UpsertNode {
            node: Node::Interaction(interaction(&sid, 1, None, 0)),
        });
        block_on(store.flush(&batch, None))
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
        assert_eq!(
            g.edge_between(
                g.temporal_chain()[1],
                g.temporal_chain()[0],
                EdgeType::Temporal,
            )
            .map(|e| e.weight),
            Some(1.0)
        );
    }

    /// `GraphStore` mock whose `load_session` never resolves — exercises the
    /// sync bridge's store-call timeout (F2). Everything else delegates to an
    /// inner `MemoryStore` (unused by the timeout test, but the trait requires
    /// a full impl).
    #[cfg(feature = "store-memory")]
    struct HangingStore {
        inner: MemoryStore,
    }

    #[cfg(feature = "store-memory")]
    #[async_trait]
    impl GraphStore for HangingStore {
        async fn init_schema(&self) -> Result<(), StoreError> {
            self.inner.init_schema().await
        }

        fn capabilities(&self) -> Capabilities {
            self.inner.capabilities()
        }

        async fn flush(&self, batch: &MutationBatch, token: Option<u64>) -> Result<(), StoreError> {
            self.inner.flush(batch, token).await
        }

        async fn load_session(&self, _session: &SessionId) -> Result<GraphSnapshot, StoreError> {
            std::future::pending().await
        }

        async fn keyword_candidates(
            &self,
            session: &SessionId,
            tokens: &[String],
            limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.inner.keyword_candidates(session, tokens, limit).await
        }

        async fn vector_candidates(
            &self,
            session: &SessionId,
            embedding: &[f32],
            expected_contract: &crate::types::EmbeddingContract,
            limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.inner
                .vector_candidates(session, embedding, expected_contract, limit)
                .await
        }

        async fn blast_radius(
            &self,
            session: &SessionId,
            node: NodeId,
            min_edge_age: Duration,
            now: DateTime<Utc>,
        ) -> Result<u64, StoreError> {
            self.inner
                .blast_radius(session, node, min_edge_age, now)
                .await
        }

        async fn interaction_span(
            &self,
            session: &SessionId,
            node: NodeId,
            min_age: Duration,
            now: DateTime<Utc>,
        ) -> Result<InteractionSpan, StoreError> {
            self.inner
                .interaction_span(session, node, min_age, now)
                .await
        }

        async fn record_canonization(
            &self,
            event: &CanonizationEvent,
            token: Option<u64>,
        ) -> Result<(), StoreError> {
            self.inner.record_canonization(event, token).await
        }
    }

    /// F2: the worker-thread bridge must not block forever on a store whose
    /// `load_session` never resolves — the parameterized timeout surfaces a
    /// typed `Backend` error instead.
    #[cfg(feature = "store-memory")]
    #[test]
    fn sync_bridge_times_out_on_a_hung_store() {
        let store: &dyn GraphStore = &HangingStore {
            inner: MemoryStore::new(),
        };
        let sid = SessionId::from("hung");
        let err = block_on(load_session_with_timeout(
            store,
            &sid,
            Duration::from_millis(50),
        ))
        .expect("load_session timeout test: worker thread")
        .expect_err("hung store must time out, not return");
        match err {
            StoreError::Backend(msg) => assert!(
                msg.contains("load_session timed out after"),
                "expected timeout message, got {msg:?}"
            ),
            other => panic!("expected Backend timeout error, got {other:?}"),
        }
    }

    /// F4: the async core is the same load as the sync wrapper — a flush ->
    /// `load_session_async` round-trip deep-equals the sync path (which runs
    /// the same core on the bridge), and both restore the same state.
    #[cfg(feature = "store-memory")]
    #[tokio::test]
    async fn load_session_async_matches_sync_round_trip() {
        let (mut g, _, _) = build_session();
        g.assert_invariants().unwrap();

        let mut expected = g.snapshot();
        expected.synonyms.clear();
        expected.reservations.clear();
        let batch = g.drain_log();
        assert!(!batch.is_empty());

        let store: &dyn GraphStore = &MemoryStore::new();
        store.flush(&batch, None).await.unwrap();

        let loaded = load_session_async(store, &SessionId::from("roundtrip"))
            .await
            .unwrap();
        assert_eq!(loaded.graph.snapshot(), expected);
        assert_eq!(loaded.graph.log_len(), 0);
        loaded.graph.assert_invariants().unwrap();

        // Sync wrapper runs the same core: identical graph + index.
        let sync_loaded = load_session(store, &SessionId::from("roundtrip")).unwrap();
        assert_eq!(loaded.graph.snapshot(), sync_loaded.graph.snapshot());
        for q in ["user schema", "api layer", "drift"] {
            assert_eq!(loaded.index.search(q, 10), sync_loaded.index.search(q, 10));
        }
    }

    /// F4: `load_session_async` keeps the missing-session contract — a
    /// `SessionNotFound` from the store yields a fresh empty session, not an
    /// error.
    #[cfg(feature = "store-memory")]
    #[tokio::test]
    async fn load_session_async_missing_session_returns_fresh_empty_session() {
        let store: &dyn GraphStore = &MemoryStore::new();
        let ghost = SessionId::from("never-written-async");
        let loaded = load_session_async(store, &ghost).await.unwrap();

        assert_eq!(loaded.graph.session_id(), &ghost);
        assert_eq!(loaded.graph.node_count(), 0);
        assert_eq!(loaded.graph.log_len(), 0);
        loaded.graph.assert_invariants().unwrap();
        assert!(loaded.index.search("anything", 10).is_empty());
    }
}
