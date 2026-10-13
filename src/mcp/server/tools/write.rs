//! `lambo_derive` and `lambo_record_action`: validated, ordered, and
//! acknowledged with a receipt before the background write (J3).

use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::json;

use crate::graph::action::Action;
use crate::graph::cite::{
    MAX_AGAINST_PER_CONCEPT, MAX_AGAINST_PER_DERIVE, MAX_REFERENCES_PER_CONCEPT,
    MAX_REFERENCES_PER_DERIVE,
};
use crate::graph::derive::ParentOf;
use crate::graph::hybrid;
use crate::mcp::server::params::{check_size, DeriveParams, RecordActionParams};
use crate::mcp::server::response::{bad_param, recorded_under, redact_urls};
use crate::mcp::server::trace::note_facts;
use crate::mcp::server::LamboServer;
use crate::surface::validate::{
    check_action_targets, check_concept_count, check_no_image_suffix, require_nonempty,
};
use crate::types::{ConceptCitations, ConceptType};

use super::scope::{scoped_ack_line, with_scope, with_session, with_warning};

/// One reference list's shape checks: each entry non-empty and within the
/// size cap, at most `per_concept` on its `owner` ("a concept's") and
/// `per_call` across the call. `on_owner` and `total` carry the running
/// counts: `on_owner` per content, as the core counts it (#19(b) review
/// L6), so a content sent as two wire concepts is one concept here too.
fn check_references(
    owner: &str,
    field: &str,
    refs: &[String],
    per_concept: usize,
    per_call: usize,
    on_owner: &mut usize,
    total: &mut usize,
) -> Result<(), CallToolResult> {
    *on_owner += refs.len();
    if *on_owner > per_concept {
        return Err(bad_param(format!(
            "{owner} {field} may list at most {per_concept} concepts"
        )));
    }
    *total += refs.len();
    if *total > per_call {
        return Err(bad_param(format!(
            "{field} may list at most {per_call} concepts across one call"
        )));
    }
    let what = if owner == "a concept's" {
        format!("concept.{field} entry")
    } else {
        format!("{field} entry")
    };
    for r in refs {
        if let Err(e) = require_nonempty(&what, r) {
            return Err(bad_param(e));
        }
        check_size(&what, r)?;
    }
    Ok(())
}

/// A derive's per-concept `depends_on` (#36 PR 4) and `against` (#19(b))
/// as core references, refused with `bad_param` when one is empty, too long
/// or over a count limit. Concepts without any are left out, so a call
/// without references is exactly the call it was before.
fn wire_citations(p: &DeriveParams) -> Result<Vec<ConceptCitations>, CallToolResult> {
    let mut out = Vec::new();
    let (mut cited, mut contradicted) = (0usize, 0usize);
    // Per content, as `graph::cite::pin` counts them.
    let mut per_content: std::collections::HashMap<&str, (usize, usize)> =
        std::collections::HashMap::new();
    for c in &p.concepts {
        let depends_on: Vec<String> = c.depends_on.iter().flatten().map(|r| r.0.clone()).collect();
        let against: Vec<String> = c.against.iter().flatten().map(|r| r.0.clone()).collect();
        if depends_on.is_empty() && against.is_empty() {
            continue;
        }
        let (on_cited, on_contradicted) = per_content.entry(c.content.as_str()).or_default();
        check_references(
            "a concept's",
            "depends_on",
            &depends_on,
            MAX_REFERENCES_PER_CONCEPT,
            MAX_REFERENCES_PER_DERIVE,
            on_cited,
            &mut cited,
        )?;
        check_references(
            "a concept's",
            "against",
            &against,
            MAX_AGAINST_PER_CONCEPT,
            MAX_AGAINST_PER_DERIVE,
            on_contradicted,
            &mut contradicted,
        )?;
        out.push(ConceptCitations {
            content: c.content.clone(),
            depends_on,
            against,
        });
    }
    Ok(out)
}

