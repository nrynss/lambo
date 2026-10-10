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

/// Lambo's own size check refuses a header declaring more than 4096 px a
/// side (a decompression bomb in a few dozen bytes) from the header alone, before any decode and with its own message. `validate`
/// refuses these first in production; this is the second line.
///
/// Mutation: remove `check_dimensions` -> red (the header read itself does
/// not apply the side limits).
#[test]
fn lambos_size_check_refuses_a_bomb_header() {
    for (w, h) in [(20_000, 20_000), (4097, 10), (10, 4097)] {
        let bomb = png_declaring(w, h);
        assert!(
            validate(&bomb, "image/png").is_err(),
            "validate refuses it too"
        );
        for err in [
            dimensions(&bomb, ImageMime::Png).unwrap_err(),
            to_canonical_png(&bomb, ImageMime::Png).unwrap_err(),
        ] {
            let EmbedError::Unreadable(msg) = &err else {
                panic!("{w}x{h}: {err:?}");
            };
            assert!(
                msg.contains(&format!("declares {w}x{h} px"))
                    && msg.contains("1 to 4096 px a side"),
                "{w}x{h}: {msg}"
            );
        }
    }
    // u32::MAX is not a legal PNG width: the header parser refuses it.
    let err = to_canonical_png(&png_declaring(u32::MAX, 1), ImageMime::Png).unwrap_err();
    assert!(
        matches!(&err, EmbedError::Unreadable(m) if m.contains("could not decode")),
        "{err:?}"
    );
    // Inside the side limit, a header that lies about its data fails as a
    // decode error, never a panic.
    let liar = png_declaring(4096, 4096);
    assert!(matches!(
        to_canonical_png(&liar, ImageMime::Png).unwrap_err(),
        EmbedError::Unreadable(m) if m.contains("could not decode")
    ));
}

/// The decoder's own limits refuse an over-limit header even with Lambo's
/// size check out of the way. 4097 x 10 RGB is far under the `image`
/// crate's default allocation limit, so only Lambo's limits refuse it.
///
/// Mutation: drop `reader.limits(limits())` -> red (the decode then fails
/// on the missing data, not on a limit).
#[test]
fn the_decoders_limits_refuse_a_bomb_header() {
    for (w, h) in [(4097, 10), (10, 4097)] {
        let bomb = png_declaring(w, h);
        let err = reader(&bomb, ImageFormat::Png).decode().unwrap_err();
        assert!(
            matches!(err, image::ImageError::Limits(_)),
            "{w}x{h}: {err:?}"
        );
    }
}

/// The limits the decoder runs under are the validator's.
#[test]
fn the_decode_limits_match_the_validator() {
    let l = limits();
    assert_eq!(l.max_image_width, Some(MAX_IMAGE_SIDE_PX));
    assert_eq!(l.max_image_height, Some(MAX_IMAGE_SIDE_PX));
    assert_eq!(l.max_alloc, Some(4096 * 4096 * 8 + 64 * 1024 * 1024));
    assert!(check_dimensions(4096, 4096).is_ok());
    assert!(check_dimensions(4097, 1).is_err());
    assert!(check_dimensions(0, 1).is_err());
}

