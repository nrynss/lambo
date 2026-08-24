//! Vertex Gemini embeddings (`gemini-embedding-001`) over Google's REST `embedContent`.
//!
//! This adapter authenticates as a Google principal. With a service-account key it mints an
//! RS256 JSON Web Token (`jsonwebtoken::EncodingKey::from_rsa_pem` on the private key PEM)
//! whose claims are `{ iss: client_email, scope, aud, iat, exp }` and exchanges it for an
//! OAuth `access_token` (a `jwt-bearer` grant); with an authorized-user ADC file it uses the
//! `refresh_token` grant. It caches the token until roughly a minute before `expires_in`, then
//! calls Vertex
//! `{location}-aiplatform.googleapis.com/v1/projects/{project}/locations/{location}/publishers/google/models/{model}:embedContent`
//! with `Authorization: Bearer <token>` and body
//! `{"content": {"parts": [{"text": "<text>"}]}, "outputDimensionality": <dim>}`.
//!
//! **outputDimensionality IS sent from the configured `dim`.** `gemini-embedding-001`
//! truncates to 768, 1536, or 3072 via that parameter, so the returned width is the
//! configured one when it is in the supported set. A4 owns the construction guard that
//! rejects any other `dim` BEFORE an unsupported value is ever sent; this adapter sends
//! `outputDimensionality` as-is and fails width==dim with `Backend` if Vertex disagrees.
//! The vector is L2-normalized so downstream Cockroach `<->` and SQLite cosine rankings
//! agree (the shared `Embedder` output contract).
//!
//! **Error classification (mirrors `bge_m3`, per the A3 brief):**
//! * reqwest connect/transport failure (token endpoint or Vertex) -> [`EmbedError::Unavailable`]
//!   so the caller can degrade to canonical matching.
//! * any non-2xx HTTP status from the token endpoint or Vertex -> [`EmbedError::Backend`]
//!   (auth, quota, or a wrong model/URL; permanent, the operator fixes it).
//! * unparseable response body, missing embedding/values, dimension mismatch, non-finite or
//!   zero-norm vector -> [`EmbedError::Backend`].
//!
//! **CON-2:** there is deliberately no retry that could change the request. A non-success
//! answer is returned as-is; the caller's degradation contract handles the rest.
//!
//! **CON-7:** `embed` rejects empty / whitespace-only text with [`EmbedError::Unavailable`]
//! BEFORE any network (no token minted, no request sent).

use async_trait::async_trait;
use serde::Deserialize;

use super::{EmbedError, Embedder};

/// Vertex REST embedContent model path of `gemini-embedding-001`.
pub(crate) const DEFAULT_MODEL: &str = "gemini-embedding-001";
/// Default Vertex region when `location` is not configured.
pub(crate) const DEFAULT_LOCATION: &str = "us-central1";

// ---------------------------------------------------------------------------
// Google auth lives in `crate::gcp_auth`, shared with the Postgres store's Cloud
// SQL IAM login (2026-08-24). Only Vertex-specific pieces stay here. Re-exported
// under the names this module has always used so call sites and tests are
// unchanged by the move.
// ---------------------------------------------------------------------------
pub use crate::gcp_auth::{
    build_client, load_credentials, GoogleAuthError, GoogleOAuthTokenSource,
};
/// Cloud-platform scope requested on the OAuth token for Vertex.
pub(crate) const OAUTH_SCOPE: &str = crate::gcp_auth::SCOPE_CLOUD_PLATFORM;

#[cfg(test)]
pub(crate) use crate::gcp_auth::TEST_RSA_PRIVATE_KEY_PEM;

/// The auth module's classification maps ONE-FOR-ONE onto this adapter's, which is what
/// keeps A3's degradation contract true after the consolidation: a token endpoint that is
/// unreachable still degrades the caller to canonical matching, and a rejected grant still
/// stops it. Pinned by `auth_error_classification_is_preserved`.
impl From<GoogleAuthError> for EmbedError {
    fn from(e: GoogleAuthError) -> Self {
        match e {
            GoogleAuthError::Unavailable(m) => EmbedError::Unavailable(m),
            GoogleAuthError::Backend(m) => EmbedError::Backend(m),
        }
    }
}

