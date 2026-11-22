//! Store-wide session discovery (#276): opt-in with `[web] discover_sessions`
//! or `--discover-sessions`.
//!
//! Off, the portal serves its allowlist and nothing in this module is
//! consulted. On, two things widen, and only for a grant [`Discovery::reaches`]:
//!
//! * **Reading.** `/s/{session}/...` for a well-formed id outside the
//!   allowlist resolves through the dynamic path `session_prefix` already
//!   uses (#241, `scope::resolve_session`): a per-request existence and
//!   erasure probe ([`GraphStore::session_readable`]), then a bounded
//!   dynamic view slot. Nothing is loaded until a request asks, and an
//!   erased or absent id is the uniform 404.
//! * **Listing.** `GET /api/sessions` names the store's live sessions after
//!   the served ones ([`Discovery::sessions`], from
//!   [`GraphStore::store_health`], the read behind `lambo stats --store`,
//!   which omits `lambo:erased` tombstones).
//!
//! **Who it reaches.** A grant whose scope already covers every served
//! session, whatever its name ([`SessionScope::covers_every_pinned`]): the
//! implicit loopback `local`, the legacy `default` (`LAMBO_AUTH_TOKEN` /
//! `--auth-token`) and a `[[web.credential]]` `"*"`. Never an exact-name or
//! prefix scope, and never an inherited `[[serve.credential]]` `"*"`, which
//! `SessionScope::star_within` pins to serve's hosted set (#4 PR 3 review
//! M1) and so does not cover every served session. When unsure, a grant is
//! not reached: it reads what it read before.
//!
//! **Names.** A store session whose name fails
//! [`parse_addressed`] cannot be routed, so it is skipped: never served,
//! never listed, never a startup failure.
//!
//! **Refresh.** Every listing comes from a store read that began after its
//! request arrived, so a session erased before a request arrives is never
//! among that request's discovered sessions (a startup session is listed
//! with the served ones, erased or not). The portal issues one read at a
//! time (a read it gave up on, past [`DISCOVERY_TIMEOUT`] or with its
//! request dropped, may still be running in the backend). A request that
//! arrives while a read is running does not take that read's outcome (the
//! read may already have looked at the store, before an erase the request
//! came after); it waits, and shares the next read, which began after it
//! arrived (single-flight). So each such request costs at most one more
//! read, and the requests that arrive during one read share the next one
//! unless it is abandoned.
//! (`formal/LamboProofs/Discovery.lean`: `fixed_fresh`,
//! `fixed_no_erased_listed`, `one_read_at_a_time`.)
//!
//! [`SessionScope::covers_every_pinned`]: crate::surface::session::SessionScope::covers_every_pinned

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::Mutex as AsyncMutex;

use crate::store::GraphStore;
use crate::surface::session::{parse_addressed, AddressedSessionId, SessionGrant};
use crate::types::{SessionId, StoreError};

/// How long one store-wide read may take before the listing goes on without
/// it (the bound the dynamic existence probe uses).
pub(super) const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a store-wide read gave no names. Logged by class, never a backend
/// message (which could carry a DSN or path).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DiscoveryFailure {
    /// The store does not implement `store_health`.
    Unsupported,
    /// The backend failed.
    Backend,
    /// The read took longer than [`DISCOVERY_TIMEOUT`].
    TimedOut,
}

impl std::fmt::Display for DiscoveryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unsupported => "the store does not support store-wide session listing",
            Self::Backend => "the store-wide session read failed in the backend",
            Self::TimedOut => "the store-wide session read timed out",
        })
    }
}

type Outcome = Result<Arc<[AddressedSessionId]>, DiscoveryFailure>;

#[derive(Default)]
struct Last {
    /// Reads started, the running one included: a read's number. A request
    /// records it on arrival, and only a read numbered above that began
    /// after the request arrived.
    started: u64,
    /// The latest finished read: its number and outcome. A read whose
    /// request was dropped before it returned leaves this as it was.
    finished: Option<(u64, Outcome)>,
}

/// The portal's discovery switch and its single-flight store read.
pub(super) struct Discovery {
    enabled: bool,
    gate: AsyncMutex<()>,
    last: Mutex<Last>,
}

