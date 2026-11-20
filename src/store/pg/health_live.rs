//! #248 on the live Postgres family: `PgStore::store_health` aggregates
//! per-session counts, raw vector coverage, canonization status counts,
//! active single-writer leases (against the store's own clock), latest durable
//! write timestamps, and omission of `lambo:erased` tombstones.
//!
//! Live only. The Postgres leg is the `#[ignore]`d test below:
//!
//! ```text
//! LAMBO_POSTGRES_DSN=<dsn> cargo test --no-default-features --features store-postgres --lib store::pg::health_live -- --ignored
//! ```
//!
//! The Cockroach leg runs inside `cockroach::conformance::conformance_suite`.

use std::time::Duration;

use chrono::{TimeZone, Utc};
use uuid::Uuid;

use crate::store::lease::{LeaseHolder, LeaseOutcome};
use crate::store::{CanonizationCounts, GraphStore};
use crate::types::{
    AgentId, CanonizationEvent, CanonizationStatus, Concept, ConceptType, Edge, EdgeType,
    EmbeddingContract, Interaction, Mutation, MutationBatch, Node, NodeId, SessionId,
};

/// Seed unique sessions on `store` (active lease, released lease, erased
/// tombstone) and verify `GraphStore::store_health` reports them accurately,
/// then erase the test sessions so shared clusters stay clean.
pub(crate) async fn check_store_health_case(store: &dyn GraphStore) {
    let dim = store
        .vector_dimensions()
        .expect("pg stores advertise a concrete vector width");
    let tag = Uuid::new_v4().simple().to_string();
    // Mix lowercase and uppercase ('Z' < 'a' in UTF-8 byte order, 'a' < 'Z' in
    // en_US.utf8) so live Postgres runs catch locale-collation ordering drift.
    let sid_active = SessionId::from(format!("health-live-a-{tag}"));
    let sid_released = SessionId::from(format!("health-live-Z-{tag}"));
    let sid_erased = SessionId::from(format!("health-live-c-{tag}"));

    let t1 = Utc.timestamp_opt(1_700_000_010, 0).unwrap();
    let t2 = Utc.timestamp_opt(1_700_000_040, 0).unwrap();
    let t3 = Utc.timestamp_opt(1_700_000_090, 0).unwrap();

    let mut vec = vec![0.0f32; dim];
    vec[0] = 1.0;

    let ia = NodeId::new();
    let c_canon = NodeId::new();
    let c_none = NodeId::new();
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 1,
                gc_mark: Default::default(),
                mutations: vec![
                    Mutation::SetEmbedding {
                        session_id: sid_active.clone(),
                        embedding: Some(EmbeddingContract {
                            kind: "fixture".into(),
                            model: None,
                            dim,
                        }),
                    },
                    Mutation::UpsertNode {
                        node: Node::Interaction(Interaction {
                            id: ia,
                            session_id: sid_active.clone(),
                            agent_id: AgentId::from("health-live"),
                            prompt_text: None,
                            previous_id: None,
                            created_at: t1,
                            event_time: None,
                        }),
                    },
                    Mutation::UpsertNode {
                        node: Node::Concept(Concept {
                            id: c_canon,
                            session_id: sid_active.clone(),
                            content: "embedded canonical fact".into(),
                            canonical_key: "embedded canonical fact".into(),
                            concept_type: ConceptType::Logic,
                            origin_interaction: ia,
                            origin_agent: AgentId::from("health-live"),
                            created_at: t1,
                            access_count: 0,
                            last_accessed: None,
                            gc_survived: 0,
                            canonization_status: CanonizationStatus::Canonical,
                            blast_radius: None,
                            last_demotion_time: None,
                            embedding: Some(vec),
                            human_confirmed: 0,
                            embedding_source: None,
                            chunk_group_id: None,
                        }),
                    },
                    Mutation::UpsertNode {
                        node: Node::Concept(Concept {
                            id: c_none,
                            session_id: sid_active.clone(),
                            content: "unembedded fact".into(),
                            canonical_key: "unembedded fact".into(),
                            concept_type: ConceptType::Entity,
                            origin_interaction: ia,
                            origin_agent: AgentId::from("health-live"),
                            created_at: t1,
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
                        }),
                    },
                    Mutation::UpsertEdge {
                        edge: Edge {
                            id: NodeId::new(),
                            session_id: sid_active.clone(),
                            source: ia,
                            target: c_canon,
                            edge_type: EdgeType::Derives,
                            weight: 1.0,
                            reinforcements: 1,
                            created_at: t1,
                            last_reinforced: t2,
                            event_time: None,
                        },
                    },
                ],
            },
            None,
        )
        .await
        .expect("flush active session");

    store
        .record_canonization(
            &CanonizationEvent {
                id: NodeId::new(),
                session_id: sid_active.clone(),
                node_id: c_canon,
                from_status: CanonizationStatus::Venerable,
                to_status: CanonizationStatus::Canonical,
                blast_radius: Some(2),
                last_demotion_time: None,
                occurred_at: t3,
                reason: None,
            },
            None,
        )
        .await
        .expect("record canonization");

    let ib = NodeId::new();
    let cb = NodeId::new();
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 1,
                gc_mark: Default::default(),
                mutations: vec![
                    Mutation::UpsertNode {
                        node: Node::Interaction(Interaction {
                            id: ib,
                            session_id: sid_released.clone(),
                            agent_id: AgentId::from("health-live"),
                            prompt_text: None,
                            previous_id: None,
                            created_at: t1,
                            event_time: None,
                        }),
                    },
                    Mutation::UpsertNode {
                        node: Node::Concept(Concept {
                            id: cb,
                            session_id: sid_released.clone(),
                            content: "released session fact".into(),
                            canonical_key: "released session fact".into(),
                            concept_type: ConceptType::Resource,
                            origin_interaction: ib,
                            origin_agent: AgentId::from("health-live"),
                            created_at: t2,
                            access_count: 0,
                            last_accessed: None,
                            gc_survived: 0,
                            canonization_status: CanonizationStatus::Candidate,
                            blast_radius: None,
                            last_demotion_time: None,
                            embedding: None,
                            human_confirmed: 0,
                            embedding_source: None,
                            chunk_group_id: None,
                        }),
                    },
                ],
            },
            None,
        )
        .await
        .expect("flush released session");

    let ic = NodeId::new();
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 1,
                gc_mark: Default::default(),
                mutations: vec![Mutation::UpsertNode {
                    node: Node::Interaction(Interaction {
                        id: ic,
                        session_id: sid_erased.clone(),
                        agent_id: AgentId::from("health-live"),
                        prompt_text: None,
                        previous_id: None,
                        created_at: t1,
                        event_time: None,
                    }),
                }],
            },
            None,
        )
        .await
        .expect("flush erased session");

    let mk_holder = |name: &str, pid: u32| LeaseHolder {
        agent: AgentId::from(name),
        pid,
        host: "health-live".into(),
        endpoint: None,
    };
    let holder_active = mk_holder("writer-active", 101);
    let LeaseOutcome::Acquired(lease_active) = store
        .acquire_lease(&sid_active, &holder_active, Duration::from_secs(60))
        .await
        .expect("acquire active lease")
    else {
        panic!("fresh active session lease must be free");
    };

    let holder_released = mk_holder("writer-released", 102);
    store
        .acquire_lease(&sid_released, &holder_released, Duration::from_secs(60))
        .await
        .expect("acquire released lease");
    store
        .release_lease(&sid_released, &holder_released)
        .await
        .expect("release lease");

    let eraser = mk_holder("health-eraser", 103);
    store
        .erase_session(&sid_erased, &eraser)
        .await
        .expect("erase session");

    let health = store.store_health().await.expect("store_health");
    let ordered = health
        .windows(2)
        .all(|p| p[0].session_id.0 < p[1].session_id.0);
    assert!(
        ordered,
        "store_health must order sessions ascending by session_id"
    );
    let pos_released = health
        .iter()
        .position(|h| h.session_id == sid_released)
        .expect("released session must appear in store_health");
    let pos_active = health
        .iter()
        .position(|h| h.session_id == sid_active)
        .expect("active session must appear in store_health");
    assert!(
        pos_released < pos_active,
        "store_health must sort by UTF-8 byte order ('Z' < 'a'), not DB locale"
    );

    let active = health
        .iter()
        .find(|h| h.session_id == sid_active)
        .expect("active session must appear in store_health");
    assert_eq!(active.concept_count, 2);
    assert_eq!(active.edge_count, 1);
    assert_eq!(active.embedded_count, 1);
    assert_eq!(
        active.canonization,
        CanonizationCounts {
            none: 1,
            candidate: 0,
            venerable: 0,
            canonical: 1,
        }
    );
    let active_lease = active.lease.as_ref().expect("active session live lease");
    assert_eq!(active_lease.holder, holder_active.token());
    assert_eq!(active_lease.acquired_at, lease_active.acquired_at);
    assert_eq!(active_lease.expires_at, lease_active.expires_at);
    assert_eq!(active.last_write_at, Some(t3));

    let released = health
        .iter()
        .find(|h| h.session_id == sid_released)
        .expect("released session must appear in store_health");
    assert_eq!(released.concept_count, 1);
    assert_eq!(released.edge_count, 0);
    assert_eq!(released.embedded_count, 0);
    assert_eq!(
        released.canonization,
        CanonizationCounts {
            none: 0,
            candidate: 1,
            venerable: 0,
            canonical: 0,
        }
    );
    assert!(
        released.lease.is_none(),
        "released lease must report no active writer"
    );
    assert_eq!(released.last_write_at, Some(t2));

    assert!(
        !health.iter().any(|h| h.session_id == sid_erased),
        "erased session tombstone must be omitted from store_health"
    );

    // Clean up test sessions on shared clusters.
    store
        .release_lease(&sid_active, &holder_active)
        .await
        .expect("release active lease");
    store
        .erase_session(&sid_active, &eraser)
        .await
        .expect("erase active test session");
    store
        .erase_session(&sid_released, &eraser)
        .await
        .expect("erase released test session");
}

#[cfg(feature = "store-postgres")]
#[tokio::test]
#[ignore = "live: requires LAMBO_POSTGRES_DSN against pinned pgvector/pgvector:pg17"]
async fn postgres_reports_store_health() {
    use super::postgres::{postgres_dsn_or_skip, PostgresStore};
    use crate::store::{StoreConfig, StoreKind};

    let Some(dsn) = postgres_dsn_or_skip("postgres_reports_store_health") else {
        return;
    };
    let store = PostgresStore::new(StoreConfig {
        kind: StoreKind::Postgres,
        dsn: Some(dsn),
        path: None,
        vector_dim: None,
    })
    .expect("construct");
    store.init_schema().await.expect("init_schema");
    check_store_health_case(&store).await;
}
