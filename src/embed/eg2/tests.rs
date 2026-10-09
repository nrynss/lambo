//! httpmock tests for the EmbeddingGemma 2 layer (#22 PR 5). No model runs
//! here; `tests/live_eg2.rs` is the live counterpart.

use super::*;
use crate::embed::{build_embedder, eg2_identity, EmbedderKind};
use httpmock::prelude::*;
use std::time::Duration;

const MODEL: &str = EG2_DEFAULT_MODEL;

/// A 1x1 RGBA PNG (header-valid and decodable), so `surface::image::validate`
/// accepts it. Built from base64 so no binary is committed.
fn png_1x1() -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==",
        )
        .unwrap()
}

/// A 2x1 RGB PNG, a different image from [`png_1x1`] (Lambo's reference
/// image), so a mock can tell the two requests apart.
fn png_2x1() -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAIAAAABCAIAAAB7QOjdAAAADUlEQVR4nGP4zwAE/wEHAAH/4iOeWQAAAABJRU5ErkJggg==")
        .unwrap()
}

/// A native-width vector that is not unit norm and not uniform, so a test can
/// see both the truncation and the normalization.
fn native() -> Vec<f32> {
    (0..EG2_NATIVE_DIM).map(|i| 1.0 + (i % 7) as f32).collect()
}

fn ok_body(embedding: &[f32], prompt_tokens: Option<u64>) -> serde_json::Value {
    let mut body = serde_json::json!({
        "object": "list",
        "data": [{ "object": "embedding", "index": 0, "embedding": embedding }],
        "model": "/models/embeddinggemma-2-Q8_0.gguf",
    });
    if let Some(n) = prompt_tokens {
        body["usage"] = serde_json::json!({ "prompt_tokens": n, "total_tokens": n });
    }
    body
}

fn norm(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

/// What b11517's `/props` says for the Q8_0 GGUF, trimmed to the keys read.
fn props(file: &str, ftype: &str, vision: bool) -> serde_json::Value {
    serde_json::json!({
        "model_path": format!("/Users/someone/models/{file}"),
        "model_ftype": ftype,
        "modalities": { "vision": vision, "audio": vision, "video": vision },
        "build_info": "b11517-8a1a9b512",
    })
}

fn embedder(server: &MockServer) -> EmbeddingGemma2Embedder {
    EmbeddingGemma2Embedder::new(server.base_url(), MODEL, 768).unwrap()
}

fn image_body(png: &[u8]) -> String {
    format!(
        r#"{{"model":"{MODEL}","input":[{{"content":[{{"type":"image_url","image_url":{{"url":"data:image/png;base64,{}"}}}}]}}]}}"#,
        base64::engine::general_purpose::STANDARD.encode(png)
    )
}

/// The request body for a user image: its canonical form (22g), never the
/// submitted bytes. The reference image is posted as it is
/// ([`image_body`]).
fn sent_body(png: &[u8]) -> String {
    image_body(&canonical::to_canonical_png(png, crate::embed::ImageMime::Png).unwrap())
}

// ---------------------------------------------------------------- requests

/// The two text roles are sent byte-exact, each with its model-card prefix,
/// and the image with none.
///
/// Mutation: swap the two prefixes, or drop one -> red.
#[tokio::test]
async fn text_bodies_are_byte_exact_with_the_role_prefix() {
    let server = MockServer::start();
    let doc = server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings").body(format!(
            r#"{{"model":"{MODEL}","input":"title: none | text: red dress"}}"#
        ));
        then.status(200).json_body(ok_body(&native(), Some(9)));
    });
    let query = server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings").body(format!(
            r#"{{"model":"{MODEL}","input":"task: search result | query: red dress"}}"#
        ));
        then.status(200).json_body(ok_body(&native(), Some(11)));
    });
    let e = embedder(&server);
    let d = e.embed("red dress").await.unwrap();
    let q = e.embed_query("red dress").await.unwrap();
    doc.assert_hits(1);
    query.assert_hits(1);
    assert_eq!(d.len(), 768);
    assert!((norm(&d) - 1.0).abs() < 1e-5);
    assert!((norm(&q) - 1.0).abs() < 1e-5);
}

/// The image goes as one input item whose content is the `image_url` part,
/// a base64 data URI of its canonical PNG, no text and no prefix.
#[tokio::test]
async fn the_image_body_is_the_nested_image_url_data_uri() {
    let server = MockServer::start();
    let png = png_1x1();
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/embeddings")
            .body(sent_body(&png));
        then.status(200).json_body(ok_body(&native(), Some(293)));
    });
    let input = crate::surface::image::validate(&png, "image/png").unwrap();
    let v = embedder(&server).embed_image(input).await.unwrap();
    mock.assert_hits(1);
    assert_eq!(v.len(), 768);
    assert!((norm(&v) - 1.0).abs() < 1e-5);
}

/// Every image goes out as its canonical form (a lossless PNG with a 768 px
/// longer side): a large PNG downscaled, a small WebP upscaled. The request
/// body carries exactly the bytes `canonical::to_canonical_png` makes, never
/// the submitted ones (22g).
///
/// Mutation: send `image.bytes()` again -> red.
#[tokio::test]
async fn the_image_body_carries_the_canonical_form() {
    use image::{codecs::webp::WebPEncoder, ImageEncoder, Rgb, RgbImage};

    let big = RgbImage::from_fn(1536, 1024, |x, y| Rgb([x as u8, y as u8, (x ^ y) as u8]));
    let mut big_png = Vec::new();
    image::DynamicImage::ImageRgb8(big.clone())
        .write_to(std::io::Cursor::new(&mut big_png), image::ImageFormat::Png)
        .unwrap();
    let mut small_webp = Vec::new();
    let small = RgbImage::from_fn(40, 30, |x, y| Rgb([x as u8, y as u8, 9]));
    WebPEncoder::new_lossless(&mut small_webp)
        .write_image(small.as_raw(), 40, 30, image::ColorType::Rgb8.into())
        .unwrap();

    for (bytes, mime, side) in [
        (&big_png, "image/png", (768, 512)),
        (&small_webp, "image/webp", (768, 576)),
    ] {
        let want =
            canonical::to_canonical_png(bytes, crate::embed::ImageMime::from_mime(mime).unwrap())
                .unwrap();
        let decoded = image::load_from_memory_with_format(&want, image::ImageFormat::Png).unwrap();
        assert_eq!((decoded.width(), decoded.height()), side, "{mime}");
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/embeddings")
                .body(image_body(&want));
            then.status(200).json_body(ok_body(&native(), Some(293)));
        });
        let input = crate::surface::image::validate(bytes, mime).unwrap();
        let sha = input.sha256();
        let v = embedder(&server).embed_image(input).await.unwrap();
        mock.assert_hits(1);
        assert_eq!(v.len(), 768);
        // What Lambo stores about the image still describes the submitted
        // bytes, not the canonical ones.
        assert_eq!(
            sha,
            crate::surface::image::validate(bytes, mime)
                .unwrap()
                .sha256()
        );
        assert_ne!(want.as_slice(), bytes.as_slice());
    }
}

