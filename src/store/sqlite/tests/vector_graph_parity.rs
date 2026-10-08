//! #8 parity: the graph-backed vector source against the SQLite scan it
//! replaces on the session holder.
//!
//! Each case seeds a SQLite session, loads it the way a holder does
//! (`load_session_async`, legacy quarantine included), then asks the same
//! question of both sources: SQLite's checked read through the public
//! `GraphStore` surface, and [`graph_vector_candidates`] over the loaded graph.
//! Answers must be identical: the same ids in the same order with the same
//! score **bits**, and the same error variant with the same message.
//!
//! The graph is fresher than the store by design (unflushed writes); that
//! difference is pinned by the holder tests in `memory/tests/vector_source.rs`.
//! Here both sources see one flushed state, which is the parity #8 promises.

use super::*;
use crate::graph::vector_source::graph_vector_candidates;
use crate::store::MAX_VECTOR_CANDIDATE_LIMIT;

/// Ids and raw score bits: equality here is bit-identity, not closeness.
fn bits(hits: &[Scored<NodeId>]) -> Vec<(NodeId, u64)> {
    hits.iter().map(|s| (s.item, s.score.to_bits())).collect()
}

async fn holder_graph(store: &SqliteStore, sid: &SessionId) -> crate::graph::Graph {
    load_session_async(store, sid)
        .await
        .expect("a holder loads the session")
        .graph
}

/// Ask both sources; assert identical answers; return SQLite's for the
/// caller's own assertions.
async fn assert_same_answer(
    store: &SqliteStore,
    graph: &crate::graph::Graph,
    sid: &SessionId,
    probe: &[f32],
    contract: &EmbeddingContract,
    limit: usize,
    label: &str,
) -> Result<Vec<Scored<NodeId>>, StoreError> {
    let from_store = store
        .vector_candidates_checked(sid, probe, contract, limit)
        .await;
    let from_graph = graph_vector_candidates(graph, sid, probe, contract, limit);
    match (&from_store, &from_graph) {
        (Ok(a), Ok(b)) => assert_eq!(
            bits(a),
            bits(b),
            "{label}: graph ranking must be bit-identical to SQLite's (limit {limit})"
        ),
        (Err(a), Err(b)) => {
            assert_eq!(
                std::mem::discriminant(a),
                std::mem::discriminant(b),
                "{label}: same refusal kind (store: {a}; graph: {b})"
            );
            assert_eq!(a.to_string(), b.to_string(), "{label}: same refusal message");
        }
        _ => panic!("{label}: one source answered, the other refused: store {from_store:?} graph {from_graph:?}"),
    }
    from_store
}

/// A concept upsert followed by its `Derives` edge from `origin`, so the
/// holder's load (which checks spec §5.7's invariants) accepts the session.
fn concept_with(
    sid: &SessionId,
    origin: NodeId,
    content: &str,
    concept_type: ConceptType,
    embedding: Option<Vec<f32>>,
) -> [Mutation; 2] {
    let id = NodeId::new();
    let ts = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
    let Mutation::UpsertNode {
        node: NodeKind::Concept(mut concept),
    } = plant_concept(sid, id, origin, content, concept_type, ts)
    else {
        unreachable!("plant_concept builds a concept upsert");
    };
    concept.embedding = embedding;
    [
        Mutation::UpsertNode {
            node: NodeKind::Concept(concept),
        },
        Mutation::UpsertEdge {
            edge: Edge {
                id: NodeId::new(),
                session_id: sid.clone(),
                source: origin,
                target: id,
                edge_type: EdgeType::Derives,
                weight: 0.9,
                reinforcements: 1,
                created_at: ts,
                last_reinforced: ts,
                event_time: None,
            },
        },
    ]
}

async fn flush(store: &SqliteStore, mutations: Vec<Mutation>) {
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations,
            },
            None,
        )
        .await
        .unwrap();
}

