//! Unit tests for the daemon loop, grouped by subject.

use super::*;
use crate::graph::Graph;
use crate::types::{
    AgentId, CanonizationStatus, Concept, ConceptType, Edge, EdgeType, Interaction, SessionId,
};
use chrono::{TimeZone, Utc};
use uuid::Uuid;

mod conditions;
mod gc_triggers;
mod loop_control;
mod recall;

fn ts(m: i64) -> chrono::DateTime<Utc> {
    Utc.timestamp_opt(1_700_000_000 + m * 60, 0).unwrap()
}

fn sid() -> SessionId {
    SessionId::from("t4.1-daemon")
}

fn interaction(id: u64) -> Interaction {
    Interaction {
        event_time: None,
        id: NodeId(Uuid::from_u64_pair(0, id)),
        session_id: sid(),
        agent_id: AgentId::from("agent-a"),
        prompt_text: Some("p".into()),
        previous_id: None,
        created_at: ts(0),
    }
}

fn concept(id: u64, origin: NodeId, content: &str) -> Concept {
    Concept {
        id: NodeId(Uuid::from_u64_pair(1, id)),
        session_id: sid(),
        content: content.into(),
        canonical_key: content.to_string(),
        concept_type: ConceptType::Entity,
        origin_interaction: origin,
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

/// A locked graph with one interaction and one concept (epoch 3:
/// interaction node + concept node + Derives edge).
fn locked_graph_with_one_concept() -> (Arc<RwLock<Graph>>, NodeId) {
    let mut g = Graph::new(sid());
    let i = interaction(1);
    let iid = i.id;
    g.insert_interaction(i).unwrap();
    let c = concept(1, iid, "user schema");
    let cid = c.id;
    g.insert_concept(c, iid).unwrap();
    (Arc::new(RwLock::new(g)), cid)
}

/// Wake the daemon and wait for the woken cycle to COMPLETE (XP-6).
///
/// Every negative assertion in this suite ("nothing was published") uses
/// this instead of sleeping: `Daemon::cycles` only advances after a full
/// cycle body ran, so the assertion cannot pass vacuously because the cycle
/// had not started yet. Under `start_paused` the wait is virtual-time, so it
/// is also free.
async fn wake_and_settle(daemon: &Daemon) {
    let before = daemon.cycles();
    daemon.wake();
    wait_until(|| daemon.cycles() > before).await;
}

/// Poll `cond` until true or a 2s timeout elapses (test helper).
async fn wait_until(cond: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("condition not met within 2s");
}

// ------------------------------------------------------------------
// T4.6 — event transport wiring: detectors publish, hot list maintains,
// GC runs on the interval, receivers never block the loop.
// ------------------------------------------------------------------

fn wall_ts(secs_ago: i64) -> chrono::DateTime<Utc> {
    Utc::now() - chrono::Duration::seconds(secs_ago)
}

fn interaction_at(
    id: u64,
    prev: Option<u64>,
    agent: &str,
    at: chrono::DateTime<Utc>,
) -> Interaction {
    Interaction {
        event_time: None,
        id: NodeId(Uuid::from_u64_pair(0, id)),
        session_id: sid(),
        agent_id: AgentId::from(agent),
        prompt_text: Some("p".into()),
        previous_id: prev.map(|p| NodeId(Uuid::from_u64_pair(0, p))),
        created_at: at,
    }
}

fn concept_at(
    id: u64,
    origin: NodeId,
    agent: &str,
    content: &str,
    at: chrono::DateTime<Utc>,
) -> Concept {
    Concept {
        id: NodeId(Uuid::from_u64_pair(1, id)),
        session_id: sid(),
        content: content.into(),
        canonical_key: content.to_string(),
        concept_type: ConceptType::Entity,
        origin_interaction: origin,
        origin_agent: AgentId::from(agent),
        created_at: at,
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

fn dep_edge_at(id: u64, source: NodeId, target: NodeId, at: chrono::DateTime<Utc>) -> Edge {
    Edge {
        event_time: None,
        id: NodeId(Uuid::from_u64_pair(3, id)),
        session_id: sid(),
        source,
        target,
        edge_type: EdgeType::Dependency,
        weight: 1.0,
        reinforcements: 1,
        created_at: at,
        last_reinforced: at,
    }
}

/// A two-agent graph with a live conflict on `c1`: agent-a's `Derives`
/// edge and agent-b's fresh `Dependency` edge (5s before now) both touch
/// it, so `conflict::detect` fires every cycle while the write stays in
/// the 30s window. Returns `(graph, c1, dependency edge, agent-b
/// interaction)` — the interaction id for planting extra agent-b writes.
fn conflicted_graph() -> (Arc<RwLock<Graph>>, NodeId, NodeId, NodeId) {
    let mut g = Graph::new(sid());
    let i1 = interaction_at(1, None, "agent-a", wall_ts(60));
    let i1_id = i1.id;
    g.insert_interaction(i1).unwrap();
    let i2 = interaction_at(2, Some(1), "agent-b", wall_ts(30));
    let i2_id = i2.id;
    g.insert_interaction(i2).unwrap();
    let c1 = concept_at(1, i1_id, "agent-a", "shared node", wall_ts(60));
    let c1_id = c1.id;
    g.insert_concept(c1, i1_id).unwrap();
    let c2 = concept_at(2, i2_id, "agent-b", "writer b", wall_ts(30));
    let c2_id = c2.id;
    g.insert_concept(c2, i2_id).unwrap();
    let e = dep_edge_at(1, c2_id, c1_id, wall_ts(5));
    let e_id = e.id;
    g.upsert_edge(e).unwrap();
    (Arc::new(RwLock::new(g)), c1_id, e_id, i2_id)
}
