//! Lambo — agentic graph memory library.
//!
//! Module skeleton for the hackathon build. Contracts land in P1; behavior in later phases.

pub mod canon;
pub mod cli;
pub mod config;
pub mod daemon;
pub mod embed;
#[cfg(feature = "fixtures")]
pub mod fixtures;
pub mod graph;
pub mod mcp;
pub mod recall;
pub mod store;
pub mod types;

pub use config::{Config, RecallWeights, ScoringWeights};
pub use embed::{
    build_embedder, embedder_from_env, BgeM3LlamaCppEmbedder, EmbedError, Embedder, EmbedderConfig,
    EmbedderKind, FixtureEmbedder,
};
pub use store::{Capabilities, GraphStore, MemoryStore};
// Explicit re-exports (no `types::*` glob — keeps the public surface auditable).
pub use types::{
    AgentId, CanonizationEvent, CanonizationStatus, Concept, ConceptType, DaemonEvent, Edge,
    EdgeType, GraphSnapshot, Interaction, InteractionSpan, LamboError, MatchStrategy, Mutation,
    MutationBatch, Node, NodeId, RecallHit, RecallQuery, RecallResult, Reservation, Scored,
    SessionId, StoreError, Synonym,
};
