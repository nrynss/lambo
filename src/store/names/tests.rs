//! One identity-and-repair contract exercised against every adapter.
use crate::graph::canonical::canonical_key;
use crate::graph::Graph;
use crate::store::GraphStore;
use crate::types::{
    AgentId, CanonizationEvent, CanonizationStatus, Concept, ConceptName, ConceptType, Edge,
    EdgeType, Interaction, Mutation, MutationBatch, Node, NodeId, SessionId, WriteIntent,
    WriteIntentOutcome, WriteIntentPayload,
};
use chrono::{TimeZone, Utc};

fn concept(sid: &SessionId, origin: NodeId, content: &str, kind: ConceptType) -> Concept {
    Concept {
        id: NodeId::new(),
        session_id: sid.clone(),
        content: content.into(),
        canonical_key: canonical_key(content, |_| None),
        concept_type: kind,
        origin_interaction: origin,
        origin_agent: AgentId::new("names-test"),
        created_at: Utc.timestamp_opt(1_790_000_000, 0).unwrap(),
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

fn batch(mutations: Vec<Mutation>) -> MutationBatch {
    MutationBatch {
        mutations,
        mutation_epoch: 100,
        gc_mark: Default::default(),
    }
}

async fn check_names_and_repair(store: &dyn GraphStore) {
    let sid = SessionId::from(format!("names-conformance-{}", uuid::Uuid::new_v4()));
    let now = Utc.timestamp_opt(1_790_000_000, 0).unwrap();
    let origin = NodeId::new();
    let mut graph = Graph::new(sid.clone());
    graph
        .insert_interaction(Interaction {
            id: origin,
            session_id: sid.clone(),
            agent_id: AgentId::new("names-test"),
            previous_id: None,
            prompt_text: None,
            created_at: now,
            event_time: None,
        })
        .unwrap();
    let target = concept(
        &sid,
        origin,
        "a writer retains the lease until shutdown",
        ConceptType::Logic,
    );
    let alias = concept(&sid, origin, "writer lease", ConceptType::Entity);
    let dependent = concept(
        &sid,
        origin,
        "implemented orderly shutdown",
        ConceptType::Resource,
    );
    for c in [&target, &alias, &dependent] {
        graph.insert_concept(c.clone(), origin).unwrap();
    }
    let edge = Edge {
        id: NodeId::new(),
        session_id: sid.clone(),
        source: dependent.id,
        target: alias.id,
        edge_type: EdgeType::Dependency,
        weight: 0.5,
        reinforcements: 3,
        created_at: now,
        event_time: Some(now - chrono::Duration::days(1)),
        last_reinforced: now,
    };
    graph.upsert_edge(edge.clone()).unwrap();
    let audit = CanonizationEvent {
        id: NodeId::new(),
        session_id: sid.clone(),
        node_id: alias.id,
        from_status: CanonizationStatus::None,
        to_status: CanonizationStatus::Candidate,
        blast_radius: None,
        last_demotion_time: None,
        occurred_at: now,
        reason: None,
    };
    let mut seed = graph.drain_log();
    seed.push(Mutation::CanonizationTransition {
        event: audit.clone(),
    });
    store.flush(&seed, None).await.unwrap();

    let name = ConceptName {
        session_id: sid.clone(),
        name: "Shutdown Rule".into(),
        name_key: canonical_key("Shutdown Rule", |_| None),
        node_id: target.id,
    };
    let registration = batch(vec![Mutation::SetConceptName { name: name.clone() }]);
    store.flush(&registration, None).await.unwrap();
    store.flush(&registration, None).await.unwrap();
    let stored = store.load_session(&sid).await.unwrap();
    assert_eq!(stored.names, vec![name.clone()]);
    let reopened = Graph::from_snapshot(stored).unwrap();
    assert_eq!(reopened.name_target("shutdown rule"), Some(target.id));

    // A late invalid claim rolls earlier changes in the same batch back.
    let before = store.load_session(&sid).await.unwrap();
    let extra = concept(&sid, origin, "uncommitted work", ConceptType::Entity);
    let mut takeover = name.clone();
    takeover.node_id = dependent.id;
    assert!(store
        .flush(
            &batch(vec![
                Mutation::UpsertNode {
                    node: Node::Concept(extra)
                },
                Mutation::SetConceptName { name: takeover },
            ]),
            None
        )
        .await
        .is_err());
    assert_eq!(store.load_session(&sid).await.unwrap(), before);

    for (label, invalid_target) in [
        ("interaction rule", origin),
        ("missing rule", NodeId::new()),
    ] {
        let invalid = ConceptName {
            session_id: sid.clone(),
            name: label.into(),
            name_key: canonical_key(label, |_| None),
            node_id: invalid_target,
        };
        assert!(store
            .flush(
                &batch(vec![Mutation::SetConceptName { name: invalid }]),
                None
            )
            .await
            .is_err());
    }
    let foreign_sid = SessionId::from(format!("foreign-name-{}", uuid::Uuid::new_v4()));
    let mut foreign = name.clone();
    foreign.session_id = foreign_sid.clone();
    assert!(store
        .flush(
            &batch(vec![Mutation::SetConceptName { name: foreign }]),
            None
        )
        .await
        .is_err());
    assert!(
        store.load_session(&foreign_sid).await.is_err(),
        "a rejected registration must not leave an empty foreign session"
    );
    assert_eq!(store.load_session(&sid).await.unwrap(), before);

    // The store gate, not only Graph, prevents shadowing a durable name.
    let shadow = concept(&sid, origin, "shutdown rule", ConceptType::Entity);
    assert!(store
        .flush(
            &batch(vec![Mutation::UpsertNode {
                node: Node::Concept(shadow)
            }]),
            None
        )
        .await
        .is_err());
    let mut occupied = name.clone();
    occupied.name = alias.content.clone();
    occupied.name_key = alias.canonical_key.clone();
    assert!(store
        .flush(
            &batch(vec![Mutation::SetConceptName { name: occupied }]),
            None
        )
        .await
        .is_err());

    // Apply the reviewed repair as one transaction, retaining evidence time and archive.
    let mut upgrading = Graph::from_snapshot(before.clone()).unwrap();
    upgrading.declare_synonym("historical shutdown", &target.content);
    upgrading.declare_synonym("Historical Shutdown", &target.content);
    let report = crate::graph::names::upgrade(&mut upgrading, &[(alias.id, target.id)]).unwrap();
    assert_eq!(report.aliases_retired, 1);
    assert_eq!(report.names_migrated, 2);
    let archived = upgrading.retired_alias(alias.id).unwrap().clone();
    let moved = upgrading
        .edge_between(dependent.id, target.id, EdgeType::Dependency)
        .unwrap()
        .clone();
    assert_eq!(moved.event_time, edge.event_time);
    assert_eq!(moved.created_at, edge.created_at);
    assert_eq!(moved.last_reinforced, edge.last_reinforced);
    assert_eq!(moved.reinforcements, edge.reinforcements);
    let repaired_name = ConceptName {
        session_id: sid.clone(),
        name: alias.content.clone(),
        name_key: alias.canonical_key.clone(),
        node_id: target.id,
    };
    let repair = upgrading.drain_log();
    store.flush(&repair, None).await.unwrap();
    store.flush(&repair, None).await.unwrap();
    let repaired = store.load_session(&sid).await.unwrap();
    assert_eq!(repaired.retired_aliases, vec![archived.clone()]);
    assert!(repaired.canonization_events.contains(&audit));
    assert_eq!(
        repaired.retired_aliases[0].concept.canonization_status,
        CanonizationStatus::Candidate
    );
    assert!(!repaired.concepts.iter().any(|c| c.id == alias.id));
    assert_eq!(
        repaired.edges.iter().find(|e| e.id == moved.id),
        Some(&moved)
    );
    assert!(repaired.names.contains(&repaired_name));
    let mut rerun = Graph::from_snapshot(repaired.clone()).unwrap();
    assert_eq!(rerun.name_target("historical shutdown"), Some(target.id));
    let repeated = crate::graph::names::upgrade(&mut rerun, &[(alias.id, target.id)]).unwrap();
    assert_eq!(repeated.already_applied, 1);
    assert!(rerun.drain_log().is_empty());
    assert!(store
        .flush(
            &batch(vec![Mutation::UpsertNode {
                node: Node::Concept(alias.clone())
            }]),
            None
        )
        .await
        .is_err());
    let mut rewritten_history = archived.clone();
    rewritten_history.target = dependent.id;
    assert!(store
        .flush(
            &batch(vec![Mutation::RetireAlias {
                retired: rewritten_history
            }]),
            None
        )
        .await
        .is_err());
    assert_eq!(store.load_session(&sid).await.unwrap(), repaired);

    // Target collection never releases or silently retargets the name.
    store
        .flush(&batch(vec![Mutation::DeleteNode { id: target.id }]), None)
        .await
        .unwrap();
    let deleted = store.load_session(&sid).await.unwrap();
    assert!(deleted.names.contains(&name));
    assert!(deleted.names.contains(&repaired_name));
    let replacement = concept(&sid, origin, "writer lease", ConceptType::Entity);
    assert!(store
        .flush(
            &batch(vec![Mutation::UpsertNode {
                node: Node::Concept(replacement)
            }]),
            None
        )
        .await
        .is_err());
    assert_eq!(store.load_session(&sid).await.unwrap(), deleted);
    Graph::from_snapshot(deleted).unwrap();

    // Both new intent kinds survive a restart while owed, then scrub together with consumption.
    let intent_now = Utc
        .timestamp_millis_opt(Utc::now().timestamp_millis())
        .unwrap();
    let payloads = [
        WriteIntentPayload::DeriveNamed {
            concepts: vec![("future ruling".into(), ConceptType::Logic)],
            pairs: vec![],
            citations: Default::default(),
            receipt_destination: None,
        },
        WriteIntentPayload::ActionNamed {
            action: "future action".into(),
            produces: vec![],
            modifies: vec![],
            depends_on: vec!["shutdown rule".into()],
            depends_on_pins: vec![Some(target.id)],
            against: vec![],
            receipt_destination: None,
        },
    ];
    let intents: Vec<_> = payloads
        .into_iter()
        .enumerate()
        .map(|(i, payload)| WriteIntent {
            session_id: sid.clone(),
            receipt: format!("named-intent-{i}"),
            agent: AgentId::new("names-test"),
            interaction: origin,
            lane_seq: i as u64,
            issued_ms: intent_now.timestamp_millis(),
            payload,
            created_at: intent_now,
            outcome: None,
        })
        .collect();
    store
        .flush(
            &batch(
                intents
                    .iter()
                    .cloned()
                    .map(|intent| Mutation::PutWriteIntent { intent })
                    .collect(),
            ),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        store.load_session(&sid).await.unwrap().write_intents,
        intents
    );
    let consumes = batch(
        intents
            .iter()
            .map(|intent| Mutation::ConsumeWriteIntent {
                session_id: sid.clone(),
                receipt: intent.receipt.clone(),
                outcome: WriteIntentOutcome {
                    tag: "failed".into(),
                    summary: "target no longer exists".into(),
                    consumed_at: intent_now,
                },
            })
            .collect(),
    );
    store.flush(&consumes, None).await.unwrap();
    store.flush(&consumes, None).await.unwrap();
    for settled in store.load_session(&sid).await.unwrap().write_intents {
        assert!(settled.outcome.is_some());
        assert!(
            matches!(settled.payload, WriteIntentPayload::Derive { concepts, pairs, .. } if concepts.is_empty() && pairs.is_empty())
        );
    }
    let holder = crate::store::lease::LeaseHolder {
        endpoint: None,
        agent: AgentId::new("names-test-eraser"),
        pid: 1,
        host: "test".into(),
    };
    let crate::store::lease::LeaseOutcome::Acquired(lease) = store
        .acquire_lease(&sid, &holder, std::time::Duration::from_secs(60))
        .await
        .unwrap()
    else {
        panic!("fresh test lease")
    };
    assert!(store.flush(&registration, None).await.is_err());
    store.flush(&registration, Some(lease.token)).await.unwrap();
    let crate::store::EraseOutcome::Erased(erased) =
        store.erase_session(&sid, &holder).await.unwrap()
    else {
        panic!("fresh test session must erase")
    };
    assert_eq!(erased.removed.concept_names, 3);
    assert_eq!(erased.removed.retired_aliases, 1);
    assert!(store.load_session(&sid).await.is_err());
}

#[cfg(feature = "store-memory")]
#[tokio::test]
async fn names_conformance_memory() {
    check_names_and_repair(&crate::store::memory::MemoryStore::new()).await;
}

#[cfg(feature = "store-sqlite")]
#[tokio::test]
async fn names_conformance_sqlite() {
    let store = crate::store::sqlite::SqliteStore::connect("sqlite::memory:").unwrap();
    store.init_schema().await.unwrap();
    check_names_and_repair(&store).await;
}

#[cfg(feature = "store-postgres")]
#[tokio::test]
#[ignore = "live: requires isolated LAMBO_POSTGRES_DSN"]
async fn names_conformance_postgres() {
    use crate::store::{StoreConfig, StoreKind};
    let store = crate::store::pg::postgres::PostgresStore::new(StoreConfig {
        kind: StoreKind::Postgres,
        dsn: Some(std::env::var("LAMBO_POSTGRES_DSN").expect("live DSN required")),
        path: None,
        vector_dim: None,
    })
    .unwrap();
    store.init_schema().await.unwrap();
    check_names_and_repair(&store).await;
}

#[cfg(feature = "store-cockroach")]
#[tokio::test]
#[ignore = "live: requires isolated LAMBO_COCKROACH_DSN"]
async fn names_conformance_cockroach() {
    use crate::store::{StoreConfig, StoreKind};
    let store = crate::store::pg::cockroach::CockroachStore::new(StoreConfig {
        kind: StoreKind::Cockroach,
        dsn: Some(std::env::var("LAMBO_COCKROACH_DSN").expect("live DSN required")),
        path: None,
        vector_dim: None,
    })
    .unwrap();
    store.init_schema().await.unwrap();
    check_names_and_repair(&store).await;
}
