//! Authorization for the portal, mirroring T8.7's fail-closed bearer posture
//! in `crate::mcp::serve`: the unprintable token, env-over-flag resolution,
//! the non-loopback refusal, and the gate in front of every route.
//!
//! The credential set is `crate::surface::session`'s [`SessionAuthority`],
//! the type `lambo serve` authenticates with (#4 PR 2, design 4.1): the
//! bearer scan, the grants and the order are shared, only the secret type
//! ([`AuthToken`]) and the 401 wording are the portal's own.
//!
//! | configured | grants, in scan order | scope |
//! |---|---|---|
//! | nothing (loopback only) | the implicit `local`, no header read | every served session |
//! | `LAMBO_AUTH_TOKEN` / `--auth-token` | `default` | every served session |
//! | `[[web.credential]]` (#4 PR 3) | each its own, after `default` if a legacy token is set too | its `sessions` (`"*"` = the allowlist) and/or `session_prefix`, intersected with the allowlist |
//! | `[web] inherit_serve_credentials` | each `[[serve.credential]]`, after the web ones, capabilities dropped | as configured for serve (`"*"` = serve's hosted set, never the allowlist), intersected with the allowlist |
//!
//! `local` exists only while no credential of any kind is configured; one
//! configured credential (legacy, web or inherited) and every request needs
//! a bearer. The scan over the set is `surface::bearer::match_any`,
//! constant-time over every credential.
//!
//! **DNS rebinding (#4 design 4.5).** Under the implicit grant nothing
//! about the caller is checked, so a web page the local user visits could
//! re-resolve its own name to 127.0.0.1 and read the portal same-origin.
//! [`HostCheck`] closes that: while no credential is configured, a request
//! whose `Host` is not `localhost`, `127.0.0.1` or `[::1]` (any port) or an
//! `--allowed-host` / `[web] allowed_hosts` entry gets one fixed 403, before
//! anything else is evaluated. With any credential configured (legacy,
//! `[[web.credential]]` or inherited, one or several) any `Host` is
//! accepted: a rebound page cannot present a token it does not know (the
//! rule `lambo serve` applies, #32 PR 5 review M1). "Only when no token" is
//! therefore "only under the implicit grant", whatever the number of
//! credentials.

use std::net::IpAddr;
use std::sync::Arc;

use axum::http::{header, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};

use super::state::AppState;
use crate::cli::caps::CliError;
use crate::config::credential::{self, SERVE_TABLE, WEB_TABLE};
use crate::config::{AllowedHost, ServeConfig, WebConfig};
use crate::mcp::AUTH_TOKEN_ENV;
use crate::surface::session::{
    parse_addressed, BearerSecret, HostedSessions, SessionAuthority, SessionCapabilities,
    SessionGrant,
};
use crate::types::SessionId;

/// The portal's credential set.
pub(super) type PortalAuthority = SessionAuthority<AuthToken>;

/// Marks a request `scope::resolve_session` already authenticated (a
/// scoped path), so the [`gate`] over the routes does not check it twice.
#[derive(Clone, Copy)]
pub(super) struct Authenticated;

/// The grant the request authenticated as, attached by [`gate`] for an
/// unscoped request (the listing reads it, #4 PR 3). Never a secret.
#[derive(Clone)]
pub(super) struct Caller(pub(super) Arc<SessionGrant>);

/// One configured read credential (#4 PR 3): a `[[web.credential]]`, or a
/// `[[serve.credential]]` imported by `[web] inherit_serve_credentials`
/// with its capabilities dropped. `Debug` is safe: [`AuthToken`] redacts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebCredential {
    /// The name and scope; never a capability (the portal is read-only).
    pub grant: SessionGrant,
    /// The token, from the credential's `token_env`.
    pub token: AuthToken,
}

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

