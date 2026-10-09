//! #22 PR 6: `Memory::recall_by` — recall by an image or a client query
//! vector, on both of a holder's vector sources (#8's graph and the store's
//! checked read), the Dresscode "close to the one you dismissed" path.

use super::writes::{ContextTolerantEmbedder, VectorSearchStore};
use super::*;
use crate::recall::query_vector::QueryBy;
use crate::test_util::dresscode::{
    assert_dismissed_is_the_top_vector_hit, assert_graded_order, client_query_vector,
    derive_graded_looks, derive_wardrobe, imageless_text, similar_query_png, DISMISSED_LABEL,
};

#[derive(Clone, Copy, Debug)]
enum Source {
    Store,
    Graph,
}

const SOURCES: [Source; 2] = [Source::Store, Source::Graph];

fn vector_store(source: Source) -> Arc<VectorSearchStore> {
    let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    Arc::new(match source {
        Source::Store => VectorSearchStore::new(inner),
        Source::Graph => VectorSearchStore::graph_ranked(inner),
    })
}

async fn open(store: Arc<VectorSearchStore>, session: &str) -> Memory {
    Memory::builder()
        .session(session)
        .agent("agent-a")
        .flush_interval(Duration::from_millis(10))
        .match_strategy(MatchStrategy::Hybrid)
        .store(store as Arc<dyn GraphStore>)
        .embedder(Arc::new(ContextTolerantEmbedder(FixtureEmbedder::new())) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .build()
        .await
        .expect("build")
}

/// The store-backed source reads what was flushed, so wait for the flush
/// before ranking (the graph-backed source needs nothing).
async fn flushed(mem: &Memory) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let stats = mem.stats();
            if stats.log_depth == 0 && stats.flush_depth == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("flushed");
}

fn image_query(png: &[u8]) -> QueryBy<'_> {
    QueryBy::Image(crate::surface::image::validate(png, "image/png").expect("valid png"))
}

fn vector_query(values: Vec<f32>) -> QueryBy<'static> {
    QueryBy::Vector {
        values,
        declared: contract("fixture", 1024),
    }
}

/// The acceptance path: a similar photo, and the same look as a client
/// vector, each find the look dismissed for Onam first on the vector leg
/// and overall, with no text at all; and neither cache holds anything
/// afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_similar_image_and_a_client_vector_find_the_dismissed_look() {
    for source in SOURCES {
        let store = vector_store(source);
        let mem = open(store.clone(), "q22-recall-by").await;
        let wardrobe = derive_wardrobe(&mem).await;
        flushed(&mem).await;
        mem.settle_daemon().await;
        let answers_before = store.answers().len();

        let png = similar_query_png();
        let by_image = mem
            .recall_by_detailed(imageless_text(5), image_query(&png))
            .await
            .unwrap();
        assert_dismissed_is_the_top_vector_hit(&by_image, &wardrobe);

        let by_vector = mem
            .recall_by_detailed(imageless_text(5), vector_query(client_query_vector()))
            .await
            .unwrap();
        assert_dismissed_is_the_top_vector_hit(&by_vector, &wardrobe);

        // Which source ranked: the store's checked read only on the store
        // source; the holder's own graph otherwise (#8).
        let store_reads = store.answers().len() - answers_before;
        match source {
            Source::Store => assert_eq!(store_reads, 2, "{source:?}"),
            Source::Graph => assert_eq!(store_reads, 0, "{source:?}"),
        }

        // Not cached (design 7.2): the query-embedding cache holds nothing,
        // and the session's pipeline cache was never handed over.
        assert!(mem.query_embeddings.lock().is_empty(), "{source:?}");
        assert!(mem.recall_cache.lock().await.is_empty(), "{source:?}");

        // The text path still works beside it, and the dismissed look is
        // what a text query for its label finds too (design 7.1).
        let by_text = mem
            .recall_detailed(RecallQuery {
                query: DISMISSED_LABEL.into(),
                top_k: 5,
                max_tokens: 2_000,
                traversal_depth: 1,
            })
            .await
            .unwrap();
        assert_eq!(
            by_text.hits.first().map(|h| h.node_id),
            Some(wardrobe.dismissed)
        );
        assert_eq!(mem.query_embeddings.lock().len(), 1, "text is cached");
        mem.close().await.unwrap();
    }
}

