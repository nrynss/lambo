//! Embeddings through the Elasticsearch Inference API (#233).
//!
//! Speaks `POST {url}/_inference/text_embedding/{inference_id}` with
//! `{"input": ["<text>"], "input_type": "INGEST" | "SEARCH"}` and reads
//! `{"text_embedding": [{"embedding": [...]}]}`. The inference endpoint id is
//! `[embedder] model`; which ids exist depends on the deployment
//! (`GET /_inference/_all` lists them, with their `task_type`). Only a
//! `text_embedding` endpoint with float elements can serve this kind: ELSER is
//! `sparse_embedding`, the multimodal endpoints are `embedding`, and a
//! byte/bit endpoint answers `text_embedding_bytes` / `text_embedding_bits`.
//!
//! **Roles.** [`Embedder::embed`] (the document role) sends `input_type:
//! INGEST` and [`Embedder::embed_query`] sends `SEARCH`, so an asymmetric
//! model embeds each in its own role. Not every Elastic service accepts the
//! field; `[embedder] send_input_type = false` omits it for both roles, and
//! the contract model then gains `;input_type=off` because the space is a
//! different one ([`ElasticEmbedder::model_identity`]).
//!
//! **Transport (shared with `bge_m3`, #21).** The client never follows a
//! redirect, and plain `http` to a loopback host ignores the proxy
//! environment ([`bge_m3::build_client`]). The URL follows `[recall]`'s rules:
//! http or https, no userinfo, no query, no fragment. With `api_key_env` the
//! adapter sends `Authorization: ApiKey <key>` (as the recall tier does), only
//! over https or http to a loopback host. A non-2xx body is read up to
//! [`scrub::read_cap`] and quoted only after every detectable echo of the key
//! is replaced ([`scrub::quotable_body`]). A 2xx body is read in chunks up to
//! [`bge_m3::success_body_cap`] (64 bytes a component, at least 4 MiB).
//!
//! **Errors.** A transport failure is [`EmbedError::Unavailable`], including
//! one that cuts a 2xx body off mid-read (a reset, or a stall past the
//! request timeout, which covers the body). A non-2xx
//! status is classified by the J3 table ([`bge_m3::classify_status`]) with two
//! Elastic-specific refinements for a `400`, read from the reasons of the
//! Elastic error JSON only (never the rest of the body, which can echo the
//! input): a reason saying `Incompatible task_type` (the endpoint is not a
//! text_embedding endpoint) and a reason naming `input_type` (the service
//! refuses the field) are permanent configuration errors with a hint. A 2xx
//! body over the cap (never quoted), a bad body, the wrong width, a
//! non-finite or zero-norm vector are [`EmbedError::Backend`].
//!
//! **CON-2:** there is no retry that changes the request. In particular a
//! refused `input_type` is never retried without it, since that would embed
//! in a different space under the same contract.
//!
//! **CON-7:** empty or whitespace-only text is refused with
//! [`EmbedError::Unavailable`] before any network.

use async_trait::async_trait;
use reqwest::header::{HeaderValue, AUTHORIZATION};
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};

use super::bge_m3::{self, scrub, EmbedStatusClass, StatusVerdict, SuccessBodyError};
use super::{api_key, EmbedError, Embedder, EmbedderConfig};

/// `input_type` for the document role ([`Embedder::embed`]).
pub(crate) const INPUT_TYPE_DOCUMENT: &str = "INGEST";
/// `input_type` for the query role ([`Embedder::embed_query`]).
pub(crate) const INPUT_TYPE_QUERY: &str = "SEARCH";
/// Appended to the contract model when `send_input_type = false`.
pub(crate) const INPUT_TYPE_OFF_SUFFIX: &str = ";input_type=off";

/// Appended to the error for a `400` whose error reason names `input_type`.
const INPUT_TYPE_HINT: &str =
    " (this inference endpoint refused the input_type field Lambo sends: \
    INGEST for documents, SEARCH for queries; not every Elastic service accepts it. Set [embedder] \
    send_input_type = false to omit it. That changes the embedding contract's model (it gains \
    ;input_type=off), so a session embedded with input_type must be re-embedded)";

