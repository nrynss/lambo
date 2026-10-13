//! Defaults of the read-only canonization dry run (#36 PR 5), outside the
//! `store-memory` gate the dry run itself sits behind, so `lambo canon`'s
//! flags name the same numbers in every build.

use std::time::Duration;

/// Clock step between cycles: the design's 61 seconds, just over the
/// 60-second age floor, so each cycle sees the previous one's edges aged.
pub const DEFAULT_STEP: Duration = Duration::from_secs(61);

/// Floor on consecutive empty cycles before a run stops.
pub const DEFAULT_MIN_QUIET: usize = 30;

/// Safety limit on cycles.
pub const DEFAULT_MAX_CYCLES: usize = 5_000;

/// Ids listed per list.
pub const DEFAULT_TOP: usize = 20;
