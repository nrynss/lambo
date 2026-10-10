//! #79 cold start against a live EmbeddingGemma 2 `llama-server`, through
//! the real cold path (review M3).
//!
//! Ignored by default and skipped unless `LAMBO_EG2_URL` is set. It lives
//! in the crate, not in `tests/live_eg2.rs`, because holding the daemon
//! back needs the crate-private `stop_daemon_for_cold_start`; a public
//! `Memory` rescores within one daemon cycle of a derive. Run it with the
//! other live EG2 tests (server flags in `tests/live_eg2.rs`):
//!
//! ```text
//! LAMBO_EG2_URL=http://127.0.0.1:8191 cargo test --features embed-eg2,store-sqlite \
//!   --lib --test live_eg2 -- --ignored --nocapture live_eg2
//! ```

use super::*;
use crate::embed::{cosine, Eg2ServerCheck, EmbeddingGemma2Embedder, EG2_DEFAULT_MODEL};
use crate::graph::image::{ImageDerive, ImagePayload};
use crate::recall::query_vector::QueryBy;

/// A `side` x `side` solid-colour PNG.
fn solid_png(rgb: [u8; 3]) -> Vec<u8> {
    use ::image::ImageEncoder;
    let side = 64;
    let img = ::image::RgbImage::from_pixel(side, side, ::image::Rgb(rgb));
    let mut out = Vec::new();
    ::image::codecs::png::PngEncoder::new(&mut out)
        .write_image(img.as_raw(), side, side, ::image::ExtendedColorType::Rgb8)
        .unwrap();
    out
}

/// Writes applied and flushed.
async fn flushed(mem: &Memory) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while mem.stats().log_depth != 0 || mem.stats().flush_depth != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the flush drains within 20 s");
}

