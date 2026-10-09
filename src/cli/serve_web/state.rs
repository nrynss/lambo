//! The one piece of state the portal's handlers share: the session, the
//! resolved backends, the auth posture, and the per-session view cache.

use std::sync::Arc;
use std::time::Duration;

use super::auth::AuthToken;
use super::views::{SessionView, ViewBounds, ViewCache};
use crate::cli::caps::CliError;
use crate::config::WebConfig;
use crate::resolve::ResolvedBackends;
use crate::store::GraphStore;
use crate::types::SessionId;

pub(super) struct AppState {
    pub(super) session: SessionId,
    pub(super) backends: ResolvedBackends,
    /// True when `--bind` reaches beyond loopback. A non-loopback bind always
    /// carries a token (see [`authorize_bind_web`](super::auth::authorize_bind_web)).
    pub(super) exposed: bool,
    /// Optional bearer token. When set, every route requires it.
    pub(super) auth: Option<AuthToken>,
    /// Every data route reads through this: one load per session per TTL,
    /// shared by every request (#4 PR 1).
    pub(super) views: ViewCache,
}

impl AppState {
    /// The state for a portal on `session`, its view bounds from `web`.
    pub(super) fn new(
        session: SessionId,
        backends: ResolvedBackends,
        exposed: bool,
        auth: Option<AuthToken>,
        web: &WebConfig,
    ) -> Self {
        let bounds = ViewBounds::resolve(web, backends.store_cfg.kind);
        Self {
            views: ViewCache::new([session.clone()], bounds),
            session,
            backends,
            exposed,
            auth,
        }
    }

    pub(super) fn store(&self) -> &dyn GraphStore {
        self.backends.store.as_ref()
    }

    /// The served session's current view (see [`ViewCache::view`]).
    pub(super) async fn view(&self) -> Result<Arc<SessionView>, CliError> {
        self.views
            .view(self.store(), &self.backends.embedding, &self.session)
            .await
    }

    /// Record the durable state's count fingerprint; return how long the
    /// current one has been standing (see [`ViewCache::observe`]).
    pub(super) fn observe(&self, fingerprint: u64) -> Duration {
        self.views.observe(&self.session, fingerprint)
    }
}
