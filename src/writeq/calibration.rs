//! Calibration telemetry: the startup probe and the observed write rate.
//!
//! **Telemetry only.** Since the J3 redesign no rate sizes a bound: the
//! admission bounds are the static [`super::WRITE_QUEUE_LANE_MAX`] and
//! [`super::WRITE_QUEUE_MAX`], and durable intents carry the close-drain
//! invariant. What survives here is the comparison an operator reads in
//! `lambo_stats`: the probe's serial figure beside the rate real writes
//! observed ([`Calibration::probe_optimism`]).
//!
//! The probe is spawned at construction ([`WritePipeline::spawn`]) and never
//! awaited by anything; it is aborted at close and on `Drop`.

use std::sync::atomic::Ordering;
use std::time::Duration;

use super::{WritePipeline, WRITE_QUEUE_LANE_MAX, WRITE_QUEUE_MAX};
use crate::embed::Embedder;

/// Sanitization clamp on a **reported** rate, in items/second — telemetry
/// hygiene, not a bound (J3 redesign: no rate sizes a bound any more).
///
/// `ObservedRate::items_per_sec` and `rate_of` reach for this when a wall
/// time reads zero or absurd — the `crate::FixtureEmbedder` case, which
/// "measures" ~98 000 items/s by not doing work. One full queue
/// ([`WRITE_QUEUE_MAX`]) per second is comfortably above any real embedder
/// this project has measured (110 to 141 items/s 4-wide, llama.cpp BGE-M3 on
/// CPU at the 35-byte probe text; ~19 to 22 at the 1 KiB one; ≈40–45 serial
/// from the same **22 to 27 ms** embed — J3-R3-5 corrected this line's
/// "22–25 ms" misquote of §Measurements) while still being a number instead of
/// infinity, so a fixture-fast reading stays plottable without pretending to
/// mean something. NOTE for the operator reading
/// `write_queue_serial_items_per_sec` against these reference figures: since
/// round 2 that key publishes the **slower of two probe sizes** (35 B and
/// 1 KiB), so ~18 to 21 items/s is the ordinary reading on this rig, not the
/// 40–45 the short text alone used to show.
/// **Stated on its own terms since J3 round-1 N4.** This used to read
/// `WRITE_QUEUE_MAX as u64`, which tied a telemetry ceiling to an admission cap
/// for no reason — and, through the build assert below, made the admission cap
/// answerable to a *measured embedder rate*: `PROBE_CLAMP_RPS > 3 ×
/// MEASURED_LOCAL_EMBEDDER_RPS` implied `WRITE_QUEUE_MAX ≥ 424` and therefore
/// `MAX_RETAINED_RECEIPTS ≥ 1696`, so both surviving bounds were structural in
/// kind and measured in magnitude, at build time, by the estimator this branch
/// deleted. The value is unchanged (1024, so no reading moves); what changed is
/// that nothing about admission now depends on a number this rig's llama.cpp
/// produced, and the assert below guards telemetry alone.
pub const PROBE_CLAMP_RPS: u64 = 1_024;

/// The fastest embedder throughput measured on this rig, in items/second, at
/// [`PROBE_CONCURRENCY`]: a live llama.cpp BGE-M3 q8_0 on CPU, probed through
/// the release binary over stdio (2026-08-20; the run reported 110, 131 and 141
/// across repeats). Recorded as a constant so the guard below can be a build
/// invariant rather than a sentence.
///
/// **Those repeats were taken at the 35-byte [`PROBE_TEXT`], and that is
/// deliberately what this constant keeps** (J3-R2-1). Probing at
/// [`PROBE_TEXT_BYTES`] the same rig reads ~19 to 22 items/s 4-wide — five times
/// lower — but the guard below wants the *largest* rate a real local embedder
/// can produce, since its job is to keep [`PROBE_CLAMP_RPS`] clear of one.
/// Replacing 141 with the smaller figure would loosen the guard while looking
/// like an update.
pub const MEASURED_LOCAL_EMBEDDER_RPS: u64 = 141;