/// Exact ties (identical vectors under different keys, and Observations that
/// share one canonical key and one vector, so only the node id separates
/// them), concepts with no vector, and every limit from 1 past the pool.
#[tokio::test]
async fn vector_graph_parity_ties_missing_vectors_and_limits() {
    const DIM: usize = 4;
    let store = vec_test_store(DIM);
    store.init_schema().await.unwrap();
    let sid = SessionId::from("graph-parity-ties");
    let contract = vec_contract(DIM);
    let ts = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
    let origin = NodeId::new();

    let mut mutations = vec![
        plant_interaction(&sid, origin, None, ts),
        Mutation::SetEmbedding {
            session_id: sid.clone(),
            embedding: Some(contract.clone()),
        },
    ];
    let tied = vec![0.6f32, 0.8, 0.0, 0.0];
    let rows: Vec<(&str, ConceptType, Option<Vec<f32>>)> = vec![
        ("tie b", ConceptType::Entity, Some(tied.clone())),
        ("tie a", ConceptType::Entity, Some(tied.clone())),
        ("tie c", ConceptType::Logic, Some(tied.clone())),
        ("shared obs", ConceptType::Observation, Some(tied.clone())),
        ("shared obs", ConceptType::Observation, Some(tied.clone())),
        ("shared obs", ConceptType::Observation, Some(tied.clone())),
        ("near", ConceptType::Entity, Some(vec![0.7, 0.7, 0.1, 0.0])),
        (
            "far",
            ConceptType::Constraint,
            Some(vec![0.0, 0.0, 1.0, 0.0]),
        ),
        (
            "opposite",
            ConceptType::Resource,
            Some(vec![-0.6, -0.8, 0.0, 0.0]),
        ),
        ("no vector one", ConceptType::Entity, None),
        ("no vector two", ConceptType::Observation, None),
    ];
    let mut with_vector = 0;
    for (content, ty, embedding) in rows {
        with_vector += usize::from(embedding.is_some());
        mutations.extend(concept_with(&sid, origin, content, ty, embedding));
    }
    flush(&store, mutations).await;
    let graph = holder_graph(&store, &sid).await;

    for (label, probe) in [
        ("on the tie", tied.clone()),
        ("between", vec![0.5, 0.5, 0.5, 0.5]),
        ("orthogonal-ish", vec![0.0, 0.1, 1.0, 0.0]),
        ("unnormalised", vec![3.0, 4.0, 0.0, 0.0]),
    ] {
        for limit in [
            1,
            2,
            3,
            5,
            6,
            with_vector,
            with_vector + 3,
            MAX_VECTOR_CANDIDATE_LIMIT,
        ] {
            let hits = assert_same_answer(&store, &graph, &sid, &probe, &contract, limit, label)
                .await
                .unwrap();
            assert_eq!(hits.len(), limit.min(with_vector), "{label} limit {limit}");
        }
    }
    // The tie group really is a tie, ordered by key then id, on both sources.
    let hits = assert_same_answer(&store, &graph, &sid, &tied, &contract, 6, "tie group")
        .await
        .unwrap();
    assert!(
        hits.iter()
            .all(|h| h.score.to_bits() == hits[0].score.to_bits()),
        "the six tied concepts share one score: {hits:?}"
    );
}

/// Every refusal and every empty answer the contract defines, on both sources.
#[tokio::test]
async fn vector_graph_parity_refusals_and_empty_answers() {
    const DIM: usize = 4;
    let store = vec_test_store(DIM);
    store.init_schema().await.unwrap();
    let contract = vec_contract(DIM);
    let probe = [1.0f32, 0.0, 0.0, 0.0];
    let ts = Utc.timestamp_opt(1_752_000_000, 0).unwrap();

    // A session with a contract and vectors.
    let sid = SessionId::from("graph-parity-refusals");
    let origin = NodeId::new();
    flush(
        &store,
        vec![
            plant_interaction(&sid, origin, None, ts),
            Mutation::SetEmbedding {
                session_id: sid.clone(),
                embedding: Some(contract.clone()),
            },
        ]
        .into_iter()
        .chain(concept_with(
            &sid,
            origin,
            "one",
            ConceptType::Entity,
            Some(vec![1.0, 0.0, 0.0, 0.0]),
        ))
        .collect(),
    )
    .await;
    let graph = holder_graph(&store, &sid).await;

    // limit over the public bound: Invariant on both, same message.
    let a = store
        .vector_candidates_checked(&sid, &probe, &contract, MAX_VECTOR_CANDIDATE_LIMIT + 1)
        .await
        .unwrap_err();
    let b = graph_vector_candidates(
        &graph,
        &sid,
        &probe,
        &contract,
        MAX_VECTOR_CANDIDATE_LIMIT + 1,
    )
    .unwrap_err();
    assert!(matches!(a, StoreError::Invariant(_)), "{a:?}");
    assert_eq!(a.to_string(), b.to_string());

    // limit 0 is empty before anything else is looked at, even a wrong contract.
    for c in [&contract, &vec_contract(999)] {
        assert!(
            assert_same_answer(&store, &graph, &sid, &probe, c, 0, "limit 0")
                .await
                .unwrap()
                .is_empty()
        );
    }

    // A probe that is not an embedding: same Backend refusal, same message.
    for (label, bad) in [
        ("zero norm", vec![0.0f32; DIM]),
        ("nan", vec![f32::NAN, 0.0, 0.0, 0.0]),
        ("inf", vec![f32::INFINITY, 0.0, 0.0, 0.0]),
    ] {
        let a = store
            .vector_candidates_checked(&sid, &bad, &contract, 5)
            .await
            .unwrap_err();
        let b = graph_vector_candidates(&graph, &sid, &bad, &contract, 5).unwrap_err();
        assert!(matches!(a, StoreError::Backend(_)), "{label}: {a:?}");
        assert_eq!(a.to_string(), b.to_string(), "{label}");
    }

    // A changed contract: Invariant with the exact message recall matches on
    // to annotate a keyword-only result (`Daemon::recall_with`, E2E-6).
    let renamed = EmbeddingContract {
        model: Some("another-model".into()),
        ..contract.clone()
    };
    let a = store
        .vector_candidates_checked(&sid, &probe, &renamed, 5)
        .await
        .unwrap_err();
    let b = graph_vector_candidates(&graph, &sid, &probe, &renamed, 5).unwrap_err();
    assert!(matches!(a, StoreError::Invariant(_)), "{a:?}");
    assert_eq!(a.to_string(), b.to_string());
    assert!(b.to_string().contains("embedding contract changed"), "{b}");

    // A probe of the wrong width under the right contract: Invariant on both.
    let wide = vec![1.0f32; DIM + 1];
    let err = assert_same_answer(&store, &graph, &sid, &wide, &contract, 5, "probe width")
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Invariant(_)), "{err:?}");

    // A session with no contract yet, and an unknown session: empty on both
    // (the holder of an unknown session loads an empty graph).
    let bare = SessionId::from("graph-parity-no-contract");
    let bare_origin = NodeId::new();
    flush(
        &store,
        vec![plant_interaction(&bare, bare_origin, None, ts)]
            .into_iter()
            .chain(concept_with(
                &bare,
                bare_origin,
                "plain",
                ConceptType::Entity,
                None,
            ))
            .collect(),
    )
    .await;
    let bare_graph = holder_graph(&store, &bare).await;
    assert!(assert_same_answer(
        &store,
        &bare_graph,
        &bare,
        &probe,
        &contract,
        5,
        "no contract"
    )
    .await
    .unwrap()
    .is_empty());
    let unknown = SessionId::from("graph-parity-unknown");
    let unknown_graph = holder_graph(&store, &unknown).await;
    assert!(assert_same_answer(
        &store,
        &unknown_graph,
        &unknown,
        &probe,
        &contract,
        5,
        "unknown"
    )
    .await
    .unwrap()
    .is_empty());

    // A contract and no vectors at all: empty on both.
    let empty = SessionId::from("graph-parity-contract-only");
    let empty_origin = NodeId::new();
    flush(
        &store,
        vec![
            plant_interaction(&empty, empty_origin, None, ts),
            Mutation::SetEmbedding {
                session_id: empty.clone(),
                embedding: Some(contract.clone()),
            },
        ]
        .into_iter()
        .chain(concept_with(
            &empty,
            empty_origin,
            "unembedded",
            ConceptType::Entity,
            None,
        ))
        .collect(),
    )
    .await;
    let empty_graph = holder_graph(&store, &empty).await;
    assert!(assert_same_answer(
        &store,
        &empty_graph,
        &empty,
        &probe,
        &contract,
        5,
        "no vectors"
    )
    .await
    .unwrap()
    .is_empty());
}