/// Text beside an image feeds the keyword leg; the image still drives the
/// vector leg, and a structural phrasing is not dispatched to traversal
/// (which would drop the image).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_beside_an_image_feeds_the_keyword_leg_and_never_dispatches() {
    let mem = open(vector_store(Source::Graph), "q22-recall-by-text").await;
    let wardrobe = derive_wardrobe(&mem).await;
    mem.settle_daemon().await;
    let png = similar_query_png();
    let mut q = imageless_text(5);
    q.query = "what depends on look dismissed for Onam".into();
    let detailed = mem.recall_by_detailed(q, image_query(&png)).await.unwrap();
    assert!(
        detailed
            .response_annotations
            .iter()
            .all(|a| a.kind != crate::recall::detail::AnnotationKind::Traversal),
        "not dispatched to traversal"
    );
    let legs = detailed.legs.get(&wardrobe.dismissed).expect("legs");
    assert!(legs.vector.is_some_and(|v| v > 0.99), "{legs:?}");
    assert!(
        legs.keyword.is_some(),
        "the caption's words matched: {legs:?}"
    );
    mem.close().await.unwrap();
}

/// A declared contract other than the live one, a wrong width, a
/// non-finite or zero vector are refused before anything runs; so is a
/// recall by vector on a store without vector search, and on a closed
/// session.
#[tokio::test]
async fn bad_query_vectors_and_stores_without_vector_search_are_refused() {
    let mem = open(vector_store(Source::Graph), "q22-recall-by-refuse").await;
    let mut other = contract("fixture", 1024);
    other.model = Some("another-model".into());
    let unit = FixtureEmbedder::new().embed_sync(DISMISSED_LABEL);
    for by in [
        QueryBy::Vector {
            values: unit.clone(),
            declared: other,
        },
        vector_query(unit[..512].to_vec()),
        vector_query(vec![0.0; 1024]),
        vector_query(vec![f32::INFINITY; 1024]),
    ] {
        let err = mem
            .recall_by_detailed(imageless_text(5), by)
            .await
            .unwrap_err();
        assert!(matches!(err, LamboError::Config(_)), "{err:?}");
    }
    mem.close().await.unwrap();
    let err = mem
        .recall_by_detailed(imageless_text(5), vector_query(unit.clone()))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("closed"), "closed: {err:?}");

    // A keyword-only store: the vector leg cannot run, so a recall by
    // vector has nothing to answer with.
    let plain = memory_on(Arc::new(MemoryStore::new()), "q22-recall-by-plain").await;
    let err = plain
        .recall_by_detailed(imageless_text(5), vector_query(unit))
        .await
        .unwrap_err();
    let LamboError::Config(msg) = err else {
        panic!("config error: {err:?}")
    };
    assert!(msg.contains("VECTOR_SEARCH"), "{msg}");
    plain.close().await.unwrap();
}

/// M1: a recall by image or vector whose vector read fails is an error,
/// not the recent leg's answer. With no text the keyword leg finds nothing,
/// so degrading would return whatever was derived last as "close to this
/// image". The error is `LamboError::Store`, which the MCP surface renders
/// as a bare class (no backend detail); the image path and the vector path
/// fail alike.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_vector_read_fails_a_recall_by_image_or_vector() {
    let store = vector_store(Source::Store);
    let mem = open(store.clone(), "q22-recall-by-read-fails").await;
    derive_wardrobe(&mem).await;
    flushed(&mem).await;
    mem.settle_daemon().await;
    store.fail_vector_reads();

    let png = similar_query_png();
    for (by, text) in [
        (image_query(&png), ""),
        (vector_query(client_query_vector()), ""),
        (image_query(&png), "look dismissed for Onam"),
    ] {
        let mut q = imageless_text(5);
        q.query = text.into();
        let err = mem.recall_by_detailed(q, by).await.unwrap_err();
        assert!(
            matches!(err, LamboError::Store(StoreError::Backend(_))),
            "{err:?}"
        );
        let shown = crate::surface::error::model_safe_message(&err);
        assert_eq!(shown, "store error (the detail was logged server-side)");
    }
    mem.close().await.unwrap();
}

