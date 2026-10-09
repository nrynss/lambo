//! #32 PR 3: the write queue's calibration probe, shared process-wide
//! through an [`EmbedderCalibration`] passed to [`MemoryBuilder`].

use super::attach::strip_ansi;
use super::*;
use crate::writeq::EmbedderCalibration;

/// Counts every embed, and optionally gates every embed after the first, so
/// a probe that runs is visible and a probe can be held mid-flight for as
/// long as a test needs (no wall-clock delay to outrun, P3-5).
#[derive(Debug)]
struct CountingEmbedder {
    inner: FixtureEmbedder,
    /// One permit: the first embed passes, every later one parks here until
    /// [`CountingEmbedder::release`] closes the semaphore (a closed
    /// semaphore's `acquire` fails at once, which is the open gate).
    gate: Option<tokio::sync::Semaphore>,
    /// While set, every embed fails (after it is counted, before the gate),
    /// so a probe ends unmeasured.
    failing: std::sync::atomic::AtomicBool,
    calls: AtomicUsize,
}

impl CountingEmbedder {
    /// Ungated: every embed answers at once.
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: FixtureEmbedder::new(),
            gate: None,
            failing: Default::default(),
            calls: AtomicUsize::new(0),
        })
    }

    /// Gated: the probe's first embed answers, its second parks until
    /// [`CountingEmbedder::release`].
    fn gated() -> Arc<Self> {
        Arc::new(Self {
            inner: FixtureEmbedder::new(),
            gate: Some(tokio::sync::Semaphore::new(1)),
            failing: Default::default(),
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Make every embed fail (`true`) or answer again (`false`).
    fn fail(&self, failing: bool) {
        self.failing.store(failing, Ordering::SeqCst);
    }

    /// Open the gate for good: every parked and later embed answers.
    fn release(&self) {
        if let Some(gate) = &self.gate {
            gate.close();
        }
    }

    /// Wait until an embed is parked on the gate (the second call has been
    /// made), so the probe is running by construction, not by timing.
    async fn parked(&self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while self.calls() < 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "the probe never reached the gate"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
}

#[async_trait]
impl Embedder for CountingEmbedder {
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.failing.load(Ordering::SeqCst) {
            return Err(crate::embed::EmbedError::Unavailable(
                "the test's embedder is down".into(),
            ));
        }
        if let Some(gate) = &self.gate
            && let Ok(permit) = gate.acquire().await
        {
            permit.forget();
        }
        self.inner.embed(text).await
    }
}

async fn open(
    session: &str,
    embedder: Arc<dyn Embedder>,
    calibration: Option<&EmbedderCalibration>,
) -> Memory {
    let mut builder = Memory::builder()
        .session(session)
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::new(MemoryStore::new()) as Arc<dyn GraphStore>)
        .embedder(embedder)
        .embedding_contract(contract("fixture", 1024));
    if let Some(calibration) = calibration {
        builder = builder.calibration(calibration.clone());
    }
    builder.build().await.expect("build")
}

async fn probe_landed(mem: &Memory) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while mem.pipeline().calibration().is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "the probe never landed"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// Let anything a build spawned, or a gate released, run: a probe's next
/// embed is its next await, so a few milliseconds of real time is ample for
/// one to show. Only the "nothing more happened" checks use it; every "still
/// running" precondition is held by a gate instead.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(100)).await;
}

/// **The PR 3 acceptance.** A second build in one process, over the same
/// embedder and the same calibration, fires zero probe embeds: it reads the
/// probe the first build spawned. The control (a third build with no
/// calibration) shows the counter would have seen a probe.
#[tokio::test]
async fn a_second_build_with_a_shared_calibration_fires_no_probe_embed() {
    let counting = CountingEmbedder::new();
    let embedder: Arc<dyn Embedder> = counting.clone();
    let calibration = EmbedderCalibration::new();

    let a = open("cal-shared-a", Arc::clone(&embedder), Some(&calibration)).await;
    probe_landed(&a).await;
    let first_probe = counting.calls();
    assert!(first_probe > 0, "the first build probed the embedder");

    let b = open("cal-shared-b", Arc::clone(&embedder), Some(&calibration)).await;
    settle().await;
    assert_eq!(
        counting.calls(),
        first_probe,
        "the second build must fire no probe embed"
    );
    assert_eq!(calibration.probes(), 1, "one probe for one embedder");
    assert_eq!(
        b.pipeline().calibration(),
        a.pipeline().calibration(),
        "both sessions read the one probe"
    );

    // Control: without the calibration a build probes for itself.
    let c = open("cal-own-c", Arc::clone(&embedder), None).await;
    probe_landed(&c).await;
    assert!(
        counting.calls() > first_probe,
        "a build with no calibration spawns its own probe"
    );

    for mem in [a, b, c] {
        mem.close().await.expect("close");
    }
}