/// An image Lambo cannot decode fails before any request reaches the server.
#[tokio::test]
async fn an_undecodable_large_image_never_reaches_the_server() {
    let server = MockServer::start();
    let any = server.mock(|when, then| {
        when.any_request();
        then.status(200).json_body(ok_body(&native(), Some(293)));
    });
    let mut png = Vec::new();
    image::DynamicImage::new_rgb8(1000, 1000)
        .write_to(std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    png.truncate(png.len() / 2);
    let input = crate::surface::image::validate(&png, "image/png").unwrap();
    let err = embedder(&server).embed_image(input).await.unwrap_err();
    assert!(
        matches!(&err, EmbedError::Backend(m) if m.contains("could not decode")),
        "{err:?}"
    );
    assert!(!err.is_transient());
    any.assert_hits(0);
}

/// Empty text is refused before any request, like every other adapter.
#[tokio::test]
async fn empty_text_is_refused_without_a_request() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings");
        then.status(200).json_body(ok_body(&native(), None));
    });
    let e = embedder(&server);
    for text in ["", "   "] {
        assert!(matches!(
            e.embed(text).await,
            Err(EmbedError::Unavailable(_))
        ));
        assert!(matches!(
            e.embed_query(text).await,
            Err(EmbedError::Unavailable(_))
        ));
    }
    mock.assert_hits(0);
}

// ---------------------------------------------------------------- MRL

/// Truncate to `dim`, THEN normalize: the result is the unit vector along the
/// first `dim` components, not the first `dim` components of the unit
/// 768-vector (which would have norm < 1).
///
/// Mutation: normalize before truncating -> red.
#[tokio::test]
async fn mrl_truncates_then_normalizes_at_every_width() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings");
        then.status(200).json_body(ok_body(&native(), None));
    });
    let full = native();
    for dim in EG2_MRL_DIMS {
        let e = EmbeddingGemma2Embedder::new(server.base_url(), MODEL, dim).unwrap();
        assert_eq!(e.dimensions(), dim);
        let v = e.embed("x").await.unwrap();
        assert_eq!(v.len(), dim);
        assert!(
            (norm(&v) - 1.0).abs() < 1e-5,
            "dim {dim}: norm {}",
            norm(&v)
        );
        let head = norm(&full[..dim]);
        for (got, raw) in v.iter().zip(&full[..dim]) {
            assert!((got - raw / head).abs() < 1e-6, "dim {dim}");
        }
    }
}

/// `dim` outside the MRL set is refused at build, naming 768.
#[test]
fn a_width_outside_the_mrl_set_is_refused_at_build() {
    for dim in [0, 64, 384, 1024, 3072] {
        let err = EmbeddingGemma2Embedder::new("http://127.0.0.1:9", MODEL, dim).unwrap_err();
        assert!(matches!(err, EmbedError::Unavailable(_)));
        assert!(err.to_string().contains("dim = 768"), "{err}");
    }
}

/// A width other than the native 768, a non-finite component anywhere (also
/// in the tail MRL discards) and a zero norm after truncation are refused as
/// `Backend`.
#[tokio::test]
async fn wrong_width_non_finite_and_zero_norm_are_refused() {
    async fn refusal(embedding: Vec<f32>, dim: usize) -> EmbedError {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(200).json_body(ok_body(&embedding, None));
        });
        EmbeddingGemma2Embedder::new(server.base_url(), MODEL, dim)
            .unwrap()
            .embed("x")
            .await
            .unwrap_err()
    }
    // BGE-M3's 1024 behind the EG2 kind.
    let err = refusal(vec![1.0; 1024], 768).await;
    assert!(matches!(err, EmbedError::Backend(_)), "{err:?}");
    assert!(err.to_string().contains("1024") && err.to_string().contains("768"));
    // MRL-truncated by a server is not EG2's native output either.
    let err = refusal(vec![1.0; 256], 256).await;
    assert!(matches!(err, EmbedError::Backend(_)), "{err:?}");

    // serde_json cannot carry NaN, so a non-finite value arrives as a huge
    // literal that overflows f32 to infinity.
    let server = MockServer::start();
    let mut raw: Vec<String> = native().iter().map(|x| x.to_string()).collect();
    raw[700] = "1e300".into();
    server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings");
        then.status(200).body(format!(
            r#"{{"data":[{{"embedding":[{}]}}]}}"#,
            raw.join(",")
        ));
    });
    let err = EmbeddingGemma2Embedder::new(server.base_url(), MODEL, 128)
        .unwrap()
        .embed("x")
        .await
        .unwrap_err();
    assert!(matches!(err, EmbedError::Backend(_)), "{err:?}");
    assert!(err.to_string().contains("non-finite"), "{err}");

    // Zero over the kept head, non-zero in the discarded tail.
    let mut v = vec![0.0; 768];
    for x in &mut v[128..] {
        *x = 1.0;
    }
    let err = refusal(v.clone(), 128).await;
    assert!(err.to_string().contains("zero-norm"), "{err}");
    // The same vector at full width is fine.
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings");
        then.status(200).json_body(ok_body(&v, None));
    });
    assert!(embedder(&server).embed("x").await.is_ok());
}

// ---------------------------------------------------------------- statuses