/// Appended to the error for a `400` whose error reason says `Incompatible
/// task_type`.
const TASK_TYPE_HINT: &str = " (this inference endpoint is not a text_embedding endpoint: ELSER \
    endpoints such as .elser-2-elastic are sparse_embedding, and multimodal endpoints such as \
    .jina-clip-v2 are task type embedding; neither can serve kind elastic. Point [embedder] model \
    at a text_embedding endpoint; GET /_inference/_all lists each endpoint with its task_type)";

/// The request body. `input` is always one text.
#[derive(Debug, Serialize)]
struct InferenceRequest<'a> {
    input: [&'a str; 1],
    #[serde(skip_serializing_if = "Option::is_none")]
    input_type: Option<&'static str>,
}

/// The success body. The byte and bit forms are read only to say why they
/// cannot be used.
#[derive(Debug, Deserialize)]
struct InferenceResponse {
    #[serde(default)]
    text_embedding: Option<Vec<EmbeddingEntry>>,
    #[serde(default)]
    text_embedding_bytes: Option<IgnoredAny>,
    #[serde(default)]
    text_embedding_bits: Option<IgnoredAny>,
}

#[derive(Debug, Deserialize)]
struct EmbeddingEntry {
    embedding: Vec<f32>,
}

/// The wording Elastic uses when a `text_embedding` request reaches an
/// endpoint of another task type (seen live on Elastic Cloud Serverless,
/// 2026-11-09, for `embedding` and `sparse_embedding` endpoints): `Incompatible
/// task_type, the requested type [text_embedding] does not match the model
/// type [...]`. Matched lowercased.
const TASK_TYPE_REASON: &str = "incompatible task_type";

/// The reason strings of an Elastic error body, lowercased: `error.reason`
/// and every `error.root_cause[].reason`. A body that is not JSON, has no
/// `error` object, or carries a reason that is not a string contributes
/// nothing for that part. Only these strings are matched by
/// [`elastic_status_rule`], never the rest of the body, which can echo the
/// input text.
fn error_reasons(body: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let Some(error) = value.get("error") else {
        return Vec::new();
    };
    let root_causes = error
        .get("root_cause")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten();
    std::iter::once(error)
        .chain(root_causes)
        .filter_map(|e| e.get("reason").and_then(serde_json::Value::as_str))
        .map(str::to_ascii_lowercase)
        .collect()
}

/// The status rule for an Elastic inference response: the J3 table, except
/// for two `400`s, which are permanent configuration errors with a hint. Only
/// the reasons of the Elastic error JSON are read ([`error_reasons`]):
///
/// - a reason containing `Incompatible task_type` ([`TASK_TYPE_REASON`]):
///   the endpoint is not a text_embedding endpoint. Checked first, since a
///   wrong endpoint is wrong whatever the request said.
/// - when the request carried `input_type`, a reason naming `input_type` or
///   `input type` (case-insensitive): the service refuses the field.
///
/// Any other `400` keeps the J3 class with no hint.
pub(crate) fn elastic_status_rule(code: u16, body: &str, sent_input_type: bool) -> StatusVerdict {
    if code == 400 {
        let reasons = error_reasons(body);
        if reasons.iter().any(|r| r.contains(TASK_TYPE_REASON)) {
            return StatusVerdict {
                class: EmbedStatusClass::PermanentConfig,
                hint: Some(TASK_TYPE_HINT),
            };
        }
        if sent_input_type
            && reasons
                .iter()
                .any(|r| r.contains("input_type") || r.contains("input type"))
        {
            return StatusVerdict {
                class: EmbedStatusClass::PermanentConfig,
                hint: Some(INPUT_TYPE_HINT),
            };
        }
    }
    StatusVerdict {
        class: bge_m3::classify_status(code),
        hint: None,
    }
}

