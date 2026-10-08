//! `lambo_recall`: three-phase recall rendered as the T5.3 context block.

use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::json;

use crate::mcp::server::params::{check_size, RecallParams};
use crate::mcp::server::response::{attach_warnings, bad_param, redact_urls, tool_err};
use crate::mcp::server::trace::{note_facts, recall_facts};
use crate::mcp::server::LamboServer;
use crate::surface::limits::{clamp_cfg_default, MAX_MAX_TOKENS, MAX_TOP_K, MAX_TRAVERSAL_DEPTH};
use crate::surface::validate::{check_in_range, require_nonempty};
use crate::types::{RecallQuery, RecallResult};

impl LamboServer {
    pub(crate) async fn recall_impl(&self, p: RecallParams) -> CallToolResult {
        if let Err(e) = self.check_agent_id(&p.agent_id) {
            return e;
        }
        let mut warnings: Vec<String> = Vec::new();
        if let Err(e) = require_nonempty("query", &p.query) {
            return bad_param(e);
        }
        if let Err(e) = check_size("query", &p.query) {
            return e;
        }
        let cfg = self.mem.config();
        // N6: when the client omits a knob it inherits the session config's
        // default, which is not bound by the MCP maxima. A config wider than the
        // MCP cap (e.g. `default_top_k` above `MAX_TOP_K`) would otherwise make
        // the tool refuse a request that named nothing wrong. Clamp the
        // config-derived default into range with a logged warning; an *explicit*
        // out-of-range value from the client is still a client error and is
        // rejected below.
        let top_k = match p.top_k {
            Some(v) => v,
            None => clamp_cfg_default("default_top_k", cfg.default_top_k, 1, MAX_TOP_K),
        };
        let max_tokens = match p.max_tokens {
            Some(v) => v,
            None => clamp_cfg_default(
                "default_max_tokens",
                cfg.default_max_tokens,
                1,
                MAX_MAX_TOKENS,
            ),
        };
        let traversal_depth = match p.traversal_depth {
            Some(v) => v,
            None => clamp_cfg_default(
                "default_traversal_depth",
                cfg.default_traversal_depth,
                0,
                MAX_TRAVERSAL_DEPTH,
            ),
        };
        if let Err(e) = check_in_range("top_k", top_k, 1, MAX_TOP_K)
            .and_then(|()| {
                check_in_range("traversal_depth", traversal_depth, 0, MAX_TRAVERSAL_DEPTH)
            })
            .and_then(|()| check_in_range("max_tokens", max_tokens, 1, MAX_MAX_TOKENS))
        {
            return bad_param(e);
        }

        let query_text = p.query.clone();
        let query = RecallQuery {
            query: p.query,
            top_k,
            max_tokens,
            traversal_depth,
        };
        // `recall_detailed` is the SAME execution `recall` projects from (it is
        // what `recall` calls); taking the detailed view here is what lets the
        // I1 ledger record per-leg scores and typed warning kinds. The response
        // below is built from the projection and is unchanged by this.
        let detailed = match self.mem.recall_detailed(query).await {
            Ok(r) => r,
            Err(e) => return tool_err("lambo_recall", e),
        };
        note_facts(|| recall_facts(&query_text, top_k, &detailed));
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
    }
}
