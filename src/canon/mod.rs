//! Canonization stages Candidate → Venerable → Canonical (P6, spec §10).
//!
//! `stage1`/`stage2`/`stage3` are store-agnostic predicates; `eval` is the
//! write path (one hop per cycle, budget, audit, emit); `task` is the
//! `canonization_eval_interval` loop that drives it in an assembled process.
//!
//! Time: gates measure **event time** — the instant a fact is about — with
//! flush time as the per-fact fallback (`event_time` module holds the design
//! and the separation count C2's solo score will consume). The eval's `now`
//! stays injectable end to end (F8): [`CanonizationTask::with_clock`] pins
//! it for tests, and every adapter resolves stored instants against the
//! caller-supplied anchor rather than reading a clock of its own.

mod eval;
mod event_time;
mod gate;
mod policy;
mod stage1;
mod stage2;
mod stage3;
mod task;

pub use eval::{eval_cycle, EvalError, EvalOutcome, EvalParams, Evaluator};
pub use event_time::separated_session_count;
pub use gate::{gate_progress, GateMetric, GateProgress};
pub use policy::{PromotionPolicy, PromotionScorer, SoloScorer, SwarmScorer};
pub use stage1::stage1_candidates;
pub use stage2::stage2_passes;
pub use stage3::{last_demotion_time, stage3_passes};
pub use task::CanonizationTask;
