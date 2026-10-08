//! `lambo_stats`: session health, and the fate of one write receipt.

use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::json;

use crate::mcp::server::params::StatsParams;
use crate::mcp::server::response::{bad_param, redact_urls};
use crate::mcp::server::stats::gc_summary_line;
use crate::mcp::server::LamboServer;
use crate::writeq::ReceiptAnswer;

impl LamboServer {
    pub(in crate::mcp::server) async fn stats_impl(&self, p: StatsParams) -> CallToolResult {
        if let Err(e) = self.check_agent_id(&p.agent_id) {
            return e;
        }
        // J1-R1-3: no warning is reachable here — see `derive_impl`.
        //
        // **The receipt is resolved FIRST, and the ordering is load-bearing**
        // (found by measuring the shipped binary, not by a test): a `wait_ms`
        // blocks for up to `RECEIPT_WAIT_MAX`, so a snapshot taken before it
        // describes the session as it was before the write the caller was
        // waiting for. That reported `write_queue_applied: 0` and
        // `concept_count: 0` beside a receipt that said `applied` — a payload
        // contradicting itself. Everything below reads the session after the
        // wait.
        let receipt = match &p.receipt {
            None => None,
            Some(raw) => {
                let id = match raw.trim().parse::<crate::writeq::ReceiptId>() {
                    Ok(id) => id,
                    Err(_) => {
                        return bad_param(
                            "receipt must be a receipt id from a lambo_derive or \
                             lambo_record_action ack",
                        )
                    }
                };
                let acting = match self.caller_agent(&p.agent_id) {
                    Ok(a) => a,
                    Err(e) => return e,
                };
                let queue = self.mem.pipeline();
                let answer = match p.wait_ms {
                    // A wait of 0 is a fetch, not a wait; both go through the
                    // same clamp so the difference is only the budget.
                    Some(ms) if ms > 0 => {
                        queue
                            .wait(&acting, id, std::time::Duration::from_millis(ms))
                            .await
                    }
                    _ => queue.lookup(&acting, id),
                };
                // Delivered here, explicitly, so `answered`'s piggyback does
                // not state the same outcome a second time in the same response
                // (J3-R1-9). Take-once is unaffected — this *is* the take.
                if answer.is_settled() {
                    queue.mark_delivered(&acting, id);
                }
                Some((id, answer))
            }
        };

        let s = self.mem.stats();
        // One GC reading for both halves of the answer (see `stats_json_with_gc`).
        let gc = self.mem.gc_stats();
        let text = format!(
            "session '{}' (owner agent '{}')\n\
             nodes={} edges={} concepts={} canonical={}\n\
             embedded={}/{}\n\
             flush_lag={:?} log_depth={} flush_depth={} dead_lettered={} degraded={}\n\
             epoch={} daemon_cycles={} canonization_cycles={} canonization_failures={}\n\
             promotion_policy={}\n\
             {}",
            s.session.0,
            s.agent.0,
            s.node_count,
            s.edge_count,
            s.concept_count,
            s.canonical_count,
            s.embedded_concepts,
            s.concept_count,
            s.flush_lag,
            s.log_depth,
            s.flush_depth,
            s.dead_lettered,
            s.degraded,
            s.epoch,
            s.daemon_cycles,
            s.canonization_cycles,
            s.canonization_failures,
            // The text half of the same answer, on its own line: an operator
            // reading the tool output should not have to open the structured
            // payload to learn which policy the cycle counts above belong to.
            self.mem.config().promotion_policy.as_str(),
            gc_summary_line(&gc),
        );
        // One payload builder shared with the I2 heartbeat, so a heartbeat can
        // never report different numbers than the tool. With `--ledger` off
        // this is exactly the payload it always was; with it on, the six
        // `ledger_*` keys are appended (I1: the dropped-line counter has to be
        // reachable from `lambo_stats`, or silence is invisible).
        let mut payload = self.stats_json_with_gc(&gc);

        let mut lines = vec![text.clone()];
        if let Some((id, answer)) = &receipt {
            // Redacted like every other model-facing string (N3): a `failed`
            // answer carries a `LamboError`, and a store error can name a DSN.
            lines.push(format!("receipt {id}: {}", redact_urls(&answer.describe())));
        }
        let text = lines.join("\n");

        {
            let obj = payload.as_object_mut().expect("stats_json built an object");
            obj.insert("summary".into(), json!(text));
            obj.insert("warnings".into(), json!([]));
            if let Some((id, answer)) = &receipt {
                let mut r = json!({
                    "id": id.to_string(),
                    "state": answer.tag(),
                    "detail": redact_urls(&answer.describe()),
                });
                // The node ids the ack could not report. Present only on
                // `applied`, and absent — not empty — otherwise, for the same
                // reason the ack omits them: an empty list would be a claim.
                // An agent that needs a node id to reserve it gets it here.
                if let ReceiptAnswer::Applied(s) = answer {
                    let obj = r.as_object_mut().expect("json! built an object");
                    obj.insert("kind".into(), json!(s.kind.tool()));
                    obj.insert("created".into(), json!(s.created));
                    obj.insert("matched".into(), json!(s.matched));
                    // I1's DOGFOOD metric 2 fact set, relocated here from the
                    // ledger call line — see `AppliedSummary`. The `_count`
                    // pair is the TRUE count beside a list truncated at
                    // MAX_RECEIPT_IDS; the other three are emitted only for the
                    // write kind that has one, so an absent key never reads as
                    // a zero.
                    for (key, value) in [
                        ("created_count", Some(s.created_count)),
                        ("matched_count", Some(s.matched_count)),
                        ("semantic_merged", s.semantic_merged),
                        ("reinforced", s.reinforced),
                        ("edges", s.edges),
                        // Applied ≠ embedded (J3-R3-1): present only for the
                        // write kind that can embed (hybrid derive), so an
                        // agent can see a write that applied without its
                        // vector instead of reading `applied` as embedded.
                        ("embedded", s.embedded),
                    ] {
                        if let Some(v) = value {
                            obj.insert(key.into(), json!(v));
                        }
                    }
                }
                obj.insert("receipt".into(), r);
            }
        }

        let mut out = CallToolResult::success(vec![ContentBlock::text(text)]);
        out.structured_content = Some(payload);
        out
    }
}
