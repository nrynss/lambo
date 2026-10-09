//! The canonical form of an image for the `lambo-eg2-v2` profile (#22, 22g):
//! what [`super::EmbeddingGemma2Embedder::embed_image`] sends to
//! `llama-server` in place of the submitted bytes.
//!
//! **Why.** With `--image-min-tokens 280 --image-max-tokens 280`, llama.cpp
//! (b11517, `calc_size_preserved_ratio` for the `gemma4v` projector) resizes
//! every image to a grid of 48 px cells. An image whose 48-aligned area is
//! under the 280-token budget rounds *up*, and its target then depends only on
//! its aspect ratio (a square becomes 816x816, 289 tokens); a larger one
//! rounds *down* (a square of 792 px or more becomes 768x768, 256 tokens). The
//! two grids give different vectors, and within one branch the server still
//! resamples from whatever resolution it was given, so the same picture
//! embedded at 512 and at 1024 px came out at cosine 0.97 to 0.999, not 1. No
//! server flag picks the branch. So Lambo sends every image at one size: its
//! longer side exactly [`EG2_CANONICAL_SIDE`].
//!
//! **Why 768 always takes the round-up branch.** The server first aligns each
//! side to the nearest multiple of 48, at least 48. A longer side of 768 is
//! already aligned and the shorter side aligns to at most 768, so the aligned
//! area is at most 768 x 768 = 589,824 px, under the 645,120 px (280 tokens of
//! 2,304 px) the budget allows, for every aspect ratio, including a shorter
//! side of 1 px (aligned up to 48). The server then scales both sides up by
//! the same factor, so its target depends only on the aspect ratio of the
//! canonical form.
//!
//! **The canonical form, exactly** (a client that computes vectors itself
//! must reproduce this to be in the same space):
//!
//! 1. Decode the PNG, JPEG or WebP with the `image` crate (0.25; zune-jpeg for
//!    JPEG, image-webp for WebP, the first frame of an animated image).
//!    EXIF orientation is **not** applied (the pixels are used as stored, as
//!    `llama-server`'s own decoder does); ICC profiles, gamma and sRGB chunks
//!    are ignored, and no colour conversion is made except the decoders' own:
//!    a palette PNG is expanded to RGB or RGBA (`tRNS` becomes alpha), a CMYK
//!    or YCCK JPEG becomes RGB. The pixel type is otherwise kept: grey, grey
//!    with alpha, RGB or RGBA, 8 or 16 bits, alpha straight (not
//!    premultiplied).
//! 2. Resize to `(w', h')`: the longer side becomes exactly 768 and the
//!    shorter side is `max(1, floor((s * 768 + floor(L / 2)) / L))` in integer
//!    arithmetic, where `L` is the longer and `s` the shorter side (the nearest
//!    integer, halves rounded up). A square stays square. An image already
//!    768 px on its longer side is not resampled. Otherwise every channel,
//!    alpha included, is resampled in one step with image-rs's separable
//!    `resize` (`imageops::resize`): [`DOWNSCALE_FILTER`] (Lanczos3) when
//!    `L > 768` and [`UPSCALE_FILTER`] (Catmull-Rom, the a = -0.5 cubic)
//!    when `L < 768`, on the stored channel values (no linearization), then
//!    rounded and clamped to the pixel type.
//! 3. Encode as a PNG of that pixel type, with no ancillary chunks
//!    (`IHDR`, `IDAT`, `IEND` only). The deflate level and filter
//!    ([`PNG_COMPRESSION`], [`PNG_FILTER`]) change the bytes but not the
//!    pixels, so they do not change the vector.
//!
//! The server then drops alpha and reduces 16 bits to 8 itself. The golden
//! tests in `canonical/tests.rs` pin the output for fixed inputs: a change
//! in the decoded pixels (for example after an `image`, `zune-jpeg` or
//! `image-webp` update) is a new profile name.
//!
//! **What this gives**, measured live on b11517
//! (`evidence/issue-22-eg2/size-invariance.txt`): a flat image embeds
//! bit-identically at every submitted size; a patterned one does not, since
//! a picture drawn at 3000 px and scaled to 768 is not the picture drawn at
//! 768 px, and an upscaled 128 px picture has less detail than either. The
//! evidence file has the measured bounds.
//!
//! **Bounded work.** Before anything is decoded, the header's dimensions are
//! checked against `crate::surface::image::MAX_IMAGE_SIDE_PX` a side and
//! [`MAX_DECODE_PIXELS`] in all (the validator already enforces the side, so
//! this is a second line against a decompression bomb), and the decoder runs
//! under the same limits plus [`MAX_DECODE_ALLOC`] bytes. The input is at
//! most `crate::surface::image::MAX_IMAGE_BYTES`. A decode failure is an
//! error (permanent for this input), never a panic.
//!
//! What is stored about an image (its id, its SHA-256, its MIME type) keeps
//! describing the bytes the client sent; only the embed request carries the
//! canonical form.

