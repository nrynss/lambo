//! Per-contract score calibration (#87).
//!
//! Lambo's absolute score constants were set against BGE-M3's cosine scale:
//! the recent leg's flat [`RECENT_SCORE`](crate::recall::candidates::RECENT_SCORE)
//! (0.35), the layered-recall floor
//! [`DEFAULT_LAYER_MIN_SIMILARITY`](crate::recall::layered::DEFAULT_LAYER_MIN_SIMILARITY)
//! (0.50) and the semantic merge threshold
//! [`SEMANTIC_MATCH_THRESHOLD_DEFAULT`](crate::graph::hybrid::SEMANTIC_MATCH_THRESHOLD_DEFAULT)
//! (0.85). Another embedder puts the same texts on another scale.
//! EmbeddingGemma 2 scores a query against an *irrelevant* concept at a median
//! of 0.63, so every one of its vector hits outranks a 0.35 recent-leg member.
//!
//! # Method: quantile matching against the reference contract
//!
//! BGE-M3 is the reference, so its constants map to themselves and no BGE-M3
//! ranking moves. For another contract `X` and a BGE-M3 constant `c`, measured
//! on the same labelled texts (the #190 set, `fixtures/eval/`), **dev split
//! only**:
//!
//! * `q = F_bge(c)`, the share of the reference null distribution at or below
//!   `c`;
//! * `c_X = F_X⁻¹(q)`, the same quantile of `X`'s null distribution.
//!
//! So each constant keeps the *rank position* it was chosen for, which is what
//! the constants' own documentation pins (the value of `RECENT_SCORE` "is not
//! spec-pinned, only its rank position relative to real matches"). The
//! held-out split is only measured, never tuned on.
//!
//! | constant | null distribution |
//! |---|---|
//! | recent score, layer floor | cosine of a query to a concept graded irrelevant |
//! | merge threshold | cosine of a labelled related-but-distinct pair |
//!
//! Vector scores themselves are **not** rescaled. A `(cos − floor) / (1 −
//! floor)` map would move every reported cosine, the layer floors, the dedupe
//! and the BGE-M3 goldens; moving the few constants that compete with a cosine
//! keeps BGE-M3 exact and leaves every reported cosine as it was (the recent
//! leg's own score, in a recall's leg detail, is the one value that moves).
//! Fusing BM25 with cosine on one scale is #173's (rank fusion), not this.
//!
//! # What applies by default
//!
//! * **Recent score**: on, for a text recall whose vector leg runs (the
//!   rows were measured on text queries, so a recall by an image or a client
//!   vector keeps 0.35). `[embedder] calibration = "off"` keeps 0.35.
//!   It keeps BGE-M3's rank position: below every true hit on the set, and
//!   above about half of all irrelevant concepts (none before). It does not
//!   lift recency above the irrelevant hits a recall returns: BGE-M3's 0.35
//!   never did either, and any value that did would also outrank true hits
//!   (the trade-off table in the evidence README).
//! * **Merge threshold**: on (owner decision on #203). A writer attach under
//!   a contract with a row merges at the row's threshold instead of
//!   `semantic_match_threshold`, so an existing EG2 store merges less after
//!   the upgrade (0.93 instead of 0.85). `[embedder]
//!   calibrated_merge_threshold = false` restores the configured threshold,
//!   and a writer attach then logs a warning when the configured one is
//!   lower. A usable `related_pairs` derivation (#176) still wins over both.
//! * **Layer floor**: on (owner decision, the follow-up to #203). A layer
//!   whose `min_similarity` nobody set (no `[[serve.layer]] min_similarity`,
//!   no [`LayerRef::with_min_similarity`](crate::LayerRef::with_min_similarity))
//!   takes the row's floor instead of BGE-M3's 0.50. It is resolved at
//!   recall time from the calling session's contract (a layer must share
//!   it), because a layer's knobs are read before any embedder is known. A
//!   floor the user set always wins. `[embedder] calibrated_layer_floor =
//!   false`, or `calibration = "off"`, keeps 0.50.
//!
//! A contract with no row (any other model string, prompt profile, dimension
//! or kind) keeps the BGE-M3 constants, as before. The evidence for each row is
//! under `evidence/issue-87-calibration/`.

use serde::{Deserialize, Serialize};

