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
use crate::mcp::server::layers::{LayerHandle, LiveLayer};
use crate::mcp::server::params::{check_size, RecallParams};
use crate::mcp::server::response::{
    attach_warnings, bad_param, config_refusal, layer_unavailable, redact_urls, tool_err,
};
use crate::mcp::server::trace::{grant, layered_recall_facts, note_facts, recall_facts};
use crate::mcp::server::LamboServer;
use crate::memory::{LayerRef, LayerSlot, LayeredRecall, Memory};
use crate::recall::query_vector::QueryBy;
use crate::surface::image::{check_submitted_vector_as, decode_base64, validate};
use crate::surface::limits::{clamp_cfg_default, MAX_MAX_TOKENS, MAX_TOP_K, MAX_TRAVERSAL_DEPTH};
use crate::surface::validate::{check_in_range, require_nonempty};
use crate::types::{EmbeddingContract, RecallQuery};

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

        // #33 PR 3: which session(s) this recall reads. Resolved before the
        // payload is decoded, so a bad scope costs nothing.
        let target = match self.recall_target(p.scope.as_deref()) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let scope = p.scope;
        let mem: &Memory = match &target {
            RecallTarget::Layer(live) => &live.mem,
            RecallTarget::Own | RecallTarget::Stack => &self.mem,
        };

        // Beside an image or a vector, blank text is no text.
        let text = if by_kind.is_some() && crate::surface::validate::is_blank(&p.query) {
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
                if let Err(e) = recall_by_preconditions(mem, true) {
                    return e;
                }
                // The length cap runs before any decoding. A frame under
                // the transports' 4 MiB frame cap (#101) can still carry
                // base64 longer than an image may be.
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
                if let Err(e) = recall_by_preconditions(mem, false) {
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
                    mem.embedding_contract(),
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
        let recalled = match (by, &target) {
            // A layered session's text recall: its own session and its
            // layers, merged (design section 2).
            (None, RecallTarget::Stack) => {
                let live: Vec<Option<LiveLayer>> = self
                    .layers()
                    .handles
                    .iter()
                    .map(LayerHandle::live)
                    .collect();
                let slots: Vec<LayerSlot<'_>> = self
                    .layers()
                    .handles
                    .iter()
                    .zip(&live)
                    .map(|(handle, live)| match live {
                        Some(live) => LayerSlot::Live(
                            LayerRef::new(&live.mem)
                                .with_weight(handle.weight)
                                .with_min_similarity(handle.min_similarity),
                        ),
                        None => LayerSlot::Unavailable {
                            session: &handle.session,
                            weight: handle.weight,
                            min_similarity: handle.min_similarity,
                        },
                    })
                    .collect();
                self.mem.recall_layered_slots(&slots, query).await
            }
            (None, _) => mem
                .recall_detailed(query)
                .await
                .map(|d| LayeredRecall::single(mem.session(), d)),
            (Some(by), _) => mem
                .recall_by_detailed(query, by)
                .await
                .map(|d| LayeredRecall::single(mem.session(), d)),
        };
        let mut recalled = match recalled {
            Ok(r) => r,
            Err(e) => return tool_err(TOOL, e),
        };
        // A recall by image or vector reads one session (the library's
        // layered recall is by text): a layered session says its layers
        // were not searched.
        if by_kind.is_some() && matches!(target, RecallTarget::Stack) {
            recalled.warnings.push(format!(
                "recall: a recall by image or by vector reads this session only; its layers \
                 ({}) were not searched. Pass scope to read a layer.",
                self.layer_names().join(", ")
            ));
        }
        // #33: every hit of a layered session names its session, whatever
        // the scope; an unlayered session's hits never do (byte-identical).
        let layered = !self.layers().is_empty();
        // I1: the text recall's facts, plus which payload kind a recall by
        // image or vector sent. Never the bytes or a component.
        note_facts(|| {
            let mut facts = if layered {
                layered_recall_facts(&query_text, top_k, &recalled)
            } else {
                recall_facts(&query_text, top_k, &recalled.detail)
            };
            if let Some(map) = facts.as_object_mut() {
                if let Some(kind) = by_kind {
                    map.insert("by".into(), json!(kind));
                }
                // #33: additive, only when the caller sent a scope.
                if let Some(scope) = &scope {
                    map.insert("scope".into(), json!(scope));
                }
            }
            facts
        });
        // These include `Memory::recall`'s embed-failure degradation warning —
        // the signal that a recall dropped its vector leg and returned
        // keyword-only hits. `attach_warnings` is what puts it where the model
        // can see it (R1/T82-9). Redact URLs as they enter the vec (N3) so both
        // the text content and the structured `warnings` are clean; the raw
        // detail is logged for the operator.
        for w in &recalled.warnings {
            tracing::debug!(warning = %w, "mcp: recall degradation warning (raw, pre-redaction)");
        }
        warnings.extend(recalled.warnings.iter().map(|w| redact_urls(w)));

        let hits: Vec<_> = recalled
            .hits
            .iter()
            .map(|lh| {
                let h = &lh.hit;
                let mut hit = json!({
                    "node_id": h.node_id.0.to_string(),
                    "content": h.content,
                    "concept_type": h.concept_type,
                    "score": h.score,
                    "is_canonical": h.is_canonical,
                    "blast_radius": h.blast_radius,
                });
                if let Some(map) = hit.as_object_mut() {
                    // #130: additive, on the truncated first hit only, so every
                    // other response is byte-identical.
                    if lh.truncated_in_context {
                        map.insert("truncated_in_context".into(), json!(true));
                    }
                    // #33: additive, on every hit of a layered session only.
                    if layered {
                        map.insert("session".into(), json!(lh.session.as_str()));
                    }
                }
                hit
            })
            .collect();

        // `content[0]` stays the context block verbatim; warnings follow it.
        // For a layered recall it is the merged context, which alone renders
        // a layer hit's `from <session>` meta (the merge is its one renderer).
        let mut out = CallToolResult::success(vec![ContentBlock::text(recalled.context.clone())]);
        attach_warnings(&mut out, &warnings);
        out.structured_content = Some(json!({
            "context": recalled.context,
            "hits": hits,
            "warnings": warnings,
        }));
        out
    }

    /// The configured layers' names, nearest tier first.
    fn layer_names(&self) -> Vec<&str> {
        self.layers()
            .handles
            .iter()
            .map(|h| h.session.as_str())
            .collect()
    }

    /// Which session(s) a recall with `scope` reads (#33 PR 3, design
    /// sections 2.6 and 3).
    ///
    /// No scope: this session, with its layers when it has any. A scope
    /// naming this session: this session alone. A scope naming one of its
    /// layers: that layer alone, looked up now; refused, retryably, while
    /// it is not serving. Anything else is `bad_param`, listing only this
    /// session's own scopes and never quoting the value sent.
    fn recall_target(&self, scope: Option<&str>) -> Result<RecallTarget, CallToolResult> {
        let Some(scope) = scope else {
            if self.layers().is_empty() {
                return Ok(RecallTarget::Own);
            }
            for name in self.layer_names() {
                self.authorize_layer(name)?;
            }
            return Ok(RecallTarget::Stack);
        };
        check_size("scope", scope)?;
        if scope == self.mem.session().as_str() {
            return Ok(RecallTarget::Own);
        }
        let Some(handle) = self.layers().handle(scope) else {
            let mut scopes = vec![self.mem.session().as_str()];
            scopes.extend(self.layer_names());
            return Err(bad_param(format!(
                "scope must name this session or one of its layers: {}",
                scopes.join(", ")
            )));
        };
        self.authorize_layer(scope)?;
        match handle.live() {
            Some(live) => Ok(RecallTarget::Layer(live)),
            None => Err(layer_unavailable(TOOL, scope, self.mem.session().as_str())),
        }
    }

    /// Defence in depth (design section 1, Auth): the call's grant must
    /// reach `layer`. The start refused any credential that reaches this
    /// session but not one of its layers, so a refusal here is a
    /// configuration bug: answered generically, without naming the layer,
    /// and logged by name for the operator.
    fn authorize_layer(&self, layer: &str) -> Result<(), CallToolResult> {
        if self.layers().authorize(grant().as_deref(), layer) {
            return Ok(());
        }
        tracing::error!(
            session = %self.mem.session(),
            layer = %layer,
            "mcp: a credential reaching a layered session was refused its layer; the serve's \
             start check should have refused this configuration"
        );
        Err(config_refusal(
            TOOL,
            "this credential cannot read every session this recall reads; ask the operator \
             to check the serve's layer and credential configuration",
        ))
    }
}

