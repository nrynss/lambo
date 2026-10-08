//! F1/F2 vector search: capability, candidates, the write gate and the
//! exact-cosine oracle.

use super::*;

fn plant_concept_with_vector(
    sid: &SessionId,
    id: NodeId,
    origin: NodeId,
    content: &str,
    ts: DateTime<Utc>,
    embedding: Vec<f32>,
) -> Mutation {
    let Mutation::UpsertNode {
        node: NodeKind::Concept(mut concept),
    } = plant_concept(sid, id, origin, content, ConceptType::Entity, ts)
    else {
        unreachable!("plant_concept builds a concept upsert");
    };
    concept.embedding = Some(embedding);
    Mutation::UpsertNode {
        node: NodeKind::Concept(concept),
    }
}

/// Seed a session with a durable contract and the given vector-bearing concepts.
async fn seed_vectors(
    store: &SqliteStore,
    sid: &SessionId,
    contract: &EmbeddingContract,
    vectors: &[(NodeId, &str, Vec<f32>)],
) {
    let ts = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
    let origin = NodeId::new();
    let mut mutations = vec![
        plant_interaction(sid, origin, None, ts),
        Mutation::SetEmbedding {
            session_id: sid.clone(),
            embedding: Some(contract.clone()),
        },
    ];
    for (id, content, vector) in vectors {
        mutations.push(plant_concept_with_vector(
            sid,
            *id,
            origin,
            content,
            ts,
            vector.clone(),
        ));
    }
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

/// F2: the capability and a concrete width land together, and the width is the
/// configured one — never a constant minted inside the adapter (the amendment to
/// issue #5, which asked for a literal `Some(1024)`).
#[tokio::test]
async fn vector_search_capability_reports_a_configured_width() {
    let store = test_store();
    store.init_schema().await.unwrap();
    assert_eq!(store.capabilities(), Capabilities::VECTOR_SEARCH);
    assert_eq!(
        store.vector_dimensions(),
        Some(crate::embed::EmbedderConfig::default().dim)
    );
    // The pair must satisfy CON-5 — advertising without a width makes recall
    // refuse to resolve at all.
    crate::resolve::check_vector_search_contract(&store, crate::store::StoreKind::Sqlite).unwrap();

    // A configured width flows through, including a width no adapter hardcodes.
    let wide = vec_test_store(1536);
    assert_eq!(wide.vector_dimensions(), Some(1536));
    crate::resolve::check_vector_compatibility(wide.vector_dimensions(), 1536).unwrap();
    assert!(crate::resolve::check_vector_compatibility(wide.vector_dimensions(), 768).is_err());

    // Zero is refused: `Some(0)` would advertise a store that can hold no vector.
    let zero = SqliteStore::connect("sqlite::memory:")
        .unwrap()
        .with_vector_dim(0);
    assert!(
        matches!(zero, Err(StoreError::Invariant(_))),
        "{:?}",
        zero.err()
    );

    // The caller-limit guard survives the capability flip.
    assert!(matches!(
        store
            .vector_candidates_checked(
                &SessionId::from("x"),
                &[0.0; 8],
                &vec_contract(8),
                crate::store::MAX_VECTOR_CANDIDATE_LIMIT + 1,
            )
            .await
            .unwrap_err(),
        StoreError::Invariant(_)
    ));
}

/// F1: the scan scores exact cosine over the flushed BLOBs, best first; ties
/// by canonical key asc, then the smaller node id (issue #2): the ordering
/// contract MemoryStore and Cockroach share.
#[tokio::test]
async fn vector_candidates_score_exact_cosine_in_rank_order() {
    let store = vec_test_store(4);
    store.init_schema().await.unwrap();
    let sid = SessionId::from("vec-rank");
    let contract = vec_contract(4);
    // `near` is the probe itself, `mid` is 45° away, `far` is orthogonal.
    let near = NodeId::new();
    let mid = NodeId::new();
    let far = NodeId::new();
    seed_vectors(
        &store,
        &sid,
        &contract,
        &[
            (near, "near", vec![1.0, 0.0, 0.0, 0.0]),
            (mid, "mid", vec![1.0, 1.0, 0.0, 0.0]),
            (far, "far", vec![0.0, 1.0, 0.0, 0.0]),
        ],
    )
    .await;

    let probe = [1.0f32, 0.0, 0.0, 0.0];
    let hits = store
        .vector_candidates_checked(&sid, &probe, &contract, 10)
        .await
        .unwrap();
    assert_eq!(
        hits.iter().map(|s| s.item).collect::<Vec<_>>(),
        vec![near, mid, far]
    );
    assert!((hits[0].score - 1.0).abs() < 1e-6, "{hits:?}");
    assert!(
        (hits[1].score - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-6,
        "{hits:?}"
    );
    assert!(hits[2].score.abs() < 1e-6, "{hits:?}");

    // A concept with no vector is not a candidate at all (NULL, not score 0).
    let bare = NodeId::new();
    let ts = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
    let origin = store.load_session(&sid).await.unwrap().interactions[0].id;
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![plant_concept(
                    &sid,
                    bare,
                    origin,
                    "no vector",
                    ConceptType::Entity,
                    ts,
                )],
            },
            None,
        )
        .await
        .unwrap();
    let hits = store
        .vector_candidates_checked(&sid, &probe, &contract, 10)
        .await
        .unwrap();
    assert_eq!(hits.len(), 3, "the vector-less concept must not appear");

    // Ties break by canonical key asc, then id (issue #2), deterministically,
    // whichever order the two identical vectors were written in. The ids are
    // named so the id order contradicts the key order: `hi` carries
    // "written first" (smaller key) and `lo` carries "written second", so
    // the old id-first chain would have returned [lo, hi].
    let (lo, hi) = {
        let a = NodeId::new();
        let b = NodeId::new();
        if a.0 < b.0 {
            (a, b)
        } else {
            (b, a)
        }
    };
    let tie_sid = SessionId::from("vec-ties");
    seed_vectors(
        &store,
        &tie_sid,
        &contract,
        &[
            (hi, "written first", vec![1.0, 0.0, 0.0, 0.0]),
            (lo, "written second", vec![1.0, 0.0, 0.0, 0.0]),
        ],
    )
    .await;
    let hits = store
        .vector_candidates_checked(&tie_sid, &probe, &contract, 10)
        .await
        .unwrap();
    assert_eq!(
        hits.iter().map(|s| s.item).collect::<Vec<_>>(),
        vec![hi, lo],
        "canonical key order (written first < written second) must beat id order"
    );

    // The frozen unchecked surface answers with the session's own contract.
    let legacy = store.vector_candidates(&sid, &probe, 10).await.unwrap();
    assert_eq!(legacy[0].item, near);
}

