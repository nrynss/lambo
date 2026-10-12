//! #33 PR 3 review L4: `lambo_recall`'s result projection as tagged in
//! v0.5.0, frozen. The body between the markers is copied verbatim from
//! `git show v0.5.0:src/mcp/server/tools/recall.rs` (lines 175-210, from
//! the `RecallResult` projection to the returned result); only the
//! function around it is added, so the copy compiles against today's
//! helpers (`redact_urls`, `attach_warnings`), which kept their 0.5.0
//! behaviour. Never edit the body: it is the golden.
//!
//! Not frozen: the body begins with `detailed.into()`, which is today's
//! `From<DetailedRecall> for RecallResult`. A regression inside that
//! conversion would change both sides of the comparison equally; the
//! context and hit fields are still compared against `recall_detailed`.
//!
//! v0.5.0 predates #130, whose `truncated_in_context` key is the one
//! intended change to this projection since the tag, so this reference
//! holds only for an answer whose first block fits the budget.

use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::json;

use crate::mcp::server::response::{attach_warnings, redact_urls};
use crate::recall::detail::DetailedRecall;
use crate::types::RecallResult;

/// The v0.5.0 `lambo_recall` result for `detailed`.
#[rustfmt::skip]
pub(in crate::mcp::server) fn v0_5_0_recall_result(detailed: DetailedRecall) -> CallToolResult {
    let mut warnings: Vec<String> = Vec::new();
    // ---- v0.5.0 body begins ----
    let result: RecallResult = detailed.into();
    // These include `Memory::recall`'s embed-failure degradation warning —
    // the signal that a recall dropped its vector leg and returned
    // keyword-only hits. `attach_warnings` is what puts it where the model
    // can see it (R1/T82-9). Redact URLs as they enter the vec (N3) so both
    // the text content and the structured `warnings` are clean; the raw
    // detail is logged for the operator.
    for w in &result.warnings {
        tracing::debug!(warning = %w, "mcp: recall degradation warning (raw, pre-redaction)");
    }
    warnings.extend(result.warnings.iter().map(|w| redact_urls(w)));

    let hits: Vec<_> = result
        .hits
        .iter()
        .map(|h| {
            json!({
                "node_id": h.node_id.0.to_string(),
                "content": h.content,
                "concept_type": h.concept_type,
                "score": h.score,
                "is_canonical": h.is_canonical,
                "blast_radius": h.blast_radius,
            })
        })
        .collect();

    // `content[0]` stays the context block verbatim; warnings follow it.
    let mut out = CallToolResult::success(vec![ContentBlock::text(result.context.clone())]);
    attach_warnings(&mut out, &warnings);
    out.structured_content = Some(json!({
        "context": result.context,
        "hits": hits,
        "warnings": warnings,
    }));
    out
    // ---- v0.5.0 body ends ----
}
