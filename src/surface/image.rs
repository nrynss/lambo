//! Validation of client-supplied images (#22), shared by every surface that
//! accepts one.
//!
//! [`validate`] is the only way to build an [`ImageInput`], so an embedder
//! adapter can trust what it receives. It checks, in order:
//!
//! 1. the declared MIME type is on the allowlist (`image/png`, `image/jpeg`,
//!    `image/webp`: exact strings, no aliases, no parameters);
//! 2. the bytes are non-empty and at most [`MAX_IMAGE_BYTES`];
//! 3. the magic bytes are one of the three formats **and** match the declared
//!    type: a mismatch is refused, never corrected, so there is no
//!    content-type guessing and no "trust the client" path;
//! 4. the header names a width and height, each between 1 and
//!    [`MAX_IMAGE_SIDE_PX`]. Only the header is read (PNG `IHDR`, the JPEG
//!    `SOF0`/`SOF1`/`SOF2` frame header, the WebP `VP8 `/`VP8L`/`VP8X`
//!    header), never the pixels, and without an image crate. A header this
//!    parser cannot read is refused.
//!
//!    For an extended (`VP8X`) WebP the header only declares a canvas; the
//!    pixels live in a later `VP8 `/`VP8L` chunk that declares its own size
//!    (up to 16383 px a side), or in any number of animation frames. So an
//!    animated WebP (the `VP8X` animation flag, or an `ANMF` chunk) is
//!    refused, and the first `VP8 `/`VP8L` chunk must be present and declare
//!    exactly the canvas size. With that, for every accepted format the
//!    dimensions checked here are the ones the bitstream decodes to, which
//!    bounds what a backend may have to decode.
//!
//! It then computes the SHA-256 once, for the image id and the embedding
//! source later PRs record.
//!
//! The input here is raw bytes. The MCP surface (#22 PR 4) decodes base64
//! with [`decode_base64`], which caps the encoded length **before** it
//! decodes, and then calls this. The transports cap a whole frame at 4 MiB
//! before it is parsed (#101), which bounds what reaches this check but is
//! above [`MAX_IMAGE_B64_LEN`], so the check still refuses.
//!
//! A client-computed vector (#22 PR 4) is checked by
//! [`check_submitted_vector`], the model-safe twin of the core's own check.
//!
//! **No message echoes the payload.** A refusal names the rule, the limit and
//! at most the sniffed format; never image bytes, and never the declared MIME
//! string either, since that is client text of unbounded length. As in
//! [`super::validate`], refusals are plain `String`s that each surface adapts.

use sha2::{Digest, Sha256};

use crate::embed::{ImageInput, ImageMime};

/// Most bytes one image may have, decoded (2 MiB). Its base64 form, 2,796,204
/// bytes, fits under the MCP transports' 4 MiB frame cap (the HTTP body cap,
/// and since #101 the stdio and session-endpoint frame cap) with room for the
/// JSON envelope.
pub const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;

/// Largest width or height, in pixels, an image header may declare.
pub const MAX_IMAGE_SIDE_PX: u32 = 4096;

/// Longest base64 text [`decode_base64`] accepts: the padded standard
/// encoding of [`MAX_IMAGE_BYTES`] (`4 * ceil(MAX_IMAGE_BYTES / 3)`), so no
/// image the byte cap allows is refused for its encoding, and nothing longer
/// is ever decoded. The MCP schema publishes it as `maxLength`.
pub const MAX_IMAGE_B64_LEN: usize = MAX_IMAGE_BYTES.div_ceil(3) * 4;

/// Most components a submitted vector may have. The MCP schema publishes it
/// as `maxItems`; the live contract's width is checked after it.
pub const MAX_VECTOR_VALUES: usize = 4096;

/// Bytes an image concept's suffix, `" [image:<id>]"`, adds to its caption
/// beyond the id itself.
const SUFFIX_OVERHEAD_BYTES: usize =
    " ".len() + crate::graph::image::IMAGE_SUFFIX_OPEN.len() + "]".len();

/// Longest caption, in bytes after trimming, whose image content
/// (`"{caption} [image:<id>]"`) fits the uniform
/// [`MAX_CONTENT_BYTES`](super::limits::MAX_CONTENT_BYTES) with an id of
/// `id_len` bytes.
pub const fn max_caption_bytes(id_len: usize) -> usize {
    super::limits::MAX_CONTENT_BYTES - SUFFIX_OVERHEAD_BYTES - id_len
}