/// F1: SQLite's score is on the **same scale** as Cockroach's, so
/// `semantic_match_threshold` and every recall rank mean the same thing on both.
///
/// Cockroach ranks by the L2 distance its `<->` operator returns and converts with
/// `1 - d²/2`; for the L2-normalized vectors every embedder is required to emit,
/// that identity *is* cosine. Getting a distance/score conversion wrong does not
/// fail — it just ranks differently — so the equality is asserted rather than
/// assumed. This is the part of SQLite↔Cockroach recall parity that needs no
/// cluster; ANN-vs-exact divergence in *which* candidates come back does.
#[tokio::test]
async fn vector_scores_match_the_cockroach_distance_conversion() {
    /// Cockroach's `distance_to_score`, transcribed.
    fn distance_to_score(dist: f64) -> f64 {
        (1.0 - 0.5 * dist * dist).clamp(-1.0, 1.0)
    }
    fn unit(v: [f32; 4]) -> Vec<f32> {
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter().map(|x| x / norm).collect()
    }
    fn l2(a: &[f32], b: &[f32]) -> f64 {
        f64::from(
            a.iter()
                .zip(b)
                .map(|(x, y)| (x - y) * (x - y))
                .sum::<f32>()
                .sqrt(),
        )
    }

    let store = vec_test_store(4);
    store.init_schema().await.unwrap();
    let sid = SessionId::from("vec-scale");
    let contract = vec_contract(4);
    let vectors = [
        unit([1.0, 0.0, 0.0, 0.0]),
        unit([1.0, 1.0, 0.0, 0.0]),
        unit([0.0, 1.0, 0.3, 0.0]),
        unit([-1.0, 0.0, 0.0, 0.0]),
    ];
    let seeded: Vec<(NodeId, &str, Vec<f32>)> = vectors
        .iter()
        .enumerate()
        .zip(["v0", "v1", "v2", "v3"])
        .map(|((_, v), label)| (NodeId::new(), label, v.clone()))
        .collect();
    seed_vectors(&store, &sid, &contract, &seeded).await;

    let probe = unit([1.0, 0.2, 0.0, 0.0]);
    let hits = store
        .vector_candidates_checked(&sid, &probe, &contract, 10)
        .await
        .unwrap();
    assert_eq!(hits.len(), 4);
    for (id, label, vector) in &seeded {
        let got = hits
            .iter()
            .find(|s| s.item == *id)
            .unwrap_or_else(|| panic!("{label} missing from {hits:?}"))
            .score;
        let want = distance_to_score(l2(&probe, vector));
        assert!(
            (got - want).abs() < 1e-6,
            "{label}: sqlite scored {got}, cockroach's conversion gives {want}"
        );
    }
}

/// B-E2E-R2-3: a probe with no direction is refused here exactly as it is
/// on the pg family, which got the refusal for free by encoding its probe.
///
/// Before this, `cosine` clamped the denominator to `1e-12` and a zero
/// probe scored every row a plausible `0.0`, so the same broken embedder
/// E2E-F9 postulated produced a loud refusal on a Postgres deployment and
/// a silent meaningless ranking on a SQLite one. Seeded rows and a real
/// session, so the answer is a refusal rather than an empty list.
///
/// The refusal has to happen before the store is read, which is where the
/// pg family does it: with the guard moved below the contract read this
/// still passes on the seeded session but the unknown-session leg returns
/// an empty list instead.
#[tokio::test]
async fn vector_candidates_refuse_a_zero_norm_probe() {
    let store = vec_test_store(4);
    store.init_schema().await.unwrap();
    let sid = SessionId::from("vec-zero-probe");
    let contract = vec_contract(4);
    seed_vectors(
        &store,
        &sid,
        &contract,
        &[(NodeId::new(), "stored", vec![1.0, 0.0, 0.0, 0.0])],
    )
    .await;

    for (label, session) in [
        ("seeded", sid.clone()),
        ("unknown", SessionId::from("nope")),
    ] {
        let err = store
            .vector_candidates_checked(&session, &[0.0, 0.0, 0.0, 0.0], &contract, 5)
            .await
            .unwrap_err();
        // Same message the codec gives on every write path and on the pg
        // family's query path: one input, one behaviour, three adapters.
        assert!(
            err.to_string().contains("zero norm"),
            "{label}: a probe with no direction must be refused, got {err}"
        );
    }

    // A direction that is merely tiny is still a direction, and the
    // legacy unchecked entry point funnels through the same guard.
    assert_eq!(
        store
            .vector_candidates(&sid, &[1e-6, 0.0, 0.0, 0.0], 5)
            .await
            .unwrap()
            .len(),
        1
    );
}

