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

use super::scope::Scoped;
use crate::embed::Modalities;
use crate::mcp::server::layers::{LayerHandle, LayerRead, LiveLayer};
use crate::mcp::server::params::{check_size, RecallParams};
use crate::mcp::server::response::{
    attach_warnings, bad_param, config_refusal, redact_urls, tool_err,
};
use crate::mcp::server::trace::{layered_recall_facts, note_facts, recall_facts};
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
        // The deployment checks below read this session's embedder, store
        // and config. A layer was built from the same serve template, so
        // they hold for it too; its own contract is checked by the read.
        let mem: &Memory = &self.mem;

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
        // Review L5: a scoped layer is looked up only now, after the
        // payload is decoded, so the call holds the layer's `Memory` (and
        // delays its re-attach) only for the read itself.
        let layer = match &target {
            RecallTarget::Layer(handle) => match self.read_layer(
                TOOL,
                handle,
                &format!(
                    "recall without scope for results from {}",
                    self.mem.session()
                ),
            ) {
                Ok(live) => Some(live),
                Err(e) => return e,
            },
            RecallTarget::Own | RecallTarget::Stack => None,
        };
        let mem: &Memory = layer.as_ref().map_or(&self.mem, |l| &l.mem);
        let recalled = match (by, &target) {
            // A layered session's text recall: its own session and its
            // layers, merged (design section 2).
            (None, RecallTarget::Stack) => {
                let live: Vec<Option<LiveLayer>> = self
                    .layers()
                    .handles
                    .iter()
                    .map(|h| h.live(LayerRead::Merged).ok())
                    .collect();
                let slots: Vec<LayerSlot<'_>> = self
                    .layers()
                    .handles
                    .iter()
                    .zip(&live)
                    .map(|(handle, live)| match live {
                        Some(live) => {
                            let layer = LayerRef::new(&live.mem).with_weight(handle.weight);
                            // #87: an unset floor stays unset, so the recall
                            // resolves it under the session's contract.
                            LayerSlot::Live(match handle.min_similarity {
                                Some(m) => layer.with_min_similarity(m),
                                None => layer,
                            })
                        }
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
        // Review M1: a layer's own answer carries ids that belong to the
        // layer, so say how to inspect one.
        if let RecallTarget::Layer(handle) = &target {
            recalled.warnings.push(format!(
                "recall: these hits are from layer {layer}, and their ids belong to it; pass \
                 scope = \"{layer}\" to lambo_inspect to inspect one",
                layer = handle.session
            ));
        }
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
                    // #179: additive, on a contradiction companion only: the
                    // ids of the hits it contradicts. It holds no top_k slot.
                    if !lh.companion_of.is_empty() {
                        map.insert(
                            "companion_of".into(),
                            json!(lh
                                .companion_of
                                .iter()
                                .map(|id| id.0.to_string())
                                .collect::<Vec<_>>()),
                        );
                    }
                    // #198: additive, on the older hit of a suspected pair
                    // whose newer concept is also shown, whatever the line
                    // gate says (the structured form of the line).
                    if let Some(by) = lh.possibly_superseded_by {
                        map.insert("possibly_superseded_by".into(), json!(by.0.to_string()));
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

    /// Which session(s) a recall with `scope` reads (#33 PR 3, design
    /// sections 2.6 and 3): no scope is this session, with its layers when
    /// it has any (each authorized for the call); a scope is resolved as
    /// every read tool resolves it (`scope`), and a layer is looked up
    /// only when the recall reads it.
    fn recall_target(&self, scope: Option<&str>) -> Result<RecallTarget<'_>, CallToolResult> {
        let Some(scope) = scope else {
            if self.layers().is_empty() {
                return Ok(RecallTarget::Own);
            }
            for name in self.layer_names() {
                self.authorize_layer(TOOL, name)?;
            }
            return Ok(RecallTarget::Stack);
        };
        Ok(match self.resolve_scope(TOOL, scope)? {
            Scoped::Own => RecallTarget::Own,
            Scoped::Layer(handle) => RecallTarget::Layer(handle),
        })
    }
}

/// What a recall reads (see [`LamboServer::recall_target`]).
enum RecallTarget<'a> {
    /// This session alone.
    Own,
    /// This session and its layers.
    Stack,
    /// One layer alone, looked up when it is read.
    Layer(&'a LayerHandle),
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