/// What a recall reads (see [`LamboServer::recall_target`]).
enum RecallTarget {
    /// This session alone.
    Own,
    /// This session and its layers.
    Stack,
    /// One layer alone, held live for the call.
    Layer(LiveLayer),
}

/// The deployment preconditions of a recall by image (`image`) or by a
/// client vector over `mem`, refused with the setting named so the caller
/// (or its operator) can act: a store with vector search, then an embedder
/// that embeds images, or the operator's client-vector opt-in.
fn recall_by_preconditions(mem: &Memory, image: bool) -> Result<(), CallToolResult> {
    if !mem.vector_candidates().available() {
        return Err(config_refusal(
            TOOL,
            "a recall by image or by vector needs a store with vector search \
                 (VECTOR_SEARCH), and this server's store does not search vectors",
        ));
    }
    if image {
        if !mem.embedder().modalities().contains(Modalities::IMAGE) {
            return Err(config_refusal(
                TOOL,
                "this server's embedder does not embed images; send a query_vector \
                     computed in this session's embedding space instead (which needs \
                     [embedder] accept_client_vectors = true)",
            ));
        }
    } else if !mem.config().accept_client_vectors {
        return Err(config_refusal(
            TOOL,
            "this server does not accept client-computed vectors; the operator \
                 enables them with [embedder] accept_client_vectors = true (or \
                 LAMBO_ACCEPT_CLIENT_VECTORS=true)",
        ));
    }
    Ok(())
}
