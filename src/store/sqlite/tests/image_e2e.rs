//! #22 PR 3 acceptance (AC3) on SQLite: a text query recalls an image
//! concept through the vector leg, synchronously and through the write
//! queue, and the image's vector and source survive a reload.
//!
//! The fixture embeds a PNG labelled `red silk saree` as exactly the vector
//! a bare text query `red silk saree` gets (design section 9), and the
//! caption, `render 17`, shares no word with the query, so the keyword leg
//! cannot be what finds it.

use super::*;
use crate::embed::{png_with_label, Embedder, FixtureEmbedder};
use crate::graph::image::{ImageDerive, ImagePayload};
use crate::memory::Memory;
use crate::store::GraphStore;
use crate::types::{MatchStrategy, RecallQuery, VectorOrigin};
use std::sync::Arc;

const QUERY: &str = "red silk saree";

fn contract() -> EmbeddingContract {
    EmbeddingContract {
        kind: "fixture".into(),
        model: None,
        dim: FixtureEmbedder::new().dimensions(),
    }
}

async fn open(store: Arc<SqliteStore>, session: &str) -> Memory {
    Memory::builder()
        .session(session)
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .match_strategy(MatchStrategy::Hybrid)
        .store(store as Arc<dyn GraphStore>)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract())
        .build()
        .await
        .expect("build")
}

/// The cosine-order fixture isolates the vector leg from structural scoring.
/// Its original default 50/50 blend could put the 0.3 look above the 0.5
/// look when their daemon scores differed; that is a steady-state inversion,
/// so #79's temporary missing-score rule cannot fix it.
async fn open_graded(store: Arc<SqliteStore>, session: &str) -> Memory {
    let config = crate::Config {
        recall_weights: crate::config::RecallWeights {
            w_daemon: 0.0,
            w_query: 1.0,
        },
        ..crate::Config::default()
    };
    Memory::builder()
        .session(session)
        .agent("agent-a")
        .config(config)
        .flush_interval(Duration::from_secs(3_600))
        .match_strategy(MatchStrategy::Hybrid)
        .store(store as Arc<dyn GraphStore>)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract())
        .build()
        .await
        .expect("build")
}

async fn derive_noise(mem: &Memory) {
    for text in [
        "quantum chromodynamics lattice gauge",
        "billing retries change",
        "user schema",
    ] {
        mem.derive(
            &[(text, ConceptType::Entity)],
            &crate::graph::derive::ParentOf::none(),
        )
        .await
        .unwrap();
    }
}

fn image<'a>(png: &'a [u8]) -> ImageDerive<'a> {
    ImageDerive {
        caption: "render 17",
        concept_type: ConceptType::Resource,
        image_id: Some("r17"),
        payload: ImagePayload::Bytes(
            crate::surface::image::validate(png, "image/png").expect("valid png"),
        ),
        parent_of: &[],
        event_time: None,
    }
}

