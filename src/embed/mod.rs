//! Embedder trait, Level B factory, and optional adapter modules (P1 / P7).
//!
//! Packaging: Cargo features gate adapters (`embed-bge`, `embed-candle`, `embed-fixture`, `embed-bedrock`);
//! `lambo.toml` / env select among compiled kinds. See
//! `dev-diary/notes/level-b-pluggability.md`.

pub mod api_key;
pub mod keep_warm;
mod math;

#[cfg(feature = "embed-bge")]
mod bge_m3;
#[cfg(feature = "embed-candle")]
mod candle;
#[cfg(feature = "embed-fixture")]
mod fixture;
#[cfg(feature = "embed-gemini")]
pub(crate) mod gemini;
#[cfg(test)]
mod trait_tests;

pub use math::cosine;

#[cfg(feature = "embed-bge")]
pub use bge_m3::BgeM3LlamaCppEmbedder;
#[cfg(feature = "embed-candle")]
pub use candle::CandleEmbedder;
#[cfg(feature = "embed-fixture")]
pub use fixture::{
    near_far_contract, png_with_label, FixtureEmbedder, FAR, IMAGE_LABEL_KEYWORD, NEAR_A, NEAR_B,
    NEAR_PAIR,
};
#[cfg(feature = "embed-gemini")]
pub use gemini::GeminiEmbedder;

use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize};
use std::env;
use std::str::FromStr;
use thiserror::Error;

/// Why an [`Embedder`] call failed. [`EmbedError::is_transient`] is the
/// classification callers act on.
///
/// `#[non_exhaustive]` (#22 review L1): #22 already adds `Unsupported`, and
/// later adapters may need another variant; that must not be a second
/// breaking change for downstream matchers. Match with a wildcard arm, or ask
/// [`EmbedError::is_transient`].
///
/// ```compile_fail,E0004
/// // Outside the crate an exhaustive match without a wildcard does not compile.
/// fn class(e: &lambo::EmbedError) -> u8 {
///     match e {
///         lambo::EmbedError::Unavailable(_) => 0,
///         lambo::EmbedError::Backend(_) => 1,
///         lambo::EmbedError::Unsupported(_) => 2,
///     }
/// }
/// ```
///
/// ```
/// fn class(e: &lambo::EmbedError) -> u8 {
///     match e {
///         lambo::EmbedError::Unavailable(_) => 0,
///         _ => 1,
///     }
/// }
/// ```
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum EmbedError {
    #[error("embedder unavailable: {0}")]
    Unavailable(String),
    #[error("backend: {0}")]
    Backend(String),
    /// The embedder cannot embed this kind of input at all (#22): for example
    /// an image sent to a text-only adapter. Permanent for this deployment,
    /// so [`Self::is_transient`] is `false`. The message names the input kind,
    /// never any of its bytes.
    #[error("unsupported input: {0}")]
    Unsupported(String),
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
    ///   status in `crate::embed::bge_m3::classify_status`'s `Transient` /
    ///   `Unclassified` classes (408/425/429/500/502/503/504, any un-named
    ///   5xx, and unrecognised statuses). Nothing about the *input* was
    ///   rejected, so the same input against a healthy server is untried.
    /// * [`Self::Backend`] — **permanent for this input or this deployment.**
    ///   The backend answered and the answer was unusable: a status the rule
    ///   table classifies as content (400/413/415/422 — a genuine refusal of
    ///   this text) or permanent-config (any 3xx — a redirect is never
    ///   followed; 401/403/404 — a wrong URL, model, or credentials),
    ///   unparseable JSON, the wrong dimensionality, a
    ///   non-finite or zero-norm vector.
    ///
    /// **Where this is imprecise, stated rather than hidden (J3-R2R-1).** The
    /// durability decision turns on whether a non-success status speaks about
    /// the *input*, the *deployment*, or the *server's momentary state* — HTTP
    /// collapses those into three buckets plus "no rule". The adapter classifies
    /// at the site that knows the status (`crate::embed::bge_m3::classify_status`),
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
    ///
    /// * [`Self::Unsupported`] — **permanent for this deployment** (#22). The
    ///   adapter cannot embed this kind of input (an image on a text-only
    ///   model); no retry against the same deployment can change that.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Unavailable(_) => true,
            Self::Backend(_) | Self::Unsupported(_) => false,
        }
    }
}

bitflags::bitflags! {
    /// The kinds of input an [`Embedder`] can embed (#22). Every adapter embeds
    /// [`Modalities::TEXT`]; an adapter that also embeds images into the same
    /// space reports [`Modalities::IMAGE`] too.
    ///
    /// A `bitflags` 2 type, like [`crate::store::Capabilities`], so `bitflags`
    /// was already part of the public API before #22 and this adds no new
    /// public dependency. Use the associated constants and the set operators
    /// (`contains`, `|`); a new kind of input is a new constant, which is not
    /// a breaking change.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Modalities: u8 {
        /// Text, through [`Embedder::embed`] and [`Embedder::embed_query`].
        const TEXT = 1;
        /// Images, through [`Embedder::embed_image`].
        const IMAGE = 2;
    }
}