use std::io::Cursor;

use image::{
    codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder},
    imageops::FilterType,
    DynamicImage, ImageFormat, ImageReader, Limits,
};

use crate::embed::{EmbedError, ImageMime};
use crate::surface::image::MAX_IMAGE_SIDE_PX;

/// The longer side, in pixels, of every image's canonical form.
pub const EG2_CANONICAL_SIDE: u32 = 768;

/// The resampling filter when the longer side is above
/// [`EG2_CANONICAL_SIDE`]. Measured better than Catmull-Rom (llama.cpp's own
/// cubic) on hard edges and equal on smooth pictures.
pub(crate) const DOWNSCALE_FILTER: FilterType = FilterType::Lanczos3;

/// The resampling filter when the longer side is below
/// [`EG2_CANONICAL_SIDE`]. Lanczos3 rings when it enlarges a hard edge:
/// measured live on b11517, a checkerboard drawn at 256 px and upscaled came
/// out at cosine 0.9796 to the one drawn at 768 px with Lanczos3, 0.9848
/// with Catmull-Rom (the server's own resampling of the raw 256 px image gave
/// 0.9848); Triangle matched Catmull-Rom at 128 and 256 px but was worse at
/// 512 (0.9870 against 0.9918). A smooth picture was within 0.0004 with all
/// three.
pub(crate) const UPSCALE_FILTER: FilterType = FilterType::CatmullRom;

/// The canonical PNG's deflate level. Does not change the pixels.
const PNG_COMPRESSION: CompressionType = CompressionType::Fast;

/// The canonical PNG's row filter. Does not change the pixels.
const PNG_FILTER: PngFilter = PngFilter::Adaptive;

/// Most pixels an image may declare before Lambo decodes it: a full
/// `MAX_IMAGE_SIDE_PX` square (16.8 MP).
pub(crate) const MAX_DECODE_PIXELS: u64 = MAX_IMAGE_SIDE_PX as u64 * MAX_IMAGE_SIDE_PX as u64;

/// Most bytes the decoder may allocate: [`MAX_DECODE_PIXELS`] at the widest
/// pixel the three formats decode to (16-bit RGBA, 8 bytes).
pub(crate) const MAX_DECODE_ALLOC: u64 = MAX_DECODE_PIXELS * 8;

fn format_of(mime: ImageMime) -> Result<ImageFormat, EmbedError> {
    match mime {
        ImageMime::Png => Ok(ImageFormat::Png),
        ImageMime::Jpeg => Ok(ImageFormat::Jpeg),
        ImageMime::Webp => Ok(ImageFormat::WebP),
        #[allow(unreachable_patterns)]
        other => Err(EmbedError::Unsupported(format!(
            "EmbeddingGemma 2 cannot canonicalize a {other} image"
        ))),
    }
}

fn limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_SIDE_PX);
    limits.max_image_height = Some(MAX_IMAGE_SIDE_PX);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    limits
}

fn reader(bytes: &[u8], format: ImageFormat) -> ImageReader<Cursor<&[u8]>> {
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits());
    reader
}

/// The header's width and height, read without decoding the pixels, and
/// checked against the side and pixel limits. Cheap.
pub(crate) fn dimensions(bytes: &[u8], mime: ImageMime) -> Result<(u32, u32), EmbedError> {
    let (width, height) = reader(bytes, format_of(mime)?)
        .into_dimensions()
        .map_err(|e| decode_error(mime, &e))?;
    check_dimensions(width, height)?;
    Ok((width, height))
}