/// Text statuses follow the J3 table unchanged; an image's `500` is refined
/// by its body: "provide the mmproj" is a permanent deployment fault naming
/// `--mmproj`, "Failed to load image" a content refusal, any other `500`
/// still transient.
///
/// Mutation: route image calls through `default_status_rule` -> red (the
/// mmproj 500 becomes `Unavailable`, retried forever).
#[tokio::test]
async fn status_classes_with_the_image_500_refined() {
    async fn text_err(status: u16, body: &str) -> EmbedError {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(status).body(body);
        });
        embedder(&server).embed("x").await.unwrap_err()
    }
    async fn image_err(status: u16, body: &str) -> EmbedError {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(status).body(body);
        });
        let png = png_1x1();
        let input = crate::surface::image::validate(&png, "image/png").unwrap();
        embedder(&server).embed_image(input).await.unwrap_err()
    }
    const MMPROJ: &str = r#"{"error":{"code":500,"message":"image input is not supported - hint: if this is unexpected, you may need to provide the mmproj","type":"server_error"}}"#;
    const DECODE: &str = r#"{"error":{"code":500,"message":"Failed to load image or audio file","type":"server_error"}}"#;

    for (status, transient) in [(500, true), (503, true), (400, false), (401, false)] {
        let err = text_err(status, "nope").await;
        assert_eq!(err.is_transient(), transient, "text {status}: {err:?}");
    }
    // A text call answered with the mmproj body is not an image fault.
    assert!(text_err(500, MMPROJ).await.is_transient());

    let err = image_err(500, MMPROJ).await;
    assert!(matches!(err, EmbedError::Backend(_)), "{err:?}");
    assert!(!err.is_transient());
    assert!(err.to_string().contains("--mmproj"), "{err}");
    assert!(
        err.to_string().contains("permanent configuration error"),
        "{err}"
    );

    let err = image_err(500, DECODE).await;
    assert!(matches!(err, EmbedError::Backend(_)), "{err:?}");
    assert!(err.to_string().contains("could not decode"), "{err}");

    assert!(image_err(500, "busy").await.is_transient());
    assert!(image_err(503, "loading").await.is_transient());
    for status in [400, 413, 415, 422] {
        let err = image_err(status, "too big").await;
        assert!(matches!(err, EmbedError::Backend(_)), "{status}: {err:?}");
    }
}

/// An image response's own token count is not judged: at the profile's
/// budget b11517 reports anything from 236 to 540 depending on the image's
/// size and shape (260 for a square of 800 px or more, 250 for 2000x330,
/// 540 for 16x4096), so a window refused ordinary photos. The budget is
/// checked with the reference image instead.
///
/// Mutation: the old 280..=312 window -> red.
#[tokio::test]
async fn an_image_is_not_judged_by_its_own_token_count() {
    for tokens in [Some(236), Some(250), Some(260), Some(293), Some(540), None] {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(200).json_body(ok_body(&native(), tokens));
        });
        let png = png_1x1();
        let input = crate::surface::image::validate(&png, "image/png").unwrap();
        assert!(
            embedder(&server).embed_image(input).await.is_ok(),
            "{tokens:?}"
        );
    }
}

/// For a server `/props` verified, the first image embed is preceded by
/// Lambo's reference image, whose count tells the profile's budget (293 on
/// b11517) from a ubatch-capped one (260) or the dynamic default (85),
/// although both of those counts can also come from a correct server for
/// other images. A wrong count refuses the image before it is sent; a right
/// one is kept for the recheck interval, so the next image sends no
/// reference.
///
/// Mutation: skip `ensure_image_budget` -> red.
#[tokio::test]
async fn a_verified_server_is_checked_with_the_reference_image() {
    for (reference_tokens, accepted) in [
        (293, true),
        (288, true),
        (301, true),
        (260, false),
        (85, false),
        (328, false),
    ] {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/props");
            then.status(200)
                .json_body(props("embeddinggemma-2-Q8_0.gguf", "Q8_0", true));
        });
        let reference = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/embeddings")
                .body(image_body(&png_1x1()));
            then.status(200)
                .json_body(ok_body(&native(), Some(reference_tokens)));
        });
        let png = png_2x1();
        let image = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/embeddings")
                .body(sent_body(&png));
            then.status(200).json_body(ok_body(&native(), Some(260)));
        });
        let e = embedder(&server);
        for _ in 0..2 {
            let input = crate::surface::image::validate(&png, "image/png").unwrap();
            let result = e.embed_image(input).await;
            assert_eq!(result.is_ok(), accepted, "{reference_tokens}: {result:?}");
            if let Err(err) = result {
                assert!(matches!(err, EmbedError::Backend(_)), "{err:?}");
                let msg = err.to_string();
                assert!(msg.contains("reference image"), "{msg}");
                assert!(msg.contains("--ubatch-size 8192"), "{msg}");
            }
        }
        if accepted {
            reference.assert_hits(1);
            image.assert_hits(2);
        } else {
            reference.assert_hits(2);
            image.assert_hits(0);
        }
    }
}

/// On a server `/props` verified as llama-server, which always reports
/// `usage`, a reference response without the token count means the budget
/// cannot be checked: image embeds are refused (naming the build), not
/// embedded unchecked. Text is unaffected. A server that never verified
/// (no reference check) still embeds an image without `usage`; its
/// unchecked budget is the once-only `/props` warning's subject.
///
/// Mutation: accept a missing count in `ensure_image_budget` -> red.
#[tokio::test]
async fn a_verified_server_without_the_token_count_refuses_images() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/props");
        then.status(200)
            .json_body(props("embeddinggemma-2-Q8_0.gguf", "Q8_0", true));
    });
    let image_post = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/embeddings")
            .body_contains("image_url");
        then.status(200).json_body(ok_body(&native(), None));
    });
    server.mock(|when, then| {
        when.method(POST)
            .path("/v1/embeddings")
            .body_contains("text: ");
        then.status(200).json_body(ok_body(&native(), None));
    });
    let e = embedder(&server);
    let png = png_2x1();
    let input = crate::surface::image::validate(&png, "image/png").unwrap();
    let err = e.embed_image(input).await.unwrap_err();
    assert!(matches!(err, EmbedError::Backend(_)), "{err:?}");
    let msg = err.to_string();
    assert!(msg.contains("without usage.prompt_tokens"), "{msg}");
    assert!(
        msg.contains("b11517-8a1a9b512"),
        "the build is named: {msg}"
    );
    // Only the reference was sent, not the image.
    image_post.assert_hits(1);
    e.embed("text still works").await.unwrap();

    // Not verified (a hosted endpoint): no reference, no count needed.
    let hosted = MockServer::start();
    let hosted_post = hosted.mock(|when, then| {
        when.method(POST).path("/v1/embeddings");
        then.status(200).json_body(ok_body(&native(), None));
    });
    let input = crate::surface::image::validate(&png, "image/png").unwrap();
    embedder(&hosted).embed_image(input).await.unwrap();
    hosted_post.assert_hits(1);
}

// ---------------------------------------------------------------- images = false

