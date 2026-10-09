//! `[serve]` in `lambo.toml`: multi-session serving (#32 PR 1).
//!
//! PR 1 parsed and validated the table and resolves credential tokens from
//! the environment. Since PR 4 an HTTP `lambo serve` pins `sessions` (beside
//! its `--session` values), serves `default_session` at `/mcp` and enforces
//! `max_attached` on the union (`crate::mcp::pin_sessions`). The rest is
//! parsed and validated but **not enforced yet**, and a serve says so once
//! ([`ServeConfig::warn_if_unenforced`]): PR 5 enforces
//! `[[serve.credential]]`, PR 6 the on-demand bounds (`attach_concurrency`,
//! `idle_detach_secs`, `per_session_rps`), PR 8 the `[[serve.projects]]` cwd
//! map. See the approved design, issue #32, sections 6.1 and 7.1.
//!
//! ```toml
//! [serve]
//! sessions = ["lambo", "rustydocs", "general"]   # pinned
//! default_session = "general"
//! max_attached = 16
//! attach_concurrency = 2
//! idle_detach_secs = 900
//! per_session_rps = 50
//!
//! [[serve.projects]]
//! path = "~/Documents/work/lambo"
//! session = "lambo"
//!
//! [[serve.credential]]
//! name = "agents"
//! token_env = "LAMBO_AGENTS_TOKEN"
//! sessions = ["lambo", "rustydocs", "general"]
//! ```
//!
//! # Level B rules
//!
//! Every table here is `deny_unknown_fields`, and [`ServeConfig::validate`]
//! runs inside [`crate::LamboFile::from_toml_str`], so a malformed `[serve]`
//! table stops every command that reads the file, like any other unknown or
//! invalid key. An older binary refuses a file that has `[serve]` at all
//! (unknown top-level key), which is the existing Level B behaviour.
//!
//! # Secrets never live in the file
//!
//! A credential names the **environment variable** that holds its token
//! (`token_env`). An inline `token = "..."` is a config error, and it is
//! accepted by the parser only so it can be refused by
//! [`ServeConfig::validate`] with a message that says what to do instead
//! (name the variable with `token_env`) rather than a bare unknown-field
//! error. Its value is discarded at parse time ([`InlineToken`]), so it is
//! never held, logged or serialized. (A parse error for any other key does
//! not quote its source line either; see `LamboFile::from_toml_str`.) Token values are read from the
//! environment at runtime by [`ServeConfig::resolve_credentials`] into
//! [`SecretToken`], whose `Debug` redacts.
//!
//! `token_env` itself is where a token is most likely to be pasted by
//! mistake, so it must be a conventional upper-case variable name and must
//! not look like a token (see `secret_env::looks_like_a_token`). A value that fails either
//! check is refused without being quoted, and every later message that names
//! the variable goes through `secret_env::shown`. The rule is shared with
//! `[embedder] api_key_env` in [`super::secret_env`].

use std::collections::BTreeSet;
use std::ffi::OsString;

use serde::{Deserialize, Serialize};

use super::secret_env;
use crate::mcp::SecretToken;
use crate::surface::session::{
    parse_addressed, AddressedSessionId, HostedSessions, SessionCapabilities, SessionGrant,
    SessionPrefix, SessionScope, MAX_ADDRESSED_LEN,
};
use crate::types::LamboError;

mod projects;
pub use projects::{
    MissingSession, SelectedSession, SessionSelectionError, SessionSource, SESSION_REQUIRED,
};

/// Default cap on attached sessions, pinned plus on-demand (#32 §3.6).
pub const DEFAULT_MAX_ATTACHED: usize = 16;

/// Default bound on concurrent attaches (#32 §3.6). PR 6 forces 1 on SQLite.
pub const DEFAULT_ATTACH_CONCURRENCY: usize = 2;

/// Default idle time before an on-demand session is detached (#32 §3.4).
pub const DEFAULT_IDLE_DETACH_SECS: u64 = 900;

/// Credential names a configuration may not use: `default` is what the legacy
/// `--auth-token` / `LAMBO_AUTH_TOKEN` becomes, and `local` is the implicit
/// loopback credential (#32 §6.1). A configured credential with either name
/// would make a log line ambiguous about which one authorized a request.
pub const RESERVED_CREDENTIAL_NAMES: &[&str] = &["default", "local"];