/// Supplies the Bearer token for Vertex calls. Production uses mint + OAuth exchange;
/// tests inject a fake so `GeminiEmbedder` logic is exercisable offline.
#[async_trait]
pub trait GeminiTokenSource: Send + Sync + std::fmt::Debug {
    async fn access_token(&mut self) -> Result<String, EmbedError>;
}

#[async_trait]
impl GeminiTokenSource for GoogleOAuthTokenSource {
    async fn access_token(&mut self) -> Result<String, EmbedError> {
        // Inherent method on the shared source; the `From` above preserves the split.
        Ok(GoogleOAuthTokenSource::access_token(self).await?)
    }
}

/// Vertex `:embedContent` response envelope: `{"embedding": {"values": [...]}}`.
/// (The old draft assumed `predictions[].embeddings.values`; the real API returns a
/// top-level `embedding` object. Caught by the live Vertex probe, 2026-08-24.)
#[derive(Debug, Deserialize)]
struct EmbedResponse {
    embedding: Embed,
}

#[derive(Debug, Deserialize)]
struct Embed {
    values: Vec<f32>,
}

/// Vertex Gemini embeddings (gemini-embedding-001).
#[derive(Debug)]
pub struct GeminiEmbedder {
    model: String,
    dim: usize,
    token_source: tokio::sync::Mutex<Box<dyn GeminiTokenSource>>,
    /// Full `embedContent` URL; injectable for tests (mirrors bge_m3's `base_url`).
    embed_url: String,
    client: reqwest::Client,
}

impl GeminiEmbedder {
    /// The canonical Vertex `embedContent` URL for `project`/`location`/`model`.
    pub fn vertex_embed_url(project: &str, location: &str, model: &str) -> String {
        format!(
            "https://{location}-aiplatform.googleapis.com/v1/projects/{project}/locations/{location}/publishers/google/models/{model}:embedContent"
        )
    }

    /// Build an embedder with an explicit token source and embed URL (production and tests).
    ///
    /// `dim` must be > 0. Store schema compatibility is enforced at process resolution
    /// (`GraphStore::vector_dimensions`), not here.
    pub fn new(
        model: impl Into<String>,
        dim: usize,
        token_source: Box<dyn GeminiTokenSource>,
        embed_url: impl Into<String>,
        client: reqwest::Client,
    ) -> Result<Self, EmbedError> {
        if dim == 0 {
            return Err(EmbedError::Unavailable("embedder dim must be > 0".into()));
        }
        let url = embed_url.into();
        if url.is_empty() {
            return Err(EmbedError::Unavailable("Vertex embed URL is empty".into()));
        }
        Ok(Self {
            model: model.into(),
            dim,
            token_source: tokio::sync::Mutex::new(token_source),
            embed_url: url,
            client,
        })
    }

    /// The configured model id (`gemini-embedding-001` by default), stamped into the
    /// session `EmbeddingContract.model`.
    pub fn model_identity(&self) -> &str {
        &self.model
    }