/// Embeddings from one Elasticsearch inference endpoint.
///
/// `Debug` is written by hand so the API key can never be printed.
#[derive(Clone)]
pub struct ElasticEmbedder {
    client: reqwest::Client,
    /// The full endpoint URL, `{base}/_inference/text_embedding/{id}`.
    url: reqwest::Url,
    /// [`Self::url`] as logs, errors and `Debug` print it: scheme, host,
    /// port and path ([`bge_m3::url_for_log`]).
    log_url: String,
    /// The inference endpoint id (`[embedder] model`).
    inference_id: String,
    /// Expected width; a response of any other width is refused.
    dim: usize,
    /// Whether `input_type` is sent (`[embedder] send_input_type`, default on).
    send_input_type: bool,
    /// `Authorization: ApiKey <key>`, marked sensitive. `None` sends no
    /// `Authorization` header.
    authorization: Option<HeaderValue>,
}

impl std::fmt::Debug for ElasticEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ElasticEmbedder")
            .field("url", &self.log_url)
            .field("inference_id", &self.inference_id)
            .field("dim", &self.dim)
            .field("send_input_type", &self.send_input_type)
            .field(
                "authorization",
                &self
                    .authorization
                    .as_ref()
                    .map(|_| "ApiKey (value not shown)"),
            )
            .finish_non_exhaustive()
    }
}

/// Build the adapter from a resolved `[embedder]` table (kind `elastic`).
/// `url` and `model` are required; `api_key_env` is optional and read here,
/// at resolve, after the transport check, so a plaintext remote URL is
/// refused before the variable is even read.
pub(crate) fn build(cfg: &EmbedderConfig) -> Result<ElasticEmbedder, EmbedError> {
    let url = cfg
        .llama_url
        .as_deref()
        .filter(|u| !u.trim().is_empty())
        .ok_or_else(|| {
            EmbedError::Unavailable(
                "embedder kind `elastic` needs [embedder] url: the Elasticsearch base URL, e.g. \
                 https://my-cluster.es.example.com (LAMBO_LLAMA_EMBED_URL overrides it)"
                    .into(),
            )
        })?;
    let id = cfg
        .llama_model
        .as_deref()
        .filter(|m| !m.trim().is_empty())
        .ok_or_else(|| {
            EmbedError::Unavailable(
                "embedder kind `elastic` needs [embedder] model: the Elasticsearch inference \
                 endpoint id, e.g. .jina-embeddings-v3 (GET /_inference/_all lists the ids \
                 this deployment exposes; LAMBO_LLAMA_MODEL overrides it)"
                    .into(),
            )
        })?;
    let mut embedder = ElasticEmbedder::new(url, id, cfg.dim)?
        .with_send_input_type(cfg.send_input_type.unwrap_or(true));
    if let Some(name) = cfg.api_key_env.as_deref() {
        embedder.check_api_key_transport()?;
        embedder = embedder.with_api_key(&api_key::resolve(name)?)?;
    }
    Ok(embedder)
}

impl ElasticEmbedder {
    /// An adapter for inference endpoint `inference_id` on the cluster at
    /// `base_url`. No network I/O. `base_url` may carry a path prefix (a
    /// proxy); the endpoint path is appended to it, with the id
    /// percent-encoded as one path segment.
    pub fn new(base_url: &str, inference_id: &str, dim: usize) -> Result<Self, EmbedError> {
        if dim == 0 {
            return Err(EmbedError::Unavailable("embedder dim must be > 0".into()));
        }
        let base = parse_base_url(base_url)?;
        let id = validate_inference_id(inference_id)?;
        let mut url = base.clone();
        url.path_segments_mut()
            .map_err(|()| {
                EmbedError::Unavailable("embedder.url must be an http:// or https:// URL".into())
            })?
            .pop_if_empty()
            .extend(["_inference", "text_embedding", id]);
        Ok(Self {
            client: bge_m3::build_client(
                base.as_str(),
                bge_m3::DEFAULT_CONNECT_TIMEOUT,
                bge_m3::DEFAULT_REQUEST_TIMEOUT,
            )?,
            log_url: bge_m3::url_for_log(url.as_str()),
            url,
            inference_id: id.to_string(),
            dim,
            send_input_type: true,
            authorization: None,
        })
    }

    /// Send `input_type` (`true`, the default) or omit it for both roles.
    pub fn with_send_input_type(mut self, send: bool) -> Self {
        self.send_input_type = send;
        self
    }