/// A truncated image (header fine, data cut) fails as unreadable
/// naming the format, never a panic, and never echoes bytes.
#[test]
fn a_truncated_large_image_is_unreadable() {
    let mut png = png_rgb(1000, 1000);
    png.truncate(png.len() / 2);
    let err = canonical_of(&png, "image/png").unwrap_err();
    assert!(
        matches!(&err, EmbedError::Unreadable(m) if m.starts_with("could not decode this image/png image")),
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

// ------------------------------------------------------- concurrent decodes

/// A large PNG that takes a while to decode and resample in a test build.
fn slow_png() -> Vec<u8> {
    png_rgb(2048, 2048)
}

/// A decode runs only with a permit: while every permit is held, a request
/// waits rather than decoding, and goes ahead once one is released.
///
/// Mutation: skip the `acquire` -> red.
#[tokio::test]
async fn a_decode_waits_for_a_permit() {
    static LIMIT: Semaphore = Semaphore::const_new(1);
    let png = png_rgb(64, 64);
    let held = LIMIT.acquire().await.unwrap();
    let waited = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        canonicalize_with(&LIMIT, &png, ImageMime::Png),
    )
    .await;
    assert!(waited.is_err(), "decoded without a permit");
    drop(held);
    let out = canonicalize_with(&LIMIT, &png, ImageMime::Png)
        .await
        .unwrap();
    assert_eq!(out, to_canonical_png(&png, ImageMime::Png).unwrap());
    assert_eq!(LIMIT.available_permits(), 1, "the permit came back");
}

/// A request that times out while its decode runs keeps the permit until
/// the decode finishes, so a retry cannot start a second decode beyond the
/// bound.
///
/// Mutation: release the permit in the async caller (drop it before or
/// after `spawn_blocking` returns) -> red.
#[tokio::test]
async fn a_timed_out_decode_holds_its_permit_until_it_finishes() {
    static LIMIT: Semaphore = Semaphore::const_new(1);
    let png = slow_png();
    let started = std::time::Instant::now();
    let timed_out = tokio::time::timeout(
        std::time::Duration::from_millis(5),
        canonicalize_with(&LIMIT, &png, ImageMime::Png),
    )
    .await;
    assert!(timed_out.is_err(), "the decode finished within 5 ms");
    assert_eq!(
        LIMIT.available_permits(),
        0,
        "the abandoned decode still holds its permit"
    );
    // A retry waits for it rather than running beside it.
    let retry = canonicalize_with(&LIMIT, &png, ImageMime::Png)
        .await
        .unwrap();
    assert_eq!(decode_png(&retry).width(), 768);
    assert_eq!(LIMIT.available_permits(), 1);
    assert!(started.elapsed() < std::time::Duration::from_secs(120));
}

/// The production bound is the documented one.
#[test]
fn the_process_wide_bound_is_two() {
    assert_eq!(MAX_CONCURRENT_DECODES, 2);
    assert!(DECODES.available_permits() <= MAX_CONCURRENT_DECODES);
}

/// An image at the exact cap, the widest pixel type at the largest size the
/// validator admits (a flat 4096 px square of 16-bit RGBA, 128 MiB
/// decoded), decodes under the limits and becomes a 768 px PNG.
///
/// Mutation: drop the headroom and have the codec count a scratch buffer,
/// or lower the cap below 16-bit RGBA -> red.
#[test]
fn an_image_at_the_exact_cap_decodes() {
    let side = MAX_IMAGE_SIDE_PX;
    let flat = image::ImageBuffer::<Rgba<u16>, Vec<u16>>::from_pixel(
        side,
        side,
        Rgba([40_000, 1_000, 20_000, 65_535]),
    );
    let mut png = Vec::new();
    DynamicImage::ImageRgba16(flat)
        .write_with_encoder(PngEncoder::new_with_quality(
            &mut png,
            CompressionType::Fast,
            PngFilter::Adaptive,
        ))
        .unwrap();
    assert!(
        png.len() < crate::surface::image::MAX_IMAGE_BYTES,
        "{}",
        png.len()
    );
    let out = canonical_of(&png, "image/png").unwrap();
    let img = decode_png(&out);
    assert_eq!((img.width(), img.height()), (768, 768));
    assert_eq!(img.color(), ColorType::Rgba16);
}

// ---------------------------------------------------------------- formats

/// A tiny CMYK JPEG (24x16, Adobe APP14 marker), made with Pillow; the
/// `image` crate cannot encode CMYK.
const CMYK_JPEG: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fixtures/images/cmyk-24x16.jpg"
));

