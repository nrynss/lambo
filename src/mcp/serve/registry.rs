//! The sessions one serve process holds (#32 PRs 4 and 6, design §3): the
//! [`SessionRegistry`], its slot states, the pinned attach, the on-demand
//! attach and its bounds, the per-session detach, the idle sweeper, and the
//! routing lookup the HTTP router asks.
//!
//! Every serve runs through a registry. A one-session serve (stdio, or HTTP
//! with one pinned session and nothing on demand) attaches its session
//! through the startup election (`roles::resolve_role`, J2's proxy included)
//! and holds it in a registry of one under [`LeaseLossPolicy::ExitProcess`]:
//! a lost lease ends the process exactly as it always has. A serve with more
//! than one pinned session, or any credential that reaches past the pinned
//! ones, attaches each session from the one `serve_builder` template (so they
//! share the embedder, the store and the write-queue calibration) under
//! [`LeaseLossPolicy::DetachSession`]: a lost lease detaches that session,
//! the others keep serving, and a pinned session is re-elected in the
//! background.
//!
//! # Pinned and on-demand sessions (design §1)
//!
//! Pinned sessions are attached at startup and held until the process
//! exits. An on-demand session is attached by the first request that
//! addresses it, from a credential authorized for it ([`SessionRegistry::get_or_attach`]):
//! one with `create`, or any credential when the session already exists (a
//! lease row, design decision 3). It is detached again when it has been
//! idle for `idle_detach_secs`, when an attach at `max_attached` evicts it as
//! the least recently used idle one, or when it loses its lease; its slot is
//! then removed, and a later request attaches it again with the next
//! fencing token.
//!
//! # Slot states
//!
//! | slot | a request gets | how it leaves |
//! |---|---|---|
//! | [`Slot::Attaching`] (on-demand) | waits for the one attach in flight (single-flight), at most `ATTACH_WAIT` | the attach ends: `Live`, or removed (a negative outcome is cached, see `on_demand`) |
//! | [`Slot::Live`] | the session's own MCP service | a detach, or the process shutdown |
//! | [`Slot::Detaching`] | 503, `Retry-After: 1` | the detach ends: `HeldElsewhere` (pinned) or removed (on-demand) |
//! | [`Slot::HeldElsewhere`] (pinned) | 503, `Retry-After` until the next retry | the background retry wins the lease |
//! | [`Slot::Failed`] (pinned) | 503, no `Retry-After` | never: an operator restarts the serve |
//! | [`Slot::Erasing`] | 410 (in scope) | the erase ends: `Erased`, or back to the state it had |
//! | [`Slot::Erased`] (pinned) | 410 (in scope) | never: the store's tombstone refuses every attach |
//! | absent, pinned | 503, `Retry-After: 1` (between states) | |
//! | absent, not pinned | a cached negative outcome; else an on-demand attach, when the serve attaches on demand (503 `Retry-After` while `2 × max_attached` attaches wait for their probe); else the uniform 404 (`surface::session`) | |
//!
//! The erase itself (#32 PR 7, design §6.3) is [`SessionRegistry::erase`],
//! in the `erase` child module.
//!
//! # What runs where
//!
//! The process-wide tasks (`process::ProcessTasks`) iterate [`SessionRegistry::attached`]:
//! one heartbeat line per attached session per interval and one refusal
//! poller over all of them. The registry owns its own tasks: the pinned
//! retry loop, the idle sweeper, and the detaches and on-demand attaches it
//! spawns; the process shutdown waits for the detaches and attaches and
//! stops the loops (see [`SessionRegistry::close_set`] and
//! [`SessionRegistry::stop_tasks`]).

mod on_demand;

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

mod erase;
mod views;

pub(super) use erase::EraseAnswer;
#[cfg(all(test, unix, feature = "store-memory", feature = "embed-fixture"))]
pub(super) use on_demand::NEGATIVE_TTL;
use on_demand::{
    choose_victim, place_share, takes_a_place, unplaced_cap, Flight, Negative, NegativeCache,
};
pub(super) use on_demand::{
    Requester, Routed, ATTACH_BUSY_RETRY, ATTACH_TIMEOUT, ATTACH_WAIT, EVICT_MIN_IDLE,
    PROBE_TIMEOUT,
};

use super::builder::explain_startup_failure;
use super::hub::{derive_endpoint, SessionEndpoint};
use super::roles::{record_refused_loser, ELECTION_RETRY};
use super::session::{session_server, AttachedSession, HostCheck};
use super::shutdown::{book_lease_loss, close_sessions, LEASE_RELEASE_GRACE, SHUTDOWN_GRACE};
use super::signals::EarlyShutdown;
use super::stages::{ShutdownProgress, Stage};
use crate::ledger::Ledger;
use crate::memory::{Attach, LeaseHeldElsewhere, Memory, MemoryBuilder};
use crate::store::StoreConfig;
use crate::types::LamboError;

/// How often a pinned session held by another writer is tried again
/// (design §3.2: `ELECTION_RETRY × 5`, never under a second). There is no
/// election wait on this path: a request for the session gets 503 in the
/// meantime.
///
/// Multiplied as a `Duration`, not through `as_secs()`, so a sub-second
/// `ELECTION_RETRY` cannot truncate it to zero and spin the retry loop; the
/// floor holds whatever `ELECTION_RETRY` becomes (#32 review nit).
pub(super) const PINNED_RETRY: Duration = {
    let five = ELECTION_RETRY.saturating_mul(5);
    if five.as_millis() < 1_000 {
        Duration::from_secs(1)
    } else {
        five
    }
};

/// The `Retry-After` of an on-demand attach refused because `max_attached`
/// sessions are attached and none of the on-demand ones is idle to evict
/// (design §3.2 step 3).
pub(super) const AT_CAPACITY_RETRY: Duration = Duration::from_secs(5);

/// The longest the idle sweeper waits between rounds (design §3.4: every
/// 30 s). A shorter `idle_detach_secs` sweeps at that interval instead, so a
/// session is never kept more than twice its idle time.
pub(super) const IDLE_SWEEP_MAX: Duration = Duration::from_secs(30);

/// How many times one request follows a session through its attaches
/// before it answers 503: an attach that ends with the session evicted
/// again before the request reaches it, over and over, is a cap far too
/// small for its load, and the request must not spin.
const ATTACH_FOLLOWS: usize = 3;

/// How the registry answers a session's lost lease (design §4.2, decision 8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LeaseLossPolicy {
    /// One pinned session: the fence ends the process (`wind_down`, JE2E-4),
    /// so its client respawns into a proxy to the new holder. Today's
    /// behaviour, unchanged.
    ExitProcess,
    /// Several pinned sessions: the session that lost its lease is detached
    /// and re-elected in the background; the process and its other
    /// sessions keep serving.
    DetachSession,
}

impl LeaseLossPolicy {
    /// The one place the policy is derived from configuration (design R4):
    /// one pinned session and nothing on demand is today's `ExitProcess`;
    /// anything more is `DetachSession`.
    pub(super) fn for_scope(pinned: usize, on_demand: bool) -> Self {
        if pinned > 1 || on_demand {
            Self::DetachSession
        } else {
            Self::ExitProcess
        }
    }
}

/// The `[serve]` bounds a registry enforces (#32 PR 6, design §3.6).
#[derive(Clone, Debug)]
pub(super) struct RegistryBounds {
    /// Attaches that may run at once: the background retries of pinned
    /// sessions and the on-demand attaches together
    /// (`SessionBounds::attach_permits`).
    pub(super) attach_permits: usize,
    /// `Some` when sessions attach on demand.
    pub(super) on_demand: Option<OnDemandBounds>,
}

impl RegistryBounds {
    /// A registry of pinned sessions only, one attach at a time (PR 4's
    /// shape).
    pub(super) fn pinned_only() -> Self {
        Self {
            attach_permits: 1,
            on_demand: None,
        }
    }
}

/// The bounds on on-demand sessions.
#[derive(Clone, Debug)]
pub(super) struct OnDemandBounds {
    /// Attached sessions, pinned plus on-demand: an on-demand session may
    /// take one of the `max_attached - pinned` places left.
    pub(super) max_attached: usize,
    /// An on-demand session unused this long is detached. At least
    /// [`MIN_IDLE_DETACH`]: [`SessionRegistry::new`] raises a shorter one
    /// (#32 PR 6 review L5).
    pub(super) idle_detach: Duration,
    /// How many credentials reach on-demand sessions
    /// (`authority::on_demand_credentials`): the on-demand places are
    /// shared among them for eviction (#32 PR 6 review M4,
    /// `on_demand::place_share`). At least 1.
    pub(super) share_among: usize,
    /// How long an on-demand session must have been idle before an attach
    /// may evict it ([`EVICT_MIN_IDLE`] in a serve).
    pub(super) min_idle_to_evict: Duration,
}

