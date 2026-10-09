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

fn canonical_of<'a>(bytes: &'a [u8], mime: &str) -> Result<Canonical<'a>, EmbedError> {
    let input = validate(bytes, mime).unwrap();
    canonicalize(&input)
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

/// The longer side becomes exactly 768 and the shorter keeps the aspect
/// ratio (nearest pixel, at least 1); at or under 768 nothing changes.
///
/// Mutation: floor instead of round, or drop the `max(1)` -> red.
#[test]
fn canonical_size_keeps_the_aspect_ratio() {
    assert_eq!(canonical_size(768, 768), (768, 768));
    assert_eq!(canonical_size(768, 1), (768, 1));
    assert_eq!(canonical_size(64, 700), (64, 700));
    assert_eq!(canonical_size(769, 769), (768, 768));
    assert_eq!(canonical_size(1536, 1024), (768, 512));
    assert_eq!(canonical_size(1024, 1536), (512, 768));
    assert_eq!(canonical_size(3000, 2000), (768, 512));
    // 333 * 768 / 1000 = 255.744 -> 256.
    assert_eq!(canonical_size(1000, 333), (768, 256));
    // 2000 * 768 / 3000 = 512 exactly; 1001 * 768 / 3000 = 256.256 -> 256.
    assert_eq!(canonical_size(3000, 1001), (768, 256));
    // A sliver keeps at least one pixel.
    assert_eq!(canonical_size(4096, 1), (768, 1));
    assert_eq!(canonical_size(1, 4096), (1, 768));
    // Same aspect ratio at two sizes: the same canonical size.
    assert_eq!(canonical_size(1100, 600), canonical_size(2200, 1200));
}

// --------------------------------------------------------------- pass-through

/// A PNG or JPEG at or under 768 px a side is sent as it came, byte for
/// byte, with its own MIME type: nothing is decoded or re-encoded.
///
/// Mutation: always re-encode -> red.
#[test]
fn small_png_and_jpeg_pass_through_byte_identical() {
    for (w, h) in [(1, 1), (64, 64), (768, 768), (768, 300), (200, 768)] {
        let png = png_rgb(w, h);
        match canonical_of(&png, "image/png").unwrap() {
            Canonical::Original(bytes, mime) => {
                assert_eq!(bytes, png.as_slice(), "{w}x{h} png");
                assert!(std::ptr::eq(bytes, png.as_slice()), "not even copied");
                assert_eq!(mime, ImageMime::Png);
            }
            other => panic!("{w}x{h} png was re-encoded: {:?}", other.mime()),
        }
        let jpg = jpeg(w, h);
        match canonical_of(&jpg, "image/jpeg").unwrap() {
            Canonical::Original(bytes, mime) => {
                assert_eq!(bytes, jpg.as_slice(), "{w}x{h} jpeg");
                assert_eq!(mime, ImageMime::Jpeg);
            }
            other => panic!("{w}x{h} jpeg was re-encoded: {:?}", other.mime()),
        }
    }
}

// ------------------------------------------------------------------ downscale

/// A PNG over 768 px is downscaled to a 768 px longer side, aspect ratio
/// kept, and sent as a PNG in its own colour type.
#[test]
fn a_large_png_is_downscaled_to_768() {
    for ((w, h), want) in [
        ((769, 769), (768, 768)),
        ((1536, 1024), (768, 512)),
        ((1024, 1536), (512, 768)),
        ((2000, 330), (768, 127)),
    ] {
        let png = png_rgb(w, h);
        let canonical = canonical_of(&png, "image/png").unwrap();
        assert_eq!(canonical.mime(), ImageMime::Png);
        assert!(
            matches!(canonical, Canonical::Png(_)),
            "{w}x{h} was not canonicalized"
        );
        let img = decode_png(canonical.bytes());
        assert_eq!((img.width(), img.height()), want, "{w}x{h}");
        assert_eq!(img.color(), ColorType::Rgb8);
    }
}

