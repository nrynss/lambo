//! Shared Google OAuth for every adapter that authenticates as a Google principal.
//!
//! Lambo talks to two Google services with **one identity**: the Gemini embedder calls
//! Vertex, and the Postgres store logs in to Cloud SQL with IAM database authentication.
//! Both read the same credential file (`GCP_LAMBO_CREDENTIALS`, falling back to
//! `GOOGLE_APPLICATION_CREDENTIALS`) and both exchange it for an OAuth access token. This
//! module is that exchange, once.
//!
//! It was two implementations until 2026-08-24: the embedder's in `embed/gemini.rs` and a
//! service-account-only copy here for the store. The copy was not merely duplication, it was
//! **wrong for the machine it ran on**: it parsed service-account keys only, so a host whose
//! credential is an authorized-user ADC file (which is what `gcloud auth
//! application-default login` writes, and what the live Vertex verification actually ran
//! with) could reach Vertex and could not reach Cloud SQL. One module, both grant types,
//! both consumers.
//!
//! **Grants.** A service-account key mints an RS256 JWT assertion
//! (`{iss, scope, aud, iat, exp}`) and exchanges it with the `jwt-bearer` grant. An
//! authorized-user ADC file uses the `refresh_token` grant. Either way the caller gets a
//! Bearer token cached until roughly a minute before `expires_in`.
//!
//! **Scopes are the caller's, on both grants.** Vertex wants `cloud-platform`; a Cloud SQL
//! IAM login wants `cloud-platform` plus `sqlservice.login`. The token source takes the
//! scope string rather than assuming one, and sends it on the wire either way: as the
//! signed `scope` claim in the JWT assertion, and as a `scope` form field on the
//! `refresh_token` grant (RFC 6749 section 6, where it is a **narrowing** request). So
//! neither consumer silently borrows the other's authority.
//!
//! The two grants differ in what a scope can reach, and the difference is the operator's
//! to know. A service-account key can ask for any scope the SA's IAM roles allow. An
//! authorized-user ADC can only ask for a subset of what it was granted at
//! `gcloud auth application-default login` time; asking for more is answered
//! `invalid_scope` by the token endpoint, which this module classifies
//! [`GoogleAuthError::Backend`]. That is the loud failure worth having: an ADC minted
//! without `sqlservice.login` fails at the token endpoint, naming the scope, instead of
//! being refused later by Postgres as opaque authentication noise. Mint one with
//! `gcloud auth application-default login --scopes=<cloud-platform>,<sqlservice.login>`
//! when the default set does not carry it.
//!
//! **Error classification is preserved across both consumers.** Transport failures are
//! [`GoogleAuthError::Unavailable`] (the caller may degrade); a non-2xx from the token
//! endpoint, or a private key that will not parse, is [`GoogleAuthError::Backend`]
//! (permanent, the operator fixes it). `embed/gemini.rs` maps these one-for-one onto
//! `EmbedError` so A3's degradation contract is unchanged.
//!
//! Gate: `embed-gemini` or `store-postgres`. Both enable the `reqwest` and `jsonwebtoken`
//! dependencies this module needs; neither is in the default feature set.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Fallback OAuth token endpoint when the credential JSON omits `token_uri`.
pub const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
/// Cloud-platform scope: what Vertex needs.
pub const SCOPE_CLOUD_PLATFORM: &str = "https://www.googleapis.com/auth/cloud-platform";
/// Additional scope a Cloud SQL IAM **database** login needs, on top of cloud-platform.
pub const SCOPE_SQL_LOGIN: &str = "https://www.googleapis.com/auth/sqlservice.login";
/// Scope string for the Cloud SQL IAM database login (both scopes, space separated).
pub const SCOPES_CLOUD_SQL_LOGIN: &str = "https://www.googleapis.com/auth/cloud-platform \
                                          https://www.googleapis.com/auth/sqlservice.login";
