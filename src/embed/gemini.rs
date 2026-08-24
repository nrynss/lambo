//! Vertex Gemini embeddings (`gemini-embedding-001`) over Google's REST `embedContent`.
//!
//! This adapter authenticates as a Google service account. It mints an RS256 JSON Web
//! Token (`jsonwebtoken::EncodingKey::from_rsa_pem` on the service account private key PEM)
//! whose claims are `{ iss: client_email, scope, aud: token_uri, iat, exp }`, exchanges it for
//! an OAuth `access_token` at the service account's `token_uri` (an urlencoded
//! `jwt-bearer` grant), caches that token until roughly a minute before `expires_in`, then
//! calls Vertex
//! `{location}-aiplatform.googleapis.com/v1/projects/{project}/locations/{location}/publishers/google/models/{model}:embedContent`
//! with `Authorization: Bearer <token>` and body `{"content": {"content": "<text>"}}`.
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
//! * unparseable response body, missing prediction/values, dimension mismatch, non-finite or
//!   zero-norm vector -> [`EmbedError::Backend`].
//!
//! **CON-2:** there is deliberately no retry that could change the request. A non-success
//! answer is returned as-is; the caller's degradation contract handles the rest.
//!
//! **CON-7:** `embed` rejects empty / whitespace-only text with [`EmbedError::Unavailable`]
//! BEFORE any network (no token minted, no request sent).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::{EmbedError, Embedder};

/// Vertex REST embedContent model path of `gemini-embedding-001`.
pub(crate) const DEFAULT_MODEL: &str = "gemini-embedding-001";
/// Default Vertex region when `location` is not configured.
pub(crate) const DEFAULT_LOCATION: &str = "us-central1";
/// Fallback OAuth token endpoint when the service-account JSON omits `token_uri`.
pub(crate) const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
/// Cloud-platform scope requested on the OAuth token.
pub(crate) const OAUTH_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
/// JWT lifetime: Google accepts up to 3600s.
const JWT_LIFETIME_SECS: u64 = 3600;
/// Cache the OAuth access token until this margin before `expires_in`.
const TOKEN_CACHE_MARGIN: Duration = Duration::from_secs(60);
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Parsed Google service-account key file (the subset this adapter needs).
#[derive(Debug, Clone, Deserialize)]
pub struct ServiceAccountCredentials {
    pub client_email: String,
    pub private_key: String,
    pub token_uri: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
}

/// Read and parse a Google service-account JSON key file.
pub fn load_credentials(path: &Path) -> Result<ServiceAccountCredentials, EmbedError> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        EmbedError::Unavailable(format!(
            "cannot read service-account credentials {}: {e}",
            path.display()
        ))
    })?;
    serde_json::from_str(&raw).map_err(|e| {
        EmbedError::Unavailable(format!(
            "malformed service-account credentials {}: {e}",
            path.display()
        ))
    })
}

/// JWT claims carried by the service-account assertion.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Claims {
    iss: String,
    scope: String,
    aud: String,
    iat: u64,
    exp: u64,
}

/// Build a `reqwest::Client` with sensible timeouts.
pub(crate) fn build_client() -> Result<reqwest::Client, EmbedError> {
    reqwest::Client::builder()
        .connect_timeout(DEFAULT_CONNECT_TIMEOUT)
        .timeout(DEFAULT_REQUEST_TIMEOUT)
        .build()
        .map_err(|e| EmbedError::Unavailable(format!("failed to build HTTP client: {e}")))
}

/// Supplies the Bearer token for Vertex calls. Production uses mint + OAuth exchange;
/// tests inject a fake so `GeminiEmbedder` logic is exercisable offline.
#[async_trait]
pub trait GeminiTokenSource: Send + Sync + std::fmt::Debug {
    async fn access_token(&mut self) -> Result<String, EmbedError>;
}

/// Production token source: mint an RS256 JWT from the service-account private key,
/// exchange it for an OAuth `access_token`, and cache the token until `expires_in - 60s`.
#[derive(Debug)]
pub struct ServiceAccountTokenSource {
    client_email: String,
    private_key: String,
    token_uri: String,
    scope: String,
    client: reqwest::Client,
    cached: Option<(String, Instant)>,
}