/// `images = false` reports text only and refuses an image as `Unsupported`
/// without sending it.
#[tokio::test]
async fn images_off_reports_text_only_and_never_sends_an_image() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings");
        then.status(200).json_body(ok_body(&native(), Some(293)));
    });
    let on = embedder(&server);
    assert_eq!(on.modalities(), Modalities::TEXT | Modalities::IMAGE);
    let off = embedder(&server).with_images(false);
    assert_eq!(off.modalities(), Modalities::TEXT);
    let png = png_1x1();
    let input = crate::surface::image::validate(&png, "image/png").unwrap();
    let err = off.embed_image(input).await.unwrap_err();
    assert!(matches!(err, EmbedError::Unsupported(_)), "{err:?}");
    assert!(err.to_string().contains("images = false"), "{err}");
    mock.assert_hits(0);
    assert!(off.embed("still embeds text").await.is_ok());
}

// ---------------------------------------------------------------- the /props check

/// A server reporting the EG2 Q8_0 file with vision is verified once and then
/// not asked again.
#[tokio::test]
async fn a_matching_server_is_verified_once() {
    let server = MockServer::start();
    let props_mock = server.mock(|when, then| {
        when.method(GET).path("/props");
        then.status(200)
            .json_body(props("embeddinggemma-2-Q8_0.gguf", "Q8_0", true));
    });
    server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings");
        then.status(200).json_body(ok_body(&native(), Some(293)));
    });
    let e = embedder(&server);
    e.embed("a").await.unwrap();
    e.embed_query("b").await.unwrap();
    let png = png_1x1();
    let input = crate::surface::image::validate(&png, "image/png").unwrap();
    e.embed_image(input).await.unwrap();
    assert_eq!(
        e.check_server().await,
        Eg2ServerCheck::Verified { vision: Some(true) }
    );
    props_mock.assert_hits(1);
}

/// Another model behind the EG2 kind (EmbeddingGemma 1, BGE-M3) or another
/// quantization than the configured artifact names is refused before any
/// embed request. A mismatch is not kept: once the server is fixed the next
/// embed goes through without restarting.
#[tokio::test]
async fn a_mismatched_server_refuses_every_embed_until_fixed() {
    for (file, ftype, needle) in [
        ("embeddinggemma-300M-Q8_0.gguf", "Q8_0", "does not name"),
        ("bge-m3-Q8_0.gguf", "Q8_0", "does not name"),
        ("embeddinggemma-2-F16.gguf", "F16", "quantized as F16"),
    ] {
        let server = MockServer::start();
        let mut bad = server.mock(|when, then| {
            when.method(GET).path("/props");
            then.status(200).json_body(props(file, ftype, true));
        });
        let post = server.mock(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(200).json_body(ok_body(&native(), Some(293)));
        });
        let e = embedder(&server);
        for _ in 0..2 {
            let err = e.embed("x").await.unwrap_err();
            assert!(matches!(err, EmbedError::Backend(_)), "{file}: {err:?}");
            let msg = err.to_string();
            assert!(msg.contains(needle), "{file}: {msg}");
            assert!(
                !msg.contains("/Users/someone"),
                "the directory is not shown: {msg}"
            );
        }
        let png = png_1x1();
        let input = crate::surface::image::validate(&png, "image/png").unwrap();
        assert!(e.embed_image(input).await.is_err());
        post.assert_hits(0);
        bad.assert_hits(3);

        bad.delete();
        server.mock(|when, then| {
            when.method(GET).path("/props");
            then.status(200)
                .json_body(props("embeddinggemma-2-Q8_0.gguf", "Q8_0", true));
        });
        e.embed("x").await.unwrap();
        post.assert_hits(1);
    }
}

/// A server without a vision projector embeds text, and refuses images before
/// sending them while `images` is on, naming `--mmproj`. Restarting it with
/// the projector is picked up by the next image embed.
#[tokio::test]
async fn a_server_without_vision_refuses_images_but_embeds_text() {
    let server = MockServer::start();
    let mut text_only = server.mock(|when, then| {
        when.method(GET).path("/props");
        then.status(200)
            .json_body(props("embeddinggemma-2-Q8_0.gguf", "Q8_0", false));
    });
    let image_post = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/embeddings")
            .body_contains("image_url");
        then.status(200).json_body(ok_body(&native(), Some(293)));
    });
    server.mock(|when, then| {
        when.method(POST)
            .path("/v1/embeddings")
            .body_contains("text: ");
        then.status(200).json_body(ok_body(&native(), Some(5)));
    });
    let e = embedder(&server);
    e.embed("text works").await.unwrap();
    e.embed("still works, without asking /props again")
        .await
        .unwrap();
    text_only.assert_hits(1);
    let png = png_1x1();
    let input = crate::surface::image::validate(&png, "image/png").unwrap();
    let err = e.embed_image(input).await.unwrap_err();
    assert!(matches!(err, EmbedError::Backend(_)), "{err:?}");
    assert!(err.to_string().contains("--mmproj"), "{err}");
    image_post.assert_hits(0);

    // With images off the same server is fine for everything it is asked.
    let off = embedder(&server).with_images(false);
    off.embed("x").await.unwrap();

    text_only.delete();
    server.mock(|when, then| {
        when.method(GET).path("/props");
        then.status(200)
            .json_body(props("embeddinggemma-2-Q8_0.gguf", "Q8_0", true));
    });
    let input = crate::surface::image::validate(&png, "image/png").unwrap();
    e.embed_image(input).await.unwrap();
    // The reference image, then the image itself.
    image_post.assert_hits(2);
}

/// A server that does not answer `/props` with a model (a 404 from a hosted
/// endpoint, a JSON without `model_path`, a non-JSON page) is used unchecked,
/// and asked once. A 5xx `/props` is asked again, but only after a wait
/// (see `a_props_check_that_cannot_run_backs_off_and_logs_once`).
#[tokio::test]
async fn an_endpoint_without_props_is_used_unchecked() {
    for (status, body) in [
        (404, "not found".to_string()),
        (200, r#"{"build_info":"x"}"#.to_string()),
        (200, "<html>".to_string()),
    ] {
        let server = MockServer::start();
        let props_mock = server.mock(|when, then| {
            when.method(GET).path("/props");
            then.status(status).body(body.clone());
        });
        server.mock(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(200).json_body(ok_body(&native(), None));
        });
        let e = embedder(&server);
        e.embed("a").await.unwrap();
        e.embed("b").await.unwrap();
        assert_eq!(e.check_server().await, Eg2ServerCheck::NotExposed, "{body}");
        props_mock.assert_hits(1);
    }

    let server = MockServer::start();
    let props_mock = server.mock(|when, then| {
        when.method(GET).path("/props");
        then.status(503).body("loading");
    });
    server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings");
        then.status(200).json_body(ok_body(&native(), None));
    });
    let e = embedder(&server);
    e.embed("a").await.unwrap();
    e.embed("b").await.unwrap();
    props_mock.assert_hits(1);

    // Turned off, nothing is asked.
    let skipped = embedder(&server).without_server_check();
    assert_eq!(skipped.check_server().await, Eg2ServerCheck::Skipped);
    skipped.embed("c").await.unwrap();
    props_mock.assert_hits(1);
}

