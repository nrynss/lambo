//! BGE-M3 embeddings served by a local llama.cpp server (default production path).
//!
//! Speaks llama.cpp's OpenAI-compatible `POST /v1/embeddings` endpoint, which is the
//! most version-stable surface across llama.cpp releases. Returns 1024-d dense vectors,
//! L2-normalized before returning so Cockroach `<->` (L2) rankings stay coherent with
//! cosine similarity (see `notes/embeddings-portable.md`).
//!
//! This backend is selected when `LAMBO_EMBEDDER=bge_m3` (the default).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

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
    /// Expected embedding dimensionality (must equal the store's `VECTOR(1024)`).
    dim: usize,
}

impl BgeM3LlamaCppEmbedder {
    /// Build an embedder for a llama.cpp server.
    ///
    /// * `llama_url` - base URL, e.g. `http://127.0.0.1:8080` (the `v1/embeddings` and
    ///   `health` paths are appended automatically).
    /// * `model` - model id sent in the request; pass `""` to let the server use its
    ///   default model.
    /// * `dim` - expected dimensionality (must be 1024 for Cockroach `VECTOR(1024)`).
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
            return Err(EmbedError::Unavailable(
                "embedding dimension must be > 0".into(),
            ));
        }
        Ok(Self {
            client: reqwest::Client::new(),
            url: format!("{base_url}/v1/embeddings"),
            base_url,
            model: model.into(),
            dim,
        })
    }

    /// Report the server health without embedding anything.
    pub async fn check_health(&self) -> Result<(), EmbedError> {
        let resp = self
            .client
            .get(format!("{}/health", self.base_url))
            .send()
            .await
            .map_err(|e| EmbedError::Backend(format!("llama.cpp health check failed: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            return Err(EmbedError::Backend(format!(
                "llama.cpp health check returned {status}"
            )));
        }
        Ok(())
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
        let body = EmbedRequest {
            model: self.model.clone(),
            input: text.to_string(),
        };
        let resp = self
            .client
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .map_err(|e| EmbedError::Backend(format!("llama.cpp request failed: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(EmbedError::Backend(format!(
                "llama.cpp returned {status}: {body}"
            )));
        }
        let parsed: EmbedResponse = resp.json().await.map_err(|e| {
            EmbedError::Backend(format!("llama.cpp returned unparseable JSON: {e}"))
        })?;
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

    #[tokio::test]
    async fn embeds_and_normalizes() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/embeddings")
                .json_body_partial(r#"{ "input": "user schema" }"#);
            then.status(200).json_body(serde_json::json!({
                "object": "list",
                "data": [{ "object": "embedding", "index": 0, "embedding": sample_embedding() }],
                "model": "bge-m3",
                "usage": { "prompt_tokens": 2, "total_tokens": 2 }
            }));
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
            then.status(200).json_body(serde_json::json!({
                "data": [{ "embedding": vec![1.0; 512] }]
            }));
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
            then.status(200).json_body(serde_json::json!({
                "data": [{ "embedding": vec![f32::NAN; 1024] }]
            }));
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
    async fn constructor_validates_inputs() {
        assert!(matches!(
            BgeM3LlamaCppEmbedder::new("", "", 1024),
            Err(EmbedError::Unavailable(_))
        ));
        assert!(matches!(
            BgeM3LlamaCppEmbedder::new("http://127.0.0.1:8080", "", 0),
            Err(EmbedError::Unavailable(_))
        ));
    }
}
