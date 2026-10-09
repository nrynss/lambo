//! #32 PR 3: the write queue's calibration probe, shared process-wide
//! through an [`EmbedderCalibration`] passed to [`MemoryBuilder`].

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
    calls: AtomicUsize,
}

impl CountingEmbedder {
    /// Ungated: every embed answers at once.
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: FixtureEmbedder::new(),
            gate: None,
            calls: AtomicUsize::new(0),
        })
    }

    /// Gated: the probe's first embed answers, its second parks until
    /// [`CountingEmbedder::release`].
    fn gated() -> Arc<Self> {
        Arc::new(Self {
            inner: FixtureEmbedder::new(),
            gate: Some(tokio::sync::Semaphore::new(1)),
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
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
