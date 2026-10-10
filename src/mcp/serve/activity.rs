//! When an attached session was last used, and whether a tool call is
//! running in it (#32 PR 6, design §3.4 and §3.6).
//!
//! The registry reads it for two decisions about on-demand sessions: the
//! idle sweeper detaches one unused for `idle_detach_secs`, and an attach at
//! `max_attached` evicts the least recently used one. Both skip a session
//! with a call in flight.
//!
//! **Idle is defined by tool calls, not connections** (design R7). An SSE
//! stream holds an MCP session open without using it, so a connection never
//! counts as in flight. The router stamps `last_used` on every request it
//! routes to the session; [`LamboServer`](crate::mcp::server::LamboServer)
//! holds an [`InFlight`] guard for the whole of each tool call, which covers
//! the calls that arrive through the session's unix endpoint (a stdio
//! proxy) as well as over HTTP.
//!
//! The clock is tokio's, so a test can pause it and advance past the idle
//! time without waiting for it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

/// One attached session's use.
#[derive(Debug)]
pub(crate) struct SessionActivity {
    last_used: parking_lot::Mutex<Instant>,
    in_flight: AtomicUsize,
}

impl SessionActivity {
    /// Used now, with nothing in flight.
    pub(crate) fn new() -> Self {
        Self {
            last_used: parking_lot::Mutex::new(Instant::now()),
            in_flight: AtomicUsize::new(0),
        }
    }

    /// Stamp `last_used` with now.
    pub(crate) fn touch(&self) {
        *self.last_used.lock() = Instant::now();
    }

    /// A tool call starts: in flight until the guard drops, which stamps
    /// `last_used` again, so a long call does not end already idle.
    pub(crate) fn enter(self: &Arc<Self>) -> InFlight {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        self.touch();
        InFlight(Arc::clone(self))
    }

    /// When the session was last used.
    pub(crate) fn last_used(&self) -> Instant {
        *self.last_used.lock()
    }

    /// How long the session has been idle at `now`: `None` while a call is
    /// in flight, since such a session is never idle.
    pub(crate) fn idle_at(&self, now: Instant) -> Option<Duration> {
        if self.in_flight.load(Ordering::SeqCst) > 0 {
            return None;
        }
        Some(now.saturating_duration_since(self.last_used()))
    }
}

impl Default for SessionActivity {
    fn default() -> Self {
        Self::new()
    }
}

/// A tool call in flight (see [`SessionActivity::enter`]).
#[derive(Debug)]
pub(crate) struct InFlight(Arc<SessionActivity>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.touch();
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_session_is_idle_only_with_nothing_in_flight() {
        let activity = Arc::new(SessionActivity::new());
        tokio::time::advance(Duration::from_secs(5)).await;
        assert_eq!(
            activity.idle_at(Instant::now()),
            Some(Duration::from_secs(5))
        );
        let call = activity.enter();
        tokio::time::advance(Duration::from_secs(60)).await;
        assert_eq!(activity.idle_at(Instant::now()), None, "a call is running");
        drop(call);
        assert_eq!(
            activity.idle_at(Instant::now()),
            Some(Duration::ZERO),
            "the end of a call is a use"
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        activity.touch();
        assert_eq!(activity.idle_at(Instant::now()), Some(Duration::ZERO));
    }
}