/// The image formats Lambo accepts (#22): PNG, JPEG and WebP, nothing else.
///
/// `#[non_exhaustive]` (#22 review L1): a later format must not break a
/// downstream exhaustive match. Match with a wildcard arm, or use
/// [`ImageMime::as_str`].
///
/// ```compile_fail,E0004
/// // Outside the crate an exhaustive match without a wildcard does not compile.
/// fn ext(m: lambo::embed::ImageMime) -> &'static str {
///     match m {
///         lambo::embed::ImageMime::Png => "png",
///         lambo::embed::ImageMime::Jpeg => "jpg",
///         lambo::embed::ImageMime::Webp => "webp",
///     }
/// }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ImageMime {
    Png,
    Jpeg,
    Webp,
}

impl ImageMime {
    /// The MIME type string: `image/png`, `image/jpeg` or `image/webp`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
        }
    }

    /// Parse a declared MIME type. Exact and case-sensitive on purpose: the
    /// allowlist is three literal strings, with no parameters and no aliases
    /// (`image/jpg` is refused), so there is nothing to guess.
    pub fn from_mime(mime: &str) -> Option<Self> {
        match mime {
            "image/png" => Some(Self::Png),
            "image/jpeg" => Some(Self::Jpeg),
            "image/webp" => Some(Self::Webp),
            _ => None,
        }
    }
}

impl std::fmt::Display for ImageMime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A validated image, ready for [`Embedder::embed_image`] (#22).
///
/// Only `crate::surface::image::validate` constructs one, so an adapter can
/// rely on all of: the bytes are non-empty and at most
/// `crate::surface::image::MAX_IMAGE_BYTES`; their magic bytes match
/// [`Self::mime`]; the header's width and height are each between 1 and
/// `crate::surface::image::MAX_IMAGE_SIDE_PX` (for an extended WebP, the
/// canvas and its image chunk agree and it is not animated); and
/// [`Self::sha256`] is the digest of exactly [`Self::bytes`]. It borrows the
/// bytes; nothing here copies or keeps them.
///
/// **Only the header is validated; the pixels are not decoded.** A PNG with
/// no image data, a JPEG with no scan, or a corrupt bitstream all pass. An
/// adapter that decodes must therefore expect a decode to fail, and must
/// return [`EmbedError::Backend`] when it does, never panic.
#[derive(Clone, Copy)]
pub struct ImageInput<'a> {
    bytes: &'a [u8],
    mime: ImageMime,
    sha256: [u8; 32],
}

impl<'a> ImageInput<'a> {
    /// Crate-private: the validator is the only constructor, which is what
    /// makes the guarantees on the type hold.
    pub(crate) fn from_validated(bytes: &'a [u8], mime: ImageMime, sha256: [u8; 32]) -> Self {
        Self {
            bytes,
            mime,
            sha256,
        }
    }

    /// The image bytes, exactly as supplied.
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// The format, declared by the caller and confirmed by the magic bytes.
    pub fn mime(&self) -> ImageMime {
        self.mime
    }

    /// SHA-256 of [`Self::bytes`].
    pub fn sha256(&self) -> [u8; 32] {
        self.sha256
    }
}

