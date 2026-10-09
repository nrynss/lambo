//! EmbeddingGemma 2 over llama.cpp's `llama-server`: text and images in one
//! embedding space (#22 PR 5).
//!
//! A thin layer on the [`BgeM3LlamaCppEmbedder`] HTTP client, so everything
//! #21 promises for that client holds here unchanged: the optional bearer
//! token, the https-or-loopback transport rule, no redirects, the capped and
//! scrubbed error body and the J3 status table. On top of it this layer adds
//! what makes the vectors EmbeddingGemma 2's, pinned as the prompt profile
//! [`EG2_PROMPT_PROFILE`] (`lambo-eg2-v1`):
//!
//! - **Role prefixes** from the model card. A document ([`Embedder::embed`])
//!   is sent as `title: none | text: <text>`, a recall query
//!   ([`Embedder::embed_query`]) as `task: search result | query: <text>`, and
//!   an image with no prefix. The server adds none, and the same sentence in
//!   the two roles embeds at cosine 0.96, so the prefix is part of the space.
//! - **A fixed 280-token image budget.** The operator starts `llama-server`
//!   with `--image-min-tokens 280 --image-max-tokens 280`; the budget changes
//!   the vectors, and only a fixed one makes them independent of the image's
//!   pixel size. The server does not report its budget, so the adapter reads
//!   the token count of each image response and refuses one outside the
//!   budget (see [`EG2_IMAGE_TOKENS`]).
//! - **MRL**: the server returns the native 768 dimensions; the adapter
//!   checks them, truncates to `dim` (768, 512, 256 or 128) and then
//!   L2-normalizes.
//!
//! The contract `model` is the configured weights artifact plus the profile,
//! `<model>;prompts=lambo-eg2-v1` ([`EmbeddingGemma2Embedder::model_identity`]),
//! because `llama-server` ignores the request's model name: two servers
//! answering to the same name can hold different weights.
//!
//! **Startup check, best effort.** Before its first embed the adapter asks
//! the server's `GET /props`, which `llama-server` (checked on b11517)
//! answers with the loaded file (`model_path`), its quantization
//! (`model_ftype`) and `modalities.vision`. A file whose name does not name
//! EmbeddingGemma 2, or a quantization other than the one the configured
//! artifact names, refuses every embed; a server without a vision projector
//! refuses image embeds while `images` is on. A server that does not answer
//! `/props` that way (a hosted endpoint, Ollama) is used unchecked, and the
//! adapter says so once in the log. The check runs lazily because resolve is
//! synchronous; it is repeated until it passes, so fixing the server needs no
//! restart of Lambo.

use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use super::bge_m3::{
    check_bearer_transport, default_status_rule, BgeM3LlamaCppEmbedder, EmbedStatusClass,
    StatusVerdict,
};
use super::{api_key, EmbedError, Embedder, EmbedderConfig, ImageInput, Modalities};

#[cfg(test)]
mod tests;

/// The prompt profile this adapter implements, named in the contract `model`.
/// Changing any part of it (a prefix, the image budget, the order of
/// truncation and normalization) is a new profile name, so a new contract.
pub const EG2_PROMPT_PROFILE: &str = "lambo-eg2-v1";

/// The weights artifact the contract names when `[embedder] model` is unset:
/// the Q8_0 GGUF of `ggml-org/embeddinggemma-2-GGUF` at revision `bfcd2987`
/// (converted from `google/embeddinggemma-2` at `914f7f89`).
pub const EG2_DEFAULT_MODEL: &str = "ggml-org/embeddinggemma-2-GGUF@bfcd2987/Q8_0";

/// The width EmbeddingGemma 2 emits before MRL truncation.
pub const EG2_NATIVE_DIM: usize = 768;

/// The widths `[embedder] dim` may take (Matryoshka truncation points).
pub const EG2_MRL_DIMS: [usize; 4] = [768, 512, 256, 128];

/// The image budget the profile pins, in soft tokens: the model card's
/// default, set on the server with `--image-min-tokens 280
/// --image-max-tokens 280`.
pub const EG2_IMAGE_TOKENS: u64 = 280;