/// A zlib stream of `raw` in stored (uncompressed) deflate blocks.
fn zlib_stored(raw: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut chunks = raw.chunks(65_535).peekable();
    if chunks.peek().is_none() {
        out.extend_from_slice(&[1, 0, 0, 0xFF, 0xFF]);
    }
    while let Some(block) = chunks.next() {
        out.push(u8::from(chunks.peek().is_none()));
        let len = u16::try_from(block.len()).unwrap();
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in raw {
        a = (a + u32::from(x)) % 65_521;
        b = (b + a) % 65_521;
    }
    out.extend_from_slice(&((b << 16) | a).to_be_bytes());
    out
}

fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// An 8-bit palette PNG of `width` x `height` with four colours, the second
/// fully transparent and the third half transparent through `tRNS`.
fn palette_png(width: u32, height: u32) -> Vec<u8> {
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 3, 0, 0, 0]);
    png_chunk(&mut out, b"IHDR", &ihdr);
    png_chunk(
        &mut out,
        b"PLTE",
        &[255, 0, 0, 0, 255, 0, 0, 0, 255, 250, 250, 250],
    );
    png_chunk(&mut out, b"tRNS", &[255, 0, 128]);
    let mut raw = Vec::new();
    for y in 0..height {
        raw.push(0);
        for x in 0..width {
            raw.push(((x / 3 + y / 2) % 4) as u8);
        }
    }
    png_chunk(&mut out, b"IDAT", &zlib_stored(&raw));
    png_chunk(&mut out, b"IEND", &[]);
    out
}

/// An 8-bit greyscale PNG.
fn grey_png(width: u32, height: u32) -> Vec<u8> {
    let img = image::GrayImage::from_fn(width, height, |x, y| {
        image::Luma([((x * 7 + y * 3) % 256) as u8])
    });
    let mut out = Vec::new();
    PngEncoder::new(&mut out)
        .write_image(img.as_raw(), width, height, ColorType::L8.into())
        .unwrap();
    out
}

/// A JPEG whose EXIF says "rotate 90 degrees clockwise" (orientation 6).
fn jpeg_with_orientation_6(width: u32, height: u32) -> Vec<u8> {
    // Big-endian TIFF header, one IFD entry: 0x0112 Orientation, SHORT, 1, 6.
    let exif = vec![
        b'M', b'M', 0, 42, 0, 0, 0, 8, // header, IFD at 8
        0, 1, // one entry
        0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, 6, 0, 0, // orientation = 6
        0, 0, 0, 0, // no next IFD
    ];
    let mut out = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut out, 90);
    encoder.set_exif_metadata(exif).unwrap();
    encoder
        .write_image(
            picture(width, height).as_raw(),
            width,
            height,
            ColorType::Rgb8.into(),
        )
        .unwrap();
    out
}

/// A CMYK JPEG is converted to RGB by the decoder and canonicalized like
/// any other.
#[test]
fn a_cmyk_jpeg_becomes_an_rgb_png() {
    let input = validate(CMYK_JPEG, "image/jpeg").expect("the validator accepts CMYK");
    let out = to_canonical_png(input.bytes(), input.mime()).unwrap();
    let img = decode_png(&out);
    assert_eq!((img.width(), img.height()), (768, 512));
    assert_eq!(img.color(), ColorType::Rgb8);
}

/// A greyscale PNG stays greyscale (one channel): the server expands it to
/// RGB itself, as it would have the original.
#[test]
fn a_greyscale_png_stays_greyscale() {
    let img = decode_png(&canonical_of(&grey_png(100, 60), "image/png").unwrap());
    assert_eq!((img.width(), img.height()), (768, 461));
    assert_eq!(img.color(), ColorType::L8);
}

/// A palette PNG with `tRNS` is expanded to RGBA, the transparency kept as
/// alpha, before it is resampled.
#[test]
fn a_palette_png_with_trns_becomes_rgba() {
    let png = palette_png(48, 24);
    let small = image::load_from_memory_with_format(&png, ImageFormat::Png).unwrap();
    assert_eq!(small.color(), ColorType::Rgba8, "the decoder expands it");
    let img = decode_png(&canonical_of(&png, "image/png").unwrap());
    assert_eq!((img.width(), img.height()), (768, 384));
    assert_eq!(img.color(), ColorType::Rgba8);
    // Index 0 (opaque red) at the top-left corner, still opaque red.
    assert_eq!(img.to_rgba8().get_pixel(0, 0).0, [255, 0, 0, 255]);
    let alphas: std::collections::BTreeSet<u8> = img.to_rgba8().pixels().map(|p| p[3]).collect();
    assert!(alphas.contains(&0) && alphas.contains(&255), "{alphas:?}");
}