/// JWT lifetime: Google accepts up to 3600s.
pub(crate) const JWT_LIFETIME_SECS: u64 = 3600;
/// Cache an access token until this margin before `expires_in`.
const TOKEN_CACHE_MARGIN: Duration = Duration::from_secs(60);
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// TTL assumed when the token response omits `expires_in`.
const DEFAULT_TOKEN_TTL_SECS: u64 = 3600;

/// Why a Google credential could not be turned into a Bearer token.
///
/// The split is the same one every lambo adapter draws: `Unavailable` is a condition that
/// may pass (the network, a file that is not there yet), `Backend` is a condition an
/// operator has to fix (a bad key, a rejected grant).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoogleAuthError {
    /// Transport failure, or a credential file that cannot be read or parsed.
    Unavailable(String),
    /// Permanent: the token endpoint refused, or the key material is unusable.
    Backend(String),
}

impl std::fmt::Display for GoogleAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GoogleAuthError::Unavailable(m) | GoogleAuthError::Backend(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for GoogleAuthError {}

/// Parsed Google credentials: a service-account key or an authorized-user ADC file.
#[derive(Debug, Clone)]
pub enum GoogleCredentials {
    ServiceAccount {
        client_email: String,
        private_key: String,
        project_id: Option<String>,
        token_uri: String,
    },
    AuthorizedUser {
        client_id: String,
        client_secret: String,
        refresh_token: String,
        quota_project_id: Option<String>,
        token_uri: String,
    },
}

impl GoogleCredentials {
    /// The GCP project this credential implies, when it names one: a service-account key
    /// carries `project_id`, an ADC file carries `quota_project_id`. `None` means the
    /// caller must be told the project by config.
    pub fn project_id(&self) -> Option<String> {
        match self {
            GoogleCredentials::ServiceAccount { project_id, .. } => project_id.clone(),
            GoogleCredentials::AuthorizedUser {
                quota_project_id, ..
            } => quota_project_id.clone(),
        }
    }

    /// OAuth token endpoint this credential names (or the Google default).
    pub fn token_uri(&self) -> &str {
        match self {
            GoogleCredentials::ServiceAccount { token_uri, .. } => token_uri,
            GoogleCredentials::AuthorizedUser { token_uri, .. } => token_uri,
        }
    }
}

/// Raw service-account key JSON.
#[derive(Deserialize)]
struct ServiceAccountJson {
    client_email: String,
    private_key: String,
    project_id: Option<String>,
    #[serde(default)]
    token_uri: Option<String>,
}

/// Raw authorized-user ADC JSON.
#[derive(Deserialize)]
struct AuthorizedUserJson {
    client_id: String,
    client_secret: String,
    refresh_token: String,
    quota_project_id: Option<String>,
    #[serde(default)]
    token_uri: Option<String>,
}

/// The shared credential file, if this process was given one.
///
/// `GCP_LAMBO_CREDENTIALS` first so a deployment can point lambo at one identity without
/// disturbing whatever `GOOGLE_APPLICATION_CREDENTIALS` means to the rest of the machine.
///
/// **An empty value is an absent value**, the same rule `Config::overlay_env` applies
/// everywhere else: `GCP_LAMBO_CREDENTIALS=` in a shell profile must not shadow a working
/// `GOOGLE_APPLICATION_CREDENTIALS` and leave the caller refusing with a nameless path.
pub fn credentials_path_from_env() -> Option<PathBuf> {
    non_empty_env("GCP_LAMBO_CREDENTIALS")
        .or_else(|| non_empty_env("GOOGLE_APPLICATION_CREDENTIALS"))
        .map(PathBuf::from)
}

/// An environment variable's value, treating empty as unset.
fn non_empty_env(var: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(var).filter(|v| !v.is_empty())
}

/// Read and parse a Google credentials file (service-account key or authorized-user ADC).
pub fn load_credentials(path: &Path) -> Result<GoogleCredentials, GoogleAuthError> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        GoogleAuthError::Unavailable(format!(
            "cannot read Google credentials {}: {e}",
            path.display()
        ))
    })?;
    let v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
        GoogleAuthError::Unavailable(format!(
            "malformed Google credentials {}: {e}",
            path.display()
        ))
    })?;
    let cred_type = v
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("service_account");
    match cred_type {
        "service_account" => {
            let sa: ServiceAccountJson = serde_json::from_value(v).map_err(|e| {
                GoogleAuthError::Unavailable(format!(
                    "malformed service-account credentials {}: {e}",
                    path.display()
                ))
            })?;
            if sa.client_email.is_empty() || sa.private_key.is_empty() {
                return Err(GoogleAuthError::Unavailable(
                    "service-account credentials missing client_email or private_key".into(),
                ));
            }
            Ok(GoogleCredentials::ServiceAccount {
                client_email: sa.client_email,
                private_key: sa.private_key,
                project_id: sa.project_id,
                token_uri: sa
                    .token_uri
                    .unwrap_or_else(|| DEFAULT_TOKEN_URI.to_string()),
            })
        }
        "authorized_user" => {
            let au: AuthorizedUserJson = serde_json::from_value(v).map_err(|e| {
                GoogleAuthError::Unavailable(format!(
                    "malformed authorized-user credentials {}: {e}",
                    path.display()
                ))
            })?;
            if au.client_id.is_empty() || au.client_secret.is_empty() || au.refresh_token.is_empty()
            {
                return Err(GoogleAuthError::Unavailable(
                    "authorized-user credentials missing client_id, client_secret or refresh_token"
                        .into(),
                ));
            }
            Ok(GoogleCredentials::AuthorizedUser {
                client_id: au.client_id,
                client_secret: au.client_secret,
                refresh_token: au.refresh_token,
                quota_project_id: au.quota_project_id,
                token_uri: au
                    .token_uri
                    .unwrap_or_else(|| DEFAULT_TOKEN_URI.to_string()),
            })
        }
        other => Err(GoogleAuthError::Unavailable(format!(
            "unsupported Google credentials type {other:?} in {}",
            path.display()
        ))),
    }
}