/// A `llama-server` restarted on the same URL with another model or another
/// quantization after it was verified is caught by the next re-check, before
/// another vector is sent for it: here the interval is zero, so every embed
/// re-checks. The default interval is [`EG2_PROPS_RECHECK_INTERVAL`].
///
/// Mutation: ignore the kept answer's age in `check` -> red.
#[tokio::test]
async fn a_server_swapped_after_verification_is_caught_at_the_recheck() {
    assert_eq!(EG2_PROPS_RECHECK_INTERVAL, Duration::from_secs(60));
    for (file, ftype, needle) in [
        ("embeddinggemma-300M-Q8_0.gguf", "Q8_0", "does not name"),
        ("embeddinggemma-2-F16.gguf", "F16", "quantized as F16"),
    ] {
        let server = MockServer::start();
        let mut good = server.mock(|when, then| {
            when.method(GET).path("/props");
            then.status(200)
                .json_body(props("embeddinggemma-2-Q8_0.gguf", "Q8_0", true));
        });
        let post = server.mock(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(200).json_body(ok_body(&native(), Some(293)));
        });
        let e = embedder(&server).with_props_recheck(Duration::ZERO);
        e.embed("a").await.unwrap();
        e.embed("b").await.unwrap();
        good.assert_hits(2);
        post.assert_hits(2);

        good.delete();
        server.mock(|when, then| {
            when.method(GET).path("/props");
            then.status(200).json_body(props(file, ftype, true));
        });
        let err = e.embed("c").await.unwrap_err();
        assert!(matches!(err, EmbedError::Backend(_)), "{file}: {err:?}");
        assert!(err.to_string().contains(needle), "{file}: {err}");
        post.assert_hits(2);
    }
}

/// An embed that fails as unavailable (a refused connection, a loading
/// server: what a restart looks like) drops the kept answer, so the next
/// embed re-checks at once instead of waiting out the interval.
///
/// Mutation: drop `forget_kept` from `post` -> red (the swapped server is
/// used unchecked for up to a minute).
#[tokio::test]
async fn an_unavailable_embed_forces_a_recheck() {
    let server = MockServer::start();
    let mut good = server.mock(|when, then| {
        when.method(GET).path("/props");
        then.status(200)
            .json_body(props("embeddinggemma-2-Q8_0.gguf", "Q8_0", true));
    });
    let mut post = server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings");
        then.status(200).json_body(ok_body(&native(), Some(293)));
    });
    let e = embedder(&server);
    e.embed("a").await.unwrap();
    good.assert_hits(1);

    // The server goes down and comes back as BGE-M3 at 768 dims... or any
    // other model; the embed in between fails as unavailable.
    good.delete();
    let swapped = server.mock(|when, then| {
        when.method(GET).path("/props");
        then.status(200)
            .json_body(props("bge-m3-Q8_0.gguf", "Q8_0", false));
    });
    post.delete();
    let mut loading = server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings");
        then.status(503).body("Loading model");
    });
    let err = e.embed("b").await.unwrap_err();
    assert!(err.is_transient(), "{err:?}");
    swapped.assert_hits(0);
    loading.delete();
    let post = server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings");
        then.status(200).json_body(ok_body(&native(), Some(293)));
    });

    let err = e.embed("c").await.unwrap_err();
    assert!(matches!(err, EmbedError::Backend(_)), "{err:?}");
    assert!(err.to_string().contains("does not name"), "{err}");
    swapped.assert_hits(1);
    post.assert_hits(0);
}

/// A server whose decoder refuses Lambo's 1x1 reference PNG is reported as a
/// server or configuration problem naming the reference check, never as a
/// decode failure of the user's image, which is not sent (review L2).
///
/// Mutation: post the reference with `image_status_rule` and no wrapping ->
/// red (the message blames "this image").
#[tokio::test]
async fn a_refused_reference_image_is_a_server_problem() {
    for (status, body) in [
        (500, "Failed to load image or audio file"),
        (400, "bad request"),
    ] {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/props");
            then.status(200)
                .json_body(props("embeddinggemma-2-Q8_0.gguf", "Q8_0", true));
        });
        let reference = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/embeddings")
                .body(image_body(&png_1x1()));
            then.status(status).body(body);
        });
        let png = png_2x1();
        let image = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/embeddings")
                .body(sent_body(&png));
            then.status(200).json_body(ok_body(&native(), Some(260)));
        });
        let e = embedder(&server);
        let input = crate::surface::image::validate(&png, "image/png").unwrap();
        let err = e.embed_image(input).await.unwrap_err();
        assert!(matches!(err, EmbedError::Backend(_)), "{status}: {err:?}");
        let msg = err.to_string();
        assert!(msg.contains("reference image check"), "{msg}");
        assert!(
            msg.contains("not a fault in the image being embedded"),
            "{msg}"
        );
        assert!(!msg.contains("could not decode this image"), "{msg}");
        reference.assert_hits(1);
        image.assert_hits(0);
    }
}

