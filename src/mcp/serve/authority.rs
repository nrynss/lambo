//! #32 PR 5: who may reach which session over HTTP (design §6.1, §6.2).
//!
//! A serve's credential set is built once, in the pre-lease group, from the
//! legacy token and `[[serve.credential]]`:
//!
//! | configured | bind | credentials |
//! |---|---|---|
//! | nothing | loopback | the implicit `local` grant: every request, no header checked (today's loopback behaviour) |
//! | nothing | anywhere else | refused at startup (`authorize_bind`) |
//! | `--auth-token` / `LAMBO_AUTH_TOKEN` | any | `default`: every pinned session, no `create`, `erase` or `admin` |
//! | `[[serve.credential]]` | any | each entry's own scope and flags, beside `default` if a legacy token is set too |
//!
//! Once any credential exists the implicit `local` is gone, so a loopback
//! serve with `[[serve.credential]]` requires a bearer token on every
//! request.
//!
//! The order every request follows is §6.2's: the guard resolves the bearer
//! to a grant (401 when none, before anything about sessions is evaluated),
//! then the router parses the addressed id and checks the grant's scope, in
//! memory, **before** the registry is consulted. Every refusal at those two
//! steps is `surface::session`'s byte-identical 404, so a caller outside a
//! session's scope cannot tell a held, detaching or failed session from one
//! this serve does not host (PR 4 review L6). Only inside its scope can a
//! caller see a 503.

use std::sync::Arc;

use super::http_guards::SecretToken;
use super::{ServeOptions, Transport};
use crate::config::ServeCredential;
use crate::surface::session::{
    parse_addressed, BearerSecret, HostedSessions, SessionAuthority, SessionGrant, SessionRefusal,
    LEGACY_CREDENTIAL_NAME, LOCAL_CREDENTIAL_NAME,
};
use crate::types::LamboError;

/// A serve's credential set.
pub(super) type ServeAuthority = SessionAuthority<SecretToken>;

impl BearerSecret for SecretToken {
    fn secret_bytes(&self) -> &[u8] {
        self.as_bytes()
    }
}

/// The grant the guard resolved for a request, carried to the router in the
/// request's extensions. A request that reaches the router without one (a
/// router served without the guard) is refused, never served.
#[derive(Clone)]
pub(super) struct Authenticated(pub(super) Arc<SessionGrant>);

fn credential_err(msg: impl std::fmt::Display) -> LamboError {
    LamboError::Config(format!("lambo serve credentials: {msg}"))
}

/// The checks a credential set must pass before a serve starts, shared by
/// the CLI's preflight (before any backend is built) and [`serve_authority`]
/// (so a library caller meets them too). Names credentials, never a token.
///
/// Refuses: a configured credential named `default` or `local` (the
/// `[serve]` parser refuses those, a library caller might not), two
/// credentials with one name, two credentials with one token, and a
/// configured token equal to the legacy `--auth-token` / `LAMBO_AUTH_TOKEN`
/// one (#32 PR 1 review): a request presenting it could not be attributed to
/// one credential, and the two may carry different scopes.
pub fn check_serve_credentials(
    legacy: Option<&SecretToken>,
    credentials: &[ServeCredential],
) -> Result<(), LamboError> {
    for (i, cred) in credentials.iter().enumerate() {
        let name = cred.grant.name();
        if name == LEGACY_CREDENTIAL_NAME || name == LOCAL_CREDENTIAL_NAME {
            return Err(credential_err(format!(
                "credential name {name:?} is reserved (\"{LEGACY_CREDENTIAL_NAME}\" is the \
                 legacy --auth-token / LAMBO_AUTH_TOKEN credential, \"{LOCAL_CREDENTIAL_NAME}\" \
                 the implicit loopback one)"
            )));
        }
        for earlier in &credentials[..i] {
            if earlier.grant.name() == name {
                return Err(credential_err(format!(
                    "two credentials are named {name:?}"
                )));
            }
            if earlier.token == cred.token {
                return Err(credential_err(format!(
                    "credentials {:?} and {name:?} resolve to the same token; give each its own",
                    earlier.grant.name()
                )));
            }
        }
        if legacy.is_some_and(|legacy| *legacy == cred.token) {
            return Err(credential_err(format!(
                "credential {name:?} resolves to the same token as --auth-token / \
                 LAMBO_AUTH_TOKEN (the legacy \"{LEGACY_CREDENTIAL_NAME}\" credential); give \
                 each its own, or drop the legacy token (the value is not shown here)"
            )));
        }
    }
    Ok(())
}