/// Build-time invariant: the clamp must sit well clear of a real embedder.
///
/// This is the guard the first version of these constants failed. A clamp
/// derived at 128 items/s sat *below* the 141 measured here, so it would have
/// clipped an ordinary local embedder's own reading. Three times the measured
/// rate is the margin.
///
/// **What this guard is about, restated (J3 round-1 N4).** It used to say that
/// violating it would make "the queue bound stop being a per-deployment
/// measurement and become a constant" — which is now the *intended* state, and
/// which is how a live build assertion came to be the thing sizing both
/// surviving bounds. Since [`PROBE_CLAMP_RPS`] no longer derives from
/// [`WRITE_QUEUE_MAX`], this constrains nothing but telemetry hygiene: it keeps
/// the sanitizer clear of any rate a real embedder can produce, so a clamped
/// reading always means "this measurement is not real" and never "this embedder
/// is fast".
const _: () = assert!(
    PROBE_CLAMP_RPS > 3 * MEASURED_LOCAL_EMBEDDER_RPS,
    "PROBE_CLAMP_RPS must stay well above the throughput a real local embedder measures, or the \
     sanitizer would clip a genuine reading and the rate telemetry would report a number no \
     deployment produced. It bounds no admission decision: the bounds are static",
);

/// Width of the calibration probe's **concurrent** leg.
///
/// Four, because the phase doc's parallelism figure is a 4-wide one (4 recalls:
/// 380 ms sequential against 64 ms concurrent). The probe re-measures the rate
/// per deployment; the 4 fixes only how wide that leg is taken. It sizes
/// **nothing** — it is the width of a telemetry reading, and the pairing that
/// matters is that the concurrent leg is reported beside the serial one so the
/// two widths are comparable (J3 round-1 N3: this said "It sizes the *aggregate*
/// bound only — the per-lane bound comes from the serial leg", which was true of
/// the deleted estimator and is true of nothing now).
pub const PROBE_CONCURRENCY: usize = 4;

/// Embeds the probe throws away before it starts timing.
///
/// One, and it is the fix for J3-R1-2: the probe fires at session build, the
/// coldest moment in the process's life, and four consecutive runs of the same
/// binary against the same llama-server measured **21.2, 101.0, 150.2 and
/// 134.7 items/s** — a 7× swing in the load-bearing number, every one of them
/// reported as `measured: true`. A discarded first embed pays the model-load
/// cost out of the probe's budget instead of out of the measurement. It is not
/// the whole fix: the observed rate ([`OBSERVED_MIN_SAMPLES`]) is what makes
/// the durability property independent of *which* reading the probe caught.
pub const PROBE_WARMUP_EMBEDS: usize = 1;

/// Embeds one calibration probe performs in total: a discarded warm-up, **two**
/// timed alone (the serial legs, which are the width a lane drains at — one at
/// [`PROBE_TEXT`] and one at [`PROBE_TEXT_BYTES`], J3-R2-1), then
/// [`PROBE_CONCURRENCY`] together.
///
/// Seven rather than six. The seventh is the representative serial leg, and it
/// is best-effort: `probe_embedder` carries on with the short figure when it
/// fails, so the count is a budget input rather than a requirement.
pub const PROBE_EMBEDS: usize = PROBE_WARMUP_EMBEDS + 2 + PROBE_CONCURRENCY;

/// Real writes observed before their measured service time replaces the
/// probe's serial figure.
///
/// [`PROBE_CONCURRENCY`], so the observed rate never rests on fewer embeds than
/// the probe's own leg did. This is J3-R1-2's remediation (a): the worker
/// already has the timings, a lane is single-consumer so those timings *are*
/// serial service time, and it covers the whole of `WriteCtx::run` rather
/// than the embed alone. Once it takes over, a cold probe stops being a
/// sentence the whole session has to live with — including a probe that failed
/// outright and left the session with no rate telemetry at all (J3 round-1 N3:
/// "and floored the bound" named `WRITE_QUEUE_MIN`, which this branch deleted;
/// a failed probe now costs `write_queue_measured: false` and nothing else).
pub const OBSERVED_MIN_SAMPLES: u64 = PROBE_CONCURRENCY as u64;

