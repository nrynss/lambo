//! Transport-neutral request rules shared by every surface.
//!
//! The CLI, the MCP tools, the read-only web portal and the write queue all
//! enforce the same request bounds, validate client strings the same way, and
//! resolve an `inspect` focus the same way. Those rules live here, owned by no
//! one surface; each surface keeps only its own adaptation (error types, exit
//! codes, tool-result shapes, HTTP statuses).
//!
//! * [`limits`]: request caps (`MAX_TOP_K`, `MAX_CONTENT_BYTES`, ...) and the
//!   config-default clamp.
//! * [`validate`]: string validation ([`validate::check_size`]).
//! * [`error`]: the model-safe class of a `LamboError` (`err_class`, N4),
//!   shared by MCP's tool errors and the write queue's receipts, crate-internal.
//! * [`focus`]: `inspect` focus resolution (`resolve_focus`), crate-internal.
//! * [`neighbourhood`]: the bounded `inspect` neighbourhood projection
//!   (`render_neighbourhood`), crate-internal.
//!
//! `crate::cli::caps` re-exports the public items, so paths written against
//! it before #25 still resolve.

pub(crate) mod error;
pub(crate) mod focus;
pub mod limits;
pub(crate) mod neighbourhood;
pub mod validate;
