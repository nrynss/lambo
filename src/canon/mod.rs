//! Canonization stages Candidate → Venerable → Canonical (P6).

mod stage1;
mod stage2;
mod stage3;

pub use stage1::stage1_candidates;
pub use stage2::stage2_passes;
pub use stage3::stage3_passes;
