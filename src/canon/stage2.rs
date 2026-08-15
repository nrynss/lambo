//! Stage 2 — Venerable (T6.2, spec §10).
//!
//! Pure predicate: reports whether a concept currently clears Stage 2
//! interaction-span evidence. It does **not** apply a
//! `Candidate → Venerable` transition (T6.4 owns writes).
//!
//! A concept passes when `store.interaction_span(session, node, min_age, now)`
//! reports **both**:
//!
//! 1. **`distinct >= 3`** inbound structural sources tracing to distinct
//!    origin interactions.
//! 2. **`coverage >= 0.3`** of the session's temporal extent.
//!
//! Structural edge types (`Dependency` / `Causal` / `Hierarchical`) and the
//! age cutoff are already applied inside the store. `min_age` is forwarded
//! unchanged — callers pass [`crate::Config::canonization_edge_min_age`]
//! (default 60s). Fresh edges must not inflate the span.
//!
//! `now` is injected (F8) and forwarded to the store as the age cutoff's
//! anchor — the predicate and the adapter it queries read the **same** clock,
//! so one eval cycle has exactly one `now` and a mocked clock can drive the
//! inflation guard end to end.
//!
//! Stage 2 is evidence-only: [`CanonizationStatus`] is not consulted.
//! Candidate is not a prerequisite (T6.4 sequences transitions).

use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::store::GraphStore;
use crate::types::{NodeId, SessionId, StoreError};

/// Distinct-origin floor (spec §10).
const MIN_DISTINCT: u64 = 3;
/// Session-extent coverage floor (spec §10).
const MIN_COVERAGE: f64 = 0.3;

/// Whether `node` currently clears Stage 2 on `store`.
pub async fn stage2_passes(
    store: &dyn GraphStore,
    session: &SessionId,
    node: NodeId,
    min_age: Duration,
    now: DateTime<Utc>,
) -> Result<bool, StoreError> {
    let span = store.interaction_span(session, node, min_age, now).await?;
    Ok(span.distinct >= MIN_DISTINCT && span.coverage >= MIN_COVERAGE)
}

#[cfg(all(test, feature = "store-memory"))]
mod tests {
    use super::*;
    use crate::store::{GraphStore, MemoryStore};
    use crate::types::{
        AgentId, CanonizationStatus, Concept, ConceptType, Edge, EdgeType, Interaction, Mutation,
        MutationBatch, Node,
    };
    use chrono::{DateTime, Duration as ChronoDuration, Utc};
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

