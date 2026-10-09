//! Bounds and rate calibration: the probe, the observed rate and the
//! constants.

use super::*;

/// **The J3 redesign's central property, pinned at the type**: the bounds
/// are the static fairness/memory caps for EVERY source, and no rate —
/// however fast, slow, zero or absent — can move them. Three rounds of
/// P1s were rates moving these bounds (width, warmth, length, failure
/// shape, concurrency scaling); this test is what makes a sixth axis
/// structurally impossible rather than merely unlikely.
#[test]
fn no_rate_can_move_the_bounds() {
    let cases = [
        // An instant embedder (the FixtureEmbedder case).
        Calibration::from_probe(Duration::from_nanos(1), None, Duration::from_nanos(1)),
        // A zero wall time must not divide by zero either.
        Calibration::from_probe(Duration::ZERO, None, Duration::ZERO),
        // A very slow embedder.
        Calibration::from_probe(Duration::from_secs(600), None, Duration::from_secs(600)),
        // The phase doc's own figures.
        Calibration::from_probe(Duration::from_millis(95), None, Duration::from_millis(64)),
        // No measurement at all.
        Calibration::unmeasured(),
        // An observed rate, absurd in either direction.
        Calibration::unmeasured().with_observed_serial(1_000_000.0),
        Calibration::unmeasured().with_observed_serial(0.001),
    ];
    for c in cases {
        assert_eq!(c.lane_bound, WRITE_QUEUE_LANE_MAX, "{c:?}");
        assert_eq!(c.bound, WRITE_QUEUE_MAX, "{c:?}");
    }

    // The RAW rates still survive as telemetry — an operator reading
    // items_per_sec beside the static bound is how "this deployment is
    // slow" stays observable now that it is no longer load-bearing.
    let fast = Calibration::from_probe(Duration::from_nanos(1), None, Duration::from_nanos(1));
    assert!(fast.measured());
    assert_eq!(fast.source, CalibrationSource::Probe);
    assert!(fast.items_per_sec.expect("measured") > 0.0);
    assert!(fast.serial_items_per_sec.expect("measured") > 0.0);

    // The unmeasured fallback says so, which is what lambo_stats reports.
    let none = Calibration::unmeasured();
    assert!(!none.measured());
    assert_eq!(none.source.tag(), "unmeasured");
    assert!(none.items_per_sec.is_none() && none.serial_items_per_sec.is_none());
    assert!(none.probe_serial_items_per_sec.is_none());
    assert!(
        none.probe_optimism().is_none(),
        "there is no pair to compare when nothing measured either side"
    );
}

/// **A reported rate never exceeds [`PROBE_CLAMP_RPS`]** (#11). The clamp's
/// own docstring says `rate_of` and `ObservedRate::items_per_sec` reach for
/// it "when a wall time reads zero or absurd", so that a clamped reading
/// always means "this measurement is not real". Both only handled exactly
/// zero: a `FixtureEmbedder` probe measured in microseconds published
/// ~200 000 items/s in `lambo_stats`, a number no deployment produced.
#[test]
fn a_reported_rate_never_exceeds_the_sanitization_clamp() {
    let clamp = PROBE_CLAMP_RPS as f64;
    for wall in [
        Duration::ZERO,
        Duration::from_nanos(1),
        Duration::from_micros(5),
        Duration::from_micros(900),
    ] {
        let rate = crate::writeq::calibration::rate_of(1, wall);
        assert!(rate <= clamp, "rate_of(1, {wall:?}) = {rate}");
        let c = Calibration::from_probe(wall, Some(wall), wall);
        assert!(c.serial_items_per_sec.expect("measured") <= clamp, "{c:?}");
        assert!(c.items_per_sec.expect("measured") <= clamp, "{c:?}");
    }
    // A real reading below the clamp is untouched.
    let real = crate::writeq::calibration::rate_of(1, Duration::from_millis(100));
    assert!((real - 10.0).abs() < 1e-9, "{real}");

    let mut observed = ObservedRate::default();
    for _ in 0..OBSERVED_MIN_SAMPLES {
        observed.sample(0.000_005);
    }
    let rate = observed.items_per_sec().expect("enough samples");
    assert!(rate <= clamp, "observed {rate}");
    assert!(
        Calibration::unmeasured()
            .with_observed_serial(1_000_000.0)
            .serial_items_per_sec
            .expect("observed")
            <= clamp,
        "an observed figure handed in from outside is sanitized too"
    );
}

