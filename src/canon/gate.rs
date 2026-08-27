//! Per-concept canonization gate progress — the T3 `gate_progress` payload.
//!
//! Why a concept is not canonical yet is fully computable, and every required
//! number is already produced by the canonization evaluation: `gc_survived`
//! lives on the concept row, blast radius is Stage 3's measurement, and
//! `distinct` / `coverage` are Stage 2's `interaction_span`. This module
//! **surfaces** those measurements (T11) rather than inventing a calculation:
//!
//! * `gc_survived` is read from the persisted [`Concept::gc_survived`] field
//!   the GC already bumps.
//! * `blast_radius` is Stage 3's own measurement: the store's aged
//!   dependent count, re-queried with the same `min_edge_age` cutoff the eval
//!   passes to [`GraphStore::blast_radius`] — the surfaced value *is* the
//!   number the engine compared, not an age-unfiltered mirror.
//! * `distinct_interactions` / `coverage` come from
//!   [`GraphStore::interaction_span`] — the exact query Stage 2 runs.
//! * `in_cooldown` / `cooldown_until` mirror Stage 3's re-promotion cooldown
//!   ([`Concept::last_demotion_time`] + the cooldown config), so a cooling
//!   Venerable that clears all four gates is explained instead of stalling
//!   invisibly.
//!
//! serve-web is a lease-free reader and cannot see the eval's transient
//! in-process results, so these are recomputed *through the eval's own queries
//! and thresholds*, never as a parallel metric the two could drift apart on.
//! The bars are the stage modules' own `MIN_*` constants — a single source.
//!
//! ## These four gates are `Swarm`'s gates (C2)
//!
//! Everything above is true **only under [`PromotionPolicy::Swarm`]**. Under
//! `Solo` the promotion decision is [`crate::canon::SoloScorer`]'s recurrence
//! score and its bands: `gc_survived` is not read, the stage-2/stage-3 evidence
//! verdict is not consulted (`admits_hop` ignores its `evidence` argument
//! outright), and so none of `gc_survived`, `blast_radius`,
//! `distinct_interactions` or `coverage` participates. Surfacing them anyway
//! would put "0 of 4 gates met" on the page beside a concept that goes
//! Canonical on the next cycle — the payload contradicting the engine, which is
//! the one thing this module promises never to do.
//!
//! So the four move into [`SwarmGates`], which [`gate_progress`] leaves absent
//! under `Solo`. What does **not** move is the cooldown, and that split is the
//! whole reason this is a *shaped* payload and not a suppressed one:
//!
//! * The re-promotion cooldown is policy-independent, and on the solo path it
//!   is the only store-side determinant left. `canon::eval` gates the
//!   Venerable → Canonical hop on `stage3::in_repromotion_cooldown` precisely
//!   **when the stage-3 evidence is absent** — which is exactly the
//!   score-admitted hop `Solo` takes on every promotion.
//! * This struct is the cooldown's only carrier. A budget-demoted concept that
//!   climbed back to Venerable on the band alone and is now stalling out a
//!   300 s cooldown has exactly one honest explanation available, and
//!   suppressing the whole struct threw that away along with the four
//!   irrelevant numbers — leaving the operator with *no* account of the stall
//!   where the pre-C2 payload at least carried this one correct fact.
//!
//! [`GateProgress::policy`] labels which policy the block describes, so a
//! client never has to infer from an absent key whether the four gates were
//! irrelevant, or merely unavailable. Absence still means one thing higher up:
//! H2 drops the entire block for a Canonical concept, which has no promotion
//! gates left to explain — but that is the caller's decision about a promoted
//! fact, not this module's about a policy.

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::Serialize;
use std::time::Duration;

use crate::canon::PromotionPolicy;
use crate::store::GraphStore;
use crate::types::{Concept, SessionId, StoreError};

use super::stage1::MIN_GC_SURVIVED;
use super::stage2::{MIN_COVERAGE, MIN_DISTINCT};
use super::stage3::MIN_BLAST_RADIUS;

