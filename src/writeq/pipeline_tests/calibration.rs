//! Rate calibration: the probe gate and the observed rate.

use super::*;

/// Wait for the calibration probe to publish (telemetry only since the J3
/// redesign — admission no longer awaits it, so tests that read the
/// calibration must).
async fn probe_landed(rig: &Rig) -> Calibration {
    until(|| rig.pipeline.calibration().is_some(), "the probe to land").await;
    rig.pipeline.calibration().expect("just observed Some")
}

/// **J3-R2-2, as a test.** A write that FAILS is not evidence that this
/// deployment retires work quickly. `spawn_worker` already excluded a fenced
/// refusal for exactly that reason; a failure is the same argument one step
/// further, and it is the more dangerous case — a fenced handle stops
/// writing, while a failing embedder recovers and then has to service the
/// queue its own failures inflated.
///
/// The failures here are stopword-only content, which `reject_empty_key`
/// refuses at the entry point *before* any embed: a real fast failure, at
/// ~0 ms against this rig's 100 ms successful write. Sampling one would tell
/// the bound this lane retires 10 000 items/s.
#[tokio::test]
async fn a_failed_write_is_never_sampled_into_the_observed_rate() {
    let calls = Arc::new(AtomicUsize::new(0));
    let rig = Rig::hybrid(
        "wq-fast-fail",
        Arc::new(SlowEmbedder {
            delay: Duration::from_millis(100),
            inner: FixtureEmbedder::new(),
            calls: calls.clone(),
        }),
    );
    let agent = AgentId::new("agent-a");

    // Well past OBSERVED_MIN_SAMPLES worth of failures, drained one at a
    // time so the lane's own bound never refuses one before it runs.
    for i in 0..(OBSERVED_MIN_SAMPLES * 2) {
        let submitted = rig.derive(&agent, "the and of a").await;
        assert!(!submitted.dropped(), "submission {i} was refused, not run");
        until(|| rig.pipeline.outstanding() == 0, "the failure to settle").await;
    }
    let counters = rig.pipeline.counters();
    assert_eq!(
        counters.failed(),
        OBSERVED_MIN_SAMPLES * 2,
        "every one of those writes must have failed, or the gate proves nothing"
    );
    assert_eq!(counters.applied(), 0, "nothing was applied");
    let after_failures = probe_landed(&rig).await;
    assert_eq!(
        after_failures.source,
        CalibrationSource::Probe,
        "{} fast failures must not become an observed rate: {after_failures:?}",
        counters.failed()
    );

    // And the gate is about failures, not about sampling being broken:
    // successful writes of the same count DO take over.
    for i in 0..OBSERVED_MIN_SAMPLES {
        rig.derive(&agent, &format!("a real concept {i}")).await;
        until(|| rig.pipeline.outstanding() == 0, "the write to settle").await;
    }
    let after_writes = rig.pipeline.calibration().expect("the probe has landed");
    assert_eq!(after_writes.source, CalibrationSource::Observed);
    let rate = after_writes.serial_items_per_sec.expect("observed");
    assert!(
        rate < 1000.0 / 100.0 + 1.0,
        "the observed rate must reflect the 100 ms writes and nothing faster: {rate}"
    );
}

