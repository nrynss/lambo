//! Read-only session classification and reviewable migration plans (#33),
//! and the offline split that writes a validated plan's target sessions.

mod classify;
mod plan;
mod signals;
mod split;

pub use classify::{classify, classify_reviewed};
pub use plan::{
    validate_plan, Assignment, ClassificationPlan, ClassificationRules, ClassifyError, Confidence,
    ItemKind, ItemRef, OperatorOverride, ProjectRule, ReviewedAssignment, SourceBinding,
    PLAN_VERSION,
};
pub use split::{
    build_split, canonical_target, target_sha256, EdgeAccounting, EdgeDisposition, LedgerEntry,
    LowConfidenceItem, Placement, SplitAccounting, SplitError, SplitOutput, SplitTarget,
    TargetCounts, NEW_CHAIN_LINK_WEIGHT, SPLIT_ALGORITHM_VERSION,
};

#[cfg(test)]
pub(crate) mod split_tests;
#[cfg(test)]
pub(crate) mod tests;
