//! Table tests for [`super::validate`]. Every image is built here in code from
//! its header layout; no binary file is committed.

use super::*;

/// A PNG of `w` x `h`: signature, a complete IHDR (CRC not checked, so
/// zeroed), then IEND.
fn png(w: u32, h: u32) -> Vec<u8> {
    let mut b = PNG_MAGIC.to_vec();
    b.extend_from_slice(&13u32.to_be_bytes());
    b.extend_from_slice(b"IHDR");
    b.extend_from_slice(&w.to_be_bytes());
    b.extend_from_slice(&h.to_be_bytes());
    b.extend_from_slice(&[8, 6, 0, 0, 0]); // depth, colour type, methods
    b.extend_from_slice(&[0; 4]); // CRC
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(b"IEND");
    b.extend_from_slice(&[0; 4]);
    b
}

/// A JPEG of `w` x `h` with frame marker `sof`: SOI, an APP0 segment, a run
/// of fill bytes, then the frame header.
fn jpeg_with(sof: u8, w: u16, h: u16) -> Vec<u8> {
    let mut b = vec![0xFF, 0xD8];
    // APP0 "JFIF" segment, length 16.
    b.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x10]);
    b.extend_from_slice(b"JFIF\0");
    b.extend_from_slice(&[1, 1, 0, 0, 1, 0, 1, 0, 0]);
    // Fill bytes before the frame marker are legal.
    b.extend_from_slice(&[0xFF, 0xFF, 0xFF, sof]);
    b.extend_from_slice(&[0x00, 0x11, 8]); // length 17, precision 8
    b.extend_from_slice(&h.to_be_bytes());
    b.extend_from_slice(&w.to_be_bytes());
    b.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
    b.extend_from_slice(&[0xFF, 0xD9]);
    b
}

fn jpeg(w: u16, h: u16) -> Vec<u8> {
    jpeg_with(0xC0, w, h)
}

fn riff(chunk: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut b = b"RIFF".to_vec();
    b.extend_from_slice(&(4 + 8 + payload.len() as u32).to_le_bytes());
    b.extend_from_slice(b"WEBP");
    b.extend_from_slice(chunk);
    b.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    b.extend_from_slice(payload);
    b
}

/// Lossy WebP: frame tag, start code, 14-bit sides.
fn webp_vp8(w: u16, h: u16) -> Vec<u8> {
    let mut p = vec![0x50, 0x01, 0x00, 0x9D, 0x01, 0x2A];
    p.extend_from_slice(&w.to_le_bytes());
    p.extend_from_slice(&h.to_le_bytes());
    p.extend_from_slice(&[0; 4]);
    riff(b"VP8 ", &p)
}

/// Lossless WebP: signature, then width-1 and height-1 packed in 14 bits each.
fn webp_vp8l(w: u32, h: u32) -> Vec<u8> {
    let bits = (w - 1) | (h - 1) << 14;
    let mut p = vec![0x2F];
    p.extend_from_slice(&bits.to_le_bytes());
    p.extend_from_slice(&[0; 3]);
    riff(b"VP8L", &p)
}

/// Extended WebP: flags, reserved, 24-bit width-1 and height-1.
fn webp_vp8x(w: u32, h: u32) -> Vec<u8> {
    let mut p = vec![0x10, 0, 0, 0];
    p.extend_from_slice(&(w - 1).to_le_bytes()[..3]);
    p.extend_from_slice(&(h - 1).to_le_bytes()[..3]);
    riff(b"VP8X", &p)
}

fn refusal(bytes: &[u8], mime: &str) -> String {
    match validate(bytes, mime) {
        Ok(input) => panic!("expected a refusal, got {input:?}"),
        Err(msg) => msg,
    }
}

#[test]
fn accepts_every_format_and_header_variant() {
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("png", png(640, 480), "image/png"),
        ("jpeg baseline", jpeg(640, 480), "image/jpeg"),
        ("jpeg extended", jpeg_with(0xC1, 640, 480), "image/jpeg"),
        ("jpeg progressive", jpeg_with(0xC2, 640, 480), "image/jpeg"),
        ("webp lossy", webp_vp8(640, 480), "image/webp"),
        ("webp lossless", webp_vp8l(640, 480), "image/webp"),
        ("webp extended", webp_vp8x(640, 480), "image/webp"),
    ];
    for (name, bytes, mime) in cases {
        let input = validate(&bytes, mime).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(input.mime().as_str(), mime, "{name}");
        assert_eq!(input.bytes(), bytes.as_slice(), "{name}");
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        assert_eq!(input.sha256(), digest, "{name}");
        assert_eq!(dimensions(&bytes, input.mime()), Some((640, 480)), "{name}");
    }
}

#[test]
fn every_side_from_one_to_the_limit_is_accepted() {
    for (w, h) in [(1, 1), (MAX_IMAGE_SIDE_PX, MAX_IMAGE_SIDE_PX), (1, 4096)] {
        validate(&png(w, h), "image/png").unwrap();
        validate(&jpeg(w as u16, h as u16), "image/jpeg").unwrap();
        validate(&webp_vp8(w as u16, h as u16), "image/webp").unwrap();
        validate(&webp_vp8l(w, h), "image/webp").unwrap();
        validate(&webp_vp8x(w, h), "image/webp").unwrap();
    }
}

