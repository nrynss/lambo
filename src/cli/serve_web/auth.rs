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
//!
//! **DNS rebinding (#4 design 4.5).** Under the implicit grant nothing
//! about the caller is checked, so a web page the local user visits could
//! re-resolve its own name to 127.0.0.1 and read the portal same-origin.
//! [`HostCheck`] closes that: while no token is configured, a request whose
//! `Host` is not `localhost`, `127.0.0.1` or `[::1]` (any port) or an
//! `--allowed-host` / `[web] allowed_hosts` entry gets one fixed 403, before
//! anything else is evaluated. With a token configured any `Host` is
//! accepted: a rebound page cannot present a token it does not know (the
//! rule `lambo serve` applies, #32 PR 5 review M1).

use std::net::IpAddr;
use std::sync::Arc;

use axum::http::{header, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};

use super::state::AppState;
use crate::cli::caps::CliError;
use crate::config::AllowedHost;
use crate::mcp::AUTH_TOKEN_ENV;
use crate::surface::session::{
    parse_addressed, BearerSecret, HostedSessions, SessionAuthority, SessionGrant,
    LOCAL_CREDENTIAL_NAME,
};
use crate::types::SessionId;

/// The portal's credential set.
pub(super) type PortalAuthority = SessionAuthority<AuthToken>;

/// Marks a request `scope::resolve_session` already authenticated (a
/// scoped path), so the [`gate`] over the routes does not check it twice.
#[derive(Clone, Copy)]
pub(super) struct Authenticated;

/// A bearer token that cannot be printed.
///
/// Mirrors `mcp::serve::SecretToken`: a redacting [`Debug`] makes "never
/// logged" a property of the type, and rejecting empty/whitespace tokens makes
/// a set-but-empty [`AUTH_TOKEN_ENV`] a usage error rather than a silent
/// authenticate-everything.
#[derive(Clone, PartialEq, Eq)]
pub struct AuthToken(String);

impl AuthToken {
    /// Refuse a token no request could present, by the rule `lambo serve`
    /// applies to its own ([`check_configured_token`], shared so the two
    /// cannot drift; #4 PR 2 review M2): empty or whitespace-only (fail
    /// closed, not silently), leading or trailing whitespace, a byte outside
    /// printable ASCII, or longer than
    /// [`MAX_BEARER_CREDENTIAL_BYTES`](crate::surface::bearer::MAX_BEARER_CREDENTIAL_BYTES).
    /// The portal authenticates through `SessionAuthority`, which trims the
    /// presented credential and refuses one over the cap before the scan,
    /// so each of those would answer every request 401. The message names
    /// the rule, never the value.
    ///
    /// [`check_configured_token`]: crate::surface::bearer::check_configured_token
    pub(super) fn new(raw: impl Into<String>) -> Result<Self, String> {
        let raw = raw.into();
        crate::surface::bearer::check_configured_token(&raw)?;
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

/// Which `Host` values the portal answers (#4 design 4.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum HostCheck {
    /// The implicit loopback grant: only these hosts (the loopback names
    /// and the configured extras).
    Only(Vec<AllowedHost>),
    /// A bearer token is required, so any `Host` is accepted.
    Any,
}

/// The `Host` names every unauthenticated portal accepts, on any port.
pub(super) const LOOPBACK_HOSTS: &[&str] = &["localhost", "127.0.0.1", "::1"];

impl HostCheck {
    /// The check for `authority`: [`HostCheck::Only`] the loopback names and
    /// `extra` while it needs no bearer, [`HostCheck::Any`] once it does.
    pub(super) fn for_authority(authority: &PortalAuthority, extra: &[AllowedHost]) -> Self {
        if authority.requires_bearer() {
            return Self::Any;
        }
        Self::Only(
            LOOPBACK_HOSTS
                .iter()
                .map(|h| AllowedHost::from_parts(h, None))
                .chain(extra.iter().cloned())
                .collect(),
        )
    }

