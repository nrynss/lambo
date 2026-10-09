//! Unit tests for the GC sweep, grouped by subject.

use super::*;
use crate::graph::Graph;
use crate::types::{
    AgentId, CanonizationEvent, ConceptType, Edge, Interaction, Mutation, Node, SessionId,
};
use chrono::TimeZone;
use uuid::Uuid;

mod cap_and_bumps;
mod collection;
mod score_cut;
mod triggers;

fn ts(m: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(1_700_000_000 + m * 60, 0).unwrap()
}

fn sid() -> SessionId {
    SessionId::from("t4.5-gc")
}

fn nid(n: u64) -> NodeId {
    NodeId(Uuid::from_u64_pair(0, n))
}

/// Deterministic clock for GC runs: every `ts(0)` write is 100 min old —
/// past the 1h TTL but heavy edges (>= 0.5) always survive step 1.
fn default_params() -> GcParams {
    GcParams {
        now: ts(100),
        ..Default::default()
    }
}

/// `default_params` with the clock moved past [`GC_RECENCY_WINDOW`], so
/// every `ts(0)` concept has zero eviction recency (issue #29: GC's
/// recency is time since last touch, not position in the session span).
fn aged_params() -> GcParams {
    GcParams {
        now: ts(0) + GC_RECENCY_WINDOW + ChronoDuration::days(1),
        ..Default::default()
    }
}

fn interaction(id: u64, prev: Option<u64>) -> Interaction {
    Interaction {
        event_time: None,
        id: nid(id),
        session_id: sid(),
        agent_id: AgentId::from("agent-a"),
        prompt_text: Some("p".into()),
        previous_id: prev.map(nid),
        created_at: ts(0),
    }
}

fn concept(id: u64, origin: u64, content: &str, ty: ConceptType) -> Concept {
    Concept {
        id: nid(id),
        session_id: sid(),
        content: content.into(),
        canonical_key: content.to_string(),
        concept_type: ty,
        origin_interaction: nid(origin),
        origin_agent: AgentId::from("agent-a"),
        created_at: ts(0),
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
    }
}

fn edge(id: u64, src: u64, tgt: u64, ty: EdgeType, weight: f64, at_min: i64) -> Edge {
    Edge {
        event_time: None,
        id: nid(id),
        session_id: sid(),
        source: nid(src),
        target: nid(tgt),
        edge_type: ty,
        weight,
        reinforcements: 1,
        created_at: ts(at_min),
        last_reinforced: ts(at_min),
    }
}

/// Insert `c` then strip its `Derives` edge, leaving an isolated concept
/// (orphan / disconnected-component material).
fn insert_isolated(g: &mut Graph, c: Concept, origin: u64) -> NodeId {
    let id = c.id;
    let oid = nid(origin);
    g.insert_concept(c, oid).unwrap();
    let derives_id = g
        .edge_between(oid, id, EdgeType::Derives)
        .expect("insert_concept creates a Derives edge")
        .id;
    g.remove_edge(derives_id).unwrap();
    id
}

// ------------------------------------------------------------------
// Issue #29 — step-2 protections, the collection cap, the trigger
// ------------------------------------------------------------------

/// A session with one interaction-spanning hub (13 → 14/15/16) so a
/// leaf's density is 1/4, plus the given low-value leaves, each with only
/// its `Derives` edge. Leaves are created at `ts(0)`.
fn hub_session(leaves: &[(u64, ConceptType)]) -> Graph {
    hub_session_patched(leaves, |c| c)
}

/// [`hub_session`] with every concept passed through `patch` before it is
/// inserted (access counts, timestamps).
fn hub_session_patched(leaves: &[(u64, ConceptType)], patch: impl Fn(Concept) -> Concept) -> Graph {
    let mut g = Graph::new(sid());
    let i1 = interaction(1, None);
    let i2 = Interaction {
        created_at: ts(100),
        ..interaction(2, Some(1))
    };
    let iid = i1.id;
    g.insert_interaction(i1).unwrap();
    g.insert_interaction(i2).unwrap();
    for id in [13u64, 14, 15, 16] {
        g.insert_concept(
            patch(concept(id, 1, &format!("anchor {id}"), ConceptType::Entity)),
            iid,
        )
        .unwrap();
    }
    for (eid, tgt) in [(101u64, 14u64), (102, 15), (103, 16)] {
        g.upsert_edge(edge(eid, 13, tgt, EdgeType::Dependency, 1.0, 0))
            .unwrap();
    }
    for (id, ty) in leaves {
        g.insert_concept(patch(concept(*id, 1, &format!("leaf {id}"), *ty)), iid)
            .unwrap();
    }
    g
}