/// A build that attaches while the shared probe is still running joins it
/// rather than starting a second one.
#[tokio::test]
async fn a_build_during_the_shared_probe_joins_it() {
    let counting = CountingEmbedder::gated();
    let embedder: Arc<dyn Embedder> = counting.clone();
    let calibration = EmbedderCalibration::new();

    let a = open("cal-join-a", Arc::clone(&embedder), Some(&calibration)).await;
    counting.parked().await;
    let b = open("cal-join-b", Arc::clone(&embedder), Some(&calibration)).await;
    assert!(
        a.pipeline().calibration().is_none(),
        "the probe is held at the gate, so b attached during it"
    );
    counting.release();
    probe_landed(&a).await;
    probe_landed(&b).await;
    assert_eq!(calibration.probes(), 1);
    assert_eq!(
        counting.calls(),
        crate::writeq::PROBE_EMBEDS,
        "exactly one probe's embeds"
    );
    for mem in [a, b] {
        mem.close().await.expect("close");
    }
}

/// One session closing must not abort a probe other sessions read (#11's
/// note for PR 3): the abort moved from the pipeline to the calibration's
/// owner.
#[tokio::test]
async fn one_sessions_close_leaves_the_shared_probe_running() {
    let counting = CountingEmbedder::gated();
    let embedder: Arc<dyn Embedder> = counting.clone();
    let calibration = EmbedderCalibration::new();

    let a = open("cal-close-a", Arc::clone(&embedder), Some(&calibration)).await;
    let b = open("cal-close-b", Arc::clone(&embedder), Some(&calibration)).await;
    counting.parked().await;
    a.close().await.expect("close a");
    drop(a);
    assert!(
        b.pipeline().calibration().is_none(),
        "the probe is held at the gate when the first session closes"
    );
    counting.release();
    probe_landed(&b).await;
    assert!(
        b.pipeline().calibration().expect("landed").measured(),
        "the probe ran to completion"
    );
    b.close().await.expect("close b");
}

/// Without a calibration the pipeline still owns its probe, and its close
/// aborts it, as before PR 3.
#[tokio::test]
async fn an_own_probe_is_still_aborted_by_its_sessions_close() {
    let counting = CountingEmbedder::gated();
    let a = open("cal-own-close", counting.clone(), None).await;
    counting.parked().await;
    a.close().await.expect("close");
    let at_close = counting.calls();
    counting.release();
    settle().await;
    assert_eq!(counting.calls(), at_close, "the probe stopped at close");
    assert!(a.pipeline().calibration().is_none());
}

/// The owner aborts: `EmbedderCalibration::abort` stops a running shared
/// probe, and every session reading it then has no probe figure.
#[tokio::test]
async fn the_calibrations_owner_aborts_the_shared_probe() {
    let counting = CountingEmbedder::gated();
    let embedder: Arc<dyn Embedder> = counting.clone();
    let calibration = EmbedderCalibration::new();

    let a = open("cal-abort-a", Arc::clone(&embedder), Some(&calibration)).await;
    let b = open("cal-abort-b", Arc::clone(&embedder), Some(&calibration)).await;
    counting.parked().await;
    calibration.abort();
    let at_abort = counting.calls();
    counting.release();
    settle().await;
    assert_eq!(counting.calls(), at_abort, "the probe stopped");
    assert!(a.pipeline().calibration().is_none());
    assert!(b.pipeline().calibration().is_none());
    // Idempotent.
    calibration.abort();
    for mem in [a, b] {
        mem.close().await.expect("close");
    }
}

/// The probe outlives its owner's handle while any session holds it (a
/// library caller that passes `EmbedderCalibration::new()` inline keeps no
/// handle of its own), and stops once the last holder is gone.
#[tokio::test]
async fn the_shared_probe_lives_while_any_holder_does() {
    let ungated = CountingEmbedder::new();
    let a = open(
        "cal-holders-a",
        ungated.clone(),
        Some(&EmbedderCalibration::new()),
    )
    .await;
    probe_landed(&a).await;
    a.close().await.expect("close");
    drop(a);

    // And the last holder's drop aborts a probe still running.
    let counting = CountingEmbedder::gated();
    let calibration = EmbedderCalibration::new();
    let b = open("cal-holders-b", counting.clone(), Some(&calibration)).await;
    counting.parked().await;
    drop(calibration);
    drop(b);
    let at_drop = counting.calls();
    counting.release();
    settle().await;
    assert_eq!(counting.calls(), at_drop, "the probe stopped at the drop");
}

