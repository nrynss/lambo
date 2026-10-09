//! #1 fencing on delete-only batches, for every dialect of the family.
//!
//! `DeleteNode`/`DeleteEdge` carry no session, so the flush used to build its
//! fenced set from the other mutations only, and a batch of nothing but
//! deletes (what a GC sweep drains) committed with no token check at all. The
//! check here is the zombie-writer shape: the first holder's lease lapses, a
//! second writer takes the session over, and the first one's stale token must
//! not be able to delete the second one's rows through either delete kind.
//!
//! Live only (it needs a real engine and its lease clock). The Postgres leg is
//! the `#[ignore]`d test below; the Cockroach leg runs inside
//! `cockroach::conformance::conformance_suite`.

use std::time::Duration;

use chrono::Utc;
use uuid::Uuid;

use super::{Dialect, PgStore};
use crate::store::lease::{LeaseHolder, LeaseOutcome};
use crate::store::GraphStore;
use crate::types::{
    AgentId, CanonizationStatus, Concept, ConceptType, Edge, EdgeType, Interaction, Mutation,
    MutationBatch, Node, NodeId, SessionId, StoreError,
};

/// Rows of `table` with this id.
async fn rows_with_id<D: Dialect>(store: &PgStore<D>, table: &str, id: NodeId) -> i64 {
    let pool = store.pool().await.expect("pool");
    sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table} WHERE id = $1"))
        .bind(id.0)
        .fetch_one(&pool)
        .await
        .expect("count rows")
}

fn holder(agent: &str, pid: u32) -> LeaseHolder {
    LeaseHolder {
        endpoint: None,
        agent: AgentId::new(agent),
        pid,
        host: "test".into(),
    }
}

