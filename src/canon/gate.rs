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

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::Serialize;
use std::time::Duration;

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

/// The four canonization gates, as `/api/inspect` carries them additively
/// beside `status` and `blast_radius`, plus the Stage-3 re-promotion cooldown
/// — the one non-threshold reason a concept that clears all four gates still
/// stalls.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct GateProgress {
    /// GC survival floor (Stage 1): `ge` 3.
    pub gc_survived: GateMetric,
    /// Blast-radius floor (Stage 3): strictly `> 5`.
    pub blast_radius: GateMetric,
    /// Distinct origin interactions (Stage 2): `ge` 3.
    pub distinct_interactions: GateMetric,
    /// Session-extent coverage (Stage 2): `ge` 0.3.
    pub coverage: GateMetric,
    /// Whether the Stage-3 re-promotion cooldown currently applies (a concept
    /// demoted inside the last `cooldown` cannot be re-promoted even when
    /// every gate above reads `met`).
    pub in_cooldown: bool,
    /// Instant at which the cooldown lifts; present only while `in_cooldown`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cooldown_until: Option<DateTime<Utc>>,
}

impl GateProgress {
    /// How many of the four gates the concept currently clears. The cooldown
    /// is deliberately not counted: it is a separate transient reason (a
    /// cooling Venerable can read all four `met` yet still not promote).
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

/// Measure `concept`'s progress across the four canonization gates.
///
/// `blast_radius` and `distinct_interactions`/`coverage` are queried from
/// `store` with the same `min_edge_age` cutoff and `interaction_span` call the
/// evaluation passes to Stages 3 and 2, so the surfaced numbers are the ones
/// the eval would reach; the cooldown mirrors Stage 3's
/// `in_repromotion_cooldown` from [`Concept::last_demotion_time`] +
/// `cooldown`.
pub async fn gate_progress(
    store: &dyn GraphStore,
    session: &SessionId,
    concept: &Concept,
    min_edge_age: Duration,
    cooldown: Duration,
    now: DateTime<Utc>,
) -> Result<GateProgress, StoreError> {
    // Stage 3's measurement: the aged dependent count, `>` 5.
    let blast = store
        .blast_radius(session, concept.id, min_edge_age, now)
        .await?;
    let span = store
        .interaction_span(session, concept.id, min_edge_age, now)
        .await?;
    let until = cooldown_until(concept.last_demotion_time, cooldown);
    // Mirror Stage 3's predicate exactly (an unrepresentable cooldown stays
    // conservative: still cooling), so the payload agrees with the promotion
    // decision.
    let in_cooldown = in_cooldown(concept.last_demotion_time, cooldown, now);
    Ok(GateProgress {
        gc_survived: GateMetric::at_least(concept.gc_survived as f64, MIN_GC_SURVIVED as f64),
        blast_radius: GateMetric::strictly_above(blast as f64, MIN_BLAST_RADIUS as f64),
        distinct_interactions: GateMetric::at_least(span.distinct as f64, MIN_DISTINCT as f64),
        coverage: GateMetric::at_least(span.coverage, MIN_COVERAGE),
        in_cooldown,
        cooldown_until: in_cooldown.then(|| until).flatten(),
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