/// EXIF orientation is not applied: a JPEG tagged "rotate 90" keeps its
/// stored width and height, as `llama-server`'s decoder would read it.
///
/// Mutation: apply the orientation -> red.
#[test]
fn exif_orientation_is_ignored() {
    use image::ImageDecoder;
    let jpg = jpeg_with_orientation_6(400, 200);
    let mut decoder = image::ImageReader::with_format(Cursor::new(&jpg[..]), ImageFormat::Jpeg)
        .into_decoder()
        .unwrap();
    assert_eq!(
        decoder.orientation().unwrap(),
        image::metadata::Orientation::Rotate90,
        "the fixture carries the tag"
    );
    let img = decode_png(&canonical_of(&jpg, "image/jpeg").unwrap());
    assert_eq!((img.width(), img.height()), (768, 384), "not rotated");
}

// ----------------------------------------------------------------- goldens

fn sha_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The SHA-256 of a decoded image: width, height, colour type and raw
/// samples (native-endian for 16 bits).
fn pixels_sha(img: &DynamicImage) -> String {
    let mut px = Vec::new();
    px.extend_from_slice(&img.width().to_be_bytes());
    px.extend_from_slice(&img.height().to_be_bytes());
    px.extend_from_slice(format!("{:?}", img.color()).as_bytes());
    px.extend_from_slice(img.as_bytes());
    sha_hex(&px)
}

/// Every sample of an image as an integer, in its own units (0..=255 for 8
/// bits, 0..=65535 for 16).
fn samples(img: &DynamicImage) -> Vec<i32> {
    let color = img.color();
    if color.bytes_per_pixel() == 2 * color.channel_count() {
        img.as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| i32::from(u16::from_ne_bytes(c)))
            .collect()
    } else {
        img.as_bytes().iter().map(|&s| i32::from(s)).collect()
    }
}

/// A `Near` reference keeps every `GRID`-th pixel of every `GRID`-th row,
/// starting at (0, 0): exact samples, not an average, so the one-step
/// tolerance still applies sample by sample, at a sixteenth of the size.
const GRID: u32 = 4;

fn grid_of<P: image::Pixel>(
    b: &image::ImageBuffer<P, Vec<P::Subpixel>>,
) -> image::ImageBuffer<P, Vec<P::Subpixel>> {
    image::ImageBuffer::from_fn(
        b.width().div_ceil(GRID),
        b.height().div_ceil(GRID),
        |x, y| *b.get_pixel(x * GRID, y * GRID),
    )
}

/// The [`GRID`] subsample of a canonical image.
fn grid(img: &DynamicImage) -> DynamicImage {
    match img {
        DynamicImage::ImageLuma8(b) => DynamicImage::ImageLuma8(grid_of(b)),
        DynamicImage::ImageLumaA8(b) => DynamicImage::ImageLumaA8(grid_of(b)),
        DynamicImage::ImageRgb8(b) => DynamicImage::ImageRgb8(grid_of(b)),
        DynamicImage::ImageRgba8(b) => DynamicImage::ImageRgba8(grid_of(b)),
        DynamicImage::ImageLuma16(b) => DynamicImage::ImageLuma16(grid_of(b)),
        DynamicImage::ImageLumaA16(b) => DynamicImage::ImageLumaA16(grid_of(b)),
        DynamicImage::ImageRgb16(b) => DynamicImage::ImageRgb16(grid_of(b)),
        DynamicImage::ImageRgba16(b) => DynamicImage::ImageRgba16(grid_of(b)),
        other => panic!("no canonical PNG is {:?}", other.color()),
    }
}

