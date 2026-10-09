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
//! two grids give different vectors, so the same picture embedded at 512 and
//! at 1024 px came out at cosine 0.97 to 0.999, not 1. No server flag picks
//! the branch. Every image whose longer side is at most
//! [`EG2_CANONICAL_MAX_SIDE`] takes the round-up branch (768 x 768 = 589,824
//! px, under the 645,120 the budget allows), so Lambo brings every image to
//! that bound before sending it.
//!
//! **The rule.**
//!
//! - A PNG or JPEG whose longer side is at most 768 px is sent byte-identical:
//!   nothing is decoded, so its vector is the one `lambo-eg2-v1` gave it.
//! - A larger PNG or JPEG is decoded, downscaled to a longer side of exactly
//!   768 px with the aspect ratio kept (the shorter side rounded to the
//!   nearest pixel, at least 1), with Lanczos3, and re-encoded as a lossless
//!   PNG in its own colour type (alpha and 16-bit depth kept; the server drops alpha and
//!   reduces 16 bits itself, as it does for a small image).
//! - A WebP of any size is decoded and re-encoded as a lossless PNG, and
//!   downscaled the same way above 768 px. `llama-server` decodes WebP only by
//!   running an `ffmpeg`/`ffprobe` found on its `PATH`: without one it refuses
//!   the image, and with one the pixels depend on that `ffmpeg`. Decoding it
//!   here keeps the vector a function of the image alone.
//!
//! So an image's vector depends only on its canonical form, and two
//! submissions of one picture at different sizes above 768 px share it up to
//! resampling. Lanczos3 was measured against Catmull-Rom (llama.cpp's own
//! cubic) on b11517: for a hard-edged checkerboard rendered at 1024 to 3000
//! px its renders agreed to at least 0.99978 pairwise (Catmull-Rom 0.99943)
//! and 0.99911 with the picture drawn at 768 px (Catmull-Rom 0.99861); a
//! smooth picture was within 0.0001 either way. A solid colour is identical
//! at every size with either. `evidence/issue-22-eg2/size-invariance.txt`
//! has the table.
//!
//! **Bounded work.** Before anything is decoded, the header's dimensions are
//! checked against `crate::surface::image::MAX_IMAGE_SIDE_PX` a side and
//! [`MAX_DECODE_PIXELS`] in all (the validator already enforces the side, so
//! this is a second line against a decompression bomb), and the decoder runs
//! under the same limits plus [`MAX_DECODE_ALLOC`] bytes. The input is at
//! most `crate::surface::image::MAX_IMAGE_BYTES`. A decode failure is
//! [`EmbedError::Backend`] (permanent for this input), never a panic.
//!
//! What is stored about an image (its id, its SHA-256, its MIME type) keeps
//! describing the bytes the client sent; only the embed request carries the
//! canonical form.

use std::io::Cursor;

use image::{imageops::FilterType, ImageFormat, ImageReader, Limits};

use crate::embed::{EmbedError, ImageInput, ImageMime};
use crate::surface::image::MAX_IMAGE_SIDE_PX;

/// The longest side, in pixels, of an image's canonical form.
pub const EG2_CANONICAL_MAX_SIDE: u32 = 768;

/// Most pixels an image may declare before Lambo decodes it: a full
/// `MAX_IMAGE_SIDE_PX` square (16.8 MP).
pub(crate) const MAX_DECODE_PIXELS: u64 = MAX_IMAGE_SIDE_PX as u64 * MAX_IMAGE_SIDE_PX as u64;

/// Most bytes the decoder may allocate: [`MAX_DECODE_PIXELS`] at the widest
/// pixel the three formats decode to (16-bit RGBA, 8 bytes).
pub(crate) const MAX_DECODE_ALLOC: u64 = MAX_DECODE_PIXELS * 8;

/// What to send for an image.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Canonical<'a> {
    /// The submitted bytes, unchanged, with their MIME type.
    Original(&'a [u8], ImageMime),
    /// A lossless PNG Lambo encoded.
    Png(Vec<u8>),
}