/// Longest caption any image derive can accept: the one with a one-byte id
/// (16,374 bytes). The MCP schema publishes it as `caption`'s `maxLength`.
pub const MAX_CAPTION_BYTES: usize = max_caption_bytes(1);

/// Refuse a caption whose image content would exceed the uniform content
/// cap once Lambo appends `" [image:<id>]"`.
///
/// `image_id` is the caller's id, or `None` for a default (digest) id, which
/// is always [`DEFAULT_IMAGE_ID_HEX`](crate::graph::image::DEFAULT_IMAGE_ID_HEX)
/// characters. The core checks the built content too, but as a
/// configuration error; checked here, the caller learns its real limit.
/// The message names lengths only, never the caption.
pub fn check_caption_fits(caption: &str, image_id: Option<&str>) -> Result<(), String> {
    use crate::graph::image::DEFAULT_IMAGE_ID_HEX;
    let id_len = image_id.map_or(DEFAULT_IMAGE_ID_HEX, str::len);
    let max = max_caption_bytes(id_len);
    let len = caption.trim().len();
    if len > max {
        let id = match image_id {
            Some(_) => format!("a {id_len}-byte image_id"),
            None => format!("the default {DEFAULT_IMAGE_ID_HEX}-character image id"),
        };
        return Err(format!(
            "caption is {len} bytes; with {id} it may be at most {max} bytes, because the \
             stored content, the caption plus \" [image:<id>]\", is capped at {} bytes",
            super::limits::MAX_CONTENT_BYTES
        ));
    }
    Ok(())
}

/// The refusal for text that is not standard padded base64, with a hint for
/// the two shapes real clients most often send: a `data:` URI (copied from a
/// browser or an `<img src>`) and line-wrapped output (GNU `base64` wraps at
/// 76 columns). Both are refused rather than repaired, as the schema says
/// one exact form; the hint names the shape, never the input.
fn base64_refusal(data: &str) -> String {
    const BASE: &str = "image.data is not valid base64 (standard alphabet, padded)";
    if data.trim_start().starts_with("data:") {
        format!(
            "{BASE}: send the base64 text alone, without a data: URI prefix such as \
             \"data:image/png;base64,\""
        )
    } else if data.contains(['\n', '\r']) {
        format!(
            "{BASE}: send it on one line, with no line breaks (GNU base64 wraps at 76 \
             columns unless given -w0)"
        )
    } else {
        BASE.to_owned()
    }
}

/// Decode an image's base64 text (standard alphabet, padded), refusing text
/// longer than [`MAX_IMAGE_B64_LEN`] before decoding any of it.
///
/// The messages name the rule and the limit; they never quote the input.
pub fn decode_base64(data: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    if data.len() > MAX_IMAGE_B64_LEN {
        return Err(format!(
            "image.data is {} characters, over the {MAX_IMAGE_B64_LEN}-character limit \
             (a {MAX_IMAGE_BYTES}-byte image, base64-encoded)",
            data.len()
        ));
    }
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| base64_refusal(data))
}

/// Check a client-computed vector against the live contract before anything
/// else sees it: the declared contract must equal `live` exactly (kind, model
/// and dim), the vector must have at most [`MAX_VECTOR_VALUES`] components
/// and exactly `live.dim` of them, every component must be finite, and the
/// norm must not be zero.
///
/// The same rules as the core's `check_supplied_values`, with a message a
/// caller may read: it names which fields of the declared contract differ
/// and shows the live contract (which `lambo_stats` publishes as
/// `embedding_contract`), but never quotes the declared strings, the values,
/// or any other client input.
pub fn check_submitted_vector(
    values: &[f32],
    declared: &crate::types::EmbeddingContract,
    live: &crate::types::EmbeddingContract,
) -> Result<(), String> {
    check_submitted_vector_as("vector", values, declared, live)
}

