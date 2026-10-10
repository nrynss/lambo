//! The on-demand attach's bounds and bookkeeping (#32 PR 6 review): who a
//! request is, what it carries while it is routed, the attach's timeouts,
//! the negative cache, the fair share of on-demand places per credential
//! and the eviction victim it allows, and the single-flight guard that
//! always answers the requests waiting on an attach.
//!
//! The attach itself (`SessionRegistry::get_or_attach` and
//! `attach_on_demand`) is in the parent module; this one holds the pieces it
//! is built from.
//!
//! # Order of an on-demand attach (review H1)
//!
//! 1. The router asks [`SessionRegistry::get_or_attach`](super::SessionRegistry::get_or_attach).
//!    A cached negative answer (absent, erased, failed) is given at once.
//! 2. The attach task takes a permit and probes the session's lease row:
//!    erased, absent without `create`, or held by another live writer each
//!    end the attach **here, before anything is evicted**.
//! 3. Only then is a place reserved under the slots' lock: a free one, or
//!    one an eviction frees ([`choose_victim`], within the credential's
//!    share). With none, 503 `Retry-After`.
//! 4. The victim is detached, then the session is acquired and admitted.
//!
//! The whole attach is bounded by [`ATTACH_TIMEOUT`], the probe by
//! [`PROBE_TIMEOUT`], and a request waiting on an attach by [`ATTACH_WAIT`]
//! (review M2).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use super::{AttachOutcome, SessionRegistry, Slot};
use crate::mcp::serve::activity::InFlight;
use crate::mcp::serve::session::AttachedSession;

/// The longest one on-demand attach may take, from its permit wait to its
/// admission (#32 PR 6 review M2). A store that hangs (a locked database, a
/// network partition) would otherwise hold an attach permit, the only one on
/// SQLite, for as long as it hangs, and every attach behind it would queue
/// forever. Past this the attach is abandoned: the lease it may have taken
/// is released (holder-scoped), its slot is removed, and the requests
/// waiting on it get 503 with `Retry-After` ([`ATTACH_BUSY_RETRY`]); the
/// next request tries again. A minute is far above a normal load (seconds
/// for thousands of concepts) and far below a client's patience. The pinned
/// background retry is bounded by it too.
pub(in crate::mcp::serve) const ATTACH_TIMEOUT: Duration = Duration::from_secs(60);

/// The longest the existence probe (one lease-row read) may take inside an
/// attach; past it the attach answers 503 like any store it cannot reach.
pub(in crate::mcp::serve) const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The longest a request waits for an attach in flight (#32 PR 6 review
/// M2). Past it the request gets 503 with [`ATTACH_BUSY_RETRY`], so an HTTP
/// connection is never held for a whole [`ATTACH_TIMEOUT`]; the attach goes
/// on, and a retry joins it or finds the session live.
pub(in crate::mcp::serve) const ATTACH_WAIT: Duration = Duration::from_secs(15);

/// The `Retry-After` of a request that gave up waiting on an attach, or
/// whose attach timed out.
pub(in crate::mcp::serve) const ATTACH_BUSY_RETRY: Duration = Duration::from_secs(5);

/// How long an on-demand session must have been idle before an attach may
/// evict it (#32 PR 6 review M1 and M4). A routed request holds the
/// session's [`InFlight`] for as long as its handler runs, but an MCP tool
/// call is dispatched by rmcp's session task a moment after the handler
/// has answered, and only then enters `call_tool`'s own guard. The floor
/// covers that hand-off, and it stops one credential from churning sessions
/// that are between calls.
pub(in crate::mcp::serve) const EVICT_MIN_IDLE: Duration = Duration::from_secs(2);

/// How long a negative attach outcome (absent, erased, failed) is answered
/// from memory before the store is asked again (#32 PR 6 review M3).
pub(in crate::mcp::serve) const NEGATIVE_TTL: Duration = Duration::from_secs(30);

/// How many negative outcomes are remembered at most; the oldest goes first.
pub(in crate::mcp::serve) const NEGATIVE_CACHE_MAX: usize = 1024;

/// Who is asking for a session: the credential the request authenticated
/// as, and whether that credential may create a session.
#[derive(Clone, Copy, Debug)]
pub(in crate::mcp::serve) struct Requester<'a> {
    /// The credential's name. An on-demand session is attributed to the
    /// credential whose request attached it, for the fair share.
    pub(in crate::mcp::serve) credential: &'a str,
    /// The credential's `create` capability.
    pub(in crate::mcp::serve) create: bool,
}

