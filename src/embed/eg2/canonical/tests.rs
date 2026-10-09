//! Unit tests for the `lambo-eg2-v2` canonical image form (22g).

use std::io::Cursor;

use image::{
    codecs::{jpeg::JpegEncoder, png::PngEncoder, webp::WebPEncoder},
    ColorType, DynamicImage, ImageEncoder, ImageFormat, Rgb, RgbImage, Rgba, RgbaImage,
};

use super::*;
use crate::surface::image::validate;

/// An RGB test picture whose pixels vary in both directions, so a resize
/// that swapped the axes or cropped would show.
fn picture(width: u32, height: u32) -> RgbImage {
    RgbImage::from_fn(width, height, |x, y| {
        Rgb([
            (x * 255 / width.max(1)) as u8,
            (y * 255 / height.max(1)) as u8,
            ((x / 8 + y / 8) % 2 * 200) as u8,
        ])
    })
}

fn png_rgb(width: u32, height: u32) -> Vec<u8> {
    let mut out = Vec::new();
    PngEncoder::new(&mut out)
        .write_image(
            picture(width, height).as_raw(),
            width,
            height,
            ColorType::Rgb8.into(),
        )
        .unwrap();
    out
}

fn jpeg(width: u32, height: u32) -> Vec<u8> {
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, 90)
        .write_image(
            picture(width, height).as_raw(),
            width,
            height,
            ColorType::Rgb8.into(),
        )
        .unwrap();
    out
}

fn webp(width: u32, height: u32) -> (Vec<u8>, RgbaImage) {
    let img = RgbaImage::from_fn(width, height, |x, y| {
        Rgba([(x % 251) as u8, (y % 241) as u8, 7, 255 - (x % 3) as u8])
    });
    let mut out = Vec::new();
    WebPEncoder::new_lossless(&mut out)
        .write_image(img.as_raw(), width, height, ColorType::Rgba8.into())
        .unwrap();
    (out, img)
}

fn decode_png(bytes: &[u8]) -> DynamicImage {
    assert_eq!(
        &bytes[..8],
        b"\x89PNG\r\n\x1a\n",
        "the canonical form is a PNG"
    );
    image::load_from_memory_with_format(bytes, ImageFormat::Png).unwrap()
}

fn canonical_of(bytes: &[u8], mime: &str) -> Result<Vec<u8>, EmbedError> {
    let input = validate(bytes, mime).unwrap();
    to_canonical_png(input.bytes(), input.mime())
}

/// The PNG CRC-32 (bitwise; tests only), so a header can be rewritten.
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

/// A small PNG whose IHDR is rewritten to declare `width` x `height` (with a
/// valid CRC): a decompression bomb's header in a few dozen bytes.
fn png_declaring(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = png_rgb(1, 1);
    // Signature (8), length (4), "IHDR" (4), then width and height.
    bytes[16..20].copy_from_slice(&width.to_be_bytes());
    bytes[20..24].copy_from_slice(&height.to_be_bytes());
    let crc = crc32(&bytes[12..29]);
    bytes[29..33].copy_from_slice(&crc.to_be_bytes());
    bytes
}

// ------------------------------------------------------------- the size rule

