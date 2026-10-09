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
//!   pixel size (exactly up to 768 px a side on b11517; larger renders come
//!   out close but not equal). The server does not report its budget, so for
//!   a server `/props` verified the adapter embeds a reference image and
//!   requires its known token count ([`EG2_REFERENCE_IMAGE_TOKENS`]).
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
//! restart of Lambo. A passing answer is trusted for
//! [`EG2_PROPS_RECHECK_INTERVAL`] (60 s) and dropped at once when an embed
//! request fails as unavailable, so a server restarted with another model or
//! quantization is caught before more vectors are stamped under the old
//! contract. Once a server has been verified, a re-check that cannot run
//! holds embeds back (transient) and a server that stops reporting its model
//! is refused. A check that cannot run (unreachable, a 5xx) is logged once
//! per run of failures and retried after a wait that doubles from 1 s up to
//! 30 s, not on every embed.

use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use super::bge_m3::{
    check_bearer_transport, default_status_rule, BgeM3LlamaCppEmbedder, EmbedStatusClass,
    StatusVerdict, LLAMA_UNREACHABLE,
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

/// Lambo's reference image: a 1x1 RGBA PNG, base64.
const REFERENCE_IMAGE_PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

/// The prompt tokens b11517 reports for [`REFERENCE_IMAGE_PNG`] at the
/// profile's budget.
///
/// The budget bounds an image's area in patches, not its token count, so the
/// count of an arbitrary image says little about the budget: at the
/// profile's 280, b11517 reports 236 to 540 prompt tokens across 100 sizes
/// and shapes (293 for a square up to 768 px, 260 for one of 800 px or more,
/// 268 for 1100x600, 250 for 2000x330, 540 for 16x4096), and the counts of
/// a capped (256) or the dynamic default budget overlap that range. One
/// fixed image tells them apart. Before the first image it embeds for a server `/props`
/// verified (and again every [`EG2_PROPS_RECHECK_INTERVAL`]), the adapter
/// embeds the reference image and requires this count within
/// [`REFERENCE_TOKENS_TOLERANCE`]. Measured on b11517 for the same image:
/// 260 when a 512 ubatch caps the budget to 256, 85 at the dynamic default,
/// 328 at a budget of 300.
pub const EG2_REFERENCE_IMAGE_TOKENS: u64 = 293;

/// How far a build's framing may move the reference count: well inside the
/// 33 tokens between the profile (293) and the nearest other budget measured
/// (260 capped, 328 at 300).
const REFERENCE_TOKENS_TOLERANCE: u64 = 8;

/// The flags the image budget needs, quoted in every budget refusal.
const BUDGET_FLAGS_HINT: &str = "Restart it with --image-min-tokens 280 --image-max-tokens 280, \
    and with a ubatch that holds a whole image (llama-server otherwise caps the budget to fit its \
    default 512; e.g. --batch-size 8192 --ubatch-size 8192)";

/// The document-role prefix (`embed`), from the model card.
pub const EG2_DOCUMENT_PREFIX: &str = "title: none | text: ";

/// The query-role prefix (`embed_query`), from the model card.
pub const EG2_QUERY_PREFIX: &str = "task: search result | query: ";

const PROPS_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a kept `/props` answer is trusted before it is asked again. A
/// `llama-server` restarted on the same port with another model or
/// quantization keeps answering embeds at width 768, so only a fresh `/props`
/// shows the change; this bounds how long vectors can be stamped under the
/// old contract after such a restart that Lambo did not see fail. One GET of
/// about 6 KiB a minute is negligible next to the embeds it guards. An embed
/// request that gets no HTTP answer (refused, reset), or llama-server's 503
/// "Loading model", drops the kept answer at once, since that is what a
/// restart looks like from here; a 503 "busy" or another transient status
/// from a server that answered does not (see [`restart_seen`]).
pub const EG2_PROPS_RECHECK_INTERVAL: Duration = Duration::from_secs(60);

/// The first wait after a `/props` check that could not run (unreachable, a
/// 5xx, a broken body). Each further failure in a row doubles it, up to
/// [`PROPS_RETRY_MAX`]; embeds in between do not ask. A gateway that
/// answers 5xx for every path but the embeddings one then costs one GET
/// every half minute, not one per embed.
const PROPS_RETRY_BASE: Duration = Duration::from_secs(1);

/// The longest wait between two `/props` checks that could not run.
const PROPS_RETRY_MAX: Duration = Duration::from_secs(30);

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
    #[serde(default)]
    build_info: Option<String>,
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
    /// turned off). It is logged once and tried again after a wait that
    /// doubles from 1 s up to 30 s; embeds in between do not ask.
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

/// The rule for Lambo's own reference image ([`REFERENCE_IMAGE_PNG`]): as
/// [`image_status_rule`], except that a server that cannot decode the fixed
/// 1x1 PNG is a server or configuration fault (`PermanentConfig`), never a
/// fact about the user's image (review L2).
fn reference_status_rule(code: u16, body: &str) -> StatusVerdict {
    if code == 500 && body.contains("Failed to load image") {
        return StatusVerdict {
            class: EmbedStatusClass::PermanentConfig,
            hint: Some(REFERENCE_DECODE_HINT),
        };
    }
    image_status_rule(code, body)
}

const REFERENCE_DECODE_HINT: &str = " (the server could not decode Lambo's fixed 1x1 reference \
     PNG, so its image decoder or --mmproj is broken; this is not about the image being \
     embedded)";

/// llama.cpp's quantization names (`llama_ftype_name`, which `/props`
/// reports as `model_ftype`; the table as of b11517), each with the token
/// GGUF file names and Hugging Face repos use for it, which is what a
/// configured artifact names. llama.cpp prefixes a name with `(guessed) `
/// when the file does not record its type; that prefix is stripped first.
const LLAMA_FTYPE_NAMES: &[(&str, &str)] = &[
    ("all F32", "F32"),
    ("F16", "F16"),
    ("BF16", "BF16"),
    ("Q1_0", "Q1_0"),
    ("Q2_0", "Q2_0"),
    ("Q4_0", "Q4_0"),
    ("Q4_1", "Q4_1"),
    ("Q5_0", "Q5_0"),
    ("Q5_1", "Q5_1"),
    ("Q8_0", "Q8_0"),
    ("MXFP4 MoE", "MXFP4_MOE"),
    ("NVFP4", "NVFP4"),
    ("Q2_K - Medium", "Q2_K"),
    ("Q2_K - Small", "Q2_K_S"),
    ("Q3_K - Small", "Q3_K_S"),
    ("Q3_K - Medium", "Q3_K_M"),
    ("Q3_K - Large", "Q3_K_L"),
    ("Q4_K - Small", "Q4_K_S"),
    ("Q4_K - Medium", "Q4_K_M"),
    ("Q5_K - Small", "Q5_K_S"),
    ("Q5_K - Medium", "Q5_K_M"),
    ("Q6_K", "Q6_K"),
    ("TQ1_0 - 1.69 bpw ternary", "TQ1_0"),
    ("TQ2_0 - 2.06 bpw ternary", "TQ2_0"),
    ("IQ2_XXS - 2.0625 bpw", "IQ2_XXS"),
    ("IQ2_XS - 2.3125 bpw", "IQ2_XS"),
    ("IQ2_S - 2.5 bpw", "IQ2_S"),
    ("IQ2_M - 2.7 bpw", "IQ2_M"),
    ("IQ3_XS - 3.3 bpw", "IQ3_XS"),
    ("IQ3_XXS - 3.0625 bpw", "IQ3_XXS"),
    ("IQ1_S - 1.5625 bpw", "IQ1_S"),
    ("IQ1_M - 1.75 bpw", "IQ1_M"),
    ("IQ4_NL - 4.5 bpw", "IQ4_NL"),
    ("IQ4_XS - 4.25 bpw", "IQ4_XS"),
    ("IQ3_S - 3.4375 bpw", "IQ3_S"),
    ("IQ3_S mix - 3.66 bpw", "IQ3_M"),
];

/// The canonical token for a `model_ftype` llama.cpp reported (`Q4_K -
/// Medium` -> `Q4_K_M`, `all F32` -> `F32`, `(guessed) Q8_0` -> `Q8_0`).
/// `None` for a name not in the table (a newer build's type, or llama.cpp's
/// "unknown, may not work"), which is then not judged.
fn reported_quant(ftype: &str) -> Option<&'static str> {
    let name = ftype.trim();
    let name = name.strip_prefix("(guessed)").unwrap_or(name).trim();
    let name = name.strip_suffix("(guessed)").unwrap_or(name).trim();
    LLAMA_FTYPE_NAMES
        .iter()
        .find(|(reported, _)| reported.eq_ignore_ascii_case(name))
        .map(|(_, canonical)| *canonical)
}

