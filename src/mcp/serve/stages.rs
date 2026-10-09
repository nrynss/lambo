//! Shutdown stage logging (#40): the holder's seven shutdown stages, each
//! logged when it starts and when it finishes, with its elapsed time.
//!
//! The live stall in #40 logged `shutdown signal received, winding down` and
//! then nothing until launchd killed the process, so nobody could say which
//! stage held it. With these lines the last `started` that has no matching
//! `finished` names the stage. The lines are stable and meant to be grepped:
//!
//! ```text
//! lambo serve: shutdown stage 1/7 transport_drain started
//! lambo serve: shutdown stage 1/7 transport_drain finished in 4 ms
//! ...
//! lambo serve: shutdown stage 7/7 ledger_close finished in 1 ms
//! lambo serve: shutdown finished in 37 ms
//! ```
//!
//! Each line also carries the structured fields `stage` (the number),
//! `stage_name` and, on `finished`, `elapsed_ms`. `Memory::close` logs its
//! own steps inside stage 3 the same way (`close: step N/10 <name> ...`,
//! `src/memory/shutdown.rs`).
//!
//! Stage 1 starts when the holder's shutdown future resolves (a signal or a
//! lost lease, see [`super::shutdown::holder_shutdown`]) and finishes when the
//! transport returns. A transport that ended on its own (a stdio client
//! hanging up) never had a drain: its stage 1 starts and finishes at once.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use super::watchdog::{self, WatchSpec};

/// One of the holder's shutdown stages, in the order `serve` runs them. The
/// stage table in [`super::shutdown`] is the authority for what each does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    /// 1: the transport winds down (HTTP graceful drain, stdio cancel).
    TransportDrain,
    /// 2: the embedder keep-warm is aborted, before the close.
    KeepWarmAbort,
    /// 3: `close_bounded` around `Memory::close`.
    SessionClose,
    /// 4: the event pump is aborted, after the close.
    EventPumpAbort,
    /// 5: `ProcessTasks::stop`.
    BackgroundTasks,
    /// 6: `Hub::release`.
    EndpointRelease,
    /// 7: `close_ledger`.
    LedgerClose,
}

impl Stage {
    /// How many stages there are; the denominator in every line.
    pub(crate) const COUNT: u8 = 7;

    /// The stage's number, 1-based, as the stage table numbers it.
    pub(crate) fn number(self) -> u8 {
        match self {
            Stage::TransportDrain => 1,
            Stage::KeepWarmAbort => 2,
            Stage::SessionClose => 3,
            Stage::EventPumpAbort => 4,
            Stage::BackgroundTasks => 5,
            Stage::EndpointRelease => 6,
            Stage::LedgerClose => 7,
        }
    }