/// The observed rate replaces the probe's serial figure, and only after
/// [`OBSERVED_MIN_SAMPLES`] — the J3-R1-2 remediation.
#[test]
fn an_observed_rate_replaces_the_probes_serial_figure_after_enough_samples() {
    let mut observed = ObservedRate::default();
    for _ in 0..(OBSERVED_MIN_SAMPLES - 1) {
        observed.sample(0.1);
        assert!(
            observed.items_per_sec().is_none(),
            "the probe's figure must stand until {OBSERVED_MIN_SAMPLES} writes are in"
        );
    }
    observed.sample(0.1);
    let rate = observed.items_per_sec().expect("enough samples");
    assert!((rate - 10.0).abs() < 0.001, "{rate}");

    // The published serial figure flips to the observed one; the probe's
    // survives beside it for the comparison (J3-R2-4). Telemetry only —
    // the bounds do not move (no_rate_can_move_the_bounds).
    let hot = Calibration::from_probe(Duration::from_millis(7), None, Duration::from_millis(7));
    let corrected = hot.with_observed_serial(rate);
    assert_eq!(corrected.source, CalibrationSource::Observed);
    assert!(corrected.measured());
    assert!((corrected.serial_items_per_sec.expect("observed") - rate).abs() < 0.001);
    assert_eq!(
        corrected.probe_serial_items_per_sec, hot.serial_items_per_sec,
        "the probe's figure survives the takeover — the ratio is the diagnosis"
    );
    assert_eq!(
        corrected.items_per_sec, hot.items_per_sec,
        "the concurrent leg is not re-measured by observation, so it survives unchanged"
    );

    // A degrading embedder moves the average within about one probe's
    // width of samples, in the direction that shows in probe_optimism.
    for _ in 0..8 {
        observed.sample(1.0);
    }
    let degraded = observed.items_per_sec().expect("samples");
    assert!(degraded < 2.0, "{degraded}");
    let optimism = hot
        .with_observed_serial(degraded)
        .probe_optimism()
        .expect("both figures exist");
    assert!(
        optimism > 100.0,
        "a 7 ms probe against ≈1.1 items/s real work reads two orders optimistic: {optimism}"
    );

    // An unmeasured probe still earns a measured serial figure from
    // observation.
    let recovered = Calibration::unmeasured().with_observed_serial(rate);
    assert!(recovered.measured());
    assert!(
        recovered.probe_optimism().is_none(),
        "no probe figure, no comparison — never an invented baseline"
    );
}