/// The text recall's twin of M1 (pre-existing): a failed vector read still
/// degrades a text recall to its keyword and recent legs, but no longer
/// silently. The result carries a `vector_degraded` annotation and the same
/// line as a warning (what `Memory::recall` and `lambo_recall` hand the
/// caller), and the line names no backend detail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_vector_read_on_a_text_recall_says_the_leg_was_skipped() {
    let store = vector_store(Source::Store);
    let mem = open(store.clone(), "q22-recall-text-read-fails").await;
    let wardrobe = derive_wardrobe(&mem).await;
    flushed(&mem).await;
    mem.settle_daemon().await;
    store.fail_vector_reads();

    let detailed = mem
        .recall_detailed(RecallQuery {
            query: "look dismissed for Onam".into(),
            top_k: 5,
            max_tokens: 2_000,
            traversal_depth: 1,
        })
        .await
        .expect("a text recall degrades, it does not fail");
    assert!(
        detailed
            .hits
            .iter()
            .any(|h| h.node_id == wardrobe.dismissed),
        "the keyword leg still answers: {:?}",
        detailed.hits
    );
    assert!(
        detailed.legs.values().all(|l| l.vector.is_none()),
        "no vector leg: {:?}",
        detailed.legs
    );
    let degraded: Vec<_> = detailed
        .response_annotations
        .iter()
        .filter(|a| a.kind == crate::recall::detail::AnnotationKind::VectorDegraded)
        .collect();
    assert_eq!(degraded.len(), 1, "{:?}", detailed.response_annotations);
    assert!(
        degraded[0].text.contains("vector leg skipped"),
        "{degraded:?}"
    );
    assert!(
        detailed.warnings.contains(&degraded[0].text),
        "the caller's warnings carry it: {:?}",
        detailed.warnings
    );
    assert!(
        detailed.warnings.iter().all(|w| !w.contains("db.internal")),
        "no backend detail: {:?}",
        detailed.warnings
    );
    let projected: crate::types::RecallResult = detailed.into();
    assert!(projected
        .warnings
        .iter()
        .any(|w| w.contains("vector leg skipped")));
    mem.close().await.unwrap();
}

/// Graded similarity (M2): looks at cosines 0.8, 0.5 and 0.3 to a client
/// query vector rank in that order on the vector leg (each at its cosine)
/// and overall, ahead of two unrelated looks derived last. With the recent
/// leg running, those two would join at `RECENT_SCORE` (0.35) and outrank
/// the 0.3 look; with no text the recent leg is skipped. On both holder
/// sources. With text beside the vector, the recent leg runs as in a text
/// recall.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graded_similarity_ranks_by_cosine_not_recency() {
    for source in SOURCES {
        let store = vector_store(source);
        let mem = open(store.clone(), "q22-recall-by-graded").await;
        let looks = derive_graded_looks(&mem).await;
        flushed(&mem).await;
        mem.settle_daemon().await;
        let detailed = mem
            .recall_by_detailed(imageless_text(5), vector_query(looks.query.clone()))
            .await
            .unwrap();
        assert_graded_order(&detailed, &looks);

        let mut with_text = imageless_text(5);
        with_text.query = "unrelated look".into();
        let detailed = mem
            .recall_by_detailed(with_text, vector_query(looks.query.clone()))
            .await
            .unwrap();
        assert!(
            looks
                .unrelated
                .iter()
                .all(|id| detailed.legs.get(id).is_some_and(|l| l.recent.is_some())),
            "{source:?}: beside text the recent leg runs: {:?}",
            detailed.legs
        );
        mem.close().await.unwrap();
    }
}