/// The longer side always becomes exactly 768 and the shorter keeps the
/// aspect ratio (nearest pixel, halves up, at least 1), whether the image
/// was larger or smaller.
///
/// Mutation: floor instead of round, drop the `max(1)`, or pass small
/// images through -> red.
#[test]
fn canonical_size_puts_the_longer_side_at_768() {
    assert_eq!(canonical_size(768, 768), (768, 768));
    assert_eq!(canonical_size(768, 1), (768, 1));
    assert_eq!(canonical_size(1, 1), (768, 768));
    assert_eq!(canonical_size(64, 64), (768, 768));
    assert_eq!(canonical_size(128, 64), (768, 384));
    // 700 -> 768 is x 1.0971...; 64 * 768 / 700 = 70.217 -> 70.
    assert_eq!(canonical_size(64, 700), (70, 768));
    assert_eq!(canonical_size(769, 769), (768, 768));
    assert_eq!(canonical_size(1536, 1024), (768, 512));
    assert_eq!(canonical_size(1024, 1536), (512, 768));
    assert_eq!(canonical_size(3000, 2000), (768, 512));
    // 333 * 768 / 1000 = 255.744 -> 256.
    assert_eq!(canonical_size(1000, 333), (768, 256));
    // 1001 * 768 / 3000 = 256.256 -> 256.
    assert_eq!(canonical_size(3000, 1001), (768, 256));
    // An exact half rounds up: 1 * 768 / 512 = 1.5 -> 2; 3 * 768 / 1536 = 1.5 -> 2.
    assert_eq!(canonical_size(512, 1), (768, 2));
    assert_eq!(canonical_size(1536, 3), (768, 2));
    // Just under a half rounds down: 1 * 768 / 1537 = 0.4997 -> 0 -> 1 (the floor).
    assert_eq!(canonical_size(1537, 1), (768, 1));
    // A sliver keeps at least one pixel, both ways.
    assert_eq!(canonical_size(4096, 1), (768, 1));
    assert_eq!(canonical_size(1, 4096), (1, 768));
    // Same aspect ratio at any size: the same canonical size.
    assert_eq!(canonical_size(1100, 600), canonical_size(2200, 1200));
    assert_eq!(canonical_size(110, 60), canonical_size(2200, 1200));
}

/// Every canonical size, for every shape the validator admits, takes the
/// server's round-up grid branch on b11517 (`calc_size_preserved_ratio`:
/// sides aligned to the nearest multiple of 48, at least 48, then compared
/// with the 280-token area of 645,120 px). Checked over a sweep of extreme
/// and ordinary aspect ratios.
#[test]
fn every_canonical_size_takes_the_round_up_grid_branch() {
    let align = |side: u32| -> u64 {
        // std::round(x / 48) * 48, with halves away from zero; at least 48.
        let r = (u64::from(side) + 24) / 48 * 48;
        r.max(48)
    };
    let max_pixels = 280u64 * 48 * 48;
    for long in [1u32, 2, 47, 48, 100, 767, 768, 769, 791, 792, 1000, 4096] {
        for short in [1u32, 2, 23, 24, 25, 100, 383, 384, 767, 768, 4096] {
            let short = short.min(long);
            for (w, h) in [(long, short), (short, long)] {
                let (cw, ch) = canonical_size(w, h);
                assert_eq!(cw.max(ch), 768, "{w}x{h}");
                assert!(cw.min(ch) >= 1, "{w}x{h}");
                assert!(
                    align(cw) * align(ch) < max_pixels,
                    "{w}x{h} -> {cw}x{ch} would take the round-down branch"
                );
            }
        }
    }
}

// ----------------------------------------------------------- every image

/// Every PNG, JPEG and WebP, at any size, becomes a PNG whose longer side is
/// exactly 768: small ones are upscaled, large ones downscaled.
///
/// Mutation: pass small images through, or skip the upscale -> red.
#[test]
fn every_image_becomes_a_768_png() {
    for ((w, h), want) in [
        ((1, 1), (768, 768)),
        ((64, 48), (768, 576)),
        ((200, 768), (200, 768)),
        ((768, 300), (768, 300)),
        ((769, 769), (768, 768)),
        ((1536, 1024), (768, 512)),
        ((2000, 330), (768, 127)),
    ] {
        for (bytes, mime) in [
            (png_rgb(w, h), "image/png"),
            (jpeg(w, h), "image/jpeg"),
            (webp(w, h).0, "image/webp"),
        ] {
            let out = canonical_of(&bytes, mime).unwrap();
            let img = decode_png(&out);
            assert_eq!((img.width(), img.height()), want, "{mime} {w}x{h}");
        }
    }
}

