//! `lambo_recall`: three-phase recall rendered as the T5.3 context block.
//!
//! #22 PR 6: the vector leg can search by an `image` (embedded here, on the
//! call path, and dropped when the call returns) or a client `query_vector`
//! instead of the text's embedding; the text is then optional. The same
//! wire checks as `lambo_derive_image` run before anything is decoded or
//! embedded, no refusal quotes the payload, nothing about it is cached, and
//! the ledger line names only the payload kind.

use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::json;

use crate::embed::Modalities;
use crate::mcp::server::params::{check_size, RecallParams};
use crate::mcp::server::response::{
    attach_warnings, bad_param, config_refusal, redact_urls, tool_err,
};
use crate::mcp::server::trace::{note_facts, recall_facts};
use crate::mcp::server::LamboServer;
use crate::recall::query_vector::QueryBy;
use crate::surface::image::{check_submitted_vector_as, decode_base64, validate};
use crate::surface::limits::{clamp_cfg_default, MAX_MAX_TOKENS, MAX_TOP_K, MAX_TRAVERSAL_DEPTH};
use crate::surface::validate::{check_in_range, require_nonempty};
use crate::types::{EmbeddingContract, RecallQuery, RecallResult};

const TOOL: &str = "lambo_recall";

impl LamboServer {
    pub(crate) async fn recall_impl(&self, p: RecallParams) -> CallToolResult {
        if let Err(e) = self.check_agent_id(&p.agent_id) {
            return e;
        }
        let mut warnings: Vec<String> = Vec::new();
        // #22 PR 6: at most one of `image` and `query_vector`; with either,
        // the text is optional (blank means none).
        let by_kind = match (&p.image, &p.query_vector) {
            (None, None) => None,
            (Some(_), None) => Some("image"),
            (None, Some(_)) => Some("vector"),
            (Some(_), Some(_)) => {
                return bad_param("send at most one of image or query_vector");
            }
        };
        if by_kind.is_none()
            && let Err(e) = require_nonempty("query", &p.query)
        {
            // The text is optional only beside the other two, so name them.
            return bad_param(format!("{e} (or send image or query_vector)"));
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

        // Beside an image or a vector, blank text is no text.
        let text = if by_kind.is_some() && p.query.trim().is_empty() {
            String::new()
        } else {
            p.query
        };
        let query_text = text.clone();
        let query = RecallQuery {
            query: text,
            top_k,
            max_tokens,
            traversal_depth,
        };
        // `recall_detailed` is the SAME execution `recall` projects from (it is
        // what `recall` calls); taking the detailed view here is what lets the
        // I1 ledger record per-leg scores and typed warning kinds. The response
        // below is built from the projection and is unchanged by this.
        // The decoded bytes must outlive the borrow `QueryBy::Image` holds,
        // so they live here.
        let bytes;
        let by = match (p.image, p.query_vector) {
            (Some(img), None) => {
                if let Err(e) = self.recall_by_preconditions(true) {
                    return e;
                }
                // The length cap runs before any decoding: stdio has no
                // transport cap of its own.
                bytes = match decode_base64(&img.data) {
                    Ok(b) => b,
                    Err(msg) => return bad_param(msg),
                };
                match validate(&bytes, &img.mime) {
                    Ok(input) => Some(QueryBy::Image(input)),
                    Err(msg) => return bad_param(msg),
                }
            }
            (None, Some(vector)) => {
                if let Err(e) = self.recall_by_preconditions(false) {
                    return e;
                }
                let declared = EmbeddingContract {
                    kind: vector.contract.kind,
                    model: vector.contract.model,
                    dim: vector.contract.dim,
                };
                // AC4 at the wire: names the fields that differ and never
                // quotes the submitted ones.
                if let Err(msg) = check_submitted_vector_as(
                    "query_vector",
                    &vector.values,
                    &declared,
                    self.mem.embedding_contract(),
                ) {
                    return bad_param(msg);
                }
                Some(QueryBy::Vector {
                    values: vector.values,
                    declared,
                })
            }
            // Neither (a text recall); both was refused above.
            _ => None,
        };
        let detailed = match by {
            None => self.mem.recall_detailed(query).await,
            Some(by) => self.mem.recall_by_detailed(query, by).await,
        };
        let detailed = match detailed {
            Ok(r) => r,
            Err(e) => return tool_err(TOOL, e),
        };
        // I1: the text recall's facts, plus which payload kind a recall by
        // image or vector sent. Never the bytes or a component.
        note_facts(|| {
            let mut facts = recall_facts(&query_text, top_k, &detailed);
            if let Some(kind) = by_kind
                && let Some(map) = facts.as_object_mut()
            {
                map.insert("by".into(), json!(kind));
            }
            facts
        });
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

    /// The deployment preconditions of a recall by image (`image`) or by a
    /// client vector, refused with the setting named so the caller (or its
    /// operator) can act: a store with vector search, then an embedder that
    /// embeds images, or the operator's client-vector opt-in.
    fn recall_by_preconditions(&self, image: bool) -> Result<(), CallToolResult> {
        if !self.mem.vector_candidates().available() {
            return Err(config_refusal(
                TOOL,
                "a recall by image or by vector needs a store with vector search \
                 (VECTOR_SEARCH), and this server's store does not search vectors",
            ));
        }
        if image {
            if !self.mem.embedder().modalities().contains(Modalities::IMAGE) {
                return Err(config_refusal(
                    TOOL,
                    "this server's embedder does not embed images; send a query_vector \
                     computed in this session's embedding space instead (which needs \
                     [embedder] accept_client_vectors = true)",
                ));
            }
        } else if !self.mem.config().accept_client_vectors {
            return Err(config_refusal(
                TOOL,
                "this server does not accept client-computed vectors; the operator \
                 enables them with [embedder] accept_client_vectors = true (or \
                 LAMBO_ACCEPT_CLIENT_VECTORS=true)",
            ));
        }
        Ok(())
    }
}