/// The downscale is a real resample, not a crop: the corners of the
/// gradient survive (top-left dark red/green, bottom-right bright).
#[test]
fn the_downscale_resamples_the_whole_picture() {
    let png = png_rgb(1536, 1536);
    let Canonical::Png(out) = canonical_of(&png, "image/png").unwrap() else {
        panic!("not canonicalized");
    };
    let img = decode_png(&out).to_rgb8();
    let tl = img.get_pixel(0, 0);
    let br = img.get_pixel(767, 767);
    assert!(tl[0] < 10 && tl[1] < 10, "{tl:?}");
    assert!(br[0] > 245 && br[1] > 245, "{br:?}");
}

/// A JPEG over 768 px is decoded and sent as a 768 px PNG.
#[test]
fn a_large_jpeg_becomes_a_768_png() {
    let jpg = jpeg(1000, 3000);
    let canonical = canonical_of(&jpg, "image/jpeg").unwrap();
    assert_eq!(canonical.mime(), ImageMime::Png);
    let img = decode_png(canonical.bytes());
    assert_eq!((img.width(), img.height()), (256, 768));
}

/// Alpha and 16-bit depth are kept: the server treats the canonical PNG the
/// way it would have treated the original's pixels.
#[test]
fn alpha_and_sixteen_bit_survive_the_downscale() {
    let mut rgba = Vec::new();
    let img = RgbaImage::from_fn(1000, 800, |x, y| Rgba([x as u8, y as u8, 3, 128]));
    PngEncoder::new(&mut rgba)
        .write_image(img.as_raw(), 1000, 800, ColorType::Rgba8.into())
        .unwrap();
    let out = canonical_of(&rgba, "image/png").unwrap();
    let decoded = decode_png(out.bytes());
    assert_eq!(decoded.color(), ColorType::Rgba8);
    assert_eq!((decoded.width(), decoded.height()), (768, 614));
    assert_eq!(decoded.to_rgba8().get_pixel(10, 10)[3], 128);

    let wide = DynamicImage::ImageRgb8(picture(900, 900)).into_rgb16();
    let mut png16 = Vec::new();
    DynamicImage::ImageRgb16(wide)
        .write_to(Cursor::new(&mut png16), ImageFormat::Png)
        .unwrap();
    let out = canonical_of(&png16, "image/png").unwrap();
    assert_eq!(decode_png(out.bytes()).color(), ColorType::Rgb16);
}

/// Canonicalizing is deterministic: the same input gives the same bytes.
#[test]
fn the_canonical_form_is_deterministic() {
    let png = png_rgb(1200, 900);
    let a = canonical_of(&png, "image/png").unwrap();
    let b = canonical_of(&png, "image/png").unwrap();
    assert_eq!(a, b);
}

// ----------------------------------------------------------------------- WebP

/// Any WebP, even a small one, is decoded and sent as a lossless PNG of the
/// same pixels; a large one is downscaled too.
///
/// Mutation: let a small WebP pass through -> red.
#[test]
fn every_webp_becomes_a_png() {
    let (small, pixels) = webp(64, 48);
    let canonical = canonical_of(&small, "image/webp").unwrap();
    assert_eq!(canonical.mime(), ImageMime::Png);
    let img = decode_png(canonical.bytes());
    assert_eq!((img.width(), img.height()), (64, 48));
    assert_eq!(img.to_rgba8(), pixels, "lossless: the same pixels");

    let (large, _) = webp(2000, 1000);
    let canonical = canonical_of(&large, "image/webp").unwrap();
    let img = decode_png(canonical.bytes());
    assert_eq!((img.width(), img.height()), (768, 384));
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
        let input = ImageInput::from_validated(&bomb, ImageMime::Png, [0; 32]);
        let err = canonicalize(&input).unwrap_err();
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
    let input = ImageInput::from_validated(&liar, ImageMime::Png, [0; 32]);
    assert!(matches!(
        canonicalize(&input).unwrap_err(),
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

/// A truncated large image (header fine, data cut) fails as a decode error
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
        let img = decode_png(c.bytes());
        assert_eq!((img.width(), img.height()), (768, 768));
    }
}