/// A 503 "busy" from the verified server is transient but keeps the checks:
/// the retried image costs no extra `/props` GET and no extra reference
/// image embed (review L1). Only a request that got no HTTP answer, or
/// llama-server's 503 "Loading model", reads as a restart.
///
/// Mutation: call `forget_kept` for every `Unavailable` again -> red.
#[tokio::test]
async fn a_busy_server_keeps_its_checks_and_only_a_restart_drops_them() {
    let server = MockServer::start();
    let props_mock = server.mock(|when, then| {
        when.method(GET).path("/props");
        then.status(200)
            .json_body(props("embeddinggemma-2-Q8_0.gguf", "Q8_0", true));
    });
    let reference = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/embeddings")
            .body(image_body(&png_1x1()));
        then.status(200).json_body(ok_body(&native(), Some(293)));
    });
    let png = png_2x1();
    let mut image = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/embeddings")
            .body(sent_body(&png));
        then.status(200).json_body(ok_body(&native(), Some(260)));
    });
    let e = embedder(&server);
    let input = || crate::surface::image::validate(&png, "image/png").unwrap();
    e.embed_image(input()).await.unwrap();
    image.delete();
    let mut busy = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/embeddings")
            .body(sent_body(&png));
        then.status(503).body(
            r#"{"error":{"code":503,"message":"Server is busy","type":"unavailable_error"}}"#,
        );
    });
    let err = e.embed_image(input()).await.unwrap_err();
    assert!(err.is_transient(), "{err:?}");
    assert!(!restart_seen(&err), "{err}");
    busy.delete();
    server.mock(|when, then| {
        when.method(POST)
            .path("/v1/embeddings")
            .body(sent_body(&png));
        then.status(200).json_body(ok_body(&native(), Some(260)));
    });
    e.embed_image(input()).await.unwrap();
    props_mock.assert_hits(1);
    reference.assert_hits(1);

    // A connection that gets no HTTP answer (the server is gone) and the
    // loading 503 both read as a restart.
    // (httpmock pools its servers, so a dropped MockServer still answers;
    // take a free port and close it instead.)
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let dead = EmbeddingGemma2Embedder::new(format!("http://127.0.0.1:{port}"), MODEL, 768)
        .unwrap()
        .without_server_check();
    let err = dead.embed("a").await.unwrap_err();
    assert!(err.is_transient(), "{err:?}");
    assert!(restart_seen(&err), "{err}");
    assert!(restart_seen(&EmbedError::Unavailable(
        "llama.cpp is momentarily unwilling (503 Service Unavailable) for model \"m\": \
         {\"error\":{\"code\":503,\"message\":\"Loading model\"}}"
            .into()
    )));
}

/// Once a server has been verified, a re-check that cannot run holds embeds
/// back as transient (the write stays durable), and a server that no longer
/// reports its model is refused: either can be another server on the URL.
/// When the verified server answers again, embeds resume.
#[tokio::test]
async fn a_verified_server_that_stops_answering_props_is_held_back() {
    let server = MockServer::start();
    let good_props = props("embeddinggemma-2-Q8_0.gguf", "Q8_0", true);
    let mut good = server.mock(|when, then| {
        when.method(GET).path("/props");
        then.status(200).json_body(good_props.clone());
    });
    let post = server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings");
        then.status(200).json_body(ok_body(&native(), Some(293)));
    });
    let e = embedder(&server)
        .with_props_recheck(Duration::ZERO)
        .with_props_retry(Duration::ZERO);
    e.embed("a").await.unwrap();
    good.delete();

    let mut failing = server.mock(|when, then| {
        when.method(GET).path("/props");
        then.status(502).body("bad gateway");
    });
    let err = e.embed("b").await.unwrap_err();
    assert!(matches!(err, EmbedError::Unavailable(_)), "{err:?}");
    assert!(err.to_string().contains("re-check"), "{err}");
    failing.delete();

    let mut gone = server.mock(|when, then| {
        when.method(GET).path("/props");
        then.status(404).body("not found");
    });
    let err = e.embed("c").await.unwrap_err();
    assert!(matches!(err, EmbedError::Backend(_)), "{err:?}");
    assert!(
        err.to_string()
            .contains("verified as EmbeddingGemma 2 earlier"),
        "{err}"
    );
    gone.delete();
    post.assert_hits(1);

    server.mock(|when, then| {
        when.method(GET).path("/props");
        then.status(200).json_body(good_props);
    });
    e.embed("d").await.unwrap();
    post.assert_hits(2);
}

/// A `/props` that answers 5xx (a gateway that routes only the embeddings
/// path) or cannot be reached is not asked on every embed: after a failure
/// the next check waits, doubling from 1 s. The first failure of a run is
/// logged once, naming why; later failures in the same run are not logged.
///
/// Mutation: drop the `retry_at` early return in `check` -> red (a GET per
/// embed); log on every failure -> red.
#[tokio::test]
async fn a_props_check_that_cannot_run_backs_off_and_logs_once() {
    let server = MockServer::start();
    let props_mock = server.mock(|when, then| {
        when.method(GET).path("/props");
        then.status(502).body("bad gateway");
    });
    let post = server.mock(|when, then| {
        when.method(POST).path("/v1/embeddings");
        then.status(200).json_body(ok_body(&native(), None));
    });
    let (logs, _guard) = crate::test_util::capture_logs(tracing::Level::WARN);

    // The default wait: one GET for a burst of embeds.
    let e = embedder(&server);
    for text in ["a", "b", "c", "d"] {
        e.embed(text).await.unwrap();
    }
    props_mock.assert_hits(1);
    post.assert_hits(4);

    // With no wait every embed asks, and the run is still logged once.
    let eager = embedder(&server).with_props_retry(Duration::ZERO);
    for text in ["a", "b", "c"] {
        eager.embed(text).await.unwrap();
    }
    props_mock.assert_hits(4);
    let lines: Vec<String> = logs
        .lines()
        .into_iter()
        .filter(|l| l.contains("could not ask the embedder"))
        .collect();
    assert_eq!(lines.len(), 2, "one per embedder: {lines:?}");
    assert!(lines[0].contains("502"), "{lines:?}");
    assert!(lines[0].contains("unchecked"), "{lines:?}");

    // Unreachable is the same: a port nothing listens on.
    let dead = EmbeddingGemma2Embedder::new("http://127.0.0.1:9", MODEL, 768)
        .unwrap()
        .with_props_retry(Duration::from_secs(3600));
    assert_eq!(dead.check_server().await, Eg2ServerCheck::Skipped);
    assert!(logs.contains("unreachable"), "{}", logs.contents());
}

/// The bearer token goes on the `/props` request as well as on the embeds.
#[tokio::test]
async fn the_props_check_carries_the_bearer_token() {
    let server = MockServer::start();
    let props_mock = server.mock(|when, then| {
        when.method(GET)
            .path("/props")
            .header("authorization", "Bearer eg2-test-placeholder");
        then.status(200)
            .json_body(props("embeddinggemma-2-Q8_0.gguf", "Q8_0", true));
    });
    let post = server.mock(|when, then| {
        when.method(POST)
            .path("/v1/embeddings")
            .header("authorization", "Bearer eg2-test-placeholder");
        then.status(200).json_body(ok_body(&native(), None));
    });
    let e = embedder(&server)
        .with_bearer_token("eg2-test-placeholder")
        .unwrap();
    e.embed("x").await.unwrap();
    props_mock.assert_hits(1);
    post.assert_hits(1);
    assert!(!format!("{e:?}").contains("eg2-test-placeholder"));
}