/// JWT claims carried by the service-account assertion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Claims {
    pub(crate) iss: String,
    pub(crate) scope: String,
    pub(crate) aud: String,
    pub(crate) iat: u64,
    pub(crate) exp: u64,
}

/// Build a `reqwest::Client` with the timeouts every Google call here uses.
pub fn build_client() -> Result<reqwest::Client, GoogleAuthError> {
    reqwest::Client::builder()
        .connect_timeout(DEFAULT_CONNECT_TIMEOUT)
        .timeout(DEFAULT_REQUEST_TIMEOUT)
        .build()
        .map_err(|e| GoogleAuthError::Unavailable(format!("failed to build HTTP client: {e}")))
}

/// Mints and caches an OAuth access token for a Google principal.
///
/// One instance per consumer: the embedder holds one scoped to Vertex, the Postgres store
/// holds one scoped to a Cloud SQL IAM login. Sharing the credential file is the point;
/// sharing a token across scopes is not.
#[derive(Debug)]
pub struct GoogleOAuthTokenSource {
    creds: GoogleCredentials,
    client: reqwest::Client,
    scope: String,
    /// The live token and the instant it stops being handed out.
    cached: Option<(String, Instant)>,
}

impl GoogleOAuthTokenSource {
    /// A token source for **Vertex**: the cloud-platform scope and nothing wider.
    ///
    /// Callers name the consumer rather than the scope so a scope cannot be got wrong at a
    /// call site: getting it wrong now means editing this function, which
    /// `vertex_asks_for_cloud_platform_only` pins.
    pub fn for_vertex(
        creds: GoogleCredentials,
        client: reqwest::Client,
    ) -> Result<Self, GoogleAuthError> {
        Self::new(creds, client, SCOPE_CLOUD_PLATFORM)
    }

