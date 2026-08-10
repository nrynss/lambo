//! Embedder trait, selector/factory, and offline fixture embedder (P1 / P7).

mod bge_m3;
mod fixture;

pub use bge_m3::BgeM3LlamaCppEmbedder;
pub use fixture::{cosine, near_far_contract, FixtureEmbedder, FAR, NEAR_A, NEAR_B, NEAR_PAIR};

use async_trait::async_trait;
use std::env;
use std::str::FromStr;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EmbedError {
    #[error("embedder unavailable: {0}")]
    Unavailable(String),
    #[error("backend: {0}")]
    Backend(String),
}

/// Pluggable embedding backend (default: BGE-M3 via llama.cpp; Bedrock Titan V2 swap-in).
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Embedding dimensionality (Titan V2 / BGE-M3 default: 1024).
    fn dimensions(&self) -> usize;

    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError>;
}

/// Embedding backend selector, parsed from `LAMBO_EMBEDDER`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EmbedderKind {
    /// BGE-M3 weights served by a local llama.cpp server (default).
    #[default]
    BgeM3,
    /// Amazon Titan Text Embeddings V2 on Bedrock (swap-in; requires account auth, T7.1).
    Bedrock,
    /// Deterministic offline embedder (tests / CI only).
    Fixture,
}

impl FromStr for EmbedderKind {
    type Err = EmbedError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "bge_m3" | "bge-m3" | "bge" | "" => Ok(Self::BgeM3),
            "bedrock" | "titan" => Ok(Self::Bedrock),
            "fixture" | "fake" => Ok(Self::Fixture),
            other => Err(EmbedError::Unavailable(format!(
                "unknown LAMBO_EMBEDDER {other:?} (expected bge_m3 | bedrock | fixture)"
            ))),
        }
    }
}

/// Resolved configuration for building an embedder (see `notes/embeddings-portable.md`).
#[derive(Clone, Debug)]
pub struct EmbedderConfig {
    pub kind: EmbedderKind,
    pub dim: usize,
    /// llama.cpp server base URL, e.g. `http://127.0.0.1:8080`.
    pub llama_url: Option<String>,
    /// Model id sent to llama.cpp (empty => server default).
    pub llama_model: Option<String>,
}

impl EmbedderConfig {
    /// Build a config from environment variables. `LAMBO_EMBEDDER` defaults to `bge_m3`.
    pub fn from_env() -> Result<Self, EmbedError> {
        let kind = env::var("LAMBO_EMBEDDER")
            .unwrap_or_default()
            .parse::<EmbedderKind>()?;
        let dim = env::var("LAMBO_EMBED_DIM")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.parse::<usize>()
                    .map_err(|e| EmbedError::Unavailable(format!("invalid LAMBO_EMBED_DIM: {e}")))
            })
            .transpose()?
            .unwrap_or(1024);

        let llama_url = env::var("LAMBO_LLAMA_EMBED_URL")
            .ok()
            .filter(|s| !s.is_empty());
        let llama_model = env::var("LAMBO_LLAMA_MODEL").ok().filter(|s| !s.is_empty());

        Ok(Self {
            kind,
            dim,
            llama_url,
            llama_model,
        })
    }
}

/// Build the configured embedder from environment variables.
///
/// * `bge_m3` (default) — `BgeM3LlamaCppEmbedder` against `LAMBO_LLAMA_EMBED_URL`.
/// * `fixture` — `FixtureEmbedder` (no network; CI/tests).
/// * `bedrock` — not yet implemented (T7.1); returns `Unavailable` with a clear note.
pub fn embedder_from_env() -> Result<Box<dyn Embedder>, EmbedError> {
    let cfg = EmbedderConfig::from_env()?;
    build_embedder(cfg)
}

/// Build an embedder from an explicit config (injectable for tests).
pub fn build_embedder(cfg: EmbedderConfig) -> Result<Box<dyn Embedder>, EmbedError> {
    // v0.1: the only supported dimension is 1024 (Cockroach `VECTOR(1024)`). Fail fast
    // rather than let an embedder diverge from the schema (adve-review-p7-embeddings R2).
    if cfg.dim != 1024 {
        return Err(EmbedError::Unavailable(format!(
            "v0.1 supports only 1024-dim embeddings (Cockroach VECTOR(1024)); got {}",
            cfg.dim
        )));
    }
    match cfg.kind {
        EmbedderKind::BgeM3 => {
            let url = cfg
                .llama_url
                .clone()
                .unwrap_or_else(|| "http://127.0.0.1:8080".to_string());
            let model = cfg.llama_model.unwrap_or_default();
            Ok(Box::new(BgeM3LlamaCppEmbedder::new(url, model, cfg.dim)?))
        }
        EmbedderKind::Fixture => Ok(Box::new(FixtureEmbedder::new())),
        EmbedderKind::Bedrock => Err(EmbedError::Unavailable(
            "bedrock embedder is a swap-in not yet implemented (T7.1); it also requires \
             the account authorizationStatus=AUTHORIZED. Use LAMBO_EMBEDDER=bge_m3 (default) \
             or fixture."
                .into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_embedder_kind() {
        assert_eq!(
            "bge_m3".parse::<EmbedderKind>().unwrap(),
            EmbedderKind::BgeM3
        );
        assert_eq!(
            "BGE-M3".parse::<EmbedderKind>().unwrap(),
            EmbedderKind::BgeM3
        );
        assert_eq!("".parse::<EmbedderKind>().unwrap(), EmbedderKind::BgeM3);
        assert_eq!(
            "bedrock".parse::<EmbedderKind>().unwrap(),
            EmbedderKind::Bedrock
        );
        assert_eq!(
            "fixture".parse::<EmbedderKind>().unwrap(),
            EmbedderKind::Fixture
        );
        assert!("nonsense".parse::<EmbedderKind>().is_err());
    }

    #[test]
    fn builds_fixture_from_config() {
        let e = build_embedder(EmbedderConfig {
            kind: EmbedderKind::Fixture,
            dim: 1024,
            llama_url: None,
            llama_model: None,
        })
        .unwrap();
        assert_eq!(e.dimensions(), 1024);
    }

    #[test]
    fn fixture_rejects_non_1024_dim() {
        let r = build_embedder(EmbedderConfig {
            kind: EmbedderKind::Fixture,
            dim: 512,
            llama_url: None,
            llama_model: None,
        });
        assert!(matches!(r, Err(EmbedError::Unavailable(_))));
    }

    #[test]
    fn builds_bge_from_config() {
        let e = build_embedder(EmbedderConfig {
            kind: EmbedderKind::BgeM3,
            dim: 1024,
            llama_url: Some("http://127.0.0.1:8080".into()),
            llama_model: None,
        })
        .unwrap();
        assert_eq!(e.dimensions(), 1024);
    }

    #[test]
    fn bedrock_is_unavailable_with_clear_note() {
        let r = build_embedder(EmbedderConfig {
            kind: EmbedderKind::Bedrock,
            dim: 1024,
            llama_url: None,
            llama_model: None,
        });
        assert!(matches!(r, Err(EmbedError::Unavailable(_))));
    }
}