/// Older images (one relevant, two noise) are derived and scored, the daemon
/// is stopped, then a fresh relevant image and a fresh noise image are
/// derived. The recall that follows must report `cold_start`, score every hit
/// `w_query × cosine`, and rank the fresh red image first, every relevant
/// image above every noise image. The old-blend column is computed from the
/// frozen table for comparison; the #79 column is what the recall returned.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a live llama-server with EG2 (LAMBO_EG2_URL)"]
async fn live_eg2_cold_start_ranks_the_fresh_matching_image_first() {
    let Some(url) = std::env::var("LAMBO_EG2_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
    else {
        eprintln!("live_eg2: LAMBO_EG2_URL not set; skipping");
        return;
    };
    let e = EmbeddingGemma2Embedder::new(&url, EG2_DEFAULT_MODEL, 768).unwrap();
    assert!(matches!(
        e.check_server().await,
        Eg2ServerCheck::Verified { .. }
    ));
    let declared = EmbeddingContract {
        kind: "embeddinggemma2".into(),
        model: Some(e.model_identity().to_string()),
        dim: 768,
    };
    let probe = e.embed_query("a red square").await.unwrap();
    // (caption, image id, rgb, relevant, fresh)
    let looks: [(&str, &str, [u8; 3], bool, bool); 5] = [
        (
            "old dark red image",
            "olddarkred",
            [170, 20, 20],
            true,
            false,
        ),
        ("old blue image", "oldblue", [0, 0, 255], false, false),
        ("old green image", "oldgreen", [0, 160, 0], false, false),
        ("fresh red image", "freshred", [255, 0, 0], true, true),
        (
            "fresh gray image",
            "freshgray",
            [128, 128, 128],
            false,
            true,
        ),
    ];
    let mut vectors = Vec::new();
    for (_, _, rgb, _, _) in &looks {
        let bytes = solid_png(*rgb);
        let input = crate::surface::image::validate(&bytes, "image/png").unwrap();
        vectors.push(e.embed_image(input).await.unwrap());
    }
    let q: Vec<f64> = vectors
        .iter()
        .map(|v| f64::from(cosine(&probe, v)))
        .collect();

    let dir = crate::test_util::ScratchDir::new("lambo-79-live-eg2");
    let path = dir.join("live.db");
    let store = Arc::new(crate::store::SqliteStore::connect(path.to_str().unwrap()).unwrap());
    store.init_schema().await.unwrap();
    let mem = Memory::builder()
        .session("issue-79-live-eg2")
        .agent("live-eg2")
        .flush_interval(Duration::from_millis(10))
        .match_strategy(MatchStrategy::Hybrid)
        .store(store as Arc<dyn GraphStore>)
        .embedder(Arc::new(e) as Arc<dyn Embedder>)
        .embedding_contract(declared.clone())
        .build()
        .await
        .unwrap();
    let w = mem.config.recall_weights;
    let agent = AgentId::from("live-eg2");
    let mut ids: Vec<Option<NodeId>> = vec![None; looks.len()];
    let derive = |i: usize| {
        let (mem, agent, declared, looks, vectors) = (&mem, &agent, &declared, &looks, &vectors);
        async move {
            let (caption, image_id, _, _, _) = looks[i];
            mem.derive_image_as(
                agent,
                ImageDerive {
                    caption,
                    concept_type: ConceptType::Resource,
                    image_id: Some(image_id),
                    payload: ImagePayload::Vector {
                        values: vectors[i].clone(),
                        declared: declared.clone(),
                    },
                    parent_of: &[],
                    event_time: None,
                },
            )
            .await
            .unwrap()
            .created[0]
        }
    };
    let query = RecallQuery {
        query: String::new(),
        top_k: 10,
        max_tokens: 10_000,
        traversal_depth: 0,
    };
    let by = || QueryBy::Vector {
        values: probe.clone(),
        declared: declared.clone(),
    };

    // Phase A: the older images, scored, then the daemon held back.
    for (i, id) in ids.iter_mut().enumerate().take(3) {
        *id = Some(derive(i).await);
    }
    flushed(&mem).await;
    mem.settle_daemon().await;
    stop_daemon_for_cold_start(&mem).await;
    let frozen = mem.daemon.scores();
    let d = |id: NodeId| frozen.ranked.iter().find(|s| s.item == id).map(|s| s.score);
    let warm = mem.recall_by_detailed(query.clone(), by()).await.unwrap();
    assert!(!warm.cold_start, "every image is scored: the blend stands");

    // Phase B: the fresh images, which the stopped daemon cannot score.
    for (i, id) in ids.iter_mut().enumerate().skip(3) {
        *id = Some(derive(i).await);
    }
    flushed(&mem).await;
    let ids: Vec<NodeId> = ids.into_iter().map(Option::unwrap).collect();
    assert_eq!(d(ids[3]), None);
    assert_eq!(d(ids[4]), None);
    let cold = mem.recall_by_detailed(query, by()).await.unwrap();
    assert!(cold.cold_start, "the fresh red image can be shown: cold");

    let pos = |id: NodeId| {
        cold.hits
            .iter()
            .position(|h| h.node_id == id)
            .unwrap_or_else(|| panic!("{id:?} returned: {:?}", cold.hits))
    };
    let old_blend: Vec<f64> = (0..looks.len())
        .map(|i| w.w_daemon * d(ids[i]).unwrap_or(0.0) + w.w_query * q[i].max(0.0))
        .collect();
    let old_rank = |i: usize| 1 + old_blend.iter().filter(|x| **x > old_blend[i]).count();
    println!(
        "#79 EG2 cold path (query \"a red square\", w_daemon {} w_query {}):",
        w.w_daemon, w.w_query
    );
    println!(
        "{:<20} {:>3} {:>7} {:>8} {:>18} {:>13}",
        "concept", "rel", "q", "d frozen", "old blend (calc)", "#79 recall"
    );
    for (i, (caption, _, _, relevant, _)) in looks.iter().enumerate() {
        let hit = &cold.hits[pos(ids[i])];
        println!(
            "{:<20} {:>3} {:>7.4} {:>8} {:>13.4} (#{}) {:>8.4} (#{})",
            caption,
            if *relevant { "yes" } else { "no" },
            q[i],
            d(ids[i]).map_or("missing".to_string(), |d| format!("{d:.4}")),
            old_blend[i],
            old_rank(i),
            hit.score,
            pos(ids[i]) + 1,
        );
        // The real cold path: no daemon share, scored or not.
        assert!(
            (hit.score - w.w_query * q[i].max(0.0)).abs() < 1e-4,
            "{caption}: {} != w_query x q",
            hit.score
        );
    }
    assert_eq!(pos(ids[3]), 0, "the fresh red image ranks first");
    let worst_relevant = [0, 3].into_iter().map(|i| pos(ids[i])).max().unwrap();
    let best_noise = [1, 2, 4].into_iter().map(|i| pos(ids[i])).min().unwrap();
    assert!(
        worst_relevant < best_noise,
        "every relevant image ranks above every noise image"
    );
    mem.close().await.unwrap();
}