/// The image concept is the top hit for the text query (design section 9
/// step 3), found by the vector leg (at the label's own vector) and not by
/// the keyword leg, and no other candidate's query evidence comes near it.
///
/// This older #22 acceptance test waits for the daemon to settle because
/// it also checks the scored steady state. The cold-start path, including a
/// lower-scored established relevant concept and supplied image vectors, is
/// checked separately below against a frozen pre-derive score table.
async fn assert_recalled_by_the_vector_leg(mem: &Memory, image: NodeId) {
    mem.settle_daemon().await;
    let detailed = mem
        .recall_detailed(RecallQuery {
            query: QUERY.into(),
            top_k: 5,
            max_tokens: 1_000,
            traversal_depth: 1,
        })
        .await
        .unwrap();
    assert_eq!(
        detailed.hits.first().map(|h| h.node_id),
        Some(image),
        "the image concept is the top hit: {:?}",
        detailed.hits
    );
    let legs = detailed
        .legs
        .get(&image)
        .expect("the image has leg provenance");
    let score = legs.vector.expect("found by the vector leg");
    assert!(score > 0.99, "the label's own vector, got {score}");
    assert_eq!(
        legs.keyword, None,
        "the caption shares no word with the query"
    );
    for (node, other) in &detailed.legs {
        if *node != image {
            let best = [other.vector, other.keyword, other.recent]
                .into_iter()
                .flatten()
                .fold(0.0_f64, f64::max);
            assert!(
                best < 0.5,
                "{node}: no other candidate matches the query ({best})"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_text_query_recalls_an_image_concept_through_the_vector_leg() {
    let _quiet = crate::test_util::quiet_logs();
    let (_dir, path) = scratch_db();
    let store = Arc::new(SqliteStore::connect(&path).unwrap());
    store.init_schema().await.unwrap();
    let session = "sqlite-image-sync";

    let mem = open(store.clone(), session).await;
    derive_noise(&mem).await;
    let png = png_with_label(QUERY);
    let out = mem
        .derive_image_as(&"agent-a".into(), image(&png))
        .await
        .unwrap();
    assert_eq!((out.created.len(), out.embedded), (1, 1));
    let image_id = out.created[0];
    assert_recalled_by_the_vector_leg(&mem, image_id).await;
    mem.close().await.unwrap();

    // Durable: the vector, its source and the contract come back from
    // SQLite, and SQLite's own checked scan ranks the image first.
    let snapshot = store.load_session(&SessionId::from(session)).await.unwrap();
    let stored = snapshot.concepts.iter().find(|c| c.id == image_id).unwrap();
    assert_eq!(stored.content, "render 17 [image:r17]");
    assert_eq!(
        stored.embedding.as_deref(),
        Some(FixtureEmbedder::new().embed_sync(QUERY).as_slice())
    );
    let source = stored.embedding_source.as_ref().expect("source persisted");
    assert_eq!(source.origin, VectorOrigin::Server);
    assert_eq!(source.sha256.as_ref().map(String::len), Some(64));
    let probe = FixtureEmbedder::new().embed_sync(QUERY);
    let ranked = store
        .vector_candidates_checked(&SessionId::from(session), &probe, &contract(), 3)
        .await
        .unwrap();
    assert_eq!(ranked.first().map(|s| s.item), Some(image_id));

    let reopened = open(store.clone(), session).await;
    assert_recalled_by_the_vector_leg(&reopened, image_id).await;
    reopened.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_acknowledged_image_derive_is_recalled_through_the_vector_leg() {
    let _quiet = crate::test_util::quiet_logs();
    let (_dir, path) = scratch_db();
    let store = Arc::new(SqliteStore::connect(&path).unwrap());
    store.init_schema().await.unwrap();
    let session = "sqlite-image-async";

    let mem = open(store.clone(), session).await;
    derive_noise(&mem).await;
    let png = png_with_label(QUERY);
    let agent = crate::types::AgentId::from("agent-a");
    let submitted = mem
        .derive_image_async_as(&agent, image(&png))
        .await
        .unwrap();
    let answer = mem
        .pipeline()
        .wait(&agent, submitted.receipt, crate::writeq::RECEIPT_WAIT_MAX)
        .await;
    let crate::writeq::ReceiptAnswer::Applied(summary) = answer else {
        panic!("applied: {answer:?}");
    };
    let image_id: NodeId = NodeId(summary.created[0].parse().unwrap());
    assert_recalled_by_the_vector_leg(&mem, image_id).await;
    mem.close().await.unwrap();

    let reopened = open(store.clone(), session).await;
    assert_recalled_by_the_vector_leg(&reopened, image_id).await;
    reopened.close().await.unwrap();
}

// -- #22 PR 6: recall by image or by a client vector -----------------------

/// The Dresscode path on SQLite (design 12, PR 6): a photo of a similar
/// outfit, and the dismissed look's vector as a client sends it, each find
/// the look dismissed for Onam first, with no text, through the session
/// holder (`Memory::recall_by`) and through the lease-free reader `lambo
/// recall --image | --query-vector-json` (whose vector leg is SQLite's own
/// checked scan), before and after a reload.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recall_by_image_and_by_vector_find_the_dismissed_look() {
    use crate::recall::query_vector::QueryBy;
    use crate::test_util::dresscode::{
        assert_dismissed_is_the_top_vector_hit, client_query_vector, derive_wardrobe,
        imageless_text, similar_query_png,
    };

    let _quiet = crate::test_util::quiet_logs();
    let (dir, path) = scratch_db();
    let store = Arc::new(SqliteStore::connect(&path).unwrap());
    store.init_schema().await.unwrap();
    let session = "sqlite-recall-by";

    let mem = open(store.clone(), session).await;
    let wardrobe = derive_wardrobe(&mem).await;
    mem.settle_daemon().await;
    let png = similar_query_png();
    let image =
        || QueryBy::Image(crate::surface::image::validate(&png, "image/png").expect("valid png"));
    let vector = || QueryBy::Vector {
        values: client_query_vector(),
        declared: contract(),
    };
    for by in [image(), vector()] {
        let detailed = mem.recall_by_detailed(imageless_text(5), by).await.unwrap();
        assert_dismissed_is_the_top_vector_hit(&detailed, &wardrobe);
    }
    mem.close().await.unwrap();

    let reopened = open(store.clone(), session).await;
    reopened.settle_daemon().await;
    for by in [image(), vector()] {
        let detailed = reopened
            .recall_by_detailed(imageless_text(5), by)
            .await
            .unwrap();
        assert_dismissed_is_the_top_vector_hit(&detailed, &wardrobe);
    }
    reopened.close().await.unwrap();

    // The CLI reader, over the flushed store.
    let backends = crate::resolve::ResolvedBackends {
        store: Box::new(SqliteStore::connect(&path).unwrap()),
        embedder: Box::new(FixtureEmbedder::new()),
        store_cfg: crate::store::StoreConfig {
            kind: crate::store::StoreKind::Sqlite,
            dsn: None,
            path: Some(path.clone()),
            vector_dim: None,
        },
        embedder_cfg: crate::embed::EmbedderConfig {
            kind: crate::embed::EmbedderKind::Fixture,
            dim: 1024,
            accept_client_vectors: true,
            ..Default::default()
        },
        embedding: contract(),
        allow_embedding_mismatch: false,
        config: crate::Config {
            accept_client_vectors: true,
            ..crate::Config::default()
        },
    };
    let png_path = dir.join("similar.png");
    std::fs::write(&png_path, &png).unwrap();
    let vector_path = dir.join("query.json");
    std::fs::write(
        &vector_path,
        serde_json::json!({"values": client_query_vector(), "contract": {"kind": "fixture", "dim": 1024}})
            .to_string(),
    )
    .unwrap();
    for by in [
        crate::cli::recall::RecallBy {
            image: Some(png_path),
            ..Default::default()
        },
        crate::cli::recall::RecallBy {
            query_vector_json: Some(vector_path),
            ..Default::default()
        },
    ] {
        let out = crate::cli::recall::run_by(&backends, session, "", &by, Some(3), None, Some(0))
            .await
            .unwrap();
        let first = out
            .lines()
            .find(|l| l.contains("[image:"))
            .unwrap_or_else(|| panic!("an image hit: {out}"));
        assert!(
            first.contains("look dismissed for Onam [image:look2]"),
            "the dismissed look is the first image block: {out}"
        );
    }
}

/// Graded similarity on SQLite (M2): the holder ranks looks at cosines 0.8,
/// 0.5 and 0.3 to a client vector in that order, ahead of two unrelated
/// looks derived last (no recent leg without text), and so does the
/// lease-free reader over SQLite's own checked scan.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graded_similarity_ranks_by_cosine_not_recency_on_sqlite() {
    use crate::recall::query_vector::QueryBy;
    use crate::test_util::dresscode::{assert_graded_order, derive_graded_looks, imageless_text};

    let _quiet = crate::test_util::quiet_logs();
    let (_dir, path) = scratch_db();
    let store = Arc::new(SqliteStore::connect(&path).unwrap());
    store.init_schema().await.unwrap();
    let session = "sqlite-recall-by-graded";
    let mem = open_graded(store.clone(), session).await;
    let looks = derive_graded_looks(&mem).await;
    mem.settle_daemon().await;
    let detailed = mem
        .recall_by_detailed(
            imageless_text(5),
            QueryBy::Vector {
                values: looks.query.clone(),
                declared: contract(),
            },
        )
        .await
        .unwrap();
    assert_graded_order(&detailed, &looks);
    mem.close().await.unwrap();

    // Reopened: the vector leg is SQLite's checked scan of the reloaded
    // vectors, the daemon's scores rebuilt from the store.
    let reopened = open_graded(store.clone(), session).await;
    reopened.settle_daemon().await;
    let detailed = reopened
        .recall_by_detailed(
            imageless_text(5),
            QueryBy::Vector {
                values: looks.query.clone(),
                declared: contract(),
            },
        )
        .await
        .unwrap();
    assert_graded_order(&detailed, &looks);
    reopened.close().await.unwrap();
}

// -- #79: assembly against a frozen pre-derive table ------------------------

/// Build a unit vector at a chosen cosine to the fixture query direction.
fn cold_vector(probe: &[f32], orth_seed: &[f32], cosine: f32) -> Vec<f32> {
    let dot: f32 = probe.iter().zip(orth_seed).map(|(a, b)| a * b).sum();
    let mut orth: Vec<f32> = orth_seed
        .iter()
        .zip(probe)
        .map(|(n, q)| n - dot * q)
        .collect();
    let norm = orth.iter().map(|x| x * x).sum::<f32>().sqrt();
    for x in &mut orth {
        *x /= norm;
    }
    let sine = (1.0 - cosine * cosine).sqrt();
    probe
        .iter()
        .zip(orth)
        .map(|(q, n)| cosine * q + sine * n)
        .collect()
}

async fn cold_image(mem: &Memory, caption: &str, image_id: &str, values: Vec<f32>) -> NodeId {
    let agent = crate::types::AgentId::from("agent-a");
    let out = mem
        .derive_image_as(
            &agent,
            ImageDerive {
                caption,
                concept_type: ConceptType::Resource,
                image_id: Some(image_id),
                payload: ImagePayload::Vector {
                    values,
                    declared: contract(),
                },
                parent_of: &[],
                event_time: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(out.created.len(), 1);
    out.created[0]
}

/// SQLite's checked vector leg and the fixture embedder feed graded image
/// similarities to assembly. The score table is frozen before the two fresh
/// derives; no daemon cycle is part of this read, even if the background
/// worker subsequently wakes. Old daemon scores are planted at opposite
/// ends to enforce the low-daemon noise guard.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_supplied_image_ranks_by_query_until_daemon_scores_it() {
    use crate::config::{RecallWeights, ScoringWeights};
    use crate::daemon::score::rescore;
    use crate::graph::Graph;
    use crate::recall::assemble::{assemble, default_token_count};
    use crate::recall::expand::expand;
    use crate::types::{ScoreTable, Scored};
    use std::collections::HashMap;

    let _quiet = crate::test_util::quiet_logs();
    let (_dir, path) = scratch_db();
    let store = Arc::new(SqliteStore::connect(&path).unwrap());
    store.init_schema().await.unwrap();
    let session = "sqlite-cold-image";
    let probe = FixtureEmbedder::new().embed_sync("cold image probe");
    let orth = FixtureEmbedder::new().embed_sync("cold image orthogonal");
    let mem = open(store.clone(), session).await;
    let old_relevant = cold_image(
        &mem,
        "established relevant",
        "oldrelevant",
        cold_vector(&probe, &orth, 0.75),
    )
    .await;
    let old_noise = cold_image(
        &mem,
        "old irrelevant",
        "oldnoise",
        cold_vector(&probe, &orth, 0.61),
    )
    .await;
    let cold_scores = ScoreTable {
        epoch: mem.graph().read().epoch(),
        ranked: vec![
            Scored::new(old_relevant, 0.20),
            Scored::new(old_noise, 0.90),
        ],
    };
    let fresh_relevant = cold_image(
        &mem,
        "fresh relevant",
        "freshrelevant",
        cold_vector(&probe, &orth, 0.85),
    )
    .await;
    let fresh_noise = cold_image(
        &mem,
        "fresh irrelevant",
        "freshnoise",
        cold_vector(&probe, &orth, 0.68),
    )
    .await;
    mem.close().await.unwrap();

    let snapshot = store.load_session(&SessionId::from(session)).await.unwrap();
    let graph = Graph::from_snapshot(snapshot).unwrap();
    let phase1 = store
        .vector_candidates_checked(&SessionId::from(session), &probe, &contract(), 4)
        .await
        .unwrap();
    assert_eq!(phase1.len(), 4);
    let by_id: HashMap<_, _> = phase1.iter().map(|hit| (hit.item, hit.score)).collect();
    for (id, expected) in [
        (fresh_relevant, 0.85),
        (old_relevant, 0.75),
        (fresh_noise, 0.68),
        (old_noise, 0.61),
    ] {
        assert!(
            (by_id[&id] - expected).abs() < 1e-5,
            "{id}: graded SQLite cosine"
        );
    }
    let expanded = expand(&graph, phase1.clone(), 0);
    let query = RecallQuery {
        query: String::new(), // by-vector reads skip the recent leg
        top_k: 4,
        max_tokens: 10_000,
        traversal_depth: 0,
    };
    let run = |scores: &ScoreTable| {
        assemble(
            &graph,
            &expanded,
            &phase1,
            scores,
            &HashMap::new(),
            &query,
            RecallWeights::default(),
            Utc::now(),
            default_token_count,
        )
    };
    let cold = run(&cold_scores);
    assert_eq!(
        cold.hits.iter().map(|hit| hit.node_id).collect::<Vec<_>>(),
        vec![fresh_relevant, old_relevant, fresh_noise, old_noise]
    );
    // The old blend ranks old noise first and fresh relevant below it.
    assert!(0.5 * (0.90 + by_id[&old_noise]) > 0.5 * by_id[&fresh_relevant]);

    let settled_scores = ScoreTable {
        epoch: graph.epoch(),
        ranked: rescore(&graph, &ScoringWeights::default()),
    };
    let settled = run(&settled_scores);
    let daemon: HashMap<_, _> = settled_scores
        .ranked
        .iter()
        .map(|hit| (hit.item, hit.score))
        .collect();
    for hit in &settled.hits {
        let expected = 0.5 * (daemon[&hit.node_id] + by_id[&hit.node_id]);
        assert!((hit.score - expected).abs() < 1e-9);
    }
}

/// The text path uses SQLite-backed derives and the real keyword leg. A high
/// old daemon score would bury the just-derived exact query under the old
/// blend; in cold mode both candidates use the query share of the custom
/// weights. This catches a fix restricted to the image/vector path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_text_uses_query_order_on_sqlite() {
    use crate::config::RecallWeights;
    use crate::graph::index::InvertedIndex;
    use crate::graph::Graph;
    use crate::recall::assemble::{assemble, default_token_count};
    use crate::recall::candidates::{candidates, Phase1Input};
    use crate::recall::expand::expand;
    use crate::types::{ScoreTable, Scored};
    use std::collections::HashMap;

    let _quiet = crate::test_util::quiet_logs();
    let (_dir, path) = scratch_db();
    let store = Arc::new(SqliteStore::connect(&path).unwrap());
    store.init_schema().await.unwrap();
    let session = "sqlite-cold-text";
    let mem = open(store.clone(), session).await;
    let old = mem
        .derive(
            &[("cold start", ConceptType::Entity)],
            &crate::graph::derive::ParentOf::none(),
        )
        .await
        .unwrap()
        .created[0];
    let scores = ScoreTable {
        epoch: mem.graph().read().epoch(),
        ranked: vec![Scored::new(old, 1.40)],
    };
    let fresh = mem
        .derive(
            &[("cold start result", ConceptType::Entity)],
            &crate::graph::derive::ParentOf::none(),
        )
        .await
        .unwrap()
        .created[0];
    mem.close().await.unwrap();

    let snapshot = store.load_session(&SessionId::from(session)).await.unwrap();
    let index = InvertedIndex::from_snapshot(&snapshot);
    let graph = Graph::from_snapshot(snapshot).unwrap();
    let query = RecallQuery {
        query: "cold start result".into(),
        top_k: 2,
        max_tokens: 10_000,
        traversal_depth: 0,
    };
    let phase1 = candidates(
        &graph,
        &index,
        Phase1Input::default(),
        &query.query,
        query.top_k,
    );
    let relevance: HashMap<_, _> = phase1.iter().map(|hit| (hit.item, hit.score)).collect();
    assert!(relevance[&fresh] > relevance[&old]);
    assert!(
        0.9 * 1.40 + 0.1 * relevance[&old] > 0.1 * relevance[&fresh],
        "the old blend must lose the cold-start test"
    );
    let expanded = expand(&graph, phase1.clone(), 0);
    let result = assemble(
        &graph,
        &expanded,
        &phase1,
        &scores,
        &HashMap::new(),
        &query,
        RecallWeights {
            w_daemon: 0.9,
            w_query: 0.1,
        },
        Utc::now(),
        default_token_count,
    );
    assert_eq!(result.hits.first().map(|hit| hit.node_id), Some(fresh));
    assert!((result.hits[0].score - 0.1 * relevance[&fresh]).abs() < 1e-9);
}
