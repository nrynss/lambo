//! The sessions one serve process holds (#32 PR 4, design §3): the
//! [`SessionRegistry`], its slot states, the pinned attach, the per-session
//! detach, and the routing lookup the HTTP router asks.
//!
//! Every serve runs through a registry. A one-session serve (stdio, or HTTP
//! with one pinned session) attaches its session through the startup
//! election (`roles::resolve_role`, J2's proxy included) and holds it in a
//! registry of one under [`LeaseLossPolicy::ExitProcess`]: a lost lease ends
//! the process exactly as it always has. A serve with more than one pinned
//! session attaches each from the one `serve_builder` template (so they share
//! the embedder, the store and the write-queue calibration) under
//! [`LeaseLossPolicy::DetachSession`]: a lost lease detaches that session,
//! the others keep serving, and the session is re-elected in the background.
//!
//! # Slot states
//!
//! | slot | a request gets | how it leaves |
//! |---|---|---|
//! | [`Slot::Live`] | the session's own MCP service | a detach, or the process shutdown |
//! | [`Slot::Detaching`] | 503, `Retry-After: 1` | the detach ends |
//! | [`Slot::HeldElsewhere`] | 503, `Retry-After` until the next retry | the background retry wins the lease |
//! | [`Slot::Failed`] | 503, no `Retry-After` | never: an operator restarts the serve |
//! | absent | the uniform 404 (`surface::session`) | never, in PR 4: only pinned sessions are hosted |
//!
//! # What runs where
//!
//! The process-wide tasks (`process::ProcessTasks`) iterate [`SessionRegistry::attached`]:
//! one heartbeat line per attached session per interval and one refusal
//! poller over all of them. The registry owns two kinds of task of its own:
//! the pinned retry loop and the detaches it spawns; the process shutdown
//! waits for the detaches and stops the loop (see [`SessionRegistry::close_set`]
//! and [`SessionRegistry::stop_tasks`]).

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use super::builder::explain_startup_failure;
use super::hub::{derive_endpoint, SessionEndpoint};
use super::roles::{record_refused_loser, ELECTION_RETRY};
use super::session::{session_server, AttachedSession};
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
    /// The one place the policy is derived from configuration (design R4).
    pub(super) fn for_pinned(count: usize) -> Self {
        if count > 1 {
            Self::DetachSession
        } else {
            Self::ExitProcess
        }
    }
}

/// One session's place in the registry.
enum Slot {
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
    /// (#32 review L1). PR 7's `Erased` slot takes over the erased case.
    Failed,
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

/// What the router gets for a session id.
pub(super) enum Lookup {
    /// Serve the request on this session.
    Live(Arc<AttachedSession>),
    /// Hosted, but not serving right now: answer 503 with this `Retry-After`.
    Unavailable { retry_after: Duration },
    /// Hosted, but its attach failed for good: answer 503 with no
    /// `Retry-After`, since retrying will not help until an operator acts.
    Failed,
    /// Not a session this serve hosts: the uniform 404.
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
    /// The agent this process writes as.
    pub(super) agent: String,
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
    /// The hosted sessions, in the order they were pinned. Iteration (the
    /// heartbeat, the shutdown's close set and its outcome lines) follows it.
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
    /// Held across a background attach and its admission, and taken by the
    /// shutdown before it snapshots the set, so an attach in flight is
    /// either in the set or never starts.
    attach_lock: tokio::sync::Mutex<()>,
    /// Opened once the startup sessions are in, so the heartbeat's first
    /// line sees them (the process tasks are spawned before the session
    /// parts, to keep the startup's log order).
    started: tokio::sync::watch::Sender<bool>,
    /// The process's J6 pre-arm, which each close reads for its second-signal
    /// escape and each attach's load races.
    early: EarlyShutdown,
    /// The pinned retry loop.
    retry: parking_lot::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Detaches in flight, joined by the process shutdown.
    detaches: parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl SessionRegistry {
    /// A registry hosting `order` (pinned, in order), serving `default` at
    /// `/mcp`. Nothing is attached yet.
    pub(super) fn new(
        order: Vec<String>,
        default: Option<String>,
        policy: LeaseLossPolicy,
        attacher: Option<SessionAttacher>,
        early: EarlyShutdown,
    ) -> Arc<Self> {
        Arc::new(Self {
            order,
            default,
            policy,
            attacher,
            slots: parking_lot::Mutex::new(HashMap::new()),
            closing: tokio::sync::watch::channel(false).0,
            attach_lock: tokio::sync::Mutex::new(()),
            started: tokio::sync::watch::channel(false).0,
            early,
            retry: parking_lot::Mutex::new(None),
            detaches: parking_lot::Mutex::new(Vec::new()),
        })
    }

    /// The hosted sessions, in pinned order.
    pub(super) fn hosted(&self) -> &[String] {
        &self.order
    }

    /// The session `/mcp` serves.
    pub(super) fn default_session(&self) -> Option<&str> {
        self.default.as_deref()
    }

    /// The attached sessions, in pinned order.
    pub(super) fn attached(&self) -> Vec<Arc<AttachedSession>> {
        let slots = self.slots.lock();
        self.order
            .iter()
            .filter_map(|id| match slots.get(id) {
                Some(Slot::Live(session)) => Some(Arc::clone(session)),
                _ => None,
            })
            .collect()
    }

    /// What a request addressed to `id` reaches.
    pub(super) fn lookup(&self, id: &str) -> Lookup {
        let slots = self.slots.lock();
        match slots.get(id) {
            Some(Slot::Live(session)) => Lookup::Live(Arc::clone(session)),
            Some(Slot::Detaching) => Lookup::Unavailable {
                retry_after: Duration::from_secs(1),
            },
            Some(Slot::HeldElsewhere { retry_at, .. }) => Lookup::Unavailable {
                retry_after: retry_at
                    .saturating_duration_since(Instant::now())
                    .max(Duration::from_secs(1)),
            },
            Some(Slot::Failed) => Lookup::Failed,
            // Hosted but in no slot: a session between states (a detach
            // clearing its slot). Not a 404, which would say "not hosted".
            None if self.order.iter().any(|hosted| hosted == id) => Lookup::Unavailable {
                retry_after: Duration::from_secs(1),
            },
            None => Lookup::NotHosted,
        }
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
        let (ledger, max_sessions) = match &self.attacher {
            Some(attacher) => (attacher.ledger.clone(), attacher.max_sessions),
            None => (None, crate::mcp::DEFAULT_MAX_SESSIONS),
        };
        let server = session_server(&mem, &ledger);
        let session = Arc::new(AttachedSession::attach(mem, server, endpoint, max_sessions));
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
        let _attaching = self.attach_lock.lock().await;
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
        // taken; the attach lock is held until then.
        let attempt = tokio::select! {
            biased;
            () = self.closed() => None,
            attempt = self.acquire(id) => Some(attempt),
        };
        let Some(attempt) = attempt else {
            self.release_abandoned(id).await;
            return;
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
                "lambo serve: a background attach was abandoned at shutdown; its lease, if it \
                 took one, is released"
            ),
            Ok(Err(e)) => tracing::warn!(
                session = %id,
                error = %e,
                "lambo serve: a background attach was abandoned at shutdown and its lease could \
                 not be released; it will lapse at TTL"
            ),
            Err(_) => tracing::warn!(
                session = %id,
                grace_secs = LEASE_RELEASE_GRACE.as_secs(),
                "lambo serve: a background attach was abandoned at shutdown and releasing its \
                 lease timed out; it will lapse at TTL"
            ),
        }
    }