#[test]
fn a_side_of_4097_px_is_refused_in_every_format() {
    let cases: Vec<(&str, Vec<u8>, &str, &str)> = vec![
        ("png width", png(4097, 1), "image/png", "width 4097"),
        ("png height", png(1, 4097), "image/png", "height 4097"),
        ("jpeg width", jpeg(4097, 1), "image/jpeg", "width 4097"),
        ("jpeg height", jpeg(1, 4097), "image/jpeg", "height 4097"),
        ("vp8", webp_vp8(4097, 1), "image/webp", "width 4097"),
        ("vp8l", webp_vp8l(1, 4097), "image/webp", "height 4097"),
        ("vp8x", webp_vp8x(4097, 1), "image/webp", "width 4097"),
        (
            "vp8x huge",
            webp_vp8x(1, 1 << 24),
            "image/webp",
            "height 16777216",
        ),
        (
            "png huge",
            png(u32::MAX, 1),
            "image/png",
            "width 4294967295",
        ),
    ];
    for (name, bytes, mime, needle) in cases {
        let msg = refusal(&bytes, mime);
        assert!(
            msg.contains(needle) && msg.contains("1..=4096"),
            "{name}: {msg}"
        );
    }
}

#[test]
fn a_zero_side_is_refused() {
    let cases: Vec<(&str, Vec<u8>, &str, &str)> = vec![
        ("png width", png(0, 10), "image/png", "width 0"),
        ("png height", png(10, 0), "image/png", "height 0"),
        // JPEG height 0 means "defined later by DNL": not readable from the
        // header, so refused.
        ("jpeg height", jpeg(10, 0), "image/jpeg", "height 0"),
        ("vp8 width", webp_vp8(0, 10), "image/webp", "width 0"),
    ];
    for (name, bytes, mime, needle) in cases {
        let msg = refusal(&bytes, mime);
        assert!(msg.contains(needle), "{name}: {msg}");
    }
}

#[test]
fn declared_and_sniffed_types_must_match() {
    let cases: Vec<(Vec<u8>, &str, &str)> = vec![
        (
            png(8, 8),
            "image/jpeg",
            "declared image/jpeg but the bytes are image/png",
        ),
        (
            png(8, 8),
            "image/webp",
            "declared image/webp but the bytes are image/png",
        ),
        (
            jpeg(8, 8),
            "image/png",
            "declared image/png but the bytes are image/jpeg",
        ),
        (
            webp_vp8l(8, 8),
            "image/png",
            "declared image/png but the bytes are image/webp",
        ),
        (
            webp_vp8(8, 8),
            "image/jpeg",
            "declared image/jpeg but the bytes are image/webp",
        ),
    ];
    for (bytes, mime, expected) in cases {
        assert_eq!(refusal(&bytes, mime), format!("image: {expected}"));
    }
}

#[test]
fn only_the_three_literal_mime_types_are_declarable() {
    let bytes = png(8, 8);
    for mime in ["image/jpg", "IMAGE/PNG", "image/png; q=1", "image/gif", ""] {
        let msg = refusal(&bytes, mime);
        assert!(msg.contains("must be exactly one of"), "{mime:?}: {msg}");
    }
}

#[test]
fn unknown_magic_bytes_are_refused() {
    let gif = b"GIF89a\x01\x00\x01\x00\x00\x00\x00".to_vec();
    let riff_not_webp = {
        let mut b = webp_vp8(8, 8);
        b[8..12].copy_from_slice(b"WAVE");
        b
    };
    for (name, bytes) in [("gif", gif), ("riff wave", riff_not_webp)] {
        for mime in ["image/png", "image/jpeg", "image/webp"] {
            let msg = refusal(&bytes, mime);
            assert_eq!(
                msg, "image: the bytes are not a PNG, JPEG or WebP image",
                "{name}"
            );
        }
    }
}

#[test]
fn an_empty_image_is_refused() {
    assert_eq!(refusal(&[], "image/png"), "image: no bytes");
}

#[test]
fn the_size_cap_is_inclusive_at_2_mib() {
    // A valid header padded out to exactly the cap is accepted; one byte
    // more is refused before any parsing.
    let mut at_cap = png(8, 8);
    at_cap.resize(MAX_IMAGE_BYTES, 0);
    validate(&at_cap, "image/png").unwrap();

    let mut over = at_cap.clone();
    over.push(0);
    assert_eq!(
        refusal(&over, "image/png"),
        format!(
            "image: {} bytes exceeds the {MAX_IMAGE_BYTES}-byte limit",
            MAX_IMAGE_BYTES + 1
        )
    );
}