/// EWMA weight for observed service time, as a divisor: the new sample gets
/// `1 / OBSERVED_EWMA_WEIGHT`.
///
/// [`PROBE_CONCURRENCY`] again, so the average moves most of the way in about
/// one probe's width of samples. A weight of 1 would make the published **rate**
/// track a single slow write and oscillate; a much larger one would keep a warm
/// figure long after the embedder degraded, which is the J3-R1-3 scenario. Both
/// are now telemetry faults rather than admission faults (J3 round-1 N3: this
/// said "would make the bound track a single slow write"), and the reason to
/// smooth is unchanged: an operator diagnosing a slow embedder needs a number
/// that means something, and a 1-weight rate means only "the last write".
pub const OBSERVED_EWMA_WEIGHT: u32 = PROBE_CONCURRENCY as u32;

/// Bound on the calibration probe — **all [`PROBE_EMBEDS`] of its embeds
/// together**.
///
/// The probe is *spawned*, not awaited, at session build — and since the J3
/// redesign **nothing else awaits it either**: admission uses the static caps,
/// so this budget prices only how long the telemetry may take to publish.
/// (Its earlier career as "the worst case an admission can wait" ended with
/// `await_calibration`.)
///
/// Unchanged at 5 s even though the probe takes seven embeds rather than
/// four, one of them at [`PROBE_TEXT_BYTES`]: a deployment too cold to answer
/// seven embeds in 5 s is better served by reporting `unmeasured` and being
/// **corrected by observation** ([`OBSERVED_MIN_SAMPLES`]) than by publishing
/// a number taken while its model was still loading.
///
/// The J3 redesign makes the trade almost free: the probe is telemetry, so a
/// blown budget costs `write_queue_measured: false` and an absent baseline for
/// [`Calibration::probe_optimism`] — never an admission. Measured warm at the
/// release binary against the live BGE-M3, all seven embeds land in ~180 ms of
/// the 5 s.
pub const PROBE_BUDGET: Duration = Duration::from_secs(5);

/// Text the probe's **short** leg embeds — 35 bytes, fixed, and chosen for one
/// property only: **every embedder accepts it.**
///
/// The sentence this used to carry, "it is measuring the deployment's embedder,
/// not its own input", was **false and load-bearing** (J3-R2-1). Input length is
/// a first-order determinant of a transformer's latency, so a rate measured on
/// 35 bytes is a rate for 35-byte writes and nothing else.
///
/// **No per-deployment measured table is stamped here (J3-R2R-6).** The table
/// this docstring once published — "1536 B and up — HTTP 500, 8 of 8" — did not
/// reproduce on the rig it named: re-measured, the refusal actually sat between
/// **2048 B and 3072 B**, and the latencies swung by up to 1.6×. A measured
/// table needs its conditions (the server's batch flags and a date), which is
/// exactly what a code docstring cannot stay in sync with. The *shape* of the
/// argument survives and is the part that is load-bearing:
///
/// * input length is first-order for a transformer's latency, so the short leg
///   measures the embedder on its shortest possible work, and
/// * an embedder has an input ceiling of **its own** (this llama-server refuses
///   somewhere above ~2 KiB on its configured batch), so a probe text large
///   enough to trip it would turn every probe into
///   [`Calibration::unmeasured`] — a worse outcome than an optimistic number.
///
/// So the short leg stays, always answerable, and [`PROBE_TEXT_BYTES`] is
/// measured **beside** it rather than instead of it. The honest reading of the
/// round-2 re-measurement (largest power of two under the smallest refusal) puts
/// the representative leg at 1024 B.
pub const PROBE_TEXT: &str = "lambo write queue calibration probe";

/// Size of the probe's **representative** leg, in bytes: 1024.
///
/// Two bounds pick this number, one from below and one from above.
///
/// * From below, the workload: lambo's own dogfood concepts — the `Logic` and
///   `Constraint` entries `lambo_recall` returns — run **700 to 1500 bytes**, so
///   a leg inside that band measures the embedder on the shape the product
///   actually writes.
/// * From above, the embedder: an embedder has an input ceiling of its own —
///   re-measured on this rig's llama-server, the refusal sits between **2048 B
///   and 3072 B** (see [`PROBE_TEXT`]; it moved off the old 1536 B claim, which
///   is why no code docstring stamps a verdict here). A representative leg must
///   stay clear of a limit it cannot know, so it takes the largest power of two
///   under the smallest refusal measured — 2048 would sit too close to a limit
///   that moved, so 1024 keeps a full power-of-two of headroom.
///
/// The leg is **best effort** on top of that: if this size is refused anyway,
/// `probe_embedder` keeps the short figure and says nothing it did not
/// measure. And the number it produces sizes nothing load-bearing — since the
/// J3 redesign, no probe number does. This leg makes
/// the probe's *published* rate honest, so that the probe-versus-observed
/// comparison in `lambo_stats` is a diagnosis rather than a pair of numbers that
/// disagree for a reason nobody recorded.
pub const PROBE_TEXT_BYTES: usize = 1024;