    /// A token source for a **Cloud SQL IAM database login**: cloud-platform plus
    /// `sqlservice.login`, which is the scope the login itself requires. See
    /// [`Self::for_vertex`] for why this is a named constructor.
    pub fn for_cloud_sql(
        creds: GoogleCredentials,
        client: reqwest::Client,
    ) -> Result<Self, GoogleAuthError> {
        Self::new(creds, client, SCOPES_CLOUD_SQL_LOGIN)
    }

    /// The scope this source asks for. Since the refresh grant sends it as a **narrowing**
    /// request, a source that asks for more than its credential was granted fails at the
    /// token endpoint with `invalid_scope`, which is why the value is worth asserting.
    pub fn scope(&self) -> &str {
        &self.scope
    }

    /// Build a token source for an explicit `scope` (see [`SCOPE_CLOUD_PLATFORM`],
    /// [`SCOPES_CLOUD_SQL_LOGIN`]). Prefer [`Self::for_vertex`] or [`Self::for_cloud_sql`];
    /// this stays for tests and for a consumer neither of those describes. No network is
    /// touched until the first token is asked for.
    pub fn new(
        creds: GoogleCredentials,
        client: reqwest::Client,
        scope: impl Into<String>,
    ) -> Result<Self, GoogleAuthError> {
        Ok(Self {
            creds,
            client,
            scope: scope.into(),
            cached: None,
        })
    }

    /// Mint the RS256 JWT assertion (service-account only; exposed for tests). A private
    /// key that will not parse or sign is a permanent, operator-fixing `Backend`.
    pub fn mint_jwt(&self) -> Result<String, GoogleAuthError> {
        let GoogleCredentials::ServiceAccount {
            client_email,
            private_key,
            ..
        } = &self.creds
        else {
            return Err(GoogleAuthError::Unavailable(
                "JWT minting requires service-account credentials".into(),
            ));
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| GoogleAuthError::Unavailable(format!("system clock before epoch: {e}")))?
            .as_secs();
        let claims = Claims {
            iss: client_email.clone(),
            scope: self.scope.clone(),
            aud: self.creds.token_uri().to_string(),
            iat: now,
            exp: now + JWT_LIFETIME_SECS,
        };
        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        let key = jsonwebtoken::EncodingKey::from_rsa_pem(private_key.as_bytes()).map_err(|e| {
            GoogleAuthError::Backend(format!(
                "failed to parse service-account private key PEM: {e}"
            ))
        })?;
        jsonwebtoken::encode(&header, &claims, &key).map_err(|e| {
            GoogleAuthError::Backend(format!("failed to sign service-account JWT: {e}"))
        })
    }

    /// A valid Bearer token, minted or from cache.
    pub async fn access_token(&mut self) -> Result<String, GoogleAuthError> {
        Ok(self.access_token_with_expiry().await?.0)
    }