/// Some credential `authorize_bind` can count: the legacy token, else the
/// first configured one. `None` means the serve has no credential at all.
pub(super) fn any_credential(opts: &ServeOptions) -> Option<&SecretToken> {
    opts.auth_token
        .as_ref()
        .or_else(|| opts.credentials.first().map(|c| &c.token))
}

/// Build the serve's credential set (see the module table). Run in the
/// pre-lease group, after `authorize_bind`, so a refusal takes no lease.
pub(super) fn serve_authority(opts: &ServeOptions) -> Result<ServeAuthority, LamboError> {
    check_serve_credentials(opts.auth_token.as_ref(), &opts.credentials)?;
    let hosted = HostedSessions::new(
        // A pinned name the strict charset refuses (one session, the loose
        // `--session` rule) is reached only at `/mcp`; see
        // [`authorize_default`].
        opts.sessions.iter().filter_map(|s| parse_addressed(s).ok()),
        opts.credentials
            .iter()
            .filter_map(|c| c.grant.scope().prefix().cloned()),
    );
    if opts.auth_token.is_none() && opts.credentials.is_empty() {
        if opts.transport == Transport::Http && !opts.bind.is_loopback() {
            // `authorize_bind` refuses this first; kept so the set itself
            // can never be implicit off loopback.
            return Err(credential_err(format!(
                "no credential is configured for --bind {}; an implicit credential exists only \
                 on loopback",
                opts.bind
            )));
        }
        return Ok(ServeAuthority::implicit(
            SessionGrant::implicit_local(),
            hosted,
        ));
    }
    let legacy = opts
        .auth_token
        .clone()
        .map(|token| (token, SessionGrant::legacy_default()));
    let configured = opts
        .credentials
        .iter()
        .map(|c| (c.token.clone(), c.grant.clone()));
    Ok(ServeAuthority::with_credentials(
        legacy.into_iter().chain(configured),
        hosted,
    ))
}

/// What an operator should hear about a serve's credentials at startup
/// (#32 PR 5 review I3 and I5), one line each. Names credentials and
/// sessions, never a token.
///
/// * The legacy token beside configured credentials: a `LAMBO_AUTH_TOKEN`
///   left exported (in a plist, a unit file) after `[[serve.credential]]`
///   was added keeps the `default` credential, and with it every pinned
///   session, reachable by whoever holds that token.
/// * A configured credential naming sessions this serve does not pin: until
///   sessions attach on demand (#32 PR 6) those names cannot be reached,
///   and a credential whose scope covers no pinned session at all reaches
///   nothing.
pub(super) fn startup_warnings(opts: &ServeOptions) -> Vec<String> {
    let mut out = Vec::new();
    if opts.transport != Transport::Http {
        return out;
    }
    if opts.auth_token.is_some() && !opts.credentials.is_empty() {
        out.push(format!(
            "the legacy --auth-token / LAMBO_AUTH_TOKEN is set beside [[serve.credential]]: it \
             is the \"{LEGACY_CREDENTIAL_NAME}\" credential and reaches every pinned session. \
             Unset it if the configured credentials replace it."
        ));
    }
    let pinned: Vec<_> = opts
        .sessions
        .iter()
        .filter_map(|s| parse_addressed(s).ok())
        .collect();
    let hosted = HostedSessions::new(pinned.iter().cloned(), std::iter::empty());
    for cred in &opts.credentials {
        let scope = cred.grant.scope();
        let unpinned: Vec<&str> = scope
            .names()
            .filter(|name| !pinned.contains(name))
            .map(|name| name.as_str())
            .collect();
        if !unpinned.is_empty() {
            out.push(format!(
                "credential {:?} names sessions this serve does not pin ({}): they cannot be \
                 reached until sessions attach on demand; pin them with --session or [serve] \
                 sessions",
                cred.grant.name(),
                unpinned.join(", ")
            ));
        }
        if !pinned.iter().any(|id| scope.covers(id, &hosted)) {
            out.push(format!(
                "credential {:?} covers no session this serve pins, so it reaches nothing yet",
                cred.grant.name()
            ));
        }
    }
    out
}

/// May `grant` use the default session `default` (what `/mcp` serves)?
/// [`SessionAuthority::authorize_default`], shared with the portal's
/// unscoped aliases (#4 PR 2).
///
/// [`SessionAuthority::authorize_default`]: crate::surface::session::SessionAuthority::authorize_default
pub(super) fn authorize_default(
    authority: &ServeAuthority,
    grant: &SessionGrant,
    default: &str,
) -> Result<(), SessionRefusal> {
    authority.authorize_default(grant, default)
}