/// How a golden case is pinned.
enum Pin {
    /// Bit-exact on every platform: the canonical PNG bytes and pixels.
    /// For cases whose arithmetic uses no platform `libm` and no run-time
    /// SIMD choice: a PNG or lossless WebP decode (integer) and a
    /// Catmull-Rom upscale (a polynomial kernel; IEEE `f32` add and
    /// multiply give the same bits everywhere).
    Exact {
        png: &'static str,
        pixels: &'static str,
    },
    /// Within one step per sample of a committed reference. For cases that
    /// are not bit-exact across platforms: a Lanczos3 downscale (its
    /// weights call `f32::sin`, which is the platform `libm`, so one weight
    /// may differ by an ulp and flip the rounding of a sample) and any JPEG
    /// (`zune-jpeg` picks an AVX2, NEON or scalar IDCT and colour conversion
    /// at run time).
    ///
    /// The canonical image must have exactly `size` and the reference's
    /// colour type, and its [`GRID`] subsample must be within one step per
    /// sample of `reference` (a lossless PNG under `fixtures/images/`),
    /// with at most [`NEAR_MAX_DIFFERING`] of the samples differing at all.
    /// `reference_pixels` pins the reference file (as [`pixels_sha`]), so
    /// it cannot be swapped silently. `full_pixels` is the full image's
    /// pixel hash on the platform that made the reference (aarch64 macOS):
    /// a match is reported, a mismatch alone is not a failure.
    Near {
        size: (u32, u32),
        reference: &'static str,
        reference_pixels: &'static str,
        full_pixels: &'static str,
    },
}

/// Most samples of a `Near` case that may differ from the reference, as a
/// fraction. A libm or SIMD wobble flips a rounding here and there (an ulp
/// in one weight moves a sample by about 1.5e-5, so roughly one sample in
/// 70,000); a decoder or resampler change that moves the picture by one
/// step nearly everywhere is a real change and fails here.
const NEAR_MAX_DIFFERING: f64 = 0.01;

/// Where the `Near` references live.
fn reference_path(file: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/images")
        .join(file)
}

/// Compare a `Near` case against its reference. `Err` names what moved.
fn check_near(name: &str, got: &DynamicImage, got_full: &str, pin: &Pin) -> Result<(), String> {
    let Pin::Near {
        size,
        reference,
        reference_pixels,
        full_pixels,
    } = pin
    else {
        unreachable!("{name} is pinned exactly")
    };
    let path = reference_path(reference);
    let got_grid = grid(got);
    if std::env::var_os("LAMBO_EG2_WRITE_GOLDEN_REFS").is_some() {
        // Re-pin helper: write this platform's output as the reference.
        // Only after a profile bump (see the test's doc comment).
        let mut out = Vec::new();
        got_grid
            .write_with_encoder(PngEncoder::new_with_quality(
                &mut out,
                CompressionType::Best,
                PngFilter::Adaptive,
            ))
            .unwrap();
        std::fs::write(&path, out).unwrap();
    }
    let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let want = decode_png(&bytes);
    let want_sha = pixels_sha(&want);
    if want_sha != *reference_pixels {
        return Err(format!(
            "{name}: reference {reference} is not the pinned one: {want_sha} \
             (pinned {reference_pixels})"
        ));
    }
    if ((got.width(), got.height()), got.color()) != (*size, want.color()) {
        return Err(format!(
            "{name}: {}x{} {:?}, pinned {}x{} {:?}",
            got.width(),
            got.height(),
            got.color(),
            size.0,
            size.1,
            want.color()
        ));
    }
    let (g, w) = (samples(&got_grid), samples(&want));
    assert_eq!(g.len(), w.len(), "{name}: the grid follows the size");
    let max = g
        .iter()
        .zip(&w)
        .map(|(a, b)| (a - b).abs())
        .max()
        .unwrap_or(0);
    let differing = g.iter().zip(&w).filter(|(a, b)| a != b).count();
    let limit = (w.len() as f64 * NEAR_MAX_DIFFERING) as usize;
    eprintln!(
        "{name}: {differing} of {} grid samples differ from the reference, max by {max}; \
         full image bit-exact with the reference platform: {}",
        w.len(),
        got_full == *full_pixels
    );
    if max > 1 || differing > limit {
        return Err(format!(
            "{name}: {differing} of {} grid samples differ from {reference} (limit {limit}), \
             max by {max} (limit 1)",
            w.len()
        ));
    }
    Ok(())
}

