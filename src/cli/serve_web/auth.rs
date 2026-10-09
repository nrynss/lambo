//! Authorization for the portal, mirroring T8.7's fail-closed bearer posture
//! in `crate::mcp::serve`: the unprintable token, env-over-flag resolution,
//! the non-loopback refusal, and the gate in front of every route. The
//! comparison is `crate::surface::bearer`'s, shared with `lambo serve`.

use std::net::IpAddr;
use std::sync::Arc;

use axum::http::{header, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};

use super::state::AppState;
use crate::cli::caps::CliError;
use crate::mcp::AUTH_TOKEN_ENV;

/// A bearer token that cannot be printed.
///
/// Mirrors `mcp::serve::SecretToken`: a redacting [`Debug`] makes "never
/// logged" a property of the type, and rejecting empty/whitespace tokens makes
/// a set-but-empty [`AUTH_TOKEN_ENV`] a usage error rather than a silent
/// authenticate-everything.
#[derive(Clone, PartialEq, Eq)]
pub struct AuthToken(String);

impl AuthToken {
    /// Reject empty and whitespace-only tokens (fail closed, not silently).
    pub(super) fn new(raw: impl Into<String>) -> Result<Self, String> {
        let raw = raw.into();
        if raw.trim().is_empty() {
            return Err(
                "auth token is empty — pass a non-empty secret, or omit it entirely to \
                 run unauthenticated on loopback"
                    .into(),
            );
        }
        Ok(Self(raw))
    }

    pub(super) fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl std::fmt::Debug for AuthToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AuthToken(<redacted>)")
    }
}

impl std::str::FromStr for AuthToken {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

/// Does an `Authorization` header carry the expected bearer token?
///
/// Scheme matched case-insensitively (RFC 7235 §2.1); the credential compared
/// byte-for-byte in constant time. The parse and the comparison are the ones
/// `mcp::serve` uses, from `crate::surface::bearer` (#28): this surface used
/// to carry its own copy, which had drifted from that one.
pub(super) fn bearer_ok(header: Option<&str>, expected: &AuthToken) -> bool {
    crate::surface::bearer::bearer_ok(header, expected.as_bytes())
}

/// Resolve the effective token from the flag and the environment (env wins).
///
/// Mirrors `mcp::serve::resolve_auth_token`: a set-but-empty env var, or
/// one that is not valid UTF-8, is an error rather than a silent fallback
/// to the flag (the error names the variable, never its value).
pub(super) fn resolve_auth_token(flag: Option<AuthToken>) -> Result<Option<AuthToken>, CliError> {
    resolve_auth_token_from(flag, std::env::var_os(AUTH_TOKEN_ENV))
}

/// [`resolve_auth_token`] over a given value of the variable.
pub(super) fn resolve_auth_token_from(
    flag: Option<AuthToken>,
    env: Option<std::ffi::OsString>,
) -> Result<Option<AuthToken>, CliError> {
    match env {
        Some(raw) => {
            let raw = raw
                .into_string()
                .map_err(|_| CliError::Usage(format!("{AUTH_TOKEN_ENV}: is not valid UTF-8")))?;
            AuthToken::new(raw)
                .map(Some)
                .map_err(|e| CliError::Usage(format!("{AUTH_TOKEN_ENV}: {e}")))
        }
        None => Ok(flag),
    }
}

/// Fail closed when a non-loopback bind has no token.
///
/// Mirrors `mcp::serve::authorize_bind` — the rule, not its J2 section: a
/// reader takes no lease and binds no session endpoint, so the pre-lease
/// ordering argument that section restates has no counterpart here.
/// serve-web is a *reader* — it never
/// takes the writer lease, so exposure is read-only — but the whole session
/// is still readable, so a token-less bind to the world is not a configuration
/// worth starting.
pub(super) fn authorize_bind_web(bind: IpAddr, token: Option<&AuthToken>) -> Result<(), CliError> {
    if bind.is_loopback() || token.is_some() {
        return Ok(());
    }
    Err(CliError::Usage(format!(
        "refusing to start: --bind {bind} exposes an unauthenticated read-only session beyond \
         loopback. Set {AUTH_TOKEN_ENV} (or pass --auth-token) to require \
         'Authorization: Bearer <token>' on every request, or bind 127.0.0.1 and reach it \
         through a tunnel or an authenticating proxy."
    )))
}

/// Bearer gate applied when a token is configured.
///
/// When [`AppState::auth`] is `Some`, every request — static asset, health
/// check, or API — must carry `Authorization: Bearer <token>`. When it is
/// `None` (the loopback default) this is a pure pass-through, so a judge's
/// browser needs no credentials. Mirrors `mcp::serve`'s `guard_request`, minus
/// the transport-specific rate/session guards this read-only process does not
/// have.
pub(super) async fn require_auth(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    req: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    if let Some(expected) = &state.auth {
        let presented = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        if !bearer_ok(presented, expected) {
            // Deliberately terse and identical for "no header" and "wrong
            // token": the difference is not the caller's business, and the
            // token itself is never echoed.
            return (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Bearer")],
                "unauthorized: this endpoint requires 'Authorization: Bearer <token>'\n",
            )
                .into_response();
        }
    }
    next.run(req).await
}
