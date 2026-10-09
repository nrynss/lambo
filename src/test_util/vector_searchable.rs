//! A `MemoryStore` that claims vector search, for tests that need the hybrid
//! write path to persist vectors (moved from `memory::tests::replay`, #22 PR 4).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::store::{Capabilities, GraphStore, MemoryStore};
use crate::types::{
    EmbeddingContract, GraphSnapshot, MutationBatch, NodeId, Scored, SessionId, StoreError,
};

/// `MemoryStore` behind a `VECTOR_SEARCH` face whose candidate reads
/// succeed (empty), so hybrid's below-threshold arm actually persists its
/// vectors — the *embedding column* is what the J3 assertions read.
pub struct VectorSearchable(pub Arc<MemoryStore>);

#[async_trait]
impl GraphStore for VectorSearchable {
    fn capabilities(&self) -> Capabilities {
        self.0.capabilities() | Capabilities::VECTOR_SEARCH
    }
    async fn init_schema(&self) -> Result<(), StoreError> {
        self.0.init_schema().await
    }
    async fn flush(&self, batch: &MutationBatch, token: Option<u64>) -> Result<(), StoreError> {
        self.0.flush(batch, token).await
    }
    async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
        self.0.load_session(session).await
    }
    async fn keyword_candidates(
        &self,
        session: &SessionId,
        tokens: &[String],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.0.keyword_candidates(session, tokens, limit).await
    }
    async fn vector_candidates(
        &self,
        _session: &SessionId,
        _embedding: &[f32],
        _limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        Ok(Vec::new())
    }
    async fn vector_candidates_checked(
        &self,
        _session: &SessionId,
        _embedding: &[f32],
        _expected_contract: &EmbeddingContract,
        _limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        Ok(Vec::new())
    }
    async fn blast_radius(
        &self,
        session: &SessionId,
        node: NodeId,
        min_edge_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<u64, StoreError> {
        self.0.blast_radius(session, node, min_edge_age, now).await
    }
    async fn interaction_span(
        &self,
        session: &SessionId,
        node: NodeId,
        min_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<crate::types::InteractionSpan, StoreError> {
        self.0.interaction_span(session, node, min_age, now).await
    }
    async fn record_canonization(
        &self,
        event: &crate::types::CanonizationEvent,
        token: Option<u64>,
    ) -> Result<(), StoreError> {
        self.0.record_canonization(event, token).await
    }
    async fn acquire_lease(
        &self,
        session: &SessionId,
        holder: &crate::store::LeaseHolder,
        ttl: Duration,
    ) -> Result<crate::store::LeaseOutcome, StoreError> {
        self.0.acquire_lease(session, holder, ttl).await
    }
    async fn read_lease(
        &self,
        session: &SessionId,
    ) -> Result<Option<crate::store::LeaseInfo>, StoreError> {
        self.0.read_lease(session).await
    }
    async fn refresh_lease(
        &self,
        session: &SessionId,
        holder: &crate::store::LeaseHolder,
        ttl: Duration,
    ) -> Result<crate::store::LeaseOutcome, StoreError> {
        self.0.refresh_lease(session, holder, ttl).await
    }
    async fn release_lease(
        &self,
        session: &SessionId,
        holder: &crate::store::LeaseHolder,
    ) -> Result<(), StoreError> {
        self.0.release_lease(session, holder).await
    }
}