/// Build-time invariant: the representative leg must actually be bigger than the
/// short one, or the second leg measures the first thing twice and J3-R2-1's
/// diagnosis silently stops being measured.
const _: () = assert!(
    PROBE_TEXT_BYTES > PROBE_TEXT.len(),
    "PROBE_TEXT_BYTES must exceed PROBE_TEXT's own length — the representative leg exists to \
     measure the embedder on input LONGER than the short leg's",
);

/// Build-time invariant: [`probe_text_at`] truncates on a byte index, so the
/// text it repeats has to be ASCII or that truncation can land mid-character.
const _: () = assert!(
    is_ascii(PROBE_TEXT),
    "PROBE_TEXT must be ASCII — probe_text_at truncates by byte index to hit PROBE_TEXT_BYTES \
     exactly, which panics on a non-boundary index",
);

/// `true` when every byte of `s` is ASCII. A `const fn` because the assertion
/// above has to run at build time, and `str::is_ascii` is not const.
pub(super) const fn is_ascii(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] >= 0x80 {
            return false;
        }
        i += 1;
    }
    true
}

/// Where the published rate came from (telemetry provenance — since the J3
/// redesign no rate sizes a bound).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CalibrationSource {
    /// Nothing measured: the probe failed or timed out. Reported as
    /// `write_queue_measured: false`.
    Unmeasured,
    /// The calibration probe, at session build.
    Probe,
    /// Real write service times, observed by the lane workers
    /// ([`OBSERVED_MIN_SAMPLES`]) — strictly better evidence than the probe,
    /// because it is this deployment doing this deployment's actual work.
    Observed,
}

impl CalibrationSource {
    /// Stable machine tag for `lambo_stats`' `write_queue_bound_source`.
    pub fn tag(self) -> &'static str {
        match self {
            CalibrationSource::Unmeasured => "unmeasured",
            CalibrationSource::Probe => "probe",
            CalibrationSource::Observed => "observed",
        }
    }
}

/// The measured rates (telemetry) beside the static bounds in force.
///
/// **Throughput, not latency, is what is measured** — the figure that motivated
/// a per-deployment probe is a *parallelism* figure (4 recalls: 380 ms
/// sequential, 64 ms concurrent, 5.94x — §Measurements), and the case the spec
/// names, a hosted embedder that is slower per call but parallelises far
/// better, inverts per-call latency while raising throughput. The probe
/// publishes a serial (1-wide) figure — the width one lane drains at, J3-R1-1
/// — and a [`PROBE_CONCURRENCY`]-wide one, at two input sizes, keeping the
/// slower serial reading (J3-R2-1); real write service times replace the
/// serial figure once [`OBSERVED_MIN_SAMPLES`] land, with the probe's kept
/// beside it for [`Calibration::probe_optimism`] (J3-R2-4).
///
/// **None of these rates sizes a bound** (the J3 redesign). Three rounds spent
/// deriving `lane_bound`/`bound` from these measurements — projected against a
/// budget share, clamped under per-source ceilings — and every round a new
/// workload covariate falsified the derivation (width, warmth, length, failure
/// shape, concurrency scaling). Durable intents carry the durability invariant
/// now, so [`Calibration::lane_bound`] and [`Calibration::bound`] are simply
/// [`WRITE_QUEUE_LANE_MAX`] and [`WRITE_QUEUE_MAX`], whatever the source: a
/// fairness share and a memory cap, reported here so `lambo_stats` shows the
/// bounds in force beside the rates that no longer produce them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Calibration {
    /// Measured **serial** (1-wide) items/second — the rate one lane's single
    /// consumer retires at.
    /// `None` when nothing measured it.
    pub serial_items_per_sec: Option<f64>,
    /// The **probe's** serial figure, kept even after
    /// [`Calibration::serial_items_per_sec`] has been replaced by an observed
    /// one (J3-R2-4).
    ///
    /// Destroying it destroyed the one self-diagnosing comparison the payload
    /// had: *"this deployment's real service time is 4× what the probe
    /// measured"* is the sentence that would have caught J3-R2-1 in
    /// `lambo_stats` instead of at a review's release-binary run, and it is
    /// only sayable while both numbers exist. `None` when no probe landed.
    pub probe_serial_items_per_sec: Option<f64>,
    /// Measured **concurrent** ([`PROBE_CONCURRENCY`]-wide) items/second.
    /// `None` when nothing measured it.
    pub items_per_sec: Option<f64>,
    /// Bound on one agent's lane.
    pub lane_bound: usize,
    /// Bound on all lanes together.
    pub bound: usize,
    /// Where [`Calibration::serial_items_per_sec`] came from.
    pub source: CalibrationSource,
}