/// Tokens an image request may carry beyond [`EG2_IMAGE_TOKENS`]: BOS, EOS and
/// the image's start and end markers. b11517 reports 293 for every image at
/// the fixed budget; a count outside `280..=280 + slack` means the server is
/// not running the profile's budget (its default sizes images at 85 to 125
/// tokens, and a `--ubatch-size` under the budget caps it at 256).
const EG2_IMAGE_FRAMING_SLACK: u64 = 32;

/// The document-role prefix (`embed`), from the model card.
pub const EG2_DOCUMENT_PREFIX: &str = "title: none | text: ";

/// The query-role prefix (`embed_query`), from the model card.
pub const EG2_QUERY_PREFIX: &str = "task: search result | query: ";

const PROPS_TIMEOUT: Duration = Duration::from_secs(5);

/// At most this much of a `/props` body is read. b11517's is about 6 KiB,
/// most of it the chat template.
const PROPS_READ_CAP: usize = 256 * 1024;

/// Appended to the error for an image sent to a server with no vision
/// projector.
const MMPROJ_HINT: &str = " (this llama-server has no vision projector: restart it with \
    --mmproj <mmproj-embeddinggemma-2-*.gguf> --image-min-tokens 280 --image-max-tokens 280, \
    or set [embedder] images = false)";

/// Appended to the error for an image the server could not decode.
const DECODE_HINT: &str = " (the server could not decode this image)";

#[derive(Debug, Serialize)]
struct TextRequest<'a> {
    model: &'a str,
    input: String,
}

/// `{"model": .., "input": [{"content": [{"type": "image_url", "image_url":
/// {"url": "data:<mime>;base64,<..>"}}]}]}`: one input item whose OpenAI-style
/// content is the image alone.
#[derive(Debug, Serialize)]
struct ImageRequest<'a> {
    model: &'a str,
    input: [ImageItem; 1],
}

#[derive(Debug, Serialize)]
struct ImageItem {
    content: [ImagePart; 1],
}

#[derive(Debug, Serialize)]
struct ImagePart {
    #[serde(rename = "type")]
    kind: &'static str,
    image_url: ImageUrl,
}

#[derive(Debug, Serialize)]
struct ImageUrl {
    url: String,
}

#[derive(Debug, Deserialize)]
struct EmbedResponse {
    data: Vec<EmbedData>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct EmbedData {
    embedding: Vec<f32>,
}

#[derive(Debug, Deserialize)]
struct Usage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
}

/// The part of `llama-server`'s `GET /props` the check reads.
#[derive(Debug, Deserialize)]
struct Props {
    #[serde(default)]
    model_path: Option<String>,
    #[serde(default)]
    model_ftype: Option<String>,
    #[serde(default)]
    modalities: Option<PropsModalities>,
}

#[derive(Debug, Deserialize)]
struct PropsModalities {
    #[serde(default)]
    vision: Option<bool>,
}

/// What the best-effort `/props` check concluded
/// ([`EmbeddingGemma2Embedder::check_server`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Eg2ServerCheck {
    /// The server named an EmbeddingGemma 2 file in the configured
    /// quantization. `vision` is its reported vision support (`None` when not
    /// reported); `Some(false)` with `images` on refuses image embeds.
    Verified { vision: Option<bool> },
    /// The server serves something else. Every embed is refused with this
    /// message until the check passes.
    Mismatch(String),
    /// The server does not report its model over `/props` (a hosted
    /// endpoint, Ollama), so nothing could be checked.
    NotExposed,
    /// The check could not run (server unreachable, a 5xx, or the check is
    /// turned off); it is tried again on the next embed.
    Skipped,
}

/// The status rule for an image request. `llama-server` answers two image
/// faults with `500`, which the J3 table reads as a busy server (transient,
/// retried forever):
///
/// - no vision projector loaded ("... provide the mmproj"): a permanent
///   deployment fault, so `PermanentConfig`, naming `--mmproj`;
/// - an image it cannot decode ("Failed to load image ..."): a fact about
///   this input, so `Content`.
///
/// Anything else follows the table.
pub(crate) fn image_status_rule(code: u16, body: &str) -> StatusVerdict {
    if code == 500 && body.contains("provide the mmproj") {
        return StatusVerdict {
            class: EmbedStatusClass::PermanentConfig,
            hint: Some(MMPROJ_HINT),
        };
    }
    if code == 500 && body.contains("Failed to load image") {
        return StatusVerdict {
            class: EmbedStatusClass::Content,
            hint: Some(DECODE_HINT),
        };
    }
    default_status_rule(code, body)
}

