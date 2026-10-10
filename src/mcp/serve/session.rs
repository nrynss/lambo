//! The holder's per-session part (#32 PR 2): what one attached session owns
//! on top of its `Memory`. The process-wide part is in `process`.
//!
//! [`serve`](super::serve) builds one [`AttachedSession`] today, below the
//! arming, and runs the per-session shutdown stages over a set of one. #32's
//! registry (PR 4) holds many; nothing here assumes there is only one.
//!
//! | piece | per session | where it stops |
//! |---|---|---|
//! | `Memory` (graph, daemon, flush, lease and its heartbeat, write pipeline) | [`AttachedSession::mem`] | stage 3, `close_bounded` |
//! | the MCP server handle | [`AttachedSession::server`] | with the transport (stage 1) |
//! | its streamable-HTTP service and MCP-session manager | [`AttachedSession::http`], [`AttachedSession::mcp_sessions`] | with the transport (stage 1), or a detach's stage 1 |
//! | the event pump | [`SessionTasks::event_pump`] | stage 4 |
//! | the lease-loss watcher (multi-session serves only) | [`SessionTasks::lease_watcher`] | stage 5 |
//! | the session endpoint (J2, the #39 seam) | [`AttachedSession::hub`] | stage 6, [`AttachedSession::release_endpoint`] |
//! | its use and its request rate (#32 PR 6) | [`AttachedSession::activity`], [`AttachedSession::rate`] | with the session |

use std::sync::Arc;
use std::time::Duration;

use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::session::SessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};

use super::activity::SessionActivity;
use super::heartbeat::log_events;
use super::http_guards::RateLimiter;
use super::hub::{bind_hub, Hub, SessionEndpoint};
use super::openers::{AttributingSessions, Openers};
use super::shutdown::SessionClose;
use crate::ledger::Ledger;
use crate::mcp::server::LamboServer;
use crate::memory::Memory;

/// The session's MCP server handle, built below the arming (see
/// [`serve`](super::serve)).
pub(super) fn session_server(mem: &Arc<Memory>, ledger: &Option<Arc<Ledger>>) -> LamboServer {
    // I1/I2. `Ledger::open` never fails — a bad path warns once and counts
    // every line as a drop — so nothing here can stop a memory server from
    // serving memory. ONE server handle per session (the HTTP factory clones
    // it), so every transport appends to the same file and the heartbeat's
    // uptime is the session's, not a request's.
    match ledger {
        Some(ledger) => LamboServer::with_ledger(mem.clone(), Arc::clone(ledger)),
        None => LamboServer::new(mem.clone()),
    }
}

/// Start the session's event pump: the daemon's events, logged until the
/// pump is aborted at stage 4 (after the close).
pub(super) fn spawn_event_pump(mem: &Memory) -> tokio::task::JoinHandle<()> {
    // Exactly once, at startup: `events()` is stateful on its first call — it
    // hands out the receiver subscribed *before* the daemon spawned, so the
    // spec §2.5 warm-up condition set (on a resumed session, the whole restored
    // set) is not lost. Draining it here also stops the broadcast channel from
    // filling and lagging the daemon.
    let events = mem.events();
    tokio::spawn(log_events(events))
}

/// One session this process holds: its `Memory` (lease taken) and what
/// `serve` builds over it.
///
/// Built by [`AttachedSession::attach`] once the lease is held and the
/// shutdown is armed; taken apart by the shutdown stages (see the table in
/// `super::shutdown`).
pub(super) struct AttachedSession {
    /// The session's memory. Its lease, fence, write pipeline and caches are
    /// its own.
    pub(super) mem: Arc<Memory>,
    /// The MCP server over [`Self::mem`]. The transport serves a clone.
    pub(super) server: LamboServer,
    /// This session's streamable-HTTP service (#32 PR 4). One per session,
    /// because rmcp's service neither reads the request path nor lets its
    /// factory see the request: the router picks the session, then hands
    /// the request to that session's service. An `Mcp-Session-Id` minted
    /// here is unknown to every other session's manager.
    pub(super) http: StreamableHttpService<LamboServer, AttributingSessions>,
    /// The MCP sessions [`Self::http`] has minted: read by the process-wide
    /// session cap, and ended one by one by a detach's stage 1.
    pub(super) mcp_sessions: Arc<LocalSessionManager>,
    /// Which credential opened each of [`Self::mcp_sessions`]' MCP
    /// sessions, by `Mcp-Session-Id` (#32 PR 5 review L1). Written by
    /// [`Self::http`]'s session manager at the moment it mints an id (see
    /// `super::openers`). A request that names an MCP session another
    /// credential opened is answered as rmcp answers an id it does not
    /// know (see `transport::serve_live`).
    pub(super) openers: Arc<Openers>,
    /// The session endpoint (J2): the accept loop and its connections.
    ///
    /// In a lock and an `Option` so stage 6 can take it through a shared
    /// reference: #32 PR 4's registry holds sessions as
    /// `Arc<AttachedSession>`, and a request or the router may still hold a
    /// clone when a detach or the shutdown reaches stage 6 (#32 review M2).
    /// An async lock, held for the whole release, so a second caller waits
    /// for the first to finish rather than returning while the socket is
    /// still being removed. A session dropped without the release still
    /// drops its `Hub`, whose `Drop` aborts the accept loop (#28).
    hub: tokio::sync::Mutex<Option<Hub>>,
    /// The endpoint address this session derived and published, if any.
    pub(super) endpoint: Option<SessionEndpoint>,
    /// The session's own background tasks.
    pub(super) tasks: SessionTasks,
    /// When the session was last used and whether a tool call is running in
    /// it (#32 PR 6): what the idle sweeper and an eviction read. Shared
    /// with [`Self::server`], which holds a call in flight on it.
    pub(super) activity: Arc<SessionActivity>,
    /// The session's own request bucket at `[serve] per_session_rps` (#32
    /// PR 6, design §3.6), drawn after the credential's, so one session's
    /// flood cannot spend another's rate. `None` when the rate is 0.
    pub(super) rate: Option<RateLimiter>,
}

