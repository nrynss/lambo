//! Model-safe classification of a [`LamboError`] (N4), shared by every path
//! that tells a caller a write or a tool call failed.
//!
//! The full error can carry a DSN, a store URL, a file path or a driver
//! message; a caller (often a model) gets only the class, and the detail goes
//! to the operator's log at the call site.

use crate::types::LamboError;

/// A short, detail-free class for a `Memory` failure (N4).
///
/// The full error can interpolate a DSN, a store URL, a file path or a driver
/// message — none of which the model needs and any of which is worth keeping
/// out of a model-facing string. Return the class; the detail is logged.
///
/// Shared since JE2E-12, so `writeq`'s async write path renders the same class
/// MCP's synchronous path (`mcp::server`'s `tool_err`) does. The alternative was
/// a second match in `writeq.rs`, which is how a new `LamboError` variant would
/// come to have one class on the sync path and another on the async one — the
/// drift this function exists to prevent, reintroduced one module over. It
/// lives here rather than in `mcp::server` (#25) so the core write queue does
/// not import the MCP surface for it. The old `crate::mcp::server::err_class`
/// path was crate-private and is gone since #25 split `mcp::server`: nothing
/// used it once every caller imported this module directly.
pub(crate) fn err_class(err: &LamboError) -> &'static str {
    match err {
        LamboError::Store(_) => "store error",
        // J3 round-1 N1: same pairing as the Conflict/SoftLock one below. The
        // split lets the durable-intent replay tell "not reached" from "refused
        // this input"; it must not give the model, the operator or the ledger a
        // new error class to learn.
        LamboError::Embed(_) | LamboError::EmbedUnavailable(_) => "embedding error",
        LamboError::Config(_) => "configuration error",
        // J1-R2-2: two variants, one class. The split exists so N4 can tell a
        // model-safe §11 refusal from a lease-lost one; an operator and the
        // ledger see the same `error_kind` either way, so nothing downstream of
        // this function moved.
        LamboError::Conflict(_) => "conflict",
        LamboError::SoftLock(_) => "conflict",
        LamboError::Other(_) => "internal error",
    }
}
