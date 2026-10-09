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

/// One RIFF chunk: fourcc, little-endian size, payload, a pad byte when the
/// payload length is odd.
fn chunk(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut b = fourcc.to_vec();
    b.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    b.extend_from_slice(payload);
    if payload.len() % 2 == 1 {
        b.push(0);
    }
    b
}

/// A WebP file holding `chunks`, in order.
fn riff_chunks(chunks: &[Vec<u8>]) -> Vec<u8> {
    let body: Vec<u8> = chunks.concat();
    let mut b = b"RIFF".to_vec();
    b.extend_from_slice(&(4 + body.len() as u32).to_le_bytes());
    b.extend_from_slice(b"WEBP");
    b.extend_from_slice(&body);
    b
}

fn riff(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    riff_chunks(&[chunk(fourcc, payload)])
}

/// Lossy `VP8 ` payload: frame tag, start code, 14-bit sides.
fn vp8_payload(w: u16, h: u16) -> Vec<u8> {
    let mut p = vec![0x50, 0x01, 0x00, 0x9D, 0x01, 0x2A];
    p.extend_from_slice(&w.to_le_bytes());
    p.extend_from_slice(&h.to_le_bytes());
    p.extend_from_slice(&[0; 4]);
    p
}

/// Lossless `VP8L` payload: signature, then width-1 and height-1 packed in
/// 14 bits each.
fn vp8l_payload(w: u32, h: u32) -> Vec<u8> {
    let bits = (w - 1) | (h - 1) << 14;
    let mut p = vec![0x2F];
    p.extend_from_slice(&bits.to_le_bytes());
    p.extend_from_slice(&[0; 3]);
    p
}

/// `VP8X` chunk: flags, reserved, 24-bit canvas width-1 and height-1.
fn vp8x_chunk(flags: u8, w: u32, h: u32) -> Vec<u8> {
    let mut p = vec![flags, 0, 0, 0];
    p.extend_from_slice(&(w - 1).to_le_bytes()[..3]);
    p.extend_from_slice(&(h - 1).to_le_bytes()[..3]);
    chunk(b"VP8X", &p)
}

/// Lossy WebP.
fn webp_vp8(w: u16, h: u16) -> Vec<u8> {
    riff(b"VP8 ", &vp8_payload(w, h))
}

/// Lossless WebP.
fn webp_vp8l(w: u32, h: u32) -> Vec<u8> {
    riff(b"VP8L", &vp8l_payload(w, h))
}

/// Extended WebP: a still (alpha-flagged) `VP8X` canvas, then a `VP8L` image
/// chunk of the same size.
fn webp_vp8x(w: u32, h: u32) -> Vec<u8> {
    riff_chunks(&[vp8x_chunk(0x10, w, h), chunk(b"VP8L", &vp8l_payload(w, h))])
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
        assert_eq!(dimensions(&bytes, input.mime()), Ok((640, 480)), "{name}");
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
            "vp8x lossy 4097",
            riff_chunks(&[
                vp8x_chunk(0, 1, 4097),
                chunk(b"VP8 ", &vp8_payload(1, 4097)),
            ]),
            "image/webp",
            "height 4097",
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

#[test]
fn a_vp8x_image_chunk_may_follow_other_chunks() {
    // ICCP and ALPH (odd length, so padded) come before the image chunk in a
    // real extended WebP; the walk steps over them.
    let lossy = riff_chunks(&[
        vp8x_chunk(0x30, 64, 48),
        chunk(b"ICCP", &[7; 12]),
        chunk(b"ALPH", &[1; 5]),
        chunk(b"VP8 ", &vp8_payload(64, 48)),
    ]);
    let input = validate(&lossy, "image/webp").unwrap();
    assert_eq!(dimensions(input.bytes(), input.mime()), Ok((64, 48)));
}

#[test]
fn a_vp8x_canvas_must_equal_its_image_chunk() {
    // M1: a 1x1 canvas over a 16383x16383 bitstream would pass a canvas-only
    // check and make a backend decode 268M pixels.
    let msg = "image: the WebP canvas size differs from the size of its image data";
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "tiny canvas, huge lossless bitstream",
            riff_chunks(&[
                vp8x_chunk(0, 1, 1),
                chunk(b"VP8L", &vp8l_payload(16383, 16383)),
            ]),
        ),
        (
            "lossy bitstream one px wider",
            riff_chunks(&[
                vp8x_chunk(0, 64, 48),
                chunk(b"ALPH", &[1; 3]),
                chunk(b"VP8 ", &vp8_payload(65, 48)),
            ]),
        ),
        (
            "canvas larger than the bitstream",
            riff_chunks(&[
                vp8x_chunk(0, 4096, 4096),
                chunk(b"VP8L", &vp8l_payload(8, 8)),
            ]),
        ),
    ];
    for (name, bytes) in cases {
        assert_eq!(refusal(&bytes, "image/webp"), msg, "{name}");
    }
}