/// The scope entry meaning "every session this serve may host" (#32 §6.1).
pub const EVERY_HOSTED_SESSION: &str = "*";

/// `[serve]`. Every key is optional; an absent table is [`ServeConfig::default`]
/// and changes nothing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ServeConfig {
    /// Pinned sessions: attached at startup, never idle-detached. Each must
    /// pass the strict addressed-id charset (#32 decision 16), because a pinned
    /// session is reachable at `/mcp/s/{session}`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<String>,
    /// What `/mcp` and a stdio serve without `--session` use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_session: Option<String>,
    /// Cap on attached sessions, pinned plus on-demand. Default
    /// [`DEFAULT_MAX_ATTACHED`]; must be >= 1 and >= the pinned count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attached: Option<usize>,
    /// Concurrent attaches. Default [`DEFAULT_ATTACH_CONCURRENCY`]; >= 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attach_concurrency: Option<usize>,
    /// Seconds an on-demand session may sit idle before it is detached.
    /// Default [`DEFAULT_IDLE_DETACH_SECS`]; >= 1. Pinned sessions never are.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_detach_secs: Option<u64>,
    /// Per-session request rate. Defaults to the serve's `--rate-limit-rps`,
    /// so a single-session serve behaves as before; 0 disables the
    /// per-session bucket, as 0 disables the global one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_session_rps: Option<u32>,
    /// The stdio cwd map, `[[serve.projects]]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub projects: Vec<ProjectConfig>,
    /// The credentials, `[[serve.credential]]`.
    #[serde(default, rename = "credential", skip_serializing_if = "Vec::is_empty")]
    pub credentials: Vec<CredentialConfig>,
}

/// One `[[serve.projects]]` entry: a stdio serve whose canonical cwd is under
/// `path` (longest prefix wins) uses `session` (#32 §2.2). Resolved by
/// [`ServeConfig::select_stdio_session`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    /// A directory: absolute, `~` or `~/...`. `~` is expanded to `$HOME` and
    /// the result canonicalized when the map is resolved.
    pub path: String,
    /// The session a stdio serve started under `path` uses.
    pub session: String,
}

/// One `[[serve.credential]]` entry, as written in the file. Holds no secret:
/// only the *name* of the environment variable that does.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CredentialConfig {
    /// A name for logs and refusals. Never the secret.
    #[serde(default)]
    pub name: String,
    /// The environment variable holding the bearer token: `[A-Z_][A-Z0-9_]*`,
    /// at most 64 bytes, and not token-shaped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_env: Option<String>,
    /// An inline `token` key, refused by [`ServeConfig::validate`]. Present
    /// only so the refusal can name `token_env` as the fix; the value itself is
    /// discarded while parsing, see [`InlineToken`].
    #[serde(default, skip_serializing)]
    pub token: Option<InlineToken>,
    /// Exact session names, or `"*"` for every session the serve may host.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<String>,
    /// Every session whose id starts with this and is longer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_prefix: Option<String>,
    /// May attach a session that does not exist yet.
    #[serde(default, skip_serializing_if = "is_false")]
    pub create: bool,
    /// May erase a session in its scope.
    #[serde(default, skip_serializing_if = "is_false")]
    pub erase: bool,
    /// May use the operator surface.
    #[serde(default, skip_serializing_if = "is_false")]
    pub admin: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Marks that a credential carried an inline `token` key. The value is
/// discarded while parsing, whatever its type, so it is never held, printed
/// or serialized; only its presence survives, for [`ServeConfig::validate`]
/// to refuse.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct InlineToken;

impl<'de> Deserialize<'de> for InlineToken {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        serde::de::IgnoredAny::deserialize(d)?;
        Ok(InlineToken)
    }
}

/// A credential resolved for use: its grant plus the token read from its
/// environment variable. `Debug` is safe: [`SecretToken`] redacts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServeCredential {
    /// The name, scope and capabilities.
    pub grant: SessionGrant,
    /// The token, from `token_env`.
    pub token: SecretToken,
}

fn serve_err(msg: impl std::fmt::Display) -> LamboError {
    LamboError::Config(format!("lambo.toml [serve]: {msg}"))
}