/// The derivations in this module's constants, asserted rather than
/// asserted-in-prose. The `const _: () = assert!` guards cover the
/// relationships; these cover the arithmetic a reader would have to redo.
#[test]
fn the_constants_say_what_their_docs_say() {
    assert_eq!(WRITE_QUEUE_MAX, MAX_RETAINED_RECEIPTS / 4);
    assert_eq!(WRITE_QUEUE_MAX, 1024);
    assert_eq!(MAX_RETAINED_RECEIPTS, 4096);
    // The J3 redesign's two static bounds: a memory cap (above) and a
    // per-agent fair share of it — 1/16, where 16 is the declared
    // multi-caller design point the receipt-wait cap already carries.
    assert_eq!(
        WRITE_QUEUE_LANE_MAX,
        WRITE_QUEUE_MAX / MAX_CONCURRENT_RECEIPT_WAITS
    );
    assert_eq!(WRITE_QUEUE_LANE_MAX, 64);
    // The telemetry sanitization clamp: one full queue per second,
    // comfortably above the fastest real embedder measured (141 items/s
    // 4-wide) while still finite for a fixture that does no work.
    // J3 round-1 N4: the clamp is its own number now. It happens to equal
    // WRITE_QUEUE_MAX and must NOT be *defined* from it — that coupling put
    // a measured embedder rate (via the const_assert beside the constant,
    // which still guards telemetry hygiene) in the chain that sized both
    // bounds. Asserted as a literal on purpose: writing
    // `WRITE_QUEUE_MAX as u64` here would re-create the coupling in the test
    // that exists to forbid it.
    assert_eq!(PROBE_CLAMP_RPS, 1024);
    assert_eq!(MEASURED_LOCAL_EMBEDDER_RPS, 141);
    // #11: warm-up, short, then 1 + PROBE_CONCURRENCY representative writes
    // of PROBE_WRITE_CONCEPTS embeds each.
    assert_eq!(PROBE_EMBEDS, 12);
    assert_eq!(
        PROBE_EMBEDS,
        PROBE_WARMUP_EMBEDS + 1 + (1 + PROBE_CONCURRENCY) * PROBE_WRITE_CONCEPTS
    );
    assert_eq!(PROBE_WRITE_CONCEPTS, 2);
    assert_eq!(PROBE_CONCEPT_BYTES, 339);
    assert_eq!(PROBE_WARMUP_BUDGET, crate::graph::hybrid::HYBRID_IO_TIMEOUT);
    // The representative leg is bigger than the short one, and the helper
    // hits the size exactly — the two facts that make the pair a
    // measurement of length rather than of two arbitrary strings.
    assert_eq!(PROBE_TEXT_BYTES, 1024);
    assert_eq!(PROBE_TEXT.len(), 35);
    assert_eq!(probe_text_at(PROBE_TEXT_BYTES).len(), PROBE_TEXT_BYTES);
    assert!(probe_text_at(PROBE_TEXT_BYTES).starts_with(PROBE_TEXT));
    assert!(PROBE_TEXT.is_ascii());
    assert_eq!(OBSERVED_MIN_SAMPLES, PROBE_CONCURRENCY as u64);
    assert_eq!(OBSERVED_EWMA_WEIGHT, PROBE_CONCURRENCY as u32);
    assert_eq!(MEASURED_WORST_FLUSH_LAG_SECS, 227);
    assert_eq!(WRITE_QUEUE_MAX_BYTES, 16 * 1024 * 1024);
    assert_eq!(MAX_RECEIPT_IDS, MAX_CONCEPTS_PER_DERIVE);
    // Above the worst flush_lag measured on the rig, which is the
    // applied-but-not-durable window a receipt has to outlive — and the
    // window now starts at the SETTLE, which is what makes that the right
    // comparison (J3-R1-3).
    assert!(RECEIPT_RETENTION.as_secs() > MEASURED_WORST_FLUSH_LAG_SECS);
    // The quiesce cannot be why a close() misses the deadline serve gives
    // it. Duplicated from the const assert on purpose: the build guard
    // proves the relation, this proves the numbers a reader is quoted.
    assert_eq!(WRITE_QUEUE_DRAIN_BUDGET.as_secs(), 2);
    assert_eq!(crate::mcp::serve::CLOSE_FLUSH_GRACE.as_secs(), 8);
    // #11: one write's own I/O bound plus the two drain budgets.
    assert_eq!(RECEIPT_WAIT_MAX.as_secs(), 34);
    assert_eq!(
        RECEIPT_WAIT_MAX,
        crate::graph::hybrid::HYBRID_IO_TIMEOUT + 2 * WRITE_QUEUE_DRAIN_BUDGET
    );
    assert_eq!(MAX_CONCURRENT_RECEIPT_WAITS * 2, 32);
    assert_eq!(crate::mcp::proxy::INFLIGHT_DEPTH_WARN, 64);
}

// -----------------------------------------------------------------------
// probe_embedder — J3-R2-7: the function that produces the load-bearing
// number had no test at all. These four cover its budget, its three
// required legs, and the one optional leg J3-R2-1 added.
// -----------------------------------------------------------------------

/// An embedder scripted per call so a probe leg can be made to hang, fail
/// or take a chosen wall time. `plan` is consulted by call index; anything
/// past its end behaves like the last entry.
struct ScriptedEmbedder {
    plan: Vec<Leg>,
    calls: AtomicU64,
}

#[derive(Clone, Copy)]
enum Leg {
    /// Answers after `.0` of simulated time.
    After(Duration),
    /// Answers after a wall time proportional to the input's length —
    /// one millisecond per byte — which is the shape a transformer has and
    /// the shape `PROBE_TEXT`'s old docstring denied (J3-R2-1).
    PerByte,
    /// Never answers.
    Hang,
    /// Refuses, the way this rig's llama-server refuses an input over its
    /// configured batch (HTTP 500).
    Refuse,
}

