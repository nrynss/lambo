//! #40: the shutdown watchdog bounds a shutdown whose own timers cannot fire.
//!
//! The wedge these tests build is a one-worker multi-thread runtime whose
//! only worker is blocked in synchronous code: the shape that voids every
//! tokio timer `serve`'s future is waiting on (see `serve::watchdog`).

use super::super::stages::{ShutdownProgress, Stage};
use super::super::watchdog::{
    expiry_line, overrun_line, StallReport, WatchSpec, EXIT_BUDGET, SHUTDOWN_WATCHDOG,
};
use super::*;
use std::sync::mpsc;

/// A runtime whose only worker is blocked until the returned sender sends
/// (or drops).
fn wedged_runtime() -> (Arc<tokio::runtime::Runtime>, mpsc::Sender<()>) {
    let rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("runtime"),
    );
    let (started_tx, started_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    rt.spawn(async move {
        started_tx.send(()).expect("signal start");
        // Synchronous on purpose: this is the wedge.
        let _ = release_rx.recv();
    });
    started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the wedging task runs");
    (rt, release_tx)
}

/// The premise, pinned: on a wedged runtime the transport's grace window,
/// a tokio timer polled inside `block_on`, does not end the transport. A
/// 50 ms grace has not fired a second later. This is why the async bounds
/// alone cannot promise `SHUTDOWN_BUDGET`, and why the #40 live stall
/// printed none of the lines a firing timer prints.
#[test]
fn an_async_shutdown_bound_does_not_fire_on_a_wedged_runtime() {
    let (rt, release) = wedged_runtime();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let driver = {
        let rt = Arc::clone(&rt);
        std::thread::spawn(move || {
            rt.block_on(async {
                let exit = run_until_shutdown(
                    std::future::pending::<()>(),
                    || {},
                    async {},
                    Duration::from_millis(50),
                )
                .await;
                assert_eq!(exit, Exit::Forced);
            });
            let _ = done_tx.send(());
        })
    };
    assert!(
        done_rx.recv_timeout(Duration::from_secs(1)).is_err(),
        "control: a 50 ms grace must NOT fire while the runtime is wedged, or this test \
         shows nothing about the watchdog's reason to exist"
    );
    drop(release);
    done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("once the worker is free the timer fires");
    driver.join().expect("driver thread");
}

/// What a test watchdog reports back.
enum Seen {
    Overrun(StallReport),
    Expired(StallReport),
}

fn test_spec(bound: Duration, slack: Duration) -> (WatchSpec, mpsc::Receiver<Seen>) {
    let (tx, rx) = mpsc::channel::<Seen>();
    let overrun_tx = tx.clone();
    let overrun_tx = std::sync::Mutex::new(overrun_tx);
    let spec = WatchSpec {
        bound,
        slack,
        on_overrun: Box::new(move |r| {
            let _ = overrun_tx.lock().unwrap().send(Seen::Overrun(r.clone()));
        }),
        on_expire: Box::new(move |r| {
            let _ = tx.send(Seen::Expired(r.clone()));
        }),
    };
    (spec, rx)
}