/// F1: the checked path refuses a durable/expected contract change — kind, model
/// and dim alike — instead of ranking vectors the caller cannot interpret. Same
/// `Invariant` classification as Cockroach and `VectorSearchStore`, so a caller
/// handles all three identically.
#[tokio::test]
async fn vector_candidates_refuse_a_changed_embedding_contract() {
    let store = vec_test_store(4);
    store.init_schema().await.unwrap();
    let sid = SessionId::from("vec-contract");
    let durable = vec_contract(4);
    let id = NodeId::new();
    seed_vectors(
        &store,
        &sid,
        &durable,
        &[(id, "stored", vec![1.0, 0.0, 0.0, 0.0])],
    )
    .await;
    let probe = [1.0f32, 0.0, 0.0, 0.0];

    // Sanity: the matching contract is served.
    assert_eq!(
        store
            .vector_candidates_checked(&sid, &probe, &durable, 5)
            .await
            .unwrap()
            .len(),
        1
    );

    let mismatches = [
        (
            "kind",
            EmbeddingContract {
                kind: "bge_m3".into(),
                ..durable.clone()
            },
        ),
        (
            "model",
            EmbeddingContract {
                model: Some("other-model".into()),
                ..durable.clone()
            },
        ),
        (
            "model cleared",
            EmbeddingContract {
                model: None,
                ..durable.clone()
            },
        ),
    ];
    for (what, expected) in mismatches {
        let err = store
            .vector_candidates_checked(&sid, &probe, &expected, 5)
            .await
            .unwrap_err();
        assert!(
            matches!(err, StoreError::Invariant(_)),
            "{what} mismatch must be Invariant, got {err:?}"
        );
        assert!(
            err.to_string().contains("embedding contract changed"),
            "{what}: {err}"
        );
    }

    // A dim mismatch is refused by the same comparison; the probe is sized to the
    // expected contract so the refusal is the contract check, not the width guard.
    let err = store
        .vector_candidates_checked(&sid, &[1.0, 0.0], &vec_contract(2), 5)
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Invariant(_)), "{err:?}");
    assert!(
        err.to_string().contains("embedding contract changed"),
        "{err}"
    );

    // A probe that disagrees with the contract it claims is a caller bug, not a
    // silently zero-scored scan (`cosine` returns 0.0 on a length mismatch).
    let err = store
        .vector_candidates_checked(&sid, &[1.0, 0.0], &durable, 5)
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Invariant(_)), "{err:?}");
    assert!(err.to_string().contains("dimensions"), "{err}");
}

/// F1: nothing-to-search is an empty answer, never an error — the shape a
/// vector-capable store returns before its first embedding lands. Recall must not
/// fail on a fresh database.
#[tokio::test]
async fn vector_candidates_are_empty_before_the_first_vector() {
    let store = vec_test_store(4);
    store.init_schema().await.unwrap();
    let contract = vec_contract(4);
    let probe = [1.0f32, 0.0, 0.0, 0.0];

    // A session no writer has touched.
    assert!(store
        .vector_candidates_checked(&SessionId::from("never-written"), &probe, &contract, 5)
        .await
        .unwrap()
        .is_empty());
    assert!(store
        .vector_candidates(&SessionId::from("never-written"), &probe, 5)
        .await
        .unwrap()
        .is_empty());

    // A session with rows but no durable contract: legacy vectors are quarantined
    // at materialization, so an unstamped session is an empty pool.
    let sid = SessionId::from("vec-unstamped");
    let ts = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
    let origin = NodeId::new();
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![
                    plant_interaction(&sid, origin, None, ts),
                    plant_concept(
                        &sid,
                        NodeId::new(),
                        origin,
                        "unstamped",
                        ConceptType::Entity,
                        ts,
                    ),
                ],
            },
            None,
        )
        .await
        .unwrap();
    assert!(store
        .vector_candidates_checked(&sid, &probe, &contract, 5)
        .await
        .unwrap()
        .is_empty());
    assert!(store
        .vector_candidates(&sid, &probe, 5)
        .await
        .unwrap()
        .is_empty());

    // A stamped session whose concepts carry no vectors yet.
    let stamped = SessionId::from("vec-stamped-empty");
    seed_vectors(&store, &stamped, &contract, &[]).await;
    assert!(store
        .vector_candidates_checked(&stamped, &probe, &contract, 5)
        .await
        .unwrap()
        .is_empty());
}

