//! BGE-M3 embeddings served by a local llama.cpp server (default production path).
//!
//! Speaks llama.cpp's OpenAI-compatible `POST /v1/embeddings` endpoint, which is the
//! most version-stable surface across llama.cpp releases. Returns dense vectors that are
//! L2-normalized before returning so Cockroach `<->` (L2) rankings stay coherent with
//! cosine similarity (see `notes/embeddings-portable.md`).
//!
//! This backend is selected when `LAMBO_EMBEDDER=bge_m3` (the default).
//!
//! Nothing here is llama.cpp-specific except [`BgeM3LlamaCppEmbedder::check_health`]:
//! with an optional bearer token ([`BgeM3LlamaCppEmbedder::with_bearer_token`],
//! configured as `[embedder] api_key_env`) the same adapter reaches hosted
//! OpenAI-compatible endpoints such as Cloudflare Workers AI (issue #21).

use async_trait::async_trait;
use reqwest::header::{HeaderValue, AUTHORIZATION};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use super::{EmbedError, Embedder};

mod scrub;

/// OpenAI-compatible embeddings request body (`model` is omitted when empty so it hits
/// a llama.cpp server's default model).
#[derive(Debug, Serialize)]
struct EmbedRequest {
    #[serde(skip_serializing_if = "String::is_empty")]
    model: String,
    input: String,
}

#[derive(Debug, Deserialize)]
struct EmbedResponse {
    data: Vec<EmbedData>,
}

#[derive(Debug, Deserialize)]
struct EmbedData {
    embedding: Vec<f32>,
}

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(5);

/// What a non-success HTTP status from llama.cpp says about retrying the same
/// input (J3-R2R-1). The durability replay's consume-or-keep decision turns on
/// whether the status speaks about *this input*, about *the deployment*, or
/// about *the server being momentarily unwilling*; HTTP collapses those into
/// three buckets plus "no rule". Pāṇini-ordered and exhaustive, with no
/// wildcard that lands in `Backend`/content: an unrecognised status is a gap in
/// OUR table, not a statement about the caller's input — and this branch has
/// already paid twice (J3-R3-1, J3-R2R-1) for treating absence of knowledge as
/// knowledge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EmbedStatusClass {
    /// The server was momentarily unwilling — retry later; leave the write durable.
    Transient,
    /// This input will never embed — settle it as `failed`, exactly as a refusal.
    Content,
    /// The deployment is misconfigured — fail the write AND warn loudly.
    PermanentConfig,
    /// No rule names this status — treat as transient and log the status it was.
    Unclassified,
}

/// Classify a non-success HTTP status. Named statuses first; then every un-named
/// 5xx is transient; everything else is [`EmbedStatusClass::Unclassified`]. The
/// class is decided HERE (the site that knows the status) and carried out as an
/// [`EmbedError`] variant, never re-derived from a message string upstream
/// (J1-R2-2: a class a decision turns on must be a type).
pub(crate) fn classify_status(code: u16) -> EmbedStatusClass {
    match code {
        // A statement about this input — consume as failed.
        400 | 413 | 415 | 422 => EmbedStatusClass::Content,
        // A statement about the deployment — an operator must act.
        401 | 403 | 404 => EmbedStatusClass::PermanentConfig,
        // A redirect: the client never follows one (issue #21, see
        // `build_client`), so the configured URL is not the endpoint. Only an
        // operator can fix that, exactly like a 404.
        300..=399 => EmbedStatusClass::PermanentConfig,
        // The server was momentarily unwilling for reasons that do not
        // mention the input (`500` is a loaded llama.cpp fast-failing a burst;
        // `503` is "no slot available" / loading the model).
        408 | 425 | 429 | 500 | 502 | 503 | 504 | 509 | 529 => EmbedStatusClass::Transient,
        // Any 5xx the table does not name is still a server-side signal.
        code if (500..=599).contains(&code) => EmbedStatusClass::Transient,
        // Everything else: no rule, so conservatively transient AND logged.
        _ => EmbedStatusClass::Unclassified,
    }
}

/// What a status rule decided about one non-success response (#22 PR 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StatusVerdict {
    pub(crate) class: EmbedStatusClass,
    /// Operator guidance appended to the error, for a response whose body
    /// names a known deployment or content fault.
    pub(crate) hint: Option<&'static str>,
}

/// A status rule: the status code and the (scrubbed, capped) error body in,
/// the class out. The class is still decided at the site that read the
/// response and carried out as an [`EmbedError`] variant (J1-R2-2); a rule
/// only lets an adapter that knows its server's bodies refine the table for
/// a request shape the table was not written for (the EmbeddingGemma 2
/// image call, whose `500` can be a permanent deployment fault).
/// How [`BgeM3LlamaCppEmbedder::post_json`] begins the message of a request that never
/// got an HTTP answer (refused, reset, timed out). A caller that must tell a
/// connection-level failure from a transient HTTP status (a 503 "busy")
/// matches on it.
pub(crate) const LLAMA_UNREACHABLE: &str = "llama.cpp unreachable at ";

pub(crate) type StatusRule = fn(u16, &str) -> StatusVerdict;

/// The body `llama-server` (checked on b11517) sends with `500` for a
/// non-causal input (an embedding) longer than its physical batch:
/// `input (N tokens) is too large to process. increase the physical batch
/// size (current batch size: M)`.
const UBATCH_TOO_SMALL: &str = "too large to process. increase the physical batch size";

/// Appended to the error for an input longer than the server's ubatch.
const UBATCH_HINT: &str = " (this input is longer than the llama-server's physical batch, which \
    an embedding must fit in whole: raise it, e.g. --batch-size 8192 --ubatch-size 8192, to embed \
    inputs this long)";

/// The J3 table, as a [`StatusRule`], with one body-named exception: a `500`
/// whose body says the input is too large for the physical batch is a fact
/// about this input on this deployment, not a busy server, so it is
/// `Content` (settled as failed, with a `--ubatch-size` hint) instead of
/// transient. Retrying it can never succeed, and before this rule it was
/// retried forever. Every other body is ignored.
pub(crate) fn default_status_rule(code: u16, body: &str) -> StatusVerdict {
    if code == 500 && body.contains(UBATCH_TOO_SMALL) {
        return StatusVerdict {
            class: EmbedStatusClass::Content,
            hint: Some(UBATCH_HINT),
        };
    }
    StatusVerdict {
        class: classify_status(code),
        hint: None,
    }
}