/// The canonical quantization a configured artifact names: the segment after
/// the last `/` that follows an `@revision`, e.g. `Q8_0` in
/// `ggml-org/embeddinggemma-2-GGUF@bfcd2987/Q8_0`. The segment may also be a
/// file name (`embeddinggemma-2-Q4_K_M.gguf`): `.gguf` is dropped and its last
/// `-` or `.` separated part is taken. `None` when there is no such segment
/// (an HF id, an Ollama tag) or it names no quantization in the table, which
/// is then not judged.
fn configured_quant(model: &str) -> Option<&'static str> {
    let (_, after_rev) = model.split_once('@')?;
    let (_, segment) = after_rev.rsplit_once('/')?;
    let stem = match segment.len().checked_sub(".gguf".len()) {
        Some(cut)
            if segment.is_char_boundary(cut) && segment[cut..].eq_ignore_ascii_case(".gguf") =>
        {
            &segment[..cut]
        }
        _ => segment,
    };
    let token = stem.rsplit(['-', '.']).next()?;
    LLAMA_FTYPE_NAMES
        .iter()
        .find(|(_, canonical)| canonical.eq_ignore_ascii_case(token))
        .map(|(_, canonical)| *canonical)
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
    // Both sides are reduced to the canonical token first: llama.cpp reports
    // `Q4_K - Medium` for the file a repo calls `Q4_K_M`. A side that does
    // not reduce is not judged.
    if let Some(want) = configured_quant(configured_model)
        && let Some(have) = model_ftype
        && let Some(have_canonical) = reported_quant(have)
        && want != have_canonical
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