/// Keyed by embedder: two embedders behind one calibration get a probe
/// each, so one embedder's figure is never reported for the other.
#[tokio::test]
async fn one_calibration_keeps_a_probe_per_embedder() {
    let first = CountingEmbedder::new();
    let second = CountingEmbedder::new();
    let calibration = EmbedderCalibration::new();

    let a = open("cal-two-a", first.clone(), Some(&calibration)).await;
    let b = open("cal-two-b", second.clone(), Some(&calibration)).await;
    probe_landed(&a).await;
    probe_landed(&b).await;
    assert_eq!(calibration.probes(), 2);
    assert_eq!(first.calls(), crate::writeq::PROBE_EMBEDS);
    assert_eq!(second.calls(), crate::writeq::PROBE_EMBEDS);
    for mem in [a, b] {
        mem.close().await.expect("close");
    }
}

/// Review P3-4: the probe's log line carries `scope` as one bare word and
/// the session as its own field, so a `key=value` parser reads both whole.
/// An owned probe is `scope=session`; a shared one is `scope=process` and
/// still names the session whose attach started it (one-session `lambo
/// serve` keeps the `session=` it logged before PR 3).
#[tokio::test]
async fn the_probe_line_names_its_scope_and_session_as_plain_fields() {
    let (logs, _guard) = capture_logs(tracing::Level::INFO);
    let own = open("cal-log-own", CountingEmbedder::new(), None).await;
    probe_landed(&own).await;
    let calibration = EmbedderCalibration::new();
    let shared = open(
        "cal-log-shared",
        CountingEmbedder::new(),
        Some(&calibration),
    )
    .await;
    probe_landed(&shared).await;

    let logged = strip_ansi(&logs.contents());
    let probe_line = |session: &str| {
        logged
            .lines()
            .find(|l| l.contains("write queue: bounds are static") && l.contains(session))
            .unwrap_or_else(|| panic!("no probe line for {session}: {logged}"))
            .to_string()
    };
    let own_line = probe_line("cal-log-own");
    assert!(
        own_line.contains("scope=session session=cal-log-own "),
        "{own_line}"
    );
    let shared_line = probe_line("cal-log-shared");
    assert!(
        shared_line.contains("scope=process session=cal-log-shared "),
        "{shared_line}"
    );
    for mem in [own, shared] {
        mem.close().await.expect("close");
    }
}

