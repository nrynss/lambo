//! Embedder trait, Level B factory, and optional adapter modules (P1 / P7).
//!
//! Packaging: Cargo features gate adapters (`embed-bge`, `embed-fixture`, `embed-bedrock`);
//! `lambo.toml` / env select among compiled kinds. See
//! `dev-diary/notes/level-b-pluggability.md`.

mod math;

#[cfg(feature = "embed-bge")]
mod bge_m3;
#[cfg(feature = "embed-fixture")]
mod fixture;

pub use math::cosine;

#[cfg(feature = "embed-bge")]
pub use bge_m3::BgeM3LlamaCppEmbedder;
#[cfg(feature = "embed-fixture")]
pub use fixture::{near_far_contract, FixtureEmbedder, FAR, NEAR_A, NEAR_B, NEAR_PAIR};

use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize};
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
    /// Embedding dimensionality this backend emits.
    fn dimensions(&self) -> usize;

    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError>;
}

/// Embedding backend selector (TOML `embedder.kind` / `LAMBO_EMBEDDER`).
///
/// Deserialize accepts the same aliases as [`FromStr`] (trimmed, case-insensitive).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbedderKind {
    /// BGE-M3 weights served by a local llama.cpp server (default). Feature: `embed-bge`.
    #[default]
    BgeM3,
    /// Amazon Titan Text Embeddings V2 on Bedrock. Feature: `embed-bedrock` (T7.1).
    Bedrock,
    /// Deterministic offline embedder. Feature: `embed-fixture`.
    Fixture,
}

impl<'de> Deserialize<'de> for EmbedderKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse()
            .map_err(|e: EmbedError| serde::de::Error::custom(e.to_string()))
    }
}

impl EmbedderKind {
    /// Cargo feature name that must be enabled to build this kind.
    pub const fn feature_name(self) -> &'static str {
        match self {
            Self::BgeM3 => "embed-bge",
            Self::Bedrock => "embed-bedrock",
            Self::Fixture => "embed-fixture",
        }
    }

    /// Whether this kind's Cargo feature is compiled into the current binary.
    ///
    /// Note: `true` does not mean the adapter is fully implemented (see [`Self::is_ready`]).
    pub const fn is_compiled(self) -> bool {
        match self {
            Self::BgeM3 => cfg!(feature = "embed-bge"),
            Self::Bedrock => cfg!(feature = "embed-bedrock"),
            Self::Fixture => cfg!(feature = "embed-fixture"),
        }
    }

    /// Whether [`build_embedder`] can return a working adapter (feature on **and** impl exists).
    pub const fn is_ready(self) -> bool {
        match self {
            Self::BgeM3 => cfg!(feature = "embed-bge"),
            Self::Fixture => cfg!(feature = "embed-fixture"),
            // T7.1 not implemented yet.
            Self::Bedrock => false,
        }
    }
}

impl FromStr for EmbedderKind {
    type Err = EmbedError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let t = s.trim();
        if t.is_empty() {
            return Err(EmbedError::Unavailable(
                "empty embedder kind (expected bge_m3 | bedrock | fixture)".into(),
            ));
        }
        match t.to_ascii_lowercase().as_str() {
            "bge_m3" | "bge-m3" | "bge" => Ok(Self::BgeM3),
            "bedrock" | "titan" => Ok(Self::Bedrock),
            "fixture" | "fake" => Ok(Self::Fixture),
            other => Err(EmbedError::Unavailable(format!(
                "unknown embedder kind {other:?} (expected bge_m3 | bedrock | fixture)"
            ))),
        }
    }
}

impl std::fmt::Display for EmbedderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BgeM3 => write!(f, "bge_m3"),
            Self::Bedrock => write!(f, "bedrock"),
            Self::Fixture => write!(f, "fixture"),
        }
    }
}

fn default_embed_dim() -> usize {
    1024
}

/// Resolved configuration for building an embedder.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbedderConfig {
    #[serde(default)]
    pub kind: EmbedderKind,
    #[serde(default = "default_embed_dim")]
    pub dim: usize,
    /// llama.cpp server base URL, e.g. `http://127.0.0.1:8080`.
    #[serde(default, alias = "url")]
    pub llama_url: Option<String>,
    /// Model id sent to llama.cpp (empty => server default).
    #[serde(default, alias = "model")]
    pub llama_model: Option<String>,
}

impl Default for EmbedderConfig {
    fn default() -> Self {
        Self {
            kind: EmbedderKind::BgeM3,
            dim: 1024,
            llama_url: None,
            llama_model: None,
        }
    }
}

impl EmbedderConfig {
    fn env_kind() -> Result<Option<EmbedderKind>, EmbedError> {
        match env::var("LAMBO_EMBEDDER") {
            Ok(s) if !s.trim().is_empty() => Ok(Some(s.parse()?)),
            _ => Ok(None),
        }
    }

    /// Build from environment only. Equivalent to `Self::default().overlay_env()`.
    pub fn from_env() -> Result<Self, EmbedError> {
        Self::default().overlay_env()
    }