impl ServiceAccountTokenSource {
    /// Build a token source from parsed service-account credentials.
    pub fn new(
        creds: ServiceAccountCredentials,
        client: reqwest::Client,
    ) -> Result<Self, EmbedError> {
        if creds.client_email.is_empty() {
            return Err(EmbedError::Unavailable(
                "service-account credentials missing client_email".into(),
            ));
        }
        if creds.private_key.is_empty() {
            return Err(EmbedError::Unavailable(
                "service-account credentials missing private_key".into(),
            ));
        }
        let token_uri = creds
            .token_uri
            .unwrap_or_else(|| DEFAULT_TOKEN_URI.to_string());
        Ok(Self {
            client_email: creds.client_email,
            private_key: creds.private_key,
            token_uri,
            scope: OAUTH_SCOPE.to_string(),
            client,
            cached: None,
        })
    }

    /// Mint the RS256 JWT assertion (exposed for tests). Signs the service-account
    /// private key (a malformed/unusable key is a permanent, operator-fixing `Backend`).
    pub fn mint_jwt(&self) -> Result<String, EmbedError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| EmbedError::Unavailable(format!("system clock before epoch: {e}")))?
            .as_secs();
        let claims = Claims {
            iss: self.client_email.clone(),
            scope: self.scope.clone(),
            aud: self.token_uri.clone(),
            iat: now,
            exp: now + JWT_LIFETIME_SECS,
        };
        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        let key =
            jsonwebtoken::EncodingKey::from_rsa_pem(self.private_key.as_bytes()).map_err(|e| {
                EmbedError::Backend(format!(
                    "failed to parse service-account private key PEM: {e}"
                ))
            })?;
        jsonwebtoken::encode(&header, &claims, &key)
            .map_err(|e| EmbedError::Backend(format!("failed to sign service-account JWT: {e}")))
    }
}

#[async_trait]
impl GeminiTokenSource for ServiceAccountTokenSource {
    async fn access_token(&mut self) -> Result<String, EmbedError> {
        if let Some((token, expires_at)) = &self.cached {
            if Instant::now() < *expires_at {
                return Ok(token.clone());
            }
        }
        let jwt = self.mint_jwt()?;
        let params = [
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", jwt.as_str()),
        ];
        let resp = self
            .client
            .post(&self.token_uri)
            .form(&params)
            .send()
            .await
            .map_err(|e| {
                EmbedError::Unavailable(format!("OAuth token endpoint unreachable: {e}"))
            })?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.map_err(|e| {
            EmbedError::Backend(format!(
                "OAuth token endpoint returned unparseable JSON: {e}"
            ))
        })?;
        if !status.is_success() {
            return Err(EmbedError::Backend(format!(
                "OAuth token endpoint returned {status}: {body}"
            )));
        }
        let token = body
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| EmbedError::Backend("OAuth token response missing access_token".into()))?
            .to_string();
        // TTL default 3600 when `expires_in` is absent; never cache for longer than it.
        let expires_in = body
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(3600);
        let ttl = expires_in
            .saturating_sub(TOKEN_CACHE_MARGIN.as_secs())
            .max(1);
        self.cached = Some((token.clone(), Instant::now() + Duration::from_secs(ttl)));
        Ok(token)
    }
}

/// Vertex `embedContent` response envelope.
#[derive(Debug, Deserialize)]
struct EmbedResponse {
    predictions: Vec<Prediction>,
}

#[derive(Debug, Deserialize)]
struct Prediction {
    embeddings: Embeddings,
}

#[derive(Debug, Deserialize)]
struct Embeddings {
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