/// The graph holds one session; asking it about another is a caller bug and
/// is refused, never answered with a silently empty list.
#[tokio::test]
async fn vector_graph_source_refuses_a_foreign_session() {
    let graph = crate::graph::Graph::new(SessionId::from("mine"));
    let err = graph_vector_candidates(
        &graph,
        &SessionId::from("theirs"),
        &[1.0, 0.0, 0.0, 0.0],
        &vec_contract(4),
        5,
    )
    .unwrap_err();
    assert!(matches!(err, StoreError::Invariant(_)), "{err:?}");
}

/// The acceptance check on the committed fixture graphs: both fixtures,
/// synthetic unit vectors on every concept (the H1 construction), every
/// concept's own vector as a probe plus a blended one, at several limits.
#[cfg(feature = "fixtures")]
#[tokio::test]
async fn vector_graph_parity_on_both_committed_fixtures() {
    const DIM: usize = 8;
    for fixture in ["session-rest-api", "session-drift"] {
        let snap: GraphSnapshot = crate::fixtures::load_snapshot(fixture).unwrap();
        let sid = snap.session_id.clone();
        let contract = vec_contract(DIM);
        let store = vec_test_store(DIM);
        store.init_schema().await.unwrap();

        let mut mutations = snapshot_to_batch(&snap).mutations;
        mutations.push(Mutation::SetEmbedding {
            session_id: sid.clone(),
            embedding: Some(contract.clone()),
        });
        // Every third concept keeps no vector, so the scan's NULL filter is
        // exercised on real fixture data too.
        let mut probes = Vec::new();
        for (i, c) in snap.concepts.iter().enumerate() {
            let mut concept = c.clone();
            if i % 3 != 2 {
                let v = synthetic_unit_vector(i, DIM);
                probes.push(v.clone());
                concept.embedding = Some(v);
            }
            mutations.push(Mutation::UpsertNode {
                node: NodeKind::Concept(concept),
            });
        }
        let embedded = probes.len();
        probes.push(synthetic_unit_vector(1_000, DIM));
        flush(&store, mutations).await;
        let graph = holder_graph(&store, &sid).await;

        for (p, probe) in probes.iter().enumerate() {
            for limit in [1, 3, 8, embedded, MAX_VECTOR_CANDIDATE_LIMIT] {
                let hits = assert_same_answer(
                    &store,
                    &graph,
                    &sid,
                    probe,
                    &contract,
                    limit,
                    &format!("{fixture} probe {p}"),
                )
                .await
                .unwrap();
                assert_eq!(hits.len(), limit.min(embedded), "{fixture} probe {p}");
            }
        }
    }
}
