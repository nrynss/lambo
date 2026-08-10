//! T0.4 — Bedrock Titan Text Embeddings V2 smoke.
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

    // Prefer explicit Bedrock region override; else AWS_REGION; else us-east-1 for model access.
    let region = env::var("LAMBO_BEDROCK_REGION")
        .or_else(|_| env::var("AWS_REGION"))
        .unwrap_or_else(|_| "us-east-1".to_string());

    println!("=== T0.4 Bedrock Titan V2 spike ===");
    println!("region: {region}");
    println!("model:  {MODEL_ID}");

    let conf = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(region.clone()))
        .load()
        .await;
    let client = aws_sdk_bedrockruntime::Client::new(&conf);

    let body = json!({
        "inputText": "user schema",
        "dimensions": DIMS,
        "normalize": true,
    });
    let body_bytes = serde_json::to_vec(&body)?;
    println!("request body: {body}");

    let t0 = Instant::now();
    let resp = client
        .invoke_model()
        .model_id(MODEL_ID)
        .content_type("application/json")
        .accept("application/json")
        .body(Blob::new(body_bytes))
        .send()
        .await
        .context("invoke_model Titan V2")?;
    let latency = t0.elapsed();

    let bytes = resp.body.into_inner();
    let parsed: Value = serde_json::from_slice(&bytes).context("parse response JSON")?;
    let embedding = parsed
        .get("embedding")
        .and_then(|v| v.as_array())
        .context("response missing embedding array")?;

    if embedding.len() != DIMS {
        bail!("expected {DIMS} dims, got {}", embedding.len());
    }

    // Sample first few components
    let sample: Vec<f64> = embedding
        .iter()
        .take(4)
        .filter_map(|v| v.as_f64())
        .collect();

    // Response keys (shape note for T7.1)
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
