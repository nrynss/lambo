//! Transport-neutral request rules shared by every surface.
//!
//! The CLI, the MCP tools, the read-only web portal and the write queue all
//! enforce the same request bounds and validate client strings the same way.
//! Those rules live here, owned by no one surface; each surface keeps only its own adaptation (error types, exit
//! codes, tool-result shapes, HTTP statuses).
//!
//! * [`limits`]: request caps (`MAX_TOP_K`, `MAX_CONTENT_BYTES`, ...) and the
//!   config-default clamp.
//! * [`validate`]: string validation ([`validate::check_size`]).
//!
//! `crate::cli::caps` re-exports the public items, so paths written against
//! it before #25 still resolve.

pub mod limits;
pub mod validate;