#[test]
fn the_props_judge_reads_the_file_name_and_quantization() {
    let ok = Eg2ServerCheck::Verified { vision: None };
    // Case, separators and a Windows path all fold.
    for path in [
        "/m/embeddinggemma-2-Q8_0.gguf",
        "C:\\m\\EmbeddingGemma_2-q8_0.gguf",
        "embeddinggemma2.gguf",
    ] {
        assert_eq!(
            judge_props(MODEL, "u", path, Some("Q8_0"), None),
            ok,
            "{path}"
        );
    }
    // No quantization in the configured string: the file name alone decides.
    assert_eq!(
        judge_props(
            "google/embeddinggemma-2",
            "u",
            "/m/embeddinggemma-2-F16.gguf",
            Some("F16"),
            None
        ),
        ok
    );
    // No ftype reported: not judged on it.
    assert_eq!(
        judge_props(MODEL, "u", "/m/embeddinggemma-2.gguf", None, None),
        ok
    );
    assert_eq!(configured_quant(MODEL), Some("Q8_0"));
    assert_eq!(configured_quant("org/repo@rev/"), None);
    assert_eq!(configured_quant("embeddinggemma-2:440m"), None);
    assert_eq!(configured_quant("google/embeddinggemma-2"), None);
    // A long file name is cut in the message.
    let long = format!("/m/{}.gguf", "x".repeat(400));
    let Eg2ServerCheck::Mismatch(msg) = judge_props(MODEL, "u", &long, None, None) else {
        panic!("not EG2");
    };
    assert!(msg.len() < 900, "{}", msg.len());
}

/// A hostile `/props` (control characters, newlines, escapes, an endless
/// `model_ftype`) cannot forge log lines or flood a message: the file name
/// and the reported quantization are each cut to 128 characters of
/// printable ASCII (review L3).
///
/// Mutation: interpolate the raw `have` or file name again -> red.
#[test]
fn hostile_props_strings_are_bounded_and_sanitized() {
    let file = format!(
        "/m/embeddinggemma-2\n2026-10-10T00:00:00Z ERROR forged\r\x1b[31m{}.gguf",
        "y".repeat(400)
    );
    let ftype = format!("(guessed){}Q4_0\r\n", "\n".repeat(5000));
    let Eg2ServerCheck::Mismatch(msg) = judge_props(MODEL, "u", &file, Some(&ftype), None) else {
        panic!("a Q4_0 server under a Q8_0 artifact must be refused");
    };
    assert!(
        msg.chars().all(|c| c.is_ascii_graphic() || c == ' '),
        "{msg:?}"
    );
    assert!(
        msg.contains("embeddinggemma-2?2026-10-10T00:00:00Z ERROR forged??"),
        "{msg}"
    );
    assert!(!msg.contains(&"y".repeat(129)), "{}", msg.len());
    assert!(msg.len() < 1200, "{}", msg.len());
    // The not-EG2 message bounds the name the same way.
    let other = format!("/m/bge\n{}.gguf", "z".repeat(400));
    let Eg2ServerCheck::Mismatch(msg) = judge_props(MODEL, "u", &other, None, None) else {
        panic!("not EG2");
    };
    assert!(msg.contains("bge?zzz"), "{msg}");
    assert!(!msg.contains('\n'), "{msg:?}");
    assert!(!msg.contains(&"z".repeat(129)), "{}", msg.len());
}

/// llama.cpp reports `model_ftype` in its own names (`Q4_K - Medium`, `all
/// F32`, a `(guessed) ` prefix; b11517's table, and b11517 reports `Q8_0` for
/// the default GGUF, checked live), while an artifact names the file's token
/// (`Q4_K_M`, `F32`), possibly as a whole file name. Both reduce to the same
/// canonical token, so a correct server is verified and a wrong one refused;
/// a name either side cannot reduce is not judged.
///
/// Mutation: compare the raw strings again -> red (every K-quant, `all F32`
/// and `(guessed)` row).
#[test]
fn the_quantization_compare_reads_llama_cpp_names() {
    let ok = Eg2ServerCheck::Verified { vision: None };
    let file = "/m/embeddinggemma-2-x.gguf";
    for (configured, reported) in [
        ("org/eg2@rev/Q8_0", "Q8_0"),
        ("org/eg2@rev/Q8_0", "(guessed) Q8_0"),
        ("org/eg2@rev/Q8_0", "Q8_0 (guessed)"),
        ("org/eg2@rev/q8_0", "Q8_0"),
        ("org/eg2@rev/F16", "F16"),
        ("org/eg2@rev/BF16", "BF16"),
        ("org/eg2@rev/F32", "all F32"),
        ("org/eg2@rev/F32", "(guessed) all F32"),
        ("org/eg2@rev/Q4_K_M", "Q4_K - Medium"),
        ("org/eg2@rev/Q4_K_S", "Q4_K - Small"),
        ("org/eg2@rev/Q3_K_L", "Q3_K - Large"),
        ("org/eg2@rev/Q2_K", "Q2_K - Medium"),
        ("org/eg2@rev/Q6_K", "Q6_K"),
        ("org/eg2@rev/IQ4_XS", "IQ4_XS - 4.25 bpw"),
        ("org/eg2@rev/IQ3_M", "IQ3_S mix - 3.66 bpw"),
        ("org/eg2@rev/MXFP4_MOE", "MXFP4 MoE"),
        // The segment as a file name.
        ("org/eg2@rev/embeddinggemma-2-Q4_K_M.gguf", "Q4_K - Medium"),
        ("org/eg2@rev/embeddinggemma-2-Q8_0.GGUF", "Q8_0"),
        ("org/eg2@rev/embeddinggemma-2.F16.gguf", "F16"),
        // Not judged: a name llama.cpp does not know, or a segment that
        // names no quantization.
        ("org/eg2@rev/Q8_0", "unknown, may not work"),
        ("org/eg2@rev/Q8_0", "Q9_9 - Future"),
        ("org/eg2@rev/main", "Q8_0"),
        ("org/eg2@rev/embeddinggemma-2.gguf", "F16"),
    ] {
        assert_eq!(
            judge_props(configured, "u", file, Some(reported), None),
            ok,
            "{configured} vs {reported}"
        );
    }
    for (configured, reported) in [
        ("org/eg2@rev/Q8_0", "F16"),
        ("org/eg2@rev/Q8_0", "(guessed) BF16"),
        ("org/eg2@rev/F16", "all F32"),
        ("org/eg2@rev/Q4_K_M", "Q4_K - Small"),
        ("org/eg2@rev/Q2_K_S", "Q2_K - Medium"),
        ("org/eg2@rev/embeddinggemma-2-Q4_K_M.gguf", "Q8_0"),
    ] {
        let Eg2ServerCheck::Mismatch(msg) =
            judge_props(configured, "u", file, Some(reported), None)
        else {
            panic!("{configured} vs {reported} should be refused");
        };
        assert!(msg.contains(&format!("quantized as {reported}")), "{msg}");
    }
    assert_eq!(reported_quant("Q4_K - Medium"), Some("Q4_K_M"));
    assert_eq!(reported_quant(" (guessed) all F32 "), Some("F32"));
    assert_eq!(configured_quant("a@r/x-Q5_K_S.gguf"), Some("Q5_K_S"));
    assert_eq!(configured_quant("a@r/.gguf"), None);
}