impl LamboServer {
    pub(crate) async fn derive_impl(&self, p: DeriveParams) -> CallToolResult {
        let acting = match self.caller_agent(&p.agent_id) {
            Ok(a) => a,
            Err(e) => return e,
        };
        // #33 PR 4: the session written to, refused before anything else.
        let target = match self.target("lambo_derive", p.scope.as_deref()) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let mem = target.mem(&self.mem);
        let event_time = p.event_time;
        // #33 PR 4: the one warning this path has is a scoped write's
        // closed-layer warning (`after_submit`, through `with_warning`).
        // J1-R1-3: before it, this path could no longer emit a warning. The attribution
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
        // #33 PR 4: nothing written into a shared layer is a bare relative
        // path, whatever its type (review L1).
        if let Err(e) = target.check_paths(
            "concept.content",
            p.concepts.iter().map(|c| c.content.as_str()),
        ) {
            return e;
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
        // Review L1: a parent_of end becomes a concept too.
        if let Err(e) = target.check_paths("parent_of", pairs.iter().flat_map(|(a, b)| [*a, *b])) {
            return e;
        }
        let parent_of = if pairs.is_empty() {
            ParentOf::none()
        } else {
            ParentOf::from_pairs(&pairs)
        };
        // #36 PR 4, #19(b): each concept's citations and contradictions,
        // checked for shape here; whether each resolves is the core
        // pre-pass's question.
        let citations = match wire_citations(&p) {
            Ok(c) => c,
            Err(e) => return e,
        };

        // #74: on a session that embeds, a call whose embedding context
        // would exceed the hybrid limit is refused now, naming the limit,
        // rather than failing on its receipt as an opaque configuration error.
        if mem.derive_embeds()
            && let Err(msg) = hybrid::check_embed_context(&concepts, &parent_of, None)
        {
            return bad_param(msg);
        }

        // J3: acknowledged after validation, before the embedder. The
        // validation pre-pass and the interaction that pins this write's place
        // in the `Temporal` chain both happen inside this call; the embed,
        // canonicalize and insert happen in the background.
        let submitted = match mem
            .derive_cited_async_scoped(
                &acting,
                &concepts,
                &parent_of,
                &citations,
                event_time,
                &target.probe(self),
                target.deliver_to().as_ref(),
            )
            .await
        {
            Ok(s) => s,
            Err(e) => return self.target_err("lambo_derive", &target, e),
        };
        // Review M3: a layer fenced between the gate and the admission
        // admitted a job its apply refuses; say so, not "accepted".
        let warning = match self.after_submit("lambo_derive", &target) {
            Ok(w) => w,
            Err(refusal) => return refusal,
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
            with_scope(
                json!({
                    "concepts_requested": concepts.len(),
                    "admitted": !submitted.dropped(),
                    "receipt": submitted.receipt.to_string(),
                }),
                p.scope.as_deref(),
            )
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
             for it, call lambo_stats with receipt={} (add wait_ms to block).\n{}{}",
            submitted.receipt,
            redact_urls(&submitted.answer.describe()),
            submitted.receipt,
            recorded_under(&acting),
            scoped_ack_line(&target),
        ))]);
        // `created` / `matched` are deliberately ABSENT rather than empty: an
        // empty list here would claim nothing was created, which is not what
        // this ack knows. They live on the receipt.
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

    pub(crate) async fn record_action_impl(&self, p: RecordActionParams) -> CallToolResult {
        let acting = match self.caller_agent(&p.agent_id) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let target = match self.target("lambo_record_action", p.scope.as_deref()) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let event_time = p.event_time;
        // J1-R1-3: only the closed-layer warning is reachable here — see
        // `derive_impl`.
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
        // #19(b): the contradicted concepts, shape-checked here and resolved
        // by the core pre-pass. Bounded on their own (at most 8), apart from
        // N1's fan-out cap, since each names an existing concept and creates
        // nothing.
        let against: Vec<String> = p
            .against
            .unwrap_or_default()
            .into_iter()
            .map(|r| r.0)
            .collect();
        let (mut on_action, mut contradicted) = (0usize, 0usize);
        if let Err(e) = check_references(
            "an action's",
            "against",
            &against,
            MAX_AGAINST_PER_CONCEPT,
            MAX_AGAINST_PER_CONCEPT,
            &mut on_action,
            &mut contradicted,
        ) {
            return e;
        }
        // #33 PR 4: no bare relative path into a shared layer.
        for (field, entries) in [
            ("produces", &produces),
            ("modifies", &modifies),
            ("depends_on", &depends_on),
        ] {
            if let Err(e) = target.check_paths(field, entries.iter().map(String::as_str)) {
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
        let submitted = match target
            .mem(&self.mem)
            .record_action_async_scoped(
                &acting,
                &action,
                &against,
                &target.probe(self),
                target.deliver_to().as_ref(),
            )
            .await
        {
            Ok(s) => s,
            Err(e) => return self.target_err("lambo_record_action", &target, e),
        };
        // Review M3, as in `derive_impl`.
        let warning = match self.after_submit("lambo_record_action", &target) {
            Ok(w) => w,
            Err(refusal) => return refusal,
        };

        // I1: `created` and `edges` are not knowable at ack time — see the
        // matching note in `derive_impl` for what that costs and where the
        // numbers went.
        note_facts(|| {
            with_scope(
                json!({
                    "admitted": !submitted.dropped(),
                    "receipt": submitted.receipt.to_string(),
                }),
                p.scope.as_deref(),
            )
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
             for it, call lambo_stats with receipt={} (add wait_ms to block).\n{}{}",
            submitted.receipt,
            redact_urls(&submitted.answer.describe()),
            submitted.receipt,
            recorded_under(&acting),
            scoped_ack_line(&target),
        ))]);
        // `action_node`, `created` and `edges` are ABSENT rather than zeroed:
        // this ack does not know them. They are on the receipt.
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