/// The strict addressed-id rule, as a config error naming `field` and `value`.
/// Session names are not secrets, so quoting them is fine and useful.
fn addressed(field: &str, value: &str) -> Result<AddressedSessionId, LamboError> {
    parse_addressed(value).map_err(|_| {
        serve_err(format!(
            "{field} {value:?} cannot be addressed by URL: a session named in [serve] must be \
             1 to {MAX_ADDRESSED_LEN} bytes of [A-Za-z0-9._:-] and must not start with '.'. \
             A session outside that rule can still be served on its own with \
             `lambo serve --session <name>`"
        ))
    })
}

/// What `lambo serve` logs once at startup when the file sets a `[serve]`
/// key this release does not enforce yet (credentials, the cwd map, the
/// on-demand bounds): an operator who configured credentials must not think
/// scoping is active. Followed by the key names that are set
/// ([`ServeConfig::unenforced_keys`]); it quotes no value from the table.
pub const SERVE_UNENFORCED_NOTICE: &str = "lambo.toml [serve] is parsed but not yet enforced \
     for some keys in this release; they have no effect yet (#32), and this serve still \
     authenticates only with --auth-token / LAMBO_AUTH_TOKEN";

impl ServeConfig {
    /// Log [`SERVE_UNENFORCED_NOTICE`] once, naming the keys, if this table
    /// sets any key [`ServeConfig::unenforced_keys`] lists for this
    /// transport. `lambo serve` calls it at startup; it never quotes a value.
    pub fn warn_if_unenforced(&self, stdio: bool) {
        let keys = self.unenforced_keys(stdio);
        if !keys.is_empty() {
            tracing::warn!(keys = %keys.join(", "), "{SERVE_UNENFORCED_NOTICE}");
        }
    }