/// BGE-M3 embeddings via a local llama.cpp server over HTTP.
///
/// `Debug` is written by hand so the bearer token can never be printed.
#[derive(Clone)]
pub struct BgeM3LlamaCppEmbedder {
    client: reqwest::Client,
    /// Full embed endpoint URL, e.g. `http://127.0.0.1:8080/v1/embeddings`.
    url: String,
    /// Base URL for `/health`, e.g. `http://127.0.0.1:8080`.
    base_url: String,
    /// [`Self::url`] as printed in logs, errors and `Debug`: scheme, host,
    /// port and path only, never userinfo or a query ([`url_for_log`]).
    log_url: String,
    /// Model id sent in the request (empty => server default).
    model: String,
    /// Expected embedding dimensionality (must match server output and store schema).
    dim: usize,
    /// `Authorization: Bearer <token>` for a hosted endpoint, marked sensitive.
    /// `None` sends no `Authorization` header at all (issue #21).
    authorization: Option<HeaderValue>,
}

impl std::fmt::Debug for BgeM3LlamaCppEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BgeM3LlamaCppEmbedder")
            .field("url", &self.log_url)
            .field("model", &self.model)
            .field("dim", &self.dim)
            .field(
                "authorization",
                &self
                    .authorization
                    .as_ref()
                    .map(|_| "Bearer (value not shown)"),
            )
            .finish_non_exhaustive()
    }
}

/// May a bearer token be sent to `base_url`? Only over `https`, or over plain
/// `http` to a loopback host (`localhost`, `127.0.0.0/8`, `::1`), where the
/// token never leaves the machine (a local server). Anything else would put
/// the token on the wire in clear text, so it is refused before the token is
/// read or sent (issue #21). The refusal names the endpoint's host and scheme,
/// never the full URL (it may carry userinfo) and never the token.
pub(crate) fn check_bearer_transport(base_url: &str) -> Result<(), EmbedError> {
    let url = reqwest::Url::parse(base_url).map_err(|_| {
        EmbedError::Unavailable(
            "the embedder URL is not a valid URL (value not shown); a bearer token is sent only \
             over https, or over http to a loopback host"
                .into(),
        )
    })?;
    if url.scheme() == "https" {
        return Ok(());
    }
    if url.scheme() == "http" && is_loopback_host(&url) {
        return Ok(());
    }
    Err(EmbedError::Unavailable(format!(
        "refusing to send the embedder API token to {} over {}: a bearer token is sent only over \
         https, or over http to a loopback host (localhost, 127.0.0.0/8, ::1). Use an https \
         URL for this endpoint, or remove api_key_env",
        url.host_str().unwrap_or("(no host)"),
        url.scheme()
    )))
}

/// Is `url`'s host loopback: `localhost`, `127.0.0.0/8`, `::1` or an
/// IPv4-mapped `127.0.0.0/8`? `url` has already normalised the host: IPv4 to
/// a dotted quad (`127.1`, `2130706433` and `0x7f.1` are `127.0.0.1`), IPv6
/// in brackets, domains lower-cased. The name `localhost` is trusted without
/// resolving it.
fn is_loopback_host(url: &reqwest::Url) -> bool {
    url.host_str().is_some_and(|host| {
        match host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
        {
            Ok(std::net::IpAddr::V4(a)) => a.is_loopback(),
            Ok(std::net::IpAddr::V6(a)) => {
                a.is_loopback() || a.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
            }
            Err(_) => host == "localhost" || host == "localhost.",
        }
    })
}

/// Should the client for `base_url` ignore the `HTTP_PROXY` / `HTTPS_PROXY` /
/// `ALL_PROXY` environment? Yes for plain `http` to a loopback host: a proxy
/// would carry the request, and any bearer token on it, off the machine in
/// clear text, which is exactly what [`check_bearer_transport`] allows
/// loopback http on the promise of never doing (issue #21). `https` keeps the
/// environment's proxies: the proxy only tunnels (`CONNECT`), TLS runs end to
/// end, and a hosted endpoint behind a corporate egress proxy is reachable
/// only through it. Plain `http` to any other host never carries a token
/// (refused) and keeps its old behaviour.
fn bypasses_env_proxy(base_url: &str) -> bool {
    reqwest::Url::parse(base_url).is_ok_and(|url| url.scheme() == "http" && is_loopback_host(&url))
}

/// `url` for a log line, an error or `Debug`: scheme, host, port and path.
/// Userinfo and the query are dropped, since either may carry a credential
/// (issue #21); an unparseable URL is not shown at all.
fn url_for_log(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(u) => {
            let host = u.host_str().unwrap_or("");
            match u.port() {
                Some(port) => format!("{}://{host}:{port}{}", u.scheme(), u.path()),
                None => format!("{}://{host}{}", u.scheme(), u.path()),
            }
        }
        Err(_) => "(unparseable URL, not shown)".to_string(),
    }
}

/// The HTTP client for `base_url`.
///
/// Redirects are never followed (`Policy::none`): an embeddings POST has no
/// legitimate redirect, and reqwest keeps `Authorization` on a same-host,
/// same-port redirect even when it downgrades `https` to `http`, so following
/// one could resend the token in clear text (issue #21). A 3xx is answered
/// as a permanent configuration error instead (see [`classify_status`]).
fn build_client(
    base_url: &str,
    connect: Duration,
    request: Duration,
) -> Result<reqwest::Client, EmbedError> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(connect)
        .timeout(request)
        .redirect(reqwest::redirect::Policy::none());
    if bypasses_env_proxy(base_url) {
        builder = builder.no_proxy();
    }
    builder
        .build()
        .map_err(|e| EmbedError::Unavailable(format!("failed to build HTTP client: {e}")))
}

impl BgeM3LlamaCppEmbedder {
    /// Build an embedder for a llama.cpp server.
    ///
    /// * `llama_url` - base URL, e.g. `http://127.0.0.1:8080` (the `v1/embeddings` and
    ///   `health` paths are appended automatically).
    /// * `model` - model id sent in the request; pass `""` to let the server use its
    ///   default model.
    /// * `dim` - expected output width from the server (must be > 0).
    ///   Store schema compatibility is enforced at process resolution
    ///   (`GraphStore::vector_dimensions`), not here.
    pub fn new(
        llama_url: impl Into<String>,
        model: impl Into<String>,
        dim: usize,
    ) -> Result<Self, EmbedError> {
        let base_url = llama_url.into().trim_end_matches('/').to_string();
        if base_url.is_empty() {
            return Err(EmbedError::Unavailable(
                "llama.cpp base URL is empty".into(),
            ));
        }
        if dim == 0 {
            return Err(EmbedError::Unavailable("embedder dim must be > 0".into()));
        }
        // `v1/embeddings` is appended as text, so a query or fragment would
        // swallow it (`...?k=v/v1/embeddings` POSTs to the base path). Refused
        // without quoting: a query may carry a key.
        if let Ok(parsed) = reqwest::Url::parse(&base_url)
            && (parsed.query().is_some() || parsed.fragment().is_some())
        {
            return Err(EmbedError::Unavailable(format!(
                "the embedder URL for {} carries a query or fragment (not shown); give the \
                 base URL only (the v1/embeddings path is appended to it)",
                url_for_log(&base_url)
            )));
        }
        let url = format!("{base_url}/v1/embeddings");
        Ok(Self {
            client: build_client(&base_url, DEFAULT_CONNECT_TIMEOUT, DEFAULT_REQUEST_TIMEOUT)?,
            log_url: url_for_log(&url),
            url,
            base_url,
            model: model.into(),
            dim,
            authorization: None,
        })
    }