/// Plants a session, hands its lease from a lapsed holder to a successor, and
/// asserts the lapsed holder's token cannot flush a delete-only batch while
/// the successor's can. Uses a unique session and removes its rows at the
/// end, so it is safe on a shared cluster.
pub(crate) async fn check_delete_only_batch_is_fenced<D: Dialect>(store: &PgStore<D>) {
    let sid = SessionId::from(format!("fenced-deletes-{}", Uuid::new_v4()));
    let ts = Utc::now();
    let (origin, concept, derives) = (NodeId::new(), NodeId::new(), NodeId::new());
    let planted = MutationBatch {
        mutation_epoch: 1,
        gc_mark: Default::default(),
        mutations: vec![
            Mutation::UpsertNode {
                node: Node::Interaction(Interaction {
                    event_time: None,
                    id: origin,
                    session_id: sid.clone(),
                    agent_id: AgentId::new("fenced-deletes"),
                    prompt_text: Some("p".into()),
                    previous_id: None,
                    created_at: ts,
                }),
            },
            Mutation::UpsertNode {
                node: Node::Concept(Concept {
                    id: concept,
                    session_id: sid.clone(),
                    content: format!("doomed {concept}"),
                    canonical_key: format!("doomed {concept}"),
                    concept_type: ConceptType::Entity,
                    origin_interaction: origin,
                    origin_agent: AgentId::new("fenced-deletes"),
                    created_at: ts,
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
                    id: derives,
                    session_id: sid.clone(),
                    source: origin,
                    target: concept,
                    edge_type: EdgeType::Derives,
                    weight: 1.0,
                    reinforcements: 0,
                    created_at: ts,
                    last_reinforced: ts,
                    event_time: None,
                },
            },
        ],
    };
    store.flush(&planted, None).await.expect("plant (unleased)");

    let ttl = Duration::from_secs(1);
    let LeaseOutcome::Acquired(first) = store
        .acquire_lease(&sid, &holder("zombie", 1), ttl)
        .await
        .expect("acquire")
    else {
        panic!("the first holder must acquire");
    };
    tokio::time::sleep(Duration::from_millis(1_300)).await;
    let LeaseOutcome::Acquired(second) = store
        .acquire_lease(&sid, &holder("successor", 2), ttl)
        .await
        .expect("takeover")
    else {
        panic!("the successor must take the lapsed lease over");
    };
    assert!(second.token > first.token);

    let deletes = |mutations: Vec<Mutation>| MutationBatch {
        mutation_epoch: 2,
        gc_mark: Default::default(),
        mutations,
    };
    let edge_only = deletes(vec![Mutation::DeleteEdge { id: derives }]);
    let node_only = deletes(vec![Mutation::DeleteNode { id: concept }]);
    for (what, batch) in [("DeleteEdge", &edge_only), ("DeleteNode", &node_only)] {
        for token in [Some(first.token), None] {
            let got = store.flush(batch, token).await;
            assert!(
                matches!(got, Err(StoreError::StaleWrite(_))),
                "a {what}-only batch under token {token:?} must be fenced, got {got:?}"
            );
        }
    }
    assert_eq!(rows_with_id(store, "concepts", concept).await, 1);
    assert_eq!(rows_with_id(store, "edges", derives).await, 1);

    store
        .flush(&edge_only, Some(second.token))
        .await
        .expect("current holder deletes the edge");
    store
        .flush(&node_only, Some(second.token))
        .await
        .expect("current holder deletes the node");
    assert_eq!(rows_with_id(store, "concepts", concept).await, 0);
    assert_eq!(rows_with_id(store, "edges", derives).await, 0);

    // A delete of a row that is already gone resolves no session: a no-op.
    store
        .flush(&node_only, Some(first.token))
        .await
        .expect("deleting a gone row is a no-op");

    cleanup_session(store, &sid).await;
    check_incident_edge_is_fenced(store).await;
}

/// Best-effort cleanup of a unique session, children first.
async fn cleanup_session<D: Dialect>(store: &PgStore<D>, sid: &SessionId) {
    let pool = store.pool().await.expect("pool");
    for table in [
        "edges",
        "concepts",
        "interactions",
        "session_leases",
        "lease_refusals",
        "sessions",
    ] {
        let _ = sqlx::query(&format!("DELETE FROM {table} WHERE session_id = $1"))
            .bind(sid.as_str())
            .execute(&pool)
            .await;
    }
}

/// A node delete also removes the edges incident to the node, and edges are
/// session-scoped while node ids are global. Plant a concept in session A
/// (whose token the caller still holds) with its only incident edge in session
/// B, whose lease has been taken over: the delete must be fenced on B's lease
/// too. A sibling concept with no foreign edge deletes under the same token,
/// so only the incident-edge arm of `DELETED_ROW_SESSIONS_SQL` can refuse.
async fn check_incident_edge_is_fenced<D: Dialect>(store: &PgStore<D>) {
    let tag = Uuid::new_v4();
    let (a, b) = (
        SessionId::from(format!("fenced-edge-a-{tag}")),
        SessionId::from(format!("fenced-edge-b-{tag}")),
    );
    let ts = Utc::now();
    let (origin_a, shared, plain, origin_b, cross) = (
        NodeId::new(),
        NodeId::new(),
        NodeId::new(),
        NodeId::new(),
        NodeId::new(),
    );
    let interaction = |sid: &SessionId, id: NodeId| Mutation::UpsertNode {
        node: Node::Interaction(Interaction {
            event_time: None,
            id,
            session_id: sid.clone(),
            agent_id: AgentId::new("fenced-edge"),
            prompt_text: Some("p".into()),
            previous_id: None,
            created_at: ts,
        }),
    };
    let concept = |id: NodeId| Mutation::UpsertNode {
        node: Node::Concept(Concept {
            id,
            session_id: a.clone(),
            content: format!("fenced {id}"),
            canonical_key: format!("fenced {id}"),
            concept_type: ConceptType::Entity,
            origin_interaction: origin_a,
            origin_agent: AgentId::new("fenced-edge"),
            created_at: ts,
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
    };
    let planted = MutationBatch {
        mutation_epoch: 1,
        gc_mark: Default::default(),
        mutations: vec![
            interaction(&a, origin_a),
            concept(shared),
            concept(plain),
            interaction(&b, origin_b),
            // Session B's edge points at session A's concept.
            Mutation::UpsertEdge {
                edge: Edge {
                    id: cross,
                    session_id: b.clone(),
                    source: origin_b,
                    target: shared,
                    edge_type: EdgeType::Derives,
                    weight: 1.0,
                    reinforcements: 0,
                    created_at: ts,
                    last_reinforced: ts,
                    event_time: None,
                },
            },
        ],
    };
    store.flush(&planted, None).await.expect("plant (unleased)");

    // A is held by its first holder; B lapses and is taken over, so the token
    // A's holder presents is current for A and stale for B only.
    let LeaseOutcome::Acquired(held_a) = store
        .acquire_lease(&a, &holder("holder-a", 1), Duration::from_secs(60))
        .await
        .expect("acquire a")
    else {
        panic!("A must acquire");
    };
    let short = Duration::from_secs(1);
    store
        .acquire_lease(&b, &holder("zombie-b", 2), short)
        .await
        .expect("acquire b");
    tokio::time::sleep(Duration::from_millis(1_300)).await;
    let LeaseOutcome::Acquired(taken_b) = store
        .acquire_lease(&b, &holder("successor-b", 3), short)
        .await
        .expect("take b over")
    else {
        panic!("the successor must take B over");
    };
    assert!(taken_b.token > held_a.token);

    let deletes = |id: NodeId| MutationBatch {
        mutation_epoch: 2,
        gc_mark: Default::default(),
        mutations: vec![Mutation::DeleteNode { id }],
    };
    let got = store.flush(&deletes(shared), Some(held_a.token)).await;
    assert!(
        matches!(got, Err(StoreError::StaleWrite(_))),
        "a node delete whose incident edge is in a taken-over session must be fenced, got {got:?}"
    );
    assert_eq!(rows_with_id(store, "concepts", shared).await, 1);
    assert_eq!(rows_with_id(store, "edges", cross).await, 1);

    store
        .flush(&deletes(plain), Some(held_a.token))
        .await
        .expect("a node with no foreign edge deletes under A's token");
    assert_eq!(rows_with_id(store, "concepts", plain).await, 0);

    store
        .flush(&deletes(shared), Some(taken_b.token))
        .await
        .expect("B's current token covers both sessions");
    assert_eq!(rows_with_id(store, "concepts", shared).await, 0);
    assert_eq!(rows_with_id(store, "edges", cross).await, 0);

    cleanup_session(store, &a).await;
    cleanup_session(store, &b).await;
}

#[cfg(feature = "store-postgres")]
#[tokio::test]
#[ignore = "live: requires LAMBO_POSTGRES_DSN against pinned pgvector/pgvector:pg17"]
async fn postgres_fences_a_delete_only_batch() {
    let Some(dsn) = super::postgres::postgres_dsn_or_skip("postgres_fences_a_delete_only_batch")
    else {
        return;
    };
    let store = super::postgres::PostgresStore::new(crate::store::StoreConfig {
        kind: crate::store::StoreKind::Postgres,
        dsn: Some(dsn),
        path: None,
        vector_dim: None,
    })
    .expect("construct");
    store.init_schema().await.expect("init_schema");
    check_delete_only_batch_is_fenced(&store).await;
}