/// F1: a stored BLOB whose decoded width disagrees with the session contract is
/// corruption, reported — not truncated, and not silently ranked last by `cosine`'s
/// length-mismatch 0.0.
#[tokio::test]
async fn vector_candidates_refuse_a_malformed_stored_blob() {
    let store = vec_test_store(4);
    store.init_schema().await.unwrap();
    let sid = SessionId::from("vec-corrupt");
    let contract = vec_contract(4);
    let id = NodeId::new();
    seed_vectors(
        &store,
        &sid,
        &contract,
        &[(id, "stored", vec![1.0, 0.0, 0.0, 0.0])],
    )
    .await;
    let probe = [1.0f32, 0.0, 0.0, 0.0];

    // Direct SQL is used here because it is the only way *left* to produce these:
    // the write path encodes through the shared codec, and since F-R1-1
    // `enforce_concept_vector_widths` refuses a width-mismatched vector at the
    // flush gate too (`vector_write_gate_refuses_a_concept_of_the_wrong_width` covers
    // that surface). What remains reachable is an externally edited database —
    // which is exactly why the read path still cannot trust the stored width.
    for (what, blob) in [
        ("short", b"[1,0,0]".to_vec()),
        ("long", b"[1,0,0,0,0]".to_vec()),
        ("unparseable", b"[1,0,oops,0]".to_vec()),
        ("not utf-8", vec![0xff, 0xfe, 0x00]),
    ] {
        sqlx::query("UPDATE concepts SET embedding = ? WHERE id = ?")
            .bind(&blob)
            .bind(id.0.to_string())
            .execute(store.pool())
            .await
            .unwrap();
        let err = store
            .vector_candidates_checked(&sid, &probe, &contract, 5)
            .await
            .unwrap_err();
        assert!(
            matches!(err, StoreError::Backend(_)),
            "{what} blob must be a Backend error, got {err:?}"
        );
    }
}

/// F-R1-1: the **write gate**. The review reproduced session-wide, permanent
/// recall failure from one `flush` through the public `GraphStore` surface — no
/// direct SQL: a batch stamping `dim = 4` and then upserting one 4-wide and one
/// 3-wide concept was accepted, after which `vector_candidates_checked`,
/// `vector_candidates` and `recall::candidates::gather` all failed for the whole
/// session, including the *good* concept. This is that repro, expressed through
/// the same public types, asserting the batch is now refused instead.
///
/// The external-crate form the reviewer used is expressible in-crate: `flush`,
/// `MutationBatch`, `Mutation`, `Node` and `Concept` are all public, and
/// `SqliteStore` reaches them through the same trait. What the in-crate form
/// cannot show is a *foreign* caller, which is immaterial — the gate is in the
/// adapter, below the trait boundary.
#[tokio::test]
async fn vector_write_gate_refuses_a_concept_of_the_wrong_width() {
    let store = vec_test_store(4);
    store.init_schema().await.unwrap();
    let sid = SessionId::from("vec-write-gate");
    let contract = vec_contract(4);
    let ts = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
    let origin = NodeId::new();
    let good = NodeId::new();
    let bad = NodeId::new();

    // The reviewer's batch, verbatim in shape: interaction, SetEmbedding{dim:4},
    // a 4-wide concept, then a 3-wide one.
    let batch = MutationBatch {
        mutation_epoch: 0,
        gc_mark: Default::default(),
        mutations: vec![
            plant_interaction(&sid, origin, None, ts),
            Mutation::SetEmbedding {
                session_id: sid.clone(),
                embedding: Some(contract.clone()),
            },
            plant_concept_with_vector(&sid, good, origin, "good", ts, vec![1.0, 0.0, 0.0, 0.0]),
            plant_concept_with_vector(&sid, bad, origin, "bad", ts, vec![1.0, 0.0, 0.0]),
        ],
    };
    let err = store.flush(&batch, None).await.unwrap_err();
    assert!(
        matches!(err, StoreError::Invariant(_)),
        "a width-mismatched concept must be refused as Invariant, got {err:?}"
    );
    let msg = err.to_string();
    for needle in ["3-dimensional", "stores vectors of 4"] {
        assert!(msg.contains(needle), "message must name both widths: {msg}");
    }

    // The refusal is atomic: `?` inside the flush transaction rolls the whole
    // batch back, so the *good* concept did not land either. That matters — a
    // half-applied batch would leave the caller unable to reason about retry.
    assert!(matches!(
        store.load_session(&sid).await.unwrap_err(),
        StoreError::SessionNotFound(_)
    ));

    // And the session is still usable: the same batch without the bad row
    // succeeds, and its vector leg answers. Before the gate, the session was
    // permanently poisoned at this point.
    seed_vectors(
        &store,
        &sid,
        &contract,
        &[(good, "good", vec![1.0, 0.0, 0.0, 0.0])],
    )
    .await;
    let hits = store
        .vector_candidates_checked(&sid, &[1.0, 0.0, 0.0, 0.0], &contract, 5)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1, "the surviving session still answers");
    assert_eq!(hits[0].item, good);
}