/// An embedder that answers the calibration probe's own texts and refuses
/// everything else, fast — the shape J3-R3-1 measured at the rig: llama
/// returns HTTP 500 in ~2 ms for an input it refuses, 30× faster than a
/// write it accepts.
struct RefusingEmbedder {
    inner: FixtureEmbedder,
    refusals: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Embedder for RefusingEmbedder {
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::EmbedError> {
        if text.contains(PROBE_TEXT) {
            return self.inner.embed(text).await;
        }
        self.refusals.fetch_add(1, Ordering::Relaxed);
        Err(crate::EmbedError::Backend("HTTP 500: input refused".into()))
    }
}

/// **J3-R3-1, as a test — the door J3-R2-2's fix left open, now closed at
/// the source.** `spawn_worker`'s `if outcome.is_ok()` filter was correct
/// about what it excluded and wrong about what reached it: on the shipping
/// hybrid path an embedder refusal was not an `Err` — the concept was
/// applied with `embedding: NULL`, the caller was told an unqualified
/// success, and the ~ms non-embed was sampled as a fast write (rate
/// inflated 20–45×; 326/361 acked writes abandoned at a clean close, at the
/// release binary). The fix is upstream: `hybrid::derive` now fails the
/// write on an embed error, so the refusal arrives here as the `Err` the
/// filter always assumed.
///
/// This test drives refusals through the **whole shipping path** — hybrid
/// strategy, vector-capable store, the embed itself refused — unlike its
/// J3-R2-2 sibling above, whose failures never reach an embed.
#[tokio::test]
async fn an_embedder_refusal_fails_the_write_and_is_never_sampled() {
    let refusals = Arc::new(AtomicUsize::new(0));
    let rig = Rig::hybrid(
        "wq-refusal-honesty",
        Arc::new(RefusingEmbedder {
            inner: FixtureEmbedder::new(),
            refusals: refusals.clone(),
        }),
    );
    let agent = AgentId::new("agent-a");

    for i in 0..(OBSERVED_MIN_SAMPLES * 2) {
        let submitted = rig.derive(&agent, &format!("real concept {i}")).await;
        assert!(!submitted.dropped(), "submission {i} was refused, not run");
        until(|| rig.pipeline.outstanding() == 0, "the refusal to settle").await;
        let answer = rig.pipeline.lookup(&agent, submitted.receipt);
        let ReceiptAnswer::Failed(why) = answer else {
            panic!(
                "a refused embed must settle FAILED, not {answer:?} — an applied answer \
                     here is the applied-with-NULL-embedding dishonesty"
            );
        };
        // Asserted at `describe()`, the string a model is actually handed —
        // `why` is the payload, and since JE2E-12 the "nothing was written"
        // half comes from the variant's own rendering rather than from the
        // payload. The property is unchanged and this is where it lives.
        let rendered = ReceiptAnswer::Failed(why.clone()).describe();
        assert!(
            rendered.contains("nothing was written"),
            "the receipt must say nothing was written: {rendered}"
        );
        // **JE2E-12.** The payload is the N4 class, not the embedder's own
        // words. `EmbedError::Backend` here carries "HTTP 500: input
        // refused" — a server's response body, which on a real deployment
        // is whatever that server chose to say — and the model must not be
        // handed it, exactly as the synchronous path does not hand it over.
        assert_eq!(
            why, "embedding error (the detail was logged server-side)",
            "the receipt carries the N4 class, the same one `tool_err` renders"
        );
        assert!(
            !rendered.contains("HTTP 500") && !rendered.contains("input refused"),
            "the embedder's own message must not reach a model: {rendered}"
        );
    }
    assert!(
        refusals.load(Ordering::Relaxed) >= (OBSERVED_MIN_SAMPLES * 2) as usize,
        "every write must have reached the embedder and been refused there"
    );
    let counters = rig.pipeline.counters();
    assert_eq!(
        counters.applied(),
        0,
        "nothing may apply without its vector"
    );
    assert_eq!(counters.failed(), OBSERVED_MIN_SAMPLES * 2);

    // Applied ≠ embedded, asserted at the graph rather than at the counters:
    // no concept row exists at all, with or without a vector. (Before the
    // fix this read OBSERVED_MIN_SAMPLES * 2 rows, every one with
    // `embedding: None`.)
    assert_eq!(
        rig.graph.read().concepts().count(),
        0,
        "a refused embed must write no concept row"
    );

    // And the estimator half: the refusals never became an observed rate.
    let calibration = rig.pipeline.calibration().expect("the probe has landed");
    assert_eq!(
        calibration.source,
        CalibrationSource::Probe,
        "{} fast refusals must not flip the source to observed: {calibration:?}",
        counters.failed()
    );
}

/// **J3-R2-4, as a test.** Observation replacing the probe's serial figure
/// must not destroy it: the *ratio* between the two is the one
/// self-diagnosing fact the payload can carry, and it is what would have
/// caught J3-R2-1 in `lambo_stats` rather than at a release binary.
///
/// The workload here is deliberately **larger than
/// [`PROBE_TEXT_BYTES`]** — 8 KiB concepts, half of `MAX_CONTENT_BYTES`.
/// That is a residual no probe leg covers, so it is also the case where
/// the two figures diverge far enough to be worth publishing: the probe
/// here reads ~6x the rate the writes retire at.
#[tokio::test]
async fn the_probes_serial_figure_survives_the_observed_rate_that_replaces_it() {
    let calls = Arc::new(AtomicUsize::new(0));
    let rig = Rig::hybrid(
        "wq-probe-vs-observed",
        Arc::new(LengthProportionalEmbedder {
            per_5_bytes: Duration::from_micros(200),
            inner: FixtureEmbedder::new(),
            calls: calls.clone(),
        }),
    );
    let agent = AgentId::new("agent-a");
    let probe_rate = {
        rig.derive(&agent, "make the probe land").await;
        let c = probe_landed(&rig).await;
        assert_eq!(c.source, CalibrationSource::Probe);
        assert_eq!(
            c.probe_serial_items_per_sec, c.serial_items_per_sec,
            "while the probe is the source, the two figures are one number"
        );
        assert!(
            c.probe_optimism().is_none(),
            "there is nothing to compare the probe against yet: {c:?}"
        );
        c.serial_items_per_sec.expect("measured")
    };

    // Distinct content per write on purpose: a repeat canonicalizes onto the
    // same key, matches without embedding, and would time the wrong thing.
    for i in 0..OBSERVED_MIN_SAMPLES {
        let content = format!("{i:04} {}", "unique concept body ".repeat(410));
        rig.derive(&agent, &content).await;
        until(|| rig.pipeline.outstanding() == 0, "the write to settle").await;
    }
    let c = rig.pipeline.calibration().expect("observed by now");
    assert_eq!(c.source, CalibrationSource::Observed);
    assert_eq!(
        c.probe_serial_items_per_sec,
        Some(probe_rate),
        "the displaced figure must still be there: {c:?}"
    );
    assert!(
        c.serial_items_per_sec.expect("observed") < probe_rate,
        "8 KiB writes are slower than a 1 KiB probe embed: {c:?}"
    );
    let optimism = c.probe_optimism().expect("both figures are present");
    assert!(
        optimism > 2.0,
        "the probe over-read by {optimism:.1}x, which is exactly the fact J3-R2-1 had to be \
             measured at a release binary to learn: {c:?}"
    );
}
