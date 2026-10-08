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
use std::time::Instant;

use parking_lot::Mutex;

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
    /// 5: `HolderTasks::stop`.
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
struct State {
    /// When the first stage started: the shutdown's own clock.
    began: Option<Instant>,
    /// The stage running now, and when it started.
    current: Option<(Stage, Instant)>,
}

/// The holder's shutdown progress: which stage is running and since when.
///
/// One per serve process, shared between the shutdown future (which starts
/// stage 1), [`super::shutdown::run_and_close`] (stages 1 to 4) and
/// [`super::serve`] (stages 5 to 7). Cloning shares the record.
///
/// The lock is a `parking_lot` mutex held for one statement at a time and
/// never across an `.await`; logging happens after it is released.
#[derive(Clone, Default)]
pub(crate) struct ShutdownProgress {
    state: Arc<Mutex<State>>,
}

impl ShutdownProgress {
    /// A fresh record with no stage started.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Start `stage`: log it and make it the current one. Idempotent for the
    /// stage already running (its start time is kept).
    pub(crate) fn begin(&self, stage: Stage) {
        let now = Instant::now();
        {
            let mut state = self.state.lock();
            if matches!(state.current, Some((running, _)) if running == stage) {
                return;
            }
            state.began.get_or_insert(now);
            state.current = Some((stage, now));
        }
        tracing::info!(
            stage = stage.number(),
            stage_name = stage.name(),
            "lambo serve: shutdown stage {}/{} {} started",
            stage.number(),
            Stage::COUNT,
            stage.name(),
        );
    }

    /// Finish `stage`: log its elapsed time. A stage that was never begun is
    /// begun and finished here, so every stage of a shutdown has both lines.
    pub(crate) fn end(&self, stage: Stage) {
        let started = {
            let state = self.state.lock();
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
        self.state.lock().current = None;
        tracing::info!(
            stage = stage.number(),
            stage_name = stage.name(),
            elapsed_ms,
            "lambo serve: shutdown stage {}/{} {} finished in {elapsed_ms} ms",
            stage.number(),
            Stage::COUNT,
            stage.name(),
        );
    }

    /// Run a synchronous stage between its two lines.
    pub(crate) fn run<T>(&self, stage: Stage, f: impl FnOnce() -> T) -> T {
        self.begin(stage);
        let out = f();
        self.end(stage);
        out
    }

    /// Log the whole shutdown's elapsed time, from the first stage's start.
    pub(crate) fn complete(&self) {
        let began = self.state.lock().began;
        if let Some(began) = began {
            let elapsed_ms = began.elapsed().as_millis();
            tracing::info!(
                elapsed_ms,
                "lambo serve: shutdown finished in {elapsed_ms} ms"
            );
        }
    }
}