impl Calibration {
    /// No measurement at all. Says so, rather than presenting a number it did
    /// not measure; the bounds are the same static ones as everywhere.
    pub fn unmeasured() -> Self {
        Self::from_rates(None, None, None, CalibrationSource::Unmeasured)
    }

    /// Publish the probe's timed legs: one embed **alone** at
    /// [`PROBE_TEXT`], optionally a second alone at [`PROBE_TEXT_BYTES`], then
    /// [`PROBE_CONCURRENCY`] embeds **together**.
    ///
    /// The serial rate published is the **slower** of the two serial legs
    /// (J3-R2-1): they differ only in input length, so the slower one is the
    /// rate for the larger workload, and an honest telemetry figure is the
    /// conservative one. `representative_wall` is `None` when that leg was
    /// refused — which is a real case, not a defensive one: this rig's
    /// llama-server answers 1280 B and refuses 1536 B.
    pub fn from_probe(
        serial_wall: Duration,
        representative_wall: Option<Duration>,
        concurrent_wall: Duration,
    ) -> Self {
        let short = rate_of(1, serial_wall);
        let serial = match representative_wall {
            Some(wall) => short.min(rate_of(1, wall)),
            None => short,
        };
        Self::from_rates(
            Some(serial),
            Some(rate_of(PROBE_CONCURRENCY, concurrent_wall)),
            Some(serial),
            CalibrationSource::Probe,
        )
    }

    /// Replace the serial rate with one observed from real writes, keeping the
    /// probe's concurrent figure **and its serial figure for the comparison**
    /// (J3-R2-4).
    ///
    /// The observed rate is better evidence about the drain than the probe's
    /// serial leg is — it times the whole of `WriteCtx::run` rather than the
    /// embed alone, on the caller's own content rather than the probe's, and it
    /// keeps tracking an embedder that degrades after startup — so it wins
    /// outright rather than being averaged in. Winning is not the same as
    /// erasing: the number it displaced stays on
    /// [`Calibration::probe_serial_items_per_sec`], because the *ratio* between
    /// them is the diagnosis.
    pub fn with_observed_serial(&self, serial_items_per_sec: f64) -> Self {
        Self::from_rates(
            Some(serial_items_per_sec),
            self.items_per_sec,
            self.probe_serial_items_per_sec,
            CalibrationSource::Observed,
        )
    }

    pub(super) fn from_rates(
        serial: Option<f64>,
        concurrent: Option<f64>,
        probe_serial: Option<f64>,
        source: CalibrationSource,
    ) -> Self {
        // J3 redesign: the rates are published, never projected. The bounds
        // are the static fairness/memory caps for EVERY source — deriving
        // them from the rates is the retired estimator role (five falsified
        // axes; see the module doc and the deletion note at
        // WRITE_QUEUE_LANE_MAX).
        Self {
            serial_items_per_sec: serial,
            probe_serial_items_per_sec: probe_serial,
            items_per_sec: concurrent,
            lane_bound: WRITE_QUEUE_LANE_MAX,
            bound: WRITE_QUEUE_MAX,
            source,
        }
    }