    /// The stage's stable name in log lines. Changing one breaks every grep
    /// an operator has written against it.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Stage::TransportDrain => "transport_drain",
            Stage::KeepWarmAbort => "keep_warm_abort",
            Stage::SessionClose => "session_close",
            Stage::EventPumpAbort => "event_pump_abort",
            Stage::BackgroundTasks => "background_tasks",
            Stage::EndpointRelease => "endpoint_release",
            Stage::LedgerClose => "ledger_close",
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct State {
    /// When the first stage started: the shutdown's own clock.
    pub(super) began: Option<Instant>,
    /// The stage running now, and when it started.
    pub(super) current: Option<(Stage, Instant)>,
    /// Set by [`ShutdownProgress::complete`] or a [`Disarm`] guard: the
    /// shutdown is over and the watchdog, if any, stands down.
    pub(super) disarmed: bool,
}

/// What a [`ShutdownProgress`] shares between its clones and its watchdog.
#[derive(Default)]
pub(super) struct Shared {
    pub(super) state: Mutex<State>,
    /// Notified on every stage change and on disarm, so the watchdog
    /// re-reads the state instead of sleeping out a stale deadline.
    pub(super) changed: Condvar,
    /// The watchdog to start with the first stage; taken when it starts.
    watch: Mutex<Option<WatchSpec>>,
}

/// The holder's shutdown progress: which stage is running and since when.
///
/// One per serve process, shared between the shutdown future (which starts
/// stage 1), [`super::shutdown::run_and_close_sessions`] (stages 1 to 4) and
/// [`super::serve`] (stages 5 to 7). Cloning shares the record. A session
/// detach (#32 PR 4) takes a record of its own,
/// [`ShutdownProgress::for_session`].
///
/// The lock is a `parking_lot` mutex held for one statement at a time and
/// never across an `.await`; logging happens after it is released.
///
/// Built by `serve` with [`ShutdownProgress::watched`], it also starts the
/// shutdown watchdog (`super::watchdog`) when the first stage begins.
#[derive(Clone, Default)]
pub(crate) struct ShutdownProgress {
    shared: Arc<Shared>,
    /// Set by [`ShutdownProgress::for_session`]: the one session these stages
    /// are about, logged as a `session` field on every line. `None` for the
    /// process's own shutdown, whose lines carry no `session`.
    session: Option<Arc<str>>,
}

impl ShutdownProgress {
    /// A fresh record with no stage started and no watchdog: the tests'
    /// record, which logs and never aborts anything. Gated as its only
    /// callers are (the close and stage tests need the in-memory store and
    /// the fixture embedder), so no feature row sees it unused.
    #[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// A record whose first stage starts the shutdown watchdog described by
    /// `spec` (see `super::watchdog`). `serve` uses
    /// [`watchdog::production`]; tests pass their own actions and bounds.
    pub(crate) fn watched(spec: WatchSpec) -> Self {
        let progress = Self::default();
        *progress.shared.watch.lock() = Some(spec);
        progress
    }

    /// The production record: the watchdog fires at
    /// [`watchdog::SHUTDOWN_WATCHDOG`] and aborts the process.
    pub(crate) fn with_production_watchdog() -> Self {
        Self::watched(watchdog::production())
    }

    /// A record for one session's own stages (#32 design §3.4): a detach that
    /// takes one session down while the process keeps serving the others.
    /// Every line it logs carries a `session` field. The stage lines read as
    /// the process's do; the summary says `session detach finished`, not
    /// `shutdown finished`, because the process is not shutting down.
    /// It never starts a watchdog: the watchdog bounds the whole process's
    /// shutdown, not a session's (see `super::watchdog`).
    pub(crate) fn for_session(session: &str) -> Self {
        Self {
            shared: Arc::default(),
            session: Some(Arc::from(session)),
        }
    }

    /// A guard that disarms the watchdog when it drops, so a `serve` that
    /// returns (or unwinds) by any path never leaves one running.
    pub(crate) fn disarm_on_drop(&self) -> Disarm {
        Disarm(self.clone())
    }

    fn disarm(&self) {
        self.shared.state.lock().disarmed = true;
        self.shared.changed.notify_all();
    }

    /// Start `stage`: log it and make it the current one. Idempotent for the
    /// stage already running (its start time is kept).
    pub(crate) fn begin(&self, stage: Stage) {
        let now = Instant::now();
        let first = {
            let mut state = self.shared.state.lock();
            if matches!(state.current, Some((running, _)) if running == stage) {
                return;
            }
            let first = state.began.is_none();
            state.began.get_or_insert(now);
            state.current = Some((stage, now));
            first
        };
        self.shared.changed.notify_all();
        // Take the spec in its own statement so the `watch` guard is released
        // before the watchdog thread is spawned. Chaining `lock().take()` into
        // the `if` would hold that guard across `watchdog::start` (#48).
        let spec = if first {
            self.shared.watch.lock().take()
        } else {
            None
        };
        if let Some(spec) = spec {
            watchdog::start(Arc::clone(&self.shared), spec, now);
        }
        match &self.session {
            None => tracing::info!(
                stage = stage.number(),
                stage_name = stage.name(),
                "lambo serve: shutdown stage {}/{} {} started",
                stage.number(),
                Stage::COUNT,
                stage.name(),
            ),
            Some(session) => tracing::info!(
                session = %session,
                stage = stage.number(),
                stage_name = stage.name(),
                "lambo serve: shutdown stage {}/{} {} started",
                stage.number(),
                Stage::COUNT,
                stage.name(),
            ),
        }
    }

