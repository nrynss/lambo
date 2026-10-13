//! Read-only session classification and reviewable migration plans (#33),
//! the offline split that writes a validated plan's target sessions, and the
//! before/after recall comparison that verifies a split.

mod classify;
mod plan;
mod signals;
mod split;
mod verify;

pub use classify::{classify, classify_reviewed};
pub use plan::{
    validate_plan, Assignment, ClassificationPlan, ClassificationRules, ClassifyError, Confidence,
    ItemKind, ItemRef, OperatorOverride, ProjectRule, ReviewedAssignment, SourceBinding,
    PLAN_VERSION,
};
pub use split::{
    build_split, canonical_target, target_sha256, DropReason, EdgeAccounting, EdgeDisposition,
    LedgerEntry, LowConfidenceItem, Placement, SplitAccounting, SplitError, SplitOutput,
    SplitTarget, StubbedReplica, TargetCounts, NEW_CHAIN_LINK_WEIGHT, SPLIT_ALGORITHM_VERSION,
};
pub use verify::{
    newest_row, verify_split, ProjectSummary, QueryComparison, RankShift, VerifyError, VerifyHit,
    VerifyInputs, VerifyParams, VerifyQueries, VerifyReport, MAX_QUERIES_PER_PROJECT,
    MAX_QUERY_BYTES, VERIFY_REPORT_VERSION,
};

#[cfg(test)]
pub(crate) mod split_tests;
#[cfg(test)]
pub(crate) mod tests;
