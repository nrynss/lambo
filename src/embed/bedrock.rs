//! Amazon Titan Text Embeddings V2 on Bedrock (`embed-bedrock`).
//!
//! Skeletal adapter for issue #3 / T7.1. The type is constructible and the
//! request/response shape is locked against the T0.4 spike. `embed` does not
//! call InvokeModel yet: the account is not AUTHORIZED
//! (`evidence/bedrock-blocked.txt`). When access lands, replace
//! [`BedrockTitanEmbedder::invoke`] with the SDK call. Do not add this feature
//! to `ship` until that happens.
//!
//! Titan V2 widths are 256, 512, and 1024. 1024 matches the Cockroach
//! `VECTOR(1024)` column. Any other width is a re-embed, not a config flip.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::{EmbedError, Embedder};

/// Titan Text Embeddings V2 model id. Session `EmbeddingContract.model`.
pub const TITAN_V2_MODEL_ID: &str = "amazon.titan-embed-text-v2:0";

/// Widths Titan V2 will emit. 1024 is the store-compatible default.
pub const TITAN_V2_DIMS: &[usize] = &[256, 512, 1024];

const DEFAULT_REGION: &str = "us-east-1";

#[derive(Debug, Serialize)]
struct TitanRequest<'a> {
    #[serde(rename = "inputText")]
    input_text: &'a str,
    dimensions: usize,
    normalize: bool,
}

#[derive(Debug, Deserialize)]
struct TitanResponse {
    embedding: Vec<f32>,
}

/// Titan V2 embedder. Constructible behind `embed-bedrock`; invoke is skeletal.
#[derive(Debug, Clone)]
pub struct BedrockTitanEmbedder {
    region: String,
    model_id: String,
    dim: usize,
}

impl BedrockTitanEmbedder {
    /// Build a Titan V2 embedder for `region` at one of [`TITAN_V2_DIMS`].
    pub fn new(region: impl Into<String>, dim: usize) -> Result<Self, EmbedError> {
        if !TITAN_V2_DIMS.contains(&dim) {
            return Err(EmbedError::Unavailable(format!(
                "Titan V2 dim must be one of {TITAN_V2_DIMS:?}, got {dim}"
            )));
        }
        let region = region.into().trim().to_string();
        if region.is_empty() {
            return Err(EmbedError::Unavailable("Bedrock region is empty".into()));
        }
        Ok(Self {
            region,
            model_id: TITAN_V2_MODEL_ID.to_string(),
            dim,
        })
    }

    /// Region from `LAMBO_BEDROCK_REGION`, then `AWS_REGION`, else `us-east-1`.
    /// Blank values are skipped so a whitespace override cannot hide `AWS_REGION`.
    pub fn from_config_dim(dim: usize) -> Result<Self, EmbedError> {
        let region = env_nonblank("LAMBO_BEDROCK_REGION")
            .or_else(|| env_nonblank("AWS_REGION"))
            .unwrap_or_else(|| DEFAULT_REGION.to_string());
        Self::new(region, dim)
    }

    /// Titan model id stamped on the session embedding contract.
    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    /// Resolved Bedrock Runtime region.
    pub fn region(&self) -> &str {
        &self.region
    }

    fn encode_request(&self, text: &str) -> Result<Vec<u8>, EmbedError> {
        serde_json::to_vec(&TitanRequest {
            input_text: text,
            dimensions: self.dim,
            normalize: true,
        })
        .map_err(|e| EmbedError::Backend(format!("Titan request encode failed: {e}")))
    }

    fn decode_response(&self, bytes: &[u8]) -> Result<Vec<f32>, EmbedError> {
        let parsed: TitanResponse = serde_json::from_slice(bytes).map_err(|e| {
            EmbedError::Backend(format!("Titan response is not valid JSON: {e}"))
        })?;
        let mut vec = parsed.embedding;
        if vec.len() != self.dim {
            return Err(EmbedError::Backend(format!(
                "Titan returned {} dims, expected {}",
                vec.len(),
                self.dim
            )));
        }
        l2_check_and_normalize(&mut vec)?;
        Ok(vec)
    }

    /// Wire InvokeModel here when the account is AUTHORIZED. Until then this
    /// is the only live path and it fail-closes. No silent fixture fallback.
    async fn invoke(&self, _text: &str) -> Result<Vec<u8>, EmbedError> {
        let _ = &self.region;
        let _ = &self.model_id;
        Err(EmbedError::Unavailable(format!(
            "Bedrock InvokeModel is not live: account model access is still blocked \
             (issue #3, model {}, region {}). See evidence/bedrock-blocked.txt",
            self.model_id, self.region
        )))
    }
}

