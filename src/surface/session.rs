//! Session addressing and session authority, shared by every surface that
//! names a session in a request (#32 PR 1).
//!
//! `lambo serve --transport http` will route `/mcp/s/{session}` (#32 PR 4) and
//! the web portal will route `/s/{session}` (#4). Both take a session id from
//! a URL path, both decide whether the presented credential may reach it, and
//! both must refuse in a way that tells the caller nothing about which sessions
//! exist. Those three rules live here once so the two surfaces cannot drift,
//! which is refactor-25's stated home for session-addressing validation.
//!
//! * [`parse_addressed`]: the strict charset for an id taken from a request
//!   (#32 decision 16). `--session` on the command line keeps the looser
//!   [`crate::surface::validate::check_size`] rule, so no deployed session name
//!   breaks; only *addressed* ids use this one.
//! * [`SessionGrant`] / [`SessionScope`] / [`HostedSessions`]: what one
//!   credential may address and do, checked **in memory only**. Nothing in this
//!   module can reach a store: the types take no store handle, so "an
//!   unauthorized request makes zero store calls" (#32 §6.2) holds by
//!   construction for the part of the check that lives here.
//! * [`SessionAuthority`]: a surface's whole credential set (#32 PR 5): which
//!   grant a presented bearer token resolves to, scanned in constant time
//!   across every credential, or the implicit loopback grant when none is
//!   configured. Generic over the surface's own secret type, so the token
//!   itself never leaves the type that redacts it.
//! * [`SessionRefusal`] and [`not_found_response`]: the one uniform 404. A
//!   malformed id, an id outside the credential's scope, and a capability the
//!   credential lacks all produce byte-identical responses (status, headers and
//!   body), and those bytes are axum's own unrouted-path 404, so a refused
//!   session is also indistinguishable from a path that was never routed.
//!
//! `lambo serve --transport http` routes `/mcp` and `/mcp/s/{session}`
//! through it (#32 PR 4) and authenticates and authorizes every request with
//! it (PR 5); PR 7's admin routes and #4's portal are to use it too.
//!
//! # The fixed order (#32 §6.2)
//!
//! 1. Bearer check, by the surface (401 with that surface's body). Nothing
//!    about sessions is evaluated before it passes.
//! 2. Id shape: [`parse_addressed`].
//! 3. Scope and capability: [`SessionGrant::authorize`].
//!
//! [`authorize_addressed`] runs steps 2 and 3 in that order. Any failure in
//! either is a [`SessionRefusal`], and every refusal renders the same 404.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

/// Longest addressed session id, in bytes (#32 decision 16).
pub const MAX_ADDRESSED_LEN: usize = 128;

/// The uniform refusal's status. See [`not_found_response`].
pub const NOT_FOUND_STATUS: StatusCode = StatusCode::NOT_FOUND;

/// The uniform refusal's body: empty, as axum's unrouted-path 404 is.
pub const NOT_FOUND_BODY: &[u8] = b"";

/// Is `b` in the addressed-id charset `[A-Za-z0-9._:-]`?
///
/// Every byte of that set is ASCII, so a multi-byte UTF-8 sequence fails on its
/// first byte and the check never has to decode. `%` is outside the set, which
/// is how "no percent-decoding" is enforced: a percent-encoded id is refused as
/// malformed rather than decoded into something else.
fn is_addressed_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-')
}

/// A session id that passed [`parse_addressed`]: 1 to [`MAX_ADDRESSED_LEN`]
/// bytes of `[A-Za-z0-9._:-]`, not starting with `.`.
///
/// A distinct type from [`crate::types::SessionId`] on purpose. That one is any
/// session name the store accepts (the CLI's looser rule); this one is proof
/// that the strict addressing rule was applied, so a routing layer cannot hand
/// an unchecked path segment to an attach.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AddressedSessionId(String);

impl AddressedSessionId {
    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The id as the crate-wide session type, for an attach or a store call.
    pub fn to_session_id(&self) -> crate::types::SessionId {
        crate::types::SessionId(self.0.clone())
    }
}

impl fmt::Display for AddressedSessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for AddressedSessionId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Why a request was refused. **Never rendered to the caller**: every reason
/// produces the same [`not_found_response`]. It exists for the operator's log
/// and for tests that prove each path refuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefusalReason {
    /// The id failed [`parse_addressed`].
    Malformed,
    /// The id is well formed but outside the credential's scope.
    OutOfScope,
    /// The id is in scope but the credential lacks the capability the request
    /// needs (`create`, `erase` or `admin`).
    MissingCapability,
    /// The id is in scope and the session does not exist, and the credential
    /// may not create it. Decided by the caller after a store probe, which the
    /// caller may make only once the in-memory checks above have passed.
    Absent,
}