/// What the `/props` check has learned so far.
#[derive(Debug, Default)]
struct CheckState {
    /// The last answer worth keeping (`Verified` or `NotExposed`) and when it
    /// was judged; trusted for [`EG2_PROPS_RECHECK_INTERVAL`].
    kept: Option<(Eg2ServerCheck, Instant)>,
    /// `/props` has verified this server at least once in this process. From
    /// then on a fresh answer is required: a re-check that cannot run holds
    /// embeds back (transient), and a server that stops reporting its model
    /// is refused, because either can be a different server on the same URL.
    verified_once: bool,
    /// `/props` checks in a row that could not run; the first is logged.
    failed_probes: u32,
    /// No `/props` check before this, after one that could not run.
    retry_at: Option<Instant>,
    /// When the reference image last showed the profile's budget; trusted
    /// for [`EG2_PROPS_RECHECK_INTERVAL`] like a kept answer.
    budget_checked_at: Option<Instant>,
    /// The build the last `/props` answer named (`build_info`), for messages:
    /// at most 64 characters of `[A-Za-z0-9._-]`.
    build: Option<String>,
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
    /// [`EG2_PROPS_RECHECK_INTERVAL`], shortened by tests.
    recheck_after: Duration,
    /// [`PROPS_RETRY_BASE`], shortened by tests.
    retry_base: Duration,
    /// What the `/props` check has learned. Never held across an await.
    state: Mutex<CheckState>,
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
            recheck_after: EG2_PROPS_RECHECK_INTERVAL,
            retry_base: PROPS_RETRY_BASE,
            state: Mutex::new(CheckState::default()),
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

    /// Trust a kept `/props` answer for `interval` instead of
    /// [`EG2_PROPS_RECHECK_INTERVAL`] (tests).
    #[cfg(test)]
    pub(crate) fn with_props_recheck(mut self, interval: Duration) -> Self {
        self.recheck_after = interval;
        self
    }