impl Discovery {
    /// Discovery on or off. Off, [`Discovery::reaches`] is false for every
    /// grant and nothing reads the store.
    pub(super) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            gate: AsyncMutex::new(()),
            last: Mutex::new(Last::default()),
        }
    }

    /// Is discovery on?
    pub(super) fn enabled(&self) -> bool {
        self.enabled
    }

    /// Does discovery widen `grant`? Only when it is on and the grant's
    /// scope covers every served session (module docs).
    pub(super) fn reaches(&self, grant: &SessionGrant) -> bool {
        self.enabled && grant.scope().covers_every_pinned()
    }

    /// The store's live, non-erased, addressable sessions, in the store's
    /// order (`session_id` ascending), from a store read that began after
    /// this call did. Single-flight (module docs): reads run one at a time
    /// under the gate, and a request holding the gate takes the latest
    /// finished read's outcome only when that read began after the request
    /// arrived; otherwise it reads again. Callers check
    /// [`Discovery::reaches`] first.
    pub(super) async fn sessions(&self, store: &dyn GraphStore) -> Outcome {
        let arrived = self.last.lock().started;
        let _gate = self.gate.lock().await;
        let read = {
            let mut last = self.last.lock();
            if let Some((read, outcome)) = &last.finished
                && *read > arrived
            {
                return outcome.clone();
            }
            last.started += 1;
            last.started
        };
        let outcome = match tokio::time::timeout(DISCOVERY_TIMEOUT, store.store_health()).await {
            Ok(Ok(health)) => Ok(addressable(health.into_iter().map(|h| h.session_id))),
            Ok(Err(StoreError::Capability(_))) => Err(DiscoveryFailure::Unsupported),
            Ok(Err(_)) => Err(DiscoveryFailure::Backend),
            Err(_) => Err(DiscoveryFailure::TimedOut),
        };
        self.last.lock().finished = Some((read, outcome.clone()));
        outcome
    }
}

/// `ids` that pass [`parse_addressed`], in order; the rest are skipped
/// (counted at `debug`, never named: such a name may hold anything).
pub(super) fn addressable(ids: impl IntoIterator<Item = SessionId>) -> Arc<[AddressedSessionId]> {
    let mut skipped = 0usize;
    let out: Vec<AddressedSessionId> = ids
        .into_iter()
        .filter_map(|id| {
            let parsed = parse_addressed(id.as_str()).ok();
            skipped += usize::from(parsed.is_none());
            parsed
        })
        .collect();
    if skipped > 0 {
        tracing::debug!(
            skipped,
            "serve-web: discovery skipped sessions whose names cannot be addressed by URL"
        );
    }
    out.into()
}

/// What an operator should hear at startup with discovery on, one line
/// each, naming credentials, never a token: who it reaches (or that it
/// reaches nobody), and, beyond loopback, that the listing still needs
/// `list_sessions`.
pub(super) fn startup_notes(reached: &[&str], exposed: bool, listing: bool) -> Vec<String> {
    let mut out = Vec::new();
    if reached.is_empty() {
        out.push(
            "[web] discover_sessions is on, but no credential's scope covers every served session \
             (the implicit loopback grant, LAMBO_AUTH_TOKEN / --auth-token, or a \
             [[web.credential]] \"*\"), so discovery widens nothing"
                .to_string(),
        );
    } else {
        out.push(format!(
            "[web] discover_sessions is on: callers as {} also read every live, non-erased session \
             in the store whose name is 1 to 128 bytes of [A-Za-z0-9._:-], including sessions created \
             after startup; other credentials read only their own scope",
            reached
                .iter()
                .map(|n| format!("'{n}'"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        if listing {
            out.push(
                "GET /api/sessions also names the discovered sessions to those callers".to_string(),
            );
        }
    }
    if exposed {
        out.push(
            "session discovery on a non-loopback bind: every caller it reaches can read every \
             live session in the store; GET /api/sessions still needs [web] list_sessions = true"
                .to_string(),
        );
    }
    out
}
