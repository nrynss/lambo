//! Canonization stages Candidate → Venerable → Canonical (P6).

mod stage1;
mod stage2;

pub use stage1::stage1_candidates;
pub use stage2::stage2_passes;
