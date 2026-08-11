//! BGE-M3 embeddings served by a local llama.cpp server (default production path).
//!
//! Speaks llama.cpp's OpenAI-compatible `POST /v1/embeddings` endpoint, which is the
//! most version-stable surface across llama.cpp releases. Returns dense vectors that are
//! L2-normalized before returning so Cockroach `<->` (L2) rankings stay coherent with
//! cosine similarity (see `notes/embeddings-portable.md`).
//!
//! This backend is selected when `LAMBO_EMBEDDER=bge_m3` (the default).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use super::{EmbedError, Embedder};

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

/// BGE-M3 embeddings via a local llama.cpp server over HTTP.
#[derive(Debug, Clone)]
pub struct BgeM3LlamaCppEmbedder {
    client: reqwest::Client,
    /// Full embed endpoint URL, e.g. `http://127.0.0.1:8080/v1/embeddings`.
    url: String,
    /// Base URL for `/health`, e.g. `http://127.0.0.1:8080`.
    base_url: String,
    /// Model id sent in the request (empty => server default).
    model: String,
    /// Expected embedding dimensionality (must match server output and store schema).
    dim: usize,
}

fn build_client(connect: Duration, request: Duration) -> Result<reqwest::Client, EmbedError> {
    reqwest::Client::builder()
        .connect_timeout(connect)
        .timeout(request)
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
        Ok(Self {
            client: build_client(DEFAULT_CONNECT_TIMEOUT, DEFAULT_REQUEST_TIMEOUT)?,
            url: format!("{base_url}/v1/embeddings"),
            base_url,
            model: model.into(),
            dim,
        })
    }

    /// Override connect/request timeouts (most users can rely on the defaults).
    pub fn with_timeouts(
        mut self,
        connect: Duration,
        request: Duration,
    ) -> Result<Self, EmbedError> {
        self.client = build_client(connect, request)?;
        Ok(self)
    }

    /// Report the server health without embedding anything.
    pub async fn check_health(&self) -> Result<(), EmbedError> {
        let resp = self
            .client
            .get(format!("{}/health", self.base_url))
            .timeout(HEALTH_TIMEOUT)
            .send()
            .await
            .map_err(|e| EmbedError::Unavailable(format!("llama.cpp health check failed: {e}")))?;
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
    async fn request_embedding(
        &self,
        model: &str,
        text: &str,
    ) -> Result<EmbedResponse, EmbedError> {
        let mut model = model.to_string();
        let mut retried = false;
        loop {
            let body = EmbedRequest {
                model: model.clone(),
                input: text.to_string(),
            };
            let resp = self
                .client
                .post(&self.url)
                .json(&body)
                .send()
                .await
                .map_err(|e| EmbedError::Unavailable(format!("llama.cpp unreachable: {e}")))?;
            let status = resp.status();
            if status.is_success() {
                return resp.json().await.map_err(|e| {
                    EmbedError::Backend(format!("llama.cpp returned unparseable JSON: {e}"))
                });
            }
            let text_body = resp.text().await.unwrap_or_default();
            // A 400 on /v1/embeddings usually means the requested model id isn't loaded.
            // Retry once against the server's default model to be robust to a mismatch.
            if !model.is_empty() && status == reqwest::StatusCode::BAD_REQUEST && !retried {
                retried = true;
                model.clear();
                continue;
            }
            return Err(EmbedError::Backend(format!(
                "llama.cpp returned {status}: {text_body}"
            )));
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

    #[tokio::test]
    async fn rejects_non_success_status() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(500).body("internal error");
        });
        let e = BgeM3LlamaCppEmbedder::new(server.base_url(), "", 1024).unwrap();
        let err = e.embed("anything").await.unwrap_err();
        assert!(matches!(err, EmbedError::Backend(_)));
        assert!(err.to_string().contains("500"));
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
    async fn retries_without_model_on_400_model_not_loaded() {
        let server = MockServer::start();
        // First request carries a model id and 400s (model not loaded).
        server.mock(|when, then| {
            when.method(POST).path("/v1/embeddings").matches(|r| {
                let body = r.body.as_deref().unwrap_or(&[]);
                String::from_utf8_lossy(body).contains("\"model\":\"my-model\"")
            });
            then.status(400).body("model 'my-model' not loaded");
        });
        // Retried request omits model -> 200.
        let ok = server.mock(|when, then| {
            when.method(POST).path("/v1/embeddings").matches(|r| {
                let body = r.body.as_deref().unwrap_or(&[]);
                !String::from_utf8_lossy(body).contains("\"model\"")
            });
            then.status(200).json_body(ok_response());
        });
        let e = BgeM3LlamaCppEmbedder::new(server.base_url(), "my-model", 1024).unwrap();
        let v = e.embed("anything").await.unwrap();
        assert_eq!(v.len(), 1024);
        ok.assert();
    }

    #[tokio::test]
    async fn no_retry_loop_when_model_empty() {
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

    /// Live smoke test against a running llama.cpp server (`./scripts/run-llama-embed.sh`).
    /// Gated so CI (no server) never runs it.
    #[tokio::test]
    #[ignore]
    async fn live_smoke_against_llama_server() {
        let url = std::env::var("LAMBO_LLAMA_EMBED_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());
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
        // Paraphrases should be nearer than an unrelated domain (semantic sanity,
        // not a hard threshold):
        let sim_a_b = crate::embed::cosine(&a, &b);
        let sim_a_far = crate::embed::cosine(&a, &far);
        assert!(
            sim_a_b > sim_a_far,
            "near {sim_a_b:.3} should exceed far {sim_a_far:.3}"
        );
        eprintln!("BGE-M3 live: near={sim_a_b:.4} far={sim_a_far:.4} dim=1024");
    }
}
