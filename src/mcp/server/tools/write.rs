//! `lambo_derive` and `lambo_record_action`: validated, ordered, and
//! acknowledged with a receipt before the background write (J3).

use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::json;

use crate::graph::action::Action;
use crate::graph::derive::ParentOf;
use crate::graph::hybrid;
use crate::mcp::server::params::{check_size, DeriveParams, RecordActionParams};
use crate::mcp::server::response::{bad_param, redact_urls, tool_err};
use crate::mcp::server::trace::note_facts;
use crate::mcp::server::LamboServer;
use crate::surface::validate::{
    check_action_targets, check_concept_count, check_no_image_suffix, require_nonempty,
};
use crate::types::ConceptType;

impl LamboServer {
    pub(crate) async fn derive_impl(&self, p: DeriveParams) -> CallToolResult {
        let acting = match self.caller_agent(&p.agent_id) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let event_time = p.event_time;
        // J1-R1-3: this path can no longer emit a warning. The attribution
        // warning was the only one it ever had, and J1 deleted it rather than
        // rewording it, so a `Vec` here would be a shape that says otherwise to
        // the next reader. The `warnings` key stays in `structuredContent`
        // because it is part of the response shape consumers read; if a later
        // phase (J3's write receipts) gives this tool something to say, it must
        // also go through `attach_warnings`, which is what puts a warning where
        // the model actually reads it (R1/T82-9).
        if p.concepts.is_empty() {
            return bad_param("concepts must contain at least one entry");
        }
        if let Err(msg) = check_concept_count(p.concepts.len()) {
            return bad_param(msg);
        }
        if let Some(bad) = p.concepts.iter().find(|c| c.content.trim().is_empty()) {
            let _ = bad;
            return bad_param("every concept.content must be a non-empty string");
        }
        for c in &p.concepts {
            if let Err(e) = check_size("concept.content", &c.content) {
                return e;
            }
            // #22 PR 4: only lambo_derive_image builds an image suffix.
            if let Err(msg) = check_no_image_suffix("concept.content", &c.content) {
                return bad_param(msg);
            }
        }

        // `derive` borrows `&[(&str, ConceptType)]` and `ParentOf<'_>` borrows
        // `&[(&str, &str)]`; both owners must outlive the await.
        let concepts: Vec<(&str, ConceptType)> = p
            .concepts
            .iter()
            .map(|c| (c.content.as_str(), ConceptType::from(c.concept_type)))
            .collect();
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
        let parent_of = if pairs.is_empty() {
            ParentOf::none()
        } else {
            ParentOf::from_pairs(&pairs)
        };

        // #74: on a session that embeds, a call whose embedding context
        // would exceed the hybrid limit is refused now, naming the limit,
        // rather than failing on its receipt as an opaque configuration error.
        if self.mem.derive_embeds()
            && let Err(msg) = hybrid::check_embed_context(&concepts, &parent_of, None)
        {
            return bad_param(msg);
        }

        // J3: acknowledged after validation, before the embedder. The
        // validation pre-pass and the interaction that pins this write's place
        // in the `Temporal` chain both happen inside this call; the embed,
        // canonicalize and insert happen in the background.
        let submitted = match self
            .mem
            .derive_async_as(&acting, &concepts, &parent_of, event_time)
            .await
        {
            Ok(s) => s,
            Err(e) => return tool_err("lambo_derive", e),
        };

        // I1 (DOGFOOD metric 2, re-derivation savings) — **changed by J3, and
        // this is the honest statement of what changed.** `created`, `matched`,
        // `semantic_merged` and `reinforced` are no longer knowable when this
        // line is written: the whole point of the async ack is that the write
        // has not happened yet. They are not lost — they are on the receipt,
        // and `receipt` is emitted here so a reader can join the two — but
        // `scripts/observability/dedup_rate.py` and `duplicates.py` read them
        // off the ledger line, so for MCP-driven sessions those two tools now
        // see zero derive facts. CLI-driven sessions are unaffected: they use
        // the synchronous `Memory::derive`, which still reports everything.
        //
        // The fix is a background-completion ledger line, which is a ledger
        // *schema* change and therefore not J3's (see §J3's handoff note). What
        // J3 owes is that the loss is visible rather than silent, which is what
        // `admitted` and `receipt` on this line are for.
        note_facts(|| {
            json!({
                "concepts_requested": concepts.len(),
                "admitted": !submitted.dropped(),
                "receipt": submitted.receipt.to_string(),
            })
        });

        let summary = if submitted.dropped() {
            format!(
                "{} concept(s) were NOT written: {}",
                concepts.len(),
                submitted.answer.describe()
            )
        } else {
            format!(
                "accepted {} concept(s) for background write; validated and ordered, not yet \
                 applied",
                concepts.len()
            )
        };
        let mut out = CallToolResult::success(vec![ContentBlock::text(format!(
            "{summary}\nreceipt {}: {}\nThe outcome arrives on your next tool response. To wait \
             for it, call lambo_stats with receipt={} (add wait_ms to block).",
            submitted.receipt,
            redact_urls(&submitted.answer.describe()),
            submitted.receipt,
        ))]);
        // `created` / `matched` are deliberately ABSENT rather than empty: an
        // empty list here would claim nothing was created, which is not what
        // this ack knows. They live on the receipt.
        out.structured_content = Some(json!({
            "summary": summary,
            "receipt": submitted.receipt.to_string(),
            "receipt_state": submitted.answer.tag(),
            "warnings": [],
        }));
        out
    }

