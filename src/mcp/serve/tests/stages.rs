//! #40: every shutdown stage, and every step of `Memory::close` inside stage
//! 3, logs `started` and `finished in N ms`, so a shutdown that stalls names
//! the stage it stalled in.

use super::*;
use crate::embed::{Embedder, FixtureEmbedder};
use crate::memory::Memory;
use crate::store::{GraphStore, MemoryStore};
use crate::test_util::capture_logs;
use crate::types::EmbeddingContract;

async fn mem(session: &str) -> Arc<Memory> {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let m = Memory::builder()
        .session(session)
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(store)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(EmbeddingContract {
            kind: "fixture".into(),
            model: None,
            dim: 1024,
        })
        .build()
        .await
        .expect("build");
    Arc::new(m)
}

/// Index of the first line containing `needle`, failing with the whole log.
fn position(lines: &[String], needle: &str) -> usize {
    lines
        .iter()
        .position(|l| l.contains(needle))
        .unwrap_or_else(|| {
            panic!(
                "no line contains {needle:?}; log was:\n{}",
                lines.join("\n")
            )
        })
}

/// The stage names are what operators grep for; they are pinned here so a
/// rename is a deliberate, reviewed change.
#[test]
fn the_stage_names_and_numbers_are_stable() {
    let table: Vec<(u8, &str)> = [
        Stage::TransportDrain,
        Stage::KeepWarmAbort,
        Stage::SessionClose,
        Stage::EventPumpAbort,
        Stage::BackgroundTasks,
        Stage::EndpointRelease,
        Stage::LedgerClose,
    ]
    .iter()
    .map(|s| (s.number(), s.name()))
    .collect();
    assert_eq!(
        table,
        vec![
            (1, "transport_drain"),
            (2, "keep_warm_abort"),
            (3, "session_close"),
            (4, "event_pump_abort"),
            (5, "background_tasks"),
            (6, "endpoint_release"),
            (7, "ledger_close"),
        ]
    );
    assert_eq!(Stage::COUNT, 7);
}

/// `run_and_close` logs stages 1 to 4 in order, each started then finished,
/// and `Memory::close`'s steps sit inside stage 3, in their documented order.
#[tokio::test]
async fn run_and_close_logs_every_stage_and_close_step_in_order() {
    let (logs, _guard) = capture_logs(tracing::Level::INFO);
    let m = mem("serve-stages-logged").await;
    let progress = ShutdownProgress::new();
    // As the shutdown future does when a signal resolves it.
    progress.begin(Stage::TransportDrain);
    let pump = tokio::spawn(async {});
    let out = run_and_close(
        m.clone(),
        async { Ok(()) },
        pump,
        &[],
        &EarlyShutdown::unarmed(),
        &progress,
    )
    .await;
    assert!(out.is_ok(), "{out:?}");

    let lines = logs.lines();
    let mut last = 0;
    let mut expect_after = |needle: &str| {
        let at = position(&lines, needle);
        assert!(
            at >= last,
            "{needle:?} logged out of order; log was:\n{}",
            lines.join("\n")
        );
        last = at;
    };
    expect_after("shutdown stage 1/7 transport_drain started");
    expect_after("shutdown stage 1/7 transport_drain finished in ");
    expect_after("shutdown stage 2/7 keep_warm_abort started");
    expect_after("shutdown stage 2/7 keep_warm_abort finished in ");
    expect_after("shutdown stage 3/7 session_close started");
    for (n, name) in [
        (1, "serialize"),
        (2, "replay_stop"),
        (3, "queue_quiesce"),
        (4, "writers_gate"),
        (5, "heartbeat_abort"),
        (6, "producer_joins"),
        (7, "flush_join"),
        (8, "final_drain"),
    ] {
        expect_after(&format!("close: step {n}/10 {name} started"));
        expect_after(&format!("close: step {n}/10 {name} finished in "));
    }
    // A fresh session's attach leaves a tail in the log (the session
    // record), so the final flush runs; an empty tail would say `skipped`.
    expect_after("close: step 9/10 final_flush started");
    expect_after("close: step 9/10 final_flush finished in ");
    expect_after("close: step 10/10 lease_release started");
    expect_after("close: step 10/10 lease_release finished in ");
    expect_after("shutdown stage 3/7 session_close finished in ");
    expect_after("shutdown stage 4/7 event_pump_abort started");
    expect_after("shutdown stage 4/7 event_pump_abort finished in ");
    assert!(
        !logs.contains("abandoned"),
        "a close that finished abandons no step:\n{}",
        logs.contents()
    );
}

/// A transport that ends on its own (a client hangup) never had a drain, and
/// its stage 1 still gets both lines, so every shutdown reads the same way.
#[tokio::test]
async fn a_transport_that_ended_on_its_own_still_logs_stage_one() {
    let (logs, _guard) = capture_logs(tracing::Level::INFO);
    let m = mem("serve-stages-own-end").await;
    let pump = tokio::spawn(async {});
    let out = run_and_close(
        m,
        async { Ok(()) },
        pump,
        &[],
        &EarlyShutdown::unarmed(),
        &ShutdownProgress::new(),
    )
    .await;
    assert!(out.is_ok(), "{out:?}");
    let lines = logs.lines();
    let started = position(&lines, "shutdown stage 1/7 transport_drain started");
    let finished = position(&lines, "shutdown stage 1/7 transport_drain finished in ");
    assert!(started < finished);
}

/// A close dropped mid-step (here: by the operator's second signal) logs the
/// step it was abandoned in, so a timed-out close names where it stood.
#[tokio::test]
async fn an_abandoned_close_names_the_step_it_was_abandoned_in() {
    let (logs, _guard) = capture_logs(tracing::Level::INFO);
    let two = EarlyShutdown::unarmed();
    two.simulate_signal();
    two.simulate_signal();
    let m = mem("serve-stages-abandoned").await;
    let out = close_bounded_until(&m, two.second_signal()).await;
    assert!(out.is_err(), "two signals abandon the close: {out:?}");
    let lines = logs.lines();
    let abandoned = position(&lines, "abandoned after ");
    assert!(
        lines[abandoned].contains("close: step ") && lines[abandoned].contains("WARN"),
        "the abandon line names the step, at WARN: {}",
        lines[abandoned]
    );
}

/// `complete` reports the whole shutdown's time from the first stage.
#[test]
fn complete_reports_the_whole_shutdown() {
    let (logs, _guard) = capture_logs(tracing::Level::INFO);
    let progress = ShutdownProgress::new();
    progress.run(Stage::BackgroundTasks, || {});
    progress.complete();
    assert!(
        logs.contains("lambo serve: shutdown finished in "),
        "{}",
        logs.contents()
    );
}