/// The quantization a configured artifact names: the segment after the last
/// `/` that follows an `@revision`, e.g. `Q8_0` in
/// `ggml-org/embeddinggemma-2-GGUF@bfcd2987/Q8_0`. `None` when the string has
/// no such segment (an HF id, an Ollama tag).
fn configured_quant(model: &str) -> Option<&str> {
    let (_, after_rev) = model.split_once('@')?;
    let (_, quant) = after_rev.rsplit_once('/')?;
    (!quant.is_empty()).then_some(quant)
}

/// Judge what `/props` reported against the configured artifact. Pure, so the
/// rules are unit-tested without a server.
fn judge_props(
    configured_model: &str,
    log_url: &str,
    model_path: &str,
    model_ftype: Option<&str>,
    vision: Option<bool>,
) -> Eg2ServerCheck {
    let file = model_path.rsplit(['/', '\\']).next().unwrap_or(model_path);
    // A file name is shown, never the directory (it may name a user), and
    // only its first 128 characters.
    let shown: String = file.chars().take(128).collect();
    let folded: String = file
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if !folded.contains("embeddinggemma2") {
        return Eg2ServerCheck::Mismatch(format!(
            "the llama-server at {log_url} has loaded `{shown}`, whose name does not name \
             EmbeddingGemma 2, but [embedder] kind is `embeddinggemma2` (model \
             {configured_model:?}); refusing to embed so no vector from another model enters \
             this contract. Point [embedder] url at the EmbeddingGemma 2 server (if the file \
             is EmbeddingGemma 2 under another name, give it a name containing \
             embeddinggemma-2)"
        ));
    }
    if let Some(want) = configured_quant(configured_model)
        && let Some(have) = model_ftype
        && !want.eq_ignore_ascii_case(have)
    {
        return Eg2ServerCheck::Mismatch(format!(
            "the llama-server at {log_url} has loaded `{shown}` quantized as {have}, but \
             [embedder] model {configured_model:?} names {want}; vectors from another \
             quantization are a different embedding contract. Load the {want} GGUF, or set \
             model to the artifact the server runs (a new contract: start a fresh session or \
             run lambo re-embed)"
        ));
    }
    Eg2ServerCheck::Verified { vision }
}

/// Truncate a native-width vector to `dim` (MRL), then L2-normalize. Refuses
/// a vector that is not EmbeddingGemma 2's native width, has a non-finite
/// component anywhere (a broken backend; the discarded tail included), or
/// has a zero norm once truncated.
fn truncate_and_normalize(mut v: Vec<f32>, dim: usize) -> Result<Vec<f32>, EmbedError> {
    if v.len() != EG2_NATIVE_DIM {
        return Err(EmbedError::Backend(format!(
            "llama.cpp returned {} dims, but EmbeddingGemma 2 emits {EG2_NATIVE_DIM}; the \
             server is not running EmbeddingGemma 2",
            v.len()
        )));
    }
    if v.iter().any(|x| !x.is_finite()) {
        return Err(EmbedError::Backend(
            "llama.cpp returned a non-finite embedding".into(),
        ));
    }
    v.truncate(dim);
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm <= f32::EPSILON {
        return Err(EmbedError::Backend(
            "llama.cpp returned a zero-norm embedding".into(),
        ));
    }
    for x in &mut v {
        *x /= norm;
    }
    Ok(v)
}

/// EmbeddingGemma 2 (text and image) over `llama-server`. Build it from
/// config with [`crate::embed::build_embedder`] (`kind = "embeddinggemma2"`).
#[derive(Debug)]
pub struct EmbeddingGemma2Embedder {
    /// The #21 client: URL, bearer, transport rule, scrub, status table.
    http: BgeM3LlamaCppEmbedder,
    /// The configured weights artifact, sent as the request's `model` and
    /// named in the contract.
    model: String,
    /// `<model>;prompts=<profile>`, the contract `model`.
    identity: String,
    /// Output width after MRL truncation.
    dim: usize,
    images: bool,
    server_check: bool,
    /// The last `/props` answer worth keeping: `Verified` or `NotExposed`.
    /// Never held across an await.
    kept: Mutex<Option<Eg2ServerCheck>>,
    /// The last failed-check message logged, so a refusal is logged once and
    /// not on every call.
    last_logged: Mutex<Option<String>>,
}