    pub(crate) async fn record_action_impl(&self, p: RecordActionParams) -> CallToolResult {
        let acting = match self.caller_agent(&p.agent_id) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let event_time = p.event_time;
        // J1-R1-3: no warning is reachable here — see `derive_impl`.
        if let Err(e) = require_nonempty("action", &p.action) {
            return bad_param(e);
        }
        if let Err(e) = check_size("action", &p.action) {
            return e;
        }
        // #22 PR 4: the action is its own Resource concept's content.
        if let Err(msg) = check_no_image_suffix("action", &p.action) {
            return bad_param(msg);
        }
        let produces: Vec<String> = p
            .produces
            .unwrap_or_default()
            .into_iter()
            .map(|r| r.0)
            .collect();
        let modifies: Vec<String> = p
            .modifies
            .unwrap_or_default()
            .into_iter()
            .map(|r| r.0)
            .collect();
        let depends_on: Vec<String> = p
            .depends_on
            .unwrap_or_default()
            .into_iter()
            .map(|r| r.0)
            .collect();
        // N1: cap the combined fan-out. Without this bound one client could hand
        // `record_action` an arbitrarily long target list and hold the single
        // process's graph write lock for as long as it takes to fan every entry
        // out into a concept and an edge — the stall vector `lambo_derive` is
        // already guarded against, on the tool that had no guard.
        let total = produces.len() + modifies.len() + depends_on.len();
        if let Err(msg) = check_action_targets(total) {
            return bad_param(msg);
        }
        if produces
            .iter()
            .chain(&modifies)
            .chain(&depends_on)
            .any(|s| s.trim().is_empty())
        {
            return bad_param("produces / modifies / depends_on entries must be non-empty");
        }
        for s in produces.iter().chain(&modifies).chain(&depends_on) {
            if let Err(e) = check_size("produces / modifies / depends_on entry", s) {
                return e;
            }
        }

        // N1, superseded by J3 and worth saying why rather than deleting.
        // `Memory::record_action` is synchronous and takes the graph write lock
        // for its whole body, so calling it inline occupied a Tokio *worker*
        // thread until it returned and a burst of large calls could starve the
        // runtime — including the worker that would run `Memory::close` on
        // SIGTERM. N1 moved it to the blocking pool with `spawn_blocking`.
        //
        // J3 removes the shape instead of offloading it: the graph write now
        // happens on a background pipeline worker and this call does validation
        // plus one brief `begin_interaction_full` lock, neither of which can
        // occupy a worker for long. So the `spawn_blocking` hop is gone, and
        // what it was defending is defended better. The load-bearing anti-hang
        // guarantee is unchanged and still `serve`'s `CLOSE_GRACE` bound
        // (src/mcp/serve.rs), which force-exits a stalled shutdown regardless.
        let produces: Vec<&str> = produces.iter().map(String::as_str).collect();
        let modifies: Vec<&str> = modifies.iter().map(String::as_str).collect();
        let depends_on: Vec<&str> = depends_on.iter().map(String::as_str).collect();
        let action = Action {
            event_time,
            action: p.action.as_str(),
            produces: &produces,
            modifies: &modifies,
            depends_on: &depends_on,
        };
        // J3: acknowledged after validation, before the graph write. Unlike
        // `derive` this path has no embedder hop, so what asynchrony buys here
        // is ORDERING with derive — see `Memory::record_action_async_as`.
        let submitted = match self.mem.record_action_async_as(&acting, &action).await {
            Ok(s) => s,
            Err(e) => return tool_err("lambo_record_action", e),
        };

        // I1: `created` and `edges` are not knowable at ack time — see the
        // matching note in `derive_impl` for what that costs and where the
        // numbers went.
        note_facts(|| {
            json!({
                "admitted": !submitted.dropped(),
                "receipt": submitted.receipt.to_string(),
            })
        });
        let summary = if submitted.dropped() {
            format!(
                "action '{}' was NOT recorded: {}",
                p.action,
                submitted.answer.describe()
            )
        } else {
            format!(
                "accepted action '{}' for background write; validated and ordered, not yet \
                 applied",
                p.action
            )
        };
        let mut out = CallToolResult::success(vec![ContentBlock::text(format!(
            "{summary}\nreceipt {}: {}\nThe outcome arrives on your next tool response. To wait \
             for it, call lambo_stats with receipt={} (add wait_ms to block).",
            submitted.receipt,
            redact_urls(&submitted.answer.describe()),
            submitted.receipt,
        ))]);
        // `action_node`, `created` and `edges` are ABSENT rather than zeroed:
        // this ack does not know them. They are on the receipt.
        out.structured_content = Some(json!({
            "summary": summary,
            "receipt": submitted.receipt.to_string(),
            "receipt_state": submitted.answer.tag(),
            "warnings": [],
        }));
        out
    }
}
