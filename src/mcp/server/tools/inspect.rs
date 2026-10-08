//! `lambo_inspect`: the bounded neighbourhood around a resolved focus.

use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::json;

use crate::mcp::server::params::{check_size, InspectParams};
use crate::mcp::server::response::{attach_warnings, bad_param};
use crate::mcp::server::trace::{focus_for_ledger, note_facts};
use crate::mcp::server::LamboServer;
use crate::surface::focus::{
    ambiguous_refusal, fuzzy_note, missing_refusal, oversized_refusal, resolve_focus, Focus,
};
use crate::surface::limits::MAX_INSPECT_DEPTH;
use crate::surface::neighbourhood::render_neighbourhood;
use crate::surface::validate::{check_in_range, require_nonempty};

impl LamboServer {
    pub(in crate::mcp::server) async fn inspect_impl(&self, p: InspectParams) -> CallToolResult {
        if let Err(e) = self.check_agent_id(&p.agent_id) {
            return e;
        }
        let mut warnings: Vec<String> = Vec::new();
        if let Err(e) = require_nonempty("focus", &p.focus) {
            return bad_param(e);
        }
        if let Err(e) = check_size("focus", &p.focus) {
            return e;
        }
        let depth = p.depth.unwrap_or(2);
        if let Err(e) = check_in_range("depth", depth, 0, MAX_INSPECT_DEPTH) {
            return bad_param(e);
        }

        // One short read section, no `.await` inside (spec §6.4).
        let resolved = {
            let g = self.mem.graph().read();
            match resolve_focus(&g, p.focus.trim()) {
                Focus::Exact(id) => Ok((id, None, render_neighbourhood(&g, id, depth))),
                Focus::Fuzzy {
                    id,
                    content,
                    bounded,
                } => {
                    // A fuzzy resolution is stated in the *text*, not only in a
                    // warning: "focus: <something the caller did not ask for>"
                    // is exactly the line a model reads straight past. A
                    // bounded one also names the bound (issue #9), so the
                    // caller knows what was not scanned.
                    let note = fuzzy_note(p.focus.trim(), &content, bounded.as_ref());
                    Ok((id, Some(note), render_neighbourhood(&g, id, depth)))
                }
                other => Err(other),
            }
        };

        let (focus_id, note, (text, structured)) = match resolved {
            Ok(v) => v,
            Err(Focus::Ambiguous {
                candidates,
                bounded,
            }) => {
                // Refuse rather than pick (R1/T82-7): an arbitrary pick fed a
                // node_id the caller never named into `lambo_reserve` and into
                // edits, and the pick changed between calls.
                let msg = format!(
                    "lambo_inspect: {}",
                    ambiguous_refusal(p.focus.trim(), &candidates, bounded.as_ref())
                );
                // Issue #9: every inspect failure names its mode and its focus
                // in the ledger facts BEFORE the error returns, so telemetry
                // can classify the refusal without reprobing.
                note_facts(|| {
                    json!({
                        "failure": "ambiguous",
                        "focus": focus_for_ledger(&p.focus),
                    })
                });
                return CallToolResult::error(vec![ContentBlock::text(msg)]);
            }
            Err(Focus::Oversized { cap, near }) => {
                // T8.7 residual #3 graph-size guard, relaxed by issue #9: the
                // O(total-content) pass is still refused, but the bounded
                // subset was scanned and matched nothing. The refusal says so
                // and names the bound, because the concept the caller meant
                // may exist outside the subset; exact / node-id focus still
                // works. The near-match remediation survives past the cap,
                // ranked within the subset only and announced as such.
                note_facts(|| {
                    json!({
                        "failure": "oversized",
                        "focus": focus_for_ledger(&p.focus),
                    })
                });
                let msg = format!("lambo_inspect: {}", oversized_refusal(cap, &near));
                return CallToolResult::error(vec![ContentBlock::text(msg)]);
            }
            Err(Focus::Missing { near }) => {
                // Issue #9: the bare "no concept matching" refusal is how 22
                // of 78 dogfood-rig inspects failed. Do what Ambiguous does:
                // refuse, explain, and offer the closest concepts with their
                // node ids, announced as suggestions.
                let msg = format!(
                    "lambo_inspect: {}",
                    missing_refusal(&p.focus, &self.mem.session().0, &near)
                );
                note_facts(|| {
                    json!({
                        "failure": "missing",
                        "focus": focus_for_ledger(&p.focus),
                    })
                });
                return CallToolResult::error(vec![ContentBlock::text(msg)]);
            }
            // Unreachable: Exact and Fuzzy are resolved in the read section
            // above, and `Focus` has no further variants.
            Err(Focus::Exact(_) | Focus::Fuzzy { .. }) => {
                unreachable!("Exact and Fuzzy resolve in the read section")
            }
        };

        // A fuzzy resolution is stated in the *text*, not only in a warning:
        // "focus: <something the caller did not ask for>" is exactly the line a
        // model reads straight past.
        let text = match &note {
            Some(n) => {
                warnings.push(n.clone());
                format!("{n}\n{text}")
            }
            None => text,
        };

        // I1: enough to place an inspect in a call sequence without carrying the
        // whole neighbourhood into the ledger. `fuzzy` is worth a field — a
        // resolution the caller did not ask for is exactly the friction
        // DOGFOOD metric 6 is looking for.
        note_facts(|| json!({ "depth": depth, "fuzzy": note.is_some() }));

        // Issue #30: an inspect that resolved returned its focus concept to the
        // caller — an access. The focus only: the neighbourhood is context
        // around what was asked for (depth up to 5 can reach hundreds of
        // concepts), and a refusal (ambiguous / missing / oversized) returned
        // no concept at all.
        self.mem.note_accesses([focus_id]);

        let mut out = CallToolResult::success(vec![ContentBlock::text(text.clone())]);
        attach_warnings(&mut out, &warnings);
        out.structured_content = Some(json!({
            "view": text,
            "focus": structured,
            "resolution": note,
            "warnings": warnings,
        }));
        out
    }
}
