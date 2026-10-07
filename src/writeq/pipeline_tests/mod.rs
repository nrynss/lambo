//! End-to-end tests of the write pipeline through `Memory`, grouped by
//! subject.

use super::*;
use crate::embed::FixtureEmbedder;
use crate::types::Interaction;
use crate::MemoryStore;
use std::sync::atomic::AtomicUsize;

mod admission;
mod calibration;
mod drain;
mod receipts;

/// An embedder that parks until it is released, so a burst can be held in
/// the queue long enough to observe the bound.
struct HeldEmbedder {
    gate: Arc<tokio::sync::Semaphore>,
    inner: FixtureEmbedder,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Embedder for HeldEmbedder {
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::EmbedError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let permit = self
            .gate
            .acquire()
            .await
            .expect("the gate outlives its holders");
        permit.forget();
        self.inner.embed(text).await
    }
}

struct Rig {
    pipeline: WritePipeline,
    graph: Arc<RwLock<Graph>>,
    now: Arc<PlMutex<DateTime<Utc>>>,
}

impl Rig {
    /// A pipeline over an empty in-RAM session, with a clock this test can
    /// move and an optional embedder gate.
    fn new(session: &str, embedder: Arc<dyn Embedder>) -> Self {
        Self::new_with_ledger(session, embedder, None)
    }

    /// [`Rig::new`], with a call ledger attached so J4's durable-intent
    /// completion lines land on a real file a test can read back.
    fn new_with_ledger(
        session: &str,
        embedder: Arc<dyn Embedder>,
        ledger: Option<Arc<crate::ledger::Ledger>>,
    ) -> Self {
        let session = SessionId::new(session);
        let graph = Arc::new(RwLock::new(Graph::new(session.clone())));
        let index = Arc::new(RwLock::new(InvertedIndex::default()));
        let now = Arc::new(PlMutex::new(Utc::now()));
        let clock_now = now.clone();
        let clock: crate::daemon::Clock = Arc::new(move || *clock_now.lock());
        let ctx = WriteCtx {
            session,
            graph: graph.clone(),
            index,
            store: Arc::new(MemoryStore::new()),
            embedder,
            embedding: EmbeddingContract {
                kind: "fixture".into(),
                model: None,
                dim: 1024,
            },
            match_strategy: MatchStrategy::Canonical,
            max_cooccurrence_per_derive: 10,
            semantic_match_threshold: 0.85,
            daemon_wake: Arc::new(Notify::new()),
            lease_lost: Arc::new(AtomicBool::new(false)),
            ledger,
        };
        Rig {
            pipeline: WritePipeline::spawn(ctx, clock),
            graph,
            now,
        }
    }

    fn fixture(session: &str) -> Self {
        Self::new(session, Arc::new(FixtureEmbedder::new()))
    }

    /// Open an interaction the way the call path does, so a job has
    /// somewhere to hang.
    fn interaction(&self, agent: &AgentId) -> NodeId {
        let id = NodeId::new();
        let mut g = self.graph.write();
        let previous_id = g.temporal_chain().last().copied();
        let session_id = g.session_id().clone();
        g.insert_interaction(Interaction {
            event_time: None,
            id,
            session_id,
            agent_id: agent.clone(),
            prompt_text: None,
            previous_id,
            created_at: *self.now.lock(),
        })
        .expect("insert interaction");
        id
    }

    async fn derive(&self, agent: &AgentId, content: &str) -> Submitted {
        let interaction = self.interaction(agent);
        self.pipeline
            .submit_derive(
                agent.clone(),
                interaction,
                vec![(content.to_string(), ConceptType::Entity)],
                Vec::new(),
            )
            .await
    }
}

// -----------------------------------------------------------------------
// The J3-R1-1 cluster: a projection is not a bound
// -----------------------------------------------------------------------

/// A store that advertises `VECTOR_SEARCH`, because `hybrid::derive` skips
/// the embedder entirely when the store has none — and a queue test whose
/// jobs never embed measures nothing about the drain. The same load-bearing
/// wrapper as `the_ack_lands_before_the_embedder_is_called`'s.
struct VectorCapable(MemoryStore);