    /// POST an embedContent request and parse a single prediction's values.
    ///
    /// CON-2: no retry. A rejected or malformed answer is returned as-is; the caller's
    /// degradation contract decides whether the write stays durable.
    async fn request_embedding(&self, text: &str) -> Result<EmbedResponse, EmbedError> {
        let token = {
            let mut guard = self.token_source.lock().await;
            guard.access_token().await?
        };
        let body = serde_json::json!({
            "content": { "content": text },
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
        let mut vec = response
            .predictions
            .into_iter()
            .next()
            .ok_or_else(|| EmbedError::Backend("Vertex returned no predictions".into()))?
            .embeddings
            .values;
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
pub(crate) const TEST_RSA_PRIVATE_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQDgTiqLX+Re051H
YBfWTwHojXANv7kFLmXZZtY6c+p5qO+TfIOWaocCH08zqzkMuEFf4wJ+HU2Zz7rt
EyNrsWuqIVGi8rDqQ9h//0w1pcbBrxH5qZzA4VGKMtwUowVsI7K31xsYq+4V0btq
BUn41iuC9DyTuguC9V/6a2GwfmqRGT80bOd+6HIINGFXpgm5n7D2jaHqx2MhLnle
WnZGIerDK2dmRQkMJY/HxhnzM+n2Q9FXEhPk0qJHb2Hzd0OCgMOZBE5x2zw0+HWI
WbAj3BK+N6IIJQQ31uNZF8PxyoLGw8v5+dez+9E1+63MVv3jEqvDyXLcttg6JH5P
sETfhndbAgMBAAECggEADpv4uGgv+x8kTMxM8SfnM2rW5AZbOiOp/Y1toZQALxla
NUx0U50vmutIINDjn9j2ZRTniihFcCGwBpXrBi4hmYyfARJ2hGOT285Ye9wGxIGv
FYg/De7+/RXP8MYnacIvdzra6HH2SVSGNOMQTNVCMz7OHT8OVeK+dBR/Ydvx++4+
Q4pkdzIbWNSTVlnGCEnWIJcYoW2Xlu55vy0VqBriZJjYJ7SHCFf5916mEHzhUk1d
NODePUX5IK5JW2W+4CaTDGV5djUZXr8CXZW5vVS2HkiYh0Xho3Lti1XajpHR7w0W
tZwFyOJBVhtSuOtt43O1SEnQpQ64r0+gi481SzsAxQKBgQD9+UfOiPlyBNqp8fmL
8DBSiWjJ/mFlPJptj5k2RGT5qD7fjnwZ/OrD+xfjWlA04U1FVsjYT4ldncHPkViu
qr/1taorMQpurAkBp23H9MUF4S8ieROA4ybke7GjcnimiysgzTIjVYbT1Hk7+2Td
KUDJHYUtNYjKPCEp/Hp77AkETQKBgQDiGEpfAVq2EZGpjCuseRt//H+4nYyGLtKO
/f6kyDhmHbLR/4HCmKj66yP6A9taNcICSSZ4en62EJlUB26pHsuteUlLIAk75ZKi
xdZyXHam963MbHf7+qmEzS6Y5nPs6dH1WeWT/Q5zW8W+9V7+GQ2bClubJ2mvOyNV
ksSAoDpeRwKBgQCMiWmTvzYRQuBhFBYbupByy7ihtdLdO1jU8ZY9ckFR6SjJekXv
94VNZ1+DnlEtwdKJYQmIsRJ5LDe4DVy+YpwQcjM07VExhp8BPE3CTQ7NPxte/xKs
yoWV/2B/6nMa7X2zC/kHlmciRrvDVkwtGYvQ/jXYm3wTNIzBeAWrFySyLQKBgBC2
EukqxHWonseVYLUCzpGLLDWND5HrbAy9oVC0q9aAY3M6G3Eyr2q8bpBQMKpeRtS8
a2eERlFWsL6RPhCqAgv0ZwJyf7w5n7kAPnV9eBenPuVZLxUk1drG/6a1geQE9Eva
NSnXDnZgViFjKX5Gg8bt4Q96vkkBaf8tNfD75tSJAoGBAPnbWbn/2JGSoDBBpbxF
QIunSRniqPIDbWbI2npY3DS7/2MnpyADY6GByjvZ19Lk1yhtO0/WEMNMKl+rsaNO
QDEwl2KdTKA4jCDdjj6tLKZ88NpSvYY/iJQVT8RM0ciJ76wbtYudZTBfEt7OosA6
mDN1kibH7c0cAYkeC2hN6nAG
-----END PRIVATE KEY-----";

#[cfg(test)]
pub(crate) const TEST_RSA_PUBLIC_KEY_PEM: &str = "-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA4E4qi1/kXtOdR2AX1k8B
6I1wDb+5BS5l2WbWOnPqeajvk3yDlmqHAh9PM6s5DLhBX+MCfh1Nmc+67RMja7Fr
qiFRovKw6kPYf/9MNaXGwa8R+amcwOFRijLcFKMFbCOyt9cbGKvuFdG7agVJ+NYr
gvQ8k7oLgvVf+mthsH5qkRk/NGznfuhyCDRhV6YJuZ+w9o2h6sdjIS55Xlp2RiHq
wytnZkUJDCWPx8YZ8zPp9kPRVxIT5NKiR29h83dDgoDDmQROcds8NPh1iFmwI9wS
vjeiCCUEN9bjWRfD8cqCxsPL+fnXs/vRNfutzFb94xKrw8ly3LbYOiR+T7BE34Z3
WwIDAQAB
-----END PUBLIC KEY-----";

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    fn sample_credentials(token_uri: &str) -> ServiceAccountCredentials {
        ServiceAccountCredentials {
            client_email: "sa@example.com".to_string(),
            private_key: TEST_RSA_PRIVATE_KEY_PEM.to_string(),
            token_uri: Some(token_uri.to_string()),
            project_id: Some("proj".to_string()),
        }
    }

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

    #[test]
    fn mints_and_verifies_service_account_jwt() {
        let src = ServiceAccountTokenSource::new(
            sample_credentials(DEFAULT_TOKEN_URI),
            reqwest::Client::new(),
        )
        .unwrap();
        let jwt = src.mint_jwt().unwrap();
        let header = jsonwebtoken::decode_header(&jwt).unwrap();
        assert_eq!(header.alg, jsonwebtoken::Algorithm::RS256);
        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
        validation.validate_aud = false; // aud is a URI; strict aud matching is not needed
        validation.set_required_spec_claims(&["iss", "exp"]);
        let key =
            jsonwebtoken::DecodingKey::from_rsa_pem(TEST_RSA_PUBLIC_KEY_PEM.as_bytes()).unwrap();
        let data = jsonwebtoken::decode::<Claims>(&jwt, &key, &validation).unwrap();
        assert_eq!(data.claims.iss, "sa@example.com");
        assert_eq!(data.claims.aud, DEFAULT_TOKEN_URI);
        assert_eq!(data.claims.exp - data.claims.iat, JWT_LIFETIME_SECS);
    }

    #[tokio::test]
    async fn exchanges_jwt_for_access_token_and_caches() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/token")
                .body_contains("grant_type=")
                .body_contains("assertion=");
            then.status(200)
                .json_body(serde_json::json!({ "access_token": "tok-1", "expires_in": 3600 }));
        });
        let creds = sample_credentials(&format!("{}/token", server.base_url()));
        let mut src = ServiceAccountTokenSource::new(creds, reqwest::Client::new()).unwrap();
        let t1 = src.access_token().await.unwrap();
        assert_eq!(t1, "tok-1");
        // Second call hits the cache: the endpoint mock must have seen exactly one request.
        let t2 = src.access_token().await.unwrap();
        assert_eq!(t2, "tok-1");
        mock.assert_hits(1);
    }

