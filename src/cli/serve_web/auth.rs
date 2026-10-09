//! Authorization for the portal, mirroring T8.7's fail-closed bearer posture
//! in `crate::mcp::serve`: the unprintable token, env-over-flag resolution,
//! the non-loopback refusal, and the gate in front of every route.
//!
//! The credential set is `crate::surface::session`'s [`SessionAuthority`],
//! the type `lambo serve` authenticates with (#4 PR 2, design 4.1): the
//! bearer scan, the grants and the order are shared, only the secret type
//! ([`AuthToken`]) and the 401 wording are the portal's own. With no token
//! configured the set is the implicit loopback grant `local`; with one, the
//! legacy grant `default`. Both reach every served session.

use std::net::IpAddr;
use std::sync::Arc;

use axum::http::{header, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};

use super::state::AppState;
use crate::cli::caps::CliError;
use crate::mcp::AUTH_TOKEN_ENV;
use crate::surface::session::{
    parse_addressed, BearerSecret, HostedSessions, SessionAuthority, SessionGrant,
    LOCAL_CREDENTIAL_NAME,
};
use crate::types::SessionId;

/// The portal's credential set.
pub(super) type PortalAuthority = SessionAuthority<AuthToken>;

/// The grant [`guard`] resolved for a request, carried to the session
/// resolution in the request's extensions. A request that arrives there
/// without one (a router served without the guard) is refused, never served.
#[derive(Clone)]
pub(super) struct Authenticated(pub(super) Arc<SessionGrant>);

/// A bearer token that cannot be printed.
///
/// Mirrors `mcp::serve::SecretToken`: a redacting [`Debug`] makes "never
/// logged" a property of the type, and rejecting empty/whitespace tokens makes
/// a set-but-empty [`AUTH_TOKEN_ENV`] a usage error rather than a silent
/// authenticate-everything.
#[derive(Clone, PartialEq, Eq)]
pub struct AuthToken(String);

impl AuthToken {
    /// Reject empty and whitespace-only tokens (fail closed, not silently),
    /// and one longer than any request may present.
    ///
    /// The portal authenticates through `SessionAuthority`, which refuses a
    /// presented credential over
    /// [`MAX_BEARER_CREDENTIAL_BYTES`](crate::surface::bearer::MAX_BEARER_CREDENTIAL_BYTES)
    /// before the scan (#32 PR 5 review L2), so a longer configured token
    /// could never be matched: every request would be 401. Refused here, as
    /// `lambo serve` refuses one, naming the bound and never the value.
    pub(super) fn new(raw: impl Into<String>) -> Result<Self, String> {
        let raw = raw.into();
        if raw.trim().is_empty() {
            return Err(
                "auth token is empty — pass a non-empty secret, or omit it entirely to \
                 run unauthenticated on loopback"
                    .into(),
            );
        }
        let max = crate::surface::bearer::MAX_BEARER_CREDENTIAL_BYTES;
        if raw.len() > max {
            return Err(format!(
                "auth token is longer than {max} bytes, which no request may present"
            ));
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

impl BearerSecret for AuthToken {
    fn secret_bytes(&self) -> &[u8] {
        self.as_bytes()
    }
}

/// The portal's credential set over the served `sessions`.
///
/// The hosted set is the allowlist's addressable names and no prefix, so a
/// grant reaches nothing outside the allowlist (design 4.2). A loose single
/// session (one name outside the strict charset) is in no hosted set; the
/// unscoped aliases reach it through
/// [`SessionAuthority::authorize_default`], under a scope over every
/// pinned session, which both of this PR's grants have.
pub(super) fn portal_authority(auth: Option<AuthToken>, sessions: &[SessionId]) -> PortalAuthority {
    let hosted = HostedSessions::new(
        sessions
            .iter()
            .filter_map(|s| parse_addressed(s.as_str()).ok()),
        std::iter::empty(),
    );
    match auth {
        Some(token) => {
            SessionAuthority::with_credentials([(token, SessionGrant::legacy_default())], hosted)
        }
        None => SessionAuthority::implicit(SessionGrant::implicit_local(), hosted),
    }
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

/// The name of the portal's one credential, for the startup log: the
/// configured token's (`default`) or the implicit loopback one (`local`).
/// Never a secret.
pub(super) fn credential_label(authority: &PortalAuthority) -> &str {
    authority
        .credential_names()
        .first()
        .copied()
        .unwrap_or(LOCAL_CREDENTIAL_NAME)
}

/// Step 1 of the fixed order (design 3.3): resolve the request's bearer
/// token to a grant, before anything about sessions is evaluated.
///
/// With a token configured, every request (static asset, health check,
/// API, unrouted path) must carry `Authorization: Bearer <token>`, compared
/// by [`SessionAuthority::authenticate`]'s constant-time scan
/// (`crate::surface::bearer`, shared with `lambo serve`). Under the implicit
/// loopback grant no header is read, so a judge's browser needs no
/// credentials. Mirrors `mcp::serve`'s `guard_request`, minus the
/// transport-specific rate/session guards this read-only process does not
/// have.
pub(super) async fn guard(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    mut req: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let Some(grant) = state.authority.authenticate(presented) else {
        return unauthorized();
    };
    req.extensions_mut().insert(Authenticated(grant));
    next.run(req).await
}

/// The 401. Deliberately terse and identical for "no header" and "wrong
/// token": the difference is not the caller's business, and the token
/// itself is never echoed. Independent of the path, so of any session.
fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        "unauthorized: this endpoint requires 'Authorization: Bearer <token>'\n",
    )
        .into_response()
}