impl EmbeddingGemma2Embedder {
    /// An adapter for the `llama-server` at `url` (the base URL; the
    /// `v1/embeddings` path is appended).
    ///
    /// * `model`: the weights artifact (see [`EG2_DEFAULT_MODEL`]); it may
    ///   not contain `;`, which separates the profile in the contract.
    /// * `dim`: one of [`EG2_MRL_DIMS`].
    ///
    /// Images are on; see [`Self::with_images`].
    pub fn new(
        url: impl Into<String>,
        model: impl Into<String>,
        dim: usize,
    ) -> Result<Self, EmbedError> {
        let model = model.into();
        if !EG2_MRL_DIMS.contains(&dim) {
            return Err(EmbedError::Unavailable(format!(
                "EmbeddingGemma 2 supports dim 768, 512, 256 or 128 (Matryoshka truncation), got \
                 {dim}; set [embedder] dim = 768 (the 1024 default is BGE-M3's)"
            )));
        }
        if model.trim().is_empty() {
            return Err(EmbedError::Unavailable(
                "the EmbeddingGemma 2 model (the weights artifact) is empty".into(),
            ));
        }
        if model.contains(';') {
            return Err(EmbedError::Unavailable(format!(
                "the EmbeddingGemma 2 model {model:?} contains `;`, which separates the prompt \
                 profile in the embedding contract; name the artifact without it"
            )));
        }
        let identity = format!("{model};prompts={EG2_PROMPT_PROFILE}");
        Ok(Self {
            http: BgeM3LlamaCppEmbedder::new(url, model.clone(), EG2_NATIVE_DIM)?,
            model,
            identity,
            dim,
            images: true,
            server_check: true,
            kept: Mutex::new(None),
            last_logged: Mutex::new(None),
        })
    }

    /// Embed images (`true`, the default) or report text only and refuse every
    /// image with [`EmbedError::Unsupported`] without sending it (`false`).
    pub fn with_images(mut self, images: bool) -> Self {
        self.images = images;
        self
    }

    /// Send `Authorization: Bearer <token>`, with the #21 rules
    /// ([`BgeM3LlamaCppEmbedder::with_bearer_token`]).
    pub fn with_bearer_token(mut self, token: &str) -> Result<Self, EmbedError> {
        self.http = self.http.with_bearer_token(token)?;
        Ok(self)
    }

    /// Override connect/request timeouts.
    pub fn with_timeouts(
        mut self,
        connect: Duration,
        request: Duration,
    ) -> Result<Self, EmbedError> {
        self.http = self.http.with_timeouts(connect, request)?;
        Ok(self)
    }

    /// Skip the `/props` check: every request goes to the server, and its own
    /// answer decides. For a server whose `/props` is known to mislead, and
    /// for tests that need the server's refusal rather than the check's.
    pub fn without_server_check(mut self) -> Self {
        self.server_check = false;
        self
    }

    /// The contract `model`: `<artifact>;prompts=lambo-eg2-v1`.
    pub fn model_identity(&self) -> &str {
        &self.identity
    }

    /// Run the `/props` check now, or return its kept answer. A
    /// [`Eg2ServerCheck::Mismatch`] is returned as a value; the embed methods
    /// turn it into a refusal.
    pub async fn check_server(&self) -> Eg2ServerCheck {
        self.check(true).await
    }

    /// The check as an embed of the given kind needs it. A kept `Verified`
    /// or `NotExposed` answers without a request, except that an image embed
    /// asks again while the kept answer says "no vision", so restarting the
    /// server with `--mmproj` is picked up without restarting Lambo. A
    /// mismatch is never kept: it is asked again on every embed until the
    /// server is fixed.
    async fn check(&self, image: bool) -> Eg2ServerCheck {
        if !self.server_check {
            return Eg2ServerCheck::Skipped;
        }
        let kept = self.kept.lock().ok().and_then(|k| k.clone());
        if let Some(kept) = kept
            && !(image && self.images && kept == Self::NO_VISION)
        {
            return kept;
        }
        let outcome = self.probe_props().await;
        if matches!(
            outcome,
            Eg2ServerCheck::Verified { .. } | Eg2ServerCheck::NotExposed
        ) {
            let changed = match self.kept.lock() {
                Ok(mut kept) if kept.as_ref() != Some(&outcome) => {
                    *kept = Some(outcome.clone());
                    true
                }
                _ => false,
            };
            if changed {
                self.log_kept(&outcome);
            }
        }
        if let Some(message) = self.failure_message(&outcome) {
            self.log_failure_once(&message);
        }
        outcome
    }