    #[tokio::test]
    async fn token_endpoint_transport_failure_is_unavailable() {
        let creds = sample_credentials("http://127.0.0.1:9/token");
        let mut src = ServiceAccountTokenSource::new(creds, reqwest::Client::new()).unwrap();
        let err = src.access_token().await.unwrap_err();
        assert!(matches!(err, EmbedError::Unavailable(_)), "{err:?}");
    }

    #[tokio::test]
    async fn token_endpoint_http_error_is_backend() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/token");
            then.status(401)
                .json_body(serde_json::json!({ "error": "invalid_grant" }));
        });
        let creds = sample_credentials(&format!("{}/token", server.base_url()));
        let mut src = ServiceAccountTokenSource::new(creds, reqwest::Client::new()).unwrap();
        let err = src.access_token().await.unwrap_err();
        assert!(matches!(err, EmbedError::Backend(_)), "{err:?}");
    }

    #[tokio::test]
    async fn embeds_and_normalizes() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/embedContent")
                .header("Authorization", "Bearer tok")
                // A3-R1-3: pin the request body so the outputDimensionality omission
                // (A3-R1-1) cannot silently regress.
                .body_contains("\"outputDimensionality\":768");
            then.status(200).json_body(serde_json::json!({
                "predictions": [ { "embeddings": { "statistics": {}, "values": sample_embedding() } } ]
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
    async fn missing_prediction_is_backend() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/embedContent");
            then.status(200)
                .json_body(serde_json::json!({ "predictions": [] }));
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
                "predictions": [ { "embeddings": { "statistics": {}, "values": vec![1.0; 512] } } ]
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
}
