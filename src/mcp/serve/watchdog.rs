//! The shutdown watchdog (#40): a plain OS thread that bounds the holder's
//! shutdown even when the async runtime cannot.
//!
//! # Why the async bounds are not enough
//!
//! Every shutdown bound in [`super::shutdown`] is a tokio timer
//! ([`SHUTDOWN_GRACE`](super::shutdown::SHUTDOWN_GRACE),
//! [`CLOSE_FLUSH_GRACE`](super::shutdown::CLOSE_FLUSH_GRACE), the lease
//! release, the endpoint release), and every one of them is polled inside
//! `serve`'s own future, which `main` drives with `Runtime::block_on`. Two
//! things void all of them at once:
//!
//! * a synchronous block inside that future's poll (a `parking_lot` lock that
//!   never comes free, a `Drop` that blocks): the timer and the work it bounds
//!   are polled by the same thread, so the timer is never looked at;
//! * every runtime worker blocked at once. A multi-thread runtime's timers
//!   are driven by a worker that parks; a `block_on` thread never drives
//!   them. Pinned by
//!   `serve::tests::watchdog::an_async_shutdown_bound_does_not_fire_on_a_wedged_runtime`.
//!
//! Either way the process logs nothing more and waits for its supervisor's
//! SIGKILL, which is what the #40 live stall looked like: `shutdown signal
//! received, winding down`, then silence until launchd's default 20 s
//! `ExitTimeOut` ran out, and no `session closed`, `close() did not finish`
//! or grace-window line, each of which a firing timer would have printed.
//!
//! # What it does
//!
//! Started by the first shutdown stage (see [`super::stages`]), the watchdog
//! thread sleeps on a condvar, wakes on every stage change, and:
//!
//! * when the running stage has outlived [`Stage::bound`] plus
//!   [`STAGE_OVERRUN_SLACK`], logs `lambo serve: shutdown watchdog: stage N/7
//!   <name> has run M ms, past its B ms bound` at WARN, once per stage. A
//!   stage only gets there if the timer that bounds it did not fire, so the
//!   line itself says the runtime was wedged;
//! * when the shutdown has run [`SHUTDOWN_WATCHDOG`], logs `lambo serve:
//!   shutdown watchdog: shutdown still running after M ms, in stage N/7
//!   <name>; aborting` at ERROR and calls [`std::process::abort`].
//!
//! Abort, not exit: SIGABRT makes macOS write a crash report with every
//! thread's stack (`~/Library/Logs/DiagnosticReports/lambo-*.ips`), and a
//! core where cores are enabled elsewhere, which is the stack capture #40's
//! runbook asked an operator to take by hand with `sample`. It costs nothing
//! the stall had not already cost: by [`SHUTDOWN_WATCHDOG`] every async bound
//! has expired, so the close would have been abandoned and its tail lost
//! anyway, and the lease lapses at its TTL exactly as it does after SIGKILL.
//!
//! Each line is written from a helper thread the watchdog waits on for at
//! most [`LOG_WAIT`], so a stderr that blocks (a full pipe under `--transport
//! stdio`) cannot keep the abort from happening.
//!
//! # Bounds
//!
//! The longest legitimate shutdown is [`EXIT_BUDGET`]: `SHUTDOWN_BUDGET`
//! (transport drain plus the bounded close) plus the endpoint release grace
//! plus the ledger's drain, 18.5 s. The watchdog fires at
//! [`SHUTDOWN_WATCHDOG`], 20 s, after it; both relations are build-time
//! asserts. A supervisor's kill timeout (launchd `ExitTimeOut`, systemd
//! `TimeoutStopSec`) must exceed [`SHUTDOWN_WATCHDOG`] for the watchdog's
//! line and crash report to happen before the SIGKILL: 30 s is the
//! recommendation. launchd's default is 20 s, which races it.
//!
//! The watchdog starts at the first stage, which is when the holder's
//! shutdown future resolves. A runtime wedged before that cannot deliver the
//! signal either (tokio reads signals on its own driver), and is outside what
//! this covers.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::stages::{Shared, Stage};