    /// POST an embedContent request and parse the `embedding.values`.
    ///
    /// CON-2: no retry. A rejected or malformed answer is returned as-is; the caller's
    /// degradation contract decides whether the write stays durable.
    async fn request_embedding(&self, text: &str) -> Result<EmbedResponse, EmbedError> {
        let token = {
            let mut guard = self.token_source.lock().await;
            guard.access_token().await?
        };
        let body = serde_json::json!({
            // Real Vertex `:embedContent` shape: `content` holds parts with text. The
            // earlier `{"content": {"content": text}}` draft was rejected by Vertex
            // (unknown field `content` at `content`), caught by the live probe.
            "content": { "parts": [ { "text": text } ] },
            // A3-R1-1: send the configured width so a non-native dim is actually
            // requested. A4 owns the construction guard that rejects a dim outside
            // {768, 1536, 3072} before this unsupported-value path is ever reached.
            "outputDimensionality": self.dim,
        });
        let resp = self
            .client
            .post(&self.embed_url)
            .bearer_auth(&token)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                EmbedError::Unavailable(format!("Vertex embedContent unreachable: {e}"))
            })?;
        let status = resp.status();
        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| EmbedError::Backend(format!("Vertex returned unparseable JSON: {e}")))?;
        if !status.is_success() {
            return Err(EmbedError::Backend(format!(
                "Vertex embedContent returned {status}: {json}"
            )));
        }
        serde_json::from_value(json).map_err(|e| {
            EmbedError::Backend(format!(
                "Vertex embedContent returned malformed payload: {e}"
            ))
        })
    }
}

/// L2-normalize a vector in place. Rejects non-finite input (NaN/Inf from a bad backend
/// would otherwise poison every downstream cosine/L2 distance).
fn l2_normalize_in_place(v: &mut [f32]) -> Result<(), EmbedError> {
    let mut sum = 0.0f32;
    for &x in v.iter() {
        if !x.is_finite() {
            return Err(EmbedError::Backend(
                "Vertex returned a non-finite embedding".into(),
            ));
        }
        sum += x * x;
    }
    let norm = sum.sqrt();
    if norm <= f32::EPSILON {
        return Err(EmbedError::Backend(
            "Vertex returned a zero-norm embedding".into(),
        ));
    }
    for x in v.iter_mut() {
        *x /= norm;
    }
    Ok(())
}