    fn concept(id: u64, origin: u64, at: DateTime<Utc>, status: CanonizationStatus) -> Concept {
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
            canonization_status: status,
            blast_radius: None,
            last_demotion_time: None,
            embedding: None,
            chunk_group_id: None,
        }
    }

    fn edge(id: u64, src: u64, tgt: u64, ty: EdgeType, at: DateTime<Utc>) -> Edge {
        Edge {
            id: nid(3, id),
            session_id: sid(),
            source: cid(src),
            target: cid(tgt),
            edge_type: ty,
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
        store.flush(&batch, None).await.unwrap();
        store
    }

    /// Two distinct origins spanning a single-point session (coverage 1.0)
    /// still fail. Status is Candidate so a status-gated predicate would
    /// pass this graph incorrectly.
    #[tokio::test]
    async fn distinct_2_fails_even_when_coverage_is_one() {
        let ts = Utc::now() - ChronoDuration::hours(1);
        let store = seed(
            &[interaction(1, ts), interaction(2, ts)],
            &[
                concept(10, 1, ts, CanonizationStatus::Candidate),
                concept(1, 1, ts, CanonizationStatus::None),
                concept(2, 2, ts, CanonizationStatus::None),
            ],
            &[
                edge(1, 1, 10, EdgeType::Dependency, ts),
                edge(2, 2, 10, EdgeType::Causal, ts),
            ],
        )
        .await;

        let span = store
            .interaction_span(&sid(), cid(10), Duration::ZERO, Utc::now())
            .await
            .unwrap();
        assert_eq!(span.distinct, 2);
        assert_eq!(span.coverage, 1.0);
        assert!(
            !stage2_passes(&store, &sid(), cid(10), Duration::ZERO, Utc::now())
                .await
                .unwrap(),
            "distinct=2 must fail even at coverage=1.0"
        );
    }

    /// Three distinct origins whose timestamps cover < 0.3 of the session
    /// fail. Constructed: supports at 0/10/20s, session extent 100s → 0.2.
    #[tokio::test]
    async fn coverage_below_0_3_fails_even_when_distinct_is_3() {
        let t0 = Utc::now() - ChronoDuration::hours(1);
        let t = |secs: i64| t0 + ChronoDuration::seconds(secs);
        let store = seed(
            &[
                interaction(1, t(0)),
                interaction(2, t(10)),
                interaction(3, t(20)),
                interaction(4, t(100)),
            ],
            &[
                concept(10, 1, t(0), CanonizationStatus::None),
                concept(1, 1, t(0), CanonizationStatus::None),
                concept(2, 2, t(10), CanonizationStatus::None),
                concept(3, 3, t(20), CanonizationStatus::None),
            ],
            &[
                edge(1, 1, 10, EdgeType::Dependency, t(0)),
                edge(2, 2, 10, EdgeType::Dependency, t(10)),
                edge(3, 3, 10, EdgeType::Hierarchical, t(20)),
            ],
        )
        .await;

        let span = store
            .interaction_span(&sid(), cid(10), Duration::ZERO, Utc::now())
            .await
            .unwrap();
        assert_eq!(span.distinct, 3);
        assert!(
            span.coverage < MIN_COVERAGE,
            "fixture premise: coverage={} must be < {MIN_COVERAGE}",
            span.coverage
        );
        assert!(
            !stage2_passes(&store, &sid(), cid(10), Duration::ZERO, Utc::now())
                .await
                .unwrap(),
            "coverage < 0.3 must fail even at distinct=3; coverage={}",
            span.coverage
        );
    }

    /// Complementary lock: distinct=3 and coverage >= 0.3 pass, and
    /// `None` status is enough — Stage 2 does not require Candidate.
    #[tokio::test]
    async fn distinct_3_with_coverage_passes_without_candidate_status() {
        let t0 = Utc::now() - ChronoDuration::hours(1);
        let t = |secs: i64| t0 + ChronoDuration::seconds(secs);
        // Supports at 0/20/40s, session extent 80s → coverage = 0.5.
        let store = seed(
            &[
                interaction(1, t(0)),
                interaction(2, t(20)),
                interaction(3, t(40)),
                interaction(4, t(80)),
            ],
            &[
                concept(10, 1, t(0), CanonizationStatus::None),
                concept(1, 1, t(0), CanonizationStatus::None),
                concept(2, 2, t(20), CanonizationStatus::None),
                concept(3, 3, t(40), CanonizationStatus::None),
            ],
            &[
                edge(1, 1, 10, EdgeType::Dependency, t(0)),
                edge(2, 2, 10, EdgeType::Causal, t(20)),
                edge(3, 3, 10, EdgeType::Hierarchical, t(40)),
            ],
        )
        .await;

        let span = store
            .interaction_span(&sid(), cid(10), Duration::ZERO, Utc::now())
            .await
            .unwrap();
        assert_eq!(span.distinct, 3);
        assert!(
            span.coverage >= MIN_COVERAGE,
            "fixture premise: coverage={}",
            span.coverage
        );
        assert!(
            stage2_passes(&store, &sid(), cid(10), Duration::ZERO, Utc::now())
                .await
                .unwrap(),
            "None-status node with span evidence must pass; coverage={}",
            span.coverage
        );
    }

    /// F16: the coverage floor is **inclusive**. Supports at 0/15/30s over a
    /// 100s session extent land the ratio on exactly 0.3; the surrounding
    /// tests bracket at 0.2 / 0.5 / 0.545 and never pin the boundary itself,
    /// so a `>=` → `>` regression survives them.
    #[tokio::test]
    async fn coverage_exactly_0_3_passes() {
        let t0 = Utc::now() - ChronoDuration::hours(1);
        let t = |secs: i64| t0 + ChronoDuration::seconds(secs);
        let store = seed(
            &[
                interaction(1, t(0)),
                interaction(2, t(15)),
                interaction(3, t(30)),
                interaction(4, t(100)),
            ],
            &[
                concept(10, 1, t(0), CanonizationStatus::None),
                concept(1, 1, t(0), CanonizationStatus::None),
                concept(2, 2, t(15), CanonizationStatus::None),
                concept(3, 3, t(30), CanonizationStatus::None),
            ],
            &[
                edge(1, 1, 10, EdgeType::Dependency, t(0)),
                edge(2, 2, 10, EdgeType::Causal, t(15)),
                edge(3, 3, 10, EdgeType::Hierarchical, t(30)),
            ],
        )
        .await;

        let span = store
            .interaction_span(&sid(), cid(10), Duration::ZERO, Utc::now())
            .await
            .unwrap();
        assert_eq!(span.distinct, 3);
        assert_eq!(
            span.coverage, MIN_COVERAGE,
            "fixture premise: 30s of a 100s extent is exactly the floor"
        );
        assert!(
            stage2_passes(&store, &sid(), cid(10), Duration::ZERO, Utc::now())
                .await
                .unwrap(),
            "coverage exactly at the floor must pass (>=, not >)"
        );
    }

    /// F17 (a): the age guard is TWO gates, and this one attacks only the
    /// **edge** gate — three aged origin interactions reached by three edges
    /// created just now. The module's burst test fires both gates together,
    /// so an adapter that dropped the `e.created_at` clause stayed green here
    /// (it was caught only in the store suites).
    #[tokio::test]
    async fn fresh_edges_from_aged_interactions_do_not_pass() {
        let now = Utc::now();
        let t = |secs: i64| now - ChronoDuration::seconds(secs);
        let min_age = Duration::from_secs(60);
        let store = seed(
            &[
                interaction(1, t(300)),
                interaction(2, t(200)),
                interaction(3, t(100)),
            ],
            &[
                concept(10, 1, t(300), CanonizationStatus::Candidate),
                concept(1, 1, t(300), CanonizationStatus::None),
                concept(2, 2, t(200), CanonizationStatus::None),
                concept(3, 3, t(100), CanonizationStatus::None),
            ],
            // Every origin interaction is aged; every EDGE is brand new.
            &[
                edge(1, 1, 10, EdgeType::Dependency, now),
                edge(2, 2, 10, EdgeType::Causal, now),
                edge(3, 3, 10, EdgeType::Hierarchical, now),
            ],
        )
        .await;

        assert!(
            stage2_passes(&store, &sid(), cid(10), Duration::ZERO, now)
                .await
                .unwrap(),
            "premise: the evidence is sufficient once ages are ignored"
        );
        assert!(
            !stage2_passes(&store, &sid(), cid(10), min_age, now)
                .await
                .unwrap(),
            "fresh edges must be cut even when their origins are old"
        );
    }

    /// F17 (b): the mirror attack on the **interaction** gate — three aged
    /// edges whose source concepts all trace back to interactions created
    /// just now.
    #[tokio::test]
    async fn aged_edges_from_fresh_interactions_do_not_pass() {
        let now = Utc::now();
        let t = |secs: i64| now - ChronoDuration::seconds(secs);
        let min_age = Duration::from_secs(60);
        let store = seed(
            // Interactions are brand new; the edges below are aged.
            &[
                interaction(1, now),
                interaction(2, now),
                interaction(3, now),
            ],
            &[
                concept(10, 1, t(300), CanonizationStatus::Candidate),
                concept(1, 1, t(300), CanonizationStatus::None),
                concept(2, 2, t(200), CanonizationStatus::None),
                concept(3, 3, t(100), CanonizationStatus::None),
            ],
            &[
                edge(1, 1, 10, EdgeType::Dependency, t(300)),
                edge(2, 2, 10, EdgeType::Causal, t(200)),
                edge(3, 3, 10, EdgeType::Hierarchical, t(100)),
            ],
        )
        .await;

        assert!(
            stage2_passes(&store, &sid(), cid(10), Duration::ZERO, now)
                .await
                .unwrap(),
            "premise: the evidence is sufficient once ages are ignored"
        );
        assert!(
            !stage2_passes(&store, &sid(), cid(10), min_age, now)
                .await
                .unwrap(),
            "fresh origin interactions must be cut even behind aged edges"
        );
    }

    /// Aged evidence stays below threshold; a fresh Dependency/Causal/
    /// Hierarchical burst would pass if counted. `min_age = 60s` must
    /// still fail; the same graph passes at `min_age = 0`.
    #[tokio::test]
    async fn fresh_burst_does_not_inflate_at_min_age_60s() {
        let now = Utc::now();
        let start = now - ChronoDuration::seconds(300);
        let aged1 = now - ChronoDuration::seconds(200);
        let aged2 = now - ChronoDuration::seconds(180);
        let min_age = Duration::from_secs(60);

        let store = seed(
            &[
                interaction(1, start),
                interaction(2, aged1),
                interaction(3, aged2),
                interaction(4, now),
                interaction(5, now),
                interaction(6, now),
            ],
            &[
                concept(10, 1, start, CanonizationStatus::None),
                concept(2, 2, aged1, CanonizationStatus::None),
                concept(3, 3, aged2, CanonizationStatus::None),
                concept(4, 4, now, CanonizationStatus::None),
                concept(5, 5, now, CanonizationStatus::None),
                concept(6, 6, now, CanonizationStatus::None),
            ],
            &[
                edge(1, 2, 10, EdgeType::Dependency, aged1),
                edge(2, 3, 10, EdgeType::Dependency, aged2),
                edge(3, 4, 10, EdgeType::Dependency, now),
                edge(4, 5, 10, EdgeType::Causal, now),
                edge(5, 6, 10, EdgeType::Hierarchical, now),
            ],
        )
        .await;

        let aged = store
            .interaction_span(&sid(), cid(10), min_age, now)
            .await
            .unwrap();
        assert!(
            aged.distinct < MIN_DISTINCT || aged.coverage < MIN_COVERAGE,
            "aged evidence must sit below threshold: {aged:?}"
        );
        assert!(
            !stage2_passes(&store, &sid(), cid(10), min_age, now)
                .await
                .unwrap(),
            "fresh burst must not promote at min_age=60s; aged={aged:?}"
        );

        let all = store
            .interaction_span(&sid(), cid(10), Duration::ZERO, now)
            .await
            .unwrap();
        assert!(
            all.distinct >= MIN_DISTINCT && all.coverage >= MIN_COVERAGE,
            "uncut graph must clear the threshold: {all:?}"
        );
        assert!(
            stage2_passes(&store, &sid(), cid(10), Duration::ZERO, now)
                .await
                .unwrap(),
            "same graph must pass at min_age=0; span={all:?}"
        );
    }

    #[cfg(feature = "fixtures")]
    #[tokio::test]
    async fn rest_api_api_layer_passes_stage2() {
        let store = crate::fixtures::load_store("session-rest-api").unwrap();
        let sid = SessionId::from("session-rest-api");
        let api = store
            .load_session(&sid)
            .await
            .unwrap()
            .concepts
            .into_iter()
            .find(|c| c.content == "api layer")
            .expect("api layer present")
            .id;

        let span = store
            .interaction_span(&sid, api, Duration::ZERO, Utc::now())
            .await
            .unwrap();
        assert_eq!(span.distinct, 3, "distinct={}", span.distinct);
        assert!(
            (span.coverage - 30.0 / 55.0).abs() < 1e-9,
            "coverage={} (expected 30/55)",
            span.coverage
        );
        assert!(
            stage2_passes(&store, &sid, api, Duration::ZERO, Utc::now())
                .await
                .unwrap(),
            "api layer must pass at min_age=0; span={span:?}"
        );

        // Fixture timestamps are 2026-08-10 ISO dates — older than 60s vs now.
        let min_age = crate::Config::default().canonization_edge_min_age;
        assert!(
            stage2_passes(&store, &sid, api, min_age, Utc::now())
                .await
                .unwrap(),
            "api layer must pass at default min_age={min_age:?}"
        );
    }

    /// Useful smoke: planted `user schema` is Canonical and still passes.
    /// Stage 2 does not require Candidate (and does not reject Canonical).
    #[cfg(feature = "fixtures")]
    #[tokio::test]
    async fn rest_api_user_schema_passes_stage2() {
        let store = crate::fixtures::load_store("session-rest-api").unwrap();
        let sid = SessionId::from("session-rest-api");
        let us = store
            .load_session(&sid)
            .await
            .unwrap()
            .concepts
            .into_iter()
            .find(|c| c.content == "user schema")
            .expect("user schema present");
        assert_eq!(us.canonization_status, CanonizationStatus::Canonical);

        let span = store
            .interaction_span(&sid, us.id, Duration::ZERO, Utc::now())
            .await
            .unwrap();
        assert_eq!(span.distinct, 6, "distinct={}", span.distinct);
        assert!(
            (span.coverage - 25.0 / 55.0).abs() < 1e-9,
            "coverage={} (expected 25/55)",
            span.coverage
        );
        assert!(
            stage2_passes(&store, &sid, us.id, Duration::ZERO, Utc::now())
                .await
                .unwrap(),
            "Canonical user schema must still pass Stage 2; span={span:?}"
        );
    }
}
