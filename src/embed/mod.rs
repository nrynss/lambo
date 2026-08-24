//! Embedder trait, Level B factory, and optional adapter modules (P1 / P7).
//!
//! Packaging: Cargo features gate adapters (`embed-bge`, `embed-candle`, `embed-fixture`, `embed-bedrock`);
//! `lambo.toml` / env select among compiled kinds. See
//! `dev-diary/notes/level-b-pluggability.md`.

mod math;

#[cfg(feature = "embed-bge")]
mod bge_m3;
#[cfg(feature = "embed-candle")]
mod candle;
#[cfg(feature = "embed-fixture")]
mod fixture;
#[cfg(feature = "embed-gemini")]
pub(crate) mod gemini;

pub use math::cosine;

#[cfg(feature = "embed-bge")]
pub use bge_m3::BgeM3LlamaCppEmbedder;
#[cfg(feature = "embed-candle")]
pub use candle::CandleEmbedder;
#[cfg(feature = "embed-fixture")]
pub use fixture::{near_far_contract, FixtureEmbedder, FAR, NEAR_A, NEAR_B, NEAR_PAIR};
#[cfg(feature = "embed-gemini")]
pub use gemini::GeminiEmbedder;

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

impl EmbedError {
    /// **Could a later attempt at the same input plausibly succeed?** (J3
    /// round-1 N1.)
    ///
    /// The durable-intent replay has to choose between two irreversible acts —
    /// settle an acked write `failed`, or leave it durable for the next process
    /// — and the choice turns entirely on this question. It is answered from the
    /// *variant*, at the site that already knows the cause, because the
    /// alternative is matching on message text: J1-R2-2's lesson is that a class
    /// must be a type, and its converse is that a `format!`ed error is not a
    /// classification.
    ///
    /// * [`Self::Unavailable`] — **transient.** The backend was never reached,
    ///   or reached and answered with a status the rule table classes as
    ///   transient or unclassified. The shipped BGE-M3 adapter produces it
    ///   from a transport failure (`llama.cpp unreachable`) and from every
    ///   status in [`crate::embed::bge_m3::classify_status`]'s `Transient` /
    ///   `Unclassified` classes (408/425/429/500/502/503/504, any un-named
    ///   5xx, and unrecognised statuses). Nothing about the *input* was
    ///   rejected, so the same input against a healthy server is untried.
    /// * [`Self::Backend`] — **permanent for this input or this deployment.**
    ///   The backend answered and the answer was unusable: a status the rule
    ///   table classifies as content (400/413/415/422 — a genuine refusal of
    ///   this text) or permanent-config (401/403/404 — a wrong URL, model, or
    ///   credentials), unparseable JSON, the wrong dimensionality, a
    ///   non-finite or zero-norm vector.
    ///
    /// **Where this is imprecise, stated rather than hidden (J3-R2R-1).** The
    /// durability decision turns on whether a non-success status speaks about
    /// the *input*, the *deployment*, or the *server's momentary state* — HTTP
    /// collapses those into three buckets plus "no rule". The adapter classifies
    /// at the site that knows the status ([`crate::embed::bge_m3::classify_status`]),
    /// and statuses that do not mention the input are `Transient` -> here, so a
    /// `503`/`502`/`429` from a live embedder is no longer mistaken for a content
    /// refusal. Two further things bound a residual misclassification rather than
    /// letting it cost the backlog: the replay path runs a liveness embed
    /// *before* its loop (so a whole outage never reaches this classifier), and
    /// a session-wide status fault (the embedder answering transiently for every
    /// intent) is caught by the replay's sequential decision rule — after
    /// [`crate::writeq::EMBEDDER_SICK_THRESHOLD`] consecutive transients the loop
    /// stops and leaves the remaining backlog durable. So a misclassification
    /// costs at most that threshold of intents, never the backlog.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Unavailable(_) => true,
            Self::Backend(_) => false,
        }
    }
}