/// [`check_submitted_vector`] for a vector sent in the field `field`
/// (`"vector"` on `lambo_derive_image`, `"query_vector"` on `lambo_recall`,
/// #22 PR 6), so each refusal names the field the caller actually sent.
pub fn check_submitted_vector_as(
    field: &str,
    values: &[f32],
    declared: &crate::types::EmbeddingContract,
    live: &crate::types::EmbeddingContract,
) -> Result<(), String> {
    if values.len() > MAX_VECTOR_VALUES {
        return Err(format!(
            "{field}.values has {} components, over the limit of {MAX_VECTOR_VALUES}",
            values.len()
        ));
    }
    let mut differ = Vec::new();
    if declared.kind != live.kind {
        differ.push("kind");
    }
    if declared.model != live.model {
        differ.push("model");
    }
    if declared.dim != live.dim {
        differ.push("dim");
    }
    if !differ.is_empty() {
        return Err(format!(
            "{field}.contract does not match this session's embedding contract ({} differ{}); \
             a vector is accepted only into the exact space it was computed in. This \
             session's contract is kind={:?} model={:?} dim={} (lambo_stats reports it as \
             embedding_contract)",
            differ.join(", "),
            if differ.len() == 1 { "s" } else { "" },
            live.kind,
            live.model.as_deref().unwrap_or(""),
            live.dim
        ));
    }
    if values.len() != live.dim {
        return Err(format!(
            "{field}.values has {} components but the embedding contract's dim is {}",
            values.len(),
            live.dim
        ));
    }
    if values.iter().any(|x| !x.is_finite()) {
        return Err(format!(
            "{field}.values has a non-finite component (NaN or infinity)"
        ));
    }
    let norm = values
        .iter()
        .map(|x| f64::from(*x) * f64::from(*x))
        .sum::<f64>()
        .sqrt();
    if norm == 0.0 || !norm.is_finite() {
        return Err(format!(
            "{field}.values has zero norm; it names no direction to search by"
        ));
    }
    Ok(())
}

const PNG_MAGIC: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const JPEG_MAGIC: &[u8; 3] = b"\xFF\xD8\xFF";

/// Validate `bytes` as an image of the declared MIME type. See the module
/// docs for the rules, in the order they are checked.
pub fn validate<'a>(bytes: &'a [u8], declared_mime: &str) -> Result<ImageInput<'a>, String> {
    let Some(declared) = ImageMime::from_mime(declared_mime) else {
        return Err(
            "image: mime type must be exactly one of image/png, image/jpeg, image/webp".into(),
        );
    };
    if bytes.is_empty() {
        return Err("image: no bytes".into());
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "image: {} bytes exceeds the {MAX_IMAGE_BYTES}-byte limit",
            bytes.len()
        ));
    }
    let Some(sniffed) = sniff(bytes) else {
        return Err("image: the bytes are not a PNG, JPEG or WebP image".into());
    };
    if sniffed != declared {
        return Err(format!(
            "image: declared {declared} but the bytes are {sniffed}"
        ));
    }
    let (width, height) = match dimensions(bytes, sniffed) {
        Ok(sides) => sides,
        Err(HeaderFault::Unreadable) => {
            return Err(format!(
                "image: the {sniffed} header is truncated or unreadable"
            ));
        }
        Err(HeaderFault::Animated) => {
            return Err("image: animated WebP is not accepted; send one still image".into());
        }
        Err(HeaderFault::CanvasMismatch) => {
            return Err(
                "image: the WebP canvas size differs from the size of its image data".into(),
            );
        }
    };
    for (side, px) in [("width", width), ("height", height)] {
        if px == 0 || px > MAX_IMAGE_SIDE_PX {
            return Err(format!(
                "image: {side} {px} px is outside 1..={MAX_IMAGE_SIDE_PX}"
            ));
        }
    }
    let sha256: [u8; 32] = Sha256::digest(bytes).into();
    Ok(ImageInput::from_validated(bytes, sniffed, sha256))
}

/// The format `bytes`' magic bytes name, if any of the three. For a surface
/// that reads a local file and must pick the declared type itself (`lambo
/// derive-image --image` without `--mime`); [`validate`] still checks it.
pub fn sniff_mime(bytes: &[u8]) -> Option<ImageMime> {
    sniff(bytes)
}

/// The format the magic bytes name, if any of the three.
fn sniff(bytes: &[u8]) -> Option<ImageMime> {
    if bytes.starts_with(PNG_MAGIC) {
        Some(ImageMime::Png)
    } else if bytes.starts_with(JPEG_MAGIC) {
        Some(ImageMime::Jpeg)
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(ImageMime::Webp)
    } else {
        None
    }
}

/// Why [`dimensions`] could not name a single still image's size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HeaderFault {
    /// Truncated, or not a header this parser reads.
    Unreadable,
    /// An animated WebP: more than one image to decode.
    Animated,
    /// A `VP8X` canvas whose image chunk declares a different size.
    CanvasMismatch,
}