/// **The canonical form is pinned.** For fixed inputs: the SHA-256 of the
/// input, and either ([`Pin::Exact`]) the SHA-256 of the canonical PNG bytes
/// and pixels, or ([`Pin::Near`]) the canonical pixels to within one step
/// per sample of a committed reference.
///
/// **A wobble of at most one step per sample is not a change.** The
/// downscale and JPEG cases are not bit-exact across platforms (macOS
/// against Linux CI, NEON against AVX2), and a one-step difference moves
/// the vector by about 1e-7. It needs no profile bump and no re-pin.
///
/// If this test fails after a dependency update (`image`, `png`,
/// `zune-jpeg`, `image-webp`, `fdeflate`, ...):
///
/// - **pixels changed** (an exact pixel hash moved, or a `Near` case is
///   more than one step from its reference anywhere, or in more than
///   [`NEAR_MAX_DIFFERING`] of its samples): the canonical image is
///   different, so vectors are different. That is a new prompt profile:
///   bump `EG2_PROMPT_PROFILE` (`lambo-eg2-v3`), note it in the CHANGELOG,
///   then re-pin (`LAMBO_EG2_WRITE_GOLDEN_REFS=1` rewrites the `Near`
///   references; copy the hashes the failure prints). Never re-pin under
///   the same profile name.
/// - **only the PNG bytes changed** (pixels equal): the encoder compresses
///   differently. The server decodes the same pixels, so vectors are
///   unchanged; re-pin the PNG golden without a profile bump.
/// - **an input changed**: the in-test generator (an encoder) moved; this
///   says nothing about the canonical form. Re-pin the input and check the
///   rest against a run on the previous version.
#[test]
fn the_canonical_form_matches_its_golden() {
    let mut rgba16 = Vec::new();
    DynamicImage::ImageRgba16(image::ImageBuffer::from_fn(900, 700, |x, y| {
        Rgba([
            (x * 73) as u16,
            (y * 91) as u16,
            ((x ^ y) * 37) as u16,
            50_000,
        ])
    }))
    .write_with_encoder(PngEncoder::new_with_quality(
        &mut rgba16,
        CompressionType::Fast,
        PngFilter::Adaptive,
    ))
    .unwrap();
    let inputs: [(&str, Vec<u8>, &str); 8] = [
        ("png rgb 1536x1024", png_rgb(1536, 1024), "image/png"),
        ("jpeg 1000x3000", jpeg(1000, 3000), "image/jpeg"),
        ("webp rgba 64x48", webp(64, 48).0, "image/webp"),
        ("webp rgba 2000x1000", webp(2000, 1000).0, "image/webp"),
        ("png rgba16 900x700", rgba16, "image/png"),
        ("png grey 100x60", grey_png(100, 60), "image/png"),
        ("png palette+tRNS 48x24", palette_png(48, 24), "image/png"),
        ("jpeg cmyk 24x16", CMYK_JPEG.to_vec(), "image/jpeg"),
    ];
    let mut report = String::new();
    let mut ok = true;
    for ((name, input, mime), (want_in, pin)) in inputs.iter().zip(&GOLDEN) {
        let out = canonical_of(input, mime).unwrap();
        // Same input, same output, run to run.
        assert_eq!(out, canonical_of(input, mime).unwrap(), "{name}");
        let img = decode_png(&out);
        let got_in = sha_hex(input);
        let (got_png, got_px) = (sha_hex(&out), pixels_sha(&img));
        report.push_str(&format!(
            "    {name}: input {got_in}, png {got_png}, pixels {got_px}\n"
        ));
        if got_in != *want_in {
            ok = false;
            eprintln!("{name}: input golden moved: {got_in} (pinned {want_in})");
        }
        match pin {
            Pin::Exact { png, pixels } => {
                for (what, g, w) in [("png", &got_png, png), ("pixels", &got_px, pixels)] {
                    if g != w {
                        ok = false;
                        eprintln!("{name}: {what} golden moved: {g} (pinned {w})");
                    }
                }
            }
            Pin::Near { .. } => {
                if let Err(e) = check_near(name, &img, &got_px, pin) {
                    ok = false;
                    eprintln!("{e}");
                }
            }
        }
    }
    assert!(
        ok,
        "canonical goldens moved; see the comment above. Now:\n{report}"
    );
}

