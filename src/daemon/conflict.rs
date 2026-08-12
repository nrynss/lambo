//! Conflict detection — spec §9, P4 T4.3 (the demo trigger).
//!
//! A conflict is: **two or more active agents with edges to the same node, at
//! least one `Causal`/`Dependency` edge with write activity inside the
//! `conflict_recency_window`** (30s by default; passed in — the function takes
//! `now`, so tests and fixtures mock the clock by simply passing a `now` of
//! their choosing).
//!
//! ## Agent attribution ("who wrote this edge")
//!
//! Edges carry no agent id. An edge is attributed to the agent of its **source**
//! node: an `Interaction`'s `agent_id` (the writer of a `Derives`/`Temporal`
//! edge, which is always interaction-initiated) or a `Concept`'s `origin_agent`
//! (the writer of a concept-to-concept edge — spec §7 `record_action` records
//! dependencies from the concepts the acting agent produced). "Agent X has an
//! edge to node N" == X is the source-agent of at least one edge incident to N
//! (N as source or as target).
//!
//! ## "Active agent"
//!
//! Spec §9 requires "two or more active agents with edges to the same node" but
//! gives no quantitative bound on "active". Interpreted minimally: an agent is
//! **active** when it has an edge to the node (any age) — the recency
//! dimension of a conflict lives in the *write-activity* clause below, not in
//! the agent set. This is the only reading consistent with the planted conflict
//! in `fixtures/session-rest-api.json` (the caching layer: agent-a's `Derives`
//! edge is old, agent-b's `Dependency` edges are fresh — the conflict must
//! still fire) and with [`crate::fixtures::load_store_relative`]'s documented
//! contract ("Makes the P4 conflict / recency window runnable").
//!
//! ## Write activity and the window
//!
//! An edge's latest write is `max(created_at, last_reinforced)` (creation or a
//! duplicate natural-key reinforcement). Write activity falls **inside** the
//! window iff that time is in `[now - window, now]` — inclusive: a write
//! exactly `window` ago or exactly at `now` counts.
//!
//! ## Future-dated edges (mocked `now`)
//!
//! Because `detect` takes `now` explicitly, a fixture or mock may run `now`
//! *earlier* than some edge timestamps. Edges written after `now` are treated
//! as **outside the window**: they have not happened yet at the instant being
//! examined, so they never count as write activity and never move
//! `seconds_ago`. They still count for agent attribution (the agent
//! demonstrably holds an edge to the node).
//!
//! ## `seconds_ago`
//!
//! The age, in whole seconds (truncated), of the most recent qualifying write
//! — the latest `Causal`/`Dependency` write inside the window. T5.3 renders
//! "Agent A wrote to it eleven seconds ago" from the payload's `agents` +
//! `seconds_ago`.
//!
//! ## Hot list
//!
//! [`insert_conflicts`] refreshes one entry per hit: the payload carries the
//! agents + `seconds_ago`, and the entry's re-validation predicate is built
//! from the same [`conflict_at`] logic and the same `now` used to detect
//! (the recall-time path, T5.3). [`HotList`] dedups by `(node, condition)` —
//! re-running the detector refreshes an existing conflict entry instead of
//! duplicating it. The hits are returned: the daemon loop (T4.6) publishes
//! them on transition and syncs the hot list against them with
//! [`HotList::retain_conditions`], so a conflict that ages out of the
//! recency window is dropped on the next cycle — no captured-`now` ghost.

use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};

use crate::daemon::hotlist::{Condition, HotList, HotListEntry, HotListPayload};
use crate::graph::Graph;
use crate::types::{AgentId, EdgeType, Node, NodeId};

/// Spec §9 `conflict_recency_window`; mirrors [`crate::config::Config`]'s
/// default (30s). `Config` drives the daemon's construction; this const is the
/// module-level default for callers that do not read config.
pub const CONFLICT_RECENCY_WINDOW: Duration = Duration::from_secs(30);

/// One detected conflict (spec §9): a node that ≥2 active agents have edges
/// to, with a recent `Causal`/`Dependency` write.
///
/// This is the daemon detector's own type (pure data); T4.6 maps it to
/// `DaemonEvent::Conflict` and recall (T5.3) renders the payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictHit {
    /// The contested node.
    pub node: NodeId,
    /// Agents with edges to the node, sorted by id (deterministic).
    pub agents: Vec<AgentId>,
    /// Age of the most recent qualifying `Causal`/`Dependency` write at
    /// detection, in seconds.
    pub seconds_ago: u64,
}