/// `(width, height)` from the header of an image whose magic bytes already
/// matched `mime`.
fn dimensions(bytes: &[u8], mime: ImageMime) -> Result<(u32, u32), HeaderFault> {
    match mime {
        ImageMime::Png => png_dimensions(bytes).ok_or(HeaderFault::Unreadable),
        ImageMime::Jpeg => jpeg_dimensions(bytes).ok_or(HeaderFault::Unreadable),
        ImageMime::Webp => webp_dimensions(bytes),
    }
}

fn be_u16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn be_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn le_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// The 24-bit little-endian integer at `at`.
fn le_u24(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at + 3)?;
    Some(u32::from(s[0]) | u32::from(s[1]) << 8 | u32::from(s[2]) << 16)
}

/// PNG: the first chunk must be a complete `IHDR` (13 data bytes and its
/// CRC), whose data starts with the big-endian width and height.
fn png_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    const IHDR_END: usize = 8 + 4 + 4 + 13 + 4;
    if b.len() < IHDR_END || be_u32(b, 8)? != 13 || b.get(12..16)? != b"IHDR" {
        return None;
    }
    Some((be_u32(b, 16)?, be_u32(b, 20)?))
}

/// JPEG: walk the marker segments from SOI to the first frame header. Only
/// baseline, extended and progressive Huffman frames (`SOF0`..`SOF2`) are
/// read; any other frame type, or a scan or end-of-image before a frame
/// header, is unreadable. The frame header holds precision, then the
/// big-endian height and width.
fn jpeg_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    let mut at = 2; // past SOI (FF D8)
    loop {
        if *b.get(at)? != 0xFF {
            return None;
        }
        // A marker may be preceded by any number of 0xFF fill bytes.
        while *b.get(at)? == 0xFF {
            at += 1;
        }
        let marker = *b.get(at)?;
        at += 1;
        match marker {
            // TEM and RST0..RST7 stand alone, with no length.
            0x01 | 0xD0..=0xD7 => continue,
            0xC0..=0xC2 => {
                let len = be_u16(b, at)?;
                if len < 8 {
                    return None;
                }
                let height = be_u16(b, at + 3)?;
                let width = be_u16(b, at + 5)?;
                return Some((u32::from(width), u32::from(height)));
            }
            // Other SOFn (DHT C4, JPG C8 and DAC CC are not frame headers),
            // or a scan or end-of-image before any frame header.
            0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF | 0xD9 | 0xDA => return None,
            _ => {
                let len = be_u16(b, at)?;
                if len < 2 {
                    return None;
                }
                at += usize::from(len);
            }
        }
    }
}

/// WebP: the first chunk after the RIFF header is the image header, one of
/// lossy `VP8 `, lossless `VP8L` or extended `VP8X`.
fn webp_dimensions(b: &[u8]) -> Result<(u32, u32), HeaderFault> {
    let first = b.get(12..16).ok_or(HeaderFault::Unreadable)?;
    match first {
        b"VP8 " | b"VP8L" => le_u32(b, 16)
            .and_then(|size| image_chunk_sides(b, first, WEBP_FIRST_PAYLOAD, size))
            .ok_or(HeaderFault::Unreadable),
        b"VP8X" => vp8x_dimensions(b),
        _ => Err(HeaderFault::Unreadable),
    }
}

/// Where the first WebP chunk's payload starts: past `RIFF`, the RIFF size,
/// `WEBP`, the chunk's fourcc and its size.
const WEBP_FIRST_PAYLOAD: usize = 20;

/// The sides a `VP8 ` or `VP8L` chunk declares, given its payload offset
/// and declared size. A chunk whose declared size does not cover the header
/// read here is unreadable, whatever bytes follow it.
fn image_chunk_sides(b: &[u8], fourcc: &[u8], payload: usize, size: u32) -> Option<(u32, u32)> {
    let size = usize::try_from(size).ok()?;
    match fourcc {
        b"VP8 " if size >= VP8_HEADER => vp8_sides(b, payload),
        b"VP8L" if size >= VP8L_HEADER => vp8l_sides(b, payload),
        _ => None,
    }
}

/// Bytes of a `VP8 ` payload `vp8_sides` reads: frame tag, start code, sides.
const VP8_HEADER: usize = 10;
/// Bytes of a `VP8L` payload `vp8l_sides` reads: signature and packed sides.
const VP8L_HEADER: usize = 5;