impl Canonical<'_> {
    pub(crate) fn bytes(&self) -> &[u8] {
        match self {
            Self::Original(bytes, _) => bytes,
            Self::Png(bytes) => bytes,
        }
    }

    pub(crate) fn mime(&self) -> ImageMime {
        match self {
            Self::Original(_, mime) => *mime,
            Self::Png(_) => ImageMime::Png,
        }
    }
}

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
/// checked against the side and pixel limits.
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

/// The canonical size for an image of `width` x `height`: unchanged when the
/// longer side is at most [`EG2_CANONICAL_MAX_SIDE`], else the longer side
/// becomes exactly that and the shorter one keeps the aspect ratio, rounded
/// to the nearest pixel (halves up) and at least 1.
pub(crate) fn canonical_size(width: u32, height: u32) -> (u32, u32) {
    let long = width.max(height);
    if long <= EG2_CANONICAL_MAX_SIDE {
        return (width, height);
    }
    let scale = |side: u32| -> u32 {
        let num = u64::from(side) * u64::from(EG2_CANONICAL_MAX_SIDE) + u64::from(long) / 2;
        u32::try_from(num / u64::from(long))
            .unwrap_or(EG2_CANONICAL_MAX_SIDE)
            .max(1)
    };
    (scale(width), scale(height))
}

/// Whether `image` is sent as it is (no decode needed). Cheap: reads the
/// header only.
pub(crate) fn needs_work(bytes: &[u8], mime: ImageMime) -> Result<bool, EmbedError> {
    if mime == ImageMime::Webp {
        return Ok(true);
    }
    let (width, height) = dimensions(bytes, mime)?;
    Ok(width.max(height) > EG2_CANONICAL_MAX_SIDE)
}

/// Decode, downscale if needed, and encode as a lossless PNG. CPU-bound:
/// the embedder runs it off the async runtime.
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
    let (target_w, target_h) = canonical_size(width, height);
    let canonical = if (target_w, target_h) == (width, height) {
        decoded
    } else {
        decoded.resize_exact(target_w, target_h, FilterType::Lanczos3)
    };
    let mut out = Vec::new();
    canonical
        .write_to(Cursor::new(&mut out), ImageFormat::Png)
        .map_err(|e| {
            EmbedError::Backend(format!(
                "could not encode the canonical PNG of this {mime} image: {}",
                e.to_string().chars().take(200).collect::<String>()
            ))
        })?;
    Ok(out)
}

/// The canonical form of `image`, synchronously. The embedder uses
/// [`needs_work`] and [`to_canonical_png`] directly so the decode runs on a
/// blocking thread; this is the same rule in one call.
#[cfg(test)]
pub(crate) fn canonicalize<'a>(image: &ImageInput<'a>) -> Result<Canonical<'a>, EmbedError> {
    let (bytes, mime) = (image.bytes(), image.mime());
    if needs_work(bytes, mime)? {
        Ok(Canonical::Png(to_canonical_png(bytes, mime)?))
    } else {
        Ok(Canonical::Original(bytes, mime))
    }
}

/// The canonical form of `image`, decoding on a blocking thread when it has
/// to (a 4096 px image takes long enough to stall other tasks).
pub(crate) async fn canonicalize_async<'a>(
    image: &ImageInput<'a>,
) -> Result<Canonical<'a>, EmbedError> {
    let (bytes, mime) = (image.bytes(), image.mime());
    if !needs_work(bytes, mime)? {
        return Ok(Canonical::Original(bytes, mime));
    }
    let owned = bytes.to_vec();
    let png = tokio::task::spawn_blocking(move || to_canonical_png(&owned, mime))
        .await
        .map_err(|e| EmbedError::Backend(format!("canonicalizing an image failed: {e}")))??;
    Ok(Canonical::Png(png))
}

#[cfg(test)]
mod tests;