fn env_nonblank(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn l2_check_and_normalize(v: &mut [f32]) -> Result<(), EmbedError> {
    let mut sum = 0.0f64;
    for &x in v.iter() {
        if !x.is_finite() {
            return Err(EmbedError::Backend(
                "Titan returned a non-finite embedding".into(),
            ));
        }
        let x = f64::from(x);
        sum += x * x;
    }
    let norm = sum.sqrt();
    if !norm.is_finite() || norm <= f64::from(f32::EPSILON) {
        return Err(EmbedError::Backend(
            "Titan returned a zero-norm embedding".into(),
        ));
    }
    for x in v.iter_mut() {
        *x = (f64::from(*x) / norm) as f32;
    }
    Ok(())
}

#[async_trait]
impl Embedder for BedrockTitanEmbedder {
    fn dimensions(&self) -> usize {
        self.dim
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        if text.trim().is_empty() {
            return Err(EmbedError::Unavailable(
                "cannot embed empty/whitespace text".into(),
            ));
        }
        let bytes = self.invoke(text).await?;
        self.decode_response(&bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsupported_dim() {
        let err = BedrockTitanEmbedder::new("us-east-1", 768).unwrap_err();
        assert!(err.to_string().contains("256"), "{err}");
    }

    #[test]
    fn accepts_titan_widths() {
        for dim in TITAN_V2_DIMS {
            let e = BedrockTitanEmbedder::new("us-east-1", *dim).unwrap();
            assert_eq!(e.dimensions(), *dim);
            assert_eq!(e.model_id(), TITAN_V2_MODEL_ID);
        }
    }

    #[test]
    fn rejects_empty_region() {
        assert!(BedrockTitanEmbedder::new("  ", 1024).is_err());
    }

    #[test]
    fn blank_bedrock_region_falls_back_to_aws_region() {
        let _g = crate::test_util::env_lock();
        std::env::set_var("LAMBO_BEDROCK_REGION", "  ");
        std::env::set_var("AWS_REGION", "eu-west-1");
        let e = BedrockTitanEmbedder::from_config_dim(1024).unwrap();
        assert_eq!(e.region(), "eu-west-1");
        std::env::remove_var("LAMBO_BEDROCK_REGION");
        std::env::remove_var("AWS_REGION");
    }

    #[test]
    fn normalize_large_finite_values() {
        let mut v = vec![f32::MAX, f32::MAX];
        l2_check_and_normalize(&mut v).unwrap();
        assert!(v.iter().all(|x| x.is_finite()));
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((n - 1.0).abs() < 1e-5);
    }

    #[test]
    fn encode_request_shape() {
        let e = BedrockTitanEmbedder::new("us-east-1", 1024).unwrap();
        let raw = e.encode_request("user schema").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["inputText"], "user schema");
        assert_eq!(v["dimensions"], 1024);
        assert_eq!(v["normalize"], true);
    }

    #[test]
    fn decode_accepts_and_normalizes() {
        let e = BedrockTitanEmbedder {
            region: "us-east-1".into(),
            model_id: TITAN_V2_MODEL_ID.into(),
            dim: 4,
        };
        let body = serde_json::json!({ "embedding": [3.0, 0.0, 0.0, 0.0] });
        let got = e.decode_response(body.to_string().as_bytes()).unwrap();
        assert_eq!(got.len(), 4);
        assert!((got[0] - 1.0).abs() < 1e-5);
    }

    #[test]
    fn decode_rejects_wrong_width() {
        let e = BedrockTitanEmbedder::new("us-east-1", 1024).unwrap();
        let body = serde_json::json!({ "embedding": [1.0, 0.0] });
        let err = e.decode_response(body.to_string().as_bytes()).unwrap_err();
        assert!(err.to_string().contains("dims"), "{err}");
    }

    #[test]
    fn normalize_rejects_non_finite() {
        let mut v = vec![1.0, f32::NAN];
        let err = l2_check_and_normalize(&mut v).unwrap_err();
        assert!(err.to_string().contains("non-finite"), "{err}");
    }

    #[tokio::test]
    async fn rejects_empty_text_before_invoke() {
        let e = BedrockTitanEmbedder::new("us-east-1", 1024).unwrap();
        for text in ["", "   ", "\t\n"] {
            let err = e.embed(text).await.unwrap_err();
            assert!(
                matches!(err, EmbedError::Unavailable(ref m) if m.contains("empty")),
                "{err:?}"
            );
        }
    }

    #[tokio::test]
    async fn embed_fail_closes_until_authorized() {
        let e = BedrockTitanEmbedder::new("us-east-1", 1024).unwrap();
        let err = e.embed("user schema").await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("InvokeModel") || msg.contains("blocked"), "{msg}");
        assert!(!msg.to_ascii_lowercase().contains("fixture"));
    }
}