    /// Merge env over a base (e.g. from `lambo.toml`). Non-empty env wins over file;
    /// empty env values leave the base intact.
    pub fn overlay_env(mut self) -> Result<Self, EmbedError> {
        if let Some(k) = Self::env_kind()? {
            self.kind = k;
        }
        if let Ok(v) = env::var("LAMBO_EMBED_DIM") {
            if !v.is_empty() {
                self.dim = v.parse().map_err(|e| {
                    EmbedError::Unavailable(format!("invalid LAMBO_EMBED_DIM: {e}"))
                })?;
            }
        }
        if let Ok(v) = env::var("LAMBO_LLAMA_EMBED_URL") {
            if !v.is_empty() {
                self.llama_url = Some(v);
            }
        }
        if let Ok(v) = env::var("LAMBO_LLAMA_MODEL") {
            if !v.is_empty() {
                self.llama_model = Some(v);
            }
        }
        Ok(self)
    }
}

/// Build the configured embedder from environment variables.
pub fn embedder_from_env() -> Result<Box<dyn Embedder>, EmbedError> {
    let cfg = EmbedderConfig::from_env()?;
    build_embedder(cfg)
}

fn missing_feature(kind: EmbedderKind) -> EmbedError {
    EmbedError::Unavailable(format!(
        "embedder kind `{kind}` is not compiled into this binary; rebuild with \
         `--features {}` (see dev-diary/notes/level-b-pluggability.md)",
        kind.feature_name()
    ))
}

// Registry design note (do not "simplify" away):
// * `is_compiled()` is a *message* pre-check ("rebuild with --features X").
// * The real gate is each `#[cfg(feature = "...")]` arm that constructs the type.
// * Both are required: pre-check cannot name uncompiled types; cfg alone is a worse error.