/// An image already 768 px on its longer side is decoded and re-encoded
/// but not resampled: the canonical PNG holds exactly its pixels.
#[test]
fn a_768_image_is_not_resampled() {
    let png = png_rgb(768, 500);
    let out = canonical_of(&png, "image/png").unwrap();
    assert_eq!(decode_png(&out).to_rgb8(), picture(768, 500));

    let (webp, pixels) = webp(300, 768);
    let out = canonical_of(&webp, "image/webp").unwrap();
    assert_eq!(
        decode_png(&out).to_rgba8(),
        pixels,
        "lossless: the same pixels"
    );
}

/// The resample is the documented one: the downscale filter above 768 px,
/// the upscale filter below, applied to the decoded image as a whole.
///
/// Mutation: swap the two filters (when they differ), or resize in two
/// steps -> red.
#[test]
fn the_resample_uses_the_filter_for_its_direction() {
    for ((w, h), filter) in [
        ((1536, 1000), DOWNSCALE_FILTER),
        ((100, 64), UPSCALE_FILTER),
    ] {
        let png = png_rgb(w, h);
        let out = canonical_of(&png, "image/png").unwrap();
        let (tw, th) = canonical_size(w, h);
        let want = DynamicImage::ImageRgb8(picture(w, h)).resize_exact(tw, th, filter);
        assert_eq!(decode_png(&out), want, "{w}x{h}");
    }
}

/// The downscale is a real resample, not a crop: the corners of the
/// gradient survive (top-left dark red/green, bottom-right bright).
#[test]
fn the_downscale_resamples_the_whole_picture() {
    let png = png_rgb(1536, 1536);
    let out = canonical_of(&png, "image/png").unwrap();
    let img = decode_png(&out).to_rgb8();
    let tl = img.get_pixel(0, 0);
    let br = img.get_pixel(767, 767);
    assert!(tl[0] < 10 && tl[1] < 10, "{tl:?}");
    assert!(br[0] > 245 && br[1] > 245, "{br:?}");
}

/// A flat image stays exactly flat through either resample, so its
/// canonical form, and its vector, is the same at every submitted size.
#[test]
fn a_flat_image_is_identical_at_every_size() {
    let flat = |side: u32| {
        let img = RgbImage::from_pixel(side, side, Rgb([200, 40, 40]));
        let mut out = Vec::new();
        PngEncoder::new(&mut out)
            .write_image(img.as_raw(), side, side, ColorType::Rgb8.into())
            .unwrap();
        out
    };
    let base = canonical_of(&flat(768), "image/png").unwrap();
    for side in [1, 16, 128, 500, 769, 1024, 3000] {
        assert_eq!(
            canonical_of(&flat(side), "image/png").unwrap(),
            base,
            "{side} px"
        );
    }
}

/// A large JPEG is decoded and sent as a 768 px PNG.
#[test]
fn a_large_jpeg_becomes_a_768_png() {
    let jpg = jpeg(1000, 3000);
    let img = decode_png(&canonical_of(&jpg, "image/jpeg").unwrap());
    assert_eq!((img.width(), img.height()), (256, 768));
}

/// Alpha and 16-bit depth are kept: the server treats the canonical PNG the
/// way it would have treated the original's pixels.
#[test]
fn alpha_and_sixteen_bit_survive_the_resample() {
    let mut rgba = Vec::new();
    let img = RgbaImage::from_fn(1000, 800, |x, y| Rgba([x as u8, y as u8, 3, 128]));
    PngEncoder::new(&mut rgba)
        .write_image(img.as_raw(), 1000, 800, ColorType::Rgba8.into())
        .unwrap();
    let decoded = decode_png(&canonical_of(&rgba, "image/png").unwrap());
    assert_eq!(decoded.color(), ColorType::Rgba8);
    assert_eq!((decoded.width(), decoded.height()), (768, 614));
    assert_eq!(decoded.to_rgba8().get_pixel(10, 10)[3], 128);

    for side in [900, 300] {
        let wide = DynamicImage::ImageRgb8(picture(side, side)).into_rgb16();
        let mut png16 = Vec::new();
        DynamicImage::ImageRgb16(wide)
            .write_to(Cursor::new(&mut png16), ImageFormat::Png)
            .unwrap();
        let out = decode_png(&canonical_of(&png16, "image/png").unwrap());
        assert_eq!(out.color(), ColorType::Rgb16, "{side} px");
    }
}