    /// The keys this table sets that a `lambo serve` over this transport
    /// does not enforce yet, by name, each its own entry.
    ///
    /// An HTTP serve enforces `sessions`, `default_session` and
    /// `max_attached` (#32 PR 4). A stdio serve owns its one `--session`, so
    /// `default_session` (what a stdio serve without `--session` will use)
    /// and `[[serve.projects]]` (the stdio cwd map) are listed until #32 PR 8
    /// enforces them; `sessions` and `max_attached` do not apply to stdio.
    pub fn unenforced_keys(&self, stdio: bool) -> Vec<&'static str> {
        let mut keys = Vec::new();
        if !self.credentials.is_empty() {
            keys.push("[[serve.credential]]");
        }
        if !self.projects.is_empty() {
            keys.push("[[serve.projects]]");
        }
        if stdio && self.default_session.is_some() {
            keys.push("default_session");
        }
        if self.attach_concurrency.is_some() {
            keys.push("attach_concurrency");
        }
        if self.idle_detach_secs.is_some() {
            keys.push("idle_detach_secs");
        }
        if self.per_session_rps.is_some() {
            keys.push("per_session_rps");
        }
        keys
    }

    /// Does this table set a key nothing enforces yet? `default_session` and
    /// `[[serve.projects]]` are enforced (stdio session selection, PR 8).
    pub fn has_unenforced_keys(&self) -> bool {
        let rest = Self {
            default_session: None,
            projects: Vec::new(),
            ..self.clone()
        };
        !rest.is_empty()
    }

    /// Is this the empty table (equivalently: no `[serve]` in the file)?
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// The effective attached-session cap.
    pub fn max_attached(&self) -> usize {
        self.max_attached.unwrap_or(DEFAULT_MAX_ATTACHED)
    }

    /// The effective concurrent-attach bound (before PR 6's SQLite clamp).
    pub fn attach_concurrency(&self) -> usize {
        self.attach_concurrency
            .unwrap_or(DEFAULT_ATTACH_CONCURRENCY)
    }

    /// The effective idle-detach time.
    pub fn idle_detach(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.idle_detach_secs.unwrap_or(DEFAULT_IDLE_DETACH_SECS))
    }

    /// The effective per-session rate, defaulting to the serve's global one.
    pub fn per_session_rps(&self, global_rps: u32) -> u32 {
        self.per_session_rps.unwrap_or(global_rps)
    }

    /// Structural validation: everything that can be checked without the
    /// environment or a store. Run by [`crate::LamboFile::from_toml_str`].
    ///
    /// Refuses, naming the key: a session name outside the strict addressed
    /// charset, a duplicate, a zero bound, more pinned sessions than
    /// `max_attached`, and every malformed credential: no or a reserved name,
    /// an inline token, a missing or malformed `token_env` (or the legacy
    /// `LAMBO_AUTH_TOKEN`), an empty scope, a bad prefix, and a name or
    /// `token_env` shared by two entries.
    pub fn validate(&self) -> Result<(), LamboError> {
        let mut pinned = BTreeSet::new();
        for name in &self.sessions {
            let id = addressed("sessions entry", name)?;
            if !pinned.insert(id) {
                return Err(serve_err(format!("sessions lists {name:?} twice")));
            }
        }
        if let Some(default) = &self.default_session {
            addressed("default_session", default)?;
        }
        if self.max_attached == Some(0) {
            return Err(serve_err("max_attached must be >= 1"));
        }
        if pinned.len() > self.max_attached() {
            return Err(serve_err(format!(
                "{} pinned sessions exceed max_attached ({}): every pinned session stays \
                 attached, so raise max_attached or pin fewer sessions",
                pinned.len(),
                self.max_attached()
            )));
        }
        if self.attach_concurrency == Some(0) {
            return Err(serve_err("attach_concurrency must be >= 1"));
        }
        if self.idle_detach_secs == Some(0) {
            return Err(serve_err(
                "idle_detach_secs must be >= 1 (pinned sessions are never idle-detached)",
            ));
        }

        let mut paths = BTreeSet::new();
        for project in &self.projects {
            if project.path.trim().is_empty() {
                return Err(serve_err("a [[serve.projects]] entry has an empty path"));
            }
            projects::check_project_path(&project.path)?;
            addressed("[[serve.projects]] session", &project.session)?;
            if !paths.insert(project.path.as_str()) {
                return Err(serve_err(format!(
                    "[[serve.projects]] lists path {:?} twice",
                    project.path
                )));
            }
        }

        let mut names = BTreeSet::new();
        let mut envs = BTreeSet::new();
        for cred in &self.credentials {
            cred.validate()?;
            if !names.insert(cred.name.as_str()) {
                return Err(serve_err(format!(
                    "two [[serve.credential]] entries are named {:?}",
                    cred.name
                )));
            }
            if let Some(env) = &cred.token_env
                && !envs.insert(env.as_str())
            {
                return Err(serve_err(format!(
                    "two [[serve.credential]] entries read token_env {}; each credential \
                     needs its own token, or a request could not be attributed to one",
                    secret_env::shown(env)
                )));
            }
        }
        Ok(())
    }

    /// What a `"*"` scope expands to: the pinned sessions plus every
    /// credential's prefix. Call after [`ServeConfig::validate`]; an entry that
    /// does not validate is skipped rather than panicking.
    pub fn hosted_sessions(&self) -> HostedSessions {
        HostedSessions::new(
            self.sessions.iter().filter_map(|s| parse_addressed(s).ok()),
            self.credentials
                .iter()
                .filter_map(|c| c.session_prefix.as_deref())
                .filter_map(|p| SessionPrefix::new(p).ok()),
        )
    }

    /// Resolve every credential's token from the process environment.
    ///
    /// Fails closed, naming the credential and its variable but never a value:
    /// an unset, empty, whitespace-only or non-UTF-8 variable is an error (as an
    /// empty `LAMBO_AUTH_TOKEN` is), and two credentials whose variables hold
    /// the same token are an error, because a request presenting it could not
    /// be attributed to one of them.
    pub fn resolve_credentials(&self) -> Result<Vec<ServeCredential>, LamboError> {
        self.resolve_credentials_with(|name| std::env::var_os(name))
    }

    /// [`ServeConfig::resolve_credentials`] with the environment injected.
    pub fn resolve_credentials_with(
        &self,
        lookup: impl Fn(&str) -> Option<OsString>,
    ) -> Result<Vec<ServeCredential>, LamboError> {
        self.validate()?;
        let mut out: Vec<ServeCredential> = Vec::with_capacity(self.credentials.len());
        for cred in &self.credentials {
            // `validate` guarantees `token_env` is present.
            let env = cred.token_env.as_deref().unwrap_or_default();
            let shown = secret_env::shown(env);
            let raw = lookup(env).ok_or_else(|| {
                serve_err(format!(
                    "credential {:?}: environment variable {shown} is not set",
                    cred.name
                ))
            })?;
            let raw = raw.into_string().map_err(|_| {
                serve_err(format!(
                    "credential {:?}: environment variable {shown} is not valid UTF-8",
                    cred.name
                ))
            })?;
            let token = SecretToken::new(raw).map_err(|_| {
                serve_err(format!(
                    "credential {:?}: environment variable {shown} is empty",
                    cred.name
                ))
            })?;
            if let Some(twin) = out.iter().find(|c| c.token == token) {
                return Err(serve_err(format!(
                    "credentials {:?} and {:?} resolve to the same token; give each its own",
                    twin.grant.name(),
                    cred.name
                )));
            }
            out.push(ServeCredential {
                grant: cred.grant()?,
                token,
            });
        }
        Ok(out)
    }
}