/// A refusal that must not reveal whether a session exists (#32 §6.2, #4).
///
/// Whatever the [`RefusalReason`], [`SessionRefusal::not_found_response`]
/// renders the same bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionRefusal {
    reason: RefusalReason,
}

impl SessionRefusal {
    /// A refusal for `reason`.
    pub fn new(reason: RefusalReason) -> Self {
        Self { reason }
    }

    /// Why, for the operator's log only.
    pub fn reason(&self) -> RefusalReason {
        self.reason
    }

    /// The uniform 404. Identical for every reason; see [`not_found_response`].
    pub fn not_found_response(&self) -> Response {
        not_found_response()
    }
}

impl fmt::Display for SessionRefusal {
    /// The operator-facing text. Deliberately names only the reason class,
    /// never the id, so a log line built from it cannot carry a probed name
    /// into a shared log.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = match self.reason {
            RefusalReason::Malformed => "malformed session id",
            RefusalReason::OutOfScope => "session outside the credential's scope",
            RefusalReason::MissingCapability => "credential lacks the required capability",
            RefusalReason::Absent => "session does not exist and may not be created",
        };
        f.write_str(what)
    }
}

/// The one 404 every session refusal renders: status 404, no headers of its
/// own, empty body. On the wire the server adds `content-length: 0`, as it
/// does for any empty response.
///
/// Exactly what axum's own router answers for a path nothing routes
/// (`StatusCode::NOT_FOUND.into_response()`, axum 0.8 `routing/not_found.rs`),
/// so a refused session id cannot be told apart from an unrouted path either.
/// That holds only if every layer in front of it (the bearer guard, transport
/// headers) is added with `.layer`, not `.route_layer`, and the session route
/// answers every method (`any`), or an unrouted method gets 405 plus `Allow`.
/// Both are pinned by a wire-level test.
pub fn not_found_response() -> Response {
    NOT_FOUND_STATUS.into_response()
}

/// Validate a session id taken from a request (#32 decision 16).
///
/// Accepts 1 to [`MAX_ADDRESSED_LEN`] bytes of `[A-Za-z0-9._:-]` that do not
/// start with `.`. Performs **no percent-decoding**: `%` is outside the
/// charset, so `a%2Fb` is refused rather than turned into `a/b`. The leading
/// dot rule refuses `.` and `..` (and hidden-file-shaped names) outright.
///
/// Every failure is the same [`SessionRefusal`] with
/// [`RefusalReason::Malformed`], so the caller renders the uniform 404.
pub fn parse_addressed(raw: &str) -> Result<AddressedSessionId, SessionRefusal> {
    if addressed_shape_ok(raw) {
        Ok(AddressedSessionId(raw.to_owned()))
    } else {
        Err(SessionRefusal::new(RefusalReason::Malformed))
    }
}

/// The shape rule behind [`parse_addressed`], as a predicate.
fn addressed_shape_ok(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_ADDRESSED_LEN
        && bytes[0] != b'.'
        && bytes.iter().all(|b| is_addressed_byte(*b))
}

/// A `session_prefix` scope: every addressed id that starts with it and is
/// strictly longer than it.
///
/// Validated like an addressed id (same charset, no leading `.`), and shorter
/// than [`MAX_ADDRESSED_LEN`] so at least one id can fit under it. The bare
/// prefix is **not** in its own scope: `dc-u-` names no user, and an app that
/// builds `{prefix}{user}` with an empty user has a bug the scope should not
/// paper over.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionPrefix(String);

impl SessionPrefix {
    /// Validate a prefix. The error text names the rule, for a config error.
    pub fn new(raw: &str) -> Result<Self, String> {
        if raw.len() >= MAX_ADDRESSED_LEN || !addressed_shape_ok(raw) {
            return Err(format!(
                "session_prefix must be 1 to {} bytes of [A-Za-z0-9._:-] and must not start \
                 with '.'",
                MAX_ADDRESSED_LEN - 1
            ));
        }
        Ok(Self(raw.to_owned()))
    }

    /// The prefix as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Is `id` inside this prefix (starts with it, and is longer)?
    pub fn covers(&self, id: &AddressedSessionId) -> bool {
        id.as_str().len() > self.0.len() && id.as_str().starts_with(&self.0)
    }
}

