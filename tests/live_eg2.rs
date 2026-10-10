//! Live EmbeddingGemma 2 test against a real `llama-server` (#22 PR 5, AC2).
//!
//! Ignored by default and skipped unless `LAMBO_EG2_URL` is set. Start the
//! server as `lambo.example.toml` shows (llama.cpp b11452 or later):
//!
//! ```text
//! llama-server --host 127.0.0.1 --port 8191 \
//!   -m embeddinggemma-2-Q8_0.gguf --mmproj mmproj-embeddinggemma-2-Q8_0.gguf \
//!   --embeddings --pooling mean \
//!   --image-min-tokens 280 --image-max-tokens 280 \
//!   --ctx-size 8192 --batch-size 8192 --ubatch-size 8192
//! LAMBO_EG2_URL=http://127.0.0.1:8191 \
//!   cargo test --features embed-eg2 --test live_eg2 -- --ignored --nocapture
//! ```
//!
//! For the no-projector case, also start a second server without `--mmproj`
//! and set `LAMBO_EG2_TEXT_ONLY_URL` to it.
//!
//! Every image is generated here as an uncompressed PNG; nothing binary is
//! committed. The run prints the design 7.3 ranking-parity table.

#![cfg(feature = "embed-eg2")]

use std::time::Instant;

use lambo::embed::{
    cosine, Eg2ServerCheck, EmbedError, Embedder, EmbeddingGemma2Embedder, EG2_DEFAULT_MODEL,
};
use lambo::recall::candidates::RECENT_SCORE;
use lambo::surface::image::validate;
use lambo::RecallWeights;