/// Never prints the bytes: an image is user content, and a `{:?}` in a log
/// line or an error must not leak it.
impl std::fmt::Debug for ImageInput<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageInput")
            .field("len", &self.bytes.len())
            .field("mime", &self.mime)
            .finish_non_exhaustive()
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
///
/// **Roles and modalities (#22).** [`Self::embed`] is the *document* role:
/// derive, record_action, re-embed, the write-queue probes and replay all use
/// it. [`Self::embed_query`] is the *query* role, used by recall alone, and
/// [`Self::embed_image`] embeds an image into the same space. Both have
/// defaults (delegate to `embed`; refuse with [`EmbedError::Unsupported`]), so
/// a text-only, symmetric adapter implements `dimensions` and `embed` and
/// nothing else.
///
/// **A wrapper must forward every method.** An embedder that wraps another
/// (to count, gate, fail or log calls) and implements only `embed` silently
/// inherits the *defaults* for the rest: a query would be embedded in the
/// document role, an image refused, and the inner adapter's identity hidden
/// from the registry, whatever the inner adapter does. So a delegating
/// embedder forwards [`Self::embed_query`], [`Self::embed_image`],
/// [`Self::modalities`] and [`Self::as_any`] to its inner embedder as well.
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

    /// Embed a recall **query** (#22). Same input and output contract as
    /// [`Self::embed`].
    ///
    /// The default is `embed`: for a symmetric model (BGE-M3, candle, Gemini,
    /// the fixture) a query and a document are embedded the same way. An
    /// adapter whose model is asymmetric (EmbeddingGemma 2's task prompts)
    /// overrides it. A change of query prompt is a change of embedding space,
    /// so such an adapter names its prompt profile in its
    /// [`crate::types::EmbeddingContract`]'s `model`.
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        self.embed(text).await
    }

    /// The kinds of input this adapter embeds. Default: text only.
    fn modalities(&self) -> Modalities {
        Modalities::TEXT
    }

    /// Embed an image into the **same space** as [`Self::embed`] (#22).
    ///
    /// Same output contract: an L2-normalized vector of width
    /// [`Self::dimensions`]. An adapter that overrides this also reports
    /// [`Modalities::IMAGE`] from [`Self::modalities`]. The default refuses
    /// with [`EmbedError::Unsupported`].
    async fn embed_image(&self, image: ImageInput<'_>) -> Result<Vec<f32>, EmbedError> {
        let _ = image;
        Err(EmbedError::Unsupported(
            "this embedder does not embed images".into(),
        ))
    }

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
    /// BGE-M3 weights served by a local llama.cpp server (default), or any
    /// OpenAI-compatible embeddings endpoint (`openai` alias, issue #21).
    /// Feature: `embed-bge`.
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
            // `openai` (issue #21): the adapter is protocol-shaped, not
            // model-shaped. It stays `BgeM3`, so it displays and stamps the
            // EmbeddingContract as `bge_m3` and no existing session changes.
            "bge_m3" | "bge-m3" | "bge" | "openai" => Ok(Self::BgeM3),
            "candle" => Ok(Self::Candle),
            "gemini" | "vertex" => Ok(Self::Gemini),
            "bedrock" | "titan" => Ok(Self::Bedrock),
            "fixture" | "fake" => Ok(Self::Fixture),
            // Never echo the value: a key or DSN pasted under `kind` would
            // reach the startup log.
            _ => Err(EmbedError::Unavailable(
                "unknown embedder kind (value not shown; expected bge_m3 | candle | gemini | \
                 bedrock | fixture)"
                    .into(),
            )),
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
    /// Name of the environment variable holding a bearer token for a hosted
    /// OpenAI-compatible endpoint (issue #21), e.g. `CLOUDFLARE_API_TOKEN`.
    /// A variable *name*, never the token: see [`api_key`]. `bge_m3` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// An inline `api_key = "..."`, accepted by the parser only so resolve can
    /// refuse it with a pointer at `api_key_env`. The value is discarded while
    /// parsing ([`api_key::InlineApiKey`]) and this field is never serialized.
    #[serde(default, skip_serializing)]
    pub api_key: Option<api_key::InlineApiKey>,
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
    /// Seconds between `lambo serve` keep-warm touches (issue #13). Absent =
    /// auto (on only where the weights sit in pageable unified memory: candle
    /// on Metal); `0` = off; `N` = every `N` s for any kind. Not part of the
    /// embedding contract. See [`keep_warm`].
    #[serde(default)]
    pub keep_warm_secs: Option<u64>,
}

