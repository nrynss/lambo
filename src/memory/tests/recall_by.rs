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

/// Review L5: the E2E-6 contract-race annotation says the results are
/// "keyword-only", which is true of a text recall and was false of a recall
/// by image with no text (its answer was the recent leg). A recall by image
/// or vector never carries it: the race fails the recall (M1), and on the
/// wire it says to re-check the contract and retry (review Low 1), with no
/// detail from the refusal. A text recall still carries the pinned line.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_contract_race_fails_a_recall_by_image_and_annotates_only_text() {
    let store = vector_store(Source::Store);
    let mem = open(store.clone(), "q22-recall-by-contract-race").await;
    derive_wardrobe(&mem).await;
    flushed(&mem).await;
    mem.settle_daemon().await;
    store.race_the_contract();

    let png = similar_query_png();
    let err = mem
        .recall_by_detailed(imageless_text(5), image_query(&png))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, LamboError::Store(StoreError::Invariant(m)) if m.contains("embedding contract changed")),
        "{err:?}"
    );
    let shown = crate::surface::error::model_safe_message(&err);
    assert!(
        shown.starts_with("store error: ") && shown.contains("re-check") && shown.contains("retry"),
        "{shown}"
    );
    assert!(!shown.contains("q22-recall-by-contract-race"), "{shown}");
    assert_ne!(shown, "store error (the detail was logged server-side)");

    let text = mem
        .recall_detailed(RecallQuery {
            query: "look dismissed for Onam".into(),
            top_k: 5,
            max_tokens: 2_000,
            traversal_depth: 1,
        })
        .await
        .unwrap();
    assert!(
        text.warnings
            .iter()
            .any(|w| w.contains("embedding contract changed") && w.contains("keyword-only")),
        "{:?}",
        text.warnings
    );
    mem.close().await.unwrap();
}

