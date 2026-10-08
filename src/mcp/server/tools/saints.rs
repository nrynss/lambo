//! `lambo_saints`: the session's canonical memories.

use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::json;

use crate::mcp::server::params::SaintsParams;
use crate::mcp::server::trace::note_facts;
use crate::mcp::server::LamboServer;

impl LamboServer {
    pub(in crate::mcp::server) async fn saints_impl(&self, p: SaintsParams) -> CallToolResult {
        if let Err(e) = self.check_agent_id(&p.agent_id) {
            return e;
        }
        // J1-R1-3: no warning is reachable here — see `derive_impl`.
        let saints = self.mem.canonical_memories();
        note_facts(|| json!({ "canonical_count": saints.len() }));
        let mut text = format!(
            "{} canonical memor{} in session '{}'\n",
            saints.len(),
            if saints.len() == 1 { "y" } else { "ies" },
            self.mem.session().0
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
        out.structured_content = Some(json!({
            "summary": text,
            "saints": rows,
            "warnings": [],
        }));
        out
    }
}