    /// Wait `base` (doubling, up to [`PROPS_RETRY_MAX`]) after a `/props`
    /// check that could not run, instead of [`PROPS_RETRY_BASE`] (tests).
    #[cfg(test)]
    pub(crate) fn with_props_retry(mut self, base: Duration) -> Self {
        self.retry_base = base;
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
    /// or `NotExposed` younger than [`Self::recheck_after`] answers without
    /// a request, except that an image embed asks again while the kept
    /// answer says "no vision", so restarting the server with `--mmproj` is
    /// picked up without restarting Lambo. A mismatch is never kept: it is
    /// asked again on every embed until the server is fixed. Once a server
    /// has been verified, a later "does not report its model" is a mismatch
    /// (another server may have taken the URL).
    async fn check(&self, image: bool) -> Eg2ServerCheck {
        if !self.server_check {
            return Eg2ServerCheck::Skipped;
        }
        let kept = self.state().kept.clone();
        if let Some((kept, judged_at)) = kept
            && judged_at.elapsed() < self.recheck_after
            && !(image && self.images && kept == Self::NO_VISION)
        {
            return kept;
        }
        if self
            .state()
            .retry_at
            .is_some_and(|retry_at| Instant::now() < retry_at)
        {
            return Eg2ServerCheck::Skipped;
        }
        let (probed, skip_reason) = match self.probe_props().await {
            Ok(outcome) => (outcome, None),
            Err(reason) => (Eg2ServerCheck::Skipped, Some(reason)),
        };
        let (outcome, changed, first_failure) = {
            let mut state = self.state();
            let first_failure = match skip_reason {
                Some(_) => {
                    state.failed_probes = state.failed_probes.saturating_add(1);
                    let doublings = (state.failed_probes - 1).min(16);
                    let wait = self
                        .retry_base
                        .saturating_mul(1 << doublings)
                        .min(PROPS_RETRY_MAX);
                    state.retry_at = Some(Instant::now() + wait);
                    state.failed_probes == 1
                }
                None => {
                    state.failed_probes = 0;
                    state.retry_at = None;
                    false
                }
            };
            let outcome = match probed {
                Eg2ServerCheck::NotExposed if state.verified_once => {
                    Eg2ServerCheck::Mismatch(self.props_gone_message())
                }
                other => other,
            };
            let changed = match &outcome {
                Eg2ServerCheck::Verified { .. } | Eg2ServerCheck::NotExposed => {
                    let changed = state.kept.as_ref().map(|(k, _)| k) != Some(&outcome);
                    state.kept = Some((outcome.clone(), Instant::now()));
                    state.verified_once |= matches!(outcome, Eg2ServerCheck::Verified { .. });
                    changed
                }
                Eg2ServerCheck::Mismatch(_) | Eg2ServerCheck::Skipped => {
                    state.kept = None;
                    false
                }
            };
            (
                outcome,
                changed,
                first_failure.then_some(state.verified_once),
            )
        };
        if let (Some(verified_once), Some(reason)) = (first_failure, skip_reason) {
            self.log_unchecked(&reason, verified_once);
        }
        if changed {
            self.log_kept(&outcome);
        }
        if let Some(message) = self.failure_message(&outcome) {
            self.log_failure_once(&message);
        }
        outcome
    }

    /// The check's state. A panic while it was held cannot leave it
    /// half-written (every update is one assignment), so a poisoned lock is
    /// used as is.
    fn state(&self) -> MutexGuard<'_, CheckState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Drop the kept `/props` answer so the next embed asks again. Called
    /// when an embed request fails in a way [`restart_seen`] reads as a
    /// restart, since a restarted server may hold another model.
    fn forget_kept(&self) {
        let mut state = self.state();
        state.kept = None;
        state.budget_checked_at = None;
    }

    /// For a server `/props` verified: embed the reference image and require
    /// [`EG2_REFERENCE_IMAGE_TOKENS`], the one count that tells the
    /// profile's budget from a capped or default one. Kept for
    /// [`Self::recheck_after`]; a failure is not kept.
    async fn ensure_image_budget(&self) -> Result<(), EmbedError> {
        if self
            .state()
            .budget_checked_at
            .is_some_and(|at| at.elapsed() < self.recheck_after)
        {
            return Ok(());
        }
        let body = image_request(&self.model, "image/png", REFERENCE_IMAGE_PNG);
        // A refusal of the reference image is a server or configuration
        // problem, whatever class it has: say so rather than let it read as
        // a fault in the user's image. A transient failure stays transient.
        let parsed = self
            .post(&body, reference_status_rule)
            .await
            .map_err(|err| match err {
                EmbedError::Backend(m) => EmbedError::Backend(format!(
                    "Lambo's reference image check (a fixed 1x1 PNG embedded before images, to \
                     check the image budget) failed at the llama-server at {}, a server or \
                     configuration problem, not a fault in the image being embedded: {m}",
                    self.http.log_base_url()
                )),
                other => other,
            })?;
        let want = EG2_REFERENCE_IMAGE_TOKENS - REFERENCE_TOKENS_TOLERANCE
            ..=EG2_REFERENCE_IMAGE_TOKENS + REFERENCE_TOKENS_TOLERANCE;
        match parsed.usage.and_then(|u| u.prompt_tokens) {
            Some(tokens) if !want.contains(&tokens) => {
                let message = format!(
                    "the llama-server at {} embedded Lambo's reference image as {tokens} tokens, \
                     but at profile {EG2_PROMPT_PROFILE}'s image budget of {EG2_IMAGE_TOKENS} \
                     b11517 embeds it as {EG2_REFERENCE_IMAGE_TOKENS}, so the server's images \
                     would be in another space; image embeds are refused. {BUDGET_FLAGS_HINT}",
                    self.http.log_base_url()
                );
                self.log_failure_once(&message);
                Err(EmbedError::Backend(message))
            }
            // A llama-server sends usage with every embedding (b11517 does),
            // and this one passed the /props check, so a response without it
            // means the server changed shape and the budget cannot be
            // checked: refuse rather than embed images unchecked.
            None => {
                let build = self.state().build.clone();
                let message = format!(
                    "the llama-server at {} ({}) answered Lambo's reference image without \
                     usage.prompt_tokens, which llama-server b11517 sends, so the image budget \
                     of profile {EG2_PROMPT_PROFILE} cannot be checked; image embeds are \
                     refused. Run a llama.cpp build that reports usage (b11517 is the one \
                     checked), or set [embedder] images = false",
                    self.http.log_base_url(),
                    build.as_deref().unwrap_or("build not reported"),
                );
                self.log_failure_once(&message);
                Err(EmbedError::Backend(message))
            }
            Some(_) => {
                self.state().budget_checked_at = Some(Instant::now());
                Ok(())
            }
        }
    }

    /// Logged once for each run of `/props` checks that could not run.
    fn log_unchecked(&self, reason: &str, verified_once: bool) {
        let url = self.http.log_base_url();
        let effect = if verified_once {
            "embeds are held back (and retried) until it answers, because this server was \
             verified earlier"
        } else {
            "embeds go to the server unchecked meanwhile"
        };
        tracing::warn!(
            url = %url,
            "could not ask the embedder at {url} for /props ({reason}): {effect}; the check is \
             retried after a wait that doubles up to {} s, and this is logged once until it \
             answers",
            PROPS_RETRY_MAX.as_secs()
        );
    }

    fn props_gone_message(&self) -> String {
        format!(
            "the llama-server at {} was verified as EmbeddingGemma 2 earlier, but now does not \
             report its model over /props, so another server may have taken its URL; refusing \
             to embed so no vector from another model enters this contract. If the change is \
             intended, restart Lambo (and if the model changed, start a fresh session or run \
             lambo re-embed)",
            self.http.log_base_url()
        )
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

    /// Ask `/props` once. `Err` names why the check could not run (the
    /// server unreachable, a 5xx, a body cut off); every other answer is
    /// judged.
    async fn probe_props(&self) -> Result<Eg2ServerCheck, String> {
        let mut resp = self
            .http
            .authorized_get("/props", PROPS_TIMEOUT)
            .send()
            .await
            .map_err(|e| format!("unreachable: {}", e.without_url()))?;
        let status = resp.status();
        if status.is_server_error() {
            return Err(format!("it answered {status}"));
        }
        if !status.is_success() {
            return Ok(Eg2ServerCheck::NotExposed);
        }
        let mut body = Vec::new();
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    if body.len() + chunk.len() > PROPS_READ_CAP {
                        return Ok(Eg2ServerCheck::NotExposed);
                    }
                    body.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(e) => return Err(format!("the body was cut off: {}", e.without_url())),
            }
        }
        let Ok(props) = serde_json::from_slice::<Props>(&body) else {
            return Ok(Eg2ServerCheck::NotExposed);
        };
        let Some(model_path) = props.model_path else {
            return Ok(Eg2ServerCheck::NotExposed);
        };
        self.state().build = props.build_info.map(|b| {
            b.chars()
                .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
                .take(64)
                .collect()
        });
        Ok(judge_props(
            &self.model,
            &self.http.log_base_url(),
            &model_path,
            props.model_ftype.as_deref(),
            props.modalities.and_then(|m| m.vision),
        ))
    }

