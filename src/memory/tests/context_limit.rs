//! #74: a hybrid derive whose embedding context would exceed
//! `MAX_HYBRID_CONTEXT_BYTES` at apply is refused when it is called, on the
//! synchronous and the acknowledged path, for a concept, a `parent_of` end
//! and an image caption with a new end; and only where the derive embeds.

use super::replay::VectorSearchable;
use super::*;
use crate::embed::png_with_label;
use crate::graph::hybrid::{MAX_HYBRID_CONTEXT_BYTES, MAX_SINGLE_CONCEPT_CONTEXT_BYTES};
use crate::graph::image::{ImageDerive, ImagePayload};
use crate::writeq::{ReceiptAnswer, RECEIPT_WAIT_MAX};

async fn memory(store: Arc<dyn GraphStore>, session: &str) -> Memory {
    Memory::builder()
        .session(session)
        .agent("agent-a")
        .flush_interval(Duration::from_secs(3_600))
        .store(store)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .match_strategy(MatchStrategy::Hybrid)
        .build()
        .await
        .expect("build")
}

async fn vector_memory(session: &str) -> Memory {
    memory(
        Arc::new(VectorSearchable(Arc::new(MemoryStore::new()))),
        session,
    )
    .await
}

fn agent() -> AgentId {
    AgentId::from("agent-a")
}

/// A refusal at call time: a `Config` error naming the hybrid limit, with
/// nothing written (not even the interaction).
fn assert_refused<T: std::fmt::Debug>(mem: &Memory, before: usize, out: Result<T, LamboError>) {
    match out {
        Err(LamboError::Config(msg)) => {
            assert!(msg.contains("at most 16384 bytes"), "{msg}");
        }
        other => panic!("refused at call time: {other:?}"),
    }
    assert_eq!(mem.stats().node_count, before, "nothing was written");
}

async fn applied(mem: &Memory, submitted: crate::writeq::Submitted) -> String {
    match mem
        .pipeline()
        .wait(&agent(), submitted.receipt, RECEIPT_WAIT_MAX)
        .await
    {
        ReceiptAnswer::Applied(summary) => summary.summary,
        other => panic!("applied: {other:?}"),
    }
}

#[tokio::test]
async fn a_lone_concept_is_refused_one_byte_over_its_framed_limit_on_both_paths() {
    let mem = vector_memory("ctx74-concept").await;
    let at = "a".repeat(MAX_SINGLE_CONCEPT_CONTEXT_BYTES);
    let over = "o".repeat(MAX_SINGLE_CONCEPT_CONTEXT_BYTES + 1);

    let out = mem
        .derive_as(
            &agent(),
            &[(at.as_str(), ConceptType::Entity)],
            &ParentOf::none(),
        )
        .await
        .expect("at the limit, the synchronous derive applies");
    assert_eq!((out.created.len(), out.embedded), (1, 1));
    let before = mem.stats().node_count;
    let out = mem
        .derive_as(
            &agent(),
            &[(over.as_str(), ConceptType::Entity)],
            &ParentOf::none(),
        )
        .await;
    assert_refused(&mem, before, out);

    let at = "b".repeat(MAX_SINGLE_CONCEPT_CONTEXT_BYTES);
    let submitted = mem
        .derive_async_as(
            &agent(),
            &[(at.as_str(), ConceptType::Entity)],
            &ParentOf::none(),
            None,
        )
        .await
        .expect("at the limit, the acknowledged derive is accepted");
    assert!(applied(&mem, submitted).await.contains("1 created"));
    let before = mem.stats().node_count;
    let out = mem
        .derive_async_as(
            &agent(),
            &[(over.as_str(), ConceptType::Entity)],
            &ParentOf::none(),
            None,
        )
        .await;
    assert_refused(&mem, before, out);
    mem.close().await.unwrap();
}

