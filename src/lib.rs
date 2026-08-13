//! Lambo — agentic graph memory library.
//!
//! Backends are **Level B** pluggable: Cargo features compile adapters in;
//! `lambo.toml` / env select among them. See `dev-diary/notes/level-b-pluggability.md`.

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
pub mod resolve;

pub mod store;
#[cfg(test)]
pub mod test_util;
pub mod types;

pub use config::{Config, LamboFile, RecallWeights, ScoringWeights};
#[cfg(feature = "embed-bge")]
pub use embed::BgeM3LlamaCppEmbedder;

pub use canon::{CanonizationTask, EvalOutcome, EvalParams, Evaluator};
pub use daemon::{Daemon, ScoreTable};
pub use embed::{
    build_embedder, cosine, embedder_from_env, EmbedError, Embedder, EmbedderConfig, EmbedderKind,
};
#[cfg(feature = "embed-fixture")]
pub use embed::{near_far_contract, FixtureEmbedder, FAR, NEAR_A, NEAR_B, NEAR_PAIR};
pub use graph::Graph;

pub use resolve::{
    assert_session_embedding_compatible, check_vector_compatibility, resolve_backends,
    resolve_from_config_path, resolve_store_only, ResolvedBackends,
};

#[cfg(feature = "store-memory")]
pub use store::MemoryStore;
pub use store::{build_store, store_from_env, Capabilities, GraphStore, StoreConfig, StoreKind};

// Explicit re-exports (no `types::*` glob — keeps the public surface auditable).
pub use types::{
    AgentId, CanonizationEvent, CanonizationStatus, Concept, ConceptType, DaemonEvent, Edge,
    EdgeType, EmbeddingContract, GraphSnapshot, Interaction, InteractionSpan, LamboError,
    MatchStrategy, Mutation, MutationBatch, Node, NodeId, RecallHit, RecallQuery, RecallResult,
    Reservation, Scored, SessionId, StoreError, Synonym,
};
