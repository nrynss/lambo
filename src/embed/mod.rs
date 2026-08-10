//! Embedder trait and offline fixture embedder (P1 / P7).

mod fixture;

pub use fixture::{cosine, near_far_contract, FixtureEmbedder, FAR, NEAR_A, NEAR_B, NEAR_PAIR};

use async_trait::async_trait;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EmbedError {
    #[error("embedder unavailable: {0}")]
    Unavailable(String),
    #[error("backend: {0}")]
    Backend(String),
}

/// Pluggable embedding backend (Bedrock Titan V2 in production).
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Embedding dimensionality (Titan V2 default: 1024).
    fn dimensions(&self) -> usize;

    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError>;
}
