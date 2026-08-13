//! Stage 3 — Canonical (T6.3, spec §10).
//!
//! Pure predicate: reports whether a concept currently clears Stage 3
//! blast-radius evidence and is outside the re-promotion cooldown. It
//! does **not** apply a `Venerable → Canonical` transition (T6.4 owns
//! writes).
//!
//! A concept passes when **both**:
//!
//! 1. **`store.blast_radius(session, node, min_edge_age, now) > 5`**
//!    (strict). The store returns [`u64`]; the comparison is against
//!    `5u64`. [`Concept::blast_radius`] is a frozen `Option<i32>`
//!    (CON-6) and is **not** consulted — never a silent `as i32`.
//! 2. **Not inside the re-promotion cooldown.** `last_demotion_time`
//!    lives on the [`Concept`] in `graph`, not on the store query. If
//!    it is `Some(t)` and `now < t + cooldown`, the predicate refuses
//!    even when blast radius is above the floor. `None` is not a
//!    cooldown. Default cooldown is
//!    [`crate::Config::canonization_repromotion_cooldown`] (300s).
//!
//! `now` is injected — the predicate has no wall clock, and (F8) neither
//! does the store: the same instant anchors the cooldown comparison and the
//! `min_edge_age` cutoff, so a mocked clock drives both gates.
//!
//! `min_edge_age` is forwarded unchanged (same inflation idea as
//! Stage 2). Callers that want the T1.4 / T3.6 fixture numbers
//! (`user schema` = 8, `api layer` = 1) pass [`Duration::ZERO`].
//! Callers that want the inflation guard pass
//! [`crate::Config::canonization_edge_min_age`] (default 60s).
//!
//! Stage 3 is evidence-only: [`CanonizationStatus`] is not consulted.
//! Venerable is not a prerequisite (T6.4 sequences transitions).

use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};

use crate::graph::Graph;
use crate::store::GraphStore;
use crate::types::{Node, NodeId, SessionId, StoreError};

/// Blast-radius floor (spec §10). Pass iff the store count is **strictly**
/// greater than this `u64` (CON-6: do not narrow to `i32`).
const MIN_BLAST_RADIUS: u64 = 5;

/// Whether `node` currently clears Stage 3 on `store` / `graph`.
///
/// Cooldown is read from the concept in `graph`. Blast radius is
/// queried from `store` and compared as `u64`.
pub async fn stage3_passes(
    store: &impl GraphStore,
    graph: &Graph,
    session: &SessionId,
    node: NodeId,
    min_edge_age: Duration,
    cooldown: Duration,
    now: DateTime<Utc>,
) -> Result<bool, StoreError> {
    let blast = store.blast_radius(session, node, min_edge_age, now).await?;
    if in_repromotion_cooldown(graph, node, cooldown, now) {
        return Ok(false);
    }
    Ok(blast > MIN_BLAST_RADIUS)
}

/// `true` when `last_demotion_time` is `Some(t)` and `now < t + cooldown`.
///
/// A missing node, a non-concept, or `last_demotion_time == None` is
/// not a cooldown. An unrepresentable `cooldown` is treated as still
/// cooling (conservative; config default is 300s).
fn in_repromotion_cooldown(
    graph: &Graph,
    node: NodeId,
    cooldown: Duration,
    now: DateTime<Utc>,
) -> bool {
    let Some(t) = last_demotion_time(graph, node) else {
        return false;
    };
    let Ok(cd) = ChronoDuration::from_std(cooldown) else {
        return true;
    };
    match t.checked_add_signed(cd) {
        Some(until) => now < until,
        None => true,
    }
}

fn last_demotion_time(graph: &Graph, node: NodeId) -> Option<DateTime<Utc>> {
    match graph.node(node) {
        Some(Node::Concept(c)) => c.last_demotion_time,
        _ => None,
    }
}