    /// May an API key be sent to this endpoint? Only over `https`, or over
    /// plain `http` to a loopback host, where it never leaves the machine.
    /// The refusal names the host and scheme, never the URL or the key.
    pub(crate) fn check_api_key_transport(&self) -> Result<(), EmbedError> {
        if crate::net::may_carry_credential(&self.url) {
            return Ok(());
        }
        Err(EmbedError::Unavailable(format!(
            "refusing to send the Elasticsearch API key to {} over {}: an ApiKey is sent only \
             over https, or over http to a loopback host (localhost, 127.0.0.0/8, ::1). Use an \
             https URL for this endpoint, or remove api_key_env",
            self.url.host_str().unwrap_or("(no host)"),
            self.url.scheme()
        )))
    }

    /// Send `Authorization: ApiKey <key>` on every request. Refused over
    /// plain `http` to a non-loopback host. The header is marked sensitive;
    /// `Debug` shows only that a key is set, and a key that is not a valid
    /// header value is refused without quoting it.
    pub fn with_api_key(mut self, key: &str) -> Result<Self, EmbedError> {
        self.check_api_key_transport()?;
        let mut value = HeaderValue::from_str(&format!("ApiKey {key}")).map_err(|_| {
            EmbedError::Unavailable(
                "the Elasticsearch API key is not a valid HTTP header value (value not shown)"
                    .into(),
            )
        })?;
        value.set_sensitive(true);
        self.authorization = Some(value);
        Ok(self)
    }

    /// The contract model this adapter stamps: the inference endpoint id, or
    /// `"{id};input_type=off"` when `input_type` is not sent, because
    /// dropping the role changes an asymmetric model's space.
    pub fn model_identity(&self) -> String {
        if self.send_input_type {
            self.inference_id.clone()
        } else {
            format!("{}{INPUT_TYPE_OFF_SUFFIX}", self.inference_id)
        }
    }

    /// The API key this adapter sends, if any.
    fn api_key(&self) -> Option<&str> {
        self.authorization
            .as_ref()
            .and_then(|auth| auth.as_bytes().strip_prefix(b"ApiKey "))
            .and_then(|key| std::str::from_utf8(key).ok())
    }

    /// At most [`scrub::read_cap`] bytes of an error body, and whether more
    /// was left unread; the rest is never downloaded.
    async fn capped_error_body(&self, mut resp: reqwest::Response) -> (String, bool) {
        let cap = scrub::read_cap(self.api_key().map_or(0, str::len));
        let mut body = Vec::new();
        let mut truncated = false;
        while let Ok(Some(chunk)) = resp.chunk().await {
            let room = cap - body.len();
            if chunk.len() > room {
                body.extend_from_slice(&chunk[..room]);
                truncated = true;
                break;
            }
            body.extend_from_slice(&chunk);
        }
        (String::from_utf8_lossy(&body).into_owned(), truncated)
    }

    /// Embed `text` in the role `input_type` names.
    async fn infer(&self, text: &str, input_type: &'static str) -> Result<Vec<f32>, EmbedError> {
        if text.trim().is_empty() {
            return Err(EmbedError::Unavailable(
                "cannot embed empty/whitespace text".into(),
            ));
        }
        let body = InferenceRequest {
            input: [text],
            input_type: self.send_input_type.then_some(input_type),
        };
        let mut req = self.client.post(self.url.clone()).json(&body);
        if let Some(auth) = &self.authorization {
            req = req.header(AUTHORIZATION, auth.clone());
        }
        let resp = req.send().await.map_err(|e| {
            EmbedError::Unavailable(format!(
                "Elasticsearch unreachable at {}: {}",
                self.log_url,
                e.without_url()
            ))
        })?;
        let status = resp.status();
        if status.is_success() {
            let cap = bge_m3::success_body_cap(self.dim);
            let bytes = bge_m3::read_success_body(resp, cap)
                .await
                .map_err(|e| match e {
                    // A transport failure mid-body says nothing about the
                    // input: transient, so the write keeps its durable intent.
                    SuccessBodyError::Cut(e) => EmbedError::Unavailable(format!(
                        "Elasticsearch response body from {} was cut off: {}",
                        self.log_url,
                        e.without_url()
                    )),
                    SuccessBodyError::TooLarge => EmbedError::Backend(format!(
                        "Elasticsearch response from {} is over {cap} bytes, more than an \
                         embedding of {} components needs; refusing it (body not shown)",
                        self.log_url, self.dim
                    )),
                })?;
            // The parse error's own text quotes body content, which an
            // echoing endpoint could fill with the key: name only its class
            // and position.
            let parsed: InferenceResponse = serde_json::from_slice(&bytes).map_err(|e| {
                EmbedError::Backend(format!(
                    "Elasticsearch returned unparseable JSON ({:?} error at line {}, column {}; \
                     body not shown)",
                    e.classify(),
                    e.line(),
                    e.column()
                ))
            })?;
            return self.vector_from(parsed);
        }
        Err(self.status_error(status, resp).await)
    }