/// The portal's credential set over the served `sessions`: the legacy
/// token's `default` grant first (when set), then `configured` in order;
/// the implicit `local` grant only when there is neither (module table).
///
/// The hosted set is the allowlist's addressable names and no prefix, so a
/// grant reaches nothing outside the allowlist (design 4.2): `"*"` is the
/// allowlist, and a prefix covers only allowlisted ids under it, because
/// the scoped path is also checked against [`SessionAuthority::is_pinned`].
/// A loose single session (one name outside the strict charset) is in no
/// hosted set; the unscoped aliases reach it through
/// [`SessionAuthority::authorize_default`], under a scope over every
/// pinned session (`local`, `default`, `"*"`).
pub(super) fn portal_authority(
    auth: Option<AuthToken>,
    configured: Vec<WebCredential>,
    sessions: &[SessionId],
) -> PortalAuthority {
    let hosted = HostedSessions::new(
        sessions
            .iter()
            .filter_map(|s| parse_addressed(s.as_str()).ok()),
        std::iter::empty(),
    );
    if auth.is_none() && configured.is_empty() {
        return SessionAuthority::implicit(SessionGrant::implicit_local(), hosted);
    }
    let legacy = auth.map(|token| (token, SessionGrant::legacy_default()));
    SessionAuthority::with_credentials(
        legacy
            .into_iter()
            .chain(configured.into_iter().map(|c| (c.token, c.grant))),
        hosted,
    )
}

/// Resolve the portal's configured read credentials from the process
/// environment: every `[[web.credential]]`, then, with `[web]
/// inherit_serve_credentials`, every `[[serve.credential]]` as a read grant
/// with the same scope and no capability (design 4.1, Q5).
///
/// "The same scope" is serve's: an inherited `"*"` covers what `"*"` covers
/// on serve, the `[serve] sessions` of this `lambo.toml` plus every name
/// under any `[[serve.credential]]` prefix (`ServeConfig::hosted_sessions`,
/// the set serve attaches on demand since #32 PR 6), intersected with the
/// portal's allowlist; never the allowlist alone (#4 PR 3 review M1). A
/// session pinned only by `lambo serve --session` is not in the file, so an
/// inherited `"*"` does not reach it: the import narrows, never widens.
///
/// Fails closed, naming the credential and its variable but never a value:
/// an unset, non-UTF-8 or unpresentable token, and one token shared by two
/// credentials (across both tables too). A configured credential equal to
/// the legacy token is [`check_web_credentials`]' refusal. The CLI runs
/// this before any backend is built.
pub fn resolve_web_credentials(
    web: &WebConfig,
    serve: &ServeConfig,
) -> Result<Vec<WebCredential>, CliError> {
    resolve_web_credentials_with(web, serve, |name| std::env::var_os(name))
}

/// [`resolve_web_credentials`] with the environment injected.
pub(super) fn resolve_web_credentials_with(
    web: &WebConfig,
    serve: &ServeConfig,
    lookup: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<Vec<WebCredential>, CliError> {
    let usage = |e: crate::types::LamboError| CliError::Usage(e.to_string());
    web.validate().map_err(usage)?;
    let mut out: Vec<WebCredential> = credential::resolve(
        web.credentials.iter().map(|c| c.entry()),
        &WEB_TABLE,
        &lookup,
        AuthToken::new,
    )
    .map_err(usage)?
    .into_iter()
    .map(read_grant)
    .collect();
    if web.inherit_serve_credentials {
        serve.validate().map_err(usage)?;
        web.validate_with_serve(serve).map_err(usage)?;
        let imported = credential::resolve(
            serve.credentials.iter().map(|c| c.entry()),
            &SERVE_TABLE,
            &lookup,
            AuthToken::new,
        )
        .map_err(usage)?;
        // Serve's `"*"` is serve's hosted set, never the portal's
        // allowlist (review M1): pin it before the grant reaches the
        // portal's authority, which judges `"*"` against the allowlist.
        let hosted = serve.hosted_sessions();
        out.extend(
            imported
                .into_iter()
                .map(|(name, scope, token)| (name, scope.star_within(hosted.clone()), token))
                .map(read_grant),
        );
    }
    Ok(out)
}

/// A resolved entry as a read grant: its scope, never a capability.
fn read_grant(
    (name, scope, token): (String, crate::surface::session::SessionScope, AuthToken),
) -> WebCredential {
    WebCredential {
        grant: SessionGrant::new(name, scope, SessionCapabilities::default()),
        token,
    }
}

/// The checks the whole credential set must pass before the portal starts
/// (the rule `lambo serve` applies, `config::check_credential_set`):
/// reserved names, a name or a token used twice, and a configured token
/// equal to the legacy one. Names credentials, never a token. The CLI runs
/// it before any backend is built; [`super::run`] again, for library
/// callers.
pub fn check_web_credentials(
    legacy: Option<&AuthToken>,
    credentials: &[WebCredential],
) -> Result<(), CliError> {
    let set: Vec<(&str, &AuthToken)> = credentials
        .iter()
        .map(|c| (c.grant.name(), &c.token))
        .collect();
    crate::config::check_credential_set(legacy, &set)
        .map_err(|e| CliError::Usage(format!("credentials: {e}")))
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

/// Each grant's name and how many served sessions it reads, in scan order,
/// for the startup count lines (design 4.1). A session counts when the
/// grant may read it at its own path, or, for a loose single default, at
/// the aliases ([`SessionAuthority::authorize_default`]). Names a
/// credential, never a token.
pub(super) fn credential_reach<'a>(
    authority: &'a PortalAuthority,
    sessions: &[SessionId],
) -> Vec<(&'a str, usize)> {
    authority
        .grants()
        .into_iter()
        .map(|grant| {
            let reach = sessions
                .iter()
                .filter(|s| authority.authorize_default(grant, s.as_str()).is_ok())
                .count();
            (grant.name(), reach)
        })
        .collect()
}

/// Each grant's exact session names that are not served, in scan order,
/// for a startup warning (the one `lambo serve` gives for unpinned names,
/// #32 PR 5 review I5): the portal never reads a session outside its
/// allowlist, so those names are unreachable. Grants with none are left
/// out. Names a credential and sessions, never a token.
pub(super) fn unserved_names<'a>(
    authority: &'a PortalAuthority,
    sessions: &[SessionId],
) -> Vec<(&'a str, Vec<&'a str>)> {
    authority
        .grants()
        .into_iter()
        .filter_map(|grant| {
            let missing: Vec<&str> = grant
                .scope()
                .names()
                .map(|n| n.as_str())
                .filter(|n| !sessions.iter().any(|s| s.as_str() == *n))
                .collect();
            (!missing.is_empty()).then_some((grant.name(), missing))
        })
        .collect()
}