    const NO_VISION: Eg2ServerCheck = Eg2ServerCheck::Verified {
        vision: Some(false),
    };

    fn log_kept(&self, outcome: &Eg2ServerCheck) {
        let url = self.http.log_base_url();
        match outcome {
            Eg2ServerCheck::NotExposed => tracing::warn!(
                url = %url,
                "the embedder at {url} does not report its model over /props, so Lambo cannot \
                 check that it serves {} or that its image budget is {EG2_IMAGE_TOKENS} tokens; \
                 the embedding contract rests on the operator's word",
                self.model
            ),
            Eg2ServerCheck::Verified { vision } => tracing::info!(
                url = %url,
                vision = ?vision,
                "llama-server at {url} serves EmbeddingGemma 2 as configured"
            ),
            Eg2ServerCheck::Mismatch(_) | Eg2ServerCheck::Skipped => {}
        }
    }

    /// The refusal a failed check implies, if any.
    fn failure_message(&self, outcome: &Eg2ServerCheck) -> Option<String> {
        match outcome {
            Eg2ServerCheck::Mismatch(message) => Some(message.clone()),
            Eg2ServerCheck::Verified {
                vision: Some(false),
            } if self.images => Some(self.no_vision_message()),
            _ => None,
        }
    }

    fn no_vision_message(&self) -> String {
        format!(
            "the llama-server at {} reports no vision support, but [embedder] images is on: \
             image embeds are refused{MMPROJ_HINT}",
            self.http.log_base_url()
        )
    }

    fn log_failure_once(&self, message: &str) {
        // The guard is dropped at the end of this statement, before any await.
        let fresh = match self.last_logged.lock() {
            Ok(mut last) if last.as_deref() != Some(message) => {
                *last = Some(message.to_string());
                true
            }
            _ => false,
        };
        if fresh {
            tracing::error!(
                url = %self.http.log_base_url(),
                "EmbeddingGemma 2 server check failed: {message}"
            );
        }
    }

    async fn probe_props(&self) -> Eg2ServerCheck {
        let Ok(mut resp) = self
            .http
            .authorized_get("/props", PROPS_TIMEOUT)
            .send()
            .await
        else {
            return Eg2ServerCheck::Skipped;
        };
        let status = resp.status();
        if status.is_server_error() {
            return Eg2ServerCheck::Skipped;
        }
        if !status.is_success() {
            return Eg2ServerCheck::NotExposed;
        }
        let mut body = Vec::new();
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    if body.len() + chunk.len() > PROPS_READ_CAP {
                        return Eg2ServerCheck::NotExposed;
                    }
                    body.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(_) => return Eg2ServerCheck::Skipped,
            }
        }
        let Ok(props) = serde_json::from_slice::<Props>(&body) else {
            return Eg2ServerCheck::NotExposed;
        };
        let Some(model_path) = props.model_path else {
            return Eg2ServerCheck::NotExposed;
        };
        judge_props(
            &self.model,
            &self.http.log_base_url(),
            &model_path,
            props.model_ftype.as_deref(),
            props.modalities.and_then(|m| m.vision),
        )
    }

    /// Refuse before sending when the check has found the server wrong.
    async fn ensure_server(&self, image: bool) -> Result<(), EmbedError> {
        match self.check(image).await {
            Eg2ServerCheck::Mismatch(message) => Err(EmbedError::Backend(message)),
            Eg2ServerCheck::Verified {
                vision: Some(false),
            } if image => Err(EmbedError::Backend(self.no_vision_message())),
            _ => Ok(()),
        }
    }

    async fn embed_text(&self, prefix: &str, text: &str) -> Result<Vec<f32>, EmbedError> {
        if text.trim().is_empty() {
            return Err(EmbedError::Unavailable(
                "cannot embed empty/whitespace text".into(),
            ));
        }
        self.ensure_server(false).await?;
        let body = TextRequest {
            model: &self.model,
            input: format!("{prefix}{text}"),
        };
        let parsed: EmbedResponse = self
            .http
            .post_json(&body, &self.model, default_status_rule)
            .await?;
        truncate_and_normalize(first_embedding(parsed.data)?, self.dim)
    }
}

