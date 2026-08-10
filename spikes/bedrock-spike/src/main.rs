//! T0.4 — Bedrock Titan Text Embeddings V2 smoke.
//!
//! Auth (first match wins):
//! 1. `AWS_BEARER_TOKEN_BEDROCK` / `LAMBO_BEDROCK_API_KEY` — HTTP Bearer (Bedrock API key)
//! 2. Default AWS credential chain (`aws login` / env keys) via SDK
//!
//! Confirms model id, 1024 dims, request/response shape, region, latency.

use anyhow::{bail, Context, Result};
use aws_sdk_bedrockruntime::primitives::Blob;
use serde_json::{json, Value};
use std::env;
use std::time::Instant;

const MODEL_ID: &str = "amazon.titan-embed-text-v2:0";
const DIMS: usize = 1024;

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::from_filename("../../.env");
    let _ = dotenvy::dotenv();

    let region = env::var("LAMBO_BEDROCK_REGION")
        .or_else(|_| env::var("AWS_REGION"))
        .unwrap_or_else(|_| "us-east-1".to_string());

    let api_key = env::var("AWS_BEARER_TOKEN_BEDROCK")
        .or_else(|_| env::var("LAMBO_BEDROCK_API_KEY"))
        .ok()
        .filter(|s| !s.is_empty());

    println!("=== T0.4 Bedrock Titan V2 spike ===");
    println!("region: {region}");
    println!("model:  {MODEL_ID}");
    println!(
        "auth:   {}",
        if api_key.is_some() {
            "Bedrock API key (Bearer)"
        } else {
            "AWS default credential chain"
        }
    );

    let body = json!({
        "inputText": "user schema",
        "dimensions": DIMS,
        "normalize": true,
    });
    println!("request body: {body}");

    let t0 = Instant::now();
    let bytes = if let Some(key) = api_key {
        invoke_with_bearer(&region, &key, &body).await?
    } else {
        invoke_with_sdk(&region, &body).await?
    };
    let latency = t0.elapsed();

    let parsed: Value = serde_json::from_slice(&bytes).context("parse response JSON")?;
    let embedding = parsed
        .get("embedding")
        .and_then(|v| v.as_array())
        .context("response missing embedding array")?;

    if embedding.len() != DIMS {
        bail!("expected {DIMS} dims, got {}", embedding.len());
    }

    let sample: Vec<f64> = embedding
        .iter()
        .take(4)
        .filter_map(|v| v.as_f64())
        .collect();

    let keys: Vec<&str> = parsed
        .as_object()
        .map(|m| m.keys().map(|k| k.as_str()).collect())
        .unwrap_or_default();

    println!("latency: {latency:?}");
    println!("response keys: {keys:?}");
    println!("dims: {}", embedding.len());
    println!("embedding[0..4]: {sample:?}");
    println!("inputTextTokenCount: {:?}", parsed.get("inputTextTokenCount"));
    println!();
    println!("=== VERDICT: OK ===");
    println!("model_id={MODEL_ID}");
    println!("region={region}");
    println!("dims={DIMS}");
    println!("normalize=true in request");
    Ok(())
}

async fn invoke_with_bearer(region: &str, api_key: &str, body: &Value) -> Result<Vec<u8>> {
    // https://docs.aws.amazon.com/bedrock/latest/userguide/api-keys-use.html
    let url = format!(
        "https://bedrock-runtime.{region}.amazonaws.com/model/{MODEL_ID}/invoke"
    );
    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {api_key}"))
        .json(body)
        .send()
        .await
        .context("HTTP POST InvokeModel (bearer)")?;

    let status = resp.status();
    let bytes = resp.bytes().await.context("read response body")?.to_vec();
    if !status.is_success() {
        let text = String::from_utf8_lossy(&bytes);
        bail!("InvokeModel HTTP {status}: {text}");
    }
    Ok(bytes)
}

async fn invoke_with_sdk(region: &str, body: &Value) -> Result<Vec<u8>> {
    let conf = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(region.to_string()))
        .load()
        .await;
    let client = aws_sdk_bedrockruntime::Client::new(&conf);
    let body_bytes = serde_json::to_vec(body)?;
    let resp = client
        .invoke_model()
        .model_id(MODEL_ID)
        .content_type("application/json")
        .accept("application/json")
        .body(Blob::new(body_bytes))
        .send()
        .await
        .context("invoke_model Titan V2 (SDK)")?;
    Ok(resp.body.into_inner())
}