#[test]
fn every_truncation_of_a_header_is_refused() {
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("png", png(8, 8), "image/png"),
        ("jpeg", jpeg(8, 8), "image/jpeg"),
        ("vp8", webp_vp8(8, 8), "image/webp"),
        ("vp8l", webp_vp8l(8, 8), "image/webp"),
        ("vp8x", webp_vp8x(8, 8), "image/webp"),
    ];
    for (name, full, mime) in cases {
        // Find the shortest prefix the parser accepts, then check that every
        // prefix shorter than it (but long enough to sniff) is refused as an
        // unreadable header, never as a size or a panic.
        let need = (1..=full.len())
            .find(|&n| validate(&full[..n], mime).is_ok())
            .unwrap_or_else(|| panic!("{name}: no prefix validates"));
        let sniffable = (1..=need).find(|&n| sniff(&full[..n]).is_some()).unwrap();
        for n in sniffable..need {
            let msg = refusal(&full[..n], mime);
            assert!(
                msg.ends_with("header is truncated or unreadable"),
                "{name} at {n} bytes: {msg}"
            );
        }
    }
}

#[test]
fn malformed_headers_are_refused() {
    let png_wrong_chunk = {
        let mut b = png(8, 8);
        b[12..16].copy_from_slice(b"tEXt");
        b
    };
    let png_wrong_ihdr_len = {
        let mut b = png(8, 8);
        b[8..12].copy_from_slice(&12u32.to_be_bytes());
        b
    };
    let jpeg_sof3 = jpeg_with(0xC3, 8, 8);
    let jpeg_scan_first = {
        let mut b = vec![0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x08];
        b.extend_from_slice(&[0; 6]);
        b
    };
    let jpeg_no_marker = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0, 0, 0x12, 0x34];
    let jpeg_short_sof = {
        let mut b = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x07, 8, 0, 8, 0, 8, 1];
        b.extend_from_slice(&[0; 8]);
        b
    };
    let jpeg_zero_len_segment = vec![0xFF, 0xD8, 0xFF, 0xE1, 0x00, 0x00, 0xFF, 0xC0];
    let vp8_bad_start = {
        let mut b = webp_vp8(8, 8);
        b[23] = 0x00;
        b
    };
    let vp8l_bad_sig = {
        let mut b = webp_vp8l(8, 8);
        b[20] = 0x00;
        b
    };
    let webp_alpha_first = {
        let mut b = webp_vp8x(8, 8);
        b[12..16].copy_from_slice(b"ALPH");
        b
    };
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        (
            "png chunk other than IHDR first",
            png_wrong_chunk,
            "image/png",
        ),
        ("png IHDR length not 13", png_wrong_ihdr_len, "image/png"),
        ("jpeg lossless SOF3", jpeg_sof3, "image/jpeg"),
        ("jpeg scan before frame", jpeg_scan_first, "image/jpeg"),
        (
            "jpeg segment not followed by a marker",
            jpeg_no_marker,
            "image/jpeg",
        ),
        ("jpeg frame header too short", jpeg_short_sof, "image/jpeg"),
        (
            "jpeg zero-length segment",
            jpeg_zero_len_segment,
            "image/jpeg",
        ),
        ("vp8 start code", vp8_bad_start, "image/webp"),
        ("vp8l signature", vp8l_bad_sig, "image/webp"),
        ("webp unknown first chunk", webp_alpha_first, "image/webp"),
    ];
    for (name, bytes, mime) in cases {
        let msg = refusal(&bytes, mime);
        assert!(
            msg.ends_with("header is truncated or unreadable"),
            "{name}: {msg}"
        );
    }
}

#[test]
fn jpeg_restart_markers_and_fill_bytes_are_skipped() {
    // RST and TEM markers carry no length; the walk must step over them.
    let mut b = vec![0xFF, 0xD8, 0xFF, 0xD0, 0xFF, 0x01, 0xFF, 0xFF];
    b.extend_from_slice(&jpeg(33, 44)[2..]);
    assert_eq!(jpeg_dimensions(&b), Some((33, 44)));
}

#[test]
fn no_refusal_echoes_the_payload_or_the_declared_string() {
    let marker = "ZZ-PRIVATE-ZZ";
    let mut payload = png(4097, 1);
    payload.extend_from_slice(marker.as_bytes());
    let mut not_an_image = marker.as_bytes().to_vec();
    not_an_image.extend_from_slice(&[0; 32]);
    let mut over = png(8, 8);
    over.extend_from_slice(marker.as_bytes());
    over.resize(MAX_IMAGE_BYTES + 1, b'Z');
    let declared = format!("image/png{marker}");
    let small = png(8, 8);
    for (bytes, mime) in [
        (payload.as_slice(), "image/png"),
        (not_an_image.as_slice(), "image/png"),
        (over.as_slice(), "image/png"),
        (small.as_slice(), declared.as_str()),
        (payload.as_slice(), "image/jpeg"),
    ] {
        let msg = refusal(bytes, mime);
        assert!(!msg.contains("PRIVATE"), "{msg}");
        assert!(msg.len() < 120, "a refusal is one short line: {msg}");
    }
}
