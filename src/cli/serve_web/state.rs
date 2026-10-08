//! The one piece of state the portal's handlers share: the session, the
//! resolved backends, the auth posture, and the freshness tracker.

use std::time::{Duration, Instant};

use parking_lot::Mutex;

use super::auth::AuthToken;
use crate::resolve::ResolvedBackends;
use crate::store::GraphStore;
use crate::types::SessionId;

/// When this reader last saw the durable snapshot *change*.
pub(super) struct Freshness {
    pub(super) fingerprint: u64,
    pub(super) observed_at: Instant,
}

pub(super) struct AppState {
    pub(super) session: SessionId,
    pub(super) backends: ResolvedBackends,
    /// True when `--bind` reaches beyond loopback. A non-loopback bind always
    /// carries a token (see [`authorize_bind_web`](super::auth::authorize_bind_web)).
    pub(super) exposed: bool,
    /// Optional bearer token. When set, every route requires it.
    pub(super) auth: Option<AuthToken>,
    pub(super) freshness: Mutex<Freshness>,
}

impl AppState {
    pub(super) fn store(&self) -> &dyn GraphStore {
        self.backends.store.as_ref()
    }

    /// Record the durable state's count fingerprint; return how long the
    /// current one has been standing.
    ///
    /// Counts only: two different graphs with identical counts read as
    /// "unchanged". That is the right trade for a freshness indicator — it is a
    /// hint about writer activity, not a consistency claim.
    pub(super) fn observe(&self, fingerprint: u64) -> Duration {
        let mut f = self.freshness.lock();
        if f.fingerprint != fingerprint {
            f.fingerprint = fingerprint;
            f.observed_at = Instant::now();
        }
        f.observed_at.elapsed()
    }
}