    /// [`Self::access_token`], plus the instant this token stops being handed out.
    ///
    /// The expiry is what lets a caller that cannot re-authenticate an existing connection
    /// rebuild it in time: the Postgres store passes the token as a connection password,
    /// so it rebuilds its pool at this instant rather than discovering expiry as a login
    /// failure an hour into a session.
    pub async fn access_token_with_expiry(&mut self) -> Result<(String, Instant), GoogleAuthError> {
        if let Some((token, expires_at)) = &self.cached {
            if Instant::now() < *expires_at {
                return Ok((token.clone(), *expires_at));
            }
        }
        let params: Vec<(&'static str, String)> = match &self.creds {
            GoogleCredentials::ServiceAccount { .. } => {
                let jwt = self.mint_jwt()?;
                vec![
                    (
                        "grant_type",
                        "urn:ietf:params:oauth:grant-type:jwt-bearer".to_string(),
                    ),
                    ("assertion", jwt),
                ]
            }
            GoogleCredentials::AuthorizedUser {
                client_id,
                client_secret,
                refresh_token,
                ..
            } => vec![
                ("grant_type", "refresh_token".to_string()),
                ("client_id", client_id.clone()),
                ("client_secret", client_secret.clone()),
                ("refresh_token", refresh_token.clone()),
                // RFC 6749 section 6: `scope` on a refresh grant is a **narrowing**
                // request, and must be a subset of what the refresh token was granted.
                // Sending it is what makes the caller's scope real on this grant rather
                // than merely documented: ask for more than was granted and Google
                // answers `invalid_scope` at the token endpoint, which this module
                // classifies `Backend` and the operator sees immediately. Omit it and an
                // ADC minted without `sqlservice.login` would sail past here and be
                // refused later, at the database, as opaque authentication noise.
                ("scope", self.scope.clone()),
            ],
        };
        let resp = self
            .client
            .post(self.creds.token_uri())
            .form(&params)
            .send()
            .await
            .map_err(|e| {
                GoogleAuthError::Unavailable(format!("OAuth token endpoint unreachable: {e}"))
            })?;
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.map_err(|e| {
            GoogleAuthError::Backend(format!(
                "OAuth token endpoint returned unparseable JSON: {e}"
            ))
        })?;
        if !status.is_success() {
            return Err(GoogleAuthError::Backend(format!(
                "OAuth token endpoint returned {status}: {body}"
            )));
        }
        let token = body
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                GoogleAuthError::Backend("OAuth token response missing access_token".into())
            })?
            .to_string();
        // TTL default when `expires_in` is absent; never cache for longer than it.
        let expires_in = body
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TOKEN_TTL_SECS);
        let ttl = expires_in
            .saturating_sub(TOKEN_CACHE_MARGIN.as_secs())
            .max(1);
        let expires_at = Instant::now() + Duration::from_secs(ttl);
        self.cached = Some((token.clone(), expires_at));
        Ok((token, expires_at))
    }
}

