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
use crate::mcp::server::response::{
    bad_param, config_refusal, recorded_under, redact_urls, tool_err,
};
use crate::mcp::server::trace::note_facts;
use crate::mcp::server::LamboServer;
use crate::memory::embed_server_image;
use crate::surface::image::{check_caption_fits, check_submitted_vector, decode_base64, validate};
use crate::surface::validate::require_nonempty;
use crate::types::{ConceptType, EmbeddingContract, MatchStrategy};

use super::scope::{scoped_ack_line, with_scope, with_session, with_warning};

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
        // #33 PR 4: the session written to. Its own embedder, config and
        // store answer every precondition below.
        let target = match self.target(TOOL, p.scope.as_deref()) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let mem = target.mem(&self.mem);
        let scope = p.scope.clone();
        // Review L4: a refusal on a scoped call names whose settings it is.
        // An unscoped call's refusals read exactly as before.
        let (whose, strategy_note, who_accepts) = match target.layer() {
            Some(layer) => (
                format!("layer {layer}'s"),
                format!(" (layer {layer}'s settings are not)"),
                format!("layer {layer}'s settings do"),
            ),
            None => (
                "this server's".to_owned(),
                String::new(),
                "this server does".to_owned(),
            ),
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
        // #33 PR 4 review L1: nothing written into a shared layer is a bare
        // relative path (a caption carries its image suffix, so never is).
        if let Err(e) = target.check_paths("parent_of", pairs.iter().flat_map(|(a, b)| [*a, *b])) {
            return e;
        }

        // #74: a parent_of end too long to embed framed with the image
        // content is refused now, not on the receipt. Only a session that
        // embeds reaches apply; the preconditions below refuse the rest.
        if mem.derive_embeds()
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
        if mem.config().match_strategy != MatchStrategy::Hybrid {
            return config_refusal(
                TOOL,
                &format!(
                    "an image derive needs match_strategy = \"hybrid\"{strategy_note}: an \
                     image concept is found only through its vector"
                ),
            );
        }
        // The core refuses this too, but as a `LamboError::Config`, which
        // `tool_err` turns into a bare class (review M3). The tool stays
        // listed on such a store: design 6.1 lists it on the embedder's
        // modality and the client-vector key only, as `match_strategy` is
        // not a listing input either, and a listed tool whose refusal names
        // the missing capability tells the operator what to change where an
        // absent tool says nothing.
        if !mem.derive_vector_candidates().available() {
            return config_refusal(
                TOOL,
                &format!(
                    "an image derive needs a store with vector search (VECTOR_SEARCH): an image \
                     concept is found only through its vector, and {whose} store does not \
                     search vectors"
                ),
            );
        }

        // The decoded bytes must outlive the borrow `ImagePayload::Bytes`
        // holds, so they live here.
        let bytes;
        let payload = match (p.image, p.vector) {
            (Some(img), None) => {
                if !mem.embedder().modalities().contains(Modalities::IMAGE) {
                    return config_refusal(
                        TOOL,
                        &format!(
                            "{whose} embedder does not embed images; send a vector computed \
                             in this session's embedding space instead (which needs [embedder] \
                             accept_client_vectors = true)"
                        ),
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
                if !mem.config().accept_client_vectors {
                    return config_refusal(
                        TOOL,
                        &format!(
                            "{who_accepts} not accept client-computed vectors; the operator \
                             enables them with [embedder] accept_client_vectors = true (or \
                             LAMBO_ACCEPT_CLIENT_VECTORS=true)"
                        ),
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
                    check_submitted_vector(&vector.values, &declared, mem.embedding_contract())
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
        // Review H2: a scoped server-side image is embedded holding nothing
        // of the layer (its Memory, in-flight guard and writers gate would
        // otherwise be held through an embed of up to 30 s, delaying its
        // re-attach), then the layer is taken back, fence re-checked, with
        // no second rate token.
        let (target, pre) = match (&derive.payload, target.layer().is_some()) {
            (ImagePayload::Bytes(input), true) => {
                let embedder = std::sync::Arc::clone(mem.embedder());
                let contract = mem.embedding_contract().clone();
                let session = mem.session().clone();
                let input = *input;
                let Some(released) = target.release() else {
                    unreachable!("a layer target releases its layer")
                };
                let pre =
                    match embed_server_image(embedder.as_ref(), &contract, &session, input).await {
                        Ok(pre) => pre,
                        Err(e) => return tool_err(TOOL, e),
                    };
                match self.retake(TOOL, released) {
                    Ok(target) => (target, Some(pre)),
                    Err(e) => return e,
                }
            }
            _ => (target, None),
        };
        let submitted = match target
            .mem(&self.mem)
            .derive_image_async_scoped(&acting, derive, pre, target.deliver_to().as_ref())
            .await
        {
            Ok(s) => s,
            Err(e) => return self.target_err(TOOL, &target, e),
        };
        // Review M3, as in `derive_impl`.
        let warning = match self.after_submit(TOOL, &target) {
            Ok(w) => w,
            Err(refusal) => return refusal,
        };

        // I1: which payload kind, whether it was admitted, and the receipt.
        // Never the caption, the id, the bytes or the vector.
        note_facts(|| {
            with_scope(
                json!({
                    "payload": payload_kind,
                    "admitted": !submitted.dropped(),
                    "receipt": submitted.receipt.to_string(),
                }),
                scope.as_deref(),
            )
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
             for it, call lambo_stats with receipt={} (add wait_ms to block).\n{}{}",
            submitted.receipt,
            redact_urls(&submitted.answer.describe()),
            submitted.receipt,
            recorded_under(&acting),
            scoped_ack_line(&target),
        ))]);
        out.structured_content = Some(with_session(
            json!({
                "summary": summary,
                "receipt": submitted.receipt.to_string(),
                "receipt_state": submitted.answer.tag(),
                "agent_id": acting.to_string(),
                "warnings": [],
            }),
            &target,
        ));
        with_warning(out, warning)
    }
}
