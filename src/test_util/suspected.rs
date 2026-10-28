//! One shared case for the #198 store surface: a `SuspectedSupersedes`
//! edge round-trips through every adapter's flush and load, confers no
//! blast radius, and lapses when its batch deletes it (a declared
//! relation) or deletes either concept (collection). Checked on MemoryStore
//! and SQLite offline, the Tiered wrapper, Postgres by hand through
//! `store::pg::suspected_live`, and Cockroach inside `conformance_suite`,
//! all through the [`GraphStore`] trait only.
//!
//! Fixed ids and the session `suspected-case`, so a re-run against a
//! persistent database upserts the same rows.

#![cfg(test)]

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use uuid::Uuid;

use crate::store::GraphStore;
use crate::types::{
    AgentId, CanonizationStatus, Concept, ConceptType, Edge, EdgeType, Interaction, Mutation,
    MutationBatch, NodeId, SessionId,
};

/// The session the case lives in.
pub fn session() -> SessionId {
    SessionId::from("suspected-case")
}

fn now() -> DateTime<Utc> {
    chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 10, 28, 12, 0, 0).unwrap()
}

fn at(minutes: i64) -> DateTime<Utc> {
    now() - ChronoDuration::minutes(minutes)
}

fn interaction_id(n: u64) -> NodeId {
    NodeId(Uuid::from_u64_pair(0x0198_0001, n))
}

/// Concept `n` (1: the older fact, 2: its suspected replacement, 3: a
/// second suspected replacement written later).
pub fn concept_id(n: u64) -> NodeId {
    NodeId(Uuid::from_u64_pair(0x0198_0002, n))
}

fn edge_id(n: u64) -> NodeId {
    NodeId(Uuid::from_u64_pair(0x0198_0003, n))
}

fn interaction(n: u64, minutes: i64, previous: Option<u64>) -> Interaction {
    Interaction {
        id: interaction_id(n),
        session_id: session(),
        agent_id: AgentId::from("agent-198"),
        prompt_text: Some(format!("turn {n}")),
        previous_id: previous.map(interaction_id),
        created_at: at(minutes),
        event_time: None,
    }
}

fn concept(n: u64, content: &str, origin: u64, minutes: i64) -> Concept {
    Concept {
        id: concept_id(n),
        session_id: session(),
        content: content.into(),
        canonical_key: content.to_lowercase(),
        concept_type: ConceptType::Logic,
        origin_interaction: interaction_id(origin),
        origin_agent: AgentId::from("agent-198"),
        created_at: at(minutes),
        access_count: 0,
        last_accessed: None,
        gc_survived: 0,
        human_confirmed: 0,
        canonization_status: CanonizationStatus::None,
        blast_radius: None,
        last_demotion_time: None,
        embedding: None,
        chunk_group_id: None,
        embedding_source: None,
    }
}

fn suspected(n: u64, newer: u64, older: u64, minutes: i64) -> Edge {
    Edge {
        event_time: None,
        id: edge_id(n),
        session_id: session(),
        source: concept_id(newer),
        target: concept_id(older),
        edge_type: EdgeType::SuspectedSupersedes,
        weight: 1.0,
        reinforcements: 1,
        created_at: at(minutes),
        last_reinforced: at(minutes),
    }
}

/// The rows, as a legal graph writes them (temporal chain, `Derives`
/// edges): three interactions, three concepts, two suspected edges
/// (2 -> 1 and 3 -> 1).
pub fn store_batch() -> MutationBatch {
    let mut g = crate::graph::Graph::new(session());
    for (n, minutes, previous) in [(1, 30, None), (2, 20, Some(1)), (3, 10, Some(2))] {
        g.insert_interaction(interaction(n, minutes, previous))
            .expect("interaction");
    }
    for (n, content, minutes) in [
        (1, "the embedder listens on port 8080", 30),
        (2, "the embedder listens on port 8081", 20),
        (3, "the embedder listens on port 8082", 10),
    ] {
        g.insert_concept(concept(n, content, n, minutes), interaction_id(n))
            .expect("concept");
    }
    g.upsert_edge(suspected(1, 2, 1, 20)).expect("edge 1");
    g.upsert_edge(suspected(2, 3, 1, 10)).expect("edge 2");
    g.drain_log()
}

fn suspected_edges(snap: &crate::types::GraphSnapshot) -> Vec<(NodeId, NodeId)> {
    let mut out: Vec<(NodeId, NodeId)> = snap
        .edges
        .iter()
        .filter(|e| e.session_id == session() && e.edge_type == EdgeType::SuspectedSupersedes)
        .map(|e| (e.source, e.target))
        .collect();
    out.sort_by_key(|(s, t)| (s.0, t.0));
    out
}

/// Flush the case, reload it, and check the edge type, its endpoints and
/// its columns round-trip; that it gives the older concept no blast
/// radius; that deleting an edge (a declared relation settling it) and a
/// concept (collection, the graph's `DeleteEdge`s first) remove it.
pub async fn check_round_trip(store: &dyn GraphStore) {
    store.flush(&store_batch(), None).await.expect("flush");
    let snap = store.load_session(&session()).await.expect("load");
    assert_eq!(
        suspected_edges(&snap),
        vec![
            (concept_id(2), concept_id(1)),
            (concept_id(3), concept_id(1))
        ]
    );
    let loaded = snap
        .edges
        .iter()
        .find(|e| e.id == edge_id(1))
        .expect("edge 1 loaded");
    let want = suspected(1, 2, 1, 20);
    assert_eq!(loaded.edge_type, want.edge_type);
    assert_eq!(loaded.weight, want.weight);
    assert_eq!(loaded.created_at, want.created_at);
    assert_eq!(loaded.event_time, None);
    // The reloaded graph is legal: a concept-to-concept edge type.
    crate::graph::Graph::from_snapshot(snap.clone())
        .expect("the snapshot loads as a graph")
        .assert_invariants()
        .expect("invariants");

    // Informational: no dependents, so no blast radius.
    for n in [1, 2, 3] {
        assert_eq!(
            store
                .blast_radius(&session(), concept_id(n), std::time::Duration::ZERO, now())
                .await
                .expect("blast_radius"),
            0,
            "concept {n}"
        );
    }

    // A declared relation settles edge 1: the graph deletes it.
    let mut settle = MutationBatch::new();
    settle.push(Mutation::DeleteEdge { id: edge_id(1) });
    store.flush(&settle, None).await.expect("flush settle");
    let snap = store.load_session(&session()).await.expect("reload");
    assert_eq!(suspected_edges(&snap), vec![(concept_id(3), concept_id(1))]);

    // Collecting the older concept takes its last edge with it: the
    // graph's own removal, which deletes every incident edge first.
    let mut g = crate::graph::Graph::from_snapshot(snap).expect("graph");
    let _ = g.drain_log();
    g.remove_node(concept_id(1)).expect("remove");
    store
        .flush(&g.drain_log(), None)
        .await
        .expect("flush collect");
    let snap = store.load_session(&session()).await.expect("reload");
    assert!(suspected_edges(&snap).is_empty());
    assert!(!snap.concepts.iter().any(|c| c.id == concept_id(1)));
}
