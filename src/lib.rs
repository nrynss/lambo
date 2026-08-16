//! Lambo — agentic graph memory library.
//!
//! Lambo gives a group of agents a shared memory that is a graph rather than a
//! transcript. Agents write what they learn and what they did; Lambo links it,
//! scores it, and hands back the part that matters for the next query.
//!
//! # The primary type
//!
//! Everything routes through [`Memory`]. The MCP server and the CLI are both
//! thin wrappers over it.
//!
//! ```no_run
//! # async fn example() -> Result<(), lambo::LamboError> {
//! use lambo::{resolve_backends, LamboFile, Memory, RecallQuery};
//!
//! // In a real program the file comes from `lambo.toml`.
//! let backends = resolve_backends(LamboFile::default())?;
//!
//! let mem = Memory::builder()
//!     .session("demo")
//!     .agent("agent-a")
//!     .backends(backends)
//!     .build()
//!     .await?;
//!
//! let result = mem.recall(RecallQuery {
//!     query: "user schema".into(),
//!     top_k: 5,
//!     max_tokens: 500,
//!     traversal_depth: 2,
//! }).await?;
//! println!("{}", result.context);
//!
//! mem.close().await?;
//! # Ok(())
//! # }
//! ```
//!
//! # How a session behaves
//!
//! * **One writer.** A session has exactly one [`Memory`] writing it, held by a
//!   lease that a second writer is refused against by name. Readers query the
//!   store directly and take no lease.
//! * **Durability is eventual.** Writes land in an in-memory graph and a
//!   write-behind log, and flush to the store on an interval, in batched
//!   statements. [`Memory::stats`] reports the current lag, which is the loss
//!   bound. There is no on-disk journal, so a tail abandoned by a close that
//!   ran out of time is lost rather than replayed.
//! * **Canonization is earned.** Concepts become canonical facts from
//!   structural evidence, not because an agent declared one important. See
//!   [`CanonizationStatus`] and [`CanonizationEvent`].
//! * **Text is validated on the way in.** Control characters other than tab and
//!   newline, and invisible formatting characters such as bidi overrides, are
//!   refused. Invisible characters that real writing needs are kept in content
//!   and ignored when matching concepts, so they cannot mint a hidden duplicate.
//!
//! # Pluggable backends
//!
//! Backends are **Level B** pluggable: Cargo features compile adapters in, and
//! `lambo.toml` or the environment selects among them at runtime. One binary
//! carries every adapter it was built with. See
//! `dev-diary/notes/level-b-pluggability.md`.
//!
//! Implement [`GraphStore`] for durable storage and [`Embedder`] for vectors.
//! A store declares what it can do through [`Capabilities`]: only a store
//! reporting `VECTOR_SEARCH` serves recall's vector leg, and the others fall
//! back to keyword matching plus graph expansion.
//!
//! # Where to look next
//!
//! * [`Memory`] and [`MemoryBuilder`] — the API surface.
//! * [`types`] — the contracts every surface shares.
//! * [`store`] and [`embed`] — the adapter traits and the shipped adapters.
//! * [`mcp`] and [`cli`] — the two user-facing surfaces.

pub mod canon;
pub mod cli;
pub mod config;
pub mod daemon;
pub mod embed;
#[cfg(feature = "fixtures")]
pub mod fixtures;
pub mod graph;
pub mod mcp;
pub mod memory;
pub mod recall;
pub mod resolve;

pub mod store;
#[cfg(test)]
pub mod test_util;
pub mod types;

pub use config::{Config, DaemonConfig, LamboFile, RecallWeights, ScoringWeights};
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
pub use memory::{CanonicalMemory, DryRun, ImpactReport, Memory, MemoryBuilder, MemoryStats};

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
