//! The one piece of state the portal's handlers share: the served sessions,
//! the resolved backends, the credential set, and the per-session view
//! cache. It holds no "current session": every request resolves its own
//! (#4 design 3.2, [`super::scope`]).

use std::sync::Arc;
use std::time::Duration;

use super::auth::{portal_authority, AuthToken, HostCheck, PortalAuthority, WebCredential};
use super::views::{SessionView, ViewBounds, ViewCache};
use crate::cli::caps::CliError;
use crate::config::{AllowedHost, WebConfig};
use crate::resolve::ResolvedBackends;
use crate::store::GraphStore;
use crate::types::SessionId;

pub(super) struct AppState {
    /// The default session: what the unscoped routes serve, and the first
    /// served session.
    pub(super) default_session: SessionId,
    /// Every served session, in order, the default first: the allowlist.
    pub(super) sessions: Vec<SessionId>,
    /// `[web] list_sessions`: is `GET /api/sessions` registered (#4 PR 3)?
    pub(super) list_sessions: bool,
    pub(super) backends: ResolvedBackends,
    /// True when `--bind` reaches beyond loopback. A non-loopback bind always
    /// carries a credential (see [`authorize_bind_web`](super::auth::authorize_bind_web)).
    pub(super) exposed: bool,
    /// Who may read which served session: the implicit loopback grant, or
    /// the configured credentials' (`surface::session`, #4 PR 2 and 3).
    pub(super) authority: PortalAuthority,
    /// Which `Host` values are answered: the loopback names and the allowed
    /// hosts under the implicit grant, any once a token is required (#4
    /// design 4.5).
    pub(super) host_check: HostCheck,
    /// Every data route reads through this: one load per session per TTL,
    /// shared by every request (#4 PR 1).
    pub(super) views: ViewCache,
}

impl AppState {
    /// The state for a portal serving `default_session` and `more` (in
    /// order, each once, the default first), its view bounds from `web`.
    /// `allowed_hosts` join the loopback names while no credential is
    /// configured. `credentials` are the resolved `[[web.credential]]` (and
    /// inherited) grants, beside the legacy `auth` token.
    #[allow(clippy::too_many_arguments)] // the composition root's inputs, each used once
    pub(super) fn new(
        default_session: SessionId,
        more: impl IntoIterator<Item = SessionId>,
        backends: ResolvedBackends,
        exposed: bool,
        auth: Option<AuthToken>,
        credentials: Vec<WebCredential>,
        allowed_hosts: &[AllowedHost],
        web: &WebConfig,
    ) -> Self {
        let mut sessions = vec![default_session.clone()];
        for session in more {
            if !sessions.contains(&session) {
                sessions.push(session);
            }
        }
        let bounds = ViewBounds::resolve(web, backends.store_cfg.kind);
        let authority = portal_authority(auth, credentials, &sessions);
        Self {
            views: ViewCache::new(sessions.iter().cloned(), bounds),
            host_check: HostCheck::for_authority(&authority, allowed_hosts),
            authority,
            sessions,
            list_sessions: web.list_sessions,
            default_session,
            backends,
            exposed,
        }
    }

    pub(super) fn store(&self) -> &dyn GraphStore {
        self.backends.store.as_ref()
    }

    /// `session`'s current view (see [`ViewCache::view`]). `session` is one
    /// the request was authorized for ([`super::scope`]).
    pub(super) async fn view(&self, session: &SessionId) -> Result<Arc<SessionView>, CliError> {
        self.views
            .view(self.store(), &self.backends.embedding, session)
            .await
    }

    /// Record `session`'s durable count fingerprint; return how long the
    /// current one has been standing (see [`ViewCache::observe`]).
    pub(super) fn observe(&self, session: &SessionId, fingerprint: u64) -> Duration {
        self.views.observe(session, fingerprint)
    }
}