#[async_trait]
impl Embedder for GeminiEmbedder {
    fn dimensions(&self) -> usize {
        self.dim
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        // CON-7: reject empty / whitespace-only text BEFORE any network.
        if text.trim().is_empty() {
            return Err(EmbedError::Unavailable(
                "cannot embed empty/whitespace text".into(),
            ));
        }
        let response = self.request_embedding(text).await?;
        let mut vec = response.embedding.values;
        if vec.len() != self.dim {
            return Err(EmbedError::Backend(format!(
                "Vertex returned {} dims, expected {}",
                vec.len(),
                self.dim
            )));
        }
        l2_normalize_in_place(&mut vec)?;
        Ok(vec)
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    fn unit_magnitude(v: &[f32]) -> f32 {
        v.iter().map(|x| x * x).sum::<f32>().sqrt()
    }

    fn sample_embedding() -> Vec<f32> {
        // Deliberately non-unit so a test proves normalization happened.
        vec![3.0; 768]
    }

    #[derive(Debug)]
    struct FakeTokenSource {
        token: String,
    }

    #[async_trait]
    impl GeminiTokenSource for FakeTokenSource {
        async fn access_token(&mut self) -> Result<String, EmbedError> {
            Ok(self.token.clone())
        }
    }

    fn test_embedder(server: &MockServer, dim: usize) -> GeminiEmbedder {
        GeminiEmbedder::new(
            "gemini-embedding-001",
            dim,
            Box::new(FakeTokenSource {
                token: "tok".to_string(),
            }),
            format!("{}/embedContent", server.base_url()),
            reqwest::Client::new(),
        )
        .unwrap()
    }

    /// The consolidation's contract: `gcp_auth`'s two-way classification arrives here
    /// unchanged, so an unreachable token endpoint still degrades the caller to canonical
    /// matching (A3/CON-2) and a refused grant still stops it. The mint/exchange/cache
    /// behaviour itself is pinned in `crate::gcp_auth`'s tests, which is where the code
    /// now lives.
    #[test]
    fn auth_error_classification_is_preserved() {
        let unavailable: EmbedError = GoogleAuthError::Unavailable("no route".into()).into();
        assert!(matches!(unavailable, EmbedError::Unavailable(ref m) if m == "no route"));
        let backend: EmbedError = GoogleAuthError::Backend("401 invalid_grant".into()).into();
        assert!(matches!(backend, EmbedError::Backend(ref m) if m == "401 invalid_grant"));
    }

    /// The Vertex adapter asks for the cloud-platform scope, not the store's wider set.
    #[test]
    fn vertex_asks_for_the_cloud_platform_scope_only() {
        assert_eq!(OAUTH_SCOPE, crate::gcp_auth::SCOPE_CLOUD_PLATFORM);
        assert!(!OAUTH_SCOPE.contains("sqlservice.login"));
    }

    #[tokio::test]
    async fn embeds_and_normalizes() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/embedContent")
                .header("Authorization", "Bearer tok")
                // A3-R1-3 + live probe: pin the real schema (parts.text) and the width so
                // neither the outputDimensionality omission nor the request-body regression
                // can silently return.
                .body_contains("\"outputDimensionality\":768")
                .body_contains("\"parts\"")
                .body_contains("\"text\"");
            then.status(200).json_body(serde_json::json!({
                "embedding": { "values": sample_embedding() }
            }));
        });
        let e = test_embedder(&server, 768);
        let v = e.embed("user schema").await.unwrap();
        assert_eq!(v.len(), 768);
        assert!(
            (unit_magnitude(&v) - 1.0).abs() < 1e-5,
            "norm={}",
            unit_magnitude(&v)
        );
        mock.assert();
    }

    /// CON-7: empty and whitespace input must fail with `Unavailable` before any network.
    #[tokio::test]
    async fn rejects_empty_and_whitespace_before_network() {
        let server = MockServer::start();
        let e = test_embedder(&server, 768);
        for bad in ["", "   ", "\t\n "] {
            let err = e.embed(bad).await.unwrap_err();
            assert!(
                matches!(err, EmbedError::Unavailable(_)),
                "{bad:?}: {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn transport_failure_is_unavailable() {
        // Nothing listens on port 9: the connect fails -> Unavailable (CAN degrade).
        let e = GeminiEmbedder::new(
            "gemini-embedding-001",
            768,
            Box::new(FakeTokenSource {
                token: "tok".to_string(),
            }),
            "http://127.0.0.1:9/embedContent",
            reqwest::Client::new(),
        )
        .unwrap();
        let err = e.embed("hello").await.unwrap_err();
        assert!(matches!(err, EmbedError::Unavailable(_)), "{err:?}");
    }

    #[tokio::test]
    async fn vertex_http_errors_are_backend() {
        let server = MockServer::start();
        for status in [400u16, 403, 500] {
            let mut mock = server.mock(|when, then| {
                when.method(POST).path("/embedContent");
                then.status(status).body("upstream error");
            });
            let e = test_embedder(&server, 768);
            let err = e.embed("hello").await.unwrap_err();
            assert!(
                matches!(err, EmbedError::Backend(_)),
                "status {status}: {err:?}"
            );
            mock.delete();
        }
    }

    #[tokio::test]
    async fn malformed_body_is_backend() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/embedContent");
            then.status(200).body("not json");
        });
        let e = test_embedder(&server, 768);
        let err = e.embed("hello").await.unwrap_err();
        assert!(matches!(err, EmbedError::Backend(_)), "{err:?}");
    }

    #[tokio::test]
    async fn missing_embedding_is_backend() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/embedContent");
            // Real shape omits the top-level `embedding` field -> deserialize fails.
            then.status(200).json_body(serde_json::json!({}));
        });
        let e = test_embedder(&server, 768);
        let err = e.embed("hello").await.unwrap_err();
        assert!(matches!(err, EmbedError::Backend(_)), "{err:?}");
    }

    #[tokio::test]
    async fn rejects_dimension_mismatch() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/embedContent");
            then.status(200).json_body(serde_json::json!({
                "embedding": { "values": vec![1.0; 512] }
            }));
        });
        let e = test_embedder(&server, 768);
        let err = e.embed("anything").await.unwrap_err();
        assert!(matches!(err, EmbedError::Backend(_)), "{err:?}");
        assert!(err.to_string().contains("512"), "{err:?}");
    }

    #[test]
    fn rejects_non_finite_and_zero_norm() {
        let mut nan = vec![f32::NAN, 1.0];
        assert!(matches!(
            l2_normalize_in_place(&mut nan),
            Err(EmbedError::Backend(_))
        ));
        let mut inf = vec![f32::INFINITY, 0.0];
        assert!(matches!(
            l2_normalize_in_place(&mut inf),
            Err(EmbedError::Backend(_))
        ));
        let mut zero = vec![0.0, 0.0];
        assert!(matches!(
            l2_normalize_in_place(&mut zero),
            Err(EmbedError::Backend(_))
        ));
        let mut ok = vec![1.0, 2.0, 2.0];
        l2_normalize_in_place(&mut ok).unwrap();
        assert!((unit_magnitude(&ok) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn model_identity_returns_configured_model() {
        let e = GeminiEmbedder::new(
            "gemini-embedding-001",
            768,
            Box::new(FakeTokenSource {
                token: "tok".to_string(),
            }),
            "http://127.0.0.1:9/embedContent",
            reqwest::Client::new(),
        )
        .unwrap();
        assert_eq!(e.model_identity(), "gemini-embedding-001");
        // as_any exposes the concrete type for gemini_identity downcast.
        assert!(e
            .as_any()
            .and_then(|a| a.downcast_ref::<GeminiEmbedder>())
            .is_some());
    }

    /// A-E2E-2 closure: an operator-runnable live test proving the real OAuth
    /// exchange and Vertex `embedContent` round-trip. `#[ignore]`d so CI never runs
    /// it (no service-account key there); run it on a machine with credentials:
    ///
    ///     LAMBO_GEMINI_CREDENTIALS=/path/sa.json cargo test \
    ///       --features embed-gemini --lib embed::gemini::tests::gemini_live_embeds_against_vertex \
    ///       -- --ignored
    ///
    /// Resolves credentials from `LAMBO_GEMINI_CREDENTIALS` else
    /// `GOOGLE_APPLICATION_CREDENTIALS`; skips cleanly (with a message) when neither
    /// is set.
    #[tokio::test]
    #[ignore]
    async fn gemini_live_embeds_against_vertex() {
        let creds_path = std::env::var_os("LAMBO_GEMINI_CREDENTIALS")
            .or_else(|| std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS"));
        let Some(creds_path) = creds_path else {
            eprintln!("skipping: set LAMBO_GEMINI_CREDENTIALS or GOOGLE_APPLICATION_CREDENTIALS");
            return;
        };
        let creds = load_credentials(std::path::Path::new(&creds_path)).unwrap();

        let project = std::env::var("LAMBO_GEMINI_PROJECT")
            .ok()
            .filter(|s| !s.is_empty())
            .or(creds.project_id())
            .expect("set LAMBO_GEMINI_PROJECT or rely on the key's project_id");
        let location = std::env::var("LAMBO_GEMINI_LOCATION")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_LOCATION.to_string());
        let model = std::env::var("LAMBO_GEMINI_MODEL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        // gemini supports 768, 1536 or 3072; default the live probe to the recommended
        // 1536 so the adapter's requested width matches the response width.
        let dim = std::env::var("LAMBO_GEMINI_DIM")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1536);
        let client = build_client().unwrap();
        let token_source =
            Box::new(GoogleOAuthTokenSource::new(creds, client.clone(), OAUTH_SCOPE).unwrap());
        let embed_url = GeminiEmbedder::vertex_embed_url(&project, &location, &model);
        let e = GeminiEmbedder::new(model, dim, token_source, embed_url, client).unwrap();
        let v = e.embed("lambo live vertex round-trip").await.unwrap();
        assert_eq!(v.len(), dim, "live Vertex returned the configured width");
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-3,
            "live Vertex vector must be L2-normalized, norm={norm}"
        );
    }
}