/// What the router gets for a session: the answer, and, when the session is
/// live, the request's hold on it.
pub(in crate::mcp::serve) struct Routed {
    /// The answer.
    pub(in crate::mcp::serve) lookup: super::Lookup,
    /// Taken under the slots' lock when the session was found live (review
    /// M1), so no eviction or idle detach can take the session between the
    /// routing and the handler. The router holds it until the session's
    /// MCP service has answered.
    pub(in crate::mcp::serve) in_flight: Option<InFlight>,
}

impl Routed {
    /// An answer with no hold on a session.
    pub(super) fn answer(lookup: super::Lookup) -> Self {
        Self {
            lookup,
            in_flight: None,
        }
    }
}

/// A remembered negative outcome of an on-demand attach.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Negative {
    /// The session does not exist, as an attach without `create` found. A
    /// request whose credential has `create` ignores it.
    Absent,
    /// The session is erased (#23).
    Erased,
    /// The attach failed in a way a retry will not fix by itself.
    Failed,
}

/// A bounded, expiring map of negative attach outcomes by session id (#32
/// PR 6 review M3). Held-elsewhere and transient errors are never kept:
/// those are retried by the next request. An entry is dropped when its
/// session attaches.
#[derive(Default)]
pub(super) struct NegativeCache {
    entries: HashMap<String, (Negative, Instant)>,
}

impl NegativeCache {
    /// The remembered outcome for `id` that applies to a request with
    /// `create`, if one is still fresh. An `Absent` entry never applies to
    /// a request that may create the session.
    pub(super) fn get(&mut self, id: &str, create: bool) -> Option<Negative> {
        let now = Instant::now();
        let (negative, at) = *self.entries.get(id)?;
        if now.saturating_duration_since(at) >= NEGATIVE_TTL {
            self.entries.remove(id);
            return None;
        }
        match negative {
            Negative::Absent if create => None,
            other => Some(other),
        }
    }

