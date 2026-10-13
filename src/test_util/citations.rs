//! Small graph builders for the #36 PR 4 resolver and citation tests.

#![cfg(test)]

use chrono::{TimeZone, Utc};

use crate::graph::derive::{derive, ParentOf};
use crate::graph::Graph;
use crate::types::{
    AgentId, CanonizationStatus, Concept, ConceptType, Interaction, NodeId, SessionId,
};

/// A graph for session `sid` with one interaction; returns both.
pub fn graph(sid: &str) -> (Graph, NodeId) {
    let mut g = Graph::new(SessionId::from(sid));
    let i = Interaction {
        event_time: None,
        id: NodeId::new(),
        session_id: SessionId::from(sid),
        agent_id: AgentId::from("agent-a"),
        prompt_text: Some("citations".into()),
        previous_id: None,
        created_at: Utc.timestamp_opt(1_752_000_000, 0).unwrap(),
    };
    let id = i.id;
    g.insert_interaction(i).unwrap();
    (g, id)
}

/// Derive `content` as `concept_type` through the real write path; its node.
pub fn concept(g: &mut Graph, i: NodeId, content: &str, concept_type: ConceptType) -> NodeId {
    let out = derive(
        g,
        i,
        &AgentId::from("agent-a"),
        &[(content, concept_type)],
        &ParentOf::none(),
        10,
    )
    .unwrap();
    out.created
        .first()
        .or(out.matched.first())
        .copied()
        .expect("derive wrote it")
}

/// Insert a concept with a chosen id (to make two ids share a short-id
/// prefix).
pub fn concept_with_id(
    g: &mut Graph,
    i: NodeId,
    id: NodeId,
    content: &str,
    concept_type: ConceptType,
) -> NodeId {
    let c = Concept {
        id,
        session_id: g.session_id().clone(),
        content: content.to_string(),
        canonical_key: crate::graph::canonical::canonical_key(content, |_| None),
        concept_type,
        origin_interaction: i,
        origin_agent: AgentId::from("agent-a"),
        created_at: Utc.timestamp_opt(1_752_000_060, 0).unwrap(),
        access_count: 0,
        last_accessed: None,
        gc_survived: 0,
        canonization_status: CanonizationStatus::None,
        blast_radius: None,
        last_demotion_time: None,
        embedding: None,
        human_confirmed: 0,
        embedding_source: None,
        chunk_group_id: None,
    };
    g.insert_concept(c, i).unwrap();
    id
}

/// An id whose simple hex form starts with `prefix` (hex, up to 16 chars),
/// distinct per `n`.
pub fn id_with_prefix(prefix: &str, n: u64) -> NodeId {
    let mut hex = prefix.to_string();
    while hex.len() < 16 {
        hex.push('0');
    }
    let hi = u64::from_str_radix(&hex[..16], 16).unwrap();
    NodeId(uuid::Uuid::from_u64_pair(hi, n))
}