    /// The one usable vector in a success body, checked and normalized.
    fn vector_from(&self, parsed: InferenceResponse) -> Result<Vec<f32>, EmbedError> {
        let id = &self.inference_id;
        let Some(entries) = parsed.text_embedding else {
            if parsed.text_embedding_bytes.is_some() || parsed.text_embedding_bits.is_some() {
                return Err(EmbedError::Backend(format!(
                    "Elasticsearch inference endpoint {id:?} returns byte or bit embeddings \
                     (text_embedding_bytes / text_embedding_bits); Lambo needs float \
                     embeddings: use an endpoint whose element_type is float"
                )));
            }
            return Err(EmbedError::Backend(format!(
                "Elasticsearch inference endpoint {id:?} answered without a text_embedding \
                 field; is it a text_embedding endpoint?"
            )));
        };
        let count = entries.len();
        let mut entries = entries.into_iter();
        let (Some(entry), None) = (entries.next(), entries.next()) else {
            return Err(EmbedError::Backend(format!(
                "Elasticsearch inference endpoint {id:?} returned {count} text_embedding \
                 entries for one input, expected exactly 1"
            )));
        };
        let mut vec = entry.embedding;
        if vec.len() != self.dim {
            return Err(EmbedError::Backend(format!(
                "Elasticsearch returned {} dims, expected {} (inference endpoint {id:?}; set \
                 [embedder] dim to the endpoint's width, and the store's vector width with it)",
                vec.len(),
                self.dim
            )));
        }
        l2_normalize_in_place(&mut vec)?;
        Ok(vec)
    }

    /// The error for a non-success status, classified and logged as
    /// `bge_m3`'s `post_json` does.
    async fn status_error(
        &self,
        status: reqwest::StatusCode,
        resp: reqwest::Response,
    ) -> EmbedError {
        let id = &self.inference_id;
        let code = status.as_u16();
        let text_body = if status.is_redirection() {
            // Neither the body nor `Location` is quoted: a redirect target
            // can carry a signed URL or a key in its query.
            "(redirect not followed; the redirect target is not shown. Point the embedder URL \
             at the cluster itself)"
                .to_string()
        } else {
            let (body, truncated) = self.capped_error_body(resp).await;
            scrub::quotable_body(&body, self.api_key(), truncated)
        };
        let verdict = elastic_status_rule(code, &text_body, self.send_input_type);
        let hint = verdict.hint.unwrap_or("");
        match verdict.class {
            EmbedStatusClass::Transient => EmbedError::Unavailable(format!(
                "Elasticsearch is momentarily unwilling ({status}) for inference endpoint \
                 {id:?}: {text_body}{hint}"
            )),
            EmbedStatusClass::Unclassified => {
                tracing::warn!(
                    url = %self.log_url,
                    inference_id = %id,
                    status = %status,
                    "Elasticsearch answered with status {status}, which the J3-R2R-1 rule table \
                     does not name; treating it as transient so the write stays durable"
                );
                EmbedError::Unavailable(format!(
                    "Elasticsearch answered {status} (unclassified by the J3 status rule table) \
                     for inference endpoint {id:?}: {text_body}{hint}"
                ))
            }
            EmbedStatusClass::Content => {
                tracing::error!(
                    url = %self.log_url,
                    inference_id = %id,
                    status = %status,
                    "Elasticsearch refused this content ({status}); not retrying (CON-2)"
                );
                EmbedError::Backend(format!(
                    "Elasticsearch refused this content with {status} for inference endpoint \
                     {id:?}: {text_body}{hint}"
                ))
            }
            EmbedStatusClass::PermanentConfig => {
                tracing::error!(
                    url = %self.log_url,
                    inference_id = %id,
                    status = %status,
                    "Elasticsearch answered {status}: a PERMANENT configuration error (URL, \
                     inference endpoint id, API key or endpoint task type). An operator must fix \
                     the embedder; every such write is being failed because retrying cannot help"
                );
                EmbedError::Backend(format!(
                    "Elasticsearch answered {status} (permanent configuration error) for \
                     inference endpoint {id:?}: {text_body}{hint}"
                ))
            }
        }
    }
}