/// Pluggable embedding backend (default: BGE-M3 via llama.cpp; Bedrock Titan V2 swap-in).
///
/// **Input contract (CON-7):** `embed` MUST reject empty / whitespace-only text with
/// [`EmbedError::Unavailable`] — no implementation may embed the empty string. The
/// production path already gates content at derive/record_action entry (GRAPH-8), so a
/// blank string reaching an embedder is a caller bug; every backend must fail it the
/// same way rather than return a vector (an empty-input vector would silently poison
/// hybrid ranking on a degraded path).
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Embedding dimensionality this backend emits.
    fn dimensions(&self) -> usize;

    /// Embed `text`.
    ///
    /// # Output contract: vectors MUST be L2-normalized (unit norm)
    ///
    /// Every returned vector must satisfy `‖v‖₂ ≈ 1`. This is not a style
    /// preference — it is the precondition that makes the storage tiers *agree*, and
    /// it was previously honoured by all implementations but documented by none
    /// (F-R1-4).
    ///
    /// The two vector-capable adapters rank by different formulas, and the formulas
    /// coincide **only** on unit-norm input:
    ///
    /// * SQLite (and the `MemoryStore` reference) score `crate::embed::cosine`, which
    ///   divides by both norms and is therefore norm-invariant.
    /// * Cockroach scores `1 − d²/2` over the `<->` L2 distance, which is *not*
    ///   norm-invariant; it equals cosine exactly when both operands are unit vectors.
    ///
    /// A non-normalizing embedder would make the two tiers disagree by a wide margin
    /// rather than marginally — a stored `[2,0,0,0]` against probe `[1,0,0,0]` scores
    /// `1.0` on SQLite and `0.5` on Cockroach — and would silently break the
    /// `semantic_match_threshold` calibration shared by both. Recall parity between
    /// tiers rests on this contract.
    ///
    /// Shipped implementations comply: `bge_m3` L2-normalizes what llama.cpp returns
    /// (and rejects a zero-norm vector), and `FixtureEmbedder` emits unit vectors by
    /// construction. `A-gemini-embedder.md` instructs the next one to.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError>;

    /// Optional downcast hook so the registry can ask a concrete adapter for
    /// identity data (K2 task 3: the candle adapter stamps its artifact
    /// identity). The default returns `None`; the candle adapter overrides it.
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        None
    }
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
    /// In-process BGE-M3 via candle (K2). Feature: `embed-candle`.
    Candle,
    /// Vertex Gemini embeddings. Feature: `embed-gemini`.
    Gemini,
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
            Self::Candle => "embed-candle",
            Self::Gemini => "embed-gemini",
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
            Self::Candle => cfg!(feature = "embed-candle"),
            Self::Gemini => cfg!(feature = "embed-gemini"),
            Self::Bedrock => cfg!(feature = "embed-bedrock"),
            Self::Fixture => cfg!(feature = "embed-fixture"),
        }
    }

    /// Whether [`build_embedder`] can return a working adapter (feature on **and** impl exists).
    pub const fn is_ready(self) -> bool {
        match self {
            Self::BgeM3 => cfg!(feature = "embed-bge"),
            Self::Candle => cfg!(feature = "embed-candle"),
            Self::Fixture => cfg!(feature = "embed-fixture"),
            Self::Gemini => cfg!(feature = "embed-gemini"),
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
                "empty embedder kind (expected bge_m3 | candle | gemini | bedrock | fixture)"
                    .into(),
            ));
        }
        match t.to_ascii_lowercase().as_str() {
            "bge_m3" | "bge-m3" | "bge" => Ok(Self::BgeM3),
            "candle" => Ok(Self::Candle),
            "gemini" | "vertex" => Ok(Self::Gemini),
            "bedrock" | "titan" => Ok(Self::Bedrock),
            "fixture" | "fake" => Ok(Self::Fixture),
            other => Err(EmbedError::Unavailable(format!(
                "unknown embedder kind {other:?} (expected bge_m3 | candle | gemini | bedrock | fixture)"
            ))),
        }
    }
}

