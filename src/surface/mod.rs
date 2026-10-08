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
//! * `bearer`: the constant-time bearer-token check both HTTP surfaces
//!   (`lambo serve --transport http` and the web portal) enforce,
//!   crate-internal (#28).
//! * `error`: the model-safe class of a `LamboError` (`err_class`, N4),
//!   shared by MCP's tool errors and the write queue's receipts, crate-internal.
//! * `focus`: `inspect` focus resolution (`resolve_focus`), crate-internal.
//! * `neighbourhood`: the bounded `inspect` neighbourhood projection
//!   (`render_neighbourhood`), crate-internal.
//!
//! `crate::cli::caps` re-exports every public item that lived there before
//! #25 (the limits and `check_size`), so those paths still resolve. The
//! crate-private items that moved here (focus resolution and the
//! neighbourhood projection from `cli::inspect`, `err_class` from
//! `mcp::server`) moved without aliases; every in-crate caller was rewritten.

pub(crate) mod bearer;
pub(crate) mod error;
pub(crate) mod focus;
pub mod limits;
pub(crate) mod neighbourhood;
pub mod validate;