/// How long a holder's shutdown may run, from its first stage, before the
/// watchdog aborts the process. [`EXIT_BUDGET`] plus 1.5 s.
pub(crate) const SHUTDOWN_WATCHDOG: Duration = Duration::from_secs(20);

/// The longest a healthy shutdown runs, from the first stage to `serve`
/// returning: [`SHUTDOWN_BUDGET`](super::shutdown::SHUTDOWN_BUDGET) (stages
/// 1 to 4), plus `hub::ENDPOINT_RELEASE_GRACE` (stage 6), plus the ledger's
/// [`SHUTDOWN_DRAIN`](crate::ledger::SHUTDOWN_DRAIN) (stage 7). Stages 2, 4
/// and 5 only abort tasks.
pub(crate) const EXIT_BUDGET: Duration = Duration::from_millis(
    super::shutdown::SHUTDOWN_BUDGET.as_millis() as u64
        + super::hub::ENDPOINT_RELEASE_GRACE.as_millis() as u64
        + crate::ledger::SHUTDOWN_DRAIN.as_millis() as u64,
);

/// Build-time invariant: the watchdog never fires on a shutdown that is
/// within its own bounds. Raising any stage bound without raising the
/// watchdog fails the build.
const _: () = assert!(
    EXIT_BUDGET.as_millis() < SHUTDOWN_WATCHDOG.as_millis(),
    "SHUTDOWN_WATCHDOG must exceed EXIT_BUDGET (SHUTDOWN_BUDGET + ENDPOINT_RELEASE_GRACE + \
     ledger SHUTDOWN_DRAIN): the watchdog is for a shutdown whose own timers did not fire, \
     never for a slow one that is still inside them",
);

/// Build-time invariant: the lease outlives the watchdog, so a process the
/// watchdog has not yet aborted still holds a valid lease (the same reason
/// `LEASE_TTL > SHUTDOWN_BUDGET` is asserted in `shutdown`).
const _: () = assert!(
    crate::store::lease::LEASE_TTL.as_millis() > SHUTDOWN_WATCHDOG.as_millis(),
    "LEASE_TTL must exceed SHUTDOWN_WATCHDOG",
);

/// How far past its own bound a stage may run before the watchdog says so.
/// Timers fire a few milliseconds late on a loaded box; a second is not that.
pub(crate) const STAGE_OVERRUN_SLACK: Duration = Duration::from_secs(1);

/// How long the watchdog waits for one of its own log lines to be written
/// before it carries on without it.
const LOG_WAIT: Duration = Duration::from_secs(1);

/// What the watchdog saw when it acted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StallReport {
    /// The stage running at the time, and how long it had run.
    pub(crate) stage: Option<(Stage, Duration)>,
    /// How long the shutdown had run, from its first stage.
    pub(crate) since_shutdown: Duration,
}

/// What the watchdog does, and when. Production uses [`production`]; tests
/// inject their own actions and shorter bounds.
pub(crate) struct WatchSpec {
    /// When to give up on the whole shutdown.
    pub(crate) bound: Duration,
    /// How far past [`Stage::bound`] a stage may run before `on_overrun`.
    pub(crate) slack: Duration,
    /// Called once per stage that outlives its bound plus the slack.
    pub(crate) on_overrun: Box<dyn Fn(&StallReport) + Send>,
    /// Called once, when the shutdown has run `bound`. Production aborts.
    pub(crate) on_expire: Box<dyn FnOnce(&StallReport) + Send>,
}

/// The production watchdog: [`SHUTDOWN_WATCHDOG`], [`STAGE_OVERRUN_SLACK`],
/// log the overrun at WARN, log the expiry at ERROR and abort.
pub(crate) fn production() -> WatchSpec {
    WatchSpec {
        bound: SHUTDOWN_WATCHDOG,
        slack: STAGE_OVERRUN_SLACK,
        on_overrun: Box::new(|report| {
            let line = overrun_line(report);
            log_detached(move || tracing::warn!("{line}"));
        }),
        on_expire: Box::new(|report| {
            let line = expiry_line(report);
            log_detached(move || tracing::error!("{line}"));
            std::process::abort();
        }),
    }
}