/// Pinned on image 0.25.10 (png 0.18.1, zune-jpeg 0.5.15, image-webp
/// 0.2.4). Per case, in the order of the test's inputs: the input's
/// SHA-256 and how the output is pinned.
const GOLDEN: [(&str, Pin); 8] = [
    (
        "6765154854a0b748f4426fc72d0027d2652634318ce3098c629f568a0993e9ef",
        Pin::Near {
            size: (768, 512),
            reference: "eg2-canonical-png-rgb-1536x1024.png",
            reference_pixels: "8f1b4ec7743ffaa128ec9184d016c12586ac4242df0eadbf6ae775be851931e6",
            full_pixels: "37f6088099e09dd6c5c412db56d38e95502da9b82a395eede0852f73b6700bd7",
        },
    ), // png rgb 1536x1024
    (
        "e9e36f9a9c840207405b307d94c75e922e952eb36c94c43426d47c6d02fb74b6",
        Pin::Near {
            size: (256, 768),
            reference: "eg2-canonical-jpeg-1000x3000.png",
            reference_pixels: "fb8a907ba357779a5746fa7edb4b6d1cf91c021591d5baf44912c8b854adbc2d",
            full_pixels: "13dc7ae0acb1726d1431b9d8be0c814dd547cb8fb2b374d216b2be624f1f81b4",
        },
    ), // jpeg 1000x3000
    (
        "729b4edd4c13dd43d239c8acf690f02bd47e4e1af9d34212c7a92b44f578d780",
        Pin::Exact {
            png: "0dee3ba2dc6ab91c0fbe72993b112384328d5d4c6508292a3175f7c0ada0721a",
            pixels: "6b7a33094e24651d4f79b54e7a3b93bbf0553d5b2dff5b94f3ca94e92bead091",
        },
    ), // webp rgba 64x48
    (
        "0c48aeecb64afcb64568b100fc8a864283f75f54027bda5c99184b78d4b288de",
        Pin::Near {
            size: (768, 384),
            reference: "eg2-canonical-webp-rgba-2000x1000.png",
            reference_pixels: "cda1f09c4065ab55a7fdc4c9a4af2744a51dbd148d497cfb1b9c9e13b4a71131",
            full_pixels: "1151f666955fac9bfda45b306ceaa104fd3b807c99ea643b8ee5adbd06bbd5a5",
        },
    ), // webp rgba 2000x1000
    (
        "f43e8c80fbad6357f0f777f8d7be32d4f237e03adaed060633446456c5bf4072",
        Pin::Near {
            size: (768, 597),
            reference: "eg2-canonical-png-rgba16-900x700.png",
            reference_pixels: "52b4c372a1259288e22a11d803ab7975c936d3c034d01f09e4503aba4ad60fb4",
            full_pixels: "45cc66f30bc51f149ed060e86b23f3251b76d0e9e6f9caac82c7824cb2acac4c",
        },
    ), // png rgba16 900x700
    (
        "75093ae433cc0b6fcbe7bb7bff61523e6601da17278981238a2149c8ef03cba2",
        Pin::Exact {
            png: "6e7b7e6536bec0884bc68f860c6d3410671da492167356483b0a0ea2b671917d",
            pixels: "f63d8f9b7b90553a1e7b15ef2a92173c3721eb30d8b105d92a74a04a0e14c374",
        },
    ), // png grey 100x60
    (
        "cd6f60839fde313dad5ab388e11d2fea7c878d9aa154ce6a1f09ebecefdfeba2",
        Pin::Exact {
            png: "f355a86bb49bf95a5b9321e45f93a330f98af846102ffe90ae24e36fbcc05c87",
            pixels: "85730fb58151cc1671bc118a9a408af384f574fc449195561362d78259c36bd2",
        },
    ), // png palette+tRNS 48x24
    (
        "6af90f38046d7b0e06a558b043575a1158637c5f69d47b24298623dc33718b01",
        Pin::Near {
            size: (768, 512),
            reference: "eg2-canonical-jpeg-cmyk-24x16.png",
            reference_pixels: "683cbe192e8afc2f4ff2336c1134181708725376a93de1ee76bc56461f5ddcce",
            full_pixels: "e260aaa9d556f9ab225bdde53fb52db3fd1e5a354c686651f3789c6a3e59d4b3",
        },
    ), // jpeg cmyk 24x16
];