/// Build an embedder from an explicit config (Level B registry).
///
/// Fail-closed when the kind's Cargo feature is off or the adapter is not implemented.
///
/// **Dim is not validated against Cockroach here.** Call
/// [`crate::resolve::resolve_backends`] (or `check_vector_compatibility`) so the
/// *store's* `vector_dimensions()` is the authority.
pub fn build_embedder(cfg: EmbedderConfig) -> Result<Box<dyn Embedder>, EmbedError> {
    if cfg.dim == 0 {
        return Err(EmbedError::Unavailable("embedder dim must be > 0".into()));
    }
    // Pre-check for a clear rebuild hint (see comment above).
    if !cfg.kind.is_compiled() {
        return Err(missing_feature(cfg.kind));
    }
    match cfg.kind {
        EmbedderKind::BgeM3 => {
            #[cfg(feature = "embed-bge")]
            {
                let url = cfg
                    .llama_url
                    .clone()
                    .unwrap_or_else(|| "http://127.0.0.1:8080".to_string());
                let model = cfg.llama_model.unwrap_or_default();
                Ok(Box::new(BgeM3LlamaCppEmbedder::new(url, model, cfg.dim)?))
            }
            #[cfg(not(feature = "embed-bge"))]
            {
                Err(missing_feature(EmbedderKind::BgeM3))
            }
        }
        EmbedderKind::Fixture => {
            #[cfg(feature = "embed-fixture")]
            {
                Ok(Box::new(FixtureEmbedder::with_dimensions(cfg.dim)?))
            }
            #[cfg(not(feature = "embed-fixture"))]
            {
                Err(missing_feature(EmbedderKind::Fixture))
            }
        }
        EmbedderKind::Bedrock => {
            #[cfg(feature = "embed-bedrock")]
            {
                Err(EmbedError::Unavailable(
                    "embed-bedrock is enabled but BedrockEmbedder is not implemented yet (T7.1); \
                     account must also be authorizationStatus=AUTHORIZED"
                        .into(),
                ))
            }
            #[cfg(not(feature = "embed-bedrock"))]
            {
                Err(missing_feature(EmbedderKind::Bedrock))
            }
        }
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
        assert_eq!(
            "bedrock".parse::<EmbedderKind>().unwrap(),
            EmbedderKind::Bedrock
        );
        assert_eq!(
            "fixture".parse::<EmbedderKind>().unwrap(),
            EmbedderKind::Fixture
        );
        assert_eq!(
            "  fake  ".parse::<EmbedderKind>().unwrap(),
            EmbedderKind::Fixture
        );
        assert!("nonsense".parse::<EmbedderKind>().is_err());
        assert!("".parse::<EmbedderKind>().is_err());
        assert!("   ".parse::<EmbedderKind>().is_err());
    }

    #[test]
    fn empty_toml_kind_rejected() {
        let r = toml::from_str::<EmbedderConfig>(r#"kind = """#);
        assert!(r.is_err(), "empty kind string must not parse");
    }

    #[test]
    fn empty_embedder_env_defaults_kind() {
        let _g = crate::test_util::env_lock();

        env::remove_var("LAMBO_EMBEDDER");
        env::remove_var("LAMBO_EMBED_DIM");
        env::remove_var("LAMBO_LLAMA_EMBED_URL");
        env::remove_var("LAMBO_LLAMA_MODEL");
        let cfg = EmbedderConfig::from_env().unwrap();
        assert_eq!(cfg.kind, EmbedderKind::BgeM3);
        assert_eq!(cfg.dim, 1024);

        // Empty string is unset — still BgeM3 (must not call FromStr("") which errors).
        env::set_var("LAMBO_EMBEDDER", "");
        let cfg = EmbedderConfig::from_env().unwrap();
        assert_eq!(cfg.kind, EmbedderKind::BgeM3);
        assert_eq!(
            EmbedderConfig::from_env().unwrap(),
            EmbedderConfig::default().overlay_env().unwrap()
        );
        env::remove_var("LAMBO_EMBEDDER");
    }

    #[test]
    fn unknown_toml_field_rejected() {
        assert!(toml::from_str::<EmbedderConfig>(r#"knd = "bge_m3""#).is_err());
    }

    #[test]
    fn toml_kind_aliases_match_from_str() {
        #[derive(Deserialize)]
        struct Wrap {
            kind: EmbedderKind,
        }
        let w: Wrap = toml::from_str(r#"kind = "bge-m3""#).unwrap();
        assert_eq!(w.kind, EmbedderKind::BgeM3);
        let w: Wrap = toml::from_str(r#"kind = "  titan  ""#).unwrap();
        assert_eq!(w.kind, EmbedderKind::Bedrock);
        let w: Wrap = toml::from_str(r#"kind = "fake""#).unwrap();
        assert_eq!(w.kind, EmbedderKind::Fixture);
    }

    #[test]
    fn partial_embedder_toml_defaults() {
        let cfg: EmbedderConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.kind, EmbedderKind::BgeM3);
        assert_eq!(cfg.dim, 1024);

        // kind only — dim defaults to 1024
        let cfg: EmbedderConfig = toml::from_str("kind = \"fixture\"").unwrap();
        assert_eq!(cfg.kind, EmbedderKind::Fixture);
        assert_eq!(cfg.dim, 1024);
    }

    #[test]
    #[cfg(feature = "embed-fixture")]
    fn builds_fixture_from_config() {
        let e = build_embedder(EmbedderConfig {
            kind: EmbedderKind::Fixture,
            dim: 1024,
            llama_url: None,
            llama_model: None,
        })
        .unwrap();
        assert_eq!(e.dimensions(), 1024);
        assert!(EmbedderKind::Fixture.is_ready());
    }

    #[test]
    fn rejects_zero_dim() {
        let r = build_embedder(EmbedderConfig {
            kind: EmbedderKind::Fixture,
            dim: 0,
            llama_url: None,
            llama_model: None,
        });
        let Err(err) = r else {
            panic!("expected err");
        };
        assert!(err.to_string().contains("dim"), "{err}");
    }

    #[test]
    #[cfg(feature = "embed-fixture")]
    fn accepts_non_default_dim_on_fixture() {
        // Dim is not globally hardwired; MemoryStore has no vector width constraint.
        let e = build_embedder(EmbedderConfig {
            kind: EmbedderKind::Fixture,
            dim: 64,
            llama_url: None,
            llama_model: None,
        })
        .unwrap();
        assert_eq!(e.dimensions(), 64);
    }

    #[test]
    #[cfg(feature = "embed-bge")]
    fn builds_bge_from_config() {
        let e = build_embedder(EmbedderConfig {
            kind: EmbedderKind::BgeM3,
            dim: 1024,
            llama_url: Some("http://127.0.0.1:8080".into()),
            llama_model: None,
        })
        .unwrap();
        assert_eq!(e.dimensions(), 1024);
        assert!(EmbedderKind::BgeM3.is_ready());
    }

    #[test]
    fn bedrock_fail_closed_no_silent_fallback() {
        let r = build_embedder(EmbedderConfig {
            kind: EmbedderKind::Bedrock,
            dim: 1024,
            llama_url: None,
            llama_model: None,
        });
        let Err(err) = r else {
            panic!("expected Unavailable, got Ok — silent fallback forbidden");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("embed-bedrock") || msg.contains("T7.1") || msg.contains("not compiled"),
            "msg={msg}"
        );
        assert!(!msg.to_ascii_lowercase().contains("fixture"));
        assert!(!EmbedderKind::Bedrock.is_ready());
    }

    #[test]
    fn kind_feature_names() {
        assert_eq!(EmbedderKind::BgeM3.feature_name(), "embed-bge");
        assert_eq!(EmbedderKind::Fixture.feature_name(), "embed-fixture");
        assert_eq!(EmbedderKind::Bedrock.feature_name(), "embed-bedrock");
    }

    #[test]
    fn cosine_always_available() {
        // Must compile even when embed-fixture is off (math module is ungated).
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
    }
}