    /// Send `Authorization: Bearer <token>` on every embed request, for a hosted
    /// OpenAI-compatible endpoint (Workers AI, an API-keyed gateway). Without
    /// this call no `Authorization` header is sent.
    ///
    /// The header value is marked sensitive. `Debug` shows only that a token
    /// is set, and no message this adapter writes is built from the token
    /// itself. A non-2xx body is quoted into the error with every detectable
    /// echo of the token replaced: any run of 8 or more consecutive bytes of
    /// it, raw, JSON-escaped or percent-encoded (see `scrub`). A shorter,
    /// case-changed or otherwise re-encoded echo is not detected. A token
    /// that is not a valid header value is refused without quoting it.
    ///
    /// Refused unless the base URL is `https`, or `http` to a loopback host
    /// (`check_bearer_transport`). Redirects are never followed, and plain
    /// `http` to loopback ignores proxy environment variables, so the token
    /// goes only to the configured endpoint.
    pub fn with_bearer_token(mut self, token: &str) -> Result<Self, EmbedError> {
        check_bearer_transport(&self.base_url)?;
        let mut value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
            EmbedError::Unavailable(
                "the embedder API token is not a valid HTTP header value (value not shown)".into(),
            )
        })?;
        value.set_sensitive(true);
        self.authorization = Some(value);
        Ok(self)
    }

    /// A GET of `path` under the base URL, carrying the bearer header when one
    /// is configured: the EmbeddingGemma 2 layer's `/props` check (#22 PR 5).
    /// It goes through the same client, so redirects are not followed and the
    /// loopback proxy rule holds.
    #[cfg(feature = "embed-eg2")]
    pub(crate) fn authorized_get(&self, path: &str, timeout: Duration) -> reqwest::RequestBuilder {
        let mut req = self
            .client
            .get(format!("{}{path}", self.base_url))
            .timeout(timeout);
        if let Some(auth) = &self.authorization {
            req = req.header(AUTHORIZATION, auth.clone());
        }
        req
    }

    /// The base URL as logs and errors may print it ([`url_for_log`]).
    #[cfg(feature = "embed-eg2")]
    pub(crate) fn log_base_url(&self) -> String {
        url_for_log(&self.base_url)
    }

    /// Override connect/request timeouts (most users can rely on the defaults).
    pub fn with_timeouts(
        mut self,
        connect: Duration,
        request: Duration,
    ) -> Result<Self, EmbedError> {
        self.client = build_client(&self.base_url, connect, request)?;
        Ok(self)
    }

    /// `body`, cut and with every detectable echo of the bearer token replaced
    /// ([`scrub::quotable_body`], whose doc lists exactly what is caught),
    /// before it is quoted into an error. A gateway may echo the key it
    /// refused, and these errors reach logs and MCP receipts (issue #21).
    fn without_token(&self, body: &str, truncated: bool) -> String {
        scrub::quotable_body(body, self.bearer_token(), truncated)
    }

    /// The bearer token this adapter sends, if any.
    fn bearer_token(&self) -> Option<&str> {
        self.authorization
            .as_ref()
            .and_then(|auth| auth.as_bytes().strip_prefix(b"Bearer "))
            .and_then(|token| std::str::from_utf8(token).ok())
    }

    /// At most [`scrub::read_cap`] bytes of an error body, and whether more
    /// was left unread. The rest is never downloaded: only that much is ever
    /// quoted or scanned, and a hostile or broken endpoint can send any
    /// amount. A read error ends the body where it stopped.
    async fn capped_error_body(&self, mut resp: reqwest::Response) -> (String, bool) {
        let cap = scrub::read_cap(self.bearer_token().map_or(0, str::len));
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

    /// Report the server health without embedding anything.
    ///
    /// **llama.cpp only.** It calls llama.cpp's `/health`, which hosted
    /// OpenAI-compatible endpoints (Workers AI, Ollama) do not serve, and it
    /// sends no `Authorization` header. Do not wire it into `doctor` or startup
    /// for this kind; today only tests call it (issue #21).
    pub async fn check_health(&self) -> Result<(), EmbedError> {
        let resp = self
            .client
            .get(format!("{}/health", self.base_url))
            .timeout(HEALTH_TIMEOUT)
            .send()
            .await
            .map_err(|e| {
                EmbedError::Unavailable(format!(
                    "llama.cpp health check failed at {}: {}",
                    url_for_log(&self.base_url),
                    e.without_url()
                ))
            })?;
        if !resp.status().is_success() {
            let status = resp.status();
            return Err(EmbedError::Backend(format!(
                "llama.cpp health check returned {status}"
            )));
        }
        Ok(())
    }

    /// POST an embed request and parse the response.
    ///
    /// Error classification (consumed by the degradation contract in T7.2):
    /// * connect-level failures (server down/unreachable) -> `Unavailable`, so the caller
    ///   can fall back to canonical matching permanently and log once instead of hammering;
    /// * server-side rejections / malformed / dimension-mismatched output -> `Backend`
    ///   (server is up; the config or version is wrong, and the fix is permanent too).
    ///
    /// **CON-2 (fail hard):** there is deliberately NO retry that clears the configured
    /// model on a 400. A rejected request embeds in the server's default space, which
    /// would silently diverge from the `EmbeddingContract` stamped from config
    /// (same dim passes the only runtime check, so the mix would be undetectable).
    /// The operator fixes the server; the caller's degradation contract handles the rest.
    async fn request_embedding(
        &self,
        model: &str,
        text: &str,
    ) -> Result<EmbedResponse, EmbedError> {
        let body = EmbedRequest {
            model: model.to_string(),
            input: text.to_string(),
        };
        self.post_json(&body, model, default_status_rule).await
    }

    /// POST `body` as JSON to the embeddings endpoint and parse a success
    /// response as `R`; the transport half of [`Self::request_embedding`],
    /// shared with the EmbeddingGemma 2 layer (#22 PR 5), which sends its own
    /// request shapes over this client. Everything #21 promises holds here:
    /// the bearer header, no redirects, the capped and scrubbed error body,
    /// URLs printed without userinfo or query. `model` only labels messages.
    /// `rule` classifies a non-success status (normally
    /// [`default_status_rule`]).
    pub(crate) async fn post_json<B, R>(
        &self,
        body: &B,
        model: &str,
        rule: StatusRule,
    ) -> Result<R, EmbedError>
    where
        B: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        let mut req = self.client.post(&self.url).json(body);
        if let Some(auth) = &self.authorization {
            req = req.header(AUTHORIZATION, auth.clone());
        }
        let resp = req.send().await.map_err(|e| {
            EmbedError::Unavailable(format!(
                "{LLAMA_UNREACHABLE}{}: {}",
                self.log_url,
                e.without_url()
            ))
        })?;
        let status = resp.status();
        if status.is_success() {
            let bytes = resp.bytes().await.map_err(|e| {
                EmbedError::Backend(format!(
                    "llama.cpp response body could not be read: {}",
                    e.without_url()
                ))
            })?;
            // The parse error's own text quotes body content (`invalid type:
            // string "..."`), which an echoing endpoint could fill with the
            // token: name only its class and position (issue #21).
            return serde_json::from_slice(&bytes).map_err(|e| {
                EmbedError::Backend(format!(
                    "llama.cpp returned unparseable JSON ({:?} error at line {}, column {}; \
                     body not shown)",
                    e.classify(),
                    e.line(),
                    e.column()
                ))
            });
        }
        let code = status.as_u16();
        let text_body = if status.is_redirection() {
            // Neither the body nor `Location` is quoted: a redirect target
            // can carry a signed URL or a key in its query (issue #21).
            "(redirect not followed; the redirect target is not shown. Point the embedder URL \
             at the endpoint itself)"
                .to_string()
        } else {
            let (body, truncated) = self.capped_error_body(resp).await;
            self.without_token(&body, truncated)
        };
        let verdict = rule(code, &text_body);
        let hint = verdict.hint.unwrap_or("");
        match verdict.class {
            EmbedStatusClass::Transient => Err(EmbedError::Unavailable(format!(
                "llama.cpp is momentarily unwilling ({status}) for model {model:?}: {text_body}{hint}"
            ))),
            EmbedStatusClass::Unclassified => {
                // The table does not name this status. Conservative: treat it
                // as transient (the write stays durable) and log the status so
                // the gap in OUR table is on the record (J3-R2R-1 property 2).
                tracing::warn!(
                    url = %self.log_url,
                    model = %model,
                    status = %status,
                    "llama.cpp answered with status {status}, which the J3-R2R-1 rule table \
                     does not name; treating it as transient so the write stays durable"
                );
                Err(EmbedError::Unavailable(format!(
                    "llama.cpp answered {status} (unclassified by the J3 status rule table) for \
                     model {model:?}: {text_body}{hint}"
                )))
            }
            EmbedStatusClass::Content => {
                tracing::error!(
                    url = %self.log_url,
                    model = %model,
                    status = %status,
                    "llama.cpp refused this content ({status}); not retrying (CON-2)"
                );
                Err(EmbedError::Backend(format!(
                    "llama.cpp refused this content with {status} for model {model:?}: \
                     {text_body}{hint}"
                )))
            }
            EmbedStatusClass::PermanentConfig => {
                // 401/403/404 — a wrong URL, model name, or credentials. The
                // write is failed (the class is permanent for the deployment,
                // so retrying cannot help) AND an operator is told loudly.
                tracing::error!(
                    url = %self.log_url,
                    model = %model,
                    status = %status,
                    "llama.cpp answered {status} — a PERMANENT configuration error (URL, model \
                     name, or credentials). An operator must fix the embedder; every such \
                     write is being failed because retrying cannot help"
                );
                Err(EmbedError::Backend(format!(
                    "llama.cpp answered {status} (permanent configuration error) for model \
                     {model:?}: {text_body}{hint}"
                )))
            }
        }
    }
}