    /// `true` when the bounds rest on a measurement of this deployment's own
    /// embedder — by probe or by observation.
    pub fn measured(&self) -> bool {
        self.source != CalibrationSource::Unmeasured
    }

    /// How many times faster the probe's serial leg read than the rate now in
    /// force, or `None` when there is no pair to compare (J3-R2-4).
    ///
    /// Above one means the probe was **optimistic about this deployment's own
    /// work** — the direction that, while an estimator still gated durability,
    /// abandoned writes (4.0× is what J3-R2-1 measured at the release binary).
    /// Far *below* one is just as diagnostic: an observed rate implausibly
    /// faster than the probe's embed-only figure is evidence of non-work being
    /// sampled, which is how J3-R3-1 read an impossible 0.02–0.05. Both
    /// directions are telemetry now — nothing acts on the ratio, and nothing
    /// needs to: no estimate gates durability any more. The ratio is logged
    /// once when the source flips so a field session says it out loud rather
    /// than leaving it to be derived from two keys.
    pub fn probe_optimism(&self) -> Option<f64> {
        match (self.probe_serial_items_per_sec, self.serial_items_per_sec) {
            (Some(probe), Some(now)) if now > 0.0 && self.source == CalibrationSource::Observed => {
                Some(probe / now)
            }
            _ => None,
        }
    }
}

/// `n` items in `wall`, as items/second. A zero or absurd wall time is the
/// [`crate::FixtureEmbedder`] case; the clamp is what handles it, so this must
/// not divide by zero first.
pub(super) fn rate_of(n: usize, wall: Duration) -> f64 {
    let secs = wall.as_secs_f64();
    if secs <= 0.0 {
        PROBE_CLAMP_RPS as f64
    } else {
        n as f64 / secs
    }
}

/// Serial write service time, as an EWMA over real writes.
///
/// A lane has one consumer, so the time [`WriteCtx::run`](super::WriteCtx::run) takes on it **is**
/// serial service time — better evidence about the drain than the probe's
/// embed-only leg, and it keeps tracking an embedder that degrades after
/// startup rather than freezing the first reading of the session (J3-R1-2).
#[derive(Debug, Default)]
pub(super) struct ObservedRate {
    pub(super) mean_secs: f64,
    pub(super) samples: u64,
}

impl ObservedRate {
    pub(super) fn sample(&mut self, secs: f64) {
        if !secs.is_finite() || secs < 0.0 {
            return;
        }
        self.samples = self.samples.saturating_add(1);
        if self.samples == 1 {
            self.mean_secs = secs;
            return;
        }
        let weight = 1.0 / OBSERVED_EWMA_WEIGHT as f64;
        self.mean_secs = self.mean_secs * (1.0 - weight) + secs * weight;
    }

    /// items/second, or `None` while the sample count is still under
    /// [`OBSERVED_MIN_SAMPLES`] — until then the probe's figure stands.
    pub(super) fn items_per_sec(&self) -> Option<f64> {
        if self.samples < OBSERVED_MIN_SAMPLES {
            return None;
        }
        if self.mean_secs <= 0.0 {
            // A fixture-fast deployment. The clamp is what handles it.
            return Some(PROBE_CLAMP_RPS as f64);
        }
        Some(1.0 / self.mean_secs)
    }
}