/// A session's own background tasks, as opposed to the process-wide ones in
/// [`ProcessTasks`](super::process::ProcessTasks).
pub(super) struct SessionTasks {
    /// The daemon-event logger; aborted at stage 4, after the close, so the
    /// final drain's events still reach the log (R1/T82-17).
    pub(super) event_pump: tokio::task::JoinHandle<()>,
    /// The lease-loss watcher of a multi-session serve (#32 design §4.2,
    /// `DetachSession`), set once the session is in the registry; aborted
    /// at stage 5. A one-session serve has none: its fence ends the whole
    /// process through `wind_down`, as it always has.
    pub(super) lease_watcher: parking_lot::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl SessionTasks {
    /// Stage 5 for this session: stop its lease-loss watcher, if any.
    pub(super) fn stop(&self) {
        if let Some(watcher) = self.lease_watcher.lock().take() {
            watcher.abort();
        }
    }
}

/// Which `Host` headers a session's streamable-HTTP service answers
/// (#32 PR 5 review M1).
///
/// rmcp's default refuses every `Host` but `localhost`, `127.0.0.1` and
/// `::1` with a 403, as DNS-rebinding protection: a page a browser loaded
/// from an attacker's name, re-pointed at a loopback serve, would otherwise
/// reach it as same-origin. That protection is what an **unauthenticated**
/// serve needs, and it is kept for one. A serve that requires a bearer token
/// on every request does not need it, because the page cannot present a
/// token it does not know, and it cannot keep it: a serve bound beyond
/// loopback is reached under its own address or a DNS name, never under
/// `localhost`, so the allow-list refused every request the credentials
/// were configured to admit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HostCheck {
    /// rmcp's loopback allow-list: the implicit `local` credential, and a
    /// stdio serve (which never routes HTTP).
    Loopback,
    /// Any `Host`: every request presents a bearer token (the legacy
    /// `default` credential or a configured one).
    Any,
}

impl HostCheck {
    /// The check for a serve with `authority` (`None`: no HTTP credential
    /// set, a stdio serve).
    pub(super) fn for_authority(authority: Option<&super::authority::ServeAuthority>) -> Self {
        match authority {
            Some(authority) if authority.requires_bearer() => Self::Any,
            _ => Self::Loopback,
        }
    }
}

/// The streamable-HTTP configuration every session's service uses: the SDK
/// default with Lambo's body cap, a 15 s SSE keep-alive, and the `Host`
/// allow-list `host` says.
pub(super) fn http_config(host: HostCheck) -> StreamableHttpServerConfig {
    // `#[non_exhaustive]` — mutate the SDK default rather than
    // constructing, so a new field cannot silently break the build.
    let mut cfg = StreamableHttpServerConfig::default()
        // #101: Lambo's ceiling, not an inherited default. rmcp streams a
        // body and answers 413 past it before parsing any of it; the guard
        // reads an opener's body under the same constant, and the
        // line-framed transports cap a frame at it too.
        .with_max_request_body_bytes(super::frames::MAX_MCP_FRAME_BYTES);
    cfg.sse_keep_alive = Some(Duration::from_secs(15));
    match host {
        HostCheck::Loopback => cfg,
        HostCheck::Any => cfg.disable_allowed_hosts(),
    }
}