impl std::fmt::Display for EmbedderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BgeM3 => write!(f, "bge_m3"),
            Self::Candle => write!(f, "candle"),
            Self::Gemini => write!(f, "gemini"),
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
    /// candle device selection: `auto` | `cpu` | `metal` | `cuda` (K2).
    ///
    /// `auto` (default) resolves Metal on Apple silicon, CUDA elsewhere, and
    /// refuses to start when neither accelerator exists. `cpu` pins the CPU
    /// backend explicitly — the only way to serve without an accelerator.
    #[serde(default)]
    pub device: Option<String>,
    /// candle hf-hub weights repo (default: the published f16 safetensors).
    #[serde(default)]
    pub repo: Option<String>,
    /// candle hf-hub revision within `repo`.
    #[serde(default)]
    pub revision: Option<String>,
    /// candle weight filename to load (`model.safetensors` | `pytorch_model.bin`).
    #[serde(default)]
    pub weights_file: Option<String>,
    /// candle offline mode: never touch the network; fail if weights are not cached.
    #[serde(default)]
    pub offline: Option<bool>,
    /// candle explicit local weights dir; bypasses hf-hub cache entirely.
    #[serde(default)]
    pub weights_dir: Option<std::path::PathBuf>,
    /// GCP project id (Vertex caller project).
    #[serde(default)]
    pub gemini_project: Option<String>,
    /// Vertex region, e.g. `us-central1`.
    #[serde(default)]
    pub gemini_location: Option<String>,
    /// Vertex model id (default `gemini-embedding-001` applied by the A3 adapter when None).
    #[serde(default)]
    pub gemini_model: Option<String>,
    /// Explicit Google service-account JSON key file path; overrides ADC (A3).
    #[serde(default)]
    pub gemini_credentials: Option<std::path::PathBuf>,
}