/// Measure the deployment's embedder in three legs, all inside one
/// [`PROBE_BUDGET`]:
///
/// 1. **Warm-up** — [`PROBE_WARMUP_EMBEDS`] embeds, timed and **thrown away**.
///    The probe fires at session build, the coldest moment in the process's
///    life; four consecutive runs of the same binary against the same
///    llama-server measured 21.2 → 150.2 items/s, a 7× swing, every one of them
///    reported as a measurement (J3-R1-2).
/// 2. **Serial, short** — one embed of [`PROBE_TEXT`] **alone**, wall-clocked.
///    This is the width a lane drains at, so it is what
///    `write_queue_serial_items_per_sec` reports (J3-R1-1). It is projected into
///    nothing: `project()` was deleted with the estimator, and
///    [`Calibration::lane_bound`] copies [`WRITE_QUEUE_LANE_MAX`] for every
///    source including `Unmeasured` (J3 round-1 N3).
/// 3. **Serial, representative** — one embed of [`PROBE_TEXT_BYTES`] bytes
///    alone, wall-clocked, and **best effort** (J3-R2-1). Input length is a
///    first-order determinant of a transformer's latency, so leg 2 alone
///    measures a rate for 35-byte writes; this leg measures the same words at
///    the size the product's own concepts carry. It is best effort because an
///    embedder may refuse the larger input outright — measured, not
///    hypothesised: this rig's llama-server answers 1280 B and returns HTTP 500
///    at 1536 B — and losing the whole probe to a refused *optional* leg would
///    trade an optimistic number for no number at all.
/// 4. **Concurrent** — [`PROBE_CONCURRENCY`] embeds together, wall-clocked, for
///    the aggregate rate and the parallelism figure an operator reads — for no
///    bound (J3 round-1 N3: "for the aggregate bound"). At
///    the representative size when leg 3 proved the embedder accepts it, at the
///    short one otherwise: the aggregate leg gets the same conservative input as
///    the serial one wherever that is known to be answerable.
///
/// Any **required** leg failing or the budget running out means the same thing:
/// this deployment's rate is not known, and saying so beats inventing a
/// number. Saying so costs nothing but the telemetry (J3 redesign): the
/// difference between `Unmeasured` and `Probe` is `write_queue_measured` and an
/// absent `probe_optimism` baseline — never a bound, and the observed rate
/// still replaces the figure after [`OBSERVED_MIN_SAMPLES`] real writes.
pub(super) async fn probe_embedder(embedder: &dyn Embedder) -> Calibration {
    let deadline = tokio::time::Instant::now() + PROBE_BUDGET;

    for _ in 0..PROBE_WARMUP_EMBEDS {
        match tokio::time::timeout_at(deadline, embedder.embed(PROBE_TEXT)).await {
            Ok(Ok(_)) => {}
            _ => return Calibration::unmeasured(),
        }
    }

    let serial_started = tokio::time::Instant::now();
    match tokio::time::timeout_at(deadline, embedder.embed(PROBE_TEXT)).await {
        Ok(Ok(_)) => {}
        _ => return Calibration::unmeasured(),
    }
    let serial_wall = serial_started.elapsed();

    let representative = probe_text_at(PROBE_TEXT_BYTES);
    let representative_started = tokio::time::Instant::now();
    // An OPTIONAL leg may never starve the required one that follows it, so it
    // gets at most half of what is left of the budget and the concurrent leg
    // keeps the other half. Without this, an embedder that *hangs* on the
    // larger input (rather than refusing it) would burn the whole of
    // PROBE_BUDGET here and take the probe down with it — turning an
    // improvement into a new way to reach `unmeasured`.
    let representative_deadline =
        representative_started + deadline.saturating_duration_since(representative_started) / 2;
    let representative_wall =
        match tokio::time::timeout_at(representative_deadline, embedder.embed(&representative))
            .await
        {
            Ok(Ok(_)) => Some(representative_started.elapsed()),
            // Refused, errored or out of budget. Keep the short figure and say
            // nothing about a size this embedder would not take.
            _ => {
                tracing::debug!(
                bytes = PROBE_TEXT_BYTES,
                "write queue: the calibration probe's representative leg was not answered; the \
                 serial rate rests on the short text alone"
            );
                None
            }
        };

    let concurrent_text: &str = match representative_wall {
        Some(_) => &representative,
        None => PROBE_TEXT,
    };
    let concurrent_started = tokio::time::Instant::now();
    let mut set = Vec::with_capacity(PROBE_CONCURRENCY);
    for _ in 0..PROBE_CONCURRENCY {
        set.push(embedder.embed(concurrent_text));
    }
    match tokio::time::timeout_at(deadline, futures_join_all(set)).await {
        Ok(results) if results.iter().all(Result::is_ok) => Calibration::from_probe(
            serial_wall,
            representative_wall,
            concurrent_started.elapsed(),
        ),
        _ => Calibration::unmeasured(),
    }
}

