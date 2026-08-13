//! Canonization stages Candidate → Venerable → Canonical (P6, spec §10).
//!
//! `stage1`/`stage2`/`stage3` are store-agnostic predicates; `eval` is the
//! write path (one hop per cycle, budget, audit, emit); `task` is the
//! `canonization_eval_interval` loop that drives it in an assembled process.

mod eval;
mod stage1;
mod stage2;
mod stage3;
mod task;

pub use eval::{eval_cycle, EvalError, EvalOutcome, EvalParams, Evaluator};
pub use stage1::stage1_candidates;
pub use stage2::stage2_passes;
pub use stage3::{last_demotion_time, stage3_passes};
pub use task::CanonizationTask;