fn check_dimensions(width: u32, height: u32) -> Result<(), EmbedError> {
    if width == 0
        || height == 0
        || width > MAX_IMAGE_SIDE_PX
        || height > MAX_IMAGE_SIDE_PX
        || u64::from(width) * u64::from(height) > MAX_DECODE_PIXELS
    {
        return Err(EmbedError::Backend(format!(
            "the image declares {width}x{height} px; EmbeddingGemma 2 decodes at most \
             {MAX_IMAGE_SIDE_PX} px a side"
        )));
    }
    Ok(())
}

/// The message for an image Lambo could not read. Names the format and the
/// decoder's reason, never image bytes.
fn decode_error(mime: ImageMime, e: &image::ImageError) -> EmbedError {
    let reason: String = e.to_string().chars().take(200).collect();
    EmbedError::Backend(format!("could not decode this {mime} image: {reason}"))
}

/// The canonical size for an image of `width` x `height`: the longer side
/// becomes exactly [`EG2_CANONICAL_SIDE`] and the shorter one keeps the
/// aspect ratio, rounded to the nearest pixel (halves up) and at least 1.
/// Integer arithmetic only, so a client can reproduce it exactly.
pub(crate) fn canonical_size(width: u32, height: u32) -> (u32, u32) {
    let long = u64::from(width.max(height).max(1));
    let side = u64::from(EG2_CANONICAL_SIDE);
    let scale = |s: u32| -> u32 {
        let scaled = (u64::from(s) * side + long / 2) / long;
        // `s <= long`, so `scaled <= side`; the clamp only guards the cast.
        u32::try_from(scaled.clamp(1, side)).unwrap_or(EG2_CANONICAL_SIDE)
    };
    (scale(width), scale(height))
}

/// Resize `decoded` to its canonical size with the filter for the direction.
fn resize(decoded: DynamicImage) -> DynamicImage {
    let (width, height) = (decoded.width(), decoded.height());
    let (target_w, target_h) = canonical_size(width, height);
    if (target_w, target_h) == (width, height) {
        return decoded;
    }
    let filter = if width.max(height) > EG2_CANONICAL_SIDE {
        DOWNSCALE_FILTER
    } else {
        UPSCALE_FILTER
    };
    decoded.resize_exact(target_w, target_h, filter)
}

/// The canonical PNG of an image: decode, resize to a longer side of exactly
/// [`EG2_CANONICAL_SIDE`], encode as a lossless PNG. CPU-bound: the embedder
/// runs it off the async runtime.
pub(crate) fn to_canonical_png(bytes: &[u8], mime: ImageMime) -> Result<Vec<u8>, EmbedError> {
    let format = format_of(mime)?;
    let (width, height) = dimensions(bytes, mime)?;
    let decoded = reader(bytes, format)
        .decode()
        .map_err(|e| decode_error(mime, &e))?;
    // A bitstream that decodes to a size other than its header's is refused
    // rather than trusted.
    if (decoded.width(), decoded.height()) != (width, height) {
        return Err(EmbedError::Backend(format!(
            "this {mime} image decodes to {}x{} px but its header declares {width}x{height}",
            decoded.width(),
            decoded.height()
        )));
    }
    let canonical = resize(decoded);
    let mut out = Vec::new();
    canonical
        .write_with_encoder(PngEncoder::new_with_quality(
            &mut out,
            PNG_COMPRESSION,
            PNG_FILTER,
        ))
        .map_err(|e| {
            EmbedError::Backend(format!(
                "could not encode the canonical PNG of this {mime} image: {}",
                e.to_string().chars().take(200).collect::<String>()
            ))
        })?;
    Ok(out)
}

/// [`to_canonical_png`] on a blocking thread (a 4096 px image takes long
/// enough to stall other tasks).
pub(crate) async fn canonicalize(bytes: &[u8], mime: ImageMime) -> Result<Vec<u8>, EmbedError> {
    let owned = bytes.to_vec();
    tokio::task::spawn_blocking(move || to_canonical_png(&owned, mime))
        .await
        .map_err(|e| EmbedError::Backend(format!("canonicalizing an image failed: {e}")))?
}

#[cfg(test)]
mod tests;