/// The served sessions `grant` may list (design 6.2), in allowlist order:
/// its exact names, or every one when its scope covers every served
/// session (`local`, `default`, a web `"*"`). An inherited `"*"` lists the
/// served sessions `[serve] sessions` pins (review M1). Never a prefix
/// expansion: a prefix grant reads the allowlisted ids under it but lists
/// none of them.
pub(super) fn listable<'a>(grant: &SessionGrant, sessions: &'a [SessionId]) -> Vec<&'a str> {
    let scope = grant.scope();
    sessions
        .iter()
        .map(SessionId::as_str)
        .filter(|s| {
            scope.covers_every_pinned()
                || parse_addressed(s).is_ok_and(|id| scope.names_exactly(&id))
        })
        .collect()
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
    /// neither is refused.
    ///
    /// The presented value is parsed by the rule a configured entry is
    /// ([`AllowedHost::parse`]), so a malformed one is refused even when its
    /// host part is loopback: user info (`evil@localhost`), an empty or
    /// non-numeric port (`localhost:`, `localhost:abc`) (#4 PR 2 review L1).
    /// More than one `Host` header is refused too (review L2; RFC 9112
    /// section 3.2): a proxy in front that read the other one would
    /// disagree with the portal about which host the request is for.
    fn allows(&self, req: &axum::extract::Request) -> bool {
        let Self::Only(allowed) = self else {
            return true;
        };
        let mut hosts = req.headers().get_all(header::HOST).iter();
        let presented = match (hosts.next(), hosts.next()) {
            (Some(value), None) => value.to_str().ok().map(str::to_string),
            (None, _) => req.uri().authority().map(|a| a.as_str().to_string()),
            (Some(_), Some(_)) => return false,
        };
        let Some(presented) = presented.and_then(|h| AllowedHost::parse(&h).ok()) else {
            return false;
        };
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
/// A request carrying two `Authorization` headers is refused like a wrong
/// token, by the extraction `lambo serve` uses
/// ([`presented_authorization`](crate::surface::bearer::presented_authorization);
/// #4 PR 2 review L3): this is the one helper behind both the scoped check
/// and the [`gate`], so the 401 is [`unauthorized`]'s, byte for byte.
///
/// `None` is answered with [`unauthorized`].
pub(super) fn authenticate(
    state: &AppState,
    req: &axum::extract::Request,
) -> Option<Arc<SessionGrant>> {
    let presented = crate::surface::bearer::presented_authorization(req.headers());
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
        req.extensions_mut().insert(Caller(grant));
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