impl Default for EmbedderConfig {
    fn default() -> Self {
        Self {
            kind: EmbedderKind::BgeM3,
            dim: 1024,
            llama_url: None,
            llama_model: None,
            api_key_env: None,
            api_key: None,
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
            keep_warm_secs: None,
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
        if let Ok(v) = env::var("LAMBO_EMBED_DIM")
            && !v.is_empty()
        {
            self.dim = v
                .parse()
                .map_err(|e| EmbedError::Unavailable(format!("invalid LAMBO_EMBED_DIM: {e}")))?;
        }
        if let Ok(v) = env::var("LAMBO_LLAMA_EMBED_URL")
            && !v.is_empty()
        {
            self.llama_url = Some(v);
        }
        if let Ok(v) = env::var("LAMBO_LLAMA_MODEL")
            && !v.is_empty()
        {
            self.llama_model = Some(v);
        }
        if let Ok(v) = env::var(api_key::API_KEY_ENV_OVERRIDE)
            && !v.is_empty()
        {
            self.api_key_env = Some(v);
        }
        if let Ok(v) = env::var("LAMBO_EMBED_DEVICE")
            && !v.is_empty()
        {
            self.device = Some(v);
        }
        if let Ok(v) = env::var("LAMBO_GEMINI_PROJECT")
            && !v.is_empty()
        {
            self.gemini_project = Some(v);
        }
        if let Ok(v) = env::var("LAMBO_GEMINI_LOCATION")
            && !v.is_empty()
        {
            self.gemini_location = Some(v);
        }
        if let Ok(v) = env::var("LAMBO_GEMINI_MODEL")
            && !v.is_empty()
        {
            self.gemini_model = Some(v);
        }
        if let Ok(v) = env::var("LAMBO_GEMINI_CREDENTIALS")
            && !v.is_empty()
        {
            self.gemini_credentials = Some(v.into());
        }
        if let Ok(v) = env::var("LAMBO_EMBED_KEEP_WARM_SECS")
            && !v.is_empty()
        {
            self.keep_warm_secs = Some(v.parse().map_err(|e| {
                EmbedError::Unavailable(format!(
                    "invalid LAMBO_EMBED_KEEP_WARM_SECS {v:?}: {e} (expected whole seconds; \
                         0 disables keep-warm)"
                ))
            })?);
        }
        // Refuse a pasted token while the file is being resolved, before any
        // later message could quote `api_key_env` (issue #21).
        api_key::validate(self.api_key_env.as_deref(), self.api_key)?;
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

/// Does this embedder hold its model weights in host-pageable unified memory
/// inside this process — memory the OS pager can compress or swap while the
/// process idles (issue #13)? Today that is exactly the candle adapter on a
/// Metal device. CUDA weights live in VRAM; the remote adapters hold their
/// weights in another process; the fixture has none.
///
/// CPU candle answering `false` is a policy choice, not a claim that its
/// weights are safe: its f32 weights (~2.2 GB of anonymous memory) are just as
/// pageable and compressible. Auto stays off there because CPU candle is an
/// explicit fallback (`device = "cpu"`) whose idle tax has not been measured,
/// and an operator who sees it can opt in with an explicit `keep_warm_secs`.
///
/// Drives the `keep_warm_secs` auto default
/// ([`keep_warm::resolve_keep_warm`]).
#[cfg(feature = "embed-candle")]
pub fn weights_in_unified_memory(embedder: &dyn Embedder) -> bool {
    embedder
        .as_any()
        .and_then(|a| a.downcast_ref::<candle::CandleEmbedder>())
        .is_some_and(|c| c.on_metal())
}

/// Same as [`weights_in_unified_memory`] on builds without the candle feature.
#[cfg(not(feature = "embed-candle"))]
pub fn weights_in_unified_memory(_embedder: &dyn Embedder) -> bool {
    false
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
/// Resolves credentials from `gemini_credentials` (explicit path) else the shared chain
/// [`crate::gcp_auth::credentials_path_from_env`] (`GCP_LAMBO_CREDENTIALS`, falling back to
/// `GOOGLE_APPLICATION_CREDENTIALS`), which is the same chain the Postgres store's Cloud SQL
/// IAM login resolves, so one export names one identity for both. Missing credentials are a
/// clear `Unavailable` naming both variables and the config key. No network is touched here:
/// token minting / OAuth happen on first `embed`.
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
    // One credential variable, one identity. The store resolves its Cloud SQL credential
    // through `gcp_auth::credentials_path_from_env` (GCP_LAMBO_CREDENTIALS, falling back
    // to GOOGLE_APPLICATION_CREDENTIALS); the embedder resolving it any other way is how
    // the shared-service-account design breaks in the operator's hands, with the store
    // authenticating and the embedder refusing to build off the same export block.
    let creds_path = cfg
        .gemini_credentials
        .clone()
        .or_else(crate::gcp_auth::credentials_path_from_env)
        .ok_or_else(|| {
            EmbedError::Unavailable(
                "Gemini embedder needs service-account credentials: set `gemini_credentials` \
                 or GCP_LAMBO_CREDENTIALS / GOOGLE_APPLICATION_CREDENTIALS"
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
    // Vertex's scope only, named by the consumer rather than spelled at the call site: the
    // store's Cloud SQL scope set is its own (`gcp_auth`).
    let token_source = Box::new(GoogleOAuthTokenSource::for_vertex(creds, client.clone())?);
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
///
/// **`api_key_env` checks.** The name rule (`crate::config::secret_env::check`)
/// runs here too, so a config built in code still cannot name
/// `LAMBO_AUTH_TOKEN`, a store DSN variable or a Google credentials variable.
/// The overlap with `[[serve.credential]] token_env` is *not* checked here:
/// it needs the `[serve]` table, which an `EmbedderConfig` does not carry. It
/// runs in `LamboFile::from_toml_str` and `LamboFile::load_resolved`, so the
/// CLI and every config-file path get it; a library caller that builds an
/// `EmbedderConfig` by hand and also runs `lambo serve` credentials must keep
/// the two variables apart itself.
pub fn build_embedder(cfg: EmbedderConfig) -> Result<Box<dyn Embedder>, EmbedError> {
    if cfg.dim == 0 {
        return Err(EmbedError::Unavailable("embedder dim must be > 0".into()));
    }
    // Issue #21: `overlay_env` already refused these on the file path; a config
    // built in code reaches here without it.
    api_key::validate(cfg.api_key_env.as_deref(), cfg.api_key)?;
    if let Some(name) = cfg.api_key_env.as_deref()
        && cfg.kind != EmbedderKind::BgeM3
    {
        // A credential key the selected adapter would ignore is refused rather
        // than silently dropped: an operator who configured one expects it used.
        return Err(EmbedError::Unavailable(format!(
            "embedder.api_key_env ({}) applies only to kind `bge_m3` (alias `openai`), but kind \
             is `{}`; \
             remove api_key_env or change the kind",
            crate::config::secret_env::shown(name),
            cfg.kind
        )));
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
                let mut embedder = BgeM3LlamaCppEmbedder::new(url.clone(), model, cfg.dim)?;
                // Issue #21: the token is read here, at resolve, so a configured
                // but unset variable stops startup instead of sending no key.
                // The transport is checked first, so a plaintext non-loopback
                // URL is refused before the token is even read.
                if let Some(name) = cfg.api_key_env.as_deref() {
                    bge_m3::check_bearer_transport(&url)?;
                    embedder = embedder.with_bearer_token(&api_key::resolve(name)?)?;
                }
                Ok(Box::new(embedder))
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
        // Issue #21: `openai` is the same adapter, displayed as `bge_m3`.
        let openai = " OpenAI ".parse::<EmbedderKind>().unwrap();
        assert_eq!(openai, EmbedderKind::BgeM3);
        assert_eq!(openai.to_string(), "bge_m3");
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
        let env = crate::test_util::env_lock();

        env.remove("LAMBO_EMBEDDER");
        env.remove("LAMBO_EMBED_DIM");
        env.remove("LAMBO_LLAMA_EMBED_URL");
        env.remove("LAMBO_LLAMA_MODEL");
        env.remove("LAMBO_GEMINI_PROJECT");
        env.remove("LAMBO_GEMINI_LOCATION");
        env.remove("LAMBO_GEMINI_MODEL");
        env.remove("LAMBO_GEMINI_CREDENTIALS");
        let cfg = EmbedderConfig::from_env().unwrap();
        assert_eq!(cfg.kind, EmbedderKind::BgeM3);
        assert_eq!(cfg.dim, 1024);
        assert_eq!(cfg.gemini_project, None);
        assert_eq!(cfg.gemini_location, None);
        assert_eq!(cfg.gemini_model, None);
        assert_eq!(cfg.gemini_credentials, None);

        // Empty string is unset — still BgeM3 (must not call FromStr("") which errors).
        env.set("LAMBO_EMBEDDER", "");
        let cfg = EmbedderConfig::from_env().unwrap();
        assert_eq!(cfg.kind, EmbedderKind::BgeM3);
        assert_eq!(
            EmbedderConfig::from_env().unwrap(),
            EmbedderConfig::default().overlay_env().unwrap()
        );
        env.remove("LAMBO_EMBEDDER");
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
        let env = crate::test_util::env_lock();

        env.set("LAMBO_GEMINI_PROJECT", "proj-1");
        env.set("LAMBO_GEMINI_LOCATION", "us-west1");
        env.set("LAMBO_GEMINI_MODEL", "gemini-embedding-001");
        env.set("LAMBO_GEMINI_CREDENTIALS", "/tmp/sa.json");
        let cfg = EmbedderConfig::from_env().unwrap();
        assert_eq!(cfg.gemini_project.as_deref(), Some("proj-1"));
        assert_eq!(cfg.gemini_location.as_deref(), Some("us-west1"));
        assert_eq!(cfg.gemini_model.as_deref(), Some("gemini-embedding-001"));
        assert_eq!(
            cfg.gemini_credentials.as_deref(),
            Some(std::path::Path::new("/tmp/sa.json"))
        );

        // Empty env value leaves the base intact.
        env.set("LAMBO_GEMINI_PROJECT", "");
        let cfg = EmbedderConfig::from_env().unwrap();
        assert_eq!(cfg.gemini_project, None);
        assert_eq!(cfg.gemini_location.as_deref(), Some("us-west1"));
    }
    #[test]
    fn gemini_overlay_env_base_then_env_precedence() {
        // A2-R1-1 closure: with a file base set, non-empty env wins and empty env
        // leaves the base intact. The base simulates a `lambo.toml` value.
        let env = crate::test_util::env_lock();
        env.remove("LAMBO_GEMINI_PROJECT");
        env.remove("LAMBO_GEMINI_LOCATION");
        env.remove("LAMBO_GEMINI_MODEL");
        env.remove("LAMBO_GEMINI_CREDENTIALS");

        let base = EmbedderConfig {
            gemini_project: Some("from-file".to_string()),
            ..Default::default()
        };

        // Non-empty env overrides the set base.
        env.set("LAMBO_GEMINI_PROJECT", "from-env");
        let cfg = base.clone().overlay_env().unwrap();
        assert_eq!(cfg.gemini_project.as_deref(), Some("from-env"));

        // Empty env leaves the set base intact.
        env.set("LAMBO_GEMINI_PROJECT", "");
        let cfg = base.overlay_env().unwrap();
        assert_eq!(cfg.gemini_project.as_deref(), Some("from-file"));
    }

    #[test]
    fn gemini_overlay_env_whitespace_is_non_empty() {
        // A2-R1-2 closure: whitespace is non-empty, so it wins over the base,
        // consistent with the untrimmed llama pattern. This locks the corner so a
        // future trim must be a deliberate contract change, not a silent one.
        let env = crate::test_util::env_lock();
        env.remove("LAMBO_GEMINI_PROJECT");
        env.set("LAMBO_GEMINI_PROJECT", "   ");
        let cfg = EmbedderConfig::from_env().unwrap();
        assert_eq!(cfg.gemini_project.as_deref(), Some("   "));
        env.remove("LAMBO_GEMINI_PROJECT");
    }

    /// Issue #13: `keep_warm_secs` is a real `[embedder]` key (absent = auto),
    /// and the table stays `deny_unknown_fields` — a typo of it is refused.
    #[test]
    fn keep_warm_secs_toml_key() {
        let cfg: EmbedderConfig =
            toml::from_str("kind = \"fixture\"\nkeep_warm_secs = 45\n").unwrap();
        assert_eq!(cfg.keep_warm_secs, Some(45));
        let cfg: EmbedderConfig = toml::from_str("kind = \"fixture\"\n").unwrap();
        assert_eq!(cfg.keep_warm_secs, None, "absent means auto");
        let cfg: EmbedderConfig = toml::from_str("keep_warm_secs = 0\n").unwrap();
        assert_eq!(cfg.keep_warm_secs, Some(0));
        let err = toml::from_str::<EmbedderConfig>("keep_warm = 30\n").unwrap_err();
        assert!(err.to_string().contains("keep_warm"), "{err}");
        assert!(toml::from_str::<EmbedderConfig>("keep_warm_secs = -1\n").is_err());
    }

    /// Issue #13: `LAMBO_EMBED_KEEP_WARM_SECS` overlays the file value with the
    /// usual rules (non-empty env wins, empty leaves the base), and garbage is
    /// a hard error naming the variable, not a silent auto.
    #[test]
    fn keep_warm_secs_env_overlay() {
        let env = crate::test_util::env_lock();
        env.remove("LAMBO_EMBED_KEEP_WARM_SECS");
        let base = EmbedderConfig {
            keep_warm_secs: Some(60),
            ..Default::default()
        };
        assert_eq!(base.clone().overlay_env().unwrap().keep_warm_secs, Some(60));

        env.set("LAMBO_EMBED_KEEP_WARM_SECS", "0");
        assert_eq!(base.clone().overlay_env().unwrap().keep_warm_secs, Some(0));

        env.set("LAMBO_EMBED_KEEP_WARM_SECS", "");
        assert_eq!(base.clone().overlay_env().unwrap().keep_warm_secs, Some(60));

        env.set("LAMBO_EMBED_KEEP_WARM_SECS", "soon");
        let err = base.overlay_env().unwrap_err().to_string();
        assert!(err.contains("LAMBO_EMBED_KEEP_WARM_SECS"), "{err}");
        env.remove("LAMBO_EMBED_KEEP_WARM_SECS");
    }

    /// Issue #21: `api_key_env` is a real `[embedder]` key holding a variable
    /// name, absent by default, and never serialized when absent.
    #[test]
    fn api_key_env_toml_key() {
        let cfg: EmbedderConfig =
            toml::from_str("kind = \"bge_m3\"\napi_key_env = \"CLOUDFLARE_API_TOKEN\"\n").unwrap();
        assert_eq!(cfg.api_key_env.as_deref(), Some("CLOUDFLARE_API_TOKEN"));
        assert_eq!(cfg.api_key, None);
        let back: EmbedderConfig = toml::from_str(&toml::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back, cfg);

        let cfg: EmbedderConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.api_key_env, None);
        assert!(!toml::to_string(&cfg).unwrap().contains("api_key"));
        // Still deny_unknown_fields: a typo of the key is refused.
        assert!(toml::from_str::<EmbedderConfig>("api_key_envv = \"X\"\n").is_err());
    }

    /// Issue #21: `LAMBO_EMBED_API_KEY_ENV` overlays the file's variable name
    /// with the usual rules, and a token-shaped value is refused at overlay
    /// without being quoted.
    #[test]
    fn api_key_env_overlay() {
        let env = crate::test_util::env_lock();
        env.remove(api_key::API_KEY_ENV_OVERRIDE);
        let base = EmbedderConfig {
            api_key_env: Some("FROM_FILE".into()),
            ..Default::default()
        };
        assert_eq!(
            base.clone().overlay_env().unwrap().api_key_env.as_deref(),
            Some("FROM_FILE")
        );
        env.set(api_key::API_KEY_ENV_OVERRIDE, "FROM_ENV");
        assert_eq!(
            base.clone().overlay_env().unwrap().api_key_env.as_deref(),
            Some("FROM_ENV")
        );
        env.set(api_key::API_KEY_ENV_OVERRIDE, "");
        assert_eq!(
            base.clone().overlay_env().unwrap().api_key_env.as_deref(),
            Some("FROM_FILE")
        );
        env.set(api_key::API_KEY_ENV_OVERRIDE, "fake-xyzzy-not-a-name");
        let err = base.overlay_env().unwrap_err().to_string();
        assert!(!err.contains("fake-xyzzy"), "{err}");
        assert!(err.contains("api_key_env"), "{err}");
        env.remove(api_key::API_KEY_ENV_OVERRIDE);
    }

    /// Issue #21: an inline `api_key` in the file is refused at overlay (the
    /// file-resolve path) and at build (a config made in code), naming
    /// `api_key_env` as the fix.
    #[test]
    fn inline_api_key_is_refused_at_resolve() {
        let env = crate::test_util::env_lock();
        env.remove(api_key::API_KEY_ENV_OVERRIDE);
        let cfg: EmbedderConfig = toml::from_str("api_key = \"fake-xyzzy\"\n").unwrap();
        let err = cfg.clone().overlay_env().unwrap_err().to_string();
        assert!(err.contains("api_key_env"), "{err}");
        let Err(err) = build_embedder(cfg) else {
            panic!("an inline api_key must not build");
        };
        assert!(err.to_string().contains("api_key_env"), "{err}");
    }

    /// Issue #21: `api_key_env` with a kind that would ignore it is refused,
    /// not silently dropped.
    #[test]
    fn api_key_env_is_refused_for_other_kinds() {
        for kind in [
            EmbedderKind::Fixture,
            EmbedderKind::Candle,
            EmbedderKind::Gemini,
        ] {
            let Err(err) = build_embedder(EmbedderConfig {
                kind,
                api_key_env: Some("CLOUDFLARE_API_TOKEN".into()),
                ..Default::default()
            }) else {
                panic!("{kind}: api_key_env must be refused");
            };
            let msg = err.to_string();
            assert!(
                msg.contains("api_key_env") && msg.contains("bge_m3"),
                "{msg}"
            );
        }
    }

    /// Issue #21: a plain-http, non-loopback URL with `api_key_env` is
    /// refused at resolve, before the variable is read: the error is the
    /// transport one even though the variable is unset.
    ///
    /// Mutation: drop the `check_bearer_transport` call in `build_embedder`
    /// -> red (the error becomes "not set").
    #[cfg(feature = "embed-bge")]
    #[test]
    fn api_key_env_over_plain_http_to_a_remote_host_is_refused_at_resolve() {
        let Err(err) = build_embedder(EmbedderConfig {
            kind: EmbedderKind::BgeM3,
            llama_url: Some("http://embeddings.example.com:8080".into()),
            api_key_env: Some("LAMBO_TEST_ISSUE21_NEVER_SET_TOKEN".into()),
            ..Default::default()
        }) else {
            panic!("a token over plain http to a remote host must be refused");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("embeddings.example.com") && msg.contains("https"),
            "{msg}"
        );
        assert!(
            !msg.contains("not set"),
            "transport must be checked first: {msg}"
        );
    }

    /// Issue #13: auto keep-warm is off for every non-candle adapter — the
    /// fixture has no weights, and the remote adapters' weights live in another
    /// process.
    #[cfg(feature = "embed-fixture")]
    #[test]
    fn fixture_is_not_reported_as_unified_memory() {
        let e = FixtureEmbedder::new();
        assert!(!weights_in_unified_memory(&e));
        assert_eq!(
            keep_warm::resolve_keep_warm(None, weights_in_unified_memory(&e)),
            None
        );
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
        let w: Wrap = toml::from_str(r#"kind = "openai""#).unwrap();
        assert_eq!(w.kind, EmbedderKind::BgeM3);
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

    /// Issue #21: `kind = "openai"` in a file builds the same adapter, and a
    /// re-serialized config writes it back as `bge_m3`.
    #[test]
    #[cfg(feature = "embed-bge")]
    fn openai_kind_builds_the_bge_adapter() {
        let cfg: EmbedderConfig =
            toml::from_str("kind = \"openai\"\nurl = \"http://127.0.0.1:9\"\n").unwrap();
        assert_eq!(cfg.kind, EmbedderKind::BgeM3);
        assert!(toml::to_string(&cfg).unwrap().contains("kind = \"bge_m3\""));
        let e = build_embedder(cfg).unwrap();
        assert_eq!(e.dimensions(), 1024);
        assert_eq!(candle_identity(e.as_ref()), None);
        assert_eq!(gemini_identity(e.as_ref()), None);
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
        // Both credential variables are cleared under the env lock rather than assumed
        // absent: the embedder now reads GCP_LAMBO_CREDENTIALS too, so a developer with
        // either exported would otherwise see this test build an embedder instead of
        // refusing, and a sibling test setting one would race it.
        let env = crate::test_util::env_lock();
        env.remove("GCP_LAMBO_CREDENTIALS");
        env.remove("GOOGLE_APPLICATION_CREDENTIALS");
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
        let dir = crate::test_util::ScratchDir::new("lambo-a3-gemini");
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
    }

    /// One credential variable means one identity: the embedder resolves
    /// `GCP_LAMBO_CREDENTIALS` exactly as the Postgres store does.
    ///
    /// Before this pin the store read `GCP_LAMBO_CREDENTIALS` then
    /// `GOOGLE_APPLICATION_CREDENTIALS` while the embedder read only the second, so the
    /// export block in `L-gcp-hosted-postgres.md` started the store and refused the
    /// embedder. Both arms are asserted, because a fix that swapped one hardcoded
    /// variable for another would pass half of this.
    #[test]
    #[cfg(feature = "embed-gemini")]
    fn gemini_resolves_the_shared_credential_variable() {
        use super::gemini::TEST_RSA_PRIVATE_KEY_PEM;
        let env = crate::test_util::env_lock();
        let dir = crate::test_util::ScratchDir::new("lambo-l1-shared-cred");
        let creds_path = dir.join("sa.json");
        std::fs::write(
            &creds_path,
            serde_json::json!({
                "client_email": "test@example.com",
                "private_key": TEST_RSA_PRIVATE_KEY_PEM,
                "token_uri": "https://oauth2.googleapis.com/token",
                "project_id": "proj",
            })
            .to_string(),
        )
        .unwrap();
        let cfg = || EmbedderConfig {
            kind: EmbedderKind::Gemini,
            dim: 1536,
            gemini_project: Some("proj".to_string()),
            gemini_location: Some("us-central1".to_string()),
            gemini_credentials: None,
            ..Default::default()
        };

        // Arm 1: the shared variable alone. This is the arm that used to refuse.
        env.set("GCP_LAMBO_CREDENTIALS", &creds_path);
        env.remove("GOOGLE_APPLICATION_CREDENTIALS");
        let built = build_embedder(cfg());
        let arm1 = built.map(|e| crate::embed::gemini_identity(e.as_ref()));

        // Arm 2: the Google-standard variable alone, which must keep working.
        env.remove("GCP_LAMBO_CREDENTIALS");
        env.set("GOOGLE_APPLICATION_CREDENTIALS", &creds_path);
        let built = build_embedder(cfg());
        let arm2 = built.map(|e| crate::embed::gemini_identity(e.as_ref()));

        // Arm 3: neither. The refusal must name both variables and the config key, so an
        // operator reading it knows every way to answer it.
        env.remove("GCP_LAMBO_CREDENTIALS");
        env.remove("GOOGLE_APPLICATION_CREDENTIALS");
        let arm3 = build_embedder(cfg()).err().map(|e| e.to_string());

        // Arm 4 (L1-R2-2): the config key outranks BOTH variables. Both point at a file
        // that does not exist, so an embedder that consulted the environment first cannot
        // build, and one that honours the config key can.
        env.set("GCP_LAMBO_CREDENTIALS", dir.join("absent-shared.json"));
        env.set(
            "GOOGLE_APPLICATION_CREDENTIALS",
            dir.join("absent-adc.json"),
        );
        let arm4 = build_embedder(EmbedderConfig {
            gemini_credentials: Some(creds_path.clone()),
            ..cfg()
        })
        .map(|e| crate::embed::gemini_identity(e.as_ref()));

        let id1 = arm1.unwrap_or_else(|e| {
            panic!(
                "GCP_LAMBO_CREDENTIALS alone must build the embedder, as it builds the store: {e}"
            )
        });
        assert_eq!(id1.as_deref(), Some("gemini-embedding-001"));
        let id2 = arm2
            .unwrap_or_else(|e| panic!("GOOGLE_APPLICATION_CREDENTIALS must keep working: {e}"));
        assert_eq!(id2.as_deref(), Some("gemini-embedding-001"));
        let msg = arm3.expect("neither variable set must refuse, not build");
        let id4 = arm4.unwrap_or_else(|e| {
            panic!("`gemini_credentials` must outrank both environment variables: {e}")
        });
        assert_eq!(id4.as_deref(), Some("gemini-embedding-001"));
        for named in [
            "gemini_credentials",
            "GCP_LAMBO_CREDENTIALS",
            "GOOGLE_APPLICATION_CREDENTIALS",
        ] {
            assert!(msg.contains(named), "refusal must name {named}, got: {msg}");
        }
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
