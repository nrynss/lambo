//! Minimal Google auth for the PostgreSQL store's shared-service-account path.
//!
//! The Mooshik "shared-SA" design uses ONE GCP service account for both the Gemini
//! embedder (Vertex) and the Postgres store (Cloud SQL IAM database authentication).
//! This module is the store's side: it reads the same credential file the embedder
//! uses (`GCP_LAMBO_CREDENTIALS`, falling back to `GOOGLE_APPLICATION_CREDENTIALS`)
//! and mints a fresh OAuth access token whose scopes include `sqlservice.login`,
//! which Cloud SQL IAM database auth requires. The store presents that token as the
//! database password to the Cloud SQL endpoint (or the Auth Proxy).
//!
//! Gate: `store-postgres` (only that feature needs it). The embedder keeps its own
//! token logic in `embed/gemini.rs`; consolidating the two behind one shared module
//! is a noted follow-up; the shared identity (one credential file) already holds.

use serde::Deserialize;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// OAuth token endpoint Google's own JSON usually names.
const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
/// Scope set that covers both an IAM database login and Vertex.
const IAM_DB_SCOPES: &str =
    "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/sqlservice.login";
/// JWT lifetime Google accepts.
const JWT_LIFETIME_SECS: u64 = 3600;
/// Cache an access token until this margin before `expires_in`.
const TOKEN_CACHE_MARGIN: Duration = Duration::from_secs(60);

/// Parsed Google credentials (the subset this module needs), mirroring the embedder.
#[derive(Debug, Clone, Deserialize)]
struct ServiceAccountKey {
    client_email: String,
    private_key: String,
    #[serde(default)]
    token_uri: Option<String>,
}

/// A token source that mints an OAuth access token from the shared credential file.
#[derive(Debug)]
pub struct CloudSqlTokenSource {
    key: ServiceAccountKey,
    token_uri: String,
    client: reqwest::Client,
    cached: Option<(String, Instant)>,
}

impl CloudSqlTokenSource {
    /// Build from the shared credential file. Returns `None` (not an error) when the
    /// file is absent, so a non-IAM deployment is untouched.
    pub fn from_env() -> Option<Result<Self, String>> {
        let path = std::env::var_os("GCP_LAMBO_CREDENTIALS")
            .or_else(|| std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS"))?;
        Some(Self::from_path(std::path::Path::new(&path)))
    }

    /// Build from an explicit credentials file path.
    pub fn from_path(path: &std::path::Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read Google credentials {}: {e}", path.display()))?;
        let key: ServiceAccountKey = serde_json::from_str(&raw)
            .map_err(|e| format!("malformed Google credentials {}: {e}", path.display()))?;
        if key.client_email.is_empty() || key.private_key.is_empty() {
            return Err("Google credentials missing client_email or private_key".into());
        }
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| format!("failed to build HTTP client: {e}"))?;
        Ok(Self {
            token_uri: key
                .token_uri
                .clone()
                .unwrap_or_else(|| DEFAULT_TOKEN_URI.to_string()),
            key,
            client,
            cached: None,
        })
    }

    /// A fresh access token with the IAM-database scope set, caching until expiry.
    pub async fn access_token(&mut self) -> Result<String, String> {
        if let Some((token, expires_at)) = &self.cached {
            if Instant::now() < *expires_at {
                return Ok(token.clone());
            }
        }
        // RS256 JWT assertion for the service account.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| format!("system clock before epoch: {e}"))?
            .as_secs();
        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        let claims = serde_json::json!({
            "iss": self.key.client_email,
            "scope": IAM_DB_SCOPES,
            "aud": self.token_uri,
            "iat": now,
            "exp": now + JWT_LIFETIME_SECS,
        });
        let key = jsonwebtoken::EncodingKey::from_rsa_pem(self.key.private_key.as_bytes())
            .map_err(|e| format!("failed to parse service-account private key PEM: {e}"))?;
        let jwt = jsonwebtoken::encode(&header, &claims, &key)
            .map_err(|e| format!("failed to sign service-account JWT: {e}"))?;

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
            .map_err(|e| format!("OAuth token endpoint unreachable: {e}"))?;
        let status = resp.status();
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("OAuth token endpoint returned unparseable JSON: {e}"))?;
        if !status.is_success() {
            return Err(format!("OAuth token endpoint returned {status}: {body}"));
        }
        let token = body
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "OAuth token response missing access_token".to_string())?
            .to_string();
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