/// One gate: the concept's current value, the bar the evaluation applies, and
/// whether it clears — decided by the stage's own comparison (`>=` for every
/// gate except blast radius, which is `>`).
#[derive(Clone, Copy, Debug, Serialize)]
pub struct GateMetric {
    /// The concept's current measurement.
    pub current: f64,
    /// The evaluation's bar for this gate.
    pub bar: f64,
    /// Whether the concept currently clears this gate.
    pub met: bool,
    /// True only for blast radius, whose Stage-3 bar is *strictly above*:
    /// the page phrases it "needs above N" rather than "needs N".
    pub strictly_above: bool,
}

impl GateMetric {
    fn at_least(current: f64, bar: f64) -> Self {
        Self {
            current,
            bar,
            met: current >= bar,
            strictly_above: false,
        }
    }

    fn strictly_above(current: f64, bar: f64) -> Self {
        Self {
            current,
            bar,
            met: current > bar,
            strictly_above: true,
        }
    }
}

/// Swarm's four store-evidence gates, grouped so that a policy which reads
/// none of them can drop all four **without dropping the cooldown that sits
/// beside them** (C2 — see the module docs).
///
/// One `Option<SwarmGates>` rather than four `Option<GateMetric>` on
/// [`GateProgress`]: four independent flags can disagree, and "coverage
/// present, distinct absent" is not a state any policy produces. The holder is
/// `#[serde(flatten)]`ed, so the wire shape is unchanged — under `Swarm` these
/// four serialize as `gate_progress`'s own keys, exactly where they have always
/// been, and under `Solo` they are simply not there.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct SwarmGates {
    /// GC survival floor (Stage 1): `ge` 3.
    pub gc_survived: GateMetric,
    /// Blast-radius floor (Stage 3): strictly `> 5`.
    pub blast_radius: GateMetric,
    /// Distinct origin interactions (Stage 2): `ge` 3.
    pub distinct_interactions: GateMetric,
    /// Session-extent coverage (Stage 2): `ge` 0.3.
    pub coverage: GateMetric,
}

impl SwarmGates {
    /// How many of the four gates the concept currently clears.
    pub fn met_count(&self) -> usize {
        [
            self.gc_survived,
            self.blast_radius,
            self.distinct_interactions,
            self.coverage,
        ]
        .iter()
        .filter(|m| m.met)
        .count()
    }
}

/// What `/api/inspect` carries additively beside `status` and `blast_radius`:
/// the live policy, swarm's four gates when that policy reads them, and the
/// Stage-3 re-promotion cooldown — the one non-threshold reason a concept that
/// clears everything else still stalls.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct GateProgress {
    /// The policy whose promotion decision the rest of this block describes.
    ///
    /// Always present, and load-bearing rather than decorative: `serve-web` is
    /// a lease-free reader that resolves its **own** config, so it can be
    /// running a different `promotion_policy` than the writer whose graph it is
    /// reading. A client handed a gate count with no policy beside it has no
    /// way to tell a meaningful "0 of 4" from a reader/writer mismatch.
    pub policy: PromotionPolicy,
    /// Swarm's four store-evidence gates — absent under
    /// [`PromotionPolicy::Solo`], which reads none of them.
    ///
    /// Flattened: under `Swarm` the four keys sit directly on this object.
    #[serde(flatten)]
    pub gates: Option<SwarmGates>,
    /// Whether the Stage-3 re-promotion cooldown currently applies (a concept
    /// demoted inside the last `cooldown` cannot be re-promoted even when
    /// every gate reads `met`).
    ///
    /// Reported under **both** policies, because it applies under both. On the
    /// score-admitted solo path it is the only store-side determinant this
    /// block carries: `canon::eval` puts `in_repromotion_cooldown` on the
    /// Venerable → Canonical hop whenever the stage-3 evidence is absent.
    pub in_cooldown: bool,
    /// Instant at which the cooldown lifts; present only while `in_cooldown`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cooldown_until: Option<DateTime<Utc>>,
}

impl GateProgress {
    /// How many of swarm's four gates the concept currently clears, or `None`
    /// when the live policy does not read them.
    ///
    /// `None` rather than `0`: a caller that rendered "0 of 4 met" beside a
    /// `Solo` concept due to go Canonical next cycle is the exact defect this
    /// signature prevents, by refusing to answer instead of answering wrongly.
    /// The cooldown is deliberately not counted either way — it is a separate
    /// transient reason (a cooling Venerable can read all four `met` yet still
    /// not promote).
    pub fn met_count(&self) -> Option<usize> {
        Some(self.gates?.met_count())
    }
}