/// The sessions a serve may host, which is what a `"*"` scope expands to:
/// every pinned name plus every name inside any credential's prefix (#32
/// §6.1).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostedSessions {
    pinned: BTreeSet<AddressedSessionId>,
    prefixes: Vec<SessionPrefix>,
}

impl HostedSessions {
    /// Build from the pinned names and every configured prefix.
    pub fn new(
        pinned: impl IntoIterator<Item = AddressedSessionId>,
        prefixes: impl IntoIterator<Item = SessionPrefix>,
    ) -> Self {
        Self {
            pinned: pinned.into_iter().collect(),
            prefixes: prefixes.into_iter().collect(),
        }
    }

    /// Is `id` one of the sessions this serve may host?
    pub fn contains(&self, id: &AddressedSessionId) -> bool {
        self.is_pinned(id) || self.prefixes.iter().any(|p| p.covers(id))
    }

    /// Is `id` one of the pinned sessions?
    pub fn is_pinned(&self, id: &AddressedSessionId) -> bool {
        self.pinned.contains(id)
    }
}

/// Which sessions one credential may address.
///
/// The union of four parts, any of which may be empty: exact names, `"*"`
/// (every hosted session, see [`HostedSessions`]), one prefix, and "every
/// pinned session" ([`SessionScope::pinned`], the scope of the legacy
/// `default` and implicit `local` credentials, which no configured credential
/// can express). An empty scope covers nothing; the config layer refuses to
/// build one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionScope {
    names: BTreeSet<AddressedSessionId>,
    every_hosted: bool,
    prefix: Option<SessionPrefix>,
    every_pinned: bool,
}

impl SessionScope {
    /// A scope from its parts.
    pub fn new(
        names: impl IntoIterator<Item = AddressedSessionId>,
        every_hosted: bool,
        prefix: Option<SessionPrefix>,
    ) -> Self {
        Self {
            names: names.into_iter().collect(),
            every_hosted,
            prefix,
            every_pinned: false,
        }
    }

    /// Every pinned session and nothing else (#32 §6.1): the scope of the
    /// legacy `default` credential and of the implicit `local` one. Narrower
    /// than `"*"`, which also covers every name inside any credential's
    /// prefix.
    pub fn pinned() -> Self {
        Self {
            every_pinned: true,
            ..Self::default()
        }
    }

    /// Does this scope cover nothing at all?
    pub fn is_empty(&self) -> bool {
        self.names.is_empty() && !self.every_hosted && self.prefix.is_none() && !self.every_pinned
    }

    /// Does this scope cover every pinned session, whatever its name? True
    /// for `"*"` and for [`SessionScope::pinned`]. A serve uses it for a
    /// pinned session whose name the strict charset refuses (a one-session
    /// serve keeps `--session`'s looser rule and is reached only at `/mcp`):
    /// no exact name or prefix can cover such a session, because none can
    /// spell it.
    pub fn covers_every_pinned(&self) -> bool {
        self.every_hosted || self.every_pinned
    }

    /// The scope's exact session names.
    pub fn names(&self) -> impl Iterator<Item = &AddressedSessionId> {
        self.names.iter()
    }

    /// The scope's prefix, if it has one (feeds [`HostedSessions`]).
    pub fn prefix(&self) -> Option<&SessionPrefix> {
        self.prefix.as_ref()
    }

    /// Does this scope cover `id`, given what the serve hosts?
    pub fn covers(&self, id: &AddressedSessionId, hosted: &HostedSessions) -> bool {
        self.names.contains(id)
            || self.prefix.as_ref().is_some_and(|p| p.covers(id))
            || (self.every_hosted && hosted.contains(id))
            || (self.every_pinned && hosted.is_pinned(id))
    }
}

/// What a request needs beyond reaching the session (#32 §6.1).
///
/// Read and write are implied by scope: there is no read-only MCP credential in
/// v1, because the MCP tools mix reads and writes in one session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionNeed {
    /// Use an existing (or pinned) session: every MCP request.
    Use,
    /// Attach a session that does not exist yet.
    Create,
    /// Erase the session (`POST /admin/s/{s}/erase`).
    Erase,
    /// The operator surface (`/admin/sessions`, `/admin/s/{s}/detach`).
    Admin,
}

/// The name the legacy `--auth-token` / `LAMBO_AUTH_TOKEN` credential goes by
/// (#32 §6.1). Reserved: no configured credential may use it.
pub const LEGACY_CREDENTIAL_NAME: &str = "default";

/// The name of the implicit loopback credential, which exists only while no
/// credential is configured (#32 §6.1). Reserved like
/// [`LEGACY_CREDENTIAL_NAME`].
pub const LOCAL_CREDENTIAL_NAME: &str = "local";

