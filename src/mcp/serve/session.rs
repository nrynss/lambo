//! The holder's per-session part (#32 PR 2): what one attached session owns
//! on top of its `Memory`. The process-wide part is in `process`.

use std::sync::Arc;

use super::heartbeat::log_events;
use crate::ledger::Ledger;
use crate::mcp::server::LamboServer;
use crate::memory::Memory;

/// The session's MCP server handle, built below the arming (see
/// [`serve`](super::serve)).
pub(super) fn session_server(mem: &Arc<Memory>, ledger: &Option<Arc<Ledger>>) -> LamboServer {
    // I1/I2. `Ledger::open` never fails — a bad path warns once and counts
    // every line as a drop — so nothing here can stop a memory server from
    // serving memory. ONE server handle for the whole process (the HTTP factory
    // clones it), so every transport appends to the same file and the
    // heartbeat's uptime is the session's, not a request's.
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
