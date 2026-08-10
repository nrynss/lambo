//! Lambo — agentic graph memory library.
//!
//! Module skeleton for the hackathon build. Contracts land in P1; behavior in later phases.

pub mod canon;
pub mod cli;
pub mod config;
pub mod daemon;
pub mod embed;
pub mod graph;
pub mod mcp;
pub mod recall;
pub mod store;
pub mod types;

pub use config::Config;
pub use store::{Capabilities, GraphStore, MemoryStore};
pub use types::*;