/// Measure `concept`'s progress toward canonization under the live `policy`.
///
/// Under [`PromotionPolicy::Swarm`], `blast_radius` and
/// `distinct_interactions`/`coverage` are queried from `store` with the same
/// `min_edge_age` cutoff and `interaction_span` call the evaluation passes to
/// Stages 3 and 2, so the surfaced numbers are the ones the eval would reach.
///
/// Under [`PromotionPolicy::Solo`] the [`SwarmGates`] block is absent and
/// **neither store query runs** — solo promotion reads none of those four
/// measurements, so there is no honest reading of them to ship.
///
/// The cooldown is computed either way and returned either way, mirroring
/// Stage 3's `in_repromotion_cooldown` from [`Concept::last_demotion_time`] +
/// `cooldown`. It is not a swarm gate; it is the hop's gate under both
/// policies, and under `Solo` the only one this block can speak to (see the
/// module docs).
///
/// The policy is a parameter rather than the caller's business precisely
/// because that promise — "the surfaced numbers are the ones the eval would
/// reach" — is made here, and a second caller must not be able to break it by
/// forgetting to check.
pub async fn gate_progress(
    store: &dyn GraphStore,
    session: &SessionId,
    concept: &Concept,
    policy: PromotionPolicy,
    min_edge_age: Duration,
    cooldown: Duration,
    now: DateTime<Utc>,
) -> Result<GateProgress, StoreError> {
    let gates = match policy {
        // Not an optimisation. Running the two gate-only queries here would put
        // four numbers on the wire that no solo promotion decision consults,
        // which is the contradiction this module exists to avoid.
        PromotionPolicy::Solo => None,
        PromotionPolicy::Swarm => {
            // Stage 3's measurement: the aged dependent count, `>` 5.
            let blast = store
                .blast_radius(session, concept.id, min_edge_age, now)
                .await?;
            let span = store
                .interaction_span(session, concept.id, min_edge_age, now)
                .await?;
            Some(SwarmGates {
                gc_survived: GateMetric::at_least(
                    concept.gc_survived as f64,
                    MIN_GC_SURVIVED as f64,
                ),
                blast_radius: GateMetric::strictly_above(blast as f64, MIN_BLAST_RADIUS as f64),
                distinct_interactions: GateMetric::at_least(
                    span.distinct as f64,
                    MIN_DISTINCT as f64,
                ),
                coverage: GateMetric::at_least(span.coverage, MIN_COVERAGE),
            })
        }
    };
    let until = cooldown_until(concept.last_demotion_time, cooldown);
    // Mirror Stage 3's predicate exactly (an unrepresentable cooldown stays
    // conservative: still cooling), so the payload agrees with the promotion
    // decision.
    let in_cooldown = in_cooldown(concept.last_demotion_time, cooldown, now);
    Ok(GateProgress {
        policy,
        gates,
        in_cooldown,
        cooldown_until: in_cooldown.then_some(until).flatten(),
    })
}

/// The instant the re-promotion cooldown lifts (`last_demotion + cooldown`).
///
/// `None` is not a cooldown. An unrepresentable `cooldown` decays to `None` —
/// but see [`in_cooldown`], which independently conserves it (mirrors
/// `stage3::in_repromotion_cooldown`).
fn cooldown_until(
    last_demotion: Option<DateTime<Utc>>,
    cooldown: Duration,
) -> Option<DateTime<Utc>> {
    let t = last_demotion?;
    let cd = ChronoDuration::from_std(cooldown).ok()?;
    t.checked_add_signed(cd)
}