/// RSA test key pair. Test-only, and never a credential: it exists so JWT minting and the
/// credential-file paths are provable without a real service account. Shared by the
/// embedder's and the store's tests through `pub(crate)`.
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

    pub(crate) fn sample_credentials(token_uri: &str) -> GoogleCredentials {
        GoogleCredentials::ServiceAccount {
            client_email: "sa@example.com".to_string(),
            private_key: TEST_RSA_PRIVATE_KEY_PEM.to_string(),
            project_id: Some("proj".to_string()),
            token_uri: token_uri.to_string(),
        }
    }

    pub(crate) fn sample_authorized_user(token_uri: &str) -> GoogleCredentials {
        GoogleCredentials::AuthorizedUser {
            client_id: "client-1".to_string(),
            client_secret: "secret-1".to_string(),
            refresh_token: "refresh-1".to_string(),
            quota_project_id: Some("quota-proj".to_string()),
            token_uri: token_uri.to_string(),
        }
    }

    #[test]
    fn mints_and_verifies_service_account_jwt() {
        let src = GoogleOAuthTokenSource::new(
            sample_credentials(DEFAULT_TOKEN_URI),
            reqwest::Client::new(),
            SCOPE_CLOUD_PLATFORM,
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

    /// The scope is the caller's, and it reaches the wire: a Cloud SQL login must not be
    /// signed with Vertex's scope set just because the embedder was written first.
    #[test]
    fn the_callers_scope_is_the_one_signed() {
        let src = GoogleOAuthTokenSource::new(
            sample_credentials(DEFAULT_TOKEN_URI),
            reqwest::Client::new(),
            SCOPES_CLOUD_SQL_LOGIN,
        )
        .unwrap();
        let jwt = src.mint_jwt().unwrap();
        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
        validation.validate_aud = false;
        validation.set_required_spec_claims(&["iss", "exp"]);
        let key =
            jsonwebtoken::DecodingKey::from_rsa_pem(TEST_RSA_PUBLIC_KEY_PEM.as_bytes()).unwrap();
        let data = jsonwebtoken::decode::<Claims>(&jwt, &key, &validation).unwrap();
        assert!(
            data.claims.scope.contains(SCOPE_SQL_LOGIN),
            "sqlservice.login missing from {:?}",
            data.claims.scope
        );
        assert!(data.claims.scope.contains(SCOPE_CLOUD_PLATFORM));
    }

    /// L1-R2-1. The scope each consumer asks for is a property of the named constructor,
    /// not of a string typed at a call site. Both call sites go through these, so swapping
    /// the two scopes is a one-line change these two tests catch.
    #[test]
    fn vertex_asks_for_cloud_platform_only() {
        let src = GoogleOAuthTokenSource::for_vertex(
            sample_credentials(DEFAULT_TOKEN_URI),
            reqwest::Client::new(),
        )
        .unwrap();
        assert_eq!(src.scope(), SCOPE_CLOUD_PLATFORM);
        assert!(
            !src.scope().contains("sqlservice.login"),
            "Vertex must not carry the database login scope: {:?}",
            src.scope()
        );
    }

    #[test]
    fn a_cloud_sql_login_asks_for_the_database_login_scope() {
        let src = GoogleOAuthTokenSource::for_cloud_sql(
            sample_credentials(DEFAULT_TOKEN_URI),
            reqwest::Client::new(),
        )
        .unwrap();
        assert!(
            src.scope().contains(SCOPE_SQL_LOGIN),
            "a Cloud SQL IAM login without sqlservice.login is refused by the database: {:?}",
            src.scope()
        );
        assert!(
            src.scope().contains(SCOPE_CLOUD_PLATFORM),
            "{:?}",
            src.scope()
        );
    }

    /// L1-R2-2 and L1-R2-3. Precedence is only meaningful when both variables are set, and
    /// an empty value is an absent value: `GCP_LAMBO_CREDENTIALS=` left in a shell profile
    /// must not shadow a working `GOOGLE_APPLICATION_CREDENTIALS`.
    #[test]
    fn the_shared_variable_wins_but_an_empty_one_does_not_shadow() {
        let _g = crate::test_util::env_lock();
        let prev_gcp = std::env::var_os("GCP_LAMBO_CREDENTIALS");
        let prev_adc = std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS");

        std::env::set_var("GCP_LAMBO_CREDENTIALS", "/shared/creds.json");
        std::env::set_var("GOOGLE_APPLICATION_CREDENTIALS", "/adc/creds.json");
        let both = credentials_path_from_env();

        std::env::set_var("GCP_LAMBO_CREDENTIALS", "");
        let shared_empty = credentials_path_from_env();

        std::env::remove_var("GCP_LAMBO_CREDENTIALS");
        let shared_unset = credentials_path_from_env();

        std::env::set_var("GOOGLE_APPLICATION_CREDENTIALS", "");
        let both_empty = credentials_path_from_env();

        match prev_gcp {
            Some(v) => std::env::set_var("GCP_LAMBO_CREDENTIALS", v),
            None => std::env::remove_var("GCP_LAMBO_CREDENTIALS"),
        }
        match prev_adc {
            Some(v) => std::env::set_var("GOOGLE_APPLICATION_CREDENTIALS", v),
            None => std::env::remove_var("GOOGLE_APPLICATION_CREDENTIALS"),
        }

        assert_eq!(
            both.as_deref(),
            Some(std::path::Path::new("/shared/creds.json")),
            "the shared variable outranks the Google-standard one when both are set"
        );
        assert_eq!(
            shared_empty.as_deref(),
            Some(std::path::Path::new("/adc/creds.json")),
            "an empty shared variable must not shadow a working one"
        );
        assert_eq!(
            shared_unset.as_deref(),
            Some(std::path::Path::new("/adc/creds.json"))
        );
        assert_eq!(both_empty, None, "two empty values name no credential file");
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
        let mut src =
            GoogleOAuthTokenSource::new(creds, reqwest::Client::new(), SCOPE_CLOUD_PLATFORM)
                .unwrap();
        let t1 = src.access_token().await.unwrap();
        assert_eq!(t1, "tok-1");
        // Second call hits the cache: the endpoint mock must have seen exactly one request.
        let t2 = src.access_token().await.unwrap();
        assert_eq!(t2, "tok-1");
        mock.assert_hits(1);
    }

    /// The expiry handed back is the one the cache honours, and it respects the margin:
    /// the Postgres store rebuilds its pool on this instant, so a wrong one is a login
    /// failure an hour later.
    #[tokio::test]
    async fn expiry_is_reported_and_carries_the_refresh_margin() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/token");
            then.status(200)
                .json_body(serde_json::json!({ "access_token": "tok-1", "expires_in": 3600 }));
        });
        let creds = sample_credentials(&format!("{}/token", server.base_url()));
        let mut src =
            GoogleOAuthTokenSource::new(creds, reqwest::Client::new(), SCOPE_CLOUD_PLATFORM)
                .unwrap();
        let before = Instant::now();
        let (_, expires_at) = src.access_token_with_expiry().await.unwrap();
        // The deadline is measured from when the token was minted, so from `before` it is
        // the margin-adjusted TTL plus however long the exchange took. What must hold is
        // that it lands a full margin short of the token's real 3600s expiry.
        let ttl = expires_at.duration_since(before);
        assert!(
            ttl < Duration::from_secs(3600),
            "token handed out up to its own expiry, with no margin: {ttl:?}"
        );
        assert!(
            ttl >= Duration::from_secs(3600 - TOKEN_CACHE_MARGIN.as_secs()),
            "refresh margin larger than the one documented: {ttl:?}"
        );
        // The cached call reports the same deadline rather than extending it.
        let (_, again) = src.access_token_with_expiry().await.unwrap();
        assert_eq!(expires_at, again);
    }

    #[tokio::test]
    async fn token_endpoint_transport_failure_is_unavailable() {
        let creds = sample_credentials("http://127.0.0.1:9/token");
        let mut src =
            GoogleOAuthTokenSource::new(creds, reqwest::Client::new(), SCOPE_CLOUD_PLATFORM)
                .unwrap();
        let err = src.access_token().await.unwrap_err();
        assert!(matches!(err, GoogleAuthError::Unavailable(_)), "{err:?}");
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
        let mut src =
            GoogleOAuthTokenSource::new(creds, reqwest::Client::new(), SCOPE_CLOUD_PLATFORM)
                .unwrap();
        let err = src.access_token().await.unwrap_err();
        assert!(matches!(err, GoogleAuthError::Backend(_)), "{err:?}");
    }

    #[tokio::test]
    async fn authorized_user_uses_refresh_token_grant_and_caches() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/token")
                .body_contains("grant_type=refresh_token")
                .body_contains("client_id=client-1")
                .body_contains("client_secret=secret-1")
                .body_contains("refresh_token=refresh-1");
            then.status(200)
                .json_body(serde_json::json!({ "access_token": "tok-au", "expires_in": 3600 }));
        });
        let creds = sample_authorized_user(&format!("{}/token", server.base_url()));
        let mut src =
            GoogleOAuthTokenSource::new(creds, reqwest::Client::new(), SCOPES_CLOUD_SQL_LOGIN)
                .unwrap();
        assert_eq!(src.access_token().await.unwrap(), "tok-au");
        assert_eq!(src.access_token().await.unwrap(), "tok-au");
        mock.assert_hits(1); // second call hits the cache
    }

    /// Form-encoded scope values, as `reqwest`'s `.form()` writes them: `:` and `/` are
    /// percent-encoded and the separating space becomes `+`. Spelled out rather than
    /// computed so the test states what is on the wire instead of restating the code.
    const CLOUD_PLATFORM_FORM: &str = "https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fcloud-platform";
    const SQL_LOGIN_FORM: &str = "https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fsqlservice.login";

    /// The scope reaches the wire on the **refresh-token** grant too, not only in the
    /// service-account JWT's signed claim.
    ///
    /// Without this, `GoogleOAuthTokenSource`'s per-caller scope was true of one grant
    /// and inert on the other, so an authorized-user ADC handed both consumers the same
    /// token carrying whatever the ADC happened to be granted. Sending it makes the ask
    /// explicit: RFC 6749 section 6 treats `scope` on a refresh grant as a narrowing
    /// request, and Google answers `invalid_scope` when the scope was never granted, so
    /// a Cloud SQL login fails at the token endpoint naming the scope rather than at the
    /// database as opaque authentication noise.
    ///
    /// The whole body is asserted, not a fragment, and both callers are exercised: a
    /// `scope` hardcoded to one consumer's constant would pass one half and fail the
    /// other. The body is deterministic because the parameter vector is built in order.
    #[tokio::test]
    async fn the_callers_scope_is_sent_on_the_refresh_grant() {
        let server = MockServer::start();
        let grant = "grant_type=refresh_token&client_id=client-1&client_secret=secret-1\
                     &refresh_token=refresh-1";
        let sql = server.mock(|when, then| {
            when.method(POST).path("/sql").body(format!(
                "{grant}&scope={CLOUD_PLATFORM_FORM}+{SQL_LOGIN_FORM}"
            ));
            then.status(200)
                .json_body(serde_json::json!({ "access_token": "tok-sql", "expires_in": 3600 }));
        });
        let vertex = server.mock(|when, then| {
            when.method(POST)
                .path("/vertex")
                .body(format!("{grant}&scope={CLOUD_PLATFORM_FORM}"));
            then.status(200)
                .json_body(serde_json::json!({ "access_token": "tok-vertex", "expires_in": 3600 }));
        });

        let mut store_side = GoogleOAuthTokenSource::new(
            sample_authorized_user(&format!("{}/sql", server.base_url())),
            reqwest::Client::new(),
            SCOPES_CLOUD_SQL_LOGIN,
        )
        .unwrap();
        assert_eq!(store_side.access_token().await.unwrap(), "tok-sql");
        sql.assert_hits(1);

        let mut embedder_side = GoogleOAuthTokenSource::new(
            sample_authorized_user(&format!("{}/vertex", server.base_url())),
            reqwest::Client::new(),
            SCOPE_CLOUD_PLATFORM,
        )
        .unwrap();
        assert_eq!(embedder_side.access_token().await.unwrap(), "tok-vertex");
        vertex.assert_hits(1);
    }

    /// The regression this consolidation exists for: an authorized-user ADC file is a
    /// credential the **store** can use, not just the embedder. The old
    /// `CloudSqlTokenSource` deserialized service-account JSON only and refused this file.
    #[test]
    fn an_authorized_user_adc_file_loads_for_the_cloud_sql_path() {
        let dir = std::env::temp_dir().join(format!("lambo-adc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("application_default_credentials.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "type": "authorized_user",
                "client_id": "client-1",
                "client_secret": "secret-1",
                "refresh_token": "refresh-1",
                "quota_project_id": "mooshik",
            })
            .to_string(),
        )
        .unwrap();
        let creds = load_credentials(&path).expect("ADC must load for the Cloud SQL path");
        assert!(matches!(creds, GoogleCredentials::AuthorizedUser { .. }));
        assert_eq!(creds.project_id().as_deref(), Some("mooshik"));
        let src =
            GoogleOAuthTokenSource::new(creds, reqwest::Client::new(), SCOPES_CLOUD_SQL_LOGIN)
                .unwrap();
        // Minting a JWT is a service-account act; an ADC file says so rather than panicking.
        assert!(matches!(
            src.mint_jwt(),
            Err(GoogleAuthError::Unavailable(_))
        ));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn an_unknown_credential_type_is_named() {
        let dir = std::env::temp_dir().join(format!("lambo-cred-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("external.json");
        std::fs::write(&path, r#"{"type":"external_account"}"#).unwrap();
        let err = load_credentials(&path).unwrap_err();
        assert!(format!("{err}").contains("external_account"), "{err}");
        std::fs::remove_file(&path).ok();
    }
}