/// Lossy `VP8 ` payload at `p`: a 3-byte frame tag whose low bit is 0 for a
/// key frame (a WebP image is one key frame), the 9D 01 2A start code, then
/// 14-bit little-endian width and height (the top two bits are scale).
fn vp8_sides(b: &[u8], p: usize) -> Option<(u32, u32)> {
    if *b.get(p)? & 1 != 0 || b.get(p.checked_add(3)?..p.checked_add(6)?)? != [0x9D, 0x01, 0x2A] {
        return None;
    }
    let sides = le_u32(b, p + 6)?;
    Some((sides & 0x3FFF, (sides >> 16) & 0x3FFF))
}

/// Lossless `VP8L` payload at `p`: the 0x2F signature, then width-1 and
/// height-1 as two 14-bit fields packed little-endian, an alpha hint bit and
/// a 3-bit version that must be 0.
fn vp8l_sides(b: &[u8], p: usize) -> Option<(u32, u32)> {
    if *b.get(p)? != 0x2F {
        return None;
    }
    let bits = le_u32(b, p.checked_add(1)?)?;
    if bits >> 29 != 0 {
        return None;
    }
    Some(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1))
}

/// Extended `VP8X`: flags and 3 reserved bytes, then 24-bit little-endian
/// canvas width-1 and height-1. The canvas is only a declaration, so the
/// animation flag is refused and the first image chunk after it must declare
/// the same size (see the module docs).
fn vp8x_dimensions(b: &[u8]) -> Result<(u32, u32), HeaderFault> {
    const ANIMATION_FLAG: u8 = 0x02;
    let p = WEBP_FIRST_PAYLOAD;
    let flags = *b.get(p).ok_or(HeaderFault::Unreadable)?;
    if flags & ANIMATION_FLAG != 0 {
        return Err(HeaderFault::Animated);
    }
    let canvas = le_u24(b, p + 4)
        .zip(le_u24(b, p + 7))
        .map(|(w, h)| (w + 1, h + 1))
        .ok_or(HeaderFault::Unreadable)?;
    if vp8x_image_sides(b)? != canvas {
        return Err(HeaderFault::CanvasMismatch);
    }
    Ok(canvas)
}

/// The sides the first `VP8 `/`VP8L` chunk after the `VP8X` chunk declares.
/// Walks the RIFF chunks (fourcc, little-endian size, payload padded to an
/// even length), bounded by the RIFF size and by the bytes supplied: a chunk
/// that starts past either end, or no image chunk at all, is unreadable (a
/// decoder ignores bytes after the RIFF end). An `ANMF` frame before any
/// image chunk is an animation whatever the flags said.
fn vp8x_image_sides(b: &[u8]) -> Result<(u32, u32), HeaderFault> {
    let riff_end = le_u32(b, 4)
        .and_then(|n| usize::try_from(n).ok())
        .and_then(|n| n.checked_add(8))
        .ok_or(HeaderFault::Unreadable)?;
    let end = riff_end.min(b.len());
    let mut at: usize = 12; // the VP8X chunk itself
    loop {
        // `at` is below `end` <= b.len() <= isize::MAX, so `at + 8` cannot
        // overflow on any target.
        if at.checked_add(8).is_none_or(|header_end| header_end > end) {
            return Err(HeaderFault::Unreadable);
        }
        let fourcc = &b[at..at + 4];
        let size = le_u32(b, at + 4).ok_or(HeaderFault::Unreadable)?;
        let payload = at + 8;
        if at != 12 {
            match fourcc {
                b"VP8 " | b"VP8L" => {
                    return image_chunk_sides(b, fourcc, payload, size)
                        .ok_or(HeaderFault::Unreadable);
                }
                b"ANMF" => return Err(HeaderFault::Animated),
                _ => {}
            }
        }
        // Every step advances by at least 8 bytes, and `size` is at most
        // u32::MAX, so neither the sum nor the loop can run away; the bound
        // check above fails once `at` passes the end.
        let padded = usize::try_from(size)
            .ok()
            .and_then(|n| n.checked_add(n & 1))
            .ok_or(HeaderFault::Unreadable)?;
        at = payload.checked_add(padded).ok_or(HeaderFault::Unreadable)?;
    }
}

#[cfg(test)]
mod tests;