impl Default for EmbedderConfig {
    fn default() -> Self {
        Self {
            kind: EmbedderKind::BgeM3,
            dim: 1024,
            llama_url: None,
            llama_model: None,
            device: None,
            repo: None,
            revision: None,
            weights_file: None,
            offline: None,
            weights_dir: None,
            gemini_project: None,
            gemini_location: None,
            gemini_model: None,
            gemini_credentials: None,
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
        if let Ok(v) = env::var("LAMBO_EMBED_DEVICE") {
            if !v.is_empty() {
                self.device = Some(v);
            }
        }
        if let Ok(v) = env::var("LAMBO_GEMINI_PROJECT") {
            if !v.is_empty() {
                self.gemini_project = Some(v);
            }
        }
        if let Ok(v) = env::var("LAMBO_GEMINI_LOCATION") {
            if !v.is_empty() {
                self.gemini_location = Some(v);
            }
        }
        if let Ok(v) = env::var("LAMBO_GEMINI_MODEL") {
            if !v.is_empty() {
                self.gemini_model = Some(v);
            }
        }
        if let Ok(v) = env::var("LAMBO_GEMINI_CREDENTIALS") {
            if !v.is_empty() {
                self.gemini_credentials = Some(v.into());
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

/// The served-artifact identity the candle adapter stamps into the session
/// contract (`EmbeddingContract.model`). Returns the empty string when the
/// embedder is not the candle adapter (the caller falls back to bge_m3's
/// `llama_model`). `build_embedder` returns a `Box<dyn Embedder>`, which erases
/// the concrete type, so this downcast name is how resolve.rs learns the
/// identity (K2 task 3).
#[cfg(feature = "embed-candle")]
pub fn candle_identity(embedder: &dyn Embedder) -> Option<String> {
    embedder
        .as_any()
        .and_then(|a| a.downcast_ref::<candle::CandleEmbedder>())
        .map(|c| c.model_identity().to_string())
}

/// Same as [`candle_identity`] on builds without the candle feature.
#[cfg(not(feature = "embed-candle"))]
pub fn candle_identity(_embedder: &dyn Embedder) -> Option<String> {
    None
}

/// The served-model identity the Gemini adapter stamps into the session contract
/// (`EmbeddingContract.model`). Returns `gemini-embedding-001` (the configured model)
/// when the embedder is the Gemini adapter, else `None` so the caller falls back to
/// bge_m3's `llama_model`. `build_embedder` returns a `Box<dyn Embedder>`, which erases
/// the concrete type, so this downcast name is how resolve.rs learns the identity (A3).
#[cfg(feature = "embed-gemini")]
pub fn gemini_identity(embedder: &dyn Embedder) -> Option<String> {
    embedder
        .as_any()
        .and_then(|a| a.downcast_ref::<gemini::GeminiEmbedder>())
        .map(|g| g.model_identity().to_string())
}

#[cfg(not(feature = "embed-gemini"))]
pub fn gemini_identity(_embedder: &dyn Embedder) -> Option<String> {
    None
}

fn missing_feature(kind: EmbedderKind) -> EmbedError {
    EmbedError::Unavailable(format!(
        "embedder kind `{kind}` is not compiled into this binary; rebuild with \
         `--features {}` (see dev-diary/notes/level-b-pluggability.md)",
        kind.feature_name()
    ))
}

/// Build the Gemini embedder from resolved config (feature `embed-gemini`).
///
/// Resolves service-account credentials from `gemini_credentials` (explicit path) else
/// `GOOGLE_APPLICATION_CREDENTIALS`. Missing credentials are a clear `Unavailable` naming
/// the variable. No network is touched here: token minting / OAuth happen on first `embed`.
#[cfg(feature = "embed-gemini")]
fn build_gemini_embedder(cfg: &EmbedderConfig) -> Result<Box<dyn Embedder>, EmbedError> {
    use crate::embed::gemini::{
        build_client, load_credentials, GeminiEmbedder, GoogleOAuthTokenSource,
    };
    // A4 dim guard: gemini-embedding-001 truncates to 768, 1536 or 3072 only. Reject any
    // other configured dim here, before an unsupported `outputDimensionality` could be sent.
    if ![768, 1536, 3072].contains(&cfg.dim) {
        return Err(EmbedError::Unavailable(format!(
            "gemini-embedding-001 supports dim 768, 1536 or 3072, got {}",
            cfg.dim
        )));
    }
    let creds_path = cfg
        .gemini_credentials
        .clone()
        .or_else(|| {
            std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS").map(std::path::PathBuf::from)
        })
        .ok_or_else(|| {
            EmbedError::Unavailable(
                "Gemini embedder needs service-account credentials: set `gemini_credentials` \
                 or GOOGLE_APPLICATION_CREDENTIALS"
                    .into(),
            )
        })?;
    let creds = load_credentials(&creds_path)?;
    let project = cfg
        .gemini_project
        .clone()
        .or(creds.project_id())
        .ok_or_else(|| {
            EmbedError::Unavailable(
                "Gemini embedder needs a GCP project: set `gemini_project` or provide \
                 service-account credentials with a project_id"
                    .into(),
            )
        })?;
    let location = cfg
        .gemini_location
        .clone()
        .unwrap_or_else(|| gemini::DEFAULT_LOCATION.to_string());
    let model = cfg
        .gemini_model
        .clone()
        .unwrap_or_else(|| gemini::DEFAULT_MODEL.to_string());
    let client = build_client()?;
    let token_source = Box::new(GoogleOAuthTokenSource::new(creds, client.clone())?);
    let embed_url = GeminiEmbedder::vertex_embed_url(&project, &location, &model);
    let embedder = GeminiEmbedder::new(model, cfg.dim, token_source, embed_url, client)?;
    Ok(Box::new(embedder))
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
        EmbedderKind::Candle => {
            #[cfg(feature = "embed-candle")]
            {
                Ok(Box::new(candle::CandleEmbedder::new(
                    cfg.dim,
                    crate::embed::candle::CandleOpts {
                        device: cfg.device,
                        repo: cfg.repo,
                        revision: cfg.revision,
                        weights_file: cfg.weights_file,
                        offline: cfg.offline.unwrap_or(false),
                        weights_dir: cfg.weights_dir.clone(),
                    },
                )?))
            }
            #[cfg(not(feature = "embed-candle"))]
            {
                Err(missing_feature(EmbedderKind::Candle))
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
        EmbedderKind::Gemini => {
            #[cfg(feature = "embed-gemini")]
            {
                build_gemini_embedder(&cfg)
            }
            #[cfg(not(feature = "embed-gemini"))]
            {
                Err(missing_feature(EmbedderKind::Gemini))
            }
        }
        EmbedderKind::Bedrock => {
            #[cfg(feature = "embed-bedrock")]
            {
                Err(EmbedError::Unavailable(
                    "embed-bedrock is enabled but the Bedrock embedder is not implemented yet; \
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
            "candle".parse::<EmbedderKind>().unwrap(),
            EmbedderKind::Candle
        );
        assert_eq!(
            "bedrock".parse::<EmbedderKind>().unwrap(),
            EmbedderKind::Bedrock
        );
        assert_eq!(
            "gemini".parse::<EmbedderKind>().unwrap(),
            EmbedderKind::Gemini
        );
        assert_eq!(
            "  vertex  ".parse::<EmbedderKind>().unwrap(),
            EmbedderKind::Gemini
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
        env::remove_var("LAMBO_GEMINI_PROJECT");
        env::remove_var("LAMBO_GEMINI_LOCATION");
        env::remove_var("LAMBO_GEMINI_MODEL");
        env::remove_var("LAMBO_GEMINI_CREDENTIALS");
        let cfg = EmbedderConfig::from_env().unwrap();
        assert_eq!(cfg.kind, EmbedderKind::BgeM3);
        assert_eq!(cfg.dim, 1024);
        assert_eq!(cfg.gemini_project, None);
        assert_eq!(cfg.gemini_location, None);
        assert_eq!(cfg.gemini_model, None);
        assert_eq!(cfg.gemini_credentials, None);

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
    fn gemini_toml_fields_deserialize() {
        let cfg: EmbedderConfig = toml::from_str(
            r#"
            kind = "gemini"
            gemini_project = "my-project"
            gemini_location = "us-central1"
            gemini_model = "gemini-embedding-001"
            gemini_credentials = "/keys/sa.json"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.kind, EmbedderKind::Gemini);
        assert_eq!(cfg.gemini_project.as_deref(), Some("my-project"));
        assert_eq!(cfg.gemini_location.as_deref(), Some("us-central1"));
        assert_eq!(cfg.gemini_model.as_deref(), Some("gemini-embedding-001"));
        assert_eq!(
            cfg.gemini_credentials.as_deref(),
            Some(std::path::Path::new("/keys/sa.json"))
        );
    }

    #[test]
    fn gemini_toml_misspelled_key_rejected() {
        // deny_unknown_fields must reject a typo'd gemini key, not silently ignore it.
        let r = toml::from_str::<EmbedderConfig>(
            r#"kind = "gemini"
            gemini_projct = "p""#,
        );
        assert!(r.is_err(), "misspelled gemini key must not parse");
    }

    #[test]
    fn gemini_overlay_env_picks_up_vars() {
        let _g = crate::test_util::env_lock();

        env::set_var("LAMBO_GEMINI_PROJECT", "proj-1");
        env::set_var("LAMBO_GEMINI_LOCATION", "us-west1");
        env::set_var("LAMBO_GEMINI_MODEL", "gemini-embedding-001");
        env::set_var("LAMBO_GEMINI_CREDENTIALS", "/tmp/sa.json");
        let cfg = EmbedderConfig::from_env().unwrap();
        assert_eq!(cfg.gemini_project.as_deref(), Some("proj-1"));
        assert_eq!(cfg.gemini_location.as_deref(), Some("us-west1"));
        assert_eq!(cfg.gemini_model.as_deref(), Some("gemini-embedding-001"));
        assert_eq!(
            cfg.gemini_credentials.as_deref(),
            Some(std::path::Path::new("/tmp/sa.json"))
        );

        // Empty env value leaves the base intact.
        env::set_var("LAMBO_GEMINI_PROJECT", "");
        let cfg = EmbedderConfig::from_env().unwrap();
        assert_eq!(cfg.gemini_project, None);
        assert_eq!(cfg.gemini_location.as_deref(), Some("us-west1"));
    }
    #[test]
    fn gemini_overlay_env_base_then_env_precedence() {
        // A2-R1-1 closure: with a file base set, non-empty env wins and empty env
        // leaves the base intact. The base simulates a `lambo.toml` value.
        let _g = crate::test_util::env_lock();
        env::remove_var("LAMBO_GEMINI_PROJECT");
        env::remove_var("LAMBO_GEMINI_LOCATION");
        env::remove_var("LAMBO_GEMINI_MODEL");
        env::remove_var("LAMBO_GEMINI_CREDENTIALS");

        let base = EmbedderConfig {
            gemini_project: Some("from-file".to_string()),
            ..Default::default()
        };

        // Non-empty env overrides the set base.
        env::set_var("LAMBO_GEMINI_PROJECT", "from-env");
        let cfg = base.clone().overlay_env().unwrap();
        assert_eq!(cfg.gemini_project.as_deref(), Some("from-env"));

        // Empty env leaves the set base intact.
        env::set_var("LAMBO_GEMINI_PROJECT", "");
        let cfg = base.overlay_env().unwrap();
        assert_eq!(cfg.gemini_project.as_deref(), Some("from-file"));
    }

    #[test]
    fn gemini_overlay_env_whitespace_is_non_empty() {
        // A2-R1-2 closure: whitespace is non-empty, so it wins over the base,
        // consistent with the untrimmed llama pattern. This locks the corner so a
        // future trim must be a deliberate contract change, not a silent one.
        let _g = crate::test_util::env_lock();
        env::remove_var("LAMBO_GEMINI_PROJECT");
        env::set_var("LAMBO_GEMINI_PROJECT", "   ");
        let cfg = EmbedderConfig::from_env().unwrap();
        assert_eq!(cfg.gemini_project.as_deref(), Some("   "));
        env::remove_var("LAMBO_GEMINI_PROJECT");
    }

    /// CON-7 agreement: every compiled embedder rejects empty/whitespace input
    /// with `EmbedError::Unavailable` BEFORE any backend traffic.
    #[cfg(all(feature = "embed-bge", feature = "embed-fixture"))]
    #[tokio::test]
    async fn all_embedders_reject_empty_input_identically() {
        let fixture = FixtureEmbedder::new();
        // BGE pointed at port 9 (discard): even if the guard regressed, the
        // request would fail fast (connection refused -> Unavailable), so this
        // leg only checks error SHAPE, not absence of traffic; the guard itself
        // is locked by bge_m3::tests::rejects_empty_text (mock server, 404 ->
        // Backend, so only the guard can produce Unavailable).
        let bge = BgeM3LlamaCppEmbedder::new("http://127.0.0.1:9", "", 1024).unwrap();
        for text in ["", "   ", "\t\n"] {
            for e in [&fixture as &dyn Embedder, &bge as &dyn Embedder] {
                let err = e.embed(text).await.unwrap_err();
                assert!(
                    matches!(err, EmbedError::Unavailable(_)),
                    "embedder must reject {text:?} with Unavailable (CON-7): {err:?}"
                );
            }
        }
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
        let w: Wrap = toml::from_str(r#"kind = "vertex""#).unwrap();
        assert_eq!(w.kind, EmbedderKind::Gemini);
        let w: Wrap = toml::from_str(r#"kind = "candle""#).unwrap();
        assert_eq!(w.kind, EmbedderKind::Candle);
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
        });
        let Err(err) = r else {
            panic!("expected Unavailable, got Ok — silent fallback forbidden");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("embed-bedrock") || msg.contains("not compiled"),
            "msg={msg}"
        );
        assert!(!msg.to_ascii_lowercase().contains("fixture"));
        assert!(!EmbedderKind::Bedrock.is_ready());
    }

    #[test]
    fn gemini_is_ready_requires_feature() {
        assert_eq!(
            EmbedderKind::Gemini.is_ready(),
            cfg!(feature = "embed-gemini"),
            "is_ready must be true exactly when the adapter feature is compiled"
        );
    }

    #[test]
    fn gemini_fail_closed_without_credentials() {
        let r = build_embedder(EmbedderConfig {
            kind: EmbedderKind::Gemini,
            dim: 1536,
            llama_url: None,
            llama_model: None,
            ..Default::default()
        });
        let Err(err) = r else {
            panic!("expected Unavailable, got Ok (silent fallback forbidden)");
        };
        let msg = err.to_string();
        assert!(!msg.to_ascii_lowercase().contains("fixture"));
        #[cfg(not(feature = "embed-gemini"))]
        assert!(
            msg.contains("embed-gemini") || msg.contains("not compiled"),
            "feature-off must name the missing feature, got: {msg}"
        );
        #[cfg(feature = "embed-gemini")]
        assert!(
            msg.contains("GOOGLE_APPLICATION_CREDENTIALS") || msg.contains("credentials"),
            "feature-on must name the missing credentials, got: {msg}"
        );
    }
    /// A1-A1-1 supersession: the feature-ON arm now builds a real `GeminiEmbedder` from
    /// valid config plus a service-account JSON key file (construction touches no network),
    /// so the old fail-closed test is replaced by a build + identity assertion.
    #[test]
    #[cfg(feature = "embed-gemini")]
    fn gemini_feature_on_builds_adapter_from_credentials() {
        use super::gemini::TEST_RSA_PRIVATE_KEY_PEM;
        let dir = std::env::temp_dir().join(format!("lambo-a3-gemini-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let creds_path = dir.join("sa.json");
        let creds_json = serde_json::json!({
            "client_email": "test@example.com",
            "private_key": TEST_RSA_PRIVATE_KEY_PEM,
            "token_uri": "https://oauth2.googleapis.com/token",
            "project_id": "proj",
        });
        std::fs::write(&creds_path, creds_json.to_string()).unwrap();
        let r = build_embedder(EmbedderConfig {
            kind: EmbedderKind::Gemini,
            dim: 1536,
            gemini_project: Some("proj".to_string()),
            gemini_location: Some("us-central1".to_string()),
            gemini_credentials: Some(creds_path.clone()),
            ..Default::default()
        });
        let embedder = r.unwrap_or_else(|e| panic!("feature-on must build the adapter: {e}"));
        let identity = crate::embed::gemini_identity(embedder.as_ref());
        assert_eq!(identity.as_deref(), Some("gemini-embedding-001"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A4 dim guard: any configured dim outside {768, 1536, 3072} is rejected at
    /// construction, naming the three, BEFORE credentials are consulted or a request
    /// is sent. 1024 is the crate default and here deliberately unsupported.
    #[test]
    #[cfg(feature = "embed-gemini")]
    fn gemini_rejects_unsupported_dim() {
        for bad in [512usize, 1024, 2048] {
            let r = build_embedder(EmbedderConfig {
                kind: EmbedderKind::Gemini,
                dim: bad,
                llama_url: None,
                llama_model: None,
                ..Default::default()
            });
            let Err(err) = r else {
                panic!("dim {bad} must be rejected at construction, got Ok");
            };
            let msg = err.to_string();
            assert!(
                msg.contains("768") && msg.contains("1536") && msg.contains("3072"),
                "dim error must name 768, 1536 and 3072, got: {msg}"
            );
            // The guard fires before credentials; the message must not be the creds error.
            assert!(
                !msg.to_ascii_lowercase().contains("credentials"),
                "dim error must not be the credentials error, got: {msg}"
            );
        }
    }

    #[test]
    fn kind_feature_names() {
        assert_eq!(EmbedderKind::BgeM3.feature_name(), "embed-bge");
        assert_eq!(EmbedderKind::Candle.feature_name(), "embed-candle");
        assert_eq!(EmbedderKind::Fixture.feature_name(), "embed-fixture");
        assert_eq!(EmbedderKind::Gemini.feature_name(), "embed-gemini");
        assert_eq!(EmbedderKind::Bedrock.feature_name(), "embed-bedrock");
    }

    #[test]
    fn cosine_always_available() {
        // Must compile even when embed-fixture is off (math module is ungated).
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
    }
}