/// The agent that wrote `node` — its source side: an interaction's `agent_id`
/// or a concept's `origin_agent`. `None` for a node missing from the graph
/// (defensive; `assert_invariants` guarantees every edge endpoint exists).
fn writer_of(graph: &Graph, node_id: NodeId) -> Option<AgentId> {
    match graph.node(node_id) {
        Some(Node::Interaction(i)) => Some(i.agent_id.clone()),
        Some(Node::Concept(c)) => Some(c.origin_agent.clone()),
        None => None,
    }
}

/// Pure conflict check for one node; shared by [`detect`] and the hot-list
/// re-validation predicates so both use identical logic.
fn conflict_at(
    graph: &Graph,
    node: NodeId,
    window: Duration,
    now: DateTime<Utc>,
) -> Option<ConflictHit> {
    let window_start = now
        - ChronoDuration::from_std(window)
            .expect("conflict_recency_window fits in chrono's duration range");

    let mut agents: Vec<AgentId> = Vec::new();
    let mut newest_qualifying_write: Option<DateTime<Utc>> = None;

    for edge in graph.incident_edges(node) {
        // The writer is the source node's agent; edges where `node` is the
        // target are written by the neighbor, edges where it is the source by
        // the node's own agent.
        let Some(writer) = writer_of(graph, edge.source) else {
            continue;
        };
        if !agents.contains(&writer) {
            agents.push(writer);
        }

        // Write activity inside [now - window, now]; future-dated edges (the
        // edge's last write is after `now`) are outside the window.
        let last_write = edge.last_reinforced.max(edge.created_at);
        let qualifying = matches!(edge.edge_type, EdgeType::Causal | EdgeType::Dependency)
            && last_write <= now
            && last_write >= window_start;
        if qualifying {
            newest_qualifying_write =
                Some(newest_qualifying_write.map_or(last_write, |t| t.max(last_write)));
        }
    }

    if agents.len() < 2 {
        return None;
    }
    let last_write = newest_qualifying_write?;

    agents.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    let seconds_ago = (now - last_write).num_seconds().max(0) as u64;
    Some(ConflictHit {
        node,
        agents,
        seconds_ago,
    })
}

/// Detect every conflict in the graph (spec §9).
///
/// Pure: no locks, no I/O, no hot-list mutation. `now` is passed in so callers
/// (and tests) control the clock. Hits are returned in deterministic
/// node-id order.
pub fn detect(graph: &Graph, window: Duration, now: DateTime<Utc>) -> Vec<ConflictHit> {
    let mut hits: Vec<ConflictHit> = Vec::new();
    for node in graph
        .temporal_chain()
        .iter()
        .copied()
        .chain(graph.concepts().map(|c| c.id))
    {
        if let Some(hit) = conflict_at(graph, node, window, now) {
            hits.push(hit);
        }
    }
    hits.sort_by_key(|h| h.node.0);
    hits
}

