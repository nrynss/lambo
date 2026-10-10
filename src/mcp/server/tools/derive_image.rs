//! `lambo_derive_image` (#22 PR 4): one image concept, validated, embedded
//! (or its client vector checked) on the call path, and acknowledged with a
//! receipt before the background write, as `lambo_derive` is (J3).
//!
//! **Nothing of the image leaves this call.** The bytes are decoded, checked
//! and embedded here and dropped when the call returns; what is queued, made
//! durable and replayed is the vector (`Memory::derive_image_as`'s async
//! twin). No message this code builds, ledger line or receipt quotes the
//! base64, the bytes or a vector component: refusals name the field and the
//! rule, and the ledger line carries only which payload kind was sent. The
//! one residual is rmcp's own parameter extraction, which can quote a value
//! sent in a wrongly typed slot back to that caller (see the internal notes
//! in `params.rs`).

use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::json;

use crate::embed::Modalities;
use crate::graph::image::{self, ImageDerive, ImagePayload};
use crate::mcp::server::params::{check_size, DeriveImageParams};
use crate::mcp::server::response::{bad_param, config_refusal, redact_urls, tool_err};
use crate::mcp::server::trace::note_facts;
use crate::mcp::server::LamboServer;
use crate::surface::image::{check_caption_fits, check_submitted_vector, decode_base64, validate};
use crate::surface::validate::require_nonempty;
use crate::types::{ConceptType, EmbeddingContract, MatchStrategy};

const TOOL: &str = "lambo_derive_image";