// ---------------------------------------------------------------- config and contract

/// The kind parses, displays and serializes as `embeddinggemma2`, and its
/// contract `model` is the artifact plus the profile.
#[test]
fn the_kind_and_its_contract_string() {
    assert_eq!(
        "eg2".parse::<EmbedderKind>().unwrap(),
        EmbedderKind::EmbeddingGemma2
    );
    assert_eq!(EmbedderKind::EmbeddingGemma2.to_string(), "embeddinggemma2");
    assert!(EmbedderKind::EmbeddingGemma2.is_ready());
    let cfg: EmbedderConfig = toml::from_str(
        "kind = \"EmbeddingGemma-2\"\ndim = 768\nurl = \"http://127.0.0.1:8191\"\nimages = false\n",
    )
    .unwrap();
    assert_eq!(cfg.kind, EmbedderKind::EmbeddingGemma2);
    assert_eq!(cfg.images, Some(false));
    let shown = toml::to_string(&cfg).unwrap();
    assert!(shown.contains("kind = \"embeddinggemma2\""), "{shown}");

    let e = build_embedder(cfg.clone()).unwrap();
    assert_eq!(e.dimensions(), 768);
    assert_eq!(e.modalities(), Modalities::TEXT);
    assert_eq!(
        eg2_identity(e.as_ref()).as_deref(),
        Some("ggml-org/embeddinggemma-2-GGUF@bfcd2987/Q8_0;prompts=lambo-eg2-v2")
    );

    let custom = build_embedder(EmbedderConfig {
        llama_model: Some("google/embeddinggemma-2@914f7f89".into()),
        dim: 256,
        images: None,
        ..cfg.clone()
    })
    .unwrap();
    assert_eq!(custom.dimensions(), 256);
    assert_eq!(custom.modalities(), Modalities::TEXT | Modalities::IMAGE);
    assert_eq!(
        eg2_identity(custom.as_ref()).as_deref(),
        Some("google/embeddinggemma-2@914f7f89;prompts=lambo-eg2-v2")
    );
    // Not the EG2 adapter: no identity.
    #[cfg(feature = "embed-fixture")]
    assert_eq!(eg2_identity(&crate::embed::FixtureEmbedder::new()), None);
}

/// Build refusals: BGE's default width, a `;` in the artifact, `images` on
/// another kind; `api_key_env` is accepted for this kind with #21's transport
/// rule.
#[test]
fn build_refusals() {
    let base = EmbedderConfig {
        kind: EmbedderKind::EmbeddingGemma2,
        dim: 768,
        ..Default::default()
    };
    let err = build_embedder(EmbedderConfig {
        dim: 1024,
        ..base.clone()
    })
    .err()
    .unwrap();
    assert!(err.to_string().contains("dim = 768"), "{err}");

    let err = build_embedder(EmbedderConfig {
        llama_model: Some("m;prompts=other".into()),
        ..base.clone()
    })
    .err()
    .unwrap();
    assert!(err.to_string().contains("`;`"), "{err}");

    for kind in [EmbedderKind::BgeM3, EmbedderKind::Fixture] {
        let err = build_embedder(EmbedderConfig {
            kind,
            images: Some(true),
            ..Default::default()
        })
        .err()
        .unwrap();
        assert!(err.to_string().contains("embedder.images"), "{err}");
    }

    // api_key_env is this kind's too; the transport is checked before the
    // (unset) variable is read.
    let err = build_embedder(EmbedderConfig {
        llama_url: Some("http://embeddings.example.com:8191".into()),
        api_key_env: Some("LAMBO_TEST_EG2_NEVER_SET_TOKEN".into()),
        ..base.clone()
    })
    .err()
    .unwrap();
    let msg = err.to_string();
    assert!(
        msg.contains("embeddings.example.com") && !msg.contains("not set"),
        "{msg}"
    );
    let err = build_embedder(EmbedderConfig {
        llama_url: Some("http://127.0.0.1:8191".into()),
        api_key_env: Some("LAMBO_TEST_EG2_NEVER_SET_TOKEN".into()),
        ..base
    })
    .err()
    .unwrap();
    assert!(
        err.to_string().contains("LAMBO_TEST_EG2_NEVER_SET_TOKEN"),
        "{err}"
    );
}

/// Resolve stamps the EG2 contract: kind `embeddinggemma2`, the identity as
/// `model`, the MRL width as `dim`.
#[cfg(feature = "store-memory")]
#[test]
fn resolve_stamps_the_eg2_contract() {
    let file = crate::config::LamboFile::from_toml_str(
        "[store]\nkind = \"memory\"\n[embedder]\nkind = \"embeddinggemma2\"\ndim = 512\n\
         url = \"http://127.0.0.1:8191\"\n",
    )
    .unwrap();
    let r = crate::resolve::resolve_backends(file).unwrap();
    assert_eq!(r.embedding.kind, "embeddinggemma2");
    assert_eq!(
        r.embedding.model.as_deref(),
        Some("ggml-org/embeddinggemma-2-GGUF@bfcd2987/Q8_0;prompts=lambo-eg2-v2")
    );
    assert_eq!(r.embedding.dim, 512);
}