/// [`PROBE_TEXT`] repeated to exactly `bytes` bytes.
///
/// The same words, only longer — so the representative leg differs from the
/// short one in **length and nothing else**, which is what makes the pair a
/// measurement of length sensitivity rather than of two unrelated inputs
/// (J3-R2-1). Byte truncation is safe because [`PROBE_TEXT`] is ASCII, which is
/// a build invariant beside the constant rather than a comment here.
pub(super) fn probe_text_at(bytes: usize) -> String {
    let mut text = PROBE_TEXT.repeat(bytes / PROBE_TEXT.len() + 1);
    text.truncate(bytes);
    text
}

/// Join a set of futures concurrently without pulling in `futures`.
///
/// `tokio::join!` needs a fixed arity and `JoinSet` needs `'static` futures;
/// these borrow the embedder. Polling them in one future is what makes the
/// measurement a *concurrency* measurement rather than a sequential one.
pub(super) async fn futures_join_all<F: std::future::Future>(futures: Vec<F>) -> Vec<F::Output> {
    use std::pin::Pin;
    use std::task::Poll;

    let mut pinned: Vec<Pin<Box<F>>> = futures.into_iter().map(Box::pin).collect();
    let mut out: Vec<Option<F::Output>> = (0..pinned.len()).map(|_| None).collect();
    std::future::poll_fn(move |cx| {
        let mut all_done = true;
        for (slot, fut) in out.iter_mut().zip(pinned.iter_mut()) {
            if slot.is_some() {
                continue;
            }
            match fut.as_mut().poll(cx) {
                Poll::Ready(v) => *slot = Some(v),
                Poll::Pending => all_done = false,
            }
        }
        if all_done {
            Poll::Ready(
                out.iter_mut()
                    .map(|s| s.take().expect("all ready"))
                    .collect(),
            )
        } else {
            Poll::Pending
        }
    })
    .await
}

impl WritePipeline {
    /// The calibration in force: the probe's, with its serial rate replaced by
    /// the observed one once enough real writes have been seen.
    ///
    /// `None` only before either has anything to say. The observed leg can
    /// stand alone — a probe that failed publishes nothing, and a session must
    /// still be able to earn a real bound after starting on the floor.
    pub fn calibration(&self) -> Option<Calibration> {
        let probe = *self.calibration.borrow();
        let observed = self.observed.lock().items_per_sec();
        let calibration = match (probe, observed) {
            (Some(probe), Some(rate)) => Some(probe.with_observed_serial(rate)),
            (Some(probe), None) => Some(probe),
            (None, Some(rate)) => Some(Calibration::unmeasured().with_observed_serial(rate)),
            (None, None) => None,
        };
        if let Some(c) = calibration {
            self.log_observed_takeover(&c);
        }
        calibration
    }

    /// Log the probe → observed transition once, with **both** rates and the
    /// ratio between them (J3-R2-4).
    ///
    /// This is the line that would have made J3-R2-1 self-reporting in the
    /// field: an operator who abandoned writes at a clean close had
    /// `bound_source: observed` and the observed rate, and no way to learn that
    /// the burst had been admitted at four times that figure. Both numbers are
    /// in `lambo_stats` now as well; this puts the comparison in the log where
    /// nobody has to be looking at the right moment to see it.
    pub(super) fn log_observed_takeover(&self, calibration: &Calibration) {
        if calibration.source != CalibrationSource::Observed
            || self.observed_logged.swap(true, Ordering::Relaxed)
        {
            return;
        }
        tracing::info!(
            session = %self.ctx.session,
            probe_serial_items_per_sec = calibration.probe_serial_items_per_sec,
            observed_serial_items_per_sec = calibration.serial_items_per_sec,
            probe_optimism = calibration.probe_optimism(),
            lane_bound = calibration.lane_bound,
            bound = calibration.bound,
            samples = OBSERVED_MIN_SAMPLES,
            "write queue: this deployment's own writes have replaced the startup probe's serial \
             figure. probe_optimism is how many times faster the probe read than the real work; \
             far from one in either direction means the probe and the workload disagree — \
             telemetry only, since durable intents carry the close-drain invariant"
        );
    }

    /// Abort the calibration probe. Called from `Memory`'s `Drop` and from
    /// `close()`: a probe outliving its session is an embed nobody will read.
    pub(crate) fn abort_probe(&self) {
        if let Some(handle) = self.probe.lock().take() {
            handle.abort();
        }
    }
}
