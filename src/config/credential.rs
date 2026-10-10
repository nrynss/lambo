//! The credential grammar `[[serve.credential]]` (#32 PR 1) and
//! `[[web.credential]]` (#4 PR 3) share: a name, the environment variable
//! holding the token, and a scope of exact session names, `"*"` and/or one
//! prefix.
//!
//! The rules live here once, parameterized by the table a refusal names
//! ([`CredentialTable`]), so the two tables cannot drift: the reserved
//! names, the inline-token refusal, the shared `token_env` rule
//! ([`super::secret_env`]), the strict addressed charset for every scope
//! entry, a duplicate entry, an empty scope, and a name or `token_env` used
//! twice. What differs stays with each table: `[[serve.credential]]` carries
//! the `create` / `erase` / `admin` capabilities, `[[web.credential]]` is
//! read-only and refuses them by name.
//!
//! Tokens are resolved from the environment by [`resolve`], into each
//! surface's own redacting secret type, never held as a bare `String` past
//! the call. Every message names the credential and the variable (through
//! [`secret_env::shown`]), never a value.

use std::collections::BTreeSet;
use std::ffi::OsString;

use super::secret_env;
use crate::surface::session::{
    parse_addressed, AddressedSessionId, SessionPrefix, SessionScope, LEGACY_CREDENTIAL_NAME,
    LOCAL_CREDENTIAL_NAME, MAX_ADDRESSED_LEN,
};
use crate::types::LamboError;

/// Credential names a configuration may not use: `default` is what the legacy
/// `--auth-token` / `LAMBO_AUTH_TOKEN` becomes, and `local` is the implicit
/// loopback credential (#32 §6.1). A configured credential with either name
/// would make a log line ambiguous about which one authorized a request.
pub const RESERVED_CREDENTIAL_NAMES: &[&str] = &[LEGACY_CREDENTIAL_NAME, LOCAL_CREDENTIAL_NAME];

/// The scope entry meaning "every session this surface may host" (#32
/// §6.1): for `lambo serve` the pinned sessions plus every credential's
/// prefix, for `lambo serve-web` the allowlist (#4 design 4.1).
pub const EVERY_HOSTED_SESSION: &str = "*";

/// Which table a credential sits in, for the refusals' wording.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CredentialTable {
    /// The top-level table, as a refusal is prefixed: `[serve]`.
    pub(crate) table: &'static str,
    /// The array of tables, as a refusal names it: `[[serve.credential]]`.
    pub(crate) entry: &'static str,
    /// The command that serves a session outside the strict charset on its
    /// own, for the addressed-id refusal's hint.
    pub(crate) command: &'static str,
}

/// `[[serve.credential]]`.
pub(crate) const SERVE_TABLE: CredentialTable = CredentialTable {
    table: "[serve]",
    entry: "[[serve.credential]]",
    command: "lambo serve",
};

impl CredentialTable {
    /// A config error prefixed with the file and this table.
    pub(crate) fn err(&self, msg: impl std::fmt::Display) -> LamboError {
        LamboError::Config(format!("lambo.toml {}: {msg}", self.table))
    }

    /// The strict addressed-id rule, as a config error naming `field` and
    /// `value`. Session names are not secrets, so quoting them is fine and
    /// useful.
    pub(crate) fn addressed(
        &self,
        field: &str,
        value: &str,
    ) -> Result<AddressedSessionId, LamboError> {
        parse_addressed(value).map_err(|_| {
            self.err(format!(
                "{field} {value:?} cannot be addressed by URL: a session named in {} must be \
                 1 to {MAX_ADDRESSED_LEN} bytes of [A-Za-z0-9._:-] and must not start with \
                 '.'. A session outside that rule can still be served on its own with `{} \
                 --session <name>`",
                self.table, self.command
            ))
        })
    }
}

/// The shared fields of one credential entry, borrowed from either table's
/// parsed form.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CredentialEntry<'a> {
    pub(crate) name: &'a str,
    pub(crate) token_env: Option<&'a str>,
    /// The entry carried an inline `token` key (its value already
    /// discarded by the parser).
    pub(crate) inline_token: bool,
    pub(crate) sessions: &'a [String],
    pub(crate) session_prefix: Option<&'a str>,
}