/// Detect conflicts and refresh the hot list with one entry per hit.
///
/// [`HotList::insert`] dedups by `(node, condition)`, so re-running this with a
/// fresh `now` refreshes the payload (`seconds_ago`) and the re-validation
/// predicate instead of duplicating (T4.2's contract). Each entry's predicate
/// re-checks the same conflict logic with the same `now` used here (recall's
/// re-validation path, T5.3).
///
/// Returns the hits — the daemon loop emits them on condition transition and
/// uses them as the fresh set it syncs the hot list against
/// ([`HotList::retain_conditions`]; T4.6 finding 2).
pub fn insert_conflicts(
    hot: &mut HotList,
    graph: &Graph,
    window: Duration,
    now: DateTime<Utc>,
) -> Vec<ConflictHit> {
    let hits = detect(graph, window, now);
    for hit in &hits {
        let node = hit.node;
        let payload = HotListPayload::Conflict {
            agents: hit.agents.clone(),
            seconds_ago: hit.seconds_ago,
        };
        let holds = move |g: &Graph| conflict_at(g, node, window, now).is_some();
        let _ = hot.insert(HotListEntry::new(node, Condition::Conflict, payload, holds));
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CanonizationStatus, Concept, ConceptType, Edge, Interaction, SessionId};
    use chrono::TimeZone;
    use uuid::Uuid;

    /// Whole-second timestamp helper (seconds since a fixed epoch).
    fn t(s: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000 + s, 0).unwrap()
    }

    fn sid() -> SessionId {
        SessionId::from("t4.3-conflict")
    }

    fn nid(kind: u64, id: u64) -> NodeId {
        NodeId(Uuid::from_u64_pair(kind, id))
    }

    fn interaction(id: u64, prev: Option<u64>, agent: &str, at: i64) -> Interaction {
        Interaction {
            id: nid(0, id),
            session_id: sid(),
            agent_id: AgentId::from(agent),
            prompt_text: Some("p".into()),
            previous_id: prev.map(|p| nid(0, p)),
            created_at: t(at),
        }
    }

    fn concept(id: u64, origin: NodeId, agent: &str, content: &str, at: i64) -> Concept {
        Concept {
            id: nid(1, id),
            session_id: sid(),
            content: content.into(),
            canonical_key: content.to_string(),
            concept_type: ConceptType::Entity,
            origin_interaction: origin,
            origin_agent: AgentId::from(agent),
            created_at: t(at),
            access_count: 0,
            last_accessed: None,
            gc_survived: 0,
            canonization_status: CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: None,
            embedding: None,
            chunk_group_id: None,
        }
    }

    fn dep_edge(id: u64, source: NodeId, target: NodeId, at: i64) -> Edge {
        Edge {
            id: nid(3, id),
            session_id: sid(),
            source,
            target,
            edge_type: EdgeType::Dependency,
            weight: 0.8,
            reinforcements: 1,
            created_at: t(at),
            last_reinforced: t(at),
        }
    }

    /// A valid (invariant-clean) two-agent session:
    /// - i1 (agent-a) derives c1 "shared node"; c2/c3 (origin agent-b) derive
    ///   from i2 (agent-b).
    /// - Two `Dependency` edges `c2 -> c1` and `c3 -> c1`, both written at
    ///   `dep_at` — so c1 is the contested node and the *only* conflicted one
    ///   (c2/c3 each have edges from a single agent).
    ///
    /// Returns `(graph, c1 id, dependency-edge ids)`.
    fn two_agent_graph(c1_at: i64, dep_at: i64) -> (Graph, NodeId, Vec<NodeId>) {
        let mut g = Graph::new(sid());
        let i1 = interaction(1, None, "agent-a", c1_at - 20);
        let i2 = interaction(2, Some(1), "agent-b", c1_at - 10);
        g.insert_interaction(i1.clone()).unwrap();
        g.insert_interaction(i2.clone()).unwrap();

        let c1 = concept(1, i1.id, "agent-a", "shared node", c1_at);
        let c1_id = c1.id;
        g.insert_concept(c1, i1.id).unwrap();
        let c2 = concept(2, i2.id, "agent-b", "writer b one", c1_at - 10);
        let c2_id = c2.id;
        g.insert_concept(c2, i2.id).unwrap();
        let c3 = concept(3, i2.id, "agent-b", "writer b two", c1_at - 10);
        let c3_id = c3.id;
        g.insert_concept(c3, i2.id).unwrap();

        let e1 = dep_edge(1, c2_id, c1_id, dep_at);
        let e2 = dep_edge(2, c3_id, c1_id, dep_at);
        let dep_ids = vec![e1.id, e2.id];
        g.upsert_edge(e1).unwrap();
        g.upsert_edge(e2).unwrap();
        (g, c1_id, dep_ids)
    }

    // ------------------------------------------------------------------
    // Planted conflict in the session-rest-api fixture (mocked now)
    // ------------------------------------------------------------------

    #[cfg(feature = "fixtures")]
    #[tokio::test]
    async fn rest_api_planted_conflict_fires() {
        use crate::fixtures;
        use crate::store::GraphStore;

        // Rebase the fixture so the most recent concept write (the caching
        // layer, authored by agent-a) lands 5s before the anchor, then detect
        // with `now == anchor` and the spec's 30s window.
        let anchor = Utc::now();
        let store =
            fixtures::load_store_relative("session-rest-api", anchor, Duration::from_secs(5))
                .unwrap();
        let snap = store
            .load_session(&SessionId::from("session-rest-api"))
            .await
            .unwrap();
        let g = Graph::from_snapshot(snap).unwrap();

        let hits = detect(&g, CONFLICT_RECENCY_WINDOW, anchor);
        assert!(!hits.is_empty(), "planted conflict must fire");

        // The planted conflict: the caching layer. agent-b's fresh Dependency
        // edges (api layer, load testing) touch it; agent-a wrote it (Derives).
        let caching = NodeId("f0000000-0000-4000-8000-000000001010".parse().unwrap());
        let hit = hits
            .iter()
            .find(|h| h.node == caching)
            .expect("caching layer is the planted conflict");
        assert_eq!(
            hit.agents,
            vec![AgentId::from("agent-a"), AgentId::from("agent-b")]
        );
        assert_eq!(
            hit.seconds_ago, 5,
            "most recent qualifying write lands 5s before anchor"
        );

        // The demo pillar (user schema) also has two agents with edges — but
        // its most recent Dependency write is 20+ minutes before `now` after
        // this rebase, so it must NOT fire: only write activity inside the
        // window counts. The caching layer is the fixture's only in-window
        // conflict, matching fixtures.rs' `load_store_relative` contract.
        assert_eq!(
            hits.len(),
            1,
            "only the planted conflict may fire at this rebase: {hits:?}"
        );
        let schema = NodeId("f0000000-0000-4000-8000-000000001001".parse().unwrap());
        assert!(
            hits.iter().all(|h| h.node != schema),
            "stale multi-agent writes are not a conflict"
        );
        // Every hit is a genuine conflict: ≥2 distinct agents each time.
        assert!(hits.iter().all(|h| h.agents.len() >= 2));

        // The user-id node is single-agent (only agent-a edges) and stale —
        // the fixture's own negative control.
        let user_id = NodeId("f0000000-0000-4000-8000-000000001013".parse().unwrap());
        assert!(
            hits.iter().all(|h| h.node != user_id),
            "single-agent node must not fire"
        );
    }

    // ------------------------------------------------------------------
    // Single-agent and stale-window negatives (synthetic, mocked now)
    // ------------------------------------------------------------------

    #[test]
    fn single_agent_does_not_fire() {
        let now = t(3600);
        // Everything that touches c1 is agent-a's: c1 and c2 both derive from
        // i1, so the Derives edges (i1 -> c1, i1 -> c2) and the Dependency
        // c2 -> c1 are all attributed to agent-a. i2 (agent-b) exists in the
        // session but has no edges to c1 — despite the fresh in-window writes
        // there is only one agent, so no conflict.
        let mut g = Graph::new(sid());
        let i1 = interaction(1, None, "agent-a", 3600 - 60);
        let i2 = interaction(2, Some(1), "agent-b", 3600 - 50);
        g.insert_interaction(i1.clone()).unwrap();
        g.insert_interaction(i2.clone()).unwrap();
        let c1 = concept(1, i1.id, "agent-a", "shared node", 3600 - 40);
        let c1_id = c1.id;
        g.insert_concept(c1, i1.id).unwrap();
        let c2 = concept(2, i1.id, "agent-a", "same agent", 3600 - 50);
        let c2_id = c2.id;
        g.insert_concept(c2, i1.id).unwrap();
        g.upsert_edge(dep_edge(1, c2_id, c1_id, 3600 - 11)).unwrap();

        assert!(
            g.assert_invariants().is_ok(),
            "test graph must be invariant-clean"
        );
        assert!(
            detect(&g, CONFLICT_RECENCY_WINDOW, now).is_empty(),
            "one agent with fresh writes is not a conflict"
        );
    }

    #[test]
    fn stale_window_does_not_fire() {
        let now = t(3600);
        // Two agents, but the only Causal/Dependency write is 31s ago —
        // outside the 30s window. The old Derives edge alone (40s ago) is not
        // a qualifying write either.
        let (g, _, _) = two_agent_graph(3600 - 40, 3600 - 31);
        assert!(
            detect(&g, CONFLICT_RECENCY_WINDOW, now).is_empty(),
            "write outside the window is not a conflict"
        );
    }

    #[test]
    fn window_boundaries_are_inclusive() {
        let now = t(3600);
        // A write exactly `window` ago still counts (inclusive lower bound).
        let (g, c1, _) = two_agent_graph(3600 - 40, 3600 - 30);
        let hits = detect(&g, CONFLICT_RECENCY_WINDOW, now);
        assert_eq!(hits.len(), 1, "exactly one contested node");
        assert_eq!(hits[0].node, c1);
        assert_eq!(hits[0].seconds_ago, 30);

        // A write exactly at `now` counts too (inclusive upper bound).
        let (g, c1, _) = two_agent_graph(3600 - 40, 3600);
        let hits = detect(&g, CONFLICT_RECENCY_WINDOW, now);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].node, c1);
        assert_eq!(hits[0].seconds_ago, 0);
    }

    #[test]
    fn future_dated_edges_are_outside_the_window() {
        let now = t(100);
        // Both agents' edges exist, but every write is dated after `now` —
        // none has "happened yet", so nothing counts as write activity.
        let (g, _, _) = two_agent_graph(110, 105);
        assert!(
            detect(&g, CONFLICT_RECENCY_WINDOW, now).is_empty(),
            "future-dated writes are outside the window"
        );

        // Mixed: agent-a's write is old (outside the window), agent-b's
        // Dependency write is future-dated. Two agents, but no qualifying
        // write inside [now - window, now].
        let (g, _, _) = two_agent_graph(60, 105);
        assert!(
            detect(&g, CONFLICT_RECENCY_WINDOW, now).is_empty(),
            "future-dated edges must not count as write activity"
        );
    }

    #[test]
    fn empty_graph_has_no_conflicts() {
        let g = Graph::new(sid());
        assert!(detect(&g, CONFLICT_RECENCY_WINDOW, t(3600)).is_empty());
    }

    // ------------------------------------------------------------------
    // Payload + hot-list integration
    // ------------------------------------------------------------------

    #[test]
    fn payload_carries_agents_and_seconds_ago() {
        let now = t(3600);
        // Dependency write 11s before now: the "eleven seconds ago" sentence
        // data T5.3 renders.
        let (g, c1, _) = two_agent_graph(3600 - 40, 3600 - 11);
        let hits = detect(&g, CONFLICT_RECENCY_WINDOW, now);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].node, c1);
        assert_eq!(
            hits[0].agents,
            vec![AgentId::from("agent-a"), AgentId::from("agent-b")]
        );
        assert_eq!(hits[0].seconds_ago, 11);
    }

    #[test]
    fn detect_is_deterministic_and_sorted() {
        let now = t(3600);
        let (g, _, _) = two_agent_graph(3600 - 40, 3600 - 11);
        let a = detect(&g, CONFLICT_RECENCY_WINDOW, now);
        let b = detect(&g, CONFLICT_RECENCY_WINDOW, now);
        assert_eq!(a, b);
        assert!(
            a.windows(2).all(|w| w[0].node.0 <= w[1].node.0),
            "sorted by node id"
        );
    }

    #[test]
    fn insert_conflicts_refreshes_instead_of_duplicating() {
        let now = t(3600);
        let (mut g, c1, dep_ids) = two_agent_graph(3600 - 40, 3600 - 11);
        let mut hot = HotList::new();

        insert_conflicts(&mut hot, &g, CONFLICT_RECENCY_WINDOW, now);
        assert_eq!(hot.len(), 1);
        let entry = hot.peek().unwrap();
        assert_eq!(entry.node, c1);
        assert_eq!(entry.condition, Condition::Conflict);
        match &entry.payload {
            HotListPayload::Conflict {
                agents,
                seconds_ago,
            } => {
                assert_eq!(
                    *agents,
                    vec![AgentId::from("agent-a"), AgentId::from("agent-b")]
                );
                assert_eq!(*seconds_ago, 11);
            }
            other => panic!("unexpected payload {other:?}"),
        }

        // Re-run the detector with a later `now`: the (node, condition) pair
        // is already present, so the entry refreshes — payload and predicate
        // update, the list does not grow.
        let later = t(3600 + 5);
        insert_conflicts(&mut hot, &g, CONFLICT_RECENCY_WINDOW, later);
        assert_eq!(hot.len(), 1, "refresh must not duplicate entries");
        match &hot.peek().unwrap().payload {
            HotListPayload::Conflict { seconds_ago, .. } => {
                assert_eq!(*seconds_ago, 16, "payload refreshed with the new now");
            }
            other => panic!("unexpected payload {other:?}"),
        }

        // Re-validation: the holds predicate re-checks the same conflict logic
        // (same window + now). Remove the recent Dependency edges — the
        // condition stops holding and the node drops off the list.
        for id in dep_ids {
            g.remove_edge(id).unwrap();
        }
        assert!(
            !hot.revalidate(&g, c1),
            "conflict gone → entry evicted on revalidation"
        );
        assert!(hot.is_empty());
    }
}