use crate::types::EmbeddingContract;

/// `[embedder] calibration`: whether the built-in per-contract recall
/// constants apply.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CalibrationMode {
    /// Use the contract's row of [`BUILTIN`], when it has one.
    #[default]
    Builtin,
    /// The BGE-M3 constants for every contract (lambo before #87).
    Off,
}

impl std::fmt::Display for CalibrationMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Builtin => "builtin",
            Self::Off => "off",
        })
    }
}

/// One contract's measured constants. Every value is a cosine in `[0, 1]`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ContractCalibration {
    /// [`EmbeddingContract::kind`].
    pub kind: &'static str,
    /// [`EmbeddingContract::model`], exactly.
    pub model: &'static str,
    /// [`EmbeddingContract::dim`].
    pub dim: usize,
    /// The recent leg's flat phase-1 score (BGE-M3: 0.35).
    pub recent_score: f64,
    /// The layered-recall similarity floor equivalent to 0.50 under BGE-M3.
    /// Applied by default to a layer whose floor nobody set (module docs).
    pub layer_min_similarity: f64,
    /// The merge threshold equivalent to BGE-M3 at 0.85. Applied by default
    /// (`calibrated_merge_threshold = false` switches it off).
    pub merge_threshold: f64,
    /// Share of the set's related-but-distinct pairs at or above the
    /// *default* 0.85 under this contract, by cosine (before #193's
    /// identifier guard), for the attach warning.
    pub default_threshold_merges: f64,
    /// Where the numbers come from.
    pub evidence: &'static str,
}

/// The EG2 contract measured for #87.
const EG2_Q8_768: ContractCalibration = ContractCalibration {
    kind: "embeddinggemma2",
    model: "ggml-org/embeddinggemma-2-GGUF@bfcd2987/Q8_0;prompts=lambo-eg2-v2",
    dim: 768,
    recent_score: 0.63,
    layer_min_similarity: 0.72,
    merge_threshold: 0.93,
    default_threshold_merges: 0.78,
    evidence: "evidence/issue-87-calibration/",
};

/// Every built-in row. BGE-M3 has none: it is the reference.
pub const BUILTIN: &[ContractCalibration] = &[EG2_Q8_768];

/// The built-in row for exactly this contract, if any.
pub fn lookup(contract: &EmbeddingContract) -> Option<&'static ContractCalibration> {
    BUILTIN.iter().find(|row| {
        row.kind == contract.kind
            && contract.model.as_deref() == Some(row.model)
            && row.dim == contract.dim
    })
}

/// The recent leg's score for a text recall whose vector leg runs under
/// `contract`. `None` (no vector leg) keeps the BGE-M3 value: the recent leg
/// then competes only with BM25, as it always has. The daemon calls this for
/// text recalls only; the rows were measured on text queries.
pub fn recent_score(mode: CalibrationMode, contract: Option<&EmbeddingContract>) -> f64 {
    match (mode, contract.and_then(lookup)) {
        (CalibrationMode::Builtin, Some(row)) => row.recent_score,
        _ => crate::recall::candidates::RECENT_SCORE,
    }
}

/// `[embedder] calibrated_layer_floor`'s default: on.
pub fn default_calibrated_layer_floor() -> bool {
    true
}

/// The floor of a layer whose `min_similarity` nobody set, in a layered
/// recall whose calling session runs under `contract`: the row's floor when
/// `mode` is `builtin`, `enabled` (`[embedder] calibrated_layer_floor`, on
/// by default) and the contract has a row; otherwise BGE-M3's
/// [`DEFAULT_LAYER_MIN_SIMILARITY`](crate::recall::layered::DEFAULT_LAYER_MIN_SIMILARITY).
/// A floor the user set is never passed through here.
pub fn layer_min_similarity(
    mode: CalibrationMode,
    enabled: bool,
    contract: &EmbeddingContract,
) -> f64 {
    match (mode, enabled, lookup(contract)) {
        (CalibrationMode::Builtin, true, Some(row)) => row.layer_min_similarity,
        _ => crate::recall::layered::DEFAULT_LAYER_MIN_SIMILARITY,
    }
}