#[async_trait::async_trait]
impl Embedder for ScriptedEmbedder {
    fn dimensions(&self) -> usize {
        8
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::EmbedError> {
        let i = self.calls.fetch_add(1, Ordering::Relaxed) as usize;
        let leg = self.plan[i.min(self.plan.len() - 1)];
        match leg {
            Leg::After(d) => {
                tokio::time::sleep(d).await;
                Ok(vec![0.0; 8])
            }
            Leg::PerByte => {
                tokio::time::sleep(Duration::from_millis(text.len() as u64)).await;
                Ok(vec![0.0; 8])
            }
            Leg::Hang => std::future::pending().await,
            Leg::Refuse => Err(crate::EmbedError::Backend(format!(
                "input of {} bytes refused",
                text.len()
            ))),
        }
    }
}

fn scripted(plan: Vec<Leg>) -> ScriptedEmbedder {
    ScriptedEmbedder {
        plan,
        calls: AtomicU64::new(0),
    }
}

/// **The probe's serial figure is the slower of its two input sizes**
/// (J3-R2-1). Both legs embed the same words; only the length differs, so
/// the gap between them *is* the length sensitivity, and a projection wants
/// the conservative end of it.
#[tokio::test(start_paused = true)]
async fn the_probes_serial_figure_is_the_slower_of_its_two_input_sizes() {
    let embedder = scripted(vec![Leg::PerByte]);
    let c = probe_embedder(&embedder).await;
    assert_eq!(c.source, CalibrationSource::Probe);
    // 35 bytes → 35 ms → 28.57 items/s. The representative write embeds
    // PROBE_WRITE_CONCEPTS contexts of just under PROBE_TEXT_BYTES each, so
    // about 2 s → ~0.49 writes/s (#11: a write, not one embed). The
    // published rate must be the second one; before J3-R2-1 it was the
    // first, and the first is a rate for 35-byte writes.
    let write_bytes: usize = probe_write_contexts().iter().map(String::len).sum();
    let rate = c.serial_items_per_sec.expect("measured");
    assert!(
        (rate - 1000.0 / write_bytes as f64).abs() < 0.01,
        "the representative write must decide the rate, not the short leg: {rate}"
    );
    assert_eq!(c.probe_serial_items_per_sec, c.serial_items_per_sec);
    assert_eq!(
        c.lane_bound, WRITE_QUEUE_LANE_MAX,
        "the bound is the static fair share whatever the probe read"
    );
}

/// **A refused representative leg costs the probe nothing but the leg**
/// (J3-R2-1). Measured, not hypothesised: this rig's llama-server answers
/// 1280 B and returns HTTP 500 at 1536 B, so a probe that failed outright
/// on the larger input would land on `unmeasured` for an ordinary local
/// setup — trading an optimistic number for no number.
#[tokio::test(start_paused = true)]
async fn a_refused_representative_leg_falls_back_to_the_short_one() {
    // warm-up, short serial, then a refusal for the representative leg,
    // then the concurrent leg answers again.
    let embedder = scripted(vec![
        Leg::After(Duration::from_millis(35)),
        Leg::After(Duration::from_millis(35)),
        Leg::Refuse,
        Leg::After(Duration::from_millis(35)),
    ]);
    let c = probe_embedder(&embedder).await;
    assert_eq!(c.source, CalibrationSource::Probe, "still measured");
    let rate = c.serial_items_per_sec.expect("measured");
    assert!(
        (rate - 1000.0 / 35.0).abs() < 0.1,
        "the short leg's own figure must stand when the larger input is refused: {rate}"
    );
}

/// **A representative leg that HANGS cannot starve the required leg after
/// it** — it is bounded by half the remaining budget, so the concurrent leg
/// still has the other half and the probe still publishes a number.
#[tokio::test(start_paused = true)]
async fn a_hanging_representative_leg_leaves_the_concurrent_leg_its_budget() {
    let embedder = scripted(vec![
        Leg::After(Duration::from_millis(35)),
        Leg::After(Duration::from_millis(35)),
        Leg::Hang,
        Leg::After(Duration::from_millis(35)),
    ]);
    let started = tokio::time::Instant::now();
    let c = probe_embedder(&embedder).await;
    assert_eq!(
        c.source,
        CalibrationSource::Probe,
        "a hang in the OPTIONAL leg must not cost the probe its measurement"
    );
    assert!(
        started.elapsed() < PROBE_BUDGET,
        "and it must not cost the whole budget either: {:?}",
        started.elapsed()
    );
}

/// **`PROBE_BUDGET` bounds all [`PROBE_EMBEDS`] together, and a probe that
/// cannot measure says so.** The docstring's claim is the strong one — one
/// deadline, not one per leg — and nothing tested it (J3-R2-7). Asserted at
/// each required leg in turn, since the budget has to hold at the last leg
/// as much as at the first.
#[tokio::test(start_paused = true)]
async fn a_probe_that_cannot_finish_inside_its_budget_reports_no_measurement() {
    let answer = Leg::After(Duration::from_millis(1));
    for (leg, plan) in [
        ("warm-up", vec![Leg::Hang]),
        ("serial", vec![answer, Leg::Hang]),
    ] {
        let embedder = scripted(plan);
        let started = tokio::time::Instant::now();
        let c = probe_embedder(&embedder).await;
        let elapsed = started.elapsed();
        assert_eq!(
            c.source,
            CalibrationSource::Unmeasured,
            "a probe whose {leg} leg never answers must not invent a number"
        );
        assert!(!c.measured(), "{leg}");
        assert_eq!(c.lane_bound, WRITE_QUEUE_LANE_MAX, "{leg}");
        assert_eq!(c.bound, WRITE_QUEUE_MAX, "{leg}");
        // The warm-up has its own bound (#11); the timed legs share one,
        // which starts after a warm-up that here took 1 ms.
        let bound = if leg == "warm-up" {
            PROBE_WARMUP_BUDGET
        } else {
            PROBE_BUDGET
        };
        assert!(
            elapsed <= bound + Duration::from_millis(50),
            "the budget covers the timed embeds TOGETHER, so a hang at the {leg} leg must \
                 still end inside it: {elapsed:?}"
        );
    }

    // A hang at the concurrent leg still ends inside the budget, but both
    // serial legs have landed by then, so their figures are published and
    // only the concurrent one is missing (#11 review P2-2).
    let mut plan = vec![answer; 2 + PROBE_WRITE_CONCEPTS];
    plan.push(Leg::Hang);
    let embedder = scripted(plan);
    let started = tokio::time::Instant::now();
    let c = probe_embedder(&embedder).await;
    assert!(
        started.elapsed() <= PROBE_BUDGET + Duration::from_millis(50),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(c.source, CalibrationSource::Probe, "{c:?}");
    assert!(c.serial_items_per_sec.is_some(), "{c:?}");
    assert_eq!(c.items_per_sec, None, "{c:?}");
}

/// **A cold first embed does not cost the probe its measurement** (#11).
///
/// The warm-up embed exists to pay the model-load cost out of the
/// measurement (J3-R1-2), but it was charged to the same [`PROBE_BUDGET`] as
/// the timed legs. Measured on the M3 Pro with the candle Metal BGE-M3 on a
/// cold page cache: the first embed outran the 5 s budget, the probe
/// reported `unmeasured`, and that session never had the probe figure
/// `probe_optimism` divides by. The warm-up now has its own bound
/// ([`PROBE_WARMUP_BUDGET`]); the timed legs keep [`PROBE_BUDGET`].
#[tokio::test(start_paused = true)]
async fn a_cold_warm_up_does_not_cost_the_probe_its_measurement() {
    let embedder = scripted(vec![
        Leg::After(PROBE_BUDGET + Duration::from_secs(2)),
        Leg::After(Duration::from_millis(35)),
    ]);
    let c = probe_embedder(&embedder).await;
    assert_eq!(
        c.source,
        CalibrationSource::Probe,
        "a slow model load is what the warm-up exists to absorb: {c:?}"
    );
    assert!(c.serial_items_per_sec.is_some(), "{c:?}");
}

/// An embedder that answers **one request at a time** at a cost per byte:
/// the shape of CPU candle or a single-slot CPU llama-server, where four
/// concurrent requests take four times as long as one (#11 review P2-2).
struct SerialisingEmbedder {
    per_byte: Duration,
    slot: tokio::sync::Mutex<()>,
}

#[async_trait::async_trait]
impl Embedder for SerialisingEmbedder {
    fn dimensions(&self) -> usize {
        8
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::EmbedError> {
        let _one_at_a_time = self.slot.lock().await;
        tokio::time::sleep(self.per_byte * text.len() as u32).await;
        Ok(vec![0.0; 8])
    }
}

/// **A one-at-a-time embedder at CPU-like cost keeps its probe figure**
/// (#11 review P2-2).
///
/// Since #11 the concurrent leg is [`PROBE_CONCURRENCY`] whole writes, eight
/// ~1 KiB embeds, inside the same [`PROBE_BUDGET`] as before. An embedder
/// that serialises requests at about 0.6 ms a byte answers both serial legs
/// well inside the budget and cannot finish the concurrent one, and the
/// whole probe reported `unmeasured`, so `write_queue_probe_optimism`, the
/// figure #11 added, was null for good on exactly the deployments where the
/// probe and the observed rate disagree most. The serial figures are what
/// `probe_optimism` divides, so they are published, and the concurrent
/// figure says it was not measured.
#[tokio::test(start_paused = true)]
async fn a_serialising_embedder_keeps_the_probes_serial_figure() {
    let embedder = SerialisingEmbedder {
        per_byte: Duration::from_micros(600),
        slot: tokio::sync::Mutex::new(()),
    };
    let c = probe_embedder(&embedder).await;
    assert_eq!(
        c.source,
        CalibrationSource::Probe,
        "both serial legs landed, so the probe measured something: {c:?}"
    );
    let write_bytes: usize = probe_write_contexts().iter().map(String::len).sum();
    let expected = 1.0 / (write_bytes as f64 * 0.000_6);
    let serial = c
        .serial_items_per_sec
        .expect("the serial figure is published");
    assert!(
        (serial - expected).abs() < 0.01,
        "the representative write decides it: {serial} against {expected}"
    );
    assert_eq!(c.probe_serial_items_per_sec, c.serial_items_per_sec);
    assert_eq!(
        c.items_per_sec, None,
        "the concurrent leg ran out of budget, so it says nothing it did not measure"
    );
    let optimism = c
        .with_observed_serial(expected / 2.0)
        .probe_optimism()
        .expect("probe_optimism survives a concurrent leg that timed out");
    assert!((optimism - 2.0).abs() < 0.01, "{optimism}");
}

/// An embedder that records every text it is asked to embed.
struct RecordingEmbedder {
    texts: parking_lot::Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Embedder for RecordingEmbedder {
    fn dimensions(&self) -> usize {
        8
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::EmbedError> {
        self.texts.lock().push(text.to_string());
        Ok(vec![0.0; 8])
    }
}

/// **The probe times a representative WRITE, in the derive path's own
/// framing** (#11).
///
/// The observed rate is one lane job per sample — a whole derive, which
/// embeds every new concept framed with the call's whole prompt
/// (`hybrid::context_text` over `hybrid::derive_prompt`). The probe used to
/// time one bare 1 KiB embed, so `probe_optimism` divided an embed rate by a
/// write rate: 4.1x on the M3 Pro bench with a real BGE-M3, before any real
/// divergence. The representative leg now embeds exactly what a
/// [`PROBE_WRITE_CONCEPTS`]-concept derive embeds, every context within
/// [`PROBE_TEXT_BYTES`] so the embedder input ceiling that sized that
/// constant still holds.
#[tokio::test(start_paused = true)]
async fn the_probe_times_a_representative_write_in_the_derive_paths_own_framing() {
    let embedder = RecordingEmbedder {
        texts: parking_lot::Mutex::new(Vec::new()),
    };
    let c = probe_embedder(&embedder).await;
    assert_eq!(c.source, CalibrationSource::Probe);

    let contexts = probe_write_contexts();
    assert_eq!(contexts.len(), PROBE_WRITE_CONCEPTS);
    let concept = probe_text_at(PROBE_CONCEPT_BYTES);
    let prompt = crate::graph::hybrid::derive_prompt(vec![concept.as_str(); PROBE_WRITE_CONCEPTS]);
    for context in &contexts {
        assert_eq!(
            context,
            &crate::graph::hybrid::context_text(&concept, Some(&prompt)),
            "the probe must embed what a derive of this shape embeds"
        );
        assert!(
            context.len() <= PROBE_TEXT_BYTES,
            "every input stays within the input-ceiling bound: {}",
            context.len()
        );
        assert!(
            context.len() + PROBE_WRITE_CONCEPTS + 1 > PROBE_TEXT_BYTES,
            "and the concepts are as large as that bound allows: {}",
            context.len()
        );
    }

    let texts = embedder.texts.lock().clone();
    assert_eq!(texts.len(), PROBE_EMBEDS, "{texts:?}");
    // warm-up and the short leg, then one representative write, then
    // PROBE_CONCURRENCY of them at once.
    assert_eq!(texts[0], PROBE_TEXT);
    assert_eq!(texts[1], PROBE_TEXT);
    let writes = &texts[2..];
    assert_eq!(writes.len(), (1 + PROBE_CONCURRENCY) * PROBE_WRITE_CONCEPTS);
    for text in writes {
        assert!(contexts.contains(text), "{text:?}");
    }
}