    /// Refuse before sending when the check has found the server wrong, or
    /// could not re-check a server it verified earlier.
    async fn ensure_server(&self, image: bool) -> Result<Eg2ServerCheck, EmbedError> {
        let outcome = self.check(image).await;
        match &outcome {
            Eg2ServerCheck::Mismatch(message) => Err(EmbedError::Backend(message.clone())),
            Eg2ServerCheck::Verified {
                vision: Some(false),
            } if image => Err(EmbedError::Backend(self.no_vision_message())),
            Eg2ServerCheck::Skipped if self.server_check && self.state().verified_once => {
                Err(EmbedError::Unavailable(format!(
                    "could not re-check the llama-server at {} over /props (unreachable or a \
                     server error) after verifying it earlier; holding embeds until it answers, \
                     so a server restarted with another model cannot write vectors under this \
                     contract",
                    self.http.log_base_url()
                )))
            }
            _ => Ok(outcome),
        }
    }

    /// POST to the embeddings endpoint; a failure that looks like a restart
    /// ([`restart_seen`]) drops the kept `/props` answer
    /// ([`Self::forget_kept`]).
    async fn post<B: Serialize>(
        &self,
        body: &B,
        rule: super::bge_m3::StatusRule,
    ) -> Result<EmbedResponse, EmbedError> {
        let result = self.http.post_json(body, &self.model, rule).await;
        if let Err(err) = &result
            && restart_seen(err)
        {
            self.forget_kept();
        }
        result
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
        let parsed = self.post(&body, default_status_rule).await?;
        truncate_and_normalize(first_embedding(parsed.data)?, self.dim)
    }
}

