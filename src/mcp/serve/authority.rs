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
    parse_addressed, BearerSecret, HostedSessions, RefusalReason, SessionAuthority, SessionGrant,
    SessionNeed, SessionRefusal, LEGACY_CREDENTIAL_NAME, LOCAL_CREDENTIAL_NAME,
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

/// Does any configured credential reach a session this serve does not pin
/// (#32 PR 6)? A `session_prefix`, or an exact name that is not pinned. Such
/// a serve attaches those sessions on demand, so it runs as a registry
/// under `DetachSession` even with one pinned session. The legacy `default`
/// and implicit `local` credentials cover the pinned sessions only, and a
/// `"*"` reaches past them only through another credential's prefix, so
/// neither changes the answer. HTTP only: stdio authenticates nobody.
pub(super) fn reaches_past_pinned(opts: &ServeOptions) -> bool {
    on_demand_credentials(opts) > 0
}

/// How many configured credentials reach a session this serve does not pin
/// (see [`reaches_past_pinned`]); 0 over stdio. The on-demand places are
/// shared among them for eviction (#32 PR 6 review M4).
pub(super) fn on_demand_credentials(opts: &ServeOptions) -> usize {
    if opts.transport != Transport::Http {
        return 0;
    }
    opts.credentials
        .iter()
        .filter(|cred| {
            let scope = cred.grant.scope();
            scope.prefix().is_some()
                || scope
                    .names()
                    .any(|name| !opts.sessions.iter().any(|pinned| pinned == name.as_str()))
        })
        .count()
}

/// What an operator should hear about a serve's credentials at startup
/// (#32 PR 5 review I3 and I5), one line each. Names credentials and
/// sessions, never a token.
///
/// * The legacy token beside configured credentials: a `LAMBO_AUTH_TOKEN`
///   left exported (in a plist, a unit file) after `[[serve.credential]]`
///   was added keeps the `default` credential, and with it every pinned
///   session, reachable by whoever holds that token.
/// * A configured credential without `create` whose scope reaches past the
///   pinned sessions (#32 PR 6): it attaches such a session on demand only
///   once the session exists (it has a lease row, design decision 3), so a
///   name nobody has created yet is the uniform 404 to it.
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
    for cred in &opts.credentials {
        if cred.grant.capabilities().create {
            continue;
        }
        let scope = cred.grant.scope();
        let unpinned: Vec<&str> = scope
            .names()
            .filter(|name| !opts.sessions.iter().any(|pinned| pinned == name.as_str()))
            .map(|name| name.as_str())
            .collect();
        let mut reach = unpinned.join(", ");
        if let Some(prefix) = scope.prefix() {
            if !reach.is_empty() {
                reach.push_str(", ");
            }
            reach.push_str(&format!("prefix {:?}", prefix.as_str()));
        }
        if !reach.is_empty() {
            out.push(format!(
                "credential {:?} reaches sessions this serve does not pin ({reach}) without \
                 create = true: it attaches one on demand only once it exists (a writer has \
                 used it), and gets the uniform 404 for one that does not",
                cred.grant.name()
            ));
        }
    }
    out
}

/// May `grant` use the default session `default` (what `/mcp` serves)?
///
/// A default whose name passes the strict charset is authorized exactly as
/// `/mcp/s/{default}` would be. One that does not (a one-session serve keeps
/// `--session`'s looser rule, and such a session is reachable only at
/// `/mcp`) can be covered by no exact name or prefix, so only a scope over
/// every pinned session reaches it: `"*"`, and the `default` and `local`
/// grants.
pub(super) fn authorize_default(
    authority: &ServeAuthority,
    grant: &SessionGrant,
    default: &str,
) -> Result<(), SessionRefusal> {
    if parse_addressed(default).is_ok() {
        return authority
            .authorize(grant, default, SessionNeed::Use)
            .map(|_| ());
    }
    if grant.scope().covers_every_pinned() {
        Ok(())
    } else {
        Err(SessionRefusal::new(RefusalReason::OutOfScope))
    }
}
