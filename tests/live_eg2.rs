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
    let mut raw = Vec::new();
    for y in 0..side {
        raw.push(0); // filter: none
        for x in 0..side {
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
    ihdr.extend_from_slice(&side.to_be_bytes());
    ihdr.extend_from_slice(&side.to_be_bytes());
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
    println!("== design 7.3 ranking parity (EG2 Q8_0, profile lambo-eg2-v1, dim 768) ==");
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