/// F-R1-1, the ordering half: what the contract *is* at the moment each concept
/// is validated. `plan_flush` makes `SetEmbedding` a barrier, so this is a
/// property of the planner rather than of statement luck.
#[tokio::test]
async fn vector_write_gate_reads_the_contract_each_step_sees() {
    let store = vec_test_store(4);
    store.init_schema().await.unwrap();
    let ts = Utc.timestamp_opt(1_752_000_000, 0).unwrap();

    // (a) Stamp-then-upsert inside ONE batch passes: the concepts are planned
    // after the barrier, so they are validated against the width just stamped —
    // a session's very first vectors always arrive this way.
    let fresh = SessionId::from("gate-stamp-then-upsert");
    let origin = NodeId::new();
    let c = NodeId::new();
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![
                    plant_interaction(&fresh, origin, None, ts),
                    Mutation::SetEmbedding {
                        session_id: fresh.clone(),
                        embedding: Some(vec_contract(4)),
                    },
                    plant_concept_with_vector(
                        &fresh,
                        c,
                        origin,
                        "after the stamp",
                        ts,
                        vec![1.0, 0.0, 0.0, 0.0],
                    ),
                ],
            },
            None,
        )
        .await
        .expect("stamp-then-upsert of a matching width must pass");

    // (b) A concept upserted BEFORE the stamp is validated against the contract
    // that was durable when it was written — here none, so it is accepted, and
    // `set_embedding`'s quarantine then NULLs it rather than leaving a vector the
    // new contract cannot interpret. This is why "no contract stamped yet" is
    // safe to accept at the gate.
    let pre = SessionId::from("gate-upsert-then-stamp");
    let origin2 = NodeId::new();
    let c2 = NodeId::new();
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![
                    plant_interaction(&pre, origin2, None, ts),
                    plant_concept_with_vector(
                        &pre,
                        c2,
                        origin2,
                        "before any stamp",
                        ts,
                        // Deliberately not 4 wide: with no contract there is no
                        // authority to check it against.
                        vec![1.0, 0.0, 0.0],
                    ),
                    Mutation::SetEmbedding {
                        session_id: pre.clone(),
                        embedding: Some(vec_contract(4)),
                    },
                ],
            },
            None,
        )
        .await
        .expect("a vector written before any contract is accepted");
    let loaded = store.load_session(&pre).await.unwrap();
    assert_eq!(loaded.embedding.as_ref(), Some(&vec_contract(4)));
    assert_eq!(
        loaded.concepts[0].embedding, None,
        "quarantined by set_embedding: stamping a width NULLs every vector of a \
             different width, and from a NULL contract that is all of them"
    );
    // Consequently the read path is clean rather than poisoned.
    assert!(store
        .vector_candidates_checked(&pre, &[1.0, 0.0, 0.0, 0.0], &vec_contract(4), 5)
        .await
        .unwrap()
        .is_empty());
}

/// F-R2-1, the ordering round 2 found: a **restamp**. Every concept can match the
/// contract of its own moment — so `enforce_concept_vector_widths` has nothing to
/// refuse — while the *final* contract disagrees with a vector written earlier in
/// the same batch. Round 2 reproduced exactly this through one public `flush` with
/// no direct SQL and reached the terminal state round 1 called fatal: a 4-wide
/// vector under a `dim = 3` contract, after which the whole session's vector leg
/// fails on every read because `select_session_vectors` returns on the first bad
/// row.
///
/// The fix is on the other side of the barrier — `set_embedding` now NULLs every
/// vector of a different width when it stamps — so the batch is **accepted and
/// self-heals** rather than refused: the earlier vector is erased, and the
/// terminal state is a `dim = 3` contract beside 3-wide vectors only. This test
/// asserts the terminal state is clean, which is the property that matters; it
/// would fail identically if a future change made the batch pass *and* keep the
/// orphan.
#[tokio::test]
async fn vector_write_gate_restamp_inside_one_batch_cannot_orphan_earlier_vectors() {
    let store = vec_test_store(4);
    store.init_schema().await.unwrap();
    let ts = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
    let sid = SessionId::from("gate-restamp-one-batch");
    let origin = NodeId::new();
    let wide = NodeId::new();
    let narrow = NodeId::new();

    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![
                    plant_interaction(&sid, origin, None, ts),
                    Mutation::SetEmbedding {
                        session_id: sid.clone(),
                        embedding: Some(vec_contract(4)),
                    },
                    plant_concept_with_vector(
                        &sid,
                        wide,
                        origin,
                        "written under dim 4",
                        ts,
                        vec![1.0, 0.0, 0.0, 0.0],
                    ),
                    Mutation::SetEmbedding {
                        session_id: sid.clone(),
                        embedding: Some(vec_contract(3)),
                    },
                    plant_concept_with_vector(
                        &sid,
                        narrow,
                        origin,
                        "written under dim 3",
                        ts,
                        vec![1.0, 0.0, 0.0],
                    ),
                ],
            },
            None,
        )
        .await
        .expect("every concept matches the contract of its own step, so nothing is refused");

    assert_restamp_left_no_orphan(&store, &sid, wide, narrow).await;
}

