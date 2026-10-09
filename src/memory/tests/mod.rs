//! Unit tests for `Memory`, grouped by subject.

use super::leases::ACTIVE_SESSIONS;
use super::writes::RETRACT_IO_TIMEOUT;
use super::*;
use crate::embed::FixtureEmbedder;
use crate::graph::action::Action;
use crate::graph::derive::ParentOf;
use crate::store::flush::FLUSH_ATTEMPT_TIMEOUT;
use crate::store::lease::{LeaseOutcome, LEASE_TTL};
use crate::store::Capabilities;
use crate::store::MemoryStore;
use crate::test_util::capture_logs;
// Only the fixtures-gated replay test builds an `Interaction` by hand.
#[cfg(feature = "fixtures")]
use crate::types::Interaction;
use crate::types::{
    tie_break_by_key, CanonizationEvent, GraphSnapshot, InteractionSpan, Mutation, MutationBatch,
    Scored, StoreError,
};
use crate::types::{
    CanonizationStatus, ConceptType, LamboError, MatchStrategy, NodeId, RecallQuery,
};
use async_trait::async_trait;
use chrono::DateTime;
use chrono::Utc;
use std::collections::HashSet;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

mod access;
mod attach;
mod leases;
mod query_cache;
mod reads;
mod replay;
mod shutdown;
mod writes;

fn contract(kind: &str, dim: usize) -> EmbeddingContract {
    EmbeddingContract {
        kind: kind.into(),
        model: None,
        dim,
    }
}

async fn memory_on(store: Arc<dyn GraphStore>, session: &str) -> Memory {
    Memory::builder()
        .session(session)
        .agent("agent-a")
        // A long flush interval keeps the background loop out of every
        // assertion: `close()` is what must make the tail durable.
        .flush_interval(Duration::from_secs(3_600))
        .store(store)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .build()
        .await
        .expect("build")
}

/// `GraphStore` that fails the first `fail_next` flushes (or every flush,
/// with `fail_next = usize::MAX`), so a batch is RETAINED inside the flush
/// task's pending buffer. Records every batch length it was handed.
struct FlakyStore {
    inner: Arc<dyn GraphStore>,
    fail_remaining: AtomicUsize,
    /// Every batch it was handed, whole: the T81-3 assertions need the
    /// mutation **sequence**, not just the lengths.
    batches: PlMutex<Vec<MutationBatch>>,
}

impl FlakyStore {
    fn new(inner: Arc<dyn GraphStore>, fail_next: usize) -> Self {
        Self {
            inner,
            fail_remaining: AtomicUsize::new(fail_next),
            batches: PlMutex::new(Vec::new()),
        }
    }

    fn batches(&self) -> Vec<MutationBatch> {
        self.batches.lock().clone()
    }

    fn batch_lens(&self) -> Vec<usize> {
        self.batches.lock().iter().map(|b| b.len()).collect()
    }
}