/// Parse and check the base URL with `[recall]`'s rules. No refusal quotes
/// the URL's userinfo or query.
fn parse_base_url(raw: &str) -> Result<reqwest::Url, EmbedError> {
    let base = reqwest::Url::parse(raw.trim()).map_err(|e| {
        EmbedError::Unavailable(format!(
            "embedder.url is not a valid URL ({e}; value not shown)"
        ))
    })?;
    if !matches!(base.scheme(), "http" | "https") || base.cannot_be_a_base() {
        return Err(EmbedError::Unavailable(
            "embedder.url must be an http:// or https:// URL for kind `elastic`".into(),
        ));
    }
    if !base.username().is_empty() || base.password().is_some() {
        return Err(EmbedError::Unavailable(
            "embedder.url carries credentials (not shown); secrets are by reference only: put \
             the Elasticsearch API key in an environment variable and name it with \
             embedder.api_key_env"
                .into(),
        ));
    }
    if base.query().is_some() || base.fragment().is_some() {
        return Err(EmbedError::Unavailable(format!(
            "embedder.url for {} carries a query string or fragment (not shown): it would be \
             sent on every request and echoed in errors; give the cluster's base URL only, and \
             the API key through embedder.api_key_env",
            bge_m3::url_for_log(base.as_str())
        )));
    }
    Ok(base)
}

/// Check the inference endpoint id: non-empty, no whitespace, not `.` or
/// `..`, no `/`. A refused id is not quoted (it may be a pasted secret).
fn validate_inference_id(raw: &str) -> Result<&str, EmbedError> {
    let id = raw.trim();
    if id.is_empty() {
        return Err(EmbedError::Unavailable(
            "embedder.model (the Elasticsearch inference endpoint id) is empty".into(),
        ));
    }
    if id.chars().any(char::is_whitespace) || id.contains('/') || id == "." || id == ".." {
        return Err(EmbedError::Unavailable(
            "embedder.model is not an Elasticsearch inference endpoint id (value not shown): an \
             id has no whitespace or '/' and is not '.' or '..'; GET /_inference/_all lists the \
             ids this deployment exposes"
                .into(),
        ));
    }
    Ok(id)
}

/// L2-normalize in place, refusing a non-finite or zero-norm vector.
fn l2_normalize_in_place(v: &mut [f32]) -> Result<(), EmbedError> {
    let mut sum = 0.0f32;
    for &x in v.iter() {
        if !x.is_finite() {
            return Err(EmbedError::Backend(
                "Elasticsearch returned a non-finite embedding".into(),
            ));
        }
        sum += x * x;
    }
    let norm = sum.sqrt();
    if !norm.is_finite() || norm <= f32::EPSILON {
        return Err(EmbedError::Backend(if norm.is_finite() {
            "Elasticsearch returned a zero-norm embedding".into()
        } else {
            "Elasticsearch returned a non-finite embedding".into()
        }));
    }
    for x in v.iter_mut() {
        *x /= norm;
    }
    Ok(())
}

#[async_trait]
impl Embedder for ElasticEmbedder {
    fn dimensions(&self) -> usize {
        self.dim
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        self.infer(text, INPUT_TYPE_DOCUMENT).await
    }

    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        self.infer(text, INPUT_TYPE_QUERY).await
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
#[path = "elastic_tests.rs"]
mod tests;