/// F-R2-1 across a flush boundary — the shape an operator actually produces,
/// since a restamp normally follows a commit rather than sharing a batch with the
/// vectors it invalidates. Round 2 reproduced both; the quarantine is a property
/// of the `SetEmbedding` statement, so both must end clean for the same reason.
#[tokio::test]
async fn vector_write_gate_restamp_across_two_flushes_cannot_orphan_earlier_vectors() {
    let store = vec_test_store(4);
    store.init_schema().await.unwrap();
    let ts = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
    let sid = SessionId::from("gate-restamp-two-flushes");
    let origin = NodeId::new();
    let wide = NodeId::new();
    let narrow = NodeId::new();

    // Flush 1: an ordinary, entirely legal dim-4 session.
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![
                    plant_interaction(&sid, origin, None, ts),
                    Mutation::SetEmbedding {
                        session_id: sid.clone(),
                        embedding: Some(vec_contract(4)),
                    },
                    plant_concept_with_vector(
                        &sid,
                        wide,
                        origin,
                        "written under dim 4",
                        ts,
                        vec![1.0, 0.0, 0.0, 0.0],
                    ),
                ],
            },
            None,
        )
        .await
        .expect("a dim-4 session with 4-wide vectors is legal");
    // Its read path answers, so the state being repaired below is a live one.
    assert_eq!(
        store
            .vector_candidates_checked(&sid, &[1.0, 0.0, 0.0, 0.0], &vec_contract(4), 5)
            .await
            .unwrap()
            .len(),
        1
    );

    // Flush 2: restamp to dim 3 and write a 3-wide concept. The already-committed
    // 4-wide vector is the one the old NULL-only quarantine left behind.
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![
                    Mutation::SetEmbedding {
                        session_id: sid.clone(),
                        embedding: Some(vec_contract(3)),
                    },
                    plant_concept_with_vector(
                        &sid,
                        narrow,
                        origin,
                        "written under dim 3",
                        ts,
                        vec![1.0, 0.0, 0.0],
                    ),
                ],
            },
            None,
        )
        .await
        .expect("the restamp and the concepts that follow it are each locally valid");

    assert_restamp_left_no_orphan(&store, &sid, wide, narrow).await;
}

/// Shared terminal-state assertion for the two restamp orderings: the durable
/// contract is the restamped one, the vector written under the *old* width is
/// gone rather than orphaned, the vector written under the new width survives,
/// and the read path answers cleanly instead of failing session-wide.
async fn assert_restamp_left_no_orphan(
    store: &SqliteStore,
    sid: &SessionId,
    wide: NodeId,
    narrow: NodeId,
) {
    let loaded = store.load_session(sid).await.unwrap();
    assert_eq!(
        loaded.embedding.as_ref(),
        Some(&vec_contract(3)),
        "the last stamp is the durable contract"
    );
    let find = |id: NodeId| {
        loaded
            .concepts
            .iter()
            .find(|c| c.id == id)
            .expect("concept present")
            .embedding
            .clone()
    };
    assert_eq!(
        find(wide),
        None,
        "the 4-wide vector must be quarantined by the dim-3 stamp, not left \
             under a contract that says 3"
    );
    assert_eq!(
        find(narrow),
        Some(vec![1.0f32, 0.0, 0.0]),
        "a vector written under the new contract is untouched"
    );
    // The whole point: before F-R2-1 both of these were `Backend` errors for the
    // entire session — including for the good concept — permanently.
    let hits = store
        .vector_candidates_checked(sid, &[1.0, 0.0, 0.0], &vec_contract(3), 5)
        .await
        .expect("the session's vector leg still answers after a restamp");
    assert_eq!(
        hits.len(),
        1,
        "only the surviving 3-wide vector is a candidate"
    );
    assert_eq!(hits[0].item, narrow);
    // The frozen surface reads the stored contract itself, so it must agree.
    let frozen = store
        .vector_candidates(sid, &[1.0, 0.0, 0.0], 5)
        .await
        .expect("the frozen surface is clean too");
    assert_eq!(frozen, hits);
}

/// The scope decision F-R2-1 forced, pinned so it cannot be "fixed" into a
/// data-losing quarantine-on-any-change: the quarantine keys on **width**, so a
/// same-width `kind`/`model` relabel leaves every vector in place. That is not an
/// oversight — `Graph::replace_embedding_with_operator_override`, the
/// `--allow-embedding-mismatch` writer attach path, requires equal widths and
/// deliberately permits a same-kind model-identifier rename *with the vectors
/// intact*. Erasing them here would destroy data on the one migration path built
/// to keep it, and width is the only contract property this storage can enforce:
/// a same-width relabel leaves every BLOB decodable.
///
/// Both halves of the `kind`/`model` property are restamped here (F-R3-2): a
/// model-identifier rename first, then a `kind` change at the same width. The
/// second is the case the two tiers deliberately disagree on — the graph tier's
/// `replace_embedding_with_operator_override` *refuses* a `kind` change while any
/// vector remains, where storage keys on width alone and keeps them.
#[tokio::test]
async fn vector_write_gate_same_width_relabel_keeps_the_vectors() {
    let store = vec_test_store(4);
    store.init_schema().await.unwrap();
    let sid = SessionId::from("gate-same-width-relabel");
    let c = NodeId::new();
    seed_vectors(
        &store,
        &sid,
        &vec_contract(4),
        &[(c, "kept across a rename", vec![1.0, 0.0, 0.0, 0.0])],
    )
    .await;

    let renamed = EmbeddingContract {
        kind: "fixture".into(),
        model: Some("test-model-v2".into()),
        dim: 4,
    };
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![Mutation::SetEmbedding {
                    session_id: sid.clone(),
                    embedding: Some(renamed.clone()),
                }],
            },
            None,
        )
        .await
        .unwrap();

    let loaded = store.load_session(&sid).await.unwrap();
    assert_eq!(loaded.embedding.as_ref(), Some(&renamed));
    assert_eq!(
        loaded.concepts[0].embedding.as_deref(),
        Some([1.0f32, 0.0, 0.0, 0.0].as_slice()),
        "a same-width relabel must not erase vectors the operator declared compatible"
    );
    let hits = store
        .vector_candidates_checked(&sid, &[1.0, 0.0, 0.0, 0.0], &renamed, 5)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].item, c);

    // F-R3-2: the `kind` half, at the same width. Without this a regression
    // widening the predicate to fire on a `kind` change would pass the rename
    // above unchanged.
    let rekinded = EmbeddingContract {
        kind: "bge_m3".into(),
        model: Some("test-model-v2".into()),
        dim: 4,
    };
    store
        .flush(
            &MutationBatch {
                mutation_epoch: 0,
                gc_mark: Default::default(),
                mutations: vec![Mutation::SetEmbedding {
                    session_id: sid.clone(),
                    embedding: Some(rekinded.clone()),
                }],
            },
            None,
        )
        .await
        .unwrap();
    let loaded = store.load_session(&sid).await.unwrap();
    assert_eq!(loaded.embedding.as_ref(), Some(&rekinded));
    assert_eq!(
        loaded.concepts[0].embedding.as_deref(),
        Some([1.0f32, 0.0, 0.0, 0.0].as_slice()),
        "a same-width kind change must not erase vectors either"
    );
    let hits = store
        .vector_candidates_checked(&sid, &[1.0, 0.0, 0.0, 0.0], &rekinded, 5)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].item, c);
}