/// The fix: with the runtime wedged mid-stage, the watchdog (an OS thread)
/// still reports the overrun and then expires, naming the stage, within its
/// bound. Production's expiry aborts the process; this one reports.
#[test]
fn the_watchdog_fires_on_a_wedged_runtime_and_names_the_stage() {
    let (rt, release) = wedged_runtime();
    let (spec, seen) = test_spec(Duration::from_millis(400), Duration::from_millis(50));
    let progress = ShutdownProgress::watched(spec);
    let started = Instant::now();
    let driver = {
        let rt = Arc::clone(&rt);
        let progress = progress.clone();
        std::thread::spawn(move || {
            rt.block_on(async {
                // Stage 2 only aborts tasks, so its bound is zero and any
                // real time in it is an overrun.
                progress.begin(Stage::KeepWarmAbort);
                // The stage's work waits on a timer the wedge never fires.
                tokio::time::sleep(Duration::from_millis(10)).await;
                progress.end(Stage::KeepWarmAbort);
            });
        })
    };

    match seen.recv_timeout(Duration::from_secs(5)) {
        Ok(Seen::Overrun(r)) => {
            let (stage, ran) = r.stage.expect("a stage is running");
            assert_eq!(stage, Stage::KeepWarmAbort);
            assert!(ran >= Duration::from_millis(50), "{ran:?}");
        }
        Ok(Seen::Expired(r)) => panic!("expired before reporting the overrun: {r:?}"),
        Err(e) => panic!("no overrun reported on a wedged runtime: {e}"),
    }
    match seen.recv_timeout(Duration::from_secs(5)) {
        Ok(Seen::Expired(r)) => {
            assert_eq!(r.stage.map(|(s, _)| s), Some(Stage::KeepWarmAbort));
            assert!(r.since_shutdown >= Duration::from_millis(400), "{r:?}");
        }
        Ok(Seen::Overrun(r)) => panic!("one overrun per stage, got a second: {r:?}"),
        Err(e) => panic!("the watchdog did not expire on a wedged runtime: {e}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the watchdog fires near its bound, not whenever the runtime frees up"
    );
    drop(release);
    driver.join().expect("driver thread");
}

/// A shutdown that completes stands the watchdog down: nothing fires after.
#[test]
fn a_completed_shutdown_disarms_the_watchdog() {
    let (spec, seen) = test_spec(Duration::from_millis(150), Duration::from_millis(20));
    let progress = ShutdownProgress::watched(spec);
    progress.run(Stage::BackgroundTasks, || {});
    progress.complete();
    assert!(
        seen.recv_timeout(Duration::from_millis(500)).is_err(),
        "a disarmed watchdog must never act"
    );
}

/// The guard `serve` holds disarms on drop, so an early return or an unwind
/// out of `serve` cannot leave a watchdog that later aborts the process.
#[test]
fn the_disarm_guard_stands_the_watchdog_down_on_drop() {
    let (spec, seen) = test_spec(Duration::from_millis(150), Duration::from_millis(20));
    let progress = ShutdownProgress::watched(spec);
    let guard = progress.disarm_on_drop();
    progress.begin(Stage::TransportDrain);
    drop(guard);
    assert!(
        seen.recv_timeout(Duration::from_millis(500)).is_err(),
        "a dropped guard must disarm the watchdog"
    );
}

/// A stage that runs inside its bound is not reported.
#[test]
fn a_stage_within_its_bound_is_not_reported() {
    let (spec, seen) = test_spec(Duration::from_secs(30), Duration::from_millis(20));
    let progress = ShutdownProgress::watched(spec);
    // Stage 1's bound is SHUTDOWN_GRACE (5 s); 100 ms in it is healthy.
    progress.begin(Stage::TransportDrain);
    assert!(seen.recv_timeout(Duration::from_millis(100)).is_err());
    progress.end(Stage::TransportDrain);
    progress.complete();
}

/// The two lines name the stage and the times, in the stable shape the
/// operator docs quote.
#[test]
fn the_watchdog_lines_name_the_stage() {
    let report = StallReport {
        stage: Some((Stage::SessionClose, Duration::from_millis(12_345))),
        since_shutdown: Duration::from_millis(20_001),
    };
    let overrun = overrun_line(&report);
    assert!(
        overrun.starts_with(
            "lambo serve: shutdown watchdog: stage 3/7 session_close has run 12345 ms, past its \
             10000 ms bound"
        ),
        "{overrun}"
    );
    let expiry = expiry_line(&report);
    assert!(
        expiry.starts_with(
            "lambo serve: shutdown watchdog: shutdown still running after 20001 ms, in stage 3/7 \
             session_close; aborting"
        ),
        "{expiry}"
    );
}

/// The numbers the operator guidance quotes (an `ExitTimeOut` of 30 s covers
/// the watchdog, which covers the exit budget). The build-time asserts keep
/// the order; this keeps the docs honest about the values.
#[test]
fn the_exit_budget_and_watchdog_are_the_documented_values() {
    assert_eq!(EXIT_BUDGET, Duration::from_millis(18_500));
    assert_eq!(SHUTDOWN_WATCHDOG, Duration::from_secs(20));
    assert!(SHUTDOWN_WATCHDOG < Duration::from_secs(30));
}