    /// Remember `negative` for `id`, dropping expired entries and then, at
    /// [`NEGATIVE_CACHE_MAX`], the oldest.
    pub(super) fn put(&mut self, id: &str, negative: Negative) {
        let now = Instant::now();
        if self.entries.len() >= NEGATIVE_CACHE_MAX && !self.entries.contains_key(id) {
            self.entries
                .retain(|_, (_, at)| now.saturating_duration_since(*at) < NEGATIVE_TTL);
            if self.entries.len() >= NEGATIVE_CACHE_MAX
                && let Some(oldest) = self
                    .entries
                    .iter()
                    .min_by_key(|(_, (_, at))| *at)
                    .map(|(id, _)| id.clone())
            {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(id.to_string(), (negative, now));
    }

    /// Forget `id` (it attached).
    pub(super) fn forget(&mut self, id: &str) {
        self.entries.remove(id);
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Each credential's share of the on-demand places (#32 PR 6 review M4):
/// `places` divided evenly among the credentials whose scope reaches past
/// the pinned sessions, rounded down, and at least 1. PR 5's rule for
/// `--max-sessions` (`http_guards::credential_share`), for the same reason:
/// rounded down, the shares never add up to more than the places, so every
/// credential can always reach its share by eviction, whatever the others
/// hold.
///
/// The share bounds only eviction. A free place goes to whoever asks, so an
/// idle credential's share is used rather than wasted; at the cap, a
/// credential at or over its share may evict only its own sessions, and one
/// under it may also evict a session of a credential over its share
/// ([`choose_victim`]).
pub(super) fn place_share(places: usize, credentials: usize) -> usize {
    crate::mcp::serve::http_guards::credential_share(places, credentials)
}

/// The session an attach by `requester` may evict to make room, among
/// `slots`' live on-demand sessions (never a pinned one, never one with a
/// call in flight or used within `min_idle`): the least recently used of
/// the requester's own, or, while the requester holds less than `share`, of
/// any credential holding more than its share. `owners` attributes each
/// on-demand session to the credential whose request attached it; a
/// session several credentials use counts against that one only.
pub(super) fn choose_victim(
    slots: &HashMap<String, Slot>,
    owners: &HashMap<String, String>,
    pinned: impl Fn(&str) -> bool,
    requester: &str,
    share: usize,
    min_idle: Duration,
) -> Option<(String, Arc<AttachedSession>)> {
    let held = held_by(slots, owners, &pinned);
    let mine = held.get(requester).copied().unwrap_or(0);
    let now = Instant::now();
    slots
        .iter()
        .filter(|(id, _)| !pinned(id))
        .filter_map(|(id, slot)| match slot {
            Slot::Live(session) => session
                .activity
                .idle_at(now)
                .filter(|idle| *idle >= min_idle)
                .map(|idle| (idle, id, session)),
            _ => None,
        })
        .filter(|(_, id, _)| {
            let owner = owners.get(id.as_str()).map(String::as_str).unwrap_or("");
            owner == requester || (mine < share && held.get(owner).copied().unwrap_or(0) > share)
        })
        // Longest idle first; the id breaks a tie, so the choice is stable.
        .max_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(a.1)))
        .map(|(_, id, session)| (id.clone(), Arc::clone(session)))
}

/// How many on-demand places each credential holds: every on-demand slot
/// that holds or is about to hold a `Memory` (live, detaching, or attaching
/// with its place reserved), by the credential that attached it.
pub(super) fn held_by(
    slots: &HashMap<String, Slot>,
    owners: &HashMap<String, String>,
    pinned: impl Fn(&str) -> bool,
) -> HashMap<String, usize> {
    let mut held: HashMap<String, usize> = HashMap::new();
    for (id, slot) in slots {
        if pinned(id) || !takes_a_place(slot) {
            continue;
        }
        let owner = owners.get(id).cloned().unwrap_or_default();
        *held.entry(owner).or_default() += 1;
    }
    held
}

/// Whether an on-demand slot counts against `max_attached`: it holds a
/// `Memory`, or is about to (an attach that has reserved its place), or is
/// still giving one up (a detach in flight). An attach that has not yet
/// passed its existence probe takes no place.
pub(super) fn takes_a_place(slot: &Slot) -> bool {
    matches!(
        slot,
        Slot::Live(_) | Slot::Detaching | Slot::Attaching { placed: true, .. }
    )
}

/// One on-demand attach in flight, as its task holds it (#32 PR 6 review
/// L1). [`Flight::finish`] removes the slot of an attach that did not end
/// live, records a negative outcome, and wakes every request waiting on it.
/// Dropped without finishing (the task panicked or was aborted), it does
/// the same with [`AttachOutcome::Failed`], so the slot never stays
/// `Attaching` and keeps no place, and no waiter is left without an answer.
pub(super) struct Flight {
    registry: Arc<SessionRegistry>,
    id: String,
    create: bool,
    tx: Option<tokio::sync::watch::Sender<Option<AttachOutcome>>>,
}

impl Flight {
    pub(super) fn new(
        registry: Arc<SessionRegistry>,
        id: String,
        create: bool,
        tx: tokio::sync::watch::Sender<Option<AttachOutcome>>,
    ) -> Self {
        Self {
            registry,
            id,
            create,
            tx: Some(tx),
        }
    }

    /// The session being attached.
    pub(super) fn id(&self) -> &str {
        &self.id
    }

    /// Whether the attach may create the session.
    pub(super) fn create(&self) -> bool {
        self.create
    }

    /// Whether this flight's `done` is the one in `slot` (its own attach,
    /// not a later one for the same id).
    pub(super) fn owns(&self, slot: Option<&Slot>) -> bool {
        match (slot, &self.tx) {
            (Some(Slot::Attaching { done, .. }), Some(tx)) => done.same_channel(&tx.subscribe()),
            _ => false,
        }
    }

    /// End the flight with `outcome`.
    pub(super) fn finish(mut self, outcome: AttachOutcome) {
        self.end(outcome);
    }

    /// End the flight with `outcome`; `true` when its slot was still its
    /// own `Attaching` one (and, unless it attached, is now removed).
    fn end(&mut self, outcome: AttachOutcome) -> bool {
        let Some(tx) = self.tx.take() else {
            return false;
        };
        let mine = {
            let mut slots = self.registry.slots.lock();
            let mine = matches!(
                slots.get(&self.id),
                Some(Slot::Attaching { done, .. }) if done.same_channel(&tx.subscribe())
            );
            if outcome != AttachOutcome::Attached && mine {
                slots.remove(&self.id);
                self.registry.owners.lock().remove(&self.id);
            }
            let mut negative = self.registry.negative.lock();
            match &outcome {
                AttachOutcome::Attached => negative.forget(&self.id),
                AttachOutcome::Absent if !self.create => negative.put(&self.id, Negative::Absent),
                AttachOutcome::Erased => negative.put(&self.id, Negative::Erased),
                AttachOutcome::Failed => negative.put(&self.id, Negative::Failed),
                AttachOutcome::Absent | AttachOutcome::Busy { .. } => {}
            }
            mine
        };
        // Nobody waiting is fine: the outcome is in the slot already.
        let _ = tx.send(Some(outcome));
        mine
    }
}

impl Drop for Flight {
    fn drop(&mut self) {
        if self.tx.is_some() {
            tracing::error!(
                session = %self.id,
                "lambo serve: an on-demand attach ended without an outcome (a panic or an \
                 abort); answering 503 and clearing its slot"
            );
            // An attach that died while its slot was still `Attaching` may
            // have taken the lease; release it by holder, in the background
            // (a drop cannot wait), tracked so the shutdown joins it. A
            // session it already admitted is live and keeps its lease.
            if self.end(AttachOutcome::Failed) && tokio::runtime::Handle::try_current().is_ok() {
                let registry = Arc::clone(&self.registry);
                let id = self.id.clone();
                let task = tokio::spawn(async move { registry.release_abandoned(&id).await });
                self.registry.track(task);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn the_negative_cache_expires_skips_absent_for_create_and_is_bounded() {
        let mut cache = NegativeCache::default();
        cache.put("gone", Negative::Erased);
        cache.put("nobody", Negative::Absent);
        assert_eq!(cache.get("gone", true), Some(Negative::Erased));
        assert_eq!(cache.get("nobody", false), Some(Negative::Absent));
        assert_eq!(cache.get("nobody", true), None, "create ignores absent");
        assert_eq!(
            cache.get("nobody", false),
            Some(Negative::Absent),
            "and does not clear it for the others"
        );
        tokio::time::advance(NEGATIVE_TTL).await;
        assert_eq!(cache.get("gone", false), None, "expired");
        cache.forget("nobody");
        assert_eq!(cache.get("nobody", false), None);

        for i in 0..NEGATIVE_CACHE_MAX + 10 {
            cache.put(&format!("s{i}"), Negative::Failed);
            tokio::time::advance(Duration::from_millis(1)).await;
        }
        assert_eq!(cache.len(), NEGATIVE_CACHE_MAX, "bounded");
        assert_eq!(cache.get("s0", false), None, "the oldest went first");
        assert_eq!(
            cache.get(&format!("s{}", NEGATIVE_CACHE_MAX + 9), false),
            Some(Negative::Failed)
        );
    }

    #[test]
    fn the_place_share_is_pr5s_rule() {
        assert_eq!(place_share(15, 1), 15);
        assert_eq!(place_share(15, 2), 7);
        assert_eq!(place_share(15, 4), 3);
        assert_eq!(place_share(2, 5), 1, "at least one");
        assert_eq!(place_share(0, 3), 1);
    }
}