/// True when `last_demotion` is `Some(t)` and `now < t + cooldown`. Mirrors
/// `stage3::in_repromotion_cooldown` exactly: `None` is not a cooldown, and an
/// unrepresentable `cooldown` is treated as still cooling (conservative;
/// config default is 300s).
fn in_cooldown(
    last_demotion: Option<DateTime<Utc>>,
    cooldown: Duration,
    now: DateTime<Utc>,
) -> bool {
    let Some(t) = last_demotion else {
        return false;
    };
    let Ok(cd) = ChronoDuration::from_std(cooldown) else {
        return true;
    };
    match t.checked_add_signed(cd) {
        Some(until) => now < until,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four gates in one place, so a case below states only what it varies.
    fn gates(gc: bool, blast: bool, distinct: bool, coverage: bool) -> SwarmGates {
        // Built through the stages' own constructors rather than by setting
        // `met` directly, so a `met` that stopped following the comparison
        // would show up here too. Blast radius is the `>` gate; the other three
        // are `>=`.
        SwarmGates {
            gc_survived: GateMetric::at_least(if gc { 3.0 } else { 2.0 }, 3.0),
            blast_radius: GateMetric::strictly_above(if blast { 6.0 } else { 5.0 }, 5.0),
            distinct_interactions: GateMetric::at_least(if distinct { 3.0 } else { 2.0 }, 3.0),
            coverage: GateMetric::at_least(if coverage { 0.3 } else { 0.29 }, 0.3),
        }
    }

    fn progress(gates: Option<SwarmGates>, policy: PromotionPolicy) -> GateProgress {
        GateProgress {
            policy,
            gates,
            in_cooldown: false,
            cooldown_until: None,
        }
    }

    /// Each of the four gates has to be counted, and counted once.
    ///
    /// T3-P3-5: `SwarmGates::met_count` had no test at all, so dropping a gate
    /// from its array — or counting one twice — was a silent off-by-one in the
    /// "N of 4" an operator reads. Every single-gate case is enumerated rather
    /// than spot-checked, because a dropped element only shows up in the case
    /// where *that* gate is the one that is met.
    #[test]
    fn met_count_counts_each_of_the_four_gates_exactly_once() {
        assert_eq!(gates(false, false, false, false).met_count(), 0);
        assert_eq!(gates(true, false, false, false).met_count(), 1);
        assert_eq!(gates(false, true, false, false).met_count(), 1);
        assert_eq!(gates(false, false, true, false).met_count(), 1);
        assert_eq!(gates(false, false, false, true).met_count(), 1);
        assert_eq!(gates(true, true, false, false).met_count(), 2);
        assert_eq!(gates(true, true, true, false).met_count(), 3);
        assert_eq!(gates(true, true, true, true).met_count(), 4);
    }

    /// The reason [`GateProgress::met_count`] returns an `Option` — and the
    /// CHANGELOG's Breaking bullet advertises the signature as a safety
    /// property, so it is pinned rather than described.
    ///
    /// T3-P3-5: this was untested and uncalled, and replacing the body with
    /// `Some(self.gates.map(..).unwrap_or(0))` — literally the "0 of 4 under
    /// Solo" defect the signature exists to prevent — passed the whole suite.
    /// The distinction that matters is between `None` (this policy does not
    /// read the four gates, so there is no count) and `Some(0)` (it does, and
    /// none of them is met), which is why both appear here.
    #[test]
    fn met_count_refuses_to_answer_when_the_policy_reads_no_gates() {
        assert_eq!(
            progress(None, PromotionPolicy::Solo).met_count(),
            None,
            "a Solo block has no gate count; answering 0 would put \"0 of 4 met\" \
             beside a concept that goes Canonical on the next cycle"
        );
        assert_eq!(
            progress(
                Some(gates(false, false, false, false)),
                PromotionPolicy::Swarm
            )
            .met_count(),
            Some(0),
            "an honest zero under Swarm must stay distinguishable from Solo's None"
        );
        assert_eq!(
            progress(
                Some(gates(true, false, true, false)),
                PromotionPolicy::Swarm
            )
            .met_count(),
            Some(2)
        );
        assert_eq!(
            progress(Some(gates(true, true, true, true)), PromotionPolicy::Swarm).met_count(),
            Some(4)
        );
        // The absence is keyed to `gates`, not to the policy label: a caller
        // that shipped `Solo` with a gate block (or `Swarm` without one — the
        // reader/writer mismatch this payload exists to expose) still gets an
        // answer about the gates it actually has.
        assert_eq!(
            progress(
                Some(gates(true, false, false, false)),
                PromotionPolicy::Solo
            )
            .met_count(),
            Some(1)
        );
        assert_eq!(progress(None, PromotionPolicy::Swarm).met_count(), None);
    }
}
