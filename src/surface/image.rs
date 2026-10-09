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
//!    header), never the pixels, and without an image crate. This bounds what
//!    a backend may have to decode. A header this parser cannot read is
//!    refused.
//!
//! It then computes the SHA-256 once, for the image id and the embedding
//! source later PRs record.
//!
//! The input here is raw bytes. The MCP surface (#22 PR 4) decodes base64 and
//! caps the encoded length before it calls this.
//!
//! **No message echoes the payload.** A refusal names the rule, the limit and
//! at most the sniffed format; never image bytes, and never the declared MIME
//! string either, since that is client text of unbounded length. As in
//! [`super::validate`], refusals are plain `String`s that each surface adapts.

use sha2::{Digest, Sha256};

use crate::embed::{ImageInput, ImageMime};

/// Most bytes one image may have, decoded (2 MiB). Its base64 form, 2,796,204
/// bytes, fits under the HTTP transport's 4 MiB body cap with room for the
/// JSON envelope.
pub const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;

/// Largest width or height, in pixels, an image header may declare.
pub const MAX_IMAGE_SIDE_PX: u32 = 4096;

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
    let Some((width, height)) = dimensions(bytes, sniffed) else {
        return Err(format!(
            "image: the {sniffed} header is truncated or unreadable"
        ));
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

/// `(width, height)` from the header of an image whose magic bytes already
/// matched `mime`. `None` when the header is truncated or not one this parser
/// reads.
fn dimensions(bytes: &[u8], mime: ImageMime) -> Option<(u32, u32)> {
    match mime {
        ImageMime::Png => png_dimensions(bytes),
        ImageMime::Jpeg => jpeg_dimensions(bytes),
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
fn webp_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    let payload = 20;
    match b.get(12..16)? {
        // Lossy: a 3-byte frame tag, the 9D 01 2A start code, then 14-bit
        // little-endian width and height (the top two bits are scale).
        b"VP8 " => {
            if b.get(payload + 3..payload + 6)? != [0x9D, 0x01, 0x2A] {
                return None;
            }
            let sides = le_u32(b, payload + 6)?;
            Some((sides & 0x3FFF, (sides >> 16) & 0x3FFF))
        }
        // Lossless: the 0x2F signature, then width-1 and height-1 as two
        // 14-bit fields packed little-endian.
        b"VP8L" => {
            if *b.get(payload)? != 0x2F {
                return None;
            }
            let bits = le_u32(b, payload + 1)?;
            Some(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1))
        }
        // Extended: flags and 3 reserved bytes, then 24-bit little-endian
        // canvas width-1 and height-1.
        b"VP8X" => Some((le_u24(b, payload + 4)? + 1, le_u24(b, payload + 7)? + 1)),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