    /// Does `req` name an accepted host? The `Host` header, else the
    /// request target's authority (HTTP/2's `:authority`); a request with
    /// neither, or with one that does not parse, is refused.
    fn allows(&self, req: &axum::extract::Request) -> bool {
        let Self::Only(allowed) = self else {
            return true;
        };
        let presented = match req.headers().get(header::HOST) {
            Some(value) => value
                .to_str()
                .ok()
                .and_then(|h| h.parse::<axum::http::uri::Authority>().ok()),
            None => req.uri().authority().cloned(),
        };
        let Some(presented) = presented else {
            return false;
        };
        let presented = AllowedHost::from_parts(presented.host(), presented.port_u16());
        allowed.iter().any(|a| a.matches(&presented))
    }
}

/// The Host refusal: one fixed 403, independent of the path (so of any
/// session) and of the `Host` presented, which is never echoed.
fn host_refused() -> Response {
    (
        StatusCode::FORBIDDEN,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "forbidden: this unauthenticated portal answers only requests addressed to localhost, \
         127.0.0.1 or [::1], or to a host named by --allowed-host or [web] allowed_hosts\n",
    )
        .into_response()
}

/// Has a Host refusal been logged at `warn` yet? The first one is, so an
/// operator behind a proxy that forwards its public name sees why; later
/// ones go to `debug`, so a scanner cannot flood the log.
static HOST_REFUSAL_WARNED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Step 0 of the fixed order (design 3.3), before routing, on every
/// request: the `Host` check. Under the implicit grant only the loopback
/// names and the allowed hosts are answered; once a token is required, any.
pub(super) async fn host_guard(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    req: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    if !state.host_check.allows(&req) {
        if HOST_REFUSAL_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            tracing::debug!("serve-web: request refused: Host not allowed");
        } else {
            tracing::warn!(
                "serve-web: request refused: its Host is not a loopback name or an allowed \
                 host (DNS-rebinding defence). Behind a proxy that forwards its public name, \
                 pass --allowed-host <name>; further refusals are logged at debug"
            );
        }
        return host_refused();
    }
    next.run(req).await
}

/// Step 1 of the fixed order (design 3.3): the request's bearer token
/// resolved to a grant, or the 401, before anything about sessions is
/// evaluated.
///
/// With a token configured, every request (static asset, health check,
/// API, unrouted path) must carry `Authorization: Bearer <token>`, compared
/// by [`SessionAuthority::authenticate`]'s constant-time scan
/// (`crate::surface::bearer`, shared with `lambo serve`). Under the implicit
/// loopback grant no header is read, so a judge's browser needs no
/// credentials. Mirrors `mcp::serve`'s `guard_request`, minus the
/// transport-specific rate/session guards this read-only process does not
/// have.
///
/// `None` is answered with [`unauthorized`].
pub(super) fn authenticate(
    state: &AppState,
    req: &axum::extract::Request,
) -> Option<Arc<SessionGrant>> {
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    state.authority.authenticate(presented)
}

/// The gate over the routes themselves (after routing, where the portal's
/// bearer gate has always been), for a request `scope::resolve_session` did
/// not already authenticate: every unscoped path, routed or not. It
/// authenticates (the 401 a client of the unscoped routes has always seen,
/// byte for byte, `Allow` on a non-`GET` route included) and then attaches
/// the default session when the grant may read it (the aliases, design
/// Q10).
pub(super) async fn gate(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    mut req: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    if req.extensions().get::<Authenticated>().is_none() {
        let Some(grant) = authenticate(&state, &req) else {
            return unauthorized();
        };
        if state
            .authority
            .authorize_default(&grant, state.default_session.as_str())
            .is_ok()
        {
            req.extensions_mut().insert(super::scope::SessionCtx {
                session: state.default_session.clone(),
            });
        }
        req.extensions_mut().insert(Authenticated);
    }
    next.run(req).await
}

/// The 401. Deliberately terse and identical for "no header" and "wrong
/// token": the difference is not the caller's business, and the token
/// itself is never echoed. Independent of the path, so of any session.
pub(super) fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        "unauthorized: this endpoint requires 'Authorization: Bearer <token>'\n",
    )
        .into_response()
}