/// Wait until `mem`'s probe figure is a measurement (a re-probe replacing an
/// unmeasured one has landed).
async fn measured(mem: &Memory) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !mem.pipeline().calibration().is_some_and(|c| c.measured()) {
        assert!(
            std::time::Instant::now() < deadline,
            "no measured probe landed"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// Review P3-3: a shared probe that failed is not terminal. The next attach
/// over the embedder probes it again once the backoff has passed, and the
/// session that attached first sees the new figure too.
#[tokio::test]
async fn a_failed_shared_probe_is_reprobed_by_the_next_attach() {
    let counting = CountingEmbedder::new();
    let embedder: Arc<dyn Embedder> = counting.clone();
    let calibration = EmbedderCalibration::with_retry_backoff(Duration::ZERO);

    counting.fail(true);
    let a = open("cal-retry-a", Arc::clone(&embedder), Some(&calibration)).await;
    probe_landed(&a).await;
    assert!(!a.pipeline().calibration().expect("landed").measured());

    counting.fail(false);
    let b = open("cal-retry-b", Arc::clone(&embedder), Some(&calibration)).await;
    measured(&b).await;
    measured(&a).await;
    assert_eq!(calibration.probes(), 1, "still one entry for one embedder");

    // A measured probe is never repeated, backoff or not.
    let measured_calls = counting.calls();
    let c = open("cal-retry-c", Arc::clone(&embedder), Some(&calibration)).await;
    settle().await;
    assert_eq!(
        counting.calls(),
        measured_calls,
        "no probe after a measurement"
    );
    for mem in [a, b, c] {
        mem.close().await.expect("close");
    }
}

/// The backoff: an embedder that is down is not probed again at every
/// attach. Inside [`crate::writeq::PROBE_RETRY_BACKOFF`] of the failed
/// probe, a new attach fires no probe embed.
#[tokio::test]
async fn a_failed_shared_probe_is_not_reprobed_inside_the_backoff() {
    let counting = CountingEmbedder::new();
    let embedder: Arc<dyn Embedder> = counting.clone();
    let calibration = EmbedderCalibration::new();

    counting.fail(true);
    let a = open("cal-backoff-a", Arc::clone(&embedder), Some(&calibration)).await;
    probe_landed(&a).await;
    let after_failure = counting.calls();

    counting.fail(false);
    let b = open("cal-backoff-b", Arc::clone(&embedder), Some(&calibration)).await;
    settle().await;
    assert_eq!(
        counting.calls(),
        after_failure,
        "no re-probe inside the backoff"
    );
    assert!(!b
        .pipeline()
        .calibration()
        .expect("the failed figure")
        .measured());
    for mem in [a, b] {
        mem.close().await.expect("close");
    }
}

/// An aborted shared probe (a library caller's `abort()`, say) is probed
/// again by the next attach, and only one re-probe runs at a time: an
/// attach while the re-probe is in flight joins it.
#[tokio::test]
async fn an_aborted_shared_probe_is_reprobed_once_by_the_next_attaches() {
    let counting = CountingEmbedder::gated();
    let embedder: Arc<dyn Embedder> = counting.clone();
    let calibration = EmbedderCalibration::with_retry_backoff(Duration::ZERO);

    let a = open("cal-reabort-a", Arc::clone(&embedder), Some(&calibration)).await;
    counting.parked().await;
    calibration.abort();
    let at_abort = counting.calls();
    assert!(a.pipeline().calibration().is_none());

    // The re-probe's embeds pass the (one-permit) gate only once released,
    // so it is held in flight while a third session attaches.
    let b = open("cal-reabort-b", Arc::clone(&embedder), Some(&calibration)).await;
    let c = open("cal-reabort-c", Arc::clone(&embedder), Some(&calibration)).await;
    counting.release();
    measured(&b).await;
    measured(&a).await;
    measured(&c).await;
    assert_eq!(
        counting.calls(),
        at_abort + crate::writeq::PROBE_EMBEDS,
        "exactly one re-probe's embeds"
    );
    for mem in [a, b, c] {
        mem.close().await.expect("close");
    }
}

/// Review L1: `shutdown` (what `lambo serve` calls at stages 2 and 5) is
/// final, unlike `abort`. An attach still in flight afterwards starts no
/// probe: not a re-probe of the aborted one, and not a first probe of a new
/// embedder.
#[tokio::test]
async fn a_shut_down_calibration_starts_no_probe() {
    let counting = CountingEmbedder::gated();
    let embedder: Arc<dyn Embedder> = counting.clone();
    let calibration = EmbedderCalibration::with_retry_backoff(Duration::ZERO);

    let a = open("cal-shut-a", Arc::clone(&embedder), Some(&calibration)).await;
    counting.parked().await;
    calibration.shutdown();
    let at_shutdown = counting.calls();
    counting.release();

    let b = open("cal-shut-b", Arc::clone(&embedder), Some(&calibration)).await;
    let other = CountingEmbedder::new();
    let c = open("cal-shut-c", other.clone(), Some(&calibration)).await;
    settle().await;
    assert_eq!(
        counting.calls(),
        at_shutdown,
        "the aborted probe was re-run"
    );
    assert_eq!(other.calls(), 0, "a new embedder was probed after shutdown");
    assert!(c.pipeline().calibration().is_none());
    for mem in [a, b, c] {
        mem.close().await.expect("close");
    }
}

/// Review L2: the re-probe backoff counts from when the last probe ended,
/// not from when it started, so a probe that ran longer than the backoff
/// before failing is not re-run the moment it ends.
#[tokio::test]
async fn the_reprobe_backoff_counts_from_the_probes_end() {
    let backoff = Duration::from_millis(300);
    let counting = CountingEmbedder::gated();
    let embedder: Arc<dyn Embedder> = counting.clone();
    let calibration = EmbedderCalibration::with_retry_backoff(backoff);

    let a = open("cal-end-a", Arc::clone(&embedder), Some(&calibration)).await;
    counting.parked().await;
    // Held past the backoff, then failed: the probe ends unmeasured now.
    tokio::time::sleep(backoff * 2).await;
    counting.fail(true);
    counting.release();
    probe_landed(&a).await;
    let at_end = counting.calls();

    let b = open("cal-end-b", Arc::clone(&embedder), Some(&calibration)).await;
    settle().await;
    assert_eq!(
        counting.calls(),
        at_end,
        "re-probed inside the backoff from its end"
    );

    tokio::time::sleep(backoff).await;
    counting.fail(false);
    let c = open("cal-end-c", Arc::clone(&embedder), Some(&calibration)).await;
    measured(&c).await;
    for mem in [a, b, c] {
        mem.close().await.expect("close");
    }
}