#[async_trait]
impl GraphStore for FlakyStore {
    async fn init_schema(&self) -> Result<(), StoreError> {
        self.inner.init_schema().await
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    async fn flush(&self, batch: &MutationBatch, token: Option<u64>) -> Result<(), StoreError> {
        self.batches.lock().push(batch.clone());
        let should_fail = self
            .fail_remaining
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n > 0).then(|| n - 1)
            })
            .is_ok();
        if should_fail {
            Err(StoreError::Backend("simulated outage".into()))
        } else {
            self.inner.flush(batch, token).await
        }
    }
    async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
        self.inner.load_session(session).await
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
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.inner
            .vector_candidates(session, embedding, limit)
            .await
    }
    async fn vector_candidates_checked(
        &self,
        session: &SessionId,
        embedding: &[f32],
        expected_contract: &EmbeddingContract,
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.inner
            .vector_candidates_checked(session, embedding, expected_contract, limit)
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

/// How a store charges for a flush (L82-1).
#[derive(Clone, Copy, Debug, PartialEq)]
enum CostModel {
    /// What both SQL adapters did before L82-1: one statement, and so one
    /// network round-trip, per mutation in the batch.
    PerMutation,
    /// What they do now: one statement per planned [`FlushStep`].
    PerPlannedStatement,
}

/// A store that charges network latency for a flush, so a test can ask
/// whether a tail drains inside `close()`'s window (L82-1).
///
/// The `+ 2` on every count is `BEGIN` and `COMMIT`, which both adapters pay
/// once per flush regardless of the batch.
struct RoundTripStore {
    inner: Arc<dyn GraphStore>,
    model: CostModel,
    /// Per-statement round-trip. The live cluster (CockroachDB serverless,
    /// GCP asia-south1) measured 10–30 ms.
    rtt: Duration,
    round_trips: AtomicUsize,
    released: AtomicUsize,
}

impl RoundTripStore {
    fn new(inner: Arc<dyn GraphStore>, model: CostModel, rtt: Duration) -> Self {
        Self {
            inner,
            model,
            rtt,
            round_trips: AtomicUsize::new(0),
            released: AtomicUsize::new(0),
        }
    }

    fn round_trips(&self) -> usize {
        self.round_trips.load(Ordering::SeqCst)
    }

    fn releases(&self) -> usize {
        self.released.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl GraphStore for RoundTripStore {
    async fn flush(&self, batch: &MutationBatch, token: Option<u64>) -> Result<(), StoreError> {
        let statements = match self.model {
            CostModel::PerMutation => batch.mutations.len(),
            CostModel::PerPlannedStatement => crate::store::batch::planned_statements(
                &batch.mutations,
                crate::store::batch::BulkLimits {
                    interactions: 1,
                    concepts: 256,
                    edges: 512,
                    accesses: 256,
                },
            ),
        } + 2;
        self.round_trips.fetch_add(statements, Ordering::SeqCst);
        tokio::time::sleep(self.rtt * u32::try_from(statements).unwrap()).await;
        self.inner.flush(batch, token).await
    }
    async fn release_lease(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
    ) -> Result<(), StoreError> {
        self.released.fetch_add(1, Ordering::SeqCst);
        self.inner.release_lease(session, holder).await
    }
    async fn init_schema(&self) -> Result<(), StoreError> {
        self.inner.init_schema().await
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
        self.inner.load_session(session).await
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
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.inner
            .vector_candidates(session, embedding, limit)
            .await
    }
    async fn vector_candidates_checked(
        &self,
        session: &SessionId,
        embedding: &[f32],
        expected_contract: &EmbeddingContract,
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.inner
            .vector_candidates_checked(session, embedding, expected_contract, limit)
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

/// Four `record_action` calls at the 64-target fan-out cap — the live L82-1
/// repro's burst — left un-flushed in the log.
async fn at_cap_burst(mem: &Memory) {
    for call in 0..4 {
        let produces: Vec<String> = (0..32).map(|n| format!("artifact {call}-{n}")).collect();
        let depends_on: Vec<String> = (0..32).map(|n| format!("dependency {call}-{n}")).collect();
        let produces_refs: Vec<&str> = produces.iter().map(String::as_str).collect();
        let depends_refs: Vec<&str> = depends_on.iter().map(String::as_str).collect();
        mem.record_action(&Action {
            event_time: None,
            action: &format!("burst action {call}"),
            produces: &produces_refs,
            modifies: &[],
            depends_on: &depends_refs,
        })
        .expect("at-cap record_action");
    }
}

/// Which store call [`ParkingStore`] suspends (once) until released.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ParkPoint {
    /// `retract`'s durable-radius query — the await inside a **write**,
    /// which is where the T81-1 race lives.
    BlastRadius,
    /// `recall`'s vector leg — the await inside a **read**, to show that
    /// `close()` does not wait for readers.
    VectorCandidates,
}

/// Delegating store that parks the **first** call to one chosen method on a
/// `Notify` and reports (on another `Notify`) that it got there. The
/// reviewer's deterministic race probe, kept as a fixture.
struct ParkingStore {
    inner: Arc<dyn GraphStore>,
    park_on: ParkPoint,
    armed: AtomicBool,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl ParkingStore {
    fn new(inner: Arc<dyn GraphStore>, park_on: ParkPoint) -> Self {
        Self {
            inner,
            park_on,
            armed: AtomicBool::new(true),
            entered: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Notified once the parked call is suspended. `notify_one` latches, so
    /// awaiting this after the fact still works.
    fn entered(&self) -> Arc<tokio::sync::Notify> {
        self.entered.clone()
    }

    fn release(&self) {
        self.release.notify_one();
    }

    async fn park(&self, point: ParkPoint) {
        if point != self.park_on || !self.armed.swap(false, Ordering::SeqCst) {
            return;
        }
        self.entered.notify_one();
        self.release.notified().await;
    }
}

#[async_trait]
impl GraphStore for ParkingStore {
    async fn init_schema(&self) -> Result<(), StoreError> {
        self.inner.init_schema().await
    }
    fn capabilities(&self) -> Capabilities {
        // Claimed so `recall` actually takes its vector leg (MemoryStore
        // itself has none); the leg then fails and recall degrades, which
        // is fine — the park is what the test needs.
        match self.park_on {
            ParkPoint::VectorCandidates => self.inner.capabilities() | Capabilities::VECTOR_SEARCH,
            ParkPoint::BlastRadius => self.inner.capabilities(),
        }
    }
    async fn flush(&self, batch: &MutationBatch, token: Option<u64>) -> Result<(), StoreError> {
        self.inner.flush(batch, token).await
    }
    async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
        self.inner.load_session(session).await
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
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.inner
            .vector_candidates(session, embedding, limit)
            .await
    }
    async fn vector_candidates_checked(
        &self,
        session: &SessionId,
        embedding: &[f32],
        expected_contract: &EmbeddingContract,
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.park(ParkPoint::VectorCandidates).await;
        self.inner
            .vector_candidates_checked(session, embedding, expected_contract, limit)
            .await
    }
    async fn blast_radius(
        &self,
        session: &SessionId,
        node: NodeId,
        min_edge_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<u64, StoreError> {
        self.park(ParkPoint::BlastRadius).await;
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

fn query(text: &str) -> RecallQuery {
    RecallQuery {
        query: text.into(),
        top_k: 5,
        max_tokens: 500,
        traversal_depth: 0,
    }
}