// ------------------------------------------------------------ PNG, by hand

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in bytes {
        a = (a + u32::from(x)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// An 8-bit RGB PNG of `side` x `side`, deflate "stored" blocks (no
/// compression), so it decodes anywhere without an encoder crate.
fn png(side: u32, pixel: impl Fn(u32, u32) -> [u8; 3]) -> Vec<u8> {
    png_wh(side, side, pixel)
}

/// [`png`] for a `width` x `height` image.
fn png_wh(width: u32, height: u32, pixel: impl Fn(u32, u32) -> [u8; 3]) -> Vec<u8> {
    let mut raw = Vec::new();
    for y in 0..height {
        raw.push(0); // filter: none
        for x in 0..width {
            raw.extend_from_slice(&pixel(x, y));
        }
    }
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65_535).collect();
    for (i, block) in blocks.iter().enumerate() {
        z.push(u8::from(i + 1 == blocks.len()));
        let len = u16::try_from(block.len()).unwrap();
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(block);
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

fn solid(side: u32, rgb: [u8; 3]) -> Vec<u8> {
    png(side, |_, _| rgb)
}

fn checkerboard(side: u32) -> Vec<u8> {
    png(side, |x, y| {
        if (x / 8 + y / 8) % 2 == 0 {
            [0, 160, 0]
        } else {
            [255, 255, 255]
        }
    })
}

// ------------------------------------------------------------ helpers

fn env_url(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn norm(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

fn assert_unit(v: &[f32], width: usize, what: &str) {
    assert_eq!(v.len(), width, "{what}: width");
    assert!((norm(v) - 1.0).abs() < 1e-4, "{what}: norm {}", norm(v));
    assert!(v.iter().all(|x| x.is_finite()), "{what}: non-finite");
}

async fn image(e: &EmbeddingGemma2Embedder, bytes: &[u8]) -> Result<Vec<f32>, EmbedError> {
    let input = validate(bytes, "image/png").expect("a generated PNG validates");
    e.embed_image(input).await
}

fn stats(xs: &[f32]) -> String {
    let min = xs.iter().copied().fold(f32::INFINITY, f32::min);
    let max = xs.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let mean = xs.iter().sum::<f32>() / xs.len() as f32;
    format!("n={:<2} min={min:.4} mean={mean:.4} max={max:.4}", xs.len())
}

fn median_ms(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(f64::total_cmp);
    xs[xs.len() / 2]
}

/// Captions for the generated images, used as recall queries.
const CAPTIONS: [&str; 4] = [
    "a solid red square",
    "a solid blue square",
    "a green and white checkerboard",
    "a bicycle",
];

/// Text documents and the recall query each should find (design 7.3's
/// text-to-text reference set).
const DOCS: [(&str, &str); 4] = [
    (
        "The deploy pipeline runs the integration tests before it promotes a build to production.",
        "which step runs tests before production",
    ),
    (
        "Session snapshots are stored in Postgres with a pgvector column for recall.",
        "where are session snapshots stored",
    ),
    (
        "The red dress was dismissed for Onam because the colour clashed with the venue.",
        "why was the outfit rejected for Onam",
    ),
    (
        "The write queue retries an embed that failed with a transient server error.",
        "what happens when an embed fails transiently",
    ),
];

/// Concept-name pairs in the document role, against the 0.85 merge
/// threshold calibrated on BGE-M3 (paraphrases, then distinct concepts).
const NEAR: [(&str, &str); 4] = [
    ("register user", "create account"),
    ("delete user", "remove account"),
    ("reset password", "change password"),
    ("deploy service", "ship application"),
];
const FAR: [(&str, &str); 4] = [
    ("register user", "delete user"),
    ("reset password", "deploy service"),
    ("user schema", "payment gateway"),
    ("red dress", "blue square"),
];

// ------------------------------------------------------------ the tests

/// AC2: text and image vectors, MRL, the role prefixes, the cross-modal
/// order, size invariance at the fixed 280-token budget, and the ranking
/// table.
#[tokio::test]
#[ignore = "needs a live llama-server with EmbeddingGemma 2 (LAMBO_EG2_URL)"]
async fn live_eg2_text_and_image() {
    let Some(url) = env_url("LAMBO_EG2_URL") else {
        eprintln!("live_eg2: LAMBO_EG2_URL not set; skipping");
        return;
    };
    let e = EmbeddingGemma2Embedder::new(&url, EG2_DEFAULT_MODEL, 768).unwrap();
    println!("server: {url}");
    println!("contract model: {}", e.model_identity());

    // The /props check passes on the configured server, with vision.
    let check = e.check_server().await;
    println!("/props check: {check:?}");
    assert_eq!(check, Eg2ServerCheck::Verified { vision: Some(true) });

    // Text: unit-norm 768, both roles.
    let doc = e.embed(DOCS[0].0).await.unwrap();
    let query = e.embed_query(DOCS[0].0).await.unwrap();
    assert_unit(&doc, 768, "document");
    assert_unit(&query, 768, "query");
    // The prefixes differ, so the same sentence lands in two places.
    let same_sentence = cosine(&doc, &query);
    println!("cos(same sentence as query, as document) = {same_sentence:.4}");
    assert!(
        same_sentence < 0.995,
        "the role prefixes must change the vector"
    );
    assert!(same_sentence > 0.8, "but not into another meaning");

    // MRL at 256: re-normalized, and the head of the 768 vector.
    let e256 = EmbeddingGemma2Embedder::new(&url, EG2_DEFAULT_MODEL, 256).unwrap();
    let doc256 = e256.embed(DOCS[0].0).await.unwrap();
    assert_unit(&doc256, 256, "document at 256");
    let head = norm(&doc[..256]);
    println!("MRL 256: norm of the 768 vector's head before re-normalizing = {head:.4}");
    for (a, b) in doc256.iter().zip(&doc[..256]) {
        assert!(
            (a - b / head).abs() < 1e-3,
            "MRL 256 is the re-normalized head"
        );
    }

    // Images: unit-norm 768.
    let red64 = solid(64, [255, 0, 0]);
    let red512 = solid(512, [255, 0, 0]);
    let blue64 = solid(64, [0, 0, 255]);
    let checker64 = checkerboard(64);
    let mut images = Vec::new();
    for (name, bytes) in [
        ("red 64px", &red64),
        ("blue 64px", &blue64),
        ("checkerboard 64px", &checker64),
    ] {
        let v = image(&e, bytes).await.unwrap();
        assert_unit(&v, 768, name);
        images.push((name, v));
    }
    let red512v = image(&e, &red512).await.unwrap();
    assert_unit(&red512v, 768, "red 512px");

    // Size invariance: the fixed budget makes 64 px and 512 px one vector.
    let size = cosine(&images[0].1, &red512v);
    println!("cos(red 64px, red 512px) = {size:.6}");
    assert!(
        size > 0.9999,
        "size invariance needs --image-min-tokens 280 --image-max-tokens 280 (got {size})"
    );

    // A large image is embedded too: at the 280 budget b11517 reports fewer
    // prompt tokens for it (250) than for a small one (293), which a
    // window starting at 280 refused. Its vector is near the small render's
    // but not equal: the budget fixes the area, not the token count.
    let red_wide = png_wh(2000, 330, |_, _| [255, 0, 0]);
    let red_wide_v = image(&e, &red_wide).await.unwrap();
    assert_unit(&red_wide_v, 768, "red 2000x330px");
    println!(
        "cos(red 64px, red 2000x330px) = {:.6}",
        cosine(&images[0].1, &red_wide_v)
    );

    // Cross-modal: each image ranks its own caption first, and red is closer
    // to "a red square" than to "a bicycle".
    let mut caption_vecs = Vec::new();
    for c in CAPTIONS {
        caption_vecs.push(e.embed_query(c).await.unwrap());
    }
    println!();
    println!("text query (rows: images) -> image cosine, columns: {CAPTIONS:?}");
    let mut t2i_rel = Vec::new();
    let mut t2i_irr = Vec::new();
    for (i, (name, v)) in images.iter().enumerate() {
        let row: Vec<f32> = caption_vecs.iter().map(|c| cosine(c, v)).collect();
        let best = (0..row.len())
            .max_by(|a, b| row[*a].total_cmp(&row[*b]))
            .unwrap();
        println!(
            "  {name:<18} {}  best: {}",
            row.iter()
                .map(|x| format!("{x:.4}"))
                .collect::<Vec<_>>()
                .join("  "),
            CAPTIONS[best]
        );
        assert_eq!(best, i, "{name} must rank its own caption first");
        for (j, x) in row.iter().enumerate() {
            if j == i {
                t2i_rel.push(*x);
            } else {
                t2i_irr.push(*x);
            }
        }
    }
    assert!(
        cosine(&caption_vecs[0], &images[0].1) > cosine(&caption_vecs[3], &images[0].1),
        "red must be closer to 'a red square' than to 'a bicycle'"
    );

    // Text to text, query role against document role.
    let mut doc_vecs = Vec::new();
    for (d, _) in DOCS {
        doc_vecs.push(e.embed(d).await.unwrap());
    }
    let mut t2t_rel = Vec::new();
    let mut t2t_irr = Vec::new();
    println!();
    println!("text query (rows) -> text document (columns) cosine");
    for (i, (_, q)) in DOCS.iter().enumerate() {
        let qv = e.embed_query(q).await.unwrap();
        let row: Vec<f32> = doc_vecs.iter().map(|d| cosine(&qv, d)).collect();
        println!(
            "  q{i} {}",
            row.iter()
                .map(|x| format!("{x:.4}"))
                .collect::<Vec<_>>()
                .join("  ")
        );
        let best = (0..row.len())
            .max_by(|a, b| row[*a].total_cmp(&row[*b]))
            .unwrap();
        assert_eq!(best, i, "query {i} must find its own document");
        for (j, x) in row.iter().enumerate() {
            if j == i {
                t2t_rel.push(*x);
            } else {
                t2t_irr.push(*x);
            }
        }
    }

    // Document-role concept pairs against the merge threshold.
    let mut near = Vec::new();
    let mut far = Vec::new();
    for (a, b) in NEAR {
        near.push(cosine(
            &e.embed(a).await.unwrap(),
            &e.embed(b).await.unwrap(),
        ));
    }
    for (a, b) in FAR {
        far.push(cosine(
            &e.embed(a).await.unwrap(),
            &e.embed(b).await.unwrap(),
        ));
    }

    // Design 7.3: the ranking-parity table.
    let w = RecallWeights::default();
    println!();
    println!("== design 7.3 ranking parity (EG2 Q8_0, profile lambo-eg2-v2, dim 768) ==");
    println!("text->image relevant    {}", stats(&t2i_rel));
    println!("text->image irrelevant  {}", stats(&t2i_irr));
    println!("text->text  relevant    {}", stats(&t2t_rel));
    println!("text->text  irrelevant  {}", stats(&t2t_irr));
    println!("doc<->doc   paraphrase  {}", stats(&near));
    println!("doc<->doc   distinct    {}", stats(&far));
    println!(
        "recall: final = {} x daemon + {} x query; RECENT_SCORE = {RECENT_SCORE}",
        w.w_daemon, w.w_query
    );
    let mean = |xs: &[f32]| f64::from(xs.iter().sum::<f32>() / xs.len() as f32);
    for (label, xs) in [("text->image", &t2i_rel), ("text->text ", &t2t_rel)] {
        let m = mean(xs);
        println!(
            "{label} relevant mean {m:.4}: phase-1 leg {} RECENT_SCORE {RECENT_SCORE}; fresh \
             (no daemon score yet) final {:.4} vs a daemon-scored older concept at 0.533 \
             (PR 3's run): {}",
            if m > RECENT_SCORE {
                "beats"
            } else {
                "loses to"
            },
            w.w_query * m,
            if w.w_query * m > 0.533 {
                "wins"
            } else {
                "loses"
            }
        );
    }

    // Latency, warm.
    let mut text_ms = Vec::new();
    let mut image_ms = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        e.embed_query("warm latency probe").await.unwrap();
        text_ms.push(t.elapsed().as_secs_f64() * 1e3);
        let t = Instant::now();
        image(&e, &blue64).await.unwrap();
        image_ms.push(t.elapsed().as_secs_f64() * 1e3);
    }
    println!(
        "warm median latency: text {:.1} ms, image {:.1} ms (through Lambo's client)",
        median_ms(text_ms),
        median_ms(image_ms)
    );
}

/// The "provide the mmproj" case: a server started without `--mmproj`
/// embeds text, and an image is a permanent configuration error naming
/// `--mmproj`, both when the `/props` check catches it and when the server's
/// own 500 does.
#[tokio::test]
#[ignore = "needs a live llama-server without --mmproj (LAMBO_EG2_TEXT_ONLY_URL)"]
async fn live_eg2_text_only_server_refuses_images_permanently() {
    let Some(url) = env_url("LAMBO_EG2_TEXT_ONLY_URL") else {
        eprintln!("live_eg2: LAMBO_EG2_TEXT_ONLY_URL not set; skipping");
        return;
    };
    let red = solid(64, [255, 0, 0]);

    let checked = EmbeddingGemma2Embedder::new(&url, EG2_DEFAULT_MODEL, 768).unwrap();
    let check = checked.check_server().await;
    println!("/props check: {check:?}");
    assert_eq!(
        check,
        Eg2ServerCheck::Verified {
            vision: Some(false)
        }
    );
    assert_unit(
        &checked.embed("text still embeds").await.unwrap(),
        768,
        "text",
    );
    let err = image(&checked, &red).await.unwrap_err();
    println!("refused by the check: {err}");
    assert!(
        matches!(err, EmbedError::Backend(_)) && !err.is_transient(),
        "{err:?}"
    );
    assert!(err.to_string().contains("--mmproj"), "{err}");

    let unchecked = EmbeddingGemma2Embedder::new(&url, EG2_DEFAULT_MODEL, 768)
        .unwrap()
        .without_server_check();
    let err = image(&unchecked, &red).await.unwrap_err();
    println!("refused by the server: {err}");
    assert!(
        matches!(err, EmbedError::Backend(_)) && !err.is_transient(),
        "{err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("500") && msg.contains("provide the mmproj") && msg.contains("--mmproj"),
        "{msg}"
    );

    let off = EmbeddingGemma2Embedder::new(&url, EG2_DEFAULT_MODEL, 768)
        .unwrap()
        .with_images(false);
    assert!(matches!(
        image(&off, &red).await,
        Err(EmbedError::Unsupported(_))
    ));
}

// ------------------------------------------------- 22g: size invariance

/// A compressed PNG (the hand-built ones above are stored, too big for the
/// 2 MiB cap at 3000 px).
fn png_compressed(side: u32, pixel: impl Fn(u32, u32) -> [u8; 3]) -> Vec<u8> {
    use image::{codecs::png::CompressionType, codecs::png::FilterType, ImageEncoder};
    let img = image::RgbImage::from_fn(side, side, |x, y| image::Rgb(pixel(x, y)));
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new_with_quality(
        &mut out,
        CompressionType::Best,
        FilterType::Adaptive,
    )
    .write_image(img.as_raw(), side, side, image::ColorType::Rgb8.into())
    .unwrap();
    out
}

/// One picture at any size: 8x8 cells whatever the side, so a render at
/// 3000 px is the 768 px one scaled up (not more, smaller cells).
fn checker_cells(side: u32) -> Vec<u8> {
    let cell = side / 8;
    png_compressed(side, |x, y| {
        if (x / cell + y / cell).is_multiple_of(2) {
            [0, 160, 0]
        } else {
            [255, 255, 255]
        }
    })
}

/// A smooth picture: two gradients and a soft disc, closer to a photo than
/// the checkerboard's hard edges.
fn gradient(side: u32) -> Vec<u8> {
    let s = side as f32;
    png_compressed(side, |x, y| {
        let (u, v) = (x as f32 / s, y as f32 / s);
        let d = ((u - 0.5).powi(2) + (v - 0.4).powi(2)).sqrt();
        let disc = (1.0 - (d * 4.0).min(1.0)) * 200.0;
        [(u * 255.0) as u8, (v * 255.0) as u8, disc as u8]
    })
}

/// The submitted bytes posted straight to the server, as `lambo-eg2-v1` did
/// (no canonical form), for the "before" column.
async fn raw_image(url: &str, png: &[u8]) -> Vec<f32> {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(png);
    let body = serde_json::json!({
        "input": [{ "content": [{
            "type": "image_url",
            "image_url": { "url": format!("data:image/png;base64,{b64}") }
        }]}]
    });
    let resp: serde_json::Value = reqwest::Client::new()
        .post(format!("{}/v1/embeddings", url.trim_end_matches('/')))
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    resp["data"][0]["embedding"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_f64().unwrap() as f32)
        .collect()
}

/// 22g: with the `lambo-eg2-v2` canonical form every image is sent with its
/// longer side at exactly 768 px, so a picture's vector does not depend on
/// the size it was submitted at beyond what resampling changes. Prints, per
/// picture and size, the cosine to the native 768 px render, before (the
/// submitted bytes as they are, as `lambo-eg2-v1` sent them) and after
/// (through the adapter), and the worst pair of sizes. Asserts that a solid
/// colour embeds bit-identically at every size, and that every pair of
/// renders of a patterned picture agrees to at least 0.975 (the measured
/// floor, 0.9818 for a checkerboard drawn at 128 px, is in
/// `evidence/issue-22-eg2/size-invariance.txt`; this bound is looser so a
/// rebuilt server does not fail the run on noise).
#[tokio::test]
#[ignore = "needs a live llama-server with EmbeddingGemma 2 (LAMBO_EG2_URL)"]
async fn live_eg2_size_invariance() {
    let Some(url) = env_url("LAMBO_EG2_URL") else {
        eprintln!("live_eg2: LAMBO_EG2_URL not set; skipping");
        return;
    };
    let e = EmbeddingGemma2Embedder::new(&url, EG2_DEFAULT_MODEL, 768).unwrap();
    println!("contract model: {}", e.model_identity());
    let sizes = [128u32, 256, 512, 768, 1024, 1536, 2048, 3000];
    type Gen = fn(u32) -> Vec<u8>;
    let pictures: [(&str, Gen); 3] = [
        ("solid", |s| png_compressed(s, |_, _| [200, 40, 40])),
        ("checker 8x8", checker_cells),
        ("gradient", gradient),
    ];
    println!("== 22g size invariance: cosine to the native 768 px render ==");
    println!("picture      | px   | bytes   | before (v1 raw) | after (v2)  | after == 768 bits");
    for (name, make) in pictures {
        let base_png = make(768);
        let base = image(&e, &base_png).await.unwrap();
        let base_raw = raw_image(&url, &base_png).await;
        let mut all = Vec::new();
        let mut worst_to_768 = 1.0f32;
        for side in sizes {
            let png = make(side);
            let after = image(&e, &png).await.unwrap();
            assert_unit(&after, 768, name);
            let before = raw_image(&url, &png).await;
            let cos_after = cosine(&after, &base);
            let cos_before = cosine(&before, &base_raw);
            println!(
                "{name:<12} | {side:>4} | {:>7} | {cos_before:.6}        | {cos_after:.6}    | {}",
                png.len(),
                after == base
            );
            if name == "solid" {
                assert_eq!(after, base, "solid at {side} px must embed bit-identically");
            }
            worst_to_768 = worst_to_768.min(cos_after);
            all.push((side, after));
        }
        let mut worst = (f32::INFINITY, 0, 0);
        let mut worst_down = 1.0f32;
        for (i, (a_side, a)) in all.iter().enumerate() {
            for (b_side, b) in &all[i + 1..] {
                let c = cosine(a, b);
                if c < worst.0 {
                    worst = (c, *a_side, *b_side);
                }
                if *a_side >= 768 && *b_side >= 768 {
                    worst_down = worst_down.min(c);
                }
            }
        }
        println!(
            "{name:<12} | worst to 768: {worst_to_768:.6}; worst pair {:.6} ({} vs {}); \
             worst pair at 768 px and above: {worst_down:.6}",
            worst.0, worst.1, worst.2
        );
        assert!(worst.0 >= 0.975, "{name}: {worst:?}");
    }

    // A WebP is sent as its canonical PNG: the same pixels as a PNG embed to
    // the same vector, whatever ffmpeg the server has or lacks.
    let side = 512;
    let img = image::RgbImage::from_fn(side, side, |x, y| image::Rgb([x as u8, y as u8, 90]));
    let mut webp = Vec::new();
    {
        use image::ImageEncoder;
        image::codecs::webp::WebPEncoder::new_lossless(&mut webp)
            .write_image(img.as_raw(), side, side, image::ColorType::Rgb8.into())
            .unwrap();
    }
    let png = png_compressed(side, |x, y| [x as u8, y as u8, 90]);
    let wv = e
        .embed_image(validate(&webp, "image/webp").unwrap())
        .await
        .unwrap();
    let pv = image(&e, &png).await.unwrap();
    println!(
        "cos(webp 512, png 512, same pixels) = {:.6}",
        cosine(&wv, &pv)
    );
    // Same pixels, same canonical PNG: the same vector, bit for bit.
    assert_eq!(wv, pv);
}