#[test]
fn an_animated_webp_is_refused() {
    let msg = "image: animated WebP is not accepted; send one still image";
    let frame = {
        // ANMF: frame offset/size/duration/flags (16 bytes), then the frame's
        // own image chunk.
        let mut p = vec![0; 16];
        p.extend_from_slice(&chunk(b"VP8L", &vp8l_payload(8, 8)));
        chunk(b"ANMF", &p)
    };
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "animation flag",
            riff_chunks(&[
                vp8x_chunk(0x02, 8, 8),
                chunk(b"ANIM", &[0; 6]),
                frame.clone(),
            ]),
        ),
        (
            // Flag set, even with a still image chunk after it.
            "animation flag over a still chunk",
            riff_chunks(&[vp8x_chunk(0x12, 8, 8), chunk(b"VP8L", &vp8l_payload(8, 8))]),
        ),
        (
            "ANMF frame without the flag",
            riff_chunks(&[vp8x_chunk(0, 8, 8), frame.clone(), frame]),
        ),
    ];
    for (name, bytes) in cases {
        assert_eq!(refusal(&bytes, "image/webp"), msg, "{name}");
    }
}

#[test]
fn a_vp8x_without_a_readable_image_chunk_is_refused() {
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("canvas only", riff_chunks(&[vp8x_chunk(0, 8, 8)])),
        (
            "metadata only",
            riff_chunks(&[vp8x_chunk(0x08, 8, 8), chunk(b"EXIF", &[0; 9])]),
        ),
        (
            "a chunk size running past the end",
            riff_chunks(&[vp8x_chunk(0, 8, 8), {
                let mut c = chunk(b"ICCP", &[0; 4]);
                c[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
                c
            }]),
        ),
        (
            "bad lossless signature",
            riff_chunks(&[
                vp8x_chunk(0, 8, 8),
                chunk(b"VP8L", &{
                    let mut p = vp8l_payload(8, 8);
                    p[0] = 0;
                    p
                }),
            ]),
        ),
    ];
    for (name, bytes) in cases {
        let msg = refusal(&bytes, "image/webp");
        assert!(
            msg.ends_with("header is truncated or unreadable"),
            "{name}: {msg}"
        );
    }
}

#[test]
fn image_chunks_must_be_well_formed_key_frames_inside_the_riff() {
    let unreadable = |name: &str, bytes: Vec<u8>| {
        let msg = refusal(&bytes, "image/webp");
        assert!(
            msg.ends_with("header is truncated or unreadable"),
            "{name}: {msg}"
        );
    };
    let shrink_size = |mut c: Vec<u8>, size: u32| {
        c[4..8].copy_from_slice(&size.to_le_bytes());
        c
    };
    // A declared chunk size shorter than the header read from it.
    unreadable(
        "short VP8 chunk",
        riff_chunks(&[shrink_size(chunk(b"VP8 ", &vp8_payload(8, 8)), 9)]),
    );
    unreadable(
        "short VP8L chunk",
        riff_chunks(&[shrink_size(chunk(b"VP8L", &vp8l_payload(8, 8)), 4)]),
    );
    unreadable(
        "short VP8X image chunk",
        riff_chunks(&[
            vp8x_chunk(0, 8, 8),
            shrink_size(chunk(b"VP8L", &vp8l_payload(8, 8)), 4),
        ]),
    );
    // A lossy inter frame, and a lossless stream of a later version.
    let mut inter = vp8_payload(8, 8);
    inter[0] |= 1;
    unreadable("VP8 inter frame", riff(b"VP8 ", &inter));
    let mut v1 = vp8l_payload(8, 8);
    v1[4] |= 0x20;
    unreadable("VP8L version 1", riff(b"VP8L", &v1));
    // An image chunk after the RIFF end is not part of the file.
    let mut past_end = riff_chunks(&[vp8x_chunk(0, 8, 8)]);
    past_end.extend_from_slice(&chunk(b"VP8L", &vp8l_payload(8, 8)));
    unreadable("image chunk past the RIFF end", past_end);
}