/// L2-normalize a vector in place. Rejects non-finite input (NaN/Inf from a bad
/// backend would otherwise poison every downstream cosine/L2 distance).
fn l2_normalize_in_place(v: &mut [f32]) -> Result<(), EmbedError> {
    let mut sum = 0.0f32;
    for &x in v.iter() {
        if !x.is_finite() {
            return Err(EmbedError::Backend(
                "llama.cpp returned a non-finite embedding".into(),
            ));
        }
        sum += x * x;
    }
    let norm = sum.sqrt();
    if norm <= f32::EPSILON {
        return Err(EmbedError::Backend(
            "llama.cpp returned a zero-norm embedding".into(),
        ));
    }
    for x in v.iter_mut() {
        *x /= norm;
    }
    Ok(())
}

#[async_trait]
impl Embedder for BgeM3LlamaCppEmbedder {
    fn dimensions(&self) -> usize {
        self.dim
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        if text.trim().is_empty() {
            return Err(EmbedError::Unavailable(
                "cannot embed empty/whitespace text".into(),
            ));
        }
        let parsed = self.request_embedding(&self.model, text).await?;
        let mut vec = parsed
            .data
            .into_iter()
            .next()
            .ok_or_else(|| {
                EmbedError::Backend("llama.cpp returned an empty embeddings list".into())
            })?
            .embedding;
        if vec.len() != self.dim {
            return Err(EmbedError::Backend(format!(
                "llama.cpp returned {} dims, expected {}",
                vec.len(),
                self.dim
            )));
        }
        l2_normalize_in_place(&mut vec)?;
        Ok(vec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    fn sample_embedding() -> Vec<f32> {
        // Deliberately non-unit so tests prove normalization happened.
        vec![3.0; 1024]
    }

    fn unit_magnitude(v: &[f32]) -> f32 {
        v.iter().map(|x| x * x).sum::<f32>().sqrt()
    }

    fn ok_response() -> serde_json::Value {
        serde_json::json!({
            "object": "list",
            "data": [{ "object": "embedding", "index": 0, "embedding": sample_embedding() }],
            "model": "bge-m3",
            "usage": { "prompt_tokens": 2, "total_tokens": 2 }
        })
    }

    #[tokio::test]
    async fn embeds_and_normalizes() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(200).json_body(ok_response());
        });
        let e = BgeM3LlamaCppEmbedder::new(server.base_url(), "", 1024).unwrap();
        let v = e.embed("user schema").await.unwrap();
        assert_eq!(v.len(), 1024);
        assert!(
            (unit_magnitude(&v) - 1.0).abs() < 1e-5,
            "norm={}",
            unit_magnitude(&v)
        );
        mock.assert();
    }

    #[tokio::test]
    async fn rejects_dimension_mismatch() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(200)
                .json_body(serde_json::json!({ "data": [{ "embedding": vec![1.0; 512] }] }));
        });
        let e = BgeM3LlamaCppEmbedder::new(server.base_url(), "", 1024).unwrap();
        let err = e.embed("anything").await.unwrap_err();
        assert!(matches!(err, EmbedError::Backend(_)), "{err:?}");
        assert!(err.to_string().contains("512"));
    }

    /// J3-R2R-1: `500` is in the rule table's TRANSIENT class — a loaded
    /// llama.cpp fast-fails a burst with it, and it says nothing about the
    /// input — so it must surface as `Unavailable` (leave the write durable),
    /// never as the permanent-`Backend` that used to destroy the whole backlog.
    #[tokio::test]
    async fn rejects_500_as_transient() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(500).body("internal error");
        });
        let e = BgeM3LlamaCppEmbedder::new(server.base_url(), "", 1024).unwrap();
        let err = e.embed("anything").await.unwrap_err();
        assert!(matches!(err, EmbedError::Unavailable(_)), "{err:?}");
        assert!(err.to_string().contains("500"));
    }

    /// A `500` whose body is llama-server's "increase the physical batch
    /// size" (b11517's exact text) is a content refusal with a ubatch hint,
    /// not a transient: the same input can never fit the same server, so
    /// retrying it would loop forever. A `500` with any other body stays
    /// transient.
    ///
    /// Mutation: drop the body check in `default_status_rule` -> red.
    #[tokio::test]
    async fn a_500_for_an_input_over_the_ubatch_is_a_content_refusal() {
        const BODY: &str = r#"{"error":{"code":500,"message":"input (3002 tokens) is too large to process. increase the physical batch size (current batch size: 512)","type":"server_error"}}"#;
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(500).body(BODY);
        });
        let e = BgeM3LlamaCppEmbedder::new(server.base_url(), "", 1024).unwrap();
        let err = e.embed("a long concept").await.unwrap_err();
        assert!(matches!(err, EmbedError::Backend(_)), "{err:?}");
        assert!(!err.is_transient(), "{err:?}");
        let msg = err.to_string();
        assert!(msg.contains("refused this content"), "{msg}");
        assert!(msg.contains("--ubatch-size"), "{msg}");

        assert_eq!(
            default_status_rule(500, BODY).class,
            EmbedStatusClass::Content
        );
        assert_eq!(
            default_status_rule(500, "internal error").class,
            EmbedStatusClass::Transient
        );
        // Only a 500 carries this meaning.
        assert_eq!(
            default_status_rule(503, BODY).class,
            EmbedStatusClass::Transient
        );
    }

    /// J3-R2R-1 algorithm unit test: the rule table itself, exhaustive and
    /// priority-ordered, with NO wildcard producing the permanent class.
    #[test]
    fn the_status_rule_table_classifies_every_named_status() {
        use EmbedStatusClass::*;
        // transient
        for code in [
            408, 425, 429, 500, 502, 503, 504, 509, 529, 505, 507, 508, 530, 599,
        ] {
            assert_eq!(classify_status(code), Transient, "status {code}");
        }
        // content — permanent for this input
        for code in [400, 413, 415, 422] {
            assert_eq!(classify_status(code), Content, "status {code}");
        }
        // permanent-config — an operator must act
        // (3xx since issue #21: redirects are never followed, so the
        // configured URL is wrong)
        for code in [401, 403, 404, 300, 301, 302, 303, 307, 308, 399] {
            assert_eq!(classify_status(code), PermanentConfig, "status {code}");
        }
        // unclassified — unnamed 4xx/1xx fall here, conservatively
        for code in [406, 409, 410, 411, 412, 414, 416, 418, 421, 101, 199] {
            assert_eq!(classify_status(code), Unclassified, "status {code}");
        }
        // No status OUTSIDE the four named content codes may be labeled Content
        // by a wildcard: an unknown status is a gap in OUR table, not a
        // statement about the caller's input (J3-R2R-1 properties 1 & 2).
        let content_codes = [400, 413, 415, 422];
        for code in 200..1000 {
            if content_codes.contains(&code) {
                continue;
            }
            assert_ne!(
                classify_status(code),
                EmbedStatusClass::Content,
                "status {code} must never fall into the content class by default"
            );
        }
    }

    /// J3-R2R-1 end-to-end at the adapter: a content refusal (`413`) is the
    /// absorbing permanent-`Backend` consumed as failed, while `503` is
    /// transient-`Unavailable`. The two must never collapse.
    #[tokio::test]
    async fn status_class_drives_the_error_variant() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(413).body("payload too large");
        });
        let e = BgeM3LlamaCppEmbedder::new(server.base_url(), "", 1024).unwrap();
        let err = e.embed("anything").await.unwrap_err();
        assert!(matches!(err, EmbedError::Backend(_)), "{err:?}");

        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(503).body("no slot available");
        });
        let e = BgeM3LlamaCppEmbedder::new(server.base_url(), "", 1024).unwrap();
        let err = e.embed("anything").await.unwrap_err();
        assert!(matches!(err, EmbedError::Unavailable(_)), "{err:?}");
        assert!(!matches!(err, EmbedError::Backend(_)));
    }

    #[tokio::test]
    async fn rejects_bad_json() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(200).body("not json");
        });
        let e = BgeM3LlamaCppEmbedder::new(server.base_url(), "", 1024).unwrap();
        let err = e.embed("anything").await.unwrap_err();
        assert!(matches!(err, EmbedError::Backend(_)));
    }

    #[tokio::test]
    async fn rejects_empty_data() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(200)
                .json_body(serde_json::json!({ "data": [] }));
        });
        let e = BgeM3LlamaCppEmbedder::new(server.base_url(), "", 1024).unwrap();
        let err = e.embed("anything").await.unwrap_err();
        assert!(matches!(err, EmbedError::Backend(_)));
    }

    #[tokio::test]
    async fn rejects_non_finite_embedding() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(200)
                .json_body(serde_json::json!({ "data": [{ "embedding": vec![f32::NAN; 1024] }] }));
        });
        let e = BgeM3LlamaCppEmbedder::new(server.base_url(), "", 1024).unwrap();
        assert!(e.embed("anything").await.is_err());
    }

    #[tokio::test]
    async fn rejects_empty_text() {
        let server = MockServer::start();
        let e = BgeM3LlamaCppEmbedder::new(server.base_url(), "", 1024).unwrap();
        assert!(matches!(
            e.embed("   ").await.unwrap_err(),
            EmbedError::Unavailable(_)
        ));
    }

    #[tokio::test]
    async fn rejects_400_with_configured_model_fail_hard() {
        let server = MockServer::start();
        // A request WITHOUT the configured model must NEVER be sent (CON-2: the
        // old fallback silently embedded in the server-default space).
        let default_model_mock = server.mock(|when, then| {
            when.method(POST).path("/v1/embeddings").matches(|r| {
                let body = r.body.as_deref().unwrap_or(&[]);
                !String::from_utf8_lossy(body).contains("\"model\"")
            });
            then.status(200).json_body(ok_response());
        });
        server.mock(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(400).body("model 'my-model' not loaded");
        });
        let e = BgeM3LlamaCppEmbedder::new(server.base_url(), "my-model", 1024).unwrap();
        let err = e.embed("anything").await.unwrap_err();
        assert!(matches!(err, EmbedError::Backend(_)), "{err:?}");
        assert!(err.to_string().contains("400"), "{err}");
        assert!(
            err.to_string().contains("my-model"),
            "the error must name the configured model: {err}"
        );
        default_model_mock.assert_hits(0);
    }

    #[tokio::test]
    async fn rejects_400_when_no_model_configured() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(400).body("bad request");
        });
        let e = BgeM3LlamaCppEmbedder::new(server.base_url(), "", 1024).unwrap();
        let err = e.embed("anything").await.unwrap_err();
        assert!(matches!(err, EmbedError::Backend(_)));
        assert!(err.to_string().contains("400"));
    }

    #[tokio::test]
    async fn health_check_ok() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/health");
            then.status(200).body("ok");
        });
        let e = BgeM3LlamaCppEmbedder::new(server.base_url(), "", 1024).unwrap();
        e.check_health().await.unwrap();
    }

    #[tokio::test]
    async fn constructor_validates_inputs_and_timeouts() {
        assert!(matches!(
            BgeM3LlamaCppEmbedder::new("", "", 1024),
            Err(EmbedError::Unavailable(_))
        ));
        assert!(matches!(
            BgeM3LlamaCppEmbedder::new("http://127.0.0.1:8080", "", 0),
            Err(EmbedError::Unavailable(_))
        ));
        let e = BgeM3LlamaCppEmbedder::new("http://127.0.0.1:8080", "", 1024)
            .unwrap()
            .with_timeouts(Duration::from_secs(1), Duration::from_secs(2))
            .unwrap();
        assert_eq!(e.dimensions(), 1024);
    }

    /// A fake token: only ever sent to a local mock server.
    const FAKE_TOKEN: &str = "fake-xyzzy-embed-token";

    fn has_authorization(r: &httpmock::prelude::HttpMockRequest) -> bool {
        r.headers
            .as_deref()
            .unwrap_or_default()
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("authorization"))
    }

    /// Issue #21: with a token, every embed request carries
    /// `Authorization: Bearer <token>`.
    #[tokio::test]
    async fn sends_bearer_token_when_configured() {
        let server = MockServer::start();
        let authed = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/embeddings")
                .header("authorization", format!("Bearer {FAKE_TOKEN}"));
            then.status(200).json_body(ok_response());
        });
        let e = BgeM3LlamaCppEmbedder::new(server.base_url(), "@cf/baai/bge-m3", 1024)
            .unwrap()
            .with_bearer_token(FAKE_TOKEN)
            .unwrap();
        e.embed("user schema").await.unwrap();
        authed.assert();
    }

    /// Issue #21: without a token no `Authorization` header is sent at all,
    /// so a local llama.cpp server sees exactly the request it saw before.
    #[tokio::test]
    async fn sends_no_authorization_header_by_default() {
        let server = MockServer::start();
        let with_auth = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/embeddings")
                .matches(has_authorization);
            then.status(500).body("an Authorization header was sent");
        });
        let without = server.mock(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(200).json_body(ok_response());
        });
        let e = BgeM3LlamaCppEmbedder::new(server.base_url(), "", 1024).unwrap();
        e.embed("user schema").await.unwrap();
        with_auth.assert_hits(0);
        without.assert();
    }

    /// Issue #21: the token never appears in `Debug` output.
    ///
    /// Mutation: derive `Debug` again -> red (the derive drops the redaction
    /// marker; drop `set_sensitive` as well and the header prints the token).
    #[test]
    fn debug_never_shows_the_token() {
        let e = BgeM3LlamaCppEmbedder::new("http://127.0.0.1:9", "", 1024)
            .unwrap()
            .with_bearer_token(FAKE_TOKEN)
            .unwrap();
        let shown = format!("{e:?}");
        assert!(!shown.contains(FAKE_TOKEN), "{shown}");
        assert!(shown.contains("value not shown"), "{shown}");
        let plain = BgeM3LlamaCppEmbedder::new("http://127.0.0.1:9", "", 1024).unwrap();
        assert!(format!("{plain:?}").contains("authorization: None"));
    }

    /// Issue #21: a token that is not a valid header value is refused at
    /// construction without being quoted.
    #[test]
    fn rejects_a_token_that_is_not_a_header_value() {
        let bad = "fake-xyzzy\nsecond-line";
        let err = BgeM3LlamaCppEmbedder::new("http://127.0.0.1:9", "", 1024)
            .unwrap()
            .with_bearer_token(bad)
            .unwrap_err();
        assert!(matches!(err, EmbedError::Unavailable(_)), "{err:?}");
        assert!(!err.to_string().contains("fake-xyzzy"), "{err}");
    }

    /// Issue #21: a bearer token is never sent in clear text over a network.
    /// Plain `http` to a non-loopback host is refused, naming the host but
    /// neither the token nor any userinfo in the URL.
    ///
    /// Mutation: drop the `check_bearer_transport` call -> red.
    #[test]
    fn a_token_is_refused_over_plain_http_to_a_remote_host() {
        for (url, host) in [
            ("http://api.example.com", "api.example.com"),
            ("http://10.0.0.5:8080/", "10.0.0.5"),
            ("http://[2001:db8::1]:8080", "[2001:db8::1]"),
            ("http://localhost.example.com", "localhost.example.com"),
            ("http://128.0.0.1", "128.0.0.1"),
            (
                "http://someone:fake-xyzzy-userinfo@example.com",
                "example.com",
            ),
            ("ftp://example.com", "example.com"),
            // Review L1: unspecified addresses are not loopback.
            ("http://0.0.0.0:8080", "0.0.0.0"),
            ("http://[::]:8080", "[::]"),
            // `localhost` as userinfo: the host is evil.com.
            ("http://localhost@evil.com", "evil.com"),
            ("http://localhost:8080@evil.com", "evil.com"),
            // The scheme is case-insensitive; the host still decides.
            ("HTTP://api.example.com", "api.example.com"),
        ] {
            let err = BgeM3LlamaCppEmbedder::new(url, "", 1024)
                .unwrap()
                .with_bearer_token(FAKE_TOKEN)
                .expect_err(&format!("{url}: a token over plain http must be refused"));
            assert!(matches!(err, EmbedError::Unavailable(_)), "{err:?}");
            let msg = err.to_string();
            assert!(msg.contains(host) && msg.contains("https"), "{msg}");
            assert!(!msg.contains(FAKE_TOKEN), "{msg}");
            assert!(!msg.contains("fake-xyzzy-userinfo"), "{msg}");
        }
        // Without a token the transport is not this check's business.
        BgeM3LlamaCppEmbedder::new("http://api.example.com", "", 1024).unwrap();
    }

    /// Issue #21: loopback over plain http (a local server) and any https
    /// endpoint still take a token.
    #[test]
    fn a_token_is_allowed_over_https_and_over_http_to_loopback() {
        for url in [
            "http://localhost:8080",
            "http://LOCALHOST:8080",
            "http://127.0.0.1:9",
            "http://127.1.2.3",
            "http://[::1]:8080",
            "http://[::ffff:127.0.0.1]:8080",
            // Review L1: shorthand, decimal, hex and octal IPv4 forms of
            // 127.0.0.1 normalise to it and are loopback.
            "http://127.1",
            "http://2130706433:8080",
            "http://0x7f.1",
            "http://0x7f000001",
            "http://0177.0.0.1",
            "http://localhost.:8080",
            // Userinfo does not change the host.
            "http://user@127.0.0.1:8080",
            // Upper-case scheme and host.
            "HTTP://LOCALHOST:8080",
            "HTTPS://API.EXAMPLE.COM",
            "https://api.cloudflare.com/client/v4/accounts/x/ai",
            "https://10.0.0.5",
        ] {
            BgeM3LlamaCppEmbedder::new(url, "", 1024)
                .unwrap()
                .with_bearer_token(FAKE_TOKEN)
                .unwrap_or_else(|e| panic!("{url}: {e}"));
        }
    }

    /// Issue #21 review M2: URL userinfo (and a query) is never printed: not
    /// by `Debug`, not in a transport error. Only scheme, host, port and path
    /// are shown.
    ///
    /// Mutation: print `self.url` in `Debug` or `{e}` in the send error -> red.
    #[tokio::test]
    async fn url_userinfo_is_never_printed() {
        let e = BgeM3LlamaCppEmbedder::new(
            "http://someone:fake-xyzzy-userinfo@127.0.0.1:9/base",
            "",
            1024,
        )
        .unwrap();
        let shown = format!("{e:?}");
        assert!(
            shown.contains("\"http://127.0.0.1:9/base/v1/embeddings\""),
            "{shown}"
        );
        {
            let url = "http://someone:fake-xyzzy-userinfo@127.0.0.1:9/base";
            let e = BgeM3LlamaCppEmbedder::new(url, "", 1024).unwrap();
            let shown = format!("{e:?}");
            assert!(!shown.contains("fake-xyzzy"), "{shown}");
            let err = e.embed("anything").await.unwrap_err();
            assert!(matches!(err, EmbedError::Unavailable(_)), "{err:?}");
            let msg = err.to_string();
            assert!(msg.contains("llama.cpp unreachable"), "{msg}");
            assert!(!msg.contains("fake-xyzzy"), "{msg}");
            let err = e.check_health().await.unwrap_err().to_string();
            assert!(!err.contains("fake-xyzzy"), "{err}");
        }
    }

    /// A base URL with a query or fragment is refused: the appended
    /// `v1/embeddings` would land inside it and the request would go to the
    /// base path. The query is not quoted (it may carry a key).
    ///
    /// Mutation: drop the query/fragment check in `new` -> red.
    #[test]
    fn a_base_url_with_a_query_or_fragment_is_refused() {
        for url in [
            "http://127.0.0.1:9/base?key=fake-xyzzy-query",
            "https://gw.example.com/ai#fake-xyzzy-fragment",
        ] {
            let err = BgeM3LlamaCppEmbedder::new(url, "", 1024).unwrap_err();
            assert!(matches!(err, EmbedError::Unavailable(_)), "{err:?}");
            let msg = err.to_string();
            assert!(msg.contains("query or fragment"), "{msg}");
            assert!(!msg.contains("fake-xyzzy"), "{msg}");
        }
    }

    /// Issue #21 review M2: a 2xx body that does not parse is not quoted
    /// into the error (a hostile or echoing endpoint could put the token in
    /// it); the error names the parse failure's class and position only.
    #[tokio::test]
    async fn an_unparseable_success_body_is_not_quoted() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(200)
                .json_body(serde_json::json!({ "data": FAKE_TOKEN }));
        });
        let e = BgeM3LlamaCppEmbedder::new(server.base_url(), "", 1024)
            .unwrap()
            .with_bearer_token(FAKE_TOKEN)
            .unwrap();
        let err = e.embed("anything").await.unwrap_err();
        assert!(matches!(err, EmbedError::Backend(_)), "{err:?}");
        let msg = err.to_string();
        assert!(msg.contains("unparseable JSON"), "{msg}");
        assert!(msg.contains("line 1"), "{msg}");
        assert!(!msg.contains("xyzzy"), "{msg}");
        assert!(!msg.contains(&server.base_url()), "{msg}");
    }

    /// Issue #21 review M1: a redirect is never followed. reqwest's default
    /// policy keeps `Authorization` on a same-host, same-port redirect (even
    /// an https-to-http downgrade), so following one could resend the token
    /// in clear text. The 3xx is a permanent configuration error naming the
    /// status, and the redirect target is not quoted.
    ///
    /// Mutation: drop `.redirect(Policy::none())` from `build_client` -> red
    /// (the target is hit, with the token).
    #[tokio::test]
    async fn a_redirect_is_not_followed_and_does_not_resend_the_token() {
        for code in [301u16, 302, 307, 308] {
            let server = MockServer::start();
            let target = server.mock(|when, then| {
                when.path("/elsewhere/v1/embeddings");
                then.status(200).json_body(ok_response());
            });
            server.mock(|when, then| {
                when.method(POST).path("/v1/embeddings");
                then.status(code)
                    .header("location", server.url("/elsewhere/v1/embeddings"))
                    .body(format!("moved to {}", server.url("/elsewhere")));
            });
            let e = BgeM3LlamaCppEmbedder::new(server.base_url(), "", 1024)
                .unwrap()
                .with_bearer_token(FAKE_TOKEN)
                .unwrap();
            let err = e.embed("anything").await.unwrap_err();
            target.assert_hits(0);
            assert!(matches!(err, EmbedError::Backend(_)), "{code}: {err:?}");
            let msg = err.to_string();
            assert!(msg.contains(&code.to_string()), "{code}: {msg}");
            assert!(msg.contains("permanent configuration error"), "{msg}");
            assert!(!msg.contains("elsewhere"), "{code}: {msg}");
            assert!(!msg.contains(FAKE_TOKEN), "{code}: {msg}");
        }
    }

    /// Issue #21 review M1: plain http to loopback ignores the proxy
    /// environment (a proxy would carry a loopback token off the machine);
    /// https and plain http elsewhere keep it.
    ///
    /// Mutation: make `bypasses_env_proxy` always false -> red.
    #[test]
    fn only_plain_http_to_loopback_ignores_env_proxies() {
        for url in [
            "http://127.0.0.1:8080",
            "http://localhost:8080",
            "HTTP://LOCALHOST",
            "http://[::1]:8080",
            "http://127.1",
        ] {
            assert!(bypasses_env_proxy(url), "{url}");
        }
        for url in [
            "https://127.0.0.1:8443",
            "https://api.cloudflare.com",
            "http://10.0.0.5:8080",
            "http://api.example.com",
            "not a url",
        ] {
            assert!(!bypasses_env_proxy(url), "{url}");
        }
    }

    /// Issue #21, classification unchanged: a rejected token (401/403) is the
    /// permanent `Backend` an operator must fix, and the error does not carry
    /// the token.
    #[tokio::test]
    async fn rejected_token_is_a_permanent_backend_error() {
        for code in [401u16, 403] {
            let server = MockServer::start();
            server.mock(|when, then| {
                when.method(POST).path("/v1/embeddings");
                then.status(code).body("authentication error");
            });
            let e = BgeM3LlamaCppEmbedder::new(server.base_url(), "", 1024)
                .unwrap()
                .with_bearer_token(FAKE_TOKEN)
                .unwrap();
            let err = e.embed("anything").await.unwrap_err();
            assert!(matches!(err, EmbedError::Backend(_)), "{code}: {err:?}");
            assert!(!err.to_string().contains(FAKE_TOKEN), "{err}");
        }
    }

    /// Issue #21 self-review: a gateway that echoes the presented key in its
    /// error body (some do, to say which key was refused) must not carry the
    /// token into the error, which reaches logs and MCP receipts. Covers one
    /// status from every class that quotes the body, and a raw, a JSON-quoted
    /// and a masked (prefix and suffix) echo; `scrub`'s own tests cover the
    /// other forms.
    ///
    /// Mutation: quote the raw body again, or replace only the exact token
    /// (review M2) -> red.
    #[tokio::test]
    async fn an_error_body_echoing_the_token_never_carries_it() {
        for code in [401u16, 400, 429, 418] {
            let server = MockServer::start();
            server.mock(|when, then| {
                when.method(POST).path("/v1/embeddings");
                // Raw, JSON-quoted, and masked down to a prefix and a suffix.
                then.status(code).body(format!(
                    "invalid key: Bearer {FAKE_TOKEN} {} ({}...{})",
                    serde_json::json!({ "key": FAKE_TOKEN }),
                    &FAKE_TOKEN[..10],
                    &FAKE_TOKEN[FAKE_TOKEN.len() - 10..]
                ));
            });
            let e = BgeM3LlamaCppEmbedder::new(server.base_url(), "", 1024)
                .unwrap()
                .with_bearer_token(FAKE_TOKEN)
                .unwrap();
            let err = e.embed("anything").await.unwrap_err().to_string();
            assert!(!err.contains(FAKE_TOKEN), "{code}: {err}");
            assert!(!err.contains(&FAKE_TOKEN[..10]), "{code}: {err}");
            assert!(
                !err.contains(&FAKE_TOKEN[FAKE_TOKEN.len() - 10..]),
                "{code}: {err}"
            );
            assert!(
                err.contains("invalid key"),
                "{code}: the rest of the body is kept: {err}"
            );
        }
    }

    /// Issue #21 review: an error body is downloaded only up to what is
    /// ever quoted or scanned, so a huge body is never read whole, and the
    /// error says the rest was not shown.
    ///
    /// Mutation: read the body with `text()` again -> red (the error then
    /// counts the bytes it read past the cut).
    #[tokio::test]
    async fn a_huge_error_body_is_read_only_up_to_the_cap() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(400).body("x".repeat(1 << 20));
        });
        let e = BgeM3LlamaCppEmbedder::new(server.base_url(), "", 1024)
            .unwrap()
            .with_bearer_token(FAKE_TOKEN)
            .unwrap();
        let err = e.embed("anything").await.unwrap_err().to_string();
        assert!(
            err.ends_with("(rest of the body not shown)"),
            "{}",
            &err[err.len() - 80..]
        );
        assert!(
            err.len() < scrub::QUOTED_BODY_MAX + 512,
            "{} bytes",
            err.len()
        );
    }

    /// Live test against Cloudflare Workers AI's OpenAI-compatible endpoint
    /// (issue #21). `#[ignore]`d, and additionally skipped unless both
    /// `CLOUDFLARE_ACCOUNT_ID` and `CLOUDFLARE_API_TOKEN` are set, so CI's
    /// `-- --ignored` never reaches the network without credentials.
    #[tokio::test]
    #[ignore]
    async fn live_workers_ai_bge_m3() {
        let var = |k: &str| std::env::var(k).ok().filter(|s| !s.trim().is_empty());
        let (Some(account), Some(token)) =
            (var("CLOUDFLARE_ACCOUNT_ID"), var("CLOUDFLARE_API_TOKEN"))
        else {
            eprintln!(
                "SKIP live_workers_ai_bge_m3: CLOUDFLARE_ACCOUNT_ID and CLOUDFLARE_API_TOKEN \
                 must both be set"
            );
            return;
        };
        let url = format!("https://api.cloudflare.com/client/v4/accounts/{account}/ai");
        let e = BgeM3LlamaCppEmbedder::new(url, "@cf/baai/bge-m3", 1024)
            .unwrap()
            .with_bearer_token(token.trim())
            .unwrap();
        let v = e.embed("register user").await.unwrap();
        assert_eq!(v.len(), 1024);
        let n = unit_magnitude(&v);
        assert!((n - 1.0).abs() < 1e-4, "L2 norm {n} should be ~1");
    }

    /// Live smoke test against a running llama.cpp server
    /// (`./scripts/run-llama-embed.sh`). `#[ignore]`d, and additionally
    /// honest-skipped without `LAMBO_LLAMA_EMBED_URL`, so CI's `-- --ignored`
    /// never tries localhost and crashes (no server there).
    #[tokio::test]
    #[ignore]
    async fn live_smoke_against_llama_server() {
        let Some(url) = std::env::var("LAMBO_LLAMA_EMBED_URL")
            .ok()
            .filter(|s| !s.is_empty())
        else {
            eprintln!("SKIP live_smoke_against_llama_server: LAMBO_LLAMA_EMBED_URL not set");
            return;
        };
        let e = BgeM3LlamaCppEmbedder::new(url.clone(), "", 1024).unwrap();
        e.check_health()
            .await
            .expect("llama.cpp server must be running (scripts/run-llama-embed.sh)");
        let a = e.embed("register user").await.unwrap();
        let b = e.embed("create account").await.unwrap();
        let far = e
            .embed("quantum chromodynamics lattice gauge")
            .await
            .unwrap();
        assert_eq!(a.len(), 1024);
        let n: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((n - 1.0).abs() < 1e-4, "L2 norm {n} should be ~1");
        let sim_a_b = crate::embed::cosine(&a, &b);
        let sim_a_far = crate::embed::cosine(&a, &far);
        assert!(
            sim_a_b > sim_a_far,
            "near {sim_a_b:.3} should exceed far {sim_a_far:.3}"
        );
        eprintln!("BGE-M3 live: near={sim_a_b:.4} far={sim_a_far:.4} dim=1024");
    }
}
