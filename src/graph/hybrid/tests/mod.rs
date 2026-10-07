//! Unit tests for hybrid derive, grouped by subject.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use tokio::sync::{Barrier, Notify};
use uuid::Uuid;

use super::*;
use crate::embed::{EmbedError, Embedder, FixtureEmbedder, FAR, NEAR_A, NEAR_B};

use crate::types::{Interaction, MutationBatch, Scored};

mod embedding;
mod merging;
mod planning;

fn ts(minutes: i64) -> DateTime<Utc> {
    let base = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
    base + chrono::Duration::minutes(minutes)
}

fn sid(name: &str) -> SessionId {
    SessionId::from(name)
}

fn agent() -> AgentId {
    AgentId::from("agent-a")
}

fn contract(kind: &str, dim: usize) -> EmbeddingContract {
    EmbeddingContract {
        kind: kind.into(),
        model: None,
        dim,
    }
}

/// Interaction with a recognizable origin `prompt_text` (the calibration
/// context the hybrid step must embed alongside the concept name).
fn interaction(id: u64, prev: Option<NodeId>, at_min: i64, prompt: &str) -> Interaction {
    Interaction {
        event_time: None,
        id: NodeId(Uuid::from_u64_pair(1, id)),
        session_id: sid("hybrid-test"),
        agent_id: agent(),
        prompt_text: Some(prompt.to_string()),
        previous_id: prev,
        created_at: ts(at_min),
    }
}

fn graph_with_interaction(
    sess: &str,
    id: u64,
    at_min: i64,
    prompt: &str,
) -> (Arc<RwLock<Graph>>, NodeId) {
    let mut g = Graph::new(sid(sess));
    let mut i = interaction(id, None, at_min, prompt);
    i.session_id = sid(sess);
    let iid = i.id;
    g.insert_interaction(i).unwrap();
    (Arc::new(RwLock::new(g)), iid)
}

/// Deterministic `Scored<NodeId>: {item, score}` helper.
fn hit(id: NodeId, score: f64) -> Scored<NodeId> {
    Scored { item: id, score }
}

/// Store double advertising configurable capabilities and returning canned
/// vector hits (mirrors recall's `SpyVectorStore`). A non-`Capability`
/// backend error from `vector_candidates_checked` can be forced for the propagate
/// case. Any async method other than `vector_candidates_checked` panics so a test
/// cannot silently reach the store through the wrong surface.
struct SpyStore {
    caps: Capabilities,
    hits: Vec<Scored<NodeId>>,
    backend_err: bool,
    vector_calls: Arc<AtomicUsize>,
}

impl SpyStore {
    fn with_vector(hits: Vec<Scored<NodeId>>) -> Self {
        Self {
            caps: Capabilities::VECTOR_SEARCH | Capabilities::HISTORY,
            hits,
            backend_err: false,
            vector_calls: Arc::new(AtomicUsize::new(0)),
        }
    }
    fn without_vector() -> Self {
        Self {
            caps: Capabilities::HISTORY,
            hits: Vec::new(),
            backend_err: false,
            vector_calls: Arc::new(AtomicUsize::new(0)),
        }
    }
    fn failing() -> Self {
        Self {
            caps: Capabilities::VECTOR_SEARCH | Capabilities::HISTORY,
            hits: Vec::new(),
            backend_err: true,
            vector_calls: Arc::new(AtomicUsize::new(0)),
        }
    }
    fn vector_calls(&self) -> usize {
        self.vector_calls.load(Ordering::SeqCst)
    }
    fn unexpected(&self) -> ! {
        panic!("SpyStore: unexpected async store call (only vector_candidates_checked allowed)")
    }
}
#[async_trait]
impl GraphStore for SpyStore {
    async fn init_schema(&self) -> Result<(), StoreError> {
        self.unexpected()
    }
    fn capabilities(&self) -> Capabilities {
        self.caps
    }
    async fn flush(&self, _batch: &MutationBatch, _token: Option<u64>) -> Result<(), StoreError> {
        self.unexpected()
    }
    async fn load_session(
        &self,
        _session: &SessionId,
    ) -> Result<crate::types::GraphSnapshot, StoreError> {
        self.unexpected()
    }
    async fn keyword_candidates(
        &self,
        _session: &SessionId,
        _tokens: &[String],
        _limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.unexpected()
    }
    async fn vector_candidates(
        &self,
        _session: &SessionId,
        _embedding: &[f32],
        _limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.unexpected()
    }
    async fn vector_candidates_checked(
        &self,
        _session: &SessionId,
        _embedding: &[f32],
        _expected_contract: &EmbeddingContract,
        _limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.vector_calls.fetch_add(1, Ordering::SeqCst);
        if self.backend_err {
            return Err(StoreError::Backend("boom".into()));
        }
        Ok(self.hits.clone())
    }
    async fn blast_radius(
        &self,
        _session: &SessionId,
        _node: NodeId,
        _min_edge_age: std::time::Duration,
        _now: chrono::DateTime<chrono::Utc>,
    ) -> Result<u64, StoreError> {
        self.unexpected()
    }
    async fn interaction_span(
        &self,
        _session: &SessionId,
        _node: NodeId,
        _min_age: std::time::Duration,
        _now: chrono::DateTime<chrono::Utc>,
    ) -> Result<crate::types::InteractionSpan, StoreError> {
        self.unexpected()
    }
    async fn record_canonization(
        &self,
        _event: &crate::types::CanonizationEvent,
        _token: Option<u64>,
    ) -> Result<(), StoreError> {
        self.unexpected()
    }
}

/// Records every text handed to `embed` (so the context rule is assertable)
/// while delegating the actual vector to `FixtureEmbedder` — the production
/// FixtureEmbedder used by the whole test suite for near/far geometry.
#[derive(Debug, Clone)]
struct RecordingEmbedder {
    inner: FixtureEmbedder,
    texts: Arc<Mutex<Vec<String>>>,
}

impl RecordingEmbedder {
    fn new() -> Self {
        Self {
            inner: FixtureEmbedder::new(),
            texts: Arc::new(Mutex::new(Vec::new())),
        }
    }
    fn embedded_texts(&self) -> Vec<String> {
        self.texts.lock().unwrap().clone()
    }
}

#[async_trait]
impl Embedder for RecordingEmbedder {
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        self.texts.lock().unwrap().push(text.to_string());
        self.inner.embed(text).await
    }
}

/// Embedder whose `embed` always fails (the degradation / logged-once case).
#[derive(Debug, Clone)]
struct FailingEmbedder {
    calls: Arc<AtomicUsize>,
}
impl FailingEmbedder {
    fn new() -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}
#[async_trait]
impl Embedder for FailingEmbedder {
    fn dimensions(&self) -> usize {
        1024
    }
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(EmbedError::Unavailable("server down".into()))
    }
}