/// `stage N/7 <name>`, or a note that no stage was running.
fn stage_label(report: &StallReport) -> String {
    match report.stage {
        Some((stage, _)) => format!("stage {}/{} {}", stage.number(), Stage::COUNT, stage.name()),
        None => "between stages".to_string(),
    }
}

/// The WARN line for a stage that outlived its bound.
pub(crate) fn overrun_line(report: &StallReport) -> String {
    let (ran, bound) = match report.stage {
        Some((stage, ran)) => (ran.as_millis(), stage.bound().as_millis()),
        None => (0, 0),
    };
    format!(
        "lambo serve: shutdown watchdog: {} has run {ran} ms, past its {bound} ms bound; the \
         timer that bounds it did not fire, so the runtime or this thread is wedged",
        stage_label(report)
    )
}

/// The ERROR line written just before the abort.
pub(crate) fn expiry_line(report: &StallReport) -> String {
    format!(
        "lambo serve: shutdown watchdog: shutdown still running after {} ms, in {}; aborting \
         so the process does not wait for its supervisor's SIGKILL (the crash report holds \
         every thread's stack; the un-flushed tail, if any, is lost)",
        report.since_shutdown.as_millis(),
        stage_label(report)
    )
}

/// Write a log line from a helper thread and wait for it at most
/// [`LOG_WAIT`]: a blocked stderr must not hold the watchdog.
fn log_detached(write: impl FnOnce() + Send + 'static) {
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let spawned = std::thread::Builder::new()
        .name("lambo-shutdown-watchdog-log".into())
        .spawn(move || {
            write();
            let _ = done_tx.send(());
        });
    if spawned.is_ok() {
        let _ = done_rx.recv_timeout(LOG_WAIT);
    }
}

/// Start the watchdog thread for a shutdown whose first stage began at
/// `armed_at`. Called once, by `ShutdownProgress::begin`.
pub(super) fn start(shared: Arc<Shared>, spec: WatchSpec, armed_at: Instant) {
    tracing::info!(
        bound_ms = spec.bound.as_millis(),
        "lambo serve: shutdown watchdog armed: the process aborts if the shutdown is still \
         running in {} ms",
        spec.bound.as_millis()
    );
    let spawned = std::thread::Builder::new()
        .name("lambo-shutdown-watchdog".into())
        .spawn(move || watch(&shared, spec, armed_at));
    if let Err(e) = spawned {
        tracing::warn!(
            error = %e,
            "lambo serve: could not start the shutdown watchdog; the shutdown is bounded by its \
             own timers only"
        );
    }
}

/// The watchdog loop: wait for a stage change, a stage overrun or the
/// whole bound, whichever comes first, until disarmed.
fn watch(shared: &Shared, spec: WatchSpec, armed_at: Instant) {
    let WatchSpec {
        bound,
        slack,
        on_overrun,
        on_expire,
    } = spec;
    let expires = armed_at + bound;
    // The stage start already reported, so each overrun is reported once.
    let mut reported: Option<(Stage, Instant)> = None;
    let mut state = shared.state.lock();
    loop {
        if state.disarmed {
            return;
        }
        let now = Instant::now();
        let running = state.current;
        let report = StallReport {
            stage: running.map(|(stage, at)| (stage, now.saturating_duration_since(at))),
            since_shutdown: now.saturating_duration_since(armed_at),
        };
        if now >= expires {
            drop(state);
            on_expire(&report);
            return;
        }
        let mut wake_at = expires;
        if let Some((stage, at)) = running {
            let due = at + stage.bound() + slack;
            if reported != Some((stage, at)) {
                if now >= due {
                    reported = Some((stage, at));
                    // Unlocked while the action runs: it logs, and a stage
                    // change must not wait on a log line.
                    drop(state);
                    on_overrun(&report);
                    state = shared.state.lock();
                    continue;
                }
                wake_at = wake_at.min(due);
            }
        }
        shared.changed.wait_until(&mut state, wake_at);
    }
}
