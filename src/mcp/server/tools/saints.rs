//! `lambo_saints`: the session's canonical memories.

use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::json;

use crate::mcp::server::params::SaintsParams;
use crate::mcp::server::trace::note_facts;
use crate::mcp::server::LamboServer;

use super::scope::{with_scope, with_session};

impl LamboServer {
    pub(in crate::mcp::server) async fn saints_impl(&self, p: SaintsParams) -> CallToolResult {
        if let Err(e) = self.check_agent_id(&p.agent_id) {
            return e;
        }
        // #33 PR 4: a layer's saints are listed from the layer, read alone
        // as `/mcp/s/<layer>` would (its rate token drawn).
        let target = match self.target("lambo_saints", p.scope.as_deref()) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let mem = target.mem(&self.mem);
        // J1-R1-3: no warning is reachable here — see `derive_impl`.
        let saints = mem.canonical_memories();
        note_facts(|| {
            with_scope(
                json!({ "canonical_count": saints.len() }),
                p.scope.as_deref(),
            )
        });
        let mut text = format!(
            "{} canonical memor{} in session '{}'\n",
            saints.len(),
            if saints.len() == 1 { "y" } else { "ies" },
            mem.session().0
        );
        for s in &saints {
            text.push_str(&format!(
                "  {} [{:?}, canonical]  blast_radius={}  accesses={}  since {}\n",
                s.content,
                s.concept_type,
                s.blast_radius,
                s.access_count,
                s.created_at.to_rfc3339()
            ));
        }
        let rows: Vec<_> = saints
            .iter()
            .map(|s| {
                json!({
                    "node_id": s.node_id.0.to_string(),
                    "content": s.content,
                    "concept_type": s.concept_type,
                    "blast_radius": s.blast_radius,
                    "access_count": s.access_count,
                    "created_at": s.created_at.to_rfc3339(),
                })
            })
            .collect();
        let mut out = CallToolResult::success(vec![ContentBlock::text(text.clone())]);
        out.structured_content = Some(with_session(
            json!({
                "summary": text,
                "saints": rows,
                "warnings": [],
            }),
            &target,
        ));
        out
    }
}