impl AttachedSession {
    /// Attach the serving parts of a session whose lease `mem` holds: bind
    /// its endpoint and start its event pump, in that order. `host` is the
    /// HTTP service's `Host` check ([`HostCheck`]); `session_rps` its own
    /// request rate (0: none).
    ///
    /// Called below the arming, like every holder startup step (see
    /// [`serve`](super::serve)), so a signal during it still reaches the
    /// close.
    pub(super) fn attach(
        mem: Arc<Memory>,
        server: LamboServer,
        endpoint: Option<SessionEndpoint>,
        max_sessions: usize,
        host: HostCheck,
        session_rps: u32,
    ) -> Self {
        // #32 PR 6: every handle of this session's server (the HTTP
        // factory's and the endpoint's) records its use on one activity.
        let activity = Arc::new(SessionActivity::new());
        let server = server.with_activity(Arc::clone(&activity));
        // J2 — the session endpoint, bound HERE: below the arming and below
        // `LamboServer`, which it needs. A bind failure degrades, it does not stop
        // this process serving memory; see `hub::bind_hub`.
        let hub = bind_hub(endpoint.as_ref(), &server, max_sessions);

        let event_pump = spawn_event_pump(&mem);

        // CLONED, not rebuilt (I1): every request handler must share the one
        // call ledger, and `LamboServer::new` per request would also rebuild
        // the whole `ToolRouter` — every tool's JSON schema included — on
        // every request. Cloning shares the `Arc<Memory>`. Building the
        // service spawns nothing; a stdio serve never routes to it.
        let factory_server = server.clone();
        let mcp_sessions = Arc::new(LocalSessionManager::default());
        let openers = Arc::new(Openers::default());
        let http = StreamableHttpService::new(
            move || Ok(factory_server.clone()),
            Arc::new(AttributingSessions::new(
                Arc::clone(&mcp_sessions),
                Arc::clone(&openers),
            )),
            http_config(host),
        );

        Self {
            mem,
            server,
            http,
            mcp_sessions,
            openers,
            hub: tokio::sync::Mutex::new(Some(hub)),
            endpoint,
            tasks: SessionTasks {
                event_pump,
                lease_watcher: parking_lot::Mutex::new(None),
            },
            activity,
            rate: RateLimiter::new(session_rps, std::time::Instant::now()),
        }
    }

    /// Take one token from this session's own bucket, if it has one (#32
    /// PR 6). Always true when the session has no bucket.
    pub(super) fn admits_request(&self) -> bool {
        self.rate
            .as_ref()
            .is_none_or(|rate| rate.try_acquire_at(std::time::Instant::now()))
    }

    /// The session's id.
    pub(super) fn id(&self) -> &crate::types::SessionId {
        self.mem.session()
    }

    /// How many MCP sessions this session's HTTP service holds open.
    pub(super) async fn live_mcp_sessions(&self) -> usize {
        self.mcp_sessions.sessions.read().await.len()
    }

    /// How many of this session's live MCP sessions `credential` opened
    /// (#32 PR 5 review M2: its share of the session cap).
    pub(super) async fn live_mcp_sessions_opened_by(&self, credential: &str) -> usize {
        let live = self.mcp_sessions.sessions.read().await;
        self.openers
            .count_opened_by(credential, |id| live.contains_key(id))
    }

    /// A detach's stage 1 (#32 design §3.4): end every MCP session this
    /// session's HTTP service minted, each through rmcp's own
    /// `close_session`. A client that calls again gets rmcp's unknown-session
    /// answer and re-initializes once the session is back.
    pub(super) async fn close_mcp_sessions(&self) {
        let ids: Vec<_> = self
            .mcp_sessions
            .sessions
            .read()
            .await
            .keys()
            .cloned()
            .collect();
        for id in ids {
            if let Err(e) = self.mcp_sessions.close_session(&id).await {
                tracing::warn!(
                    session = %self.mem.session(),
                    error = %e,
                    "lambo serve: could not close an MCP session during a detach"
                );
            }
        }
    }

    /// What stages 3 and 4 need of this session.
    pub(super) fn closing(&self) -> SessionClose<'_> {
        SessionClose {
            mem: &self.mem,
            event_pump: &self.tasks.event_pump,
        }
    }

    /// Stage 6 for this session (J2 / JE2E-2): AFTER `close()`, the accept
    /// loop stops and every endpoint session is ended (bounded), then the
    /// socket file goes, only if it is still the one this process bound.
    ///
    /// Through `&self`, so a session shared as `Arc<AttachedSession>` can be
    /// released while other clones are alive. The hub is taken out of its
    /// slot under a lock held until the release ends, so a second call (a
    /// detach racing the shutdown) waits for the first and then finds
    /// nothing: the endpoint is released exactly once, and neither caller
    /// returns before it is gone.
    pub(super) async fn release_endpoint(&self) {
        // Held across the release on purpose (an async lock): see `hub`.
        let mut slot = self.hub.lock().await;
        if let Some(hub) = slot.take() {
            hub.release(self.endpoint.as_ref()).await;
        }
    }
}