impl CredentialEntry<'_> {
    /// The checks for one entry; the cross-entry ones are in
    /// [`validate_set`].
    pub(crate) fn validate(&self, t: &CredentialTable) -> Result<(), LamboError> {
        if self.name.is_empty() {
            return Err(t.err(format!("a {} entry has no name", t.entry)));
        }
        let name = self.name;
        if parse_addressed(name).is_err() {
            return Err(t.err(format!(
                "credential name {name:?} must be 1 to {MAX_ADDRESSED_LEN} bytes of \
                 [A-Za-z0-9._:-] and must not start with '.'"
            )));
        }
        if RESERVED_CREDENTIAL_NAMES.contains(&name) {
            return Err(t.err(format!(
                "credential name {name:?} is reserved (\"default\" is the legacy \
                 --auth-token / LAMBO_AUTH_TOKEN credential, \"local\" the implicit loopback \
                 one); choose another name"
            )));
        }
        if self.inline_token {
            return Err(t.err(format!(
                "credential {name:?} has an inline token, which is refused: a secret in \
                 lambo.toml ends up in backups, diffs and support bundles. Put the token in an \
                 environment variable and name it with token_env (the value is not shown here)"
            )));
        }
        let Some(env) = self.token_env else {
            return Err(t.err(format!(
                "credential {name:?} has no token_env (the environment variable holding its \
                 token)"
            )));
        };
        secret_env::check(env).map_err(|why| {
            t.err(why.message(&format!("credential {name:?}: token_env"), "token_env"))
        })?;
        let mut seen = BTreeSet::new();
        for entry in self.sessions {
            if entry != EVERY_HOSTED_SESSION {
                t.addressed(&format!("credential {name:?} sessions entry"), entry)?;
            }
            if !seen.insert(entry.as_str()) {
                return Err(t.err(format!("credential {name:?} lists session {entry:?} twice")));
            }
        }
        if let Some(prefix) = self.session_prefix {
            SessionPrefix::new(prefix).map_err(|e| t.err(format!("credential {name:?}: {e}")))?;
        }
        if self.sessions.is_empty() && self.session_prefix.is_none() {
            return Err(t.err(format!(
                "credential {name:?} covers no session: give it sessions = [...] and/or \
                 session_prefix"
            )));
        }
        Ok(())
    }

    /// This entry's scope. Call after validation.
    pub(crate) fn scope(&self, t: &CredentialTable) -> Result<SessionScope, LamboError> {
        let mut names = Vec::new();
        let mut every_hosted = false;
        for entry in self.sessions {
            if entry == EVERY_HOSTED_SESSION {
                every_hosted = true;
            } else {
                names.push(t.addressed("sessions entry", entry)?);
            }
        }
        let prefix = self
            .session_prefix
            .map(SessionPrefix::new)
            .transpose()
            .map_err(|e| t.err(e))?;
        Ok(SessionScope::new(names, every_hosted, prefix))
    }
}

/// Every entry's own checks, then the cross-entry ones: no two entries share
/// a name or a `token_env`.
pub(crate) fn validate_set<'a>(
    entries: impl IntoIterator<Item = CredentialEntry<'a>>,
    t: &CredentialTable,
) -> Result<(), LamboError> {
    let mut names = BTreeSet::new();
    let mut envs = BTreeSet::new();
    for cred in entries {
        cred.validate(t)?;
        if !names.insert(cred.name) {
            return Err(t.err(format!("two {} entries are named {:?}", t.entry, cred.name)));
        }
        if let Some(env) = cred.token_env
            && !envs.insert(env)
        {
            return Err(t.err(format!(
                "two {} entries read token_env {}; each credential needs its own token, or a \
                 request could not be attributed to one",
                t.entry,
                secret_env::shown(env)
            )));
        }
    }
    Ok(())
}

/// Resolve each entry's token from `lookup` into the surface's secret type
/// with `make` (whose refusal reason never quotes the value), in order,
/// beside the entry's scope.
///
/// Fails closed, naming the credential and its variable but never a value:
/// an unset, non-UTF-8 or refused variable is an error, and two entries
/// whose variables hold the same token are an error, because a request
/// presenting it could not be attributed to one of them. Call after
/// [`validate_set`]: an entry without `token_env` reads the empty name.
pub(crate) fn resolve<'a, T: PartialEq>(
    entries: impl IntoIterator<Item = CredentialEntry<'a>>,
    t: &CredentialTable,
    lookup: impl Fn(&str) -> Option<OsString>,
    make: impl Fn(String) -> Result<T, String>,
) -> Result<Vec<(String, SessionScope, T)>, LamboError> {
    let mut out: Vec<(String, SessionScope, T)> = Vec::new();
    for cred in entries {
        let env = cred.token_env.unwrap_or_default();
        let shown = secret_env::shown(env);
        let raw = lookup(env).ok_or_else(|| {
            t.err(format!(
                "credential {:?}: environment variable {shown} is not set",
                cred.name
            ))
        })?;
        let raw = raw.into_string().map_err(|_| {
            t.err(format!(
                "credential {:?}: environment variable {shown} is not valid UTF-8",
                cred.name
            ))
        })?;
        let token = make(raw).map_err(|why| {
            t.err(format!(
                "credential {:?}: environment variable {shown}: {why}",
                cred.name
            ))
        })?;
        if let Some((twin, _, _)) = out.iter().find(|(_, _, other)| *other == token) {
            return Err(t.err(format!(
                "credentials {twin:?} and {:?} resolve to the same token; give each its own",
                cred.name
            )));
        }
        out.push((cred.name.to_string(), cred.scope(t)?, token));
    }
    Ok(out)
}