/// The shortest `idle_detach` a registry runs with (#32 PR 6 review L5):
/// `[serve]` refuses 0, and a library caller's 0 would make the idle
/// sweeper spin and detach every session the moment its calls end.
pub(super) const MIN_IDLE_DETACH: Duration = Duration::from_secs(1);

/// How an on-demand attach ended, as every request waiting on it learns it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum AttachOutcome {
    /// The session is live.
    Attached,
    /// It does not exist, and the attach could not create it (no `create`).
    Absent,
    /// It was erased (#23): it is never recreated.
    Erased,
    /// Not now: held by another writer, no room under `max_attached`, the
    /// serve closing, or a store that could not be reached.
    Busy { retry_after: Duration },
    /// The attach failed in a way a retry will not fix by itself.
    Failed,
}

/// Why a session is detached, for its log line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DetachReason {
    /// Its lease was lost (the lease watcher).
    LeaseLost,
    /// It was idle for `idle_detach_secs` (the idle sweeper).
    Idle,
    /// An attach at `max_attached` evicted it as the least recently used.
    Evicted,
}

impl DetachReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::LeaseLost => "lease lost",
            Self::Idle => "idle",
            Self::Evicted => "evicted",
        }
    }
}

/// One session's place in the registry.
enum Slot {
    /// An on-demand attach is in flight (#32 PR 6): every request for the
    /// session waits on `done` for its one outcome, so N concurrent first
    /// requests cause one `build_attach` (single-flight, design §3.2).
    Attaching {
        done: tokio::sync::watch::Receiver<Option<AttachOutcome>>,
        /// Whether the attach may create the session: a request whose
        /// credential has `create` does not take an `Absent` from one that
        /// had not.
        create: bool,
        /// Whether the attach has passed its existence probe and reserved
        /// one of the on-demand places (#32 PR 6 review H1). Until then it
        /// takes no place under `max_attached` and has evicted nothing.
        placed: bool,
    },
    /// Attached and serving.
    Live(Arc<AttachedSession>),
    /// Being taken down by a detach.
    Detaching,
    /// Another writer holds the lease; the retry loop tries again at
    /// `retry_at`.
    HeldElsewhere {
        retry_at: Instant,
        /// The handle a detach took down, while anything may still hold it
        /// (#32 review M1). The retry waits for it to go before it attaches
        /// again: while it lives it keeps its second-writer registration (a
        /// fenced close fails, and a failed close keeps it, R2-4) and its
        /// in-RAM graph.
        previous: Option<PreviousHandle>,
        /// Set once a transient attach error in the current streak has been
        /// logged at WARN, so a long outage logs once, not every retry.
        warned: bool,
    },
    /// A background attach failed with an error that will not clear on its
    /// own (an erased session, an embedding-contract mismatch, an
    /// unprovisioned store): logged once at ERROR and no longer retried
    /// (#32 review L1). An erased session gets [`Slot::Erased`] instead
    /// (#32 PR 7).
    Failed,
    /// Being erased by this process (#32 PR 7, design §6.3): requests get
    /// the erased refusal, and no attach or retry starts for it.
    Erasing,
    /// Erased: the store holds the #23 tombstone, which refuses every
    /// acquire, so nothing in this process attaches it again. Requests get
    /// the erased refusal without a store call (a negative cache, kept for
    /// hosted sessions only, so its size is bounded by the hosted set).
    Erased,
}

/// A detached session's old `Memory`, held weakly.
struct PreviousHandle {
    mem: Weak<Memory>,
    detached_at: Instant,
}

/// How long a retry waits for a detached session's old handle to drop
/// before attaching anyway. Every owner the registry knows of is gone when
/// the detach ends; what can remain is a request or an MCP-session task
/// finishing, which takes milliseconds. Past this, the handle is leaked, and
/// the `SecondSessionWriter` ERROR the re-attach then logs is a true report.
const PREVIOUS_HANDLE_WAIT: Duration = Duration::from_secs(30);

/// The states [`SessionRegistry::force_state`] can put a session in.
#[cfg(all(test, unix, feature = "store-memory", feature = "embed-fixture"))]
#[derive(Clone, Copy, Debug)]
pub(super) enum ForcedState {
    Detaching,
    HeldElsewhere,
    Failed,
    Erased,
}

/// What the router gets for a session id.
pub(super) enum Lookup {
    /// Serve the request on this session.
    Live(Arc<AttachedSession>),
    /// Hosted, but not serving right now: answer 503 with this `Retry-After`.
    Unavailable { retry_after: Duration },
    /// Hosted, but its attach failed for good: answer 503 with no
    /// `Retry-After`, since retrying will not help until an operator acts.
    /// A pinned session stays failed until a restart; an on-demand one is
    /// answered from the negative cache for `NEGATIVE_TTL`, then tried
    /// again.
    Failed,
    /// Erased: an on-demand session found tombstoned (#23), or a session
    /// erased, or being erased, by this process (#32 PR 7). It is never
    /// attached or recreated again. Reached only inside the caller's scope.
    Erased,
    /// Not a session this serve hosts, or an on-demand session that does
    /// not exist and that the caller may not create: the uniform 404.
    NotHosted,
}

/// What a multi-session registry needs to attach a pinned session itself:
/// the template builder every session is cloned from, and what each
/// session's builder adds to it.
pub(super) struct SessionAttacher {
    /// `serve_builder`'s output with no session, endpoint or scoped ledger:
    /// every session is `template.clone().session(id)...`, so the embedder,
    /// store, config and calibration are shared (design §3.1, PR 3).
    pub(super) template: MemoryBuilder,
    /// For each session's endpoint derivation.
    pub(super) store_cfg: StoreConfig,
    /// The process's ledger, unscoped; each session gets
    /// `ledger.for_session(id)`.
    pub(super) ledger: Option<Arc<Ledger>>,
    /// The per-session endpoint's MCP-session cap (`--max-sessions`).
    pub(super) max_sessions: usize,
    /// Each session's HTTP `Host` check (#32 PR 5 review M1).
    pub(super) host_check: HostCheck,
    /// The agent this process writes as.
    pub(super) agent: String,
    /// Each session's own request rate, `[serve] per_session_rps` (#32 PR
    /// 6); 0 is none.
    pub(super) session_rps: u32,
}

/// A pinned session's startup attach: the lease is ours, or another writer
/// holds it.
pub(super) enum Acquired {
    /// The lease is ours. The session parts are built later, below the
    /// arming, by [`SessionRegistry::admit`].
    Attached(Arc<Memory>, Option<SessionEndpoint>),
    /// Another writer holds the lease.
    Held(Box<LeaseHeldElsewhere>),
}

/// The attached sessions of one serve process (design §3.1).
pub(super) struct SessionRegistry {
    /// The pinned sessions, in the order they were pinned. Iteration (the
    /// heartbeat, the shutdown's close set and its outcome lines) follows
    /// it, then the on-demand sessions by id.
    order: Vec<String>,
    /// What `/mcp` serves.
    default: Option<String>,
    policy: LeaseLossPolicy,
    /// `Some` when this registry attaches sessions itself (more than one
    /// pinned session); a one-session registry's session comes from the
    /// election.
    attacher: Option<SessionAttacher>,
    slots: parking_lot::Mutex<HashMap<String, Slot>>,
    /// Set when the process shutdown takes the attached set: no attach
    /// starts after it, and one in flight is abandoned (#32 review L8).
    closing: tokio::sync::watch::Sender<bool>,
    /// One permit per attach that may run at once (`attach_concurrency`,
    /// #32 PR 6; PR 4's attach lock was one). Held across an attach and its
    /// admission; the shutdown takes every permit before it snapshots the
    /// set, so an attach in flight is either in the set or never starts.
    attach_permits: tokio::sync::Semaphore,
    /// How many permits [`Self::attach_permits`] was made with.
    permit_count: u32,
    /// The on-demand bounds, when sessions attach on demand.
    on_demand: Option<OnDemandBounds>,
    /// The handles on-demand detaches took down, while anything may still
    /// hold them: an attach of the same id waits for its handle to go (see
    /// `PreviousHandle`), as a pinned retry does.
    previous: parking_lot::Mutex<HashMap<String, PreviousHandle>>,
    /// Which credential's request attached each on-demand session (#32 PR 6
    /// review M4), for the fair share of places. Set when an attach
    /// reserves its place, dropped with the slot. Locked after `slots`,
    /// never before.
    owners: parking_lot::Mutex<HashMap<String, String>>,
    /// Negative on-demand attach outcomes, answered without the store for
    /// a while (#32 PR 6 review M3). Locked after `slots`, never before.
    negative: parking_lot::Mutex<NegativeCache>,
    /// Opened once the startup sessions are in, so the heartbeat's first
    /// line sees them (the process tasks are spawned before the session
    /// parts, to keep the startup's log order).
    started: tokio::sync::watch::Sender<bool>,
    /// The process's J6 pre-arm, which each close reads for its second-signal
    /// escape and each attach's load races.
    early: EarlyShutdown,
    /// The pinned retry loop.
    retry: parking_lot::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// The idle sweeper (on demand only).
    sweeper: parking_lot::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Detaches and on-demand attaches in flight, joined by the process
    /// shutdown. An in-serve erase (#32 PR 7) runs here too, so the
    /// shutdown waits for it.
    detaches: parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// The store every session shares (one `GraphStore`, Level B), taken
    /// from the first session admitted: what an erase of a session that is
    /// not attached runs against when no template builder is at hand (a
    /// one-session registry, #32 PR 7).
    store: std::sync::OnceLock<Arc<dyn crate::store::GraphStore>>,
    /// Set by a test to make the next on-demand attach panic right after
    /// it is admitted (#32 PR 6 Sonnet review L-a).
    #[cfg(test)]
    panic_after_admit: std::sync::atomic::AtomicBool,
}