#[cfg(all(test, feature = "store-memory"))]
mod tests {
    use super::*;
    use crate::store::{GraphStore, MemoryStore};
    use crate::types::{
        AgentId, CanonizationStatus, Concept, ConceptType, Edge, EdgeType, Interaction, Mutation,
        MutationBatch, Node,
    };
    use chrono::{TimeZone, Utc};
    use uuid::Uuid;

    fn sid() -> SessionId {
        SessionId::from("test-session")
    }

    fn nid(kind: u64, id: u64) -> NodeId {
        NodeId(Uuid::from_u64_pair(kind, id))
    }

    fn iid(id: u64) -> NodeId {
        nid(1, id)
    }

    fn cid(id: u64) -> NodeId {
        nid(2, id)
    }

    fn ts() -> DateTime<Utc> {
        Utc.timestamp_opt(1_752_000_000, 0).unwrap()
    }

    fn default_cooldown() -> Duration {
        crate::Config::default().canonization_repromotion_cooldown
    }

    fn interaction(id: u64, at: DateTime<Utc>) -> Interaction {
        Interaction {
            id: iid(id),
            session_id: sid(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: Some(format!("i{id}")),
            previous_id: None,
            created_at: at,
        }
    }

    fn concept(
        id: u64,
        origin: u64,
        at: DateTime<Utc>,
        last_demotion: Option<DateTime<Utc>>,
    ) -> Concept {
        Concept {
            id: cid(id),
            session_id: sid(),
            content: format!("c{id}"),
            canonical_key: format!("c{id}"),
            concept_type: ConceptType::Entity,
            origin_interaction: iid(origin),
            origin_agent: AgentId::from("agent-a"),
            created_at: at,
            access_count: 0,
            last_accessed: None,
            gc_survived: 0,
            canonization_status: CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: last_demotion,
            embedding: None,
            chunk_group_id: None,
        }
    }

    fn edge(id: u64, src: u64, tgt: u64, at: DateTime<Utc>) -> Edge {
        Edge {
            id: nid(3, id),
            session_id: sid(),
            source: cid(src),
            target: cid(tgt),
            edge_type: EdgeType::Dependency,
            weight: 1.0,
            reinforcements: 1,
            created_at: at,
            last_reinforced: at,
        }
    }

    async fn seed(
        interactions: &[Interaction],
        concepts: &[Concept],
        edges: &[Edge],
    ) -> MemoryStore {
        let store = MemoryStore::new();
        let mut batch = MutationBatch::new();
        for i in interactions {
            batch.push(Mutation::UpsertNode {
                node: Node::Interaction(i.clone()),
            });
        }
        for c in concepts {
            batch.push(Mutation::UpsertNode {
                node: Node::Concept(c.clone()),
            });
        }
        for e in edges {
            batch.push(Mutation::UpsertEdge { edge: e.clone() });
        }
        store.flush(&batch).await.unwrap();
        store
    }

    /// Hub `cid(10)` plus `n` exclusive dependents (`cid(1..=n)`).
    /// Each dependent has only the hub as an inbound structural source,
    /// so `blast_radius(hub) == n`.
    async fn store_with_blast(n: u64) -> MemoryStore {
        let at = ts();
        let interactions = [interaction(1, at)];
        let mut concepts = vec![concept(10, 1, at, None)];
        let mut edges = Vec::new();
        for i in 1..=n {
            concepts.push(concept(i, 1, at, None));
            edges.push(edge(i, 10, i, at));
        }
        seed(&interactions, &concepts, &edges).await
    }

    fn graph_with(hub: Concept) -> Graph {
        let mut g = Graph::new(sid());
        let i = interaction(1, hub.created_at);
        let iid = i.id;
        g.insert_interaction(i).unwrap();
        g.insert_concept(hub, iid).unwrap();
        g
    }

    async fn passes(
        store: &MemoryStore,
        graph: &Graph,
        node: NodeId,
        cooldown: Duration,
        now: DateTime<Utc>,
    ) -> bool {
        stage3_passes(store, graph, &sid(), node, Duration::ZERO, cooldown, now)
            .await
            .unwrap()
    }

    /// Strict `>`: blast == 5 fails, blast == 6 passes.
    #[tokio::test]
    async fn blast_5_fails_blast_6_passes() {
        let now = ts();
        let cooldown = default_cooldown();
        let g5 = graph_with(concept(10, 1, now, None));
        let s5 = store_with_blast(5).await;
        assert_eq!(
            s5.blast_radius(&sid(), cid(10), Duration::ZERO, now)
                .await
                .unwrap(),
            5
        );
        assert!(
            !passes(&s5, &g5, cid(10), cooldown, now).await,
            "blast == 5 must fail (strict > 5)"
        );

        let g6 = graph_with(concept(10, 1, now, None));
        let s6 = store_with_blast(6).await;
        assert_eq!(
            s6.blast_radius(&sid(), cid(10), Duration::ZERO, now)
                .await
                .unwrap(),
            6
        );
        assert!(
            passes(&s6, &g6, cid(10), cooldown, now).await,
            "blast == 6 must pass"
        );
    }

    /// `last_demotion_time == None` is not a cooldown, even at the
    /// default 300s window.
    #[tokio::test]
    async fn last_demotion_none_is_not_blocked() {
        let now = ts();
        let hub = concept(10, 1, now, None);
        assert!(hub.last_demotion_time.is_none());
        let g = graph_with(hub);
        let store = store_with_blast(6).await;
        assert!(
            passes(&store, &g, cid(10), default_cooldown(), now).await,
            "None last_demotion_time must not block a blast=6 node"
        );
    }

    /// Just-demoted (`last_demotion_time = now`) is refused for the
    /// default 300s cooldown even when blast is 8. Would pass if the
    /// cooldown field were ignored.
    #[tokio::test]
    async fn just_demoted_refused_during_cooldown() {
        let now = ts();
        let hub = concept(10, 1, now, Some(now));
        let g = graph_with(hub);
        let store = store_with_blast(8).await;
        assert_eq!(
            store
                .blast_radius(&sid(), cid(10), Duration::ZERO, now)
                .await
                .unwrap(),
            8
        );
        assert!(
            !passes(&store, &g, cid(10), default_cooldown(), now).await,
            "last_demotion_time = now must refuse at cooldown=300s even with blast 8"
        );
    }

    /// Same node passes once the injected clock reaches
    /// `last_demotion_time + cooldown`. One nanosecond earlier still
    /// refuses — locks the inclusive `>=` boundary.
    #[tokio::test]
    async fn passes_when_mocked_clock_reaches_cooldown() {
        let demoted_at = ts();
        let cooldown = default_cooldown();
        let hub = concept(10, 1, demoted_at, Some(demoted_at));
        let g = graph_with(hub);
        let store = store_with_blast(8).await;

        let still_cooling = demoted_at + ChronoDuration::from_std(cooldown).unwrap()
            - ChronoDuration::nanoseconds(1);
        assert!(
            !passes(&store, &g, cid(10), cooldown, still_cooling).await,
            "now == last + cooldown - 1ns must still refuse"
        );

        let cooled = demoted_at + ChronoDuration::from_std(cooldown).unwrap();
        assert!(
            passes(&store, &g, cid(10), cooldown, cooled).await,
            "now == last + cooldown must pass (mocked clock)"
        );
    }

    /// Missing session is a store error and must surface, not become
    /// `Ok(false)`.
    #[tokio::test]
    async fn store_error_propagates() {
        let store = MemoryStore::new();
        let g = graph_with(concept(10, 1, ts(), None));
        let err = stage3_passes(
            &store,
            &g,
            &sid(),
            cid(10),
            Duration::ZERO,
            default_cooldown(),
            ts(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, StoreError::SessionNotFound(_)),
            "expected SessionNotFound, got {err:?}"
        );
    }

    #[cfg(feature = "fixtures")]
    async fn rest_api() -> (MemoryStore, Graph, SessionId) {
        let store = crate::fixtures::load_store("session-rest-api").unwrap();
        let sid = SessionId::from("session-rest-api");
        let snap = crate::fixtures::load_snapshot("session-rest-api").unwrap();
        let graph = Graph::from_snapshot(snap).unwrap();
        (store, graph, sid)
    }

    /// Injected clock for the `session-rest-api` fixture (F8): every planted
    /// timestamp is 2026-08-10T09:00–09:55Z, so the store's age cutoff must be
    /// anchored *after* the session. `ts()` predates the fixture by a year and
    /// would cut every edge out of the blast radius now that the adapters read
    /// the caller's clock instead of the wall clock.
    #[cfg(feature = "fixtures")]
    fn fixture_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 10, 10, 0, 0).unwrap()
    }

    #[cfg(feature = "fixtures")]
    fn fixture_id(content: &str, graph: &Graph) -> NodeId {
        graph
            .concepts()
            .find(|c| c.content == content)
            .unwrap_or_else(|| panic!("{content} present"))
            .id
    }

    /// Planted pillar `user schema` (blast 8, last_demotion None) passes;
    /// `api layer` (blast 1) does not.
    #[cfg(feature = "fixtures")]
    #[tokio::test]
    async fn rest_api_user_schema_passes_api_layer_fails() {
        let (store, graph, sid) = rest_api().await;
        let us = fixture_id("user schema", &graph);
        let api = fixture_id("api layer", &graph);
        let now = fixture_now();
        let cooldown = default_cooldown();

        let us_blast = store
            .blast_radius(&sid, us, Duration::ZERO, now)
            .await
            .unwrap();
        assert_eq!(us_blast, 8, "fixture premise: user schema blast=8");
        match graph.node(us) {
            Some(Node::Concept(c)) => assert!(c.last_demotion_time.is_none()),
            other => panic!("user schema must be a concept, got {other:?}"),
        }
        assert!(
            stage3_passes(&store, &graph, &sid, us, Duration::ZERO, cooldown, now,)
                .await
                .unwrap(),
            "user schema blast=8 / last_demotion None must pass Stage 3"
        );

        let api_blast = store
            .blast_radius(&sid, api, Duration::ZERO, now)
            .await
            .unwrap();
        assert_eq!(api_blast, 1, "fixture premise: api layer blast=1");
        assert!(
            !stage3_passes(&store, &graph, &sid, api, Duration::ZERO, cooldown, now,)
                .await
                .unwrap(),
            "api layer blast=1 must fail Stage 3"
        );
    }

    /// Fixture pillar with `last_demotion_time = now` is refused at
    /// 300s even though store blast is 8. Would pass if cooldown were
    /// ignored.
    #[cfg(feature = "fixtures")]
    #[tokio::test]
    async fn rest_api_just_demoted_pillar_refused() {
        let store = crate::fixtures::load_store("session-rest-api").unwrap();
        let sid = SessionId::from("session-rest-api");
        let mut snap = crate::fixtures::load_snapshot("session-rest-api").unwrap();
        let now = fixture_now();
        let us_id = snap
            .concepts
            .iter()
            .find(|c| c.content == "user schema")
            .expect("user schema present")
            .id;
        {
            let us = snap
                .concepts
                .iter_mut()
                .find(|c| c.id == us_id)
                .expect("user schema present");
            us.last_demotion_time = Some(now);
        }
        let graph = Graph::from_snapshot(snap).unwrap();
        assert_eq!(
            store
                .blast_radius(&sid, us_id, Duration::ZERO, now)
                .await
                .unwrap(),
            8
        );
        assert!(
            !stage3_passes(
                &store,
                &graph,
                &sid,
                us_id,
                Duration::ZERO,
                default_cooldown(),
                now,
            )
            .await
            .unwrap(),
            "just-demoted user schema must be refused despite blast 8"
        );
    }
}
