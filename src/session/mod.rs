//! Read-only session classification and reviewable migration plans (#33).

mod classify;
mod plan;
mod signals;

pub use classify::{classify, classify_reviewed};
pub use plan::{
    validate_plan, Assignment, ClassificationPlan, ClassificationRules, ClassifyError, Confidence,
    ItemKind, ItemRef, OperatorOverride, ProjectRule, ReviewedAssignment, SourceBinding,
    PLAN_VERSION,
};

#[cfg(test)]
pub(crate) mod tests;