#[async_trait::async_trait]
impl GraphStore for VectorCapable {
    fn capabilities(&self) -> crate::store::Capabilities {
        crate::store::Capabilities::VECTOR_SEARCH
    }
    async fn init_schema(&self) -> Result<(), crate::types::StoreError> {
        self.0.init_schema().await
    }
    fn vector_dimensions(&self) -> Option<usize> {
        self.0.vector_dimensions()
    }
    async fn flush(
        &self,
        batch: &crate::types::MutationBatch,
        token: Option<u64>,
    ) -> Result<(), crate::types::StoreError> {
        self.0.flush(batch, token).await
    }
    async fn load_session(
        &self,
        session: &SessionId,
    ) -> Result<crate::types::GraphSnapshot, crate::types::StoreError> {
        self.0.load_session(session).await
    }
    async fn keyword_candidates(
        &self,
        session: &SessionId,
        tokens: &[String],
        limit: usize,
    ) -> Result<Vec<crate::types::Scored<NodeId>>, crate::types::StoreError> {
        self.0.keyword_candidates(session, tokens, limit).await
    }
    async fn vector_candidates(
        &self,
        session: &SessionId,
        embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<crate::types::Scored<NodeId>>, crate::types::StoreError> {
        self.0.vector_candidates(session, embedding, limit).await
    }
    async fn blast_radius(
        &self,
        session: &SessionId,
        node: NodeId,
        min_edge_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<u64, crate::types::StoreError> {
        self.0.blast_radius(session, node, min_edge_age, now).await
    }
    async fn interaction_span(
        &self,
        session: &SessionId,
        node: NodeId,
        min_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<crate::types::InteractionSpan, crate::types::StoreError> {
        self.0.interaction_span(session, node, min_age, now).await
    }
    async fn record_canonization(
        &self,
        event: &crate::types::CanonizationEvent,
        token: Option<u64>,
    ) -> Result<(), crate::types::StoreError> {
        self.0.record_canonization(event, token).await
    }
}

/// An embedder that costs a fixed wall-clock delay per call and
/// parallelises perfectly — **the exact shape a concurrent probe rewards
/// and a single-consumer lane cannot exploit.** Four of these together
/// finish in one delay; four in a row take four.
struct SlowEmbedder {
    delay: Duration,
    inner: FixtureEmbedder,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Embedder for SlowEmbedder {
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::EmbedError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        tokio::time::sleep(self.delay).await;
        self.inner.embed(text).await
    }
}

impl Rig {
    /// A rig whose writes actually embed: `Hybrid` against a store that
    /// advertises `VECTOR_SEARCH`.
    fn hybrid(session: &str, embedder: Arc<dyn Embedder>) -> Self {
        let mut rig = Self::new(session, embedder);
        let ctx = Arc::get_mut(&mut rig.pipeline.ctx).expect("sole owner at build");
        ctx.match_strategy = MatchStrategy::Hybrid;
        ctx.store = Arc::new(VectorCapable(MemoryStore::new()));
        rig
    }
}

/// Spin until `cond` holds, with a deadline, so a broken invariant fails
/// the test instead of hanging the suite.
async fn until(mut cond: impl FnMut() -> bool, what: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !cond() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// Let the calibration probe through a gated embedder without knowing how
/// many embeds it takes, then take back whatever it did not use so real
/// work still parks.
async fn calibrate_through_gate(rig: &Rig, gate: &tokio::sync::Semaphore) -> Calibration {
    let calibration = loop {
        if let Some(c) = rig.pipeline.calibration() {
            break c;
        }
        gate.add_permits(1);
        tokio::time::sleep(Duration::from_millis(1)).await;
    };
    while gate.available_permits() > 0 {
        if let Ok(p) = gate.try_acquire() {
            p.forget();
        }
    }
    calibration
}

/// An embedder whose cost is **proportional to its input's length** —
/// the shape every transformer has, and the shape `PROBE_TEXT`'s old
/// docstring denied ("it is measuring the deployment's embedder, not its own
/// input"). One millisecond per 5 bytes here, so the probe's 35-byte text
/// costs 7 ms and a 512-byte concept costs 102 ms: a 14.6x gap, which the
/// estimator era projected bounds through and the redesign only reports.
struct LengthProportionalEmbedder {
    per_5_bytes: Duration,
    inner: FixtureEmbedder,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Embedder for LengthProportionalEmbedder {
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::EmbedError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        tokio::time::sleep(self.per_5_bytes * (text.len() as u32 / 5)).await;
        self.inner.embed(text).await
    }
}