/// Whether a failed embed request looks like a llama-server restart, which
/// drops the kept `/props` answer and image budget check: no HTTP answer at
/// all (refused, reset), or llama-server's 503 "Loading model" (a server
/// that just started). A 503 "busy" or another transient status comes from a
/// server that is still the one checked, and dropping the checks there would
/// add a `/props` GET and a reference image embed to every retried image on
/// an already loaded server; the [`EG2_PROPS_RECHECK_INTERVAL`] expiry still
/// bounds how long such a server is trusted.
fn restart_seen(err: &EmbedError) -> bool {
    matches!(err, EmbedError::Unavailable(msg)
        if msg.starts_with(LLAMA_UNREACHABLE) || msg.contains("Loading model"))
}

/// One input item whose content is the image alone, as a base64 data URI.
fn image_request<'a>(model: &'a str, mime: &str, base64: &str) -> ImageRequest<'a> {
    ImageRequest {
        model,
        input: [ImageItem {
            content: [ImagePart {
                kind: "image_url",
                image_url: ImageUrl {
                    url: format!("data:{mime};base64,{base64}"),
                },
            }],
        }],
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
        if let Eg2ServerCheck::Verified { .. } = self.ensure_server(true).await? {
            self.ensure_image_budget().await?;
        }
        let encoded = base64::engine::general_purpose::STANDARD.encode(image.bytes());
        let body = image_request(&self.model, image.mime().as_str(), &encoded);
        // The image's own token count is not judged: it varies with the
        // image's size and shape (see EG2_REFERENCE_IMAGE_TOKENS).
        let parsed = self.post(&body, image_status_rule).await?;
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