// #79: keep the daemon's public score-table read handle across fresh derives.
// The daemon can wake, but cannot publish its new table until the assertions
// finish. This tests the actual Memory route without a scheduler race.
#[cfg(feature = "store-sqlite")]
fn cold_vector(probe: &[f32], seed: &[f32], cosine: f32) -> Vec<f32> {
    let dot: f32 = probe.iter().zip(seed).map(|(a, b)| a * b).sum();
    let mut orth: Vec<f32> = seed.iter().zip(probe).map(|(n, q)| n - dot * q).collect();
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

#[cfg(feature = "store-sqlite")]
async fn cold_sqlite_memory(session: &str) -> (crate::test_util::ScratchDir, Memory) {
    use crate::store::SqliteStore;
    let dir = crate::test_util::ScratchDir::new("lambo-79-cold");
    let path = dir.join("cold.db");
    let store = Arc::new(SqliteStore::connect(path.to_str().unwrap()).unwrap());
    store.init_schema().await.unwrap();
    let mem = Memory::builder()
        .session(session)
        .agent("agent-a")
        .flush_interval(Duration::from_millis(10))
        .match_strategy(MatchStrategy::Hybrid)
        .store(store as Arc<dyn GraphStore>)
        .embedder(Arc::new(ContextTolerantEmbedder(FixtureEmbedder::new())) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .build()
        .await
        .unwrap();
    (dir, mem)
}

#[cfg(feature = "store-sqlite")]
async fn cold_image(mem: &Memory, caption: &str, image_id: &str, values: Vec<f32>) -> NodeId {
    use crate::graph::image::{ImageDerive, ImagePayload};
    let out = mem
        .derive_image_as(
            &AgentId::from("agent-a"),
            ImageDerive {
                caption,
                concept_type: ConceptType::Resource,
                image_id: Some(image_id),
                payload: ImagePayload::Vector {
                    values,
                    declared: contract("fixture", 1024),
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

#[cfg(feature = "store-sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_public_recall_by_ranks_fresh_supplied_image_and_guards_noise() {
    let _quiet = crate::test_util::quiet_logs();
    let (_dir, mem) = cold_sqlite_memory("issue-79-public-image").await;
    let probe = FixtureEmbedder::new().embed_sync("cold image probe");
    let seed = FixtureEmbedder::new().embed_sync("cold image orthogonal");
    let old_relevant = cold_image(
        &mem,
        "old relevant",
        "oldrelevant",
        cold_vector(&probe, &seed, 0.75),
    )
    .await;
    let old_noise = cold_image(
        &mem,
        "old noise",
        "oldnoise",
        cold_vector(&probe, &seed, 0.61),
    )
    .await;
    // A recent, daemon-scored text concept: with text it would join the
    // recent leg at RECENT_SCORE; with no text that leg is skipped (#22 PR 6),
    // so cold mode must not let it in on the floor.
    let recent_text = mem
        .derive(
            &[("orbital neutrino mechanics", ConceptType::Entity)],
            &ParentOf::none(),
        )
        .await
        .unwrap()
        .created[0];
    flushed(&mem).await;
    mem.settle_daemon().await;
    stop_daemon_for_cold_start(&mem).await;
    let scores = mem.daemon.scores();
    assert!(scores.ranked.iter().any(|hit| hit.item == old_relevant));
    assert!(scores.ranked.iter().any(|hit| hit.item == old_noise));
    assert!(scores.ranked.iter().any(|hit| hit.item == recent_text));
    let fresh_relevant = cold_image(
        &mem,
        "fresh relevant",
        "freshrelevant",
        cold_vector(&probe, &seed, 0.85),
    )
    .await;
    let fresh_noise = cold_image(
        &mem,
        "fresh noise",
        "freshnoise",
        cold_vector(&probe, &seed, 0.68),
    )
    .await;
    flushed(&mem).await;
    assert!(scores
        .ranked
        .iter()
        .all(|hit| hit.item != fresh_relevant && hit.item != fresh_noise));
    let query = RecallQuery {
        query: String::new(),
        top_k: 4,
        max_tokens: 10_000,
        traversal_depth: 0,
    };
    let result = mem
        .recall_by(query.clone(), vector_query(probe.clone()))
        .await
        .unwrap();
    assert_eq!(
        result
            .hits
            .iter()
            .map(|hit| hit.node_id)
            .collect::<Vec<_>>(),
        vec![fresh_relevant, old_relevant, fresh_noise, old_noise]
    );
    let detailed = mem
        .recall_by_detailed(query, vector_query(probe))
        .await
        .unwrap();
    assert!(detailed.legs.values().all(|leg| leg.recent.is_none()));
    // Cold mode on the vector leg alone: every hit is 0.5 × its cosine, and
    // the recent text concept gains nothing from recency or its daemon score.
    for hit in &detailed.hits {
        let v = detailed.legs.get(&hit.node_id).and_then(|l| l.vector);
        let expected = 0.5 * v.unwrap_or(0.0).max(0.0);
        assert!((hit.score - expected).abs() < 1e-6, "{hit:?} vs {v:?}");
    }
    assert!(
        detailed.hits.iter().all(|hit| hit.node_id != recent_text),
        "top_k 4 is the four images: {:?}",
        detailed.hits
    );
    mem.close().await.unwrap();
}

#[cfg(feature = "store-sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_public_text_recall_keeps_strong_fresh_vector_above_recent_noise() {
    let _quiet = crate::test_util::quiet_logs();
    let (_dir, mem) = cold_sqlite_memory("issue-79-public-text").await;
    mem.derive(
        &[("create account guidance", ConceptType::Entity)],
        &ParentOf::none(),
    )
    .await
    .unwrap();
    flushed(&mem).await;
    mem.settle_daemon().await;
    stop_daemon_for_cold_start(&mem).await;
    let scores = mem.daemon.scores();
    let fresh = mem
        .derive(&[("register user", ConceptType::Entity)], &ParentOf::none())
        .await
        .unwrap()
        .created[0];
    let recent = mem
        .derive(
            &[("orbital neutrino mechanics", ConceptType::Entity)],
            &ParentOf::none(),
        )
        .await
        .unwrap()
        .created[0];
    flushed(&mem).await;
    assert!(scores
        .ranked
        .iter()
        .all(|hit| hit.item != fresh && hit.item != recent));
    let query = RecallQuery {
        query: "create account".into(),
        top_k: 5,
        max_tokens: 10_000,
        traversal_depth: 0,
    };
    let detailed = mem.recall_detailed(query).await.unwrap();
    let fresh_leg = detailed.legs.get(&fresh).unwrap();
    let recent_leg = detailed.legs.get(&recent).unwrap();
    assert!(fresh_leg.vector.unwrap() > 0.90);
    assert!(recent_leg.recent.is_some());
    let ids: Vec<NodeId> = detailed.hits.iter().map(|hit| hit.node_id).collect();
    assert!(ids.iter().position(|id| *id == fresh) < ids.iter().position(|id| *id == recent));
    mem.close().await.unwrap();
}

/// #79 leg scale, text recall: in cold mode every member scores
/// `w_query × q`, where `q` is the max-merged phase-1 score. The recent
/// leg's flat `RECENT_SCORE` floor is on the same `q` scale as the vector
/// cosine and the keyword BM25, so cold mode orders the three legs exactly
/// as phase 1 does. The planted hazard is a recent-only concept: being
/// recent, it has a high daemon recency (d = 0.7 here), so under the
/// ordinary blend it would sit at `0.5 × d + 0.175 = 0.525`, above a fresh
/// unscored strong vector hit at `0.5 × 0.80 = 0.40`.
/// Cold mode must keep the fresh vector hit above it, and keyword hits in
/// BM25 order above the recency floor.
#[cfg(feature = "store-sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_cold_text_recall_orders_vector_keyword_and_recent_legs_on_one_scale() {
    use crate::recall::candidates::RECENT_SCORE;
    let _quiet = crate::test_util::quiet_logs();
    let (_dir, mem) = cold_sqlite_memory("issue-79-leg-scale").await;
    let text = "create account";
    let probe = ContextTolerantEmbedder(FixtureEmbedder::new())
        .embed_query(text)
        .await
        .unwrap();
    let seed = FixtureEmbedder::new().embed_sync("leg scale orthogonal");
    let derive_one = |content: &'static str| {
        let mem = &mem;
        async move {
            mem.derive(&[(content, ConceptType::Entity)], &ParentOf::none())
                .await
                .unwrap()
                .created[0]
        }
    };
    // Older, daemon-scored concepts: two keyword hits of different BM25,
    // a filler, then the planted recent-only concept.
    let strong_keyword = derive_one("create account guidance").await;
    let weak_keyword = derive_one("account settings page").await;
    derive_one("lunar tide tables").await;
    let planted = derive_one("orbital neutrino mechanics").await;
    flushed(&mem).await;
    mem.settle_daemon().await;
    stop_daemon_for_cold_start(&mem).await;
    let scores = mem.daemon.scores();
    let daemon = |id: NodeId| scores.ranked.iter().find(|s| s.item == id).map(|s| s.score);
    let planted_d = daemon(planted).expect("the planted concept is scored");

    // The fresh strong vector hit, after the table froze.
    let fresh = cold_image(
        &mem,
        "fresh vector match",
        "freshvector",
        cold_vector(&probe, &seed, 0.80),
    )
    .await;
    flushed(&mem).await;
    assert_eq!(daemon(fresh), None, "the fresh hit is not yet scored");
    // The hazard is real: under the ordinary blend the planted recent-only
    // hit would outrank the fresh strong vector hit.
    assert!(
        0.5 * planted_d + 0.5 * RECENT_SCORE > 0.5 * 0.80,
        "planted daemon score {planted_d} too low to test the hazard"
    );

    let query = RecallQuery {
        query: text.into(),
        top_k: 8,
        max_tokens: 10_000,
        traversal_depth: 0,
    };
    let detailed = mem.recall_detailed(query).await.unwrap();
    let leg = |id: NodeId| {
        *detailed
            .legs
            .get(&id)
            .unwrap_or_else(|| panic!("{id:?} in phase 1"))
    };
    let q = |l: crate::recall::candidates::LegScores| {
        [l.keyword, l.recent, l.vector]
            .into_iter()
            .flatten()
            .fold(f64::NEG_INFINITY, f64::max)
    };
    let pos = |id: NodeId| {
        detailed
            .hits
            .iter()
            .position(|h| h.node_id == id)
            .unwrap_or_else(|| panic!("{id:?} returned: {:?}", detailed.hits))
    };

    // Provenance: each leg is what the case says it is.
    assert!((leg(fresh).vector.unwrap() - 0.80).abs() < 1e-3);
    assert_eq!(leg(fresh).recent, Some(RECENT_SCORE), "fresh is recent too");
    assert!(leg(strong_keyword).keyword.unwrap() > leg(weak_keyword).keyword.unwrap());
    assert_eq!(leg(strong_keyword).recent, None);
    assert_eq!(leg(planted).keyword, None);
    assert_eq!(leg(planted).recent, Some(RECENT_SCORE));
    assert_eq!(
        q(leg(planted)),
        RECENT_SCORE,
        "planted is on the recency floor"
    );

    // Cold mode: every hit is w_query × q, the daemon share withheld even
    // for the high-daemon recent concept. The result says so (M2).
    assert!(detailed.cold_start, "the recall reports cold mode");
    for hit in &detailed.hits {
        let expected = detailed.legs.get(&hit.node_id).map_or(0.0, |l| 0.5 * q(*l));
        assert!(
            (hit.score - expected).abs() < 1e-9,
            "{}: {} != 0.5 × q {expected}",
            hit.content,
            hit.score
        );
    }
    // One scale: the fresh strong vector hit and both keyword hits outrank
    // the planted recent-only hit; keyword hits keep BM25 order.
    assert!(pos(fresh) < pos(planted));
    assert!(pos(strong_keyword) < pos(weak_keyword));
    assert!(pos(weak_keyword) < pos(planted));
    // And the whole list is in non-increasing q order.
    let qs: Vec<f64> = detailed
        .hits
        .iter()
        .map(|h| detailed.legs.get(&h.node_id).map_or(0.0, |l| q(*l)))
        .collect();
    assert!(qs.windows(2).all(|w| w[0] >= w[1]), "q order: {qs:?}");
    mem.close().await.unwrap();
}
