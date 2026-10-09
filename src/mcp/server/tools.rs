//! The tool bodies, one file per tool (the two text write tools share one).
//!
//! The `#[tool]` wrappers in the facade are the only thing the router can
//! reach, and each runs one of these bodies through `answered`, so everything
//! here is behind `contain_panic` (plus the I1 trace and receipt delivery);
//! these are the parts that do the work.
//!
//! **Read tools answer from the RAM graph even after `close()`** (R1/T82-14):
//! `lambo_stats`, `lambo_saints` and `lambo_inspect` read state that is still
//! valid — a closed session's graph does not change — while every write tool
//! and `lambo_recall` refuse. That is deliberate: an operator inspecting why a
//! close failed still needs `lambo_stats` to answer.

mod derive_image;
mod inspect;
mod recall;
mod reserve;
mod saints;
mod stats;
mod write;