impl CredentialConfig {
    /// The checks for one entry; the cross-entry ones are in
    /// [`ServeConfig::validate`].
    fn validate(&self) -> Result<(), LamboError> {
        if self.name.is_empty() {
            return Err(serve_err("a [[serve.credential]] entry has no name"));
        }
        let name = &self.name;
        if parse_addressed(name).is_err() {
            return Err(serve_err(format!(
                "credential name {name:?} must be 1 to {MAX_ADDRESSED_LEN} bytes of \
                 [A-Za-z0-9._:-] and must not start with '.'"
            )));
        }
        if RESERVED_CREDENTIAL_NAMES.contains(&name.as_str()) {
            return Err(serve_err(format!(
                "credential name {name:?} is reserved (\"default\" is the legacy \
                 --auth-token / LAMBO_AUTH_TOKEN credential, \"local\" the implicit loopback \
                 one); choose another name"
            )));
        }
        if self.token.is_some() {
            return Err(serve_err(format!(
                "credential {name:?} has an inline token, which is refused: a secret in \
                 lambo.toml ends up in backups, diffs and support bundles. Put the token in an \
                 environment variable and name it with token_env (the value is not shown here)"
            )));
        }
        let Some(env) = &self.token_env else {
            return Err(serve_err(format!(
                "credential {name:?} has no token_env (the environment variable holding its \
                 token)"
            )));
        };
        secret_env::check(env).map_err(|why| {
            serve_err(why.message(&format!("credential {name:?}: token_env"), "token_env"))
        })?;
        let mut seen = BTreeSet::new();
        for entry in &self.sessions {
            if entry != EVERY_HOSTED_SESSION {
                addressed(&format!("credential {name:?} sessions entry"), entry)?;
            }
            if !seen.insert(entry.as_str()) {
                return Err(serve_err(format!(
                    "credential {name:?} lists session {entry:?} twice"
                )));
            }
        }
        if let Some(prefix) = &self.session_prefix {
            SessionPrefix::new(prefix)
                .map_err(|e| serve_err(format!("credential {name:?}: {e}")))?;
        }
        if self.sessions.is_empty() && self.session_prefix.is_none() {
            return Err(serve_err(format!(
                "credential {name:?} covers no session: give it sessions = [...] and/or \
                 session_prefix"
            )));
        }
        Ok(())
    }

    /// This entry's grant. Call after validation.
    fn grant(&self) -> Result<SessionGrant, LamboError> {
        let mut names = Vec::new();
        let mut every_hosted = false;
        for entry in &self.sessions {
            if entry == EVERY_HOSTED_SESSION {
                every_hosted = true;
            } else {
                names.push(addressed("sessions entry", entry)?);
            }
        }
        let prefix = self
            .session_prefix
            .as_deref()
            .map(SessionPrefix::new)
            .transpose()
            .map_err(serve_err)?;
        Ok(SessionGrant::new(
            self.name.clone(),
            SessionScope::new(names, every_hosted, prefix),
            SessionCapabilities {
                create: self.create,
                erase: self.erase,
                admin: self.admin,
            },
        ))
    }
}

#[cfg(test)]
mod tests;