impl LamboServer {
    pub(crate) async fn derive_image_impl(&self, p: DeriveImageParams) -> CallToolResult {
        let acting = match self.caller_agent(&p.agent_id) {
            Ok(a) => a,
            Err(e) => return e,
        };
        // Exactly one payload (design 6.1). Checked first: it is the shape of
        // the call, and nothing below means anything without it.
        let payload_kind = match (&p.image, &p.vector) {
            (Some(_), None) => "image",
            (None, Some(_)) => "vector",
            _ => return bad_param("send exactly one of image or vector"),
        };

        if let Err(msg) = require_nonempty("caption", &p.caption) {
            return bad_param(msg);
        }
        if let Err(e) = check_size("caption", &p.caption) {
            return e;
        }
        if let Err(msg) = image::check_caption(&p.caption) {
            return bad_param(msg);
        }
        if let Some(id) = &p.image_id
            && let Err(msg) = image::validate_image_id(id)
        {
            return bad_param(msg);
        }
        // The caption's real limit is the content cap less the suffix
        // Lambo appends (review M2); past it the core would refuse the
        // built content as an opaque configuration error.
        if let Err(msg) = check_caption_fits(&p.caption, p.image_id.as_deref()) {
            return bad_param(msg);
        }
        let pairs: Vec<(&str, &str)> = p
            .parent_of
            .iter()
            .flatten()
            .map(|r| (r.parent.as_str(), r.child.as_str()))
            .collect();
        if pairs
            .iter()
            .any(|(a, b)| a.trim().is_empty() || b.trim().is_empty())
        {
            return bad_param("parent_of entries must have non-empty parent and child");
        }
        for (a, b) in &pairs {
            if let Err(e) = check_size("parent_of.parent", a) {
                return e;
            }
            if let Err(e) = check_size("parent_of.child", b) {
                return e;
            }
        }

        // #74: a parent_of end too long to embed framed with the image
        // content is refused now, not on the receipt. Only a session that
        // embeds reaches apply; the preconditions below refuse the rest.
        if self.mem.derive_embeds()
            && let Err(msg) = image::check_embed_context(
                &p.caption,
                p.image_id.as_deref(),
                ConceptType::from(p.concept_type),
                &pairs,
            )
        {
            return bad_param(msg);
        }

        // Deployment preconditions, named so the caller (or its operator) can
        // act: these are Lambo's own settings, not environment detail.
        if self.mem.config().match_strategy != MatchStrategy::Hybrid {
            return config_refusal(
                TOOL,
                "an image derive needs match_strategy = \"hybrid\": an image concept is found \
                 only through its vector",
            );
        }
        // The core refuses this too, but as a `LamboError::Config`, which
        // `tool_err` turns into a bare class (review M3). The tool stays
        // listed on such a store: design 6.1 lists it on the embedder's
        // modality and the client-vector key only, as `match_strategy` is
        // not a listing input either, and a listed tool whose refusal names
        // the missing capability tells the operator what to change where an
        // absent tool says nothing.
        if !self.mem.derive_vector_candidates().available() {
            return config_refusal(
                TOOL,
                "an image derive needs a store with vector search (VECTOR_SEARCH): an image \
                 concept is found only through its vector, and this server's store does not \
                 search vectors",
            );
        }

        // The decoded bytes must outlive the borrow `ImagePayload::Bytes`
        // holds, so they live here.
        let bytes;
        let payload = match (p.image, p.vector) {
            (Some(img), None) => {
                if !self.mem.embedder().modalities().contains(Modalities::IMAGE) {
                    return config_refusal(
                        TOOL,
                        "this server's embedder does not embed images; send a vector \
                         computed in this session's embedding space instead (which needs \
                         [embedder] accept_client_vectors = true)",
                    );
                }
                // The length cap runs before any decoding. A frame under
                // the transports' 4 MiB frame cap (#101) can still carry
                // base64 longer than an image may be.
                bytes = match decode_base64(&img.data) {
                    Ok(b) => b,
                    Err(msg) => return bad_param(msg),
                };
                match validate(&bytes, &img.mime) {
                    Ok(input) => ImagePayload::Bytes(input),
                    Err(msg) => return bad_param(msg),
                }
            }
            (None, Some(vector)) => {
                if !self.mem.config().accept_client_vectors {
                    return config_refusal(
                        TOOL,
                        "this server does not accept client-computed vectors; the operator \
                         enables them with [embedder] accept_client_vectors = true (or \
                         LAMBO_ACCEPT_CLIENT_VECTORS=true)",
                    );
                }
                let declared = EmbeddingContract {
                    kind: vector.contract.kind,
                    model: vector.contract.model,
                    dim: vector.contract.dim,
                };
                // AC4 at the wire: refused here with a message that names the
                // fields that differ and never quotes the submitted ones.
                if let Err(msg) =
                    check_submitted_vector(&vector.values, &declared, self.mem.embedding_contract())
                {
                    return bad_param(msg);
                }
                ImagePayload::Vector {
                    values: vector.values,
                    declared,
                }
            }
            // Already refused by the one-of check above.
            _ => return bad_param("send exactly one of image or vector"),
        };

        let derive = ImageDerive {
            caption: &p.caption,
            concept_type: ConceptType::from(p.concept_type),
            image_id: p.image_id.as_deref(),
            payload,
            parent_of: &pairs,
            event_time: p.event_time,
        };
        let submitted = match self.mem.derive_image_async_as(&acting, derive).await {
            Ok(s) => s,
            Err(e) => return tool_err(TOOL, e),
        };

        // I1: which payload kind, whether it was admitted, and the receipt.
        // Never the caption, the id, the bytes or the vector.
        note_facts(|| {
            json!({
                "payload": payload_kind,
                "admitted": !submitted.dropped(),
                "receipt": submitted.receipt.to_string(),
            })
        });

        let summary = if submitted.dropped() {
            format!(
                "the image concept was NOT written: {}",
                submitted.answer.describe()
            )
        } else {
            "accepted 1 image concept for background write; validated, embedded and ordered, \
             not yet applied"
                .to_owned()
        };
        let mut out = CallToolResult::success(vec![ContentBlock::text(format!(
            "{summary}\nreceipt {}: {}\nThe outcome arrives on your next tool response. To wait \
             for it, call lambo_stats with receipt={} (add wait_ms to block).",
            submitted.receipt,
            redact_urls(&submitted.answer.describe()),
            submitted.receipt,
        ))]);
        out.structured_content = Some(json!({
            "summary": summary,
            "receipt": submitted.receipt.to_string(),
            "receipt_state": submitted.answer.tag(),
            "warnings": [],
        }));
        out
    }
}