impl SessionRegistry {
    /// A registry hosting `order` (pinned, in order), serving `default` at
    /// `/mcp`. Nothing is attached yet.
    ///
    /// `bounds` sets how many attaches run at once and whether, and within
    /// which bounds, sessions attach on demand. On-demand attach needs an
    /// attacher; without one it is off.
    pub(super) fn new(
        order: Vec<String>,
        default: Option<String>,
        policy: LeaseLossPolicy,
        attacher: Option<SessionAttacher>,
        early: EarlyShutdown,
        bounds: RegistryBounds,
    ) -> Arc<Self> {
        let permit_count = u32::try_from(bounds.attach_permits.max(1)).unwrap_or(u32::MAX);
        let on_demand = bounds
            .on_demand
            .filter(|_| attacher.is_some())
            .map(|bounds| OnDemandBounds {
                idle_detach: bounds.idle_detach.max(MIN_IDLE_DETACH),
                share_among: bounds.share_among.max(1),
                ..bounds
            });
        Arc::new(Self {
            order,
            default,
            policy,
            attacher,
            slots: parking_lot::Mutex::new(HashMap::new()),
            closing: tokio::sync::watch::channel(false).0,
            attach_permits: tokio::sync::Semaphore::new(permit_count as usize),
            permit_count,
            on_demand,
            previous: parking_lot::Mutex::new(HashMap::new()),
            owners: parking_lot::Mutex::new(HashMap::new()),
            negative: parking_lot::Mutex::new(NegativeCache::default()),
            started: tokio::sync::watch::channel(false).0,
            early,
            retry: parking_lot::Mutex::new(None),
            sweeper: parking_lot::Mutex::new(None),
            detaches: parking_lot::Mutex::new(Vec::new()),
            store: std::sync::OnceLock::new(),
            #[cfg(test)]
            panic_after_admit: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// The pinned sessions, in order.
    pub(super) fn hosted(&self) -> &[String] {
        &self.order
    }

    /// Is `id` one of the pinned sessions?
    pub(super) fn is_pinned(&self, id: &str) -> bool {
        self.order.iter().any(|pinned| pinned == id)
    }

    /// Does this registry attach sessions on demand?
    pub(super) fn attaches_on_demand(&self) -> bool {
        self.on_demand.is_some()
    }

    /// The session `/mcp` serves.
    pub(super) fn default_session(&self) -> Option<&str> {
        self.default.as_deref()
    }

    /// The attached sessions: the pinned ones in pinned order, then the
    /// on-demand ones by id.
    pub(super) fn attached(&self) -> Vec<Arc<AttachedSession>> {
        let slots = self.slots.lock();
        let mut out: Vec<_> = self
            .order
            .iter()
            .filter_map(|id| match slots.get(id) {
                Some(Slot::Live(session)) => Some(Arc::clone(session)),
                _ => None,
            })
            .collect();
        let mut on_demand: Vec<_> = slots
            .iter()
            .filter(|(id, _)| !self.is_pinned(id))
            .filter_map(|(id, slot)| match slot {
                Slot::Live(session) => Some((id.clone(), Arc::clone(session))),
                _ => None,
            })
            .collect();
        on_demand.sort_by(|a, b| a.0.cmp(&b.0));
        out.extend(on_demand.into_iter().map(|(_, session)| session));
        out
    }

    /// What a request addressed to `id` reaches right now, without
    /// attaching anything (an on-demand session not attached is
    /// [`Lookup::NotHosted`] here). [`SessionRegistry::get_or_attach`] is
    /// what the router asks; this is the tests' look at a slot.
    #[cfg(all(test, unix, feature = "store-memory", feature = "embed-fixture"))]
    pub(super) fn lookup(&self, id: &str) -> Lookup {
        let slots = self.slots.lock();
        match slots.get(id) {
            Some(slot) => answer_for(slot),
            // Hosted but in no slot: a session between states (a detach
            // clearing its slot). Not a 404, which would say "not hosted".
            None if self.is_pinned(id) => Lookup::Unavailable {
                retry_after: Duration::from_secs(1),
            },
            None => Lookup::NotHosted,
        }
    }

    /// What a request addressed to `id` reaches, attaching an on-demand
    /// session first when it is not attached (#32 PR 6, design §3.2).
    ///
    /// Reached only after the request's grant is authorized for `id`
    /// (`transport::serve_session`, design §6.2), so nothing here is an
    /// oracle: an out-of-scope caller never gets this far, and no store
    /// call is made for it. `requester` is the grant's credential and its
    /// `create` capability.
    ///
    /// 1. A live session is served at once: its use is stamped, and the
    ///    request's [`Routed::in_flight`] is taken under the slots' lock, so
    ///    neither an eviction nor the idle sweeper can take the session
    ///    while the request is on its way to it (#32 PR 6 review M1). A
    ///    request for a session whose attach is in flight waits for that
    ///    attach's outcome (single-flight: N concurrent first requests
    ///    cause one `build_attach`), for at most `ATTACH_WAIT`.
    /// 2. An absent session that is not pinned is answered from the
    ///    negative cache while it holds a fresh outcome for it (#32 PR 6
    ///    review M3), and otherwise starts an attach, unless the serve does
    ///    not attach on demand (the uniform 404), is closing (503), or
    ///    already has `2 × max_attached` attaches waiting for their probe
    ///    (503 `Retry-After`, Sonnet review L-b). The
    ///    attach (`attach_on_demand`) probes for the session before it
    ///    reserves a place or evicts anything (#32 PR 6 review H1); see
    ///    `on_demand` for the order.
    pub(super) async fn get_or_attach(
        self: &Arc<Self>,
        id: &str,
        requester: Requester<'_>,
    ) -> Routed {
        for _ in 0..ATTACH_FOLLOWS {
            let (mut done, flight_create) = match self.route_or_start(id, requester) {
                Route::Answer(routed) => return routed,
                Route::Wait { done, create } => (done, create),
            };
            // The sender is the attach's `Flight`, which always sends before
            // it goes, even on a panic; a closed channel is a retry-later,
            // never a hang, and so is a wait past `ATTACH_WAIT` (#32 PR 6
            // review M2): the attach goes on without this request.
            let outcome =
                match tokio::time::timeout(ATTACH_WAIT, done.wait_for(Option::is_some)).await {
                    Ok(Ok(outcome)) => outcome.clone(),
                    Ok(Err(_)) => None,
                    Err(_) => {
                        return Routed::answer(Lookup::Unavailable {
                            retry_after: ATTACH_BUSY_RETRY,
                        });
                    }
                };
            let lookup = match outcome.unwrap_or(AttachOutcome::Busy {
                retry_after: Duration::from_secs(1),
            }) {
                // Live now: the next round serves it.
                AttachOutcome::Attached => continue,
                // An attach that could not create it, while this request
                // may: start one that can.
                AttachOutcome::Absent if requester.create && !flight_create => continue,
                AttachOutcome::Absent => Lookup::NotHosted,
                AttachOutcome::Erased => Lookup::Erased,
                AttachOutcome::Busy { retry_after } => Lookup::Unavailable { retry_after },
                AttachOutcome::Failed => Lookup::Failed,
            };
            return Routed::answer(lookup);
        }
        Routed::answer(Lookup::Unavailable {
            retry_after: Duration::from_secs(1),
        })
    }

    /// Step 1 and 2 of [`SessionRegistry::get_or_attach`], under the slots'
    /// lock: answer now, wait on an attach in flight, or start one.
    fn route_or_start(self: &Arc<Self>, id: &str, requester: Requester<'_>) -> Route {
        let mut slots = self.slots.lock();
        match slots.get(id) {
            Some(Slot::Live(session)) => {
                // In flight from here, under the lock an eviction and the
                // idle sweeper choose under (review M1).
                let in_flight = session.activity.enter();
                return Route::Answer(Routed {
                    lookup: Lookup::Live(Arc::clone(session)),
                    in_flight: Some(in_flight),
                });
            }
            Some(Slot::Attaching { done, create, .. }) => {
                return Route::Wait {
                    done: done.clone(),
                    create: *create,
                };
            }
            Some(slot) => return Route::Answer(Routed::answer(answer_for(slot))),
            None if self.is_pinned(id) => {
                return Route::Answer(Routed::answer(Lookup::Unavailable {
                    retry_after: Duration::from_secs(1),
                }));
            }
            None => {}
        }
        let Some(bounds) = &self.on_demand else {
            return Route::Answer(Routed::answer(Lookup::NotHosted));
        };
        // Design §3.2 step 1: no attach starts once the shutdown has begun.
        if self.is_closing() {
            return Route::Answer(Routed::answer(Lookup::Unavailable {
                retry_after: Duration::from_secs(1),
            }));
        }
        // A negative outcome still fresh (review M3): no store call, no
        // permit. An `Absent` one never stops a credential with `create`.
        let cached = self.negative.lock().get(id, requester.create);
        if let Some(negative) = cached {
            return Route::Answer(Routed::answer(match negative {
                Negative::Absent => Lookup::NotHosted,
                Negative::Erased => Lookup::Erased,
                Negative::Failed => Lookup::Failed,
            }));
        }
        // An attach that has not probed takes no place (review H1), so the
        // ones waiting for their probe are bounded on their own (Sonnet
        // review L-b): a flood of distinct ids starts no task past the cap.
        let unplaced = slots
            .values()
            .filter(|slot| matches!(slot, Slot::Attaching { placed: false, .. }))
            .count();
        if unplaced >= unplaced_cap(bounds.max_attached) {
            tracing::debug!(
                session = %id,
                credential = %requester.credential,
                unplaced,
                "lambo serve: an on-demand attach was refused: too many attaches are waiting \
                 already (503)"
            );
            return Route::Answer(Routed::answer(Lookup::Unavailable {
                retry_after: ATTACH_BUSY_RETRY,
            }));
        }
        // No place is taken and nothing is evicted yet: the attach probes
        // first (review H1).
        let (tx, done) = tokio::sync::watch::channel(None);
        slots.insert(
            id.to_string(),
            Slot::Attaching {
                done: done.clone(),
                create: requester.create,
                placed: false,
            },
        );
        let flight = Flight::new(Arc::clone(self), id.to_string(), requester.create, tx);
        let credential = requester.credential.to_string();
        let registry = Arc::clone(self);
        let task = tokio::spawn(async move {
            registry.attach_on_demand(flight, credential).await;
        });
        // Tracked before the slots' lock is released, so a shutdown that
        // takes the set after it also joins this task (review L4).
        self.track(task);
        drop(slots);
        Route::Wait {
            done,
            create: requester.create,
        }
    }

    /// An on-demand attach, on its own task so a request that goes away
    /// does not cancel it half way (a lease taken and not admitted), bounded
    /// by `ATTACH_TIMEOUT` (review M2), and ended through its [`Flight`],
    /// which answers every request waiting on it even if this panics
    /// (review L1).
    async fn attach_on_demand(self: Arc<Self>, flight: Flight, credential: String) {
        let acquiring = std::sync::atomic::AtomicBool::new(false);
        let outcome = match tokio::time::timeout(
            ATTACH_TIMEOUT,
            self.attach_outcome(&flight, &credential, &acquiring),
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(_) => {
                tracing::warn!(
                    session = %flight.id(),
                    timeout_secs = ATTACH_TIMEOUT.as_secs(),
                    "lambo serve: an on-demand attach did not finish in time and was abandoned \
                     (503); a later request tries again"
                );
                // The permit went with the attach's future. A lease the
                // acquire may have taken is released by holder.
                if acquiring.load(std::sync::atomic::Ordering::SeqCst) {
                    self.release_abandoned(flight.id()).await;
                }
                AttachOutcome::Busy {
                    retry_after: ATTACH_BUSY_RETRY,
                }
            }
        };
        flight.finish(outcome);
    }

    /// The attach itself: a permit, the existence probe, the place (and
    /// the eviction that frees one), the acquire and the admission, each
    /// raced against the shutdown.
    ///
    /// Nothing is evicted until the probe says the attach will go ahead
    /// (#32 PR 6 review H1): an erased session, one that does not exist
    /// and may not be created, and one another live writer holds each end
    /// the attach before a place is reserved.
    async fn attach_outcome(
        self: &Arc<Self>,
        flight: &Flight,
        credential: &str,
        acquiring: &std::sync::atomic::AtomicBool,
    ) -> AttachOutcome {
        let id = flight.id();
        let busy = AttachOutcome::Busy {
            retry_after: Duration::from_secs(1),
        };
        let Some(attacher) = &self.attacher else {
            return AttachOutcome::Failed;
        };
        // Step 4: a permit (`attach_concurrency`), released when the attach
        // is admitted or has failed.
        let permit = tokio::select! {
            biased;
            () = self.closed() => return busy,
            permit = self.attach_permits.acquire() => permit,
        };
        let Ok(_permit) = permit else {
            return busy;
        };
        if self.is_closing() {
            return busy;
        }
        if self.awaiting_previous_on_demand(id) {
            return busy;
        }
        // Step 2, existence: a lease row (#23 never deletes one, and every
        // writer attach makes one). Read for a `create` credential too, so
        // an erased session is told apart before any acquire, and a session
        // another live writer holds before anything is evicted.
        let Some(store) = attacher.template.shared_store() else {
            return AttachOutcome::Failed;
        };
        let session = crate::types::SessionId::new(id);
        let probe = tokio::select! {
            biased;
            () = self.closed() => return busy,
            probe = tokio::time::timeout(PROBE_TIMEOUT, store.read_lease(&session)) => probe,
        };
        let mine = crate::store::lease::LeaseHolder::for_this_process(&crate::types::AgentId::new(
            &attacher.agent,
        ))
        .token();
        match probe {
            Ok(Ok(Some(row))) if crate::store::erase::is_tombstone(&row) => {
                tracing::info!(
                    session = %id,
                    "lambo serve: an on-demand attach of an erased session was refused"
                );
                return AttachOutcome::Erased;
            }
            Ok(Ok(Some(row))) => {
                if let Some(lapses_in) = held_elsewhere(&row, &mine) {
                    tracing::info!(
                        session = %id,
                        holder = %row.holder,
                        retry_secs = lapses_in.as_secs(),
                        "lambo serve: an on-demand session is held by another writer (503)"
                    );
                    return AttachOutcome::Busy {
                        retry_after: lapses_in,
                    };
                }
            }
            Ok(Ok(None)) if flight.create() => {}
            Ok(Ok(None)) => return AttachOutcome::Absent,
            Ok(Err(e)) => {
                tracing::warn!(
                    session = %id,
                    error = %e,
                    "lambo serve: an on-demand attach could not read the session's lease; \
                     answering 503"
                );
                return busy;
            }
            Err(_) => {
                tracing::warn!(
                    session = %id,
                    timeout_secs = PROBE_TIMEOUT.as_secs(),
                    "lambo serve: an on-demand attach's lease read timed out; answering 503"
                );
                return busy;
            }
        }
        // Step 3, capacity, now that the attach will go ahead.
        let victim = match self.reserve_place(flight, credential) {
            Reserved::Free => None,
            Reserved::Evicting(victim) => Some(victim),
            Reserved::Full => {
                return AttachOutcome::Busy {
                    retry_after: AT_CAPACITY_RETRY,
                };
            }
            Reserved::Stopped => return busy,
        };
        if let Some((victim_id, victim)) = victim {
            // Detached on a tracked task of its own, so neither the attach's
            // timeout nor the shutdown can leave it half done; the attach
            // waits for it, so the evicted lease is released before the new
            // one is taken and the live `Memory`s never exceed the cap.
            let (detached, gone) = tokio::sync::oneshot::channel::<()>();
            let registry = Arc::clone(self);
            let task = tokio::spawn(async move {
                registry
                    .run_detach(&victim_id, victim, DetachReason::Evicted)
                    .await;
                let _ = detached.send(());
            });
            self.track(task);
            tokio::select! {
                biased;
                () = self.closed() => return busy,
                _ = gone => {}
            }
        }
        // Step 5, `build_attach` through the template, raced against the
        // shutdown like a pinned retry (#32 review L8).
        acquiring.store(true, std::sync::atomic::Ordering::SeqCst);
        let attempt = tokio::select! {
            biased;
            () = self.closed() => None,
            attempt = self.acquire(id) => Some(attempt),
        };
        let Some(attempt) = attempt else {
            self.release_abandoned(id).await;
            return busy;
        };
        match attempt {
            Ok(Acquired::Attached(mem, endpoint)) => {
                let session = self.admit(mem, endpoint);
                #[cfg(test)]
                if self
                    .panic_after_admit
                    .swap(false, std::sync::atomic::Ordering::SeqCst)
                {
                    panic!("test: an on-demand attach panics after its admission");
                }
                tracing::info!(
                    session = %session.id(),
                    agent = %session.mem.agent(),
                    credential = %credential,
                    "lambo serve: session attached (on demand)"
                );
                AttachOutcome::Attached
            }
            Ok(Acquired::Held(held)) => {
                // Taken between the probe and the acquire. No election wait
                // on the request path (design §3.2): the caller retries once
                // the holder's lease could lapse.
                let lapses_in = (held.current.expires_at - chrono::Utc::now())
                    .to_std()
                    .unwrap_or_default()
                    .max(Duration::from_secs(1));
                tracing::info!(
                    session = %id,
                    holder = %held.current.holder,
                    retry_secs = lapses_in.as_secs(),
                    "lambo serve: an on-demand session is held by another writer (503)"
                );
                AttachOutcome::Busy {
                    retry_after: lapses_in,
                }
            }
            Err(e) if is_transient(&e) => {
                tracing::warn!(
                    session = %id,
                    error = %e,
                    "lambo serve: an on-demand attach failed; answering 503 (a later request \
                     tries again)"
                );
                busy
            }
            Err(e) if self.is_closing() || self.shutdown_signalled().await => {
                tracing::debug!(
                    session = %id,
                    error = %e,
                    "lambo serve: an on-demand attach stopped for the shutdown"
                );
                busy
            }
            Err(e) => {
                tracing::error!(
                    session = %id,
                    error = %e,
                    "lambo serve: an on-demand session could not be attached (503); the other \
                     sessions keep serving"
                );
                AttachOutcome::Failed
            }
        }
    }

    /// Reserve one of the on-demand places for `flight`, under the slots'
    /// lock: a free one, or the place of the eviction victim the
    /// requester's share allows (`on_demand::choose_victim`), which is set
    /// `Detaching` here so no other attach picks it and no request reaches
    /// it. The session is attributed to `credential` from here on.
    fn reserve_place(&self, flight: &Flight, credential: &str) -> Reserved {
        let Some(bounds) = &self.on_demand else {
            return Reserved::Stopped;
        };
        let mut slots = self.slots.lock();
        if self.is_closing() || !flight.owns(slots.get(flight.id())) {
            return Reserved::Stopped;
        }
        let places = bounds.max_attached.saturating_sub(self.order.len());
        let taken = slots
            .iter()
            .filter(|(id, slot)| !self.is_pinned(id) && takes_a_place(slot))
            .count();
        let mut owners = self.owners.lock();
        let victim = if taken >= places {
            let share = place_share(places, bounds.share_among);
            let Some((victim_id, victim)) = choose_victim(
                &slots,
                &owners,
                |id| self.is_pinned(id),
                credential,
                share,
                bounds.min_idle_to_evict,
            ) else {
                tracing::info!(
                    max_attached = bounds.max_attached,
                    credential = %credential,
                    share,
                    "lambo serve: an on-demand attach was refused: max_attached sessions are \
                     attached and none this credential may evict is idle (503)"
                );
                return Reserved::Full;
            };
            tracing::info!(
                session = %victim_id,
                owner = %owners.get(&victim_id).map(String::as_str).unwrap_or(""),
                attaching = %flight.id(),
                credential = %credential,
                "lambo serve: evicting the least recently used idle on-demand session to make \
                 room under max_attached"
            );
            slots.insert(victim_id.clone(), Slot::Detaching);
            Some((victim_id, victim))
        } else {
            None
        };
        if let Some(Slot::Attaching { placed, .. }) = slots.get_mut(flight.id()) {
            *placed = true;
        }
        owners.insert(flight.id().to_string(), credential.to_string());
        match victim {
            Some(victim) => Reserved::Evicting(victim),
            None => Reserved::Free,
        }
    }

    /// Whether an on-demand session's previous handle (its last detach's)
    /// is still alive and worth waiting for; see `PreviousHandle`. The
    /// request is answered 503 meanwhile rather than waiting.
    fn awaiting_previous_on_demand(&self, id: &str) -> bool {
        let mut previous = self.previous.lock();
        previous.retain(|_, handle| handle.mem.strong_count() > 0);
        let Some(handle) = previous.get(id) else {
            return false;
        };
        if handle.detached_at.elapsed() < PREVIOUS_HANDLE_WAIT {
            return true;
        }
        tracing::warn!(
            session = %id,
            waited_secs = PREVIOUS_HANDLE_WAIT.as_secs(),
            "lambo serve: the detached handle of this session is still alive; attaching it \
             again anyway"
        );
        previous.remove(id);
        false
    }

    /// Keep `task` for the shutdown to join (stage 3).
    fn track(&self, task: tokio::task::JoinHandle<()>) {
        let mut tasks = self.detaches.lock();
        tasks.retain(|task| !task.is_finished());
        tasks.push(task);
    }

    /// Put hosted session `id` in a state that is not serving, without the
    /// detach, lease or retry that would normally lead there, so the
    /// router's answers for each state can be compared (#32 PR 5). Gated
    /// like its reader, the registry tests.
    #[cfg(all(test, unix, feature = "store-memory", feature = "embed-fixture"))]
    pub(super) fn force_state(&self, id: &str, state: ForcedState) {
        let slot = match state {
            ForcedState::Detaching => Slot::Detaching,
            ForcedState::HeldElsewhere => Slot::HeldElsewhere {
                retry_at: Instant::now() + PINNED_RETRY,
                previous: None,
                warned: false,
            },
            ForcedState::Failed => Slot::Failed,
            ForcedState::Erased => Slot::Erased,
        };
        self.slots.lock().insert(id.to_string(), slot);
    }

    /// Abort every on-demand attach and detach task in flight, which ends
    /// an attach as a panic in it would (#32 PR 6 review L1). Gated like
    /// its reader, the registry tests.
    #[cfg(all(test, unix, feature = "store-memory", feature = "embed-fixture"))]
    pub(super) fn abort_tasks(&self) {
        for task in self.detaches.lock().iter() {
            task.abort();
        }
    }

    /// Make the next on-demand attach panic right after its admission, as a
    /// bug between `admit` and the flight's end would (#32 PR 6 Sonnet
    /// review L-a). Gated like its reader, the registry tests.
    #[cfg(all(test, unix, feature = "store-memory", feature = "embed-fixture"))]
    pub(super) fn panic_after_next_admit(&self) {
        self.panic_after_admit
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Mark the startup set complete: the process tasks waiting on it start.
    pub(super) fn mark_started(&self) {
        self.started.send_replace(true);
    }

    /// Resolve once [`SessionRegistry::mark_started`] has run.
    pub(super) async fn started(&self) {
        let mut rx = self.started.subscribe();
        // An error means the registry is gone, and so is anything to wait for.
        let _ = rx.wait_for(|started| *started).await;
    }

    /// Hold `session` as attached and, under `DetachSession`, start its
    /// lease-loss watcher.
    ///
    /// The slot is filled first, so a fence that latches the moment the
    /// watcher starts finds the session live and detaches it.
    pub(super) fn insert_live(self: &Arc<Self>, session: Arc<AttachedSession>) {
        let id = session.id().to_string();
        // Every session shares the one store; the first one in leaves it
        // for an erase of a session that is not attached (#32 PR 7).
        let _ = self.store.set(Arc::clone(session.mem.store()));
        self.slots
            .lock()
            .insert(id, Slot::Live(Arc::clone(&session)));
        if self.policy == LeaseLossPolicy::DetachSession {
            let watcher = tokio::spawn(watch_lease(
                Arc::downgrade(self),
                Arc::clone(&session.mem),
                session.server.ledger().cloned(),
            ));
            *session.tasks.lease_watcher.lock() = Some(watcher);
        }
    }

    /// Take the lease for pinned session `id` through the template builder:
    /// configuration validation, preflight, the acquire (which arms the J6
    /// pre-arm on the first one), the load raced against it, the contract
    /// check and the background tasks — `build_attach`, unchanged.
    pub(super) async fn acquire(&self, id: &str) -> Result<Acquired, LamboError> {
        let attacher = self
            .attacher
            .as_ref()
            .ok_or_else(|| LamboError::Config("this serve attaches no session itself".into()))?;
        let endpoint = derive_endpoint(id, &attacher.store_cfg);
        let mut builder = attacher
            .template
            .clone()
            .session(id)
            .ledger(attacher.ledger.as_ref().map(|l| l.for_session(id)));
        if let Some(endpoint) = &endpoint {
            builder = builder.endpoint(endpoint.published());
        }
        Ok(
            match builder
                .build_attach()
                .await
                .map_err(explain_startup_failure)?
            {
                Attach::Attached(mem) => Acquired::Attached(Arc::from(mem), endpoint),
                Attach::Held(held) => Acquired::Held(held),
            },
        )
    }

    /// Build the serving parts of a session whose lease `mem` holds (its
    /// server, endpoint and event pump), below the arming, and hold it.
    pub(super) fn admit(
        self: &Arc<Self>,
        mem: Arc<Memory>,
        endpoint: Option<SessionEndpoint>,
    ) -> Arc<AttachedSession> {
        let (ledger, max_sessions, host_check, session_rps) = match &self.attacher {
            Some(attacher) => (
                attacher.ledger.clone(),
                attacher.max_sessions,
                attacher.host_check,
                attacher.session_rps,
            ),
            None => (
                None,
                crate::mcp::DEFAULT_MAX_SESSIONS,
                HostCheck::Loopback,
                0,
            ),
        };
        let server = session_server(&mem, &ledger);
        let session = Arc::new(AttachedSession::attach(
            mem,
            server,
            endpoint,
            max_sessions,
            host_check,
            session_rps,
        ));
        self.insert_live(Arc::clone(&session));
        session
    }

    /// A pinned session whose lease another writer holds: serve the others
    /// and try it again in [`PINNED_RETRY`]. Logged, and (J4) recorded from
    /// the loser's side, once.
    pub(super) async fn mark_held(&self, id: &str, held: &LeaseHeldElsewhere) {
        if let Some(attacher) = &self.attacher {
            let ledger = attacher.ledger.as_ref().map(|l| l.for_session(id));
            let my_token = crate::store::lease::LeaseHolder::for_this_process(
                &crate::types::AgentId::new(&attacher.agent),
            )
            .token();
            record_refused_loser(
                &ledger,
                &held.store,
                &crate::types::SessionId::new(id),
                &attacher.agent,
                &my_token,
                &held.current.holder,
            )
            .await;
        }
        tracing::warn!(
            session = %id,
            holder = %held.current.holder,
            retry_secs = PINNED_RETRY.as_secs(),
            "lambo serve: a pinned session is held by another writer; serving the other sessions \
             and retrying it in the background (requests for it get 503 until then)"
        );
        self.slots.lock().insert(
            id.to_string(),
            Slot::HeldElsewhere {
                retry_at: Instant::now() + PINNED_RETRY,
                previous: None,
                warned: false,
            },
        );
    }

    /// Start the background retry of pinned sessions held elsewhere
    /// (`DetachSession` only: a one-session serve never holds a slot in
    /// that state). Stopped at stage 5.
    pub(super) fn spawn_retry_loop(self: &Arc<Self>) {
        if self.policy != LeaseLossPolicy::DetachSession {
            return;
        }
        let registry = Arc::downgrade(self);
        *self.retry.lock() = Some(tokio::spawn(retry_loop(registry)));
    }

    /// The pinned sessions due for a retry.
    fn due(&self) -> Vec<String> {
        let now = Instant::now();
        let slots = self.slots.lock();
        self.order
            .iter()
            .filter(|id| {
                matches!(slots.get(id.as_str()), Some(Slot::HeldElsewhere { retry_at, .. }) if *retry_at <= now)
            })
            .cloned()
            .collect()
    }

    /// Whether a detached session's old handle is still alive and worth
    /// waiting for (see [`PREVIOUS_HANDLE_WAIT`]). When it is, the retry is
    /// pushed back by [`ELECTION_RETRY`]; once it is gone, or the wait is
    /// over, the slot forgets it.
    fn awaiting_previous(&self, id: &str) -> bool {
        let mut slots = self.slots.lock();
        let Some(Slot::HeldElsewhere {
            retry_at, previous, ..
        }) = slots.get_mut(id)
        else {
            return false;
        };
        let Some(handle) = previous.as_ref() else {
            return false;
        };
        if handle.mem.strong_count() == 0 {
            *previous = None;
            return false;
        }
        if handle.detached_at.elapsed() < PREVIOUS_HANDLE_WAIT {
            *retry_at = Instant::now() + ELECTION_RETRY;
            return true;
        }
        tracing::warn!(
            session = %id,
            waited_secs = PREVIOUS_HANDLE_WAIT.as_secs(),
            "lambo serve: the detached handle of this session is still alive; attaching it \
             again anyway"
        );
        *previous = None;
        false
    }

    /// One background attempt to take pinned session `id` back.
    ///
    /// Held elsewhere, or a transient error (the store or the embedder
    /// could not be reached): tried again in [`PINNED_RETRY`], the error
    /// logged at WARN once per streak. Any other error will not clear on
    /// its own: logged once at ERROR, and the slot becomes
    /// [`Slot::Failed`] (#32 review L1).
    async fn retry(self: &Arc<Self>, id: &str) {
        // One of the `attach_concurrency` permits (#32 PR 6), raced against
        // the shutdown, which takes them all.
        let permit = tokio::select! {
            biased;
            () = self.closed() => return,
            permit = self.attach_permits.acquire() => permit,
        };
        let Ok(_attaching) = permit else {
            return;
        };
        // Only a session still held elsewhere is retried: an erase (#32 PR
        // 7) may have taken the slot while this waited for its permit.
        if !matches!(self.slots.lock().get(id), Some(Slot::HeldElsewhere { .. })) {
            return;
        }
        if self.is_closing() || self.awaiting_previous(id) {
            return;
        }
        let warned = matches!(
            self.slots.lock().get(id),
            Some(Slot::HeldElsewhere { warned: true, .. })
        );
        // The whole attach observes the shutdown (#32 review L8): the
        // preflight, the acquire and the store round trips are not raced
        // against J6's pre-arm, and a serve whose sessions were all held at
        // startup has not armed it at all. Abandoned, it may hold the lease
        // it took, so that is released before the shutdown's close set is
        // taken; the attach permit is held until then.
        // Bounded like an on-demand attach (#32 PR 6 review M2), so a hung
        // store cannot hold an attach permit for good; a timeout is a
        // transient failure, tried again in `PINNED_RETRY`.
        let attempt = tokio::select! {
            biased;
            () = self.closed() => None,
            attempt = tokio::time::timeout(ATTACH_TIMEOUT, self.acquire(id)) => Some(attempt),
        };
        let Some(attempt) = attempt else {
            self.release_abandoned(id).await;
            return;
        };
        let attempt = match attempt {
            Ok(attempt) => attempt,
            Err(_) => {
                self.release_abandoned(id).await;
                Err(LamboError::Store(crate::types::StoreError::Other(
                    anyhow::anyhow!(
                        "the attach did not finish within {} s",
                        ATTACH_TIMEOUT.as_secs()
                    ),
                )))
            }
        };
        let (next, warned) = match attempt {
            Ok(Acquired::Attached(mem, endpoint)) => {
                let session = self.admit(mem, endpoint);
                tracing::info!(
                    session = %session.id(),
                    agent = %session.mem.agent(),
                    "lambo serve: session attached (re-elected in the background)"
                );
                return;
            }
            Ok(Acquired::Held(held)) => {
                tracing::debug!(
                    session = %id,
                    holder = %held.current.holder,
                    "lambo serve: pinned session still held elsewhere"
                );
                (Instant::now() + PINNED_RETRY, false)
            }
            Err(e) if is_transient(&e) => {
                if warned {
                    tracing::debug!(
                        session = %id,
                        error = %e,
                        "lambo serve: retrying a pinned session failed again"
                    );
                } else {
                    tracing::warn!(
                        session = %id,
                        error = %e,
                        retry_secs = PINNED_RETRY.as_secs(),
                        "lambo serve: retrying a pinned session failed; trying again later"
                    );
                }
                (Instant::now() + PINNED_RETRY, true)
            }
            // A shutdown signal that lands during the attach's load makes it
            // fail with a configuration error ("shutdown signal arrived"),
            // which is not the session's fault: the process is exiting, so
            // leave the slot as it was and say nothing alarming (#32 PR 4
            // Sonnet review L-A). `build_attach`'s error path has already
            // released any lease it took.
            Err(e) if self.is_closing() || self.shutdown_signalled().await => {
                tracing::debug!(
                    session = %id,
                    error = %e,
                    "lambo serve: a background attach stopped for the shutdown"
                );
                return;
            }
            // Erased (decided on the lease row, never the message): the
            // tombstone refuses every acquire, so the slot is `Erased`, not
            // `Failed` (#32 PR 4 note for PR 7).
            Err(e) if self.is_tombstoned(id).await => {
                tracing::warn!(
                    session = %id,
                    error = %e,
                    "lambo serve: a pinned session was erased; it is not retried and requests \
                     for it get the erased refusal"
                );
                self.slots.lock().insert(id.to_string(), Slot::Erased);
                return;
            }
            Err(e) => {
                tracing::error!(
                    session = %id,
                    error = %e,
                    "lambo serve: a pinned session could not be attached again and will not be \
                     retried: requests for it get 503 until the serve is restarted. The other \
                     sessions keep serving"
                );
                self.slots.lock().insert(id.to_string(), Slot::Failed);
                return;
            }
        };
        self.slots.lock().insert(
            id.to_string(),
            Slot::HeldElsewhere {
                retry_at: next,
                previous: None,
                warned,
            },
        );
    }

    /// Whether a shutdown signal has already been recorded. Never waits:
    /// `EarlyShutdown::fired` resolves at once when one was, and a zero
    /// timeout polls it exactly once.
    async fn shutdown_signalled(&self) -> bool {
        tokio::time::timeout(std::time::Duration::ZERO, self.early.fired())
            .await
            .is_ok()
    }

    /// Whether the process shutdown has taken the attached set.
    fn is_closing(&self) -> bool {
        *self.closing.borrow()
    }

    /// Resolve once the process shutdown has taken the attached set.
    async fn closed(&self) {
        let mut rx = self.closing.subscribe();
        // `self` holds the sender, so this cannot fail while it is awaited.
        let _ = rx.wait_for(|closing| *closing).await;
    }

    /// Release the lease a background attach of `id` may have taken before
    /// the shutdown abandoned it (#32 review L8). Holder-scoped, so a lease
    /// the attach never took, or another writer holds, is left alone;
    /// bounded by `LEASE_RELEASE_GRACE`, past which the row lapses at TTL.
    async fn release_abandoned(&self, id: &str) {
        let Some(attacher) = &self.attacher else {
            return;
        };
        let Some(store) = attacher.template.shared_store() else {
            return;
        };
        let holder = crate::store::lease::LeaseHolder::for_this_process(
            &crate::types::AgentId::new(&attacher.agent),
        );
        let session = crate::types::SessionId::new(id);
        match tokio::time::timeout(LEASE_RELEASE_GRACE, store.release_lease(&session, &holder))
            .await
        {
            Ok(Ok(())) => tracing::info!(
                session = %id,
                "lambo serve: an attach was abandoned (shutdown or timeout); its lease, if it \
                 took one, is released"
            ),
            Ok(Err(e)) => tracing::warn!(
                session = %id,
                error = %e,
                "lambo serve: an attach was abandoned (shutdown or timeout) and its lease could \
                 not be released; it will lapse at TTL"
            ),
            Err(_) => tracing::warn!(
                session = %id,
                grace_secs = LEASE_RELEASE_GRACE.as_secs(),
                "lambo serve: an attach was abandoned (shutdown or timeout) and releasing its \
                 lease timed out; it will lapse at TTL"
            ),
        }
    }

    /// Detach session `id` in the background (design §3.4); the process
    /// shutdown waits for it.
    pub(super) fn spawn_detach(self: &Arc<Self>, id: String, reason: DetachReason) {
        let registry = Arc::clone(self);
        let task = tokio::spawn(async move { registry.detach(&id, reason).await });
        self.track(task);
    }

    /// Take session `id` down if it is live (see
    /// [`SessionRegistry::run_detach`]); a session in any other state is
    /// left as it is.
    pub(super) async fn detach(self: &Arc<Self>, id: &str, reason: DetachReason) {
        let session = {
            let mut slots = self.slots.lock();
            match slots.get(id) {
                Some(Slot::Live(session)) => {
                    let session = Arc::clone(session);
                    slots.insert(id.to_string(), Slot::Detaching);
                    session
                }
                _ => return,
            }
        };
        self.run_detach(id, session, reason).await;
    }

    /// Take one session down while the process keeps serving the others
    /// (design §3.4), under its own stage record
    /// ([`ShutdownProgress::for_session`]): stage 1 ends its MCP sessions,
    /// stages 3 and 4 close it ([`close_sessions`]), stage 5 stops its
    /// watcher, stage 6 releases its endpoint. Stages 2 and 7 are the
    /// process's and are skipped; the write-queue calibration is the
    /// process's too and is not touched (PR 3).
    ///
    /// The caller has set the session's slot to `Detaching`, and it reads
    /// so throughout. Then a pinned session's slot becomes `HeldElsewhere`,
    /// so the retry loop takes it back once it can; an on-demand session's
    /// slot is removed (#32 PR 6), so its next request attaches it again.
    /// The detach drops the registry's handle on the session before that
    /// (#32 review M1): the slot was its only long-lived owner, so the
    /// `Memory` goes with it, and with it its second-writer registration
    /// and its graph. The next attach waits for the handle to be gone
    /// (`PreviousHandle`).
    ///
    /// A lost lease's close refuses to flush or release (the fence); an
    /// idle or evicted session's close flushes its tail and releases its
    /// lease, keeping the fencing token, so the next attach mints the next
    /// one (#23).
    async fn run_detach(
        self: &Arc<Self>,
        id: &str,
        session: Arc<AttachedSession>,
        reason: DetachReason,
    ) {
        tracing::info!(
            session = %id,
            reason = reason.as_str(),
            "lambo serve: detaching a session"
        );
        let progress = ShutdownProgress::for_session(id);
        // Stage 1: this session's MCP sessions, bounded like the transport,
        // and cut short once the process shutdown takes the attached set:
        // its own transport drain has ended the connections by then, and a
        // detach must not spend a second SHUTDOWN_GRACE inside the process's
        // stage 3 (#32 review L9).
        progress.begin(Stage::TransportDrain);
        tokio::select! {
            drained = tokio::time::timeout(SHUTDOWN_GRACE, session.close_mcp_sessions()) => {
                if drained.is_err() {
                    tracing::warn!(
                        session = %id,
                        grace_secs = SHUTDOWN_GRACE.as_secs(),
                        "lambo serve: MCP sessions did not end within the grace window; closing \
                         anyway"
                    );
                }
            }
            () = self.closed() => {}
        }
        progress.end(Stage::TransportDrain);
        // Stages 3 and 4. A fenced handle's close refuses to flush or
        // release, so its outcome is the honest "tail lost" line, named.
        let _ = close_sessions(&[session.closing()], &self.early, &progress)
            .await
            .named()
            .report();
        // Stage 5: this session's own task.
        progress.run(Stage::BackgroundTasks, || session.tasks.stop());
        // Stage 6.
        progress.begin(Stage::EndpointRelease);
        session.release_endpoint().await;
        progress.end(Stage::EndpointRelease);
        progress.complete();
        let previous = PreviousHandle {
            mem: Arc::downgrade(&session.mem),
            detached_at: Instant::now(),
        };
        drop(session);
        if self.is_pinned(id) {
            self.slots.lock().insert(
                id.to_string(),
                Slot::HeldElsewhere {
                    retry_at: Instant::now() + PINNED_RETRY,
                    previous: Some(previous),
                    warned: false,
                },
            );
            return;
        }
        if previous.mem.strong_count() > 0 {
            self.previous.lock().insert(id.to_string(), previous);
        }
        let mut slots = self.slots.lock();
        if matches!(slots.get(id), Some(Slot::Detaching)) {
            slots.remove(id);
            self.owners.lock().remove(id);
        }
    }

    /// Start the idle sweeper (design §3.4) when sessions attach on demand:
    /// every [`IDLE_SWEEP_MAX`] (or `idle_detach`, when shorter) it detaches
    /// each on-demand session unused for `idle_detach` with no call in
    /// flight. Pinned sessions are never idle-detached. Stopped at stage 5.
    pub(super) fn spawn_idle_sweeper(self: &Arc<Self>) {
        let Some(bounds) = &self.on_demand else {
            return;
        };
        // `new` keeps `idle_detach` at `MIN_IDLE_DETACH` or more, so this
        // never sleeps zero (#32 PR 6 review L5).
        let every = bounds.idle_detach.min(IDLE_SWEEP_MAX).max(MIN_IDLE_DETACH);
        let registry = Arc::downgrade(self);
        *self.sweeper.lock() = Some(tokio::spawn(idle_sweeper(registry, every)));
    }

    /// One round of the idle sweeper: take every idle on-demand session out
    /// of service and detach it in the background.
    fn sweep_idle(self: &Arc<Self>) {
        let Some(bounds) = &self.on_demand else {
            return;
        };
        let now = tokio::time::Instant::now();
        let mut slots = self.slots.lock();
        // Checked under the lock the shutdown's set is taken under, and each
        // detach tracked before it is released, so a sweep cannot start a
        // detach the shutdown does not join (#32 PR 6 review L4).
        if self.is_closing() {
            return;
        }
        let idle: Vec<(String, Arc<AttachedSession>)> = {
            let due: Vec<String> = slots
                .iter()
                .filter(|(id, _)| !self.is_pinned(id))
                .filter_map(|(id, slot)| match slot {
                    Slot::Live(session)
                        if session
                            .activity
                            .idle_at(now)
                            .is_some_and(|idle| idle >= bounds.idle_detach) =>
                    {
                        Some(id.clone())
                    }
                    _ => None,
                })
                .collect();
            due.into_iter()
                .filter_map(|id| match slots.insert(id.clone(), Slot::Detaching) {
                    Some(Slot::Live(session)) => Some((id, session)),
                    _ => None,
                })
                .collect()
        };
        for (id, session) in idle {
            let registry = Arc::clone(self);
            let task = tokio::spawn(async move {
                registry.run_detach(&id, session, DetachReason::Idle).await;
            });
            self.track(task);
        }
        drop(slots);
    }

    /// The process shutdown's attached set (stage 3): no attach starts after
    /// this, an attach in flight finishes first (and is in the set), and the
    /// live sessions leave their slots, so the set's handles are the last
    /// the registry gives out.
    pub(super) async fn close_set(&self) -> Vec<Arc<AttachedSession>> {
        self.closing.send_replace(true);
        // The idle sweeper stops before the set is taken (#32 PR 6 review
        // L4); a round already under way saw `closing` under the slots' lock
        // or tracked its detaches before this takes that lock.
        if let Some(sweeper) = self.sweeper.lock().take() {
            sweeper.abort();
        }
        // Every permit: each attach in flight has admitted its session or
        // given up by then. Never closed, so this cannot fail.
        let _no_attach_in_flight = self.attach_permits.acquire_many(self.permit_count).await;
        let mut slots = self.slots.lock();
        let mut set: Vec<_> = self
            .order
            .iter()
            .filter_map(|id| match slots.remove(id) {
                Some(Slot::Live(session)) => Some(session),
                Some(other) => {
                    slots.insert(id.clone(), other);
                    None
                }
                None => None,
            })
            .collect();
        let mut on_demand: Vec<String> = slots
            .iter()
            .filter(|(id, slot)| !self.is_pinned(id) && matches!(slot, Slot::Live(_)))
            .map(|(id, _)| id.clone())
            .collect();
        on_demand.sort();
        set.extend(
            on_demand
                .into_iter()
                .filter_map(|id| match slots.remove(&id) {
                    Some(Slot::Live(session)) => Some(session),
                    _ => None,
                }),
        );
        set
    }

    /// Wait for every detach (and on-demand attach) in flight, so its close
    /// and lease release finish before the process exits (stage 3, beside
    /// the closes). The caller bounds the wait (`close_holder`, by
    /// `CLOSE_GRACE`).
    pub(super) async fn join_detaches(&self) {
        let detaches = std::mem::take(&mut *self.detaches.lock());
        for detach in detaches {
            let _ = detach.await;
        }
    }

    /// Stage 5 for the registry: stop the retry loop, the idle sweeper and
    /// every closed session's own task.
    pub(super) fn stop_tasks(&self, closed: &[Arc<AttachedSession>]) {
        if let Some(retry) = self.retry.lock().take() {
            retry.abort();
        }
        if let Some(sweeper) = self.sweeper.lock().take() {
            sweeper.abort();
        }
        for session in closed {
            session.tasks.stop();
        }
    }
}

/// Whether a background attach error may clear without an operator: the
/// store could not be reached or answered with a backend error, or the
/// embedder could not be reached. Everything else (an erased session, a
/// contract mismatch, an unprovisioned store, a configuration error) gets
/// the same answer on every retry.
pub(super) fn is_transient(err: &LamboError) -> bool {
    use crate::types::StoreError;
    matches!(
        err,
        LamboError::Store(StoreError::Backend(_) | StoreError::Other(_))
            | LamboError::EmbedUnavailable(_)
    )
}

/// What [`SessionRegistry::route_or_start`] decided.
enum Route {
    /// Answer the request with this.
    Answer(Routed),
    /// Wait for the attach in flight.
    Wait {
        done: tokio::sync::watch::Receiver<Option<AttachOutcome>>,
        create: bool,
    },
}

/// What a request gets for a session in `slot`, without attaching.
fn answer_for(slot: &Slot) -> Lookup {
    match slot {
        Slot::Live(session) => Lookup::Live(Arc::clone(session)),
        // The attach in flight is `get_or_attach`'s to wait on; asked
        // without it, the session is not serving yet.
        Slot::Attaching { .. } | Slot::Detaching => Lookup::Unavailable {
            retry_after: Duration::from_secs(1),
        },
        Slot::HeldElsewhere { retry_at, .. } => Lookup::Unavailable {
            retry_after: retry_at
                .saturating_duration_since(Instant::now())
                .max(Duration::from_secs(1)),
        },
        Slot::Failed => Lookup::Failed,
        Slot::Erasing | Slot::Erased => Lookup::Erased,
    }
}

/// What [`SessionRegistry::reserve_place`] found.
enum Reserved {
    /// A free place.
    Free,
    /// The place of this session, now `Detaching`, which the attach
    /// detaches before it acquires.
    Evicting((String, Arc<AttachedSession>)),
    /// No free place, and none the requester may evict: 503.
    Full,
    /// The serve is closing, or the attach's slot is gone.
    Stopped,
}

/// When another live writer holds the lease in `row` (not released, not
/// this process's holder `mine`, not lapsed): how long until it could
/// lapse, at least a second. `None` when the attach may go ahead.
fn held_elsewhere(row: &crate::store::LeaseInfo, mine: &str) -> Option<Duration> {
    if row.holder == crate::store::lease::RELEASED_HOLDER || row.holder == mine {
        return None;
    }
    // A lapsed lease converts to an error: the acquire takes it over.
    let left = (row.expires_at - chrono::Utc::now()).to_std().ok()?;
    Some(left.max(Duration::from_secs(1)))
}

/// The idle sweeper: one [`SessionRegistry::sweep_idle`] round every
/// `every`. Holds the registry weakly; ends when it is gone or closing.
async fn idle_sweeper(registry: Weak<SessionRegistry>, every: Duration) {
    loop {
        tokio::time::sleep(every).await;
        let Some(registry) = registry.upgrade() else {
            return;
        };
        if registry.is_closing() {
            return;
        }
        registry.sweep_idle();
    }
}

/// The registry's background retry of pinned sessions held elsewhere. Holds
/// the registry weakly; ends when it is gone or closing.
async fn retry_loop(registry: Weak<SessionRegistry>) {
    loop {
        tokio::time::sleep(ELECTION_RETRY).await;
        let Some(registry) = registry.upgrade() else {
            return;
        };
        if registry.is_closing() {
            return;
        }
        for id in registry.due() {
            registry.retry(&id).await;
        }
    }
}

/// A session's lease-loss watcher under `DetachSession` (design §4.2): when
/// its fence latches, book the `lease:lost` line (as `wind_down` does for a
/// one-session serve) and detach the session. The detach is spawned, not
/// awaited: it aborts this watcher at its stage 5.
async fn watch_lease(
    registry: Weak<SessionRegistry>,
    mem: Arc<Memory>,
    ledger: Option<Arc<Ledger>>,
) {
    let winner = mem.lease_lost_latched().await;
    book_lease_loss(&mem, ledger.as_ref(), &winner);
    tracing::warn!(
        session = %mem.session(),
        holder = %winner,
        "lambo serve: lease lost to {winner}; detaching this session and serving the others. \
         Its writes have been refused since the fence latched, and the tail it could not flush \
         is discarded, exactly as a crash would discard it. A pinned session is retried in \
         the background; an on-demand one attaches again on its next request"
    );
    let id = mem.session().to_string();
    drop(mem);
    if let Some(registry) = registry.upgrade() {
        registry.spawn_detach(id, DetachReason::LeaseLost);
    }
}

/// The process-wide MCP-session cap counts every attached session's MCP
/// sessions (design §3.6): one `--max-sessions` for the whole process. Each
/// credential's share of it counts the MCP sessions that credential opened,
/// across every attached session (#32 PR 5 review M2).
#[async_trait::async_trait]
impl super::http_guards::LiveSessions for SessionRegistry {
    async fn live(&self) -> usize {
        let mut total = 0;
        for session in self.attached() {
            total += session.live_mcp_sessions().await;
        }
        total
    }

    async fn live_opened_by(&self, credential: &str) -> usize {
        let mut total = 0;
        for session in self.attached() {
            total += session.live_mcp_sessions_opened_by(credential).await;
        }
        total
    }
}