/// F1 boundaries: a top-k above the candidate count returns the whole pool, and
/// `k = 0` returns empty without touching the database.
#[tokio::test]
async fn vector_candidates_boundaries_top_k_and_zero() {
    let store = vec_test_store(4);
    store.init_schema().await.unwrap();
    let sid = SessionId::from("vec-bounds");
    let contract = vec_contract(4);
    // Distinct content: the partial unique index on (session_id, canonical_key)
    // rejects duplicate non-Observation keys.
    let labels = ["c0", "c1", "c2"];
    let seeded: Vec<(NodeId, &str, Vec<f32>)> = labels
        .iter()
        .enumerate()
        .map(|(i, label)| (NodeId::new(), *label, vec![1.0, i as f32 * 0.25, 0.0, 0.0]))
        .collect();
    seed_vectors(&store, &sid, &contract, &seeded).await;
    let probe = [1.0f32, 0.0, 0.0, 0.0];

    let all = store
        .vector_candidates_checked(&sid, &probe, &contract, 100)
        .await
        .unwrap();
    assert_eq!(all.len(), 3, "top-k above the pool returns the whole pool");
    let one = store
        .vector_candidates_checked(&sid, &probe, &contract, 1)
        .await
        .unwrap();
    assert_eq!(one, all[..1].to_vec(), "k truncates the same ranking");
    assert!(store
        .vector_candidates_checked(&sid, &probe, &contract, 0)
        .await
        .unwrap()
        .is_empty());
    // k = 0 short-circuits before the contract is even read, so a contract that
    // would otherwise be refused still returns empty rather than erroring.
    assert!(store
        .vector_candidates_checked(&sid, &probe, &vec_contract(999), 0)
        .await
        .unwrap()
        .is_empty());
    assert!(store
        .vector_candidates(&sid, &probe, 0)
        .await
        .unwrap()
        .is_empty());
}

/// An **exact-cosine oracle** with the ordering contract of `MemoryStore`'s
/// `VectorSearchStore` and of `rank_by_cosine`: best score first by `total_cmp`
/// descending, ties broken by canonical key ascending then the smaller
/// `NodeId` (issue #2), then truncated to `limit`.
///
/// Reimplemented here rather than reused because `VectorSearchStore` is private to
/// `memory.rs`'s test module. That is not a weakness of the comparison: the oracle
/// is deliberately the naive formulation — score everything, sort, truncate — with
/// no transaction, no BLOB codec, no SQL and no width checking, so agreement is
/// evidence about the adapter rather than about shared code.
#[cfg(feature = "fixtures")]
fn cosine_oracle(
    probe: &[f32],
    pool: &[(NodeId, Vec<f32>, String)],
    limit: usize,
) -> Vec<Scored<NodeId>> {
    let mut scored: Vec<(Scored<NodeId>, &str)> = pool
        .iter()
        .map(|(id, v, key)| {
            (
                Scored::new(*id, f64::from(crate::embed::cosine(probe, v))),
                key.as_str(),
            )
        })
        .collect();
    scored.sort_by(|(a, a_key), (b, b_key)| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| tie_break_by_key(Some(a_key), &a.item, Some(b_key), &b.item))
    });
    let mut scored: Vec<Scored<NodeId>> = scored.into_iter().map(|(s, _)| s).collect();
    scored.truncate(limit);
    scored
}

