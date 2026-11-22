//! Canonization stages Candidate → Venerable → Canonical (spec §10).
//!
//! `stage1`/`stage2`/`stage3` are store-agnostic predicates; `eval` is the
//! write path (one hop per cycle, budget, audit, emit); `task` is the
//! `canonization_eval_interval` loop that drives it in an assembled process.
//!
//! Time: gates measure **event time** — the instant a fact is about — with
//! flush time as the per-fact fallback (`event_time` module holds the design
//! and the separation count the solo promotion score consumes). The eval's
//! `now` stays injectable end to end: [`CanonizationTask::with_clock`] pins
//! it for tests, and every adapter resolves stored instants against the
//! caller-supplied anchor rather than reading a clock of its own.

pub mod alias;
#[cfg(feature = "store-memory")]
pub mod alias_preview;
#[cfg(all(test, feature = "store-memory"))]
mod contradicts_tests;
#[cfg(feature = "store-memory")]
pub mod dry_run;
pub mod dry_run_defaults;
mod eval;
mod event_time;
mod explain;
#[cfg(all(test, feature = "store-memory", feature = "fixtures"))]
mod fair_test_tests;
mod gate;
mod policy;
pub mod similar;
#[cfg(test)]
mod similar_tests;
mod stage1;
mod stage2;
mod stage3;
pub(crate) mod supersession;
mod task;

pub use eval::{eval_cycle, EvalError, EvalOutcome, EvalParams, Evaluator};
pub use event_time::separated_session_count;
pub(crate) use explain::explain_node;
pub(crate) use explain::with_last_demotion;
pub use explain::{
    admission_inputs, explain_admission, render_admission, Admission, AdmissionCheck,
    AdmissionInputs, Comparison,
};
pub use explain::{explain_measured, Measurements};
pub use gate::{
    gate_progress, gate_progress_measured, gate_progress_with_config, GateMetric, GateProgress,
    SwarmGates,
};
pub use policy::{
    solo_breakdown, PromotionPolicy, PromotionScorer, SoloBreakdown, SoloScorer, SwarmScorer,
    Writers,
};
// `deserialize_promotion_policy` is re-exported because `lambo.toml`'s
// `promotion_policy` key must be parsed by `PromotionPolicy::from_str` and not
// by serde's exact-`PascalCase` derive — one parser for the file and the
// environment both (see the function's docs). `mod policy` is private, so
// `config.rs` cannot name the function without a re-export; `pub(crate)` is the
// narrowest one that works, and keeps serde plumbing off the library's semver
// surface (there is nothing an external caller could do with it).
pub(crate) use policy::deserialize_config_value as deserialize_promotion_policy;
pub use stage1::{
    dependent_interaction_counts, dependent_interactions, solo_dependent_interaction_counts,
    solo_dependent_interactions, stage1_candidates,
};
pub use stage2::{stage2_evidence_passes, stage2_passes, stage2_passes_with_window};
pub use stage3::{last_demotion_time, stage3_passes};
pub use task::CanonizationTask;

/// Test-only aliases of the stage thresholds and the Solo score's weights and
/// bars, for `crate::formal_constants_tests` (#291):
/// `formal/LamboProofs/Canon.lean` and `Policy.lean` hard-code them, and
/// `stage1`, `stage2`, `stage3` and `policy` are private modules.
#[cfg(test)]
pub(crate) mod formal_pins {
    pub(crate) use super::policy::{
        CANDIDATE_BAR, CANONICAL_BAR, DEPENDENT_INTERACTION_WEIGHT, HUMAN_CONFIRMED_WEIGHT,
        REVERT_WEIGHT, SESSION_SEPARATION, SESSION_WEIGHT, VENERABLE_BAR,
    };

    pub(crate) const MIN_GC_SURVIVED: i32 = super::stage1::MIN_GC_SURVIVED;
    pub(crate) const MIN_DEPENDENT_INTERACTIONS: usize = super::stage1::MIN_DEPENDENT_INTERACTIONS;
    pub(crate) const MIN_DISTINCT: u64 = super::stage2::MIN_DISTINCT;
    pub(crate) const MIN_COVERAGE: f64 = super::stage2::MIN_COVERAGE;
    pub(crate) const MIN_BLAST_RADIUS: u64 = super::stage3::MIN_BLAST_RADIUS;
}