/// The capability flags a credential carries beyond read and write.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionCapabilities {
    /// May attach a session that does not exist yet.
    pub create: bool,
    /// May erase a session in its scope.
    pub erase: bool,
    /// May use the operator surface for sessions in its scope.
    pub admin: bool,
}

impl SessionCapabilities {
    /// Does this set allow `need`?
    pub fn allows(&self, need: SessionNeed) -> bool {
        match need {
            SessionNeed::Use => true,
            SessionNeed::Create => self.create,
            SessionNeed::Erase => self.erase,
            SessionNeed::Admin => self.admin,
        }
    }
}

/// One credential's authority: a name (for logs; never the secret), a scope
/// and capabilities. The secret itself stays with the surface that compares it
/// (`SecretToken` for serve, `AuthToken` for the portal), so this type is safe
/// to log and to share with #4.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionGrant {
    name: String,
    scope: SessionScope,
    capabilities: SessionCapabilities,
}

impl SessionGrant {
    /// A grant from its parts.
    pub fn new(
        name: impl Into<String>,
        scope: SessionScope,
        capabilities: SessionCapabilities,
    ) -> Self {
        Self {
            name: name.into(),
            scope,
            capabilities,
        }
    }

    /// The legacy credential (#32 §6.1): what `--auth-token` /
    /// `LAMBO_AUTH_TOKEN` becomes. Every pinned session, and no `create`,
    /// `erase` or `admin`.
    pub fn legacy_default() -> Self {
        Self::new(
            LEGACY_CREDENTIAL_NAME,
            SessionScope::pinned(),
            SessionCapabilities::default(),
        )
    }

    /// The implicit loopback credential (#32 §6.1, decision 5): the scope of
    /// [`SessionGrant::legacy_default`] and no capability, so wire erase
    /// always needs a configured credential, loopback included.
    pub fn implicit_local() -> Self {
        Self::new(
            LOCAL_CREDENTIAL_NAME,
            SessionScope::pinned(),
            SessionCapabilities::default(),
        )
    }

    /// The credential's configured name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// What it may address.
    pub fn scope(&self) -> &SessionScope {
        &self.scope
    }

    /// What it may do there.
    pub fn capabilities(&self) -> SessionCapabilities {
        self.capabilities
    }

    /// Step 3 of §6.2, in memory only: is `id` in scope, and does the grant
    /// allow `need`?
    ///
    /// Scope is checked before capability, but both refuse with the same
    /// uniform 404, so the order is invisible to the caller; it only decides
    /// which reason the operator's log names.
    pub fn authorize(
        &self,
        id: &AddressedSessionId,
        need: SessionNeed,
        hosted: &HostedSessions,
    ) -> Result<(), SessionRefusal> {
        if !self.scope.covers(id, hosted) {
            return Err(SessionRefusal::new(RefusalReason::OutOfScope));
        }
        if !self.capabilities.allows(need) {
            return Err(SessionRefusal::new(RefusalReason::MissingCapability));
        }
        Ok(())
    }
}

/// Steps 2 and 3 of §6.2, in their fixed order: parse the addressed id, then
/// authorize it against `grant`. Run only after the surface's bearer check has
/// passed and resolved `grant`.
pub fn authorize_addressed(
    raw: &str,
    grant: &SessionGrant,
    need: SessionNeed,
    hosted: &HostedSessions,
) -> Result<AddressedSessionId, SessionRefusal> {
    let id = parse_addressed(raw)?;
    grant.authorize(&id, need, hosted)?;
    Ok(id)
}

/// A surface's secret type, as the bytes [`SessionAuthority`] compares.
/// Implemented by each surface's own redacting token type, so the scan never
/// needs the secret as a printable string.
pub(crate) trait BearerSecret {
    /// The secret, for a constant-time comparison only.
    fn secret_bytes(&self) -> &[u8];
}

/// Step 1 of §6.2 for a whole credential set: which grant, if any, a
/// request's bearer token resolves to (#32 PR 5).
///
/// Holds either an implicit grant, used for every request without looking at
/// any header (the loopback serve with no credential configured, today's
/// unauthenticated loopback behaviour), or the configured credentials, each a
/// secret and its grant. Never both: the implicit `local` grant is gone once
/// any credential exists (§6.1). With neither, every request is refused,
/// which is the fail-closed answer for a set nobody can present.
///
/// [`SessionAuthority::authorize`] then runs steps 2 and 3 against the
/// grant, in memory only.
pub(crate) struct SessionAuthority<T> {
    credentials: Vec<(T, Arc<SessionGrant>)>,
    implicit: Option<Arc<SessionGrant>>,
    hosted: HostedSessions,
}