/// **Acceptance: Done-when box 5, the cluster-free half** (F-R1-4).
///
/// The vector agreement matrix, in the shape of
/// `assert_structural_agreement_matrix`: both committed fixture graphs, plus a
/// stamped contract and synthetic unit vectors, seeded into `SqliteStore` and into
/// an exact-cosine oracle; every returned `Vec<Scored<NodeId>>` asserted **exactly
/// equal** across probes × limits — same ids, same order, same `f64` scores.
///
/// What this replaces: `vector_scores_match_the_cockroach_distance_conversion`
/// *transcribes* Cockroach's `distance_to_score` into its own body, so it proves
/// the formula was copied correctly and nothing about which candidates or which
/// ranks an adapter returns. This asserts the answers themselves, over real
/// committed graphs, through the whole adapter path — flush → BLOB codec →
/// transaction → scan → rank.
///
/// The probe set is chosen so ranking mistakes are visible: a stored vector itself
/// (score 1.0, must rank first), its negation (score −1.0, must rank *last* — this
/// is what catches a `sort` that mishandles sign or a `total_cmp` swapped for
/// `partial_cmp`), a midpoint between two stored vectors, and an off-axis probe.
/// Limits sweep 1 → past the pool size to pin truncation and the whole-pool case.
#[cfg(feature = "fixtures")]
#[tokio::test]
async fn vector_candidates_agree_with_an_exact_cosine_oracle_on_both_fixtures() {
    const DIM: usize = 8;
    let mut total_assertions = 0usize;
    for fixture in ["session-rest-api", "session-drift"] {
        let snap: GraphSnapshot = crate::fixtures::load_snapshot(fixture).unwrap();
        let sid = snap.session_id.clone();
        let contract = vec_contract(DIM);

        // The fixtures carry no contract and no vectors (asserted below, so this
        // stays honest if a fixture ever gains them). Attach both.
        assert!(
            snap.embedding.is_none() && snap.concepts.iter().all(|c| c.embedding.is_none()),
            "{fixture}: fixture is expected to carry no vectors; \
                 this test supplies them"
        );

        let pool: Vec<(NodeId, Vec<f32>, String)> = snap
            .concepts
            .iter()
            .enumerate()
            .map(|(i, c)| (c.id, synthetic_unit_vector(i, DIM), c.canonical_key.clone()))
            .collect();

        let store = vec_test_store(DIM);
        store.init_schema().await.unwrap();

        // Structure first (interactions are FK targets for concepts), then the
        // contract, then the vector-bearing concepts. `SetEmbedding` is a planner
        // barrier, so the concepts are validated against the width it just
        // stamped — the ordering the write gate depends on (F-R1-1).
        let mut mutations = snapshot_to_batch(&snap).mutations;
        mutations.push(Mutation::SetEmbedding {
            session_id: sid.clone(),
            embedding: Some(contract.clone()),
        });
        for (i, c) in snap.concepts.iter().enumerate() {
            let mut concept = c.clone();
            concept.embedding = Some(synthetic_unit_vector(i, DIM));
            mutations.push(Mutation::UpsertNode {
                node: NodeKind::Concept(concept),
            });
        }
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

        // Probes, in the pool's own space so scores span [-1, 1].
        let first = &pool[0].1;
        let second = &pool[1].1;
        let negated: Vec<f32> = first.iter().map(|x| -x).collect();
        let midpoint: Vec<f32> = {
            let mut m: Vec<f32> = first
                .iter()
                .zip(second.iter())
                .map(|(a, b)| (a + b) / 2.0)
                .collect();
            let n = m.iter().map(|x| x * x).sum::<f32>().sqrt();
            for x in &mut m {
                *x /= n;
            }
            m
        };
        let off_axis = synthetic_unit_vector(usize::from(u8::MAX), DIM);
        let probes: [(&str, Vec<f32>); 4] = [
            ("stored-itself", first.clone()),
            ("negated", negated),
            ("midpoint", midpoint),
            ("off-axis", off_axis),
        ];

        for (label, probe) in &probes {
            for limit in [1usize, 3, 5, pool.len(), pool.len() + 7] {
                let got = store
                    .vector_candidates_checked(&sid, probe, &contract, limit)
                    .await
                    .unwrap();
                let want = cosine_oracle(probe, &pool, limit);
                assert_eq!(
                    got, want,
                    "{fixture}: probe {label}, limit {limit} — SQLite disagrees \
                         with the exact-cosine oracle"
                );
                total_assertions += 1;
            }
        }

        // Anchors independent of the oracle, so a bug shared by both would still
        // be caught: a stored vector scores 1.0 against itself and ranks first;
        // its negation scores -1.0 and ranks LAST of the whole pool.
        let top = store
            .vector_candidates_checked(&sid, first, &contract, pool.len())
            .await
            .unwrap();
        assert_eq!(top[0].item, pool[0].0);
        assert!((top[0].score - 1.0).abs() < 1e-6, "{:?}", top[0]);
        let bottom = store
            .vector_candidates_checked(
                &sid,
                &first.iter().map(|x| -x).collect::<Vec<f32>>(),
                &contract,
                pool.len(),
            )
            .await
            .unwrap();
        assert_eq!(bottom.len(), pool.len());
        assert_eq!(bottom[pool.len() - 1].item, pool[0].0);
        assert!(
            (bottom[pool.len() - 1].score + 1.0).abs() < 1e-6,
            "{:?}",
            bottom[pool.len() - 1]
        );
    }
    // 2 fixtures × 4 probes × 5 limits.
    assert_eq!(total_assertions, 40, "matrix dimensions drifted");
}