fn first_embedding(data: Vec<EmbedData>) -> Result<Vec<f32>, EmbedError> {
    data.into_iter()
        .next()
        .map(|d| d.embedding)
        .ok_or_else(|| EmbedError::Backend("llama.cpp returned an empty embeddings list".into()))
}

#[async_trait]
impl Embedder for EmbeddingGemma2Embedder {
    fn dimensions(&self) -> usize {
        self.dim
    }

    /// The document role: `title: none | text: <text>`.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        self.embed_text(EG2_DOCUMENT_PREFIX, text).await
    }

    /// The query role: `task: search result | query: <text>`.
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        self.embed_text(EG2_QUERY_PREFIX, text).await
    }

    fn modalities(&self) -> Modalities {
        if self.images {
            Modalities::TEXT | Modalities::IMAGE
        } else {
            Modalities::TEXT
        }
    }

    /// The image alone, no prefix, as a base64 data URI in the nested
    /// `image_url` shape. The response's token count must show the fixed
    /// 280-token budget.
    async fn embed_image(&self, image: ImageInput<'_>) -> Result<Vec<f32>, EmbedError> {
        if !self.images {
            return Err(EmbedError::Unsupported(
                "this EmbeddingGemma 2 embedder has images turned off ([embedder] images = \
                 false)"
                    .into(),
            ));
        }
        self.ensure_server(true).await?;
        let data_uri = format!(
            "data:{};base64,{}",
            image.mime().as_str(),
            base64::engine::general_purpose::STANDARD.encode(image.bytes())
        );
        let body = ImageRequest {
            model: &self.model,
            input: [ImageItem {
                content: [ImagePart {
                    kind: "image_url",
                    image_url: ImageUrl { url: data_uri },
                }],
            }],
        };
        let parsed: EmbedResponse = self
            .http
            .post_json(&body, &self.model, image_status_rule)
            .await?;
        if let Some(tokens) = parsed.usage.as_ref().and_then(|u| u.prompt_tokens)
            && !(EG2_IMAGE_TOKENS..=EG2_IMAGE_TOKENS + EG2_IMAGE_FRAMING_SLACK).contains(&tokens)
        {
            let message = format!(
                "the llama-server at {} embedded this image as {tokens} tokens, but profile \
                 {EG2_PROMPT_PROFILE} fixes the image budget at {EG2_IMAGE_TOKENS} soft tokens \
                 (293 tokens with framing on b11517), so its vector would be in another space. \
                 Restart it with --image-min-tokens 280 --image-max-tokens 280, and with a \
                 ubatch that holds a whole image (llama-server otherwise caps the budget to fit \
                 its default 512; e.g. --batch-size 8192 --ubatch-size 8192)",
                self.http.log_base_url()
            );
            self.log_failure_once(&message);
            return Err(EmbedError::Backend(message));
        }
        truncate_and_normalize(first_embedding(parsed.data)?, self.dim)
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }
}

/// Build from resolved config (`kind = "embeddinggemma2"`): `url` (default
/// `http://127.0.0.1:8080`), `model` (default [`EG2_DEFAULT_MODEL`]), `dim`,
/// `images` (default on), `api_key_env` (the #21 rules: the transport is
/// checked before the variable is read).
pub(crate) fn build(cfg: &EmbedderConfig) -> Result<EmbeddingGemma2Embedder, EmbedError> {
    let url = cfg
        .llama_url
        .clone()
        .unwrap_or_else(|| "http://127.0.0.1:8080".to_string());
    let model = cfg
        .llama_model
        .clone()
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| EG2_DEFAULT_MODEL.to_string());
    let mut embedder = EmbeddingGemma2Embedder::new(url.clone(), model, cfg.dim)?
        .with_images(cfg.images.unwrap_or(true));
    if let Some(name) = cfg.api_key_env.as_deref() {
        check_bearer_transport(&url)?;
        embedder = embedder.with_bearer_token(&api_key::resolve(name)?)?;
    }
    Ok(embedder)
}
