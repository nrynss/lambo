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
/// The final score mixes the query legs with the daemon's structural score
/// table (`RecallWeights`, 0.5 each), and a concept derived moments ago is
/// not in that table until the daemon's next cycle, so the rank of any fresh
/// concept, text or image, races the daemon. The check therefore waits for
/// the daemon to score the current epoch first (`Memory::settle_daemon`),
/// which makes "the top hit" deterministic. Before that cycle the image
/// scores 0.5 (a perfect vector match, no daemon score) against older noise
/// at about 0.533: a cold-start ranking question recorded for PR 5's parity
/// measurement (design 7.3), not a defect of this PR.
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
