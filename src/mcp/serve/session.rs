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
//! | the event pump | [`SessionTasks::event_pump`] | stage 4 |
//! | the session endpoint (J2, the #39 seam) | [`AttachedSession::hub`] | stage 6, [`AttachedSession::release_endpoint`] |

use std::sync::Arc;

use super::heartbeat::log_events;
use super::hub::{bind_hub, Hub, SessionEndpoint};
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
    /// The session endpoint (J2): the accept loop and its connections.
    ///
    /// In a lock and an `Option` so stage 6 can take it through a shared
    /// reference: #32 PR 4's registry holds sessions as
    /// `Arc<AttachedSession>`, and a request or the router may still hold a
    /// clone when a detach or the shutdown reaches stage 6 (#32 review M2).
    /// A session dropped without the release still drops its `Hub`, whose
    /// `Drop` aborts the accept loop (#28).
    hub: parking_lot::Mutex<Option<Hub>>,
    /// The endpoint address this session derived and published, if any.
    pub(super) endpoint: Option<SessionEndpoint>,
    /// The session's own background tasks.
    pub(super) tasks: SessionTasks,
}

/// A session's own background tasks, as opposed to the process-wide ones in
/// [`ProcessTasks`](super::process::ProcessTasks).
pub(super) struct SessionTasks {
    /// The daemon-event logger; aborted at stage 4, after the close, so the
    /// final drain's events still reach the log (R1/T82-17).
    pub(super) event_pump: tokio::task::JoinHandle<()>,
}

impl AttachedSession {
    /// Attach the serving parts of a session whose lease `mem` holds: bind
    /// its endpoint and start its event pump, in that order.
    ///
    /// Called below the arming, like every holder startup step (see
    /// [`serve`](super::serve)), so a signal during it still reaches the
    /// close.
    pub(super) fn attach(
        mem: Arc<Memory>,
        server: LamboServer,
        endpoint: Option<SessionEndpoint>,
        max_sessions: usize,
    ) -> Self {
        // J2 — the session endpoint, bound HERE: below the arming and below
        // `LamboServer`, which it needs. A bind failure degrades, it does not stop
        // this process serving memory; see `hub::bind_hub`.
        let hub = bind_hub(endpoint.as_ref(), &server, max_sessions);

        let event_pump = spawn_event_pump(&mem);

        Self {
            mem,
            server,
            hub: parking_lot::Mutex::new(Some(hub)),
            endpoint,
            tasks: SessionTasks { event_pump },
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
    /// slot, so a second call (a detach racing the shutdown) finds nothing
    /// and returns at once: the endpoint is released exactly once.
    pub(super) async fn release_endpoint(&self) {
        // Its own statement: the guard is released before the await below.
        let hub = self.hub.lock().take();
        if let Some(hub) = hub {
            hub.release(self.endpoint.as_ref()).await;
        }
    }
}
