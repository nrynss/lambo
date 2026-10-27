//! How phase 1 combines its keyword, vector and recent legs into one
//! query-relevance value (#173).
//!
//! The legs score on different scales: BM25 is unbounded and routinely
//! above 1, a cosine is at most 1, and the recent leg is a flat constant
//! (0.35, or a contract's calibrated value, #87). [`FusionKind::Max`] (the
//! default) keeps the highest raw score, so the leg with the biggest unit
//! wins. [`FusionKind::Rrf`] is reciprocal rank fusion: each leg ranks its
//! own members (score descending, the issue-2 tie-break), and a node's
//! relevance is
//!
//! ```text
//! Σ_leg w_leg / (k + rank_leg)  /  Σ_active w_leg / (k + 1)
//! ```
//!
//! over the legs it is in, divided by the most that the legs which produced
//! a candidate in this recall can give (rank 1 in every one), so it is in
//! `[0, 1]` and `final = daemon × w_daemon + relevance × w_query` keeps its
//! shape. Under RRF the recent leg is a real leg, ranked by recency (newest
//! interaction first), instead of a flat floor: its calibrated score (#87)
//! is still recorded, but no longer ranks anything.
//!
//! Both values are computed on every recall ([`super::candidates::LegScores`]
//! carries each leg's rank, `max` and `rrf`), so the ledger can shadow-score
//! one kind while the other ranks. The evidence for the defaults is in
//! `evidence/issue-173-fusion/`.

use serde::{Deserialize, Serialize};

use crate::types::LamboError;

/// Which value phase 1 ranks on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FusionKind {
    /// The highest raw leg score (lambo before #173).
    #[default]
    Max,
    /// Reciprocal rank fusion over the legs' ranks.
    Rrf,
}

impl std::fmt::Display for FusionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Max => "max",
            Self::Rrf => "rrf",
        })
    }
}

/// The RRF constant `k` by default.
pub const DEFAULT_RRF_K: f64 = 60.0;
/// The keyword and vector legs' default RRF weight.
pub const DEFAULT_LEG_WEIGHT: f64 = 1.0;
/// The recent leg's default RRF weight.
pub const DEFAULT_RECENT_WEIGHT: f64 = 0.3;

/// `[fusion]`: how phase 1 combines its legs (#173).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecallFusion {
    /// `max` (the default) or `rrf`.
    #[serde(default)]
    pub kind: FusionKind,
    /// RRF's `k`: finite and at least 0. Larger values flatten the gap
    /// between ranks; with the daemon blend it also sets how much a rank
    /// difference moves the final score.
    #[serde(default = "default_k")]
    pub k: f64,
    /// The keyword leg's RRF weight. Finite and at least 0.
    #[serde(default = "default_leg_weight")]
    pub keyword_weight: f64,
    /// The vector leg's RRF weight. Finite and at least 0.
    #[serde(default = "default_leg_weight")]
    pub vector_weight: f64,
    /// The recent leg's RRF weight. Finite and at least 0.
    #[serde(default = "default_recent_weight")]
    pub recent_weight: f64,
}

fn default_k() -> f64 {
    DEFAULT_RRF_K
}
fn default_leg_weight() -> f64 {
    DEFAULT_LEG_WEIGHT
}
fn default_recent_weight() -> f64 {
    DEFAULT_RECENT_WEIGHT
}

impl Default for RecallFusion {
    fn default() -> Self {
        Self {
            kind: FusionKind::Max,
            k: DEFAULT_RRF_K,
            keyword_weight: DEFAULT_LEG_WEIGHT,
            vector_weight: DEFAULT_LEG_WEIGHT,
            recent_weight: DEFAULT_RECENT_WEIGHT,
        }
    }
}

impl RecallFusion {
    /// The `Config` refusals: a non-finite or negative `k` or weight, or
    /// RRF with every weight 0 (it would rank nothing).
    pub fn validate(&self) -> Result<(), LamboError> {
        let bad = |name: &str, v: f64| {
            LamboError::Config(format!(
                "[fusion] {name} must be finite and at least 0, got {v}"
            ))
        };
        for (name, v) in [
            ("k", self.k),
            ("keyword_weight", self.keyword_weight),
            ("vector_weight", self.vector_weight),
            ("recent_weight", self.recent_weight),
        ] {
            if !v.is_finite() || v < 0.0 {
                return Err(bad(name, v));
            }
        }
        if self.kind == FusionKind::Rrf
            && self.keyword_weight + self.vector_weight + self.recent_weight == 0.0
        {
            return Err(LamboError::Config(
                "[fusion] kind = \"rrf\" needs at least one leg weight above 0".into(),
            ));
        }
        Ok(())
    }

    /// One leg's RRF term for a member at 1-based `rank`.
    pub(crate) fn term(&self, weight: f64, rank: usize) -> f64 {
        weight / (self.k + rank as f64)
    }
}

#[cfg(test)]
#[path = "fusion_tests.rs"]
mod tests;