/// A layer's effective floor: `configured` (a floor the user set) when
/// `Some`, else [`layer_min_similarity`] under `config`'s switches.
pub(crate) fn resolve_layer_floor(
    configured: Option<f64>,
    config: &crate::Config,
    contract: &EmbeddingContract,
) -> f64 {
    configured.unwrap_or_else(|| {
        layer_min_similarity(config.calibration, config.calibrated_layer_floor, contract)
    })
}

/// Why a layer's effective floor sits below its contract's calibrated one,
/// if it does: a floor the user set lower (the old documentation's example
/// set 0.50), or the calibration switched off. `None` when the floor is the
/// calibrated one or higher, or the contract has no row. A serve logs this
/// when it builds a layered session's server.
pub fn layer_floor_warning(
    layer: &str,
    configured: Option<f64>,
    config: &crate::Config,
    contract: &EmbeddingContract,
) -> Option<String> {
    let row = lookup(contract)?;
    let floor = resolve_layer_floor(configured, config, contract);
    if floor >= row.layer_min_similarity {
        return None;
    }
    let why = match configured {
        Some(set) => format!("[[serve.layer]] {layer:?} sets min_similarity = {set}"),
        None => format!(
            "layer {layer:?} keeps BGE-M3's floor {floor}: [embedder] {}",
            if config.calibration == CalibrationMode::Off {
                "calibration = \"off\""
            } else {
                "calibrated_layer_floor = false"
            }
        ),
    };
    let switched_off =
        !(config.calibration == CalibrationMode::Builtin && config.calibrated_layer_floor);
    let fix = match (configured.is_some(), switched_off) {
        (true, false) => "Remove that line",
        (true, true) => {
            "Remove that line and turn the calibration back on (remove `[embedder] calibration \
             = \"off\"` and `calibrated_layer_floor = false`)"
        }
        (false, _) => {
            "Turn the calibration back on (remove `[embedder] calibration = \"off\"` and \
             `calibrated_layer_floor = false`)"
        }
    };
    Some(format!(
        "{why}, below the {} calibrated for this embedding contract, so the layer admits more \
         irrelevant concepts than 0.50 does under BGE-M3. {fix} to use the calibrated floor \
         (evidence: {})",
        row.layer_min_similarity, row.evidence,
    ))
}

/// `[embedder] calibrated_merge_threshold`'s default: on.
pub fn default_calibrated_merge_threshold() -> bool {
    true
}

/// The merge threshold a reader of a session under `contract` uses (#181),
/// in the writer attach's order: `derived`, the labelled-pairs threshold the
/// store caches for the configured pairs file
/// ([`super::canary::cached_merge_threshold`]); else the contract's
/// calibrated one when `enabled` and it has a row; else the configured one.
/// Like [`merge_threshold`] without its warning, for a path that runs per
/// recall.
pub(crate) fn reader_merge_threshold(
    derived: Option<f64>,
    enabled: bool,
    contract: &EmbeddingContract,
    configured: f64,
) -> f64 {
    derived.unwrap_or(match lookup(contract) {
        Some(row) if enabled => row.merge_threshold,
        _ => configured,
    })
}

/// The built-in merge threshold for a writer attach that did not derive one
/// from labelled pairs. `Some` when `enabled` (the default) and the contract
/// has a row; otherwise the configured threshold stays, with a warning when
/// the contract is known to merge far more under it than BGE-M3 does.
pub fn merge_threshold(
    enabled: bool,
    contract: &EmbeddingContract,
    configured: f64,
) -> Option<f64> {
    let row = lookup(contract)?;
    if enabled {
        return Some(row.merge_threshold);
    }
    if configured < row.merge_threshold {
        tracing::warn!(
            embedder = %contract.kind,
            model = row.model,
            dim = row.dim,
            configured,
            calibrated = row.merge_threshold,
            "[embedder] calibrated_merge_threshold = false: merging at semantic_match_threshold \
             {configured}, which was calibrated on BGE-M3. Under this embedding contract {:.0}% \
             of the labelled related-but-distinct pairs score 0.85 or more (BGE-M3: 35-44%). \
             Remove the setting to merge at the calibrated {} (evidence: {})",
            row.default_threshold_merges * 100.0,
            row.merge_threshold,
            row.evidence,
        );
    }
    None
}

#[cfg(test)]
#[path = "calibration_tests.rs"]
mod tests;