    /// Finish `stage`: log its elapsed time. A stage that was never begun is
    /// begun and finished here, so every stage of a shutdown has both lines.
    pub(crate) fn end(&self, stage: Stage) {
        let started = {
            let state = self.shared.state.lock();
            match state.current {
                Some((running, at)) if running == stage => Some(at),
                _ => None,
            }
        };
        let started = match started {
            Some(at) => at,
            None => {
                self.begin(stage);
                Instant::now()
            }
        };
        let elapsed_ms = started.elapsed().as_millis();
        self.shared.state.lock().current = None;
        self.shared.changed.notify_all();
        match &self.session {
            None => tracing::info!(
                stage = stage.number(),
                stage_name = stage.name(),
                elapsed_ms,
                "lambo serve: shutdown stage {}/{} {} finished in {elapsed_ms} ms",
                stage.number(),
                Stage::COUNT,
                stage.name(),
            ),
            Some(session) => tracing::info!(
                session = %session,
                stage = stage.number(),
                stage_name = stage.name(),
                elapsed_ms,
                "lambo serve: shutdown stage {}/{} {} finished in {elapsed_ms} ms",
                stage.number(),
                Stage::COUNT,
                stage.name(),
            ),
        }
    }

    /// Run a synchronous stage between its two lines.
    pub(crate) fn run<T>(&self, stage: Stage, f: impl FnOnce() -> T) -> T {
        self.begin(stage);
        let out = f();
        self.end(stage);
        out
    }

    /// Log the whole shutdown's elapsed time, from the first stage's start,
    /// and stand the watchdog down.
    pub(crate) fn complete(&self) {
        self.disarm();
        let began = self.shared.state.lock().began;
        if let Some(began) = began {
            let elapsed_ms = began.elapsed().as_millis();
            match &self.session {
                None => tracing::info!(
                    elapsed_ms,
                    "lambo serve: shutdown finished in {elapsed_ms} ms"
                ),
                // A detach takes one session down while the process keeps
                // serving, so its summary must not read as the process's
                // shutdown. Its stage lines keep the shared text.
                Some(session) => tracing::info!(
                    session = %session,
                    elapsed_ms,
                    "lambo serve: session detach finished in {elapsed_ms} ms"
                ),
            }
        }
    }
}

/// Disarms a [`ShutdownProgress`]'s watchdog on drop; see
/// [`ShutdownProgress::disarm_on_drop`].
pub(crate) struct Disarm(ShutdownProgress);

impl Drop for Disarm {
    fn drop(&mut self) {
        self.0.disarm();
    }
}

impl Stage {
    /// The longest this stage runs when the runtime is healthy: the timer
    /// that bounds it, or zero for a stage that only aborts tasks. The
    /// watchdog warns when a stage overruns this (plus its slack), which
    /// can only happen when that timer did not fire.
    pub(crate) fn bound(self) -> Duration {
        use super::shutdown::{CLOSE_GRACE, SHUTDOWN_GRACE};
        match self {
            Stage::TransportDrain => SHUTDOWN_GRACE,
            Stage::SessionClose => CLOSE_GRACE,
            Stage::EndpointRelease => super::hub::ENDPOINT_RELEASE_GRACE,
            Stage::LedgerClose => crate::ledger::SHUTDOWN_DRAIN,
            Stage::KeepWarmAbort | Stage::EventPumpAbort | Stage::BackgroundTasks => Duration::ZERO,
        }
    }
}