    /// Detach session `id` in the background (design §3.4); the process
    /// shutdown waits for it.
    pub(super) fn spawn_detach(self: &Arc<Self>, id: String) {
        let registry = Arc::clone(self);
        let task = tokio::spawn(async move { registry.detach(&id).await });
        let mut detaches = self.detaches.lock();
        detaches.retain(|task| !task.is_finished());
        detaches.push(task);
    }

    /// Take one session down while the process keeps serving the others
    /// (design §3.4), under its own stage record
    /// ([`ShutdownProgress::for_session`]): stage 1 ends its MCP sessions,
    /// stages 3 and 4 close it ([`close_sessions`]), stage 5 stops its
    /// watcher, stage 6 releases its endpoint. Stages 2 and 7 are the
    /// process's and are skipped; the write-queue calibration is the
    /// process's too and is not touched (PR 3).
    ///
    /// The session's slot reads `Detaching` throughout, then
    /// `HeldElsewhere`, so the retry loop takes it back once it can. The
    /// detach drops the registry's handle on the session before that (#32
    /// review M1): the slot was its only long-lived owner, so the fenced
    /// `Memory` goes with it, and with it its second-writer registration
    /// and its graph. The retry waits for the handle to be gone before it
    /// attaches again (`PreviousHandle`). A lost
    /// lease is the only detach in PR 4; idle, eviction, erase and the
    /// operator's detach come with later PRs.
    pub(super) async fn detach(self: &Arc<Self>, id: &str) {
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
        self.slots.lock().insert(
            id.to_string(),
            Slot::HeldElsewhere {
                retry_at: Instant::now() + PINNED_RETRY,
                previous: Some(previous),
                warned: false,
            },
        );
    }

    /// The process shutdown's attached set (stage 3): no attach starts after
    /// this, an attach in flight finishes first (and is in the set), and the
    /// live sessions leave their slots, so the set's handles are the last
    /// the registry gives out.
    pub(super) async fn close_set(&self) -> Vec<Arc<AttachedSession>> {
        self.closing.send_replace(true);
        let _no_attach_in_flight = self.attach_lock.lock().await;
        let mut slots = self.slots.lock();
        self.order
            .iter()
            .filter_map(|id| match slots.remove(id) {
                Some(Slot::Live(session)) => Some(session),
                Some(other) => {
                    slots.insert(id.clone(), other);
                    None
                }
                None => None,
            })
            .collect()
    }

    /// Wait for every detach in flight, so its close and lease release
    /// finish before the process exits (stage 3, beside the closes). The
    /// caller bounds the wait (`close_holder`, by `CLOSE_GRACE`).
    pub(super) async fn join_detaches(&self) {
        let detaches = std::mem::take(&mut *self.detaches.lock());
        for detach in detaches {
            let _ = detach.await;
        }
    }

    /// Stage 5 for the registry: stop the retry loop and every closed
    /// session's own task.
    pub(super) fn stop_tasks(&self, closed: &[Arc<AttachedSession>]) {
        if let Some(retry) = self.retry.lock().take() {
            retry.abort();
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
fn is_transient(err: &LamboError) -> bool {
    use crate::types::StoreError;
    matches!(
        err,
        LamboError::Store(StoreError::Backend(_) | StoreError::Other(_))
            | LamboError::EmbedUnavailable(_)
    )
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
         is discarded, exactly as a crash would discard it. It is retried in the background"
    );
    let id = mem.session().to_string();
    drop(mem);
    if let Some(registry) = registry.upgrade() {
        registry.spawn_detach(id);
    }
}

/// The process-wide MCP-session cap counts every attached session's MCP
/// sessions (design §3.6): one `--max-sessions` for the whole process.
#[async_trait::async_trait]
impl super::http_guards::LiveSessions for SessionRegistry {
    async fn live(&self) -> usize {
        let mut total = 0;
        for session in self.attached() {
            total += session.live_mcp_sessions().await;
        }
        total
    }
}
