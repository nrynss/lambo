//! Request limits shared by every surface (CLI, MCP, the web portal) and by
//! the write queue.
//!
//! These bounds started on the MCP tools (T8.2). T8.3 made them shared so
//! every surface refuses the same oversized input and none can drift; #25
//! moved them out of `cli::caps` so the core write queue no longer imports
//! from the CLI. `crate::cli::caps` re-exports every item here, so its paths
//! stay valid.

/// Upper bound on `top_k` a client may ask for. Recall assembles and renders
/// every hit, so an unbounded `top_k` from one client is a cheap way to stall
/// the single process every other client shares.
pub const MAX_TOP_K: usize = 100;
/// Upper bound on `traversal_depth` (spec §8 phase 2 is a BFS — depth is an
/// exponent, not a linear cost).
pub const MAX_TRAVERSAL_DEPTH: usize = 5;
/// Upper bound on `max_tokens` for one context block.
pub const MAX_MAX_TOKENS: usize = 100_000;
/// Upper bound on concepts in a single `derive` call.
pub const MAX_CONCEPTS_PER_DERIVE: usize = 64;
/// Upper bound on the combined `produces` + `modifies` + `depends_on` target
/// count in a single `record-action` call.
pub const MAX_ACTION_TARGETS: usize = 64;
/// Upper bound on `reserve` TTL — a soft lock (spec §11), not a lease.
pub const MAX_RESERVE_TTL_SECS: u64 = 3600;
/// Upper bound on `inspect` depth.
pub const MAX_INSPECT_DEPTH: usize = 5;
/// Cap on neighbours rendered by `inspect`, as a **total** across every hop.
///
/// `render_neighbourhood` (`src/cli/inspect.rs`) initialises one budget from
/// this constant *before* the hop loop and decrements it as it renders, so the
/// bound is on the whole rendered neighbourhood, not on each frontier level.
/// (T88-H8: this doc-comment previously said "per frontier level", which the
/// code has never done.)
pub const MAX_INSPECT_NODES: usize = 200;
/// Upper bound on **every** client-supplied string this surface accepts.
///
/// Sized to match `graph::hybrid::MAX_HYBRID_CONTEXT_BYTES` so the surface
/// refuses before the graph does.
pub const MAX_CONTENT_BYTES: usize = 16_384;
/// Candidate concepts listed when `inspect`'s focus is ambiguous.
pub const MAX_INSPECT_CANDIDATES: usize = 10;

/// Clamp a config-derived default into the surface-enforced range.
///
/// A session config can set a `default_top_k` (etc.) wider than the surface
/// maximum; a caller that omits the knob would then inherit a value the
/// surface refuses. Clamp it into `lo..=hi` and log when that changes it.
pub fn clamp_cfg_default(name: &str, value: usize, lo: usize, hi: usize) -> usize {
    let clamped = value.clamp(lo, hi);
    if clamped != value {
        tracing::warn!(
            config_key = name,
            configured = value,
            clamped_to = clamped,
            "session config default is outside the surface bound — using the clamped value"
        );
    }
    clamped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_match_the_t8_2_values() {
        assert_eq!(MAX_TOP_K, 100);
        assert_eq!(MAX_TRAVERSAL_DEPTH, 5);
        assert_eq!(MAX_MAX_TOKENS, 100_000);
        assert_eq!(MAX_CONCEPTS_PER_DERIVE, 64);
        assert_eq!(MAX_ACTION_TARGETS, 64);
        assert_eq!(MAX_RESERVE_TTL_SECS, 3600);
        assert_eq!(MAX_INSPECT_DEPTH, 5);
        assert_eq!(MAX_INSPECT_NODES, 200);
        assert_eq!(MAX_CONTENT_BYTES, 16_384);
        assert_eq!(MAX_INSPECT_CANDIDATES, 10);
    }

    #[test]
    fn clamp_cfg_default_pins_the_bounds() {
        assert_eq!(
            clamp_cfg_default("default_top_k", MAX_TOP_K + 500, 1, MAX_TOP_K),
            MAX_TOP_K
        );
        assert_eq!(clamp_cfg_default("default_top_k", 0, 1, MAX_TOP_K), 1);
        assert_eq!(clamp_cfg_default("default_top_k", 7, 1, MAX_TOP_K), 7);
    }
}