/// The canonical PNG carries no ancillary chunks: only IHDR, IDAT and IEND.
#[test]
fn the_canonical_png_has_only_critical_chunks() {
    let out = canonical_of(&png_rgb(1000, 700), "image/png").unwrap();
    let mut at = 8;
    let mut kinds = Vec::new();
    while at + 8 <= out.len() {
        let len = u32::from_be_bytes(out[at..at + 4].try_into().unwrap()) as usize;
        kinds.push(String::from_utf8_lossy(&out[at + 4..at + 8]).into_owned());
        at += 12 + len;
    }
    kinds.dedup();
    assert_eq!(kinds, ["IHDR", "IDAT", "IEND"]);
}

// ------------------------------------------------------------- bounded decode

/// A header declaring more than 4096 px a side (a decompression bomb in a
/// few dozen bytes) is refused from the header alone, before any decode.
/// `validate` refuses it first in production; this is the second line.
#[test]
fn a_bomb_header_is_refused_before_decoding() {
    for (w, h) in [(20_000, 20_000), (4097, 10), (10, 4097), (u32::MAX, 1)] {
        let bomb = png_declaring(w, h);
        assert!(
            validate(&bomb, "image/png").is_err(),
            "validate refuses it too"
        );
        let err = to_canonical_png(&bomb, ImageMime::Png).unwrap_err();
        let EmbedError::Backend(msg) = &err else {
            panic!("{err:?}");
        };
        // u32::MAX is not a legal PNG width, so the header parser refuses
        // it; the others are refused by the side limit.
        if w == u32::MAX {
            assert!(msg.contains("could not decode"), "{msg}");
        } else {
            assert!(
                msg.contains("4096 px a side") || msg.contains("limit"),
                "{msg}"
            );
        }
    }
    // Inside the side limit, a header that lies about its data fails as a
    // decode error, never a panic.
    let liar = png_declaring(4096, 4096);
    assert!(matches!(
        to_canonical_png(&liar, ImageMime::Png).unwrap_err(),
        EmbedError::Backend(m) if m.contains("could not decode")
    ));
}

/// The limits the decoder runs under are the validator's.
#[test]
fn the_decode_limits_match_the_validator() {
    let l = limits();
    assert_eq!(l.max_image_width, Some(MAX_IMAGE_SIDE_PX));
    assert_eq!(l.max_image_height, Some(MAX_IMAGE_SIDE_PX));
    assert_eq!(l.max_alloc, Some(4096 * 4096 * 8));
    assert!(check_dimensions(4096, 4096).is_ok());
    assert!(check_dimensions(4097, 1).is_err());
    assert!(check_dimensions(0, 1).is_err());
}

/// A truncated image (header fine, data cut) fails as a decode error
/// naming the format, never a panic, and never echoes bytes.
#[test]
fn a_truncated_large_image_is_a_backend_error() {
    let mut png = png_rgb(1000, 1000);
    png.truncate(png.len() / 2);
    let err = canonical_of(&png, "image/png").unwrap_err();
    assert!(
        matches!(&err, EmbedError::Backend(m) if m.starts_with("could not decode this image/png image")),
        "{err:?}"
    );
    let mut jpg = jpeg(1000, 1000);
    jpg.truncate(jpg.len() / 2);
    let err = canonical_of(&jpg, "image/jpeg");
    // zune-jpeg may fill a truncated scan with grey rather than fail; either
    // way there is no panic, and a success is a 768 px PNG.
    if let Ok(c) = err {
        let img = decode_png(&c);
        assert_eq!((img.width(), img.height()), (768, 768));
    }
}
