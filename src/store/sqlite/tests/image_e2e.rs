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

/// The image concept is recalled for the text query, found by the vector
/// leg (at the label's own vector) and not by the keyword leg, and no other
/// candidate's query evidence comes near it.
///
/// Why not "the top hit" (design section 9 step 3): the final score mixes
/// the query legs with the daemon's structural score table (`RecallWeights`,
/// 0.5 each), and a concept derived moments ago is not in that table until
/// the daemon's next cycle, so the final rank of any fresh concept, text or
/// image, races the daemon. Which leg found it, and how strongly, does not.
async fn assert_recalled_by_the_vector_leg(mem: &Memory, image: NodeId) {
    let detailed = mem
        .recall_detailed(RecallQuery {
            query: QUERY.into(),
            top_k: 5,
            max_tokens: 1_000,
            traversal_depth: 1,
        })
        .await
        .unwrap();
    assert!(
        detailed.hits.iter().any(|h| h.node_id == image),
        "the image concept is recalled: {:?}",
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
