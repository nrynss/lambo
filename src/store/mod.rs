//! GraphStore trait and adapters (P1 / P3).

mod memory;

pub use memory::MemoryStore;

use async_trait::async_trait;
use bitflags::bitflags;
use std::time::Duration;

use crate::types::{
    CanonizationEvent, GraphSnapshot, InteractionSpan, MutationBatch, NodeId, Scored, SessionId,
    StoreError,
};

bitflags! {
    /// Adapter capabilities (spec §3.2).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Capabilities: u32 {
        const VECTOR_SEARCH = 0b0001;
        const HISTORY = 0b0010;
    }
}

/// Durable / query surface — Lambo vocabulary only (spec §3.2).
#[async_trait]
pub trait GraphStore: Send + Sync {
    async fn init_schema(&self) -> Result<(), StoreError>;
    fn capabilities(&self) -> Capabilities;

    async fn flush(&self, batch: &MutationBatch) -> Result<(), StoreError>;
    async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError>;

    async fn keyword_candidates(
        &self,
        session: &SessionId,
        tokens: &[String],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError>;

    /// Requires [`Capabilities::VECTOR_SEARCH`]; adapters without it return
    /// [`StoreError::Capability`].
    async fn vector_candidates(
        &self,
        session: &SessionId,
        embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError>;

    async fn blast_radius(
        &self,
        session: &SessionId,
        node: NodeId,
        min_edge_age: Duration,
    ) -> Result<u64, StoreError>;

    async fn interaction_span(
        &self,
        session: &SessionId,
        node: NodeId,
        min_age: Duration,
    ) -> Result<InteractionSpan, StoreError>;

    async fn record_canonization(&self, event: &CanonizationEvent) -> Result<(), StoreError>;
}
