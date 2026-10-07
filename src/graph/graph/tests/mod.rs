//! Unit tests for the in-memory `Graph`, grouped by subject.

use super::*;
use chrono::{DateTime, TimeZone, Utc};
use uuid::Uuid;

mod accesses;
mod embeddings;
mod mutation_log;
mod root_goal;
mod snapshot;
mod structure;
mod transitions;

fn ts(minutes: i64) -> DateTime<Utc> {
    let base = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
    base + chrono::Duration::minutes(minutes)
}

fn sid() -> SessionId {
    SessionId::from("test-session")
}

fn interaction(id: u64, prev: Option<NodeId>, at_min: i64) -> Interaction {
    Interaction {
        event_time: None,
        id: NodeId(Uuid::from_u64_pair(1, id)),
        session_id: sid(),
        agent_id: crate::types::AgentId::from("agent-a"),
        prompt_text: Some(format!("prompt {id}")),
        previous_id: prev,
        created_at: ts(at_min),
    }
}

fn concept(id: u64, origin: NodeId, content: &str) -> Concept {
    Concept {
        id: NodeId(Uuid::from_u64_pair(2, id)),
        session_id: sid(),
        content: content.into(),
        canonical_key: content.into(),
        concept_type: crate::types::ConceptType::Entity,
        origin_interaction: origin,
        origin_agent: crate::types::AgentId::from("agent-a"),
        created_at: ts(0),
        access_count: 0,
        last_accessed: None,
        gc_survived: 0,
        canonization_status: crate::types::CanonizationStatus::None,
        blast_radius: None,
        last_demotion_time: None,
        embedding: None,
        human_confirmed: 0,
        chunk_group_id: None,
    }
}

fn edge(id: u64, src: NodeId, tgt: NodeId, ty: EdgeType, w: f64) -> Edge {
    Edge {
        event_time: None,
        id: NodeId(Uuid::from_u64_pair(3, id)),
        session_id: sid(),
        source: src,
        target: tgt,
        edge_type: ty,
        weight: w,
        reinforcements: 1,
        created_at: ts(0),
        last_reinforced: ts(0),
    }
}

fn uid(u: u64) -> NodeId {
    NodeId(Uuid::from_u64_pair(0, u))
}

/// Helper: fresh graph with one interaction + one derived concept.
fn small_graph() -> (Graph, NodeId, NodeId) {
    let mut g = Graph::new(sid());
    let i = interaction(1, None, 0);
    let iid = i.id;
    g.insert_interaction(i).unwrap();
    let c = concept(1, iid, "user schema");
    let cid = c.id;
    g.insert_concept(c, iid).unwrap();
    (g, iid, cid)
}

// Adve-review T2.1 I4: the owner (T2.3+ `Memory`) wraps Graph in
// `Arc<RwLock<Graph>>` (spec §6.4). Compile-time proof it can.
const _: () = {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}
    let _ = [assert_send::<Graph>, assert_sync::<Graph>];
};