#[tokio::test]
async fn a_new_parent_of_end_is_refused_one_byte_over_on_both_paths() {
    let mem = vector_memory("ctx74-end").await;
    // end + " — " (5) + "c" (the call's text).
    let at_len = MAX_HYBRID_CONTEXT_BYTES - 6;
    for (path, end_char) in [("sync", 's'), ("async", 'a')] {
        let at = end_char.to_string().repeat(at_len);
        let over = end_char.to_string().repeat(at_len + 1);
        let at_pairs = [(at.as_str(), "c")];
        let over_pairs = [(over.as_str(), "c")];
        let concepts = [("c", ConceptType::Entity)];
        if path == "sync" {
            let out = mem
                .derive_as(&agent(), &concepts, &ParentOf::from_pairs(&at_pairs))
                .await
                .expect("at the limit, applied");
            assert_eq!(out.embedded, 2, "the concept and its new end");
            let before = mem.stats().node_count;
            let out = mem
                .derive_as(&agent(), &concepts, &ParentOf::from_pairs(&over_pairs))
                .await;
            assert_refused(&mem, before, out);
        } else {
            let submitted = mem
                .derive_async_as(&agent(), &concepts, &ParentOf::from_pairs(&at_pairs), None)
                .await
                .expect("at the limit, accepted");
            let summary = applied(&mem, submitted).await;
            assert!(summary.contains("1 created (1 embedded)"), "{summary}");
            let before = mem.stats().node_count;
            let out = mem
                .derive_async_as(
                    &agent(),
                    &concepts,
                    &ParentOf::from_pairs(&over_pairs),
                    None,
                )
                .await;
            assert_refused(&mem, before, out);
        }
    }
    mem.close().await.unwrap();
}

fn image<'a>(
    caption: &'a str,
    id: Option<&'a str>,
    png: &'a [u8],
    pairs: &'a [(&'a str, &'a str)],
) -> ImageDerive<'a> {
    ImageDerive {
        caption,
        concept_type: ConceptType::Resource,
        image_id: id,
        payload: ImagePayload::Bytes(
            crate::surface::image::validate(png, "image/png").expect("valid png"),
        ),
        parent_of: pairs,
        event_time: None,
    }
}

/// The image's own text is never embedded, but a new end is framed with the
/// whole image content (caption plus suffix).
#[tokio::test]
async fn an_image_caption_with_a_new_end_is_refused_one_byte_over_on_both_paths() {
    let mem = vector_memory("ctx74-image").await;
    let png = png_with_label("red silk saree");
    // "wardrobe" (8) + " — " (5) + caption + " [image:r17]" (12).
    let at_len = MAX_HYBRID_CONTEXT_BYTES - 8 - 5 - 12;
    let pairs = [("wardrobe", "shelf")];

    let at = "s".repeat(at_len);
    mem.derive_image_as(&agent(), image(&at, Some("r17"), &png, &pairs))
        .await
        .expect("at the limit, applied");
    let over = "s".repeat(at_len + 1);
    let before = mem.stats().node_count;
    let out = mem
        .derive_image_as(&agent(), image(&over, Some("r18"), &png, &pairs))
        .await;
    assert_refused(&mem, before, out);

    let pairs = [("closet", "rack")];
    // "closet" (6) + 5 + caption + " [image:" + 16-char default id + "]" (25).
    let at_len = MAX_HYBRID_CONTEXT_BYTES - 6 - 5 - 25;
    let at = "a".repeat(at_len);
    let submitted = mem
        .derive_image_async_as(&agent(), image(&at, None, &png, &pairs))
        .await
        .expect("at the limit, accepted");
    let summary = applied(&mem, submitted).await;
    assert!(summary.contains("3 created"), "{summary}");
    let over = "a".repeat(at_len + 1);
    let before = mem.stats().node_count;
    let out = mem
        .derive_image_async_as(&agent(), image(&over, None, &png, &pairs))
        .await;
    assert_refused(&mem, before, out);
    mem.close().await.unwrap();
}

/// The exemptions #22 PR 4 set stay: on a store without vector search
/// nothing is embedded, so nothing is refused for its length, on either path.
#[tokio::test]
async fn a_store_without_vector_search_takes_an_over_long_concept() {
    let mem = memory(Arc::new(MemoryStore::new()), "ctx74-novector").await;
    let over = "o".repeat(MAX_SINGLE_CONCEPT_CONTEXT_BYTES + 1);
    let out = mem
        .derive_as(
            &agent(),
            &[(over.as_str(), ConceptType::Entity)],
            &ParentOf::none(),
        )
        .await
        .expect("nothing embeds, nothing is refused");
    assert_eq!((out.created.len(), out.embedded), (1, 0));
    let other = "p".repeat(MAX_SINGLE_CONCEPT_CONTEXT_BYTES + 1);
    let submitted = mem
        .derive_async_as(
            &agent(),
            &[(other.as_str(), ConceptType::Entity)],
            &ParentOf::none(),
            None,
        )
        .await
        .expect("accepted");
    assert!(applied(&mem, submitted).await.contains("1 created"));
    mem.close().await.unwrap();
}
