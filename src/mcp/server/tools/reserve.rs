//! `lambo_reserve`: take or release a §11 soft lock under the caller's id.

use std::time::Duration;

use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::json;

use crate::mcp::server::params::{check_size, ReserveParams};
use crate::mcp::server::response::{attach_warnings, bad_param, conflict_err, tool_err};
use crate::mcp::server::trace::note_facts;
use crate::mcp::server::LamboServer;
use crate::surface::limits::MAX_RESERVE_TTL_SECS;
use crate::surface::validate::check_in_range;
use crate::types::{LamboError, NodeId};

impl LamboServer {
    pub(in crate::mcp::server) async fn reserve_impl(&self, p: ReserveParams) -> CallToolResult {
        let releasing = p.release.unwrap_or(false);
        // I1: grant/refusal for EVERY exit of this tool, set before the first
        // one can be taken — which means before the `agent_id` check, not after.
        // Each success path overwrites it with `granted: true`; anything that
        // returns early — an empty or oversized `agent_id`, a bad node_id, a
        // `Conflict` from a lock another agent holds — leaves this standing, so a
        // refusal can never be recorded as a grant by a path somebody forgot to
        // annotate. The id check used to run first, which left its own two exits
        // reporting `op=None granted=None`.
        let op = if releasing { "release" } else { "reserve" };
        note_facts(|| json!({ "op": op, "granted": false }));
        // J1: the caller's id IS the lock identity. No refusal here any more —
        // two clients through one serve contend for real, each under its own
        // name, which is the whole point. The id is unauthenticated, and the
        // tool description says so rather than this code pretending otherwise.
        let acting = match self.caller_agent(&p.agent_id) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let mut warnings: Vec<String> = Vec::new();
        // N5: node_id is a client string too — size- and control-checked before
        // it is parsed, so the same uniform guard covers every field.
        if let Err(e) = check_size("node_id", &p.node_id) {
            return e;
        }
        let node_id = match uuid::Uuid::parse_str(p.node_id.trim()) {
            Ok(u) => NodeId(u),
            Err(e) => return bad_param(format!("node_id must be a UUID: {e}")),
        };

        if releasing {
            return match self.mem.release_as(&acting, node_id) {
                Ok(()) => {
                    note_facts(|| json!({ "op": "release", "granted": true }));
                    let msg = format!("released {}", node_id.0);
                    let mut out = CallToolResult::success(vec![ContentBlock::text(msg.clone())]);
                    attach_warnings(&mut out, &warnings);
                    out.structured_content =
                        Some(json!({ "released": true, "node_id": node_id.0.to_string(),
                                     "summary": msg, "warnings": warnings }));
                    out
                }
                Err(LamboError::SoftLock(msg)) => {
                    conflict_err("lambo_reserve (release)", &msg, "nothing was released")
                }
                Err(e) => tool_err("lambo_reserve (release)", e),
            };
        }

        let ttl_secs = p.ttl_seconds.unwrap_or(30);
        if let Err(e) = check_in_range("ttl_seconds", ttl_secs, 1, MAX_RESERVE_TTL_SECS) {
            return bad_param(e);
        }
        let reservation = match self
            .mem
            .reserve_as(&acting, node_id, Duration::from_secs(ttl_secs))
        {
            Ok(r) => r,
            Err(LamboError::SoftLock(msg)) => {
                return conflict_err("lambo_reserve", &msg, "nothing was reserved")
            }
            Err(e) => return tool_err("lambo_reserve", e),
        };
        note_facts(|| json!({ "op": "reserve", "granted": true, "ttl_seconds": ttl_secs }));
        warnings.push(
            "reservations are advisory and RAM-local: they are lost on server restart".into(),
        );
        let summary = format!(
            "reserved {} until {} for agent '{}'",
            node_id.0,
            reservation.expires_at.to_rfc3339(),
            reservation.agent_id.0
        );
        let mut out = CallToolResult::success(vec![ContentBlock::text(summary.clone())]);
        attach_warnings(&mut out, &warnings);
        out.structured_content = Some(json!({
            "summary": summary,
            "node_id": node_id.0.to_string(),
            "agent_id": reservation.agent_id.0,
            "expires_at": reservation.expires_at.to_rfc3339(),
            "warnings": warnings,
        }));
        out
    }
}