impl<T: BearerSecret> SessionAuthority<T> {
    /// Requests must present one of `credentials`.
    pub(crate) fn with_credentials(
        credentials: impl IntoIterator<Item = (T, SessionGrant)>,
        hosted: HostedSessions,
    ) -> Self {
        Self {
            credentials: credentials
                .into_iter()
                .map(|(secret, grant)| (secret, Arc::new(grant)))
                .collect(),
            implicit: None,
            hosted,
        }
    }

    /// Every request gets `grant`, with no header checked.
    pub(crate) fn implicit(grant: SessionGrant, hosted: HostedSessions) -> Self {
        Self {
            credentials: Vec::new(),
            implicit: Some(Arc::new(grant)),
            hosted,
        }
    }

    /// Must a request present a bearer token?
    pub(crate) fn requires_bearer(&self) -> bool {
        self.implicit.is_none()
    }

    /// How many credentials a request can arrive as: the configured ones,
    /// or 1 for an implicit grant (#32 PR 5 review M2: each one's share of
    /// the serve's bounds). Never 0.
    pub(crate) fn credential_count(&self) -> usize {
        self.credentials.len().max(1)
    }

    /// The configured credentials' names, in order (for the startup log;
    /// never a secret). Empty under an implicit grant.
    pub(crate) fn credential_names(&self) -> Vec<&str> {
        self.credentials.iter().map(|(_, g)| g.name()).collect()
    }

    /// The grant for an `Authorization` header value, or `None` (the
    /// surface's 401).
    ///
    /// The presented credential is compared against **every** configured
    /// secret in constant time with no early exit
    /// ([`crate::surface::bearer::match_any`]), so timing does not reveal
    /// which credential a guess nearly matched or where it sits in the list.
    pub(crate) fn authenticate(&self, header: Option<&str>) -> Option<Arc<SessionGrant>> {
        if let Some(grant) = &self.implicit {
            return Some(Arc::clone(grant));
        }
        // A request without a bearer credential is still scanned (as an
        // empty one, which matches nothing), so "no header" and "wrong
        // token" cost the same.
        let presented = crate::surface::bearer::bearer_credential(header).unwrap_or_default();
        // Over the fixed cap: refused before the scan (#32 PR 5 review L2).
        if presented.len() > crate::surface::bearer::MAX_BEARER_CREDENTIAL_BYTES {
            return None;
        }
        let index = crate::surface::bearer::match_any(
            presented.as_bytes(),
            self.credentials
                .iter()
                .map(|(secret, _)| secret.secret_bytes()),
        )?;
        self.credentials
            .get(index)
            .map(|(_, grant)| Arc::clone(grant))
    }

    /// Steps 2 and 3 of §6.2 for an authenticated `grant`:
    /// [`authorize_addressed`] against this set's hosted sessions.
    pub(crate) fn authorize(
        &self,
        grant: &SessionGrant,
        raw: &str,
        need: SessionNeed,
    ) -> Result<AddressedSessionId, SessionRefusal> {
        authorize_addressed(raw, grant, need, &self.hosted)
    }

    /// May `grant` use the surface's default session `default` (what an
    /// unscoped route serves: serve's `/mcp`, the portal's aliases)?
    ///
    /// A default whose name passes the strict charset is authorized exactly
    /// as its addressed route would be. One that does not (a one-session
    /// surface keeps `--session`'s looser rule, and such a session is
    /// reachable only through the unscoped route) can be covered by no exact
    /// name or prefix, so only a scope over every pinned session reaches it:
    /// `"*"`, and the `default` and `local` grants.
    pub(crate) fn authorize_default(
        &self,
        grant: &SessionGrant,
        default: &str,
    ) -> Result<(), SessionRefusal> {
        if parse_addressed(default).is_ok() {
            return self.authorize(grant, default, SessionNeed::Use).map(|_| ());
        }
        if grant.scope().covers_every_pinned() {
            Ok(())
        } else {
            Err(SessionRefusal::new(RefusalReason::OutOfScope))
        }
    }

    /// Is `id` one of this set's pinned sessions? A prefix scope covers ids
    /// by shape alone; a surface that serves only what it pins (the portal's
    /// allowlist, #4 design 4.2) checks this after [`Self::authorize`].
    pub(crate) fn is_pinned(&self, id: &AddressedSessionId) -> bool {
        self.hosted.is_pinned(id)
    }
}

#[cfg(test)]
mod tests;
