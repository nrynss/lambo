//! Shared dense-vector (de)serialization for the sqlx-backed adapters
//! (CON-8: flush→load parity — Cockroach and SQLite must round-trip
//! `Concept.embedding` identically).
//!
//! The wire form is the T0.3 "Attempt A" text literal `[x,y,z]` with Rust's
//! shortest-round-trip `f32` `Display` (spike-verified exact at eps=1e-4 over
//! 1024 dims). Cockroach binds the text and casts `$n::VECTOR` server-side and
//! reads it back via `embedding::STRING`. SQLite stored the same text as a
//! BLOB until #55; it now stores packed little-endian `f32`
//! ([`encode_vector_blob`]), which round-trips bit-exactly, and keeps the text
//! decoder only for the `lambo provision` transcode of older stores.

use crate::types::StoreError;

// Moved to the always-compiled seam module (#8): the graph-backed vector
// source refuses a probe that is not an embedding exactly as the adapters do,
// and it runs in builds without a SQL adapter, where this module is absent.
pub use super::vector_source::ensure_is_an_embedding;

/// Encode an embedding as the `[x,y,z]` text literal. Rejects non-finite
/// elements (a `NaN`/`Inf` vector is not a legal embedding and Cockroach would
/// reject the literal) and zero-norm vectors (E2E-F9).
///
/// # Why zero norm is refused here (E2E-F9)
///
/// [`crate::embed::Embedder::embed`]'s documented output contract is unit norm,
/// and nothing enforced it. A zero vector is finite, so the finite-element
/// check passed it, and the two dialects then disagreed about it: pgvector's
/// `<=>` is `NaN` against every row, so `distance_to_score` propagated `NaN`
/// into ranking, while Cockroach's `<->` is a finite `1` and scored a steady
/// `0.5`. One contract-violating row, two different answers, neither of them a
/// refusal.
///
/// Refused in the shared codec rather than in one adapter because that is the
/// one place both dialects and SQLite pass through, and because a vector with
/// no direction has no cosine to any other vector on any of them: it is not a
/// backend quirk to paper over, it is not an embedding.
///
/// The empty slice is not refused: it carries no direction either, but it is a
/// separate degenerate case (a width-zero embedding), it is rejected earlier by
/// `check_embedding_dim` on every path that has a configured width, and the
/// codec's own round-trip pin covers `dim = 0`.
// A SQLite-only build writes packed f32 (#55) and only decodes text (the
// provision transcode); the encoder is the pg family's.
#[cfg_attr(
    not(any(feature = "store-cockroach", feature = "store-postgres")),
    allow(dead_code)
)]
pub fn encode_vector(v: &[f32]) -> Result<String, StoreError> {
    ensure_is_an_embedding(v)?;
    let mut s = String::with_capacity(v.len() * 8);
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!("{x}"));
    }
    s.push(']');
    Ok(s)
}

/// Inverse of [`encode_vector`] — parses the text literal read-back form.
pub fn decode_vector(s: &str) -> Result<Vec<f32>, StoreError> {
    let t = s.trim().trim_start_matches('[').trim_end_matches(']');
    if t.is_empty() {
        return Ok(Vec::new());
    }
    t.split(',')
        .map(|p| {
            p.trim().parse::<f32>().map_err(|e| {
                StoreError::Backend(format!("decode VECTOR element {p:?} from {s:?}: {e}"))
            })
        })
        .collect()
}

/// The SQLite codec name recorded in `store_meta` (`key = 'vector_codec'`)
/// once every `concepts.embedding` holds packed little-endian `f32` (#55).
/// A store without the row was written by a pre-#55 build, whose rows are the
/// CON-8 text literal; `lambo provision` transcodes those and records this.
#[cfg(feature = "store-sqlite")]
pub const SQLITE_VECTOR_CODEC: &str = "f32le";

/// SQLite's stored form of an embedding (#55): `concepts.embedding` is a
/// `BLOB` of packed little-endian `f32`, 4 bytes per element, no header. The
/// width is the BLOB length over 4; the session contract checks it on read.
///
/// Refuses what [`encode_vector`] refuses (non-finite elements, zero norm), so
/// the SQLite and SQL-text paths accept exactly the same vectors.
///
/// The SQLite write path (`concept_binds`), session load (`load_concepts`) and
/// the vector candidate scan (`select_session_vectors`) all go through this
/// pair and [`decode_vector_blob`]. The CON-8 text codec stays for the pg
/// family, which binds text.
#[cfg(feature = "store-sqlite")]
pub fn encode_vector_blob(v: &[f32]) -> Result<Vec<u8>, StoreError> {
    ensure_is_an_embedding(v)?;
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    Ok(out)
}

/// Inverse of [`encode_vector_blob`], bit-exact. `owner` names the row in the
/// error (the concept id): a BLOB whose length is not a multiple of 4 is a
/// corrupt row (or an untranscoded text row), reported as
/// [`StoreError::Backend`], never a panic.
#[cfg(feature = "store-sqlite")]
pub fn decode_vector_blob(owner: &str, bytes: &[u8]) -> Result<Vec<f32>, StoreError> {
    if !bytes.len().is_multiple_of(4) {
        return Err(StoreError::Backend(format!(
            "concepts.embedding for {owner} is {} bytes, not packed f32 (a multiple of 4)",
            bytes.len()
        )));
    }
    Ok(bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect())
}

/// Decode a pre-#55 SQLite row: the UTF-8 bytes of the CON-8 text literal.
/// Used only by the `lambo provision` transcode; nothing else reads text rows.
#[cfg(feature = "store-sqlite")]
pub fn decode_legacy_text_blob(owner: &str, bytes: &[u8]) -> Result<Vec<f32>, StoreError> {
    let text = std::str::from_utf8(bytes).map_err(|e| {
        StoreError::Backend(format!(
            "concepts.embedding for {owner} is not valid UTF-8 text: {e}"
        ))
    })?;
    decode_vector(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_vec(dim: usize, seed: f32) -> Vec<f32> {
        (0..dim)
            .map(|i| ((i as f32 + 1.0) * seed).sin() * 0.5)
            .collect()
    }

    #[test]
    fn vector_encode_decode_roundtrip_exact() {
        for dim in [0usize, 1, 8, 1024] {
            let v = sample_vec(dim, 0.17);
            let text = encode_vector(&v).unwrap();
            assert!(text.starts_with('[') && text.ends_with(']'));
            let back = decode_vector(&text).unwrap();
            assert_eq!(
                v, back,
                "dim {dim}: encode -> decode must be exact (shortest f32 repr)"
            );
        }
    }

    /// E2E-F9: a zero vector is finite and used to encode cleanly, after which
    /// pgvector answered `NaN` and Cockroach answered `1` for the same data.
    /// Delete the zero-norm branch in `encode_vector` and this fails.
    #[test]
    fn encode_refuses_a_zero_norm_embedding() {
        let err = encode_vector(&[0.0; 8])
            .expect_err("a zero vector is not an embedding")
            .to_string();
        assert!(err.contains("zero norm"), "{err}");
        assert!(err.contains("unit-norm"), "{err}");
        // Named for both stores, because the whole point is that they disagree.
        assert!(err.contains("NaN"), "{err}");

        // Underflow: every component is non-zero but the f32 norm is not.
        assert!(
            encode_vector(&[1e-30_f32; 8]).is_err(),
            "a norm that underflows to zero is the same defect one step further away"
        );

        // A tiny but representable direction is still a direction.
        assert!(encode_vector(&[1e-6_f32, 0.0, 0.0]).is_ok());
        // The empty slice keeps its existing meaning (see the doc comment).
        assert_eq!(encode_vector(&[]).unwrap(), "[]");
    }

    #[test]
    fn vector_decode_accepts_cockroach_renderings() {
        // Cockroach `embedding::STRING` output has no spaces; tolerate any whitespace.
        assert_eq!(
            decode_vector("[0.5,-0.25,1e2]").unwrap(),
            vec![0.5, -0.25, 100.0]
        );
        assert_eq!(decode_vector("[]").unwrap(), Vec::<f32>::new());
        assert_eq!(decode_vector(" [ 1 , 2 ] ").unwrap(), vec![1.0, 2.0]);
        assert!(decode_vector("[1,oops]").is_err());
    }

    #[test]
    fn vector_encode_rejects_non_finite() {
        let mut v = sample_vec(4, 1.0);
        v[2] = f32::NAN;
        assert!(encode_vector(&v).is_err());
        v[2] = f32::INFINITY;
        assert!(encode_vector(&v).is_err());
    }

    /// #55 (CON-8): the packed SQLite form round-trips every `f32` bit-exactly,
    /// including the values a decimal form is most likely to bend.
    #[cfg(feature = "store-sqlite")]
    #[test]
    fn packed_blob_round_trips_bit_exactly() {
        let edge = vec![
            1.0,
            -0.0,
            f32::MIN_POSITIVE,
            f32::from_bits(1), // smallest subnormal
            f32::MAX,
            f32::MIN,
            f32::EPSILON,
            0.1,
            -1.0 / 3.0,
        ];
        for v in [
            edge,
            sample_vec(1, 0.3),
            sample_vec(8, 0.17),
            sample_vec(1024, 0.17),
        ] {
            let blob = encode_vector_blob(&v).unwrap();
            assert_eq!(blob.len(), v.len() * 4, "4 bytes per element, no header");
            let back = decode_vector_blob("c", &blob).unwrap();
            let bits = |v: &[f32]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
            assert_eq!(bits(&v), bits(&back));
        }
        // Little-endian, fixed: 1.0f32 is 0x3f800000.
        assert_eq!(encode_vector_blob(&[1.0]).unwrap(), vec![0, 0, 0x80, 0x3f]);
        assert!(decode_vector_blob("c", &[]).unwrap().is_empty());
    }

    /// #55: the packed codec refuses what the text codec refuses, and a BLOB
    /// that is not whole `f32`s is a named Backend error, never a panic.
    #[cfg(feature = "store-sqlite")]
    #[test]
    fn packed_blob_refuses_bad_vectors_and_ragged_rows() {
        assert!(encode_vector_blob(&[0.0; 4]).is_err());
        assert!(encode_vector_blob(&[f32::NAN, 1.0]).is_err());
        assert!(encode_vector_blob(&[f32::INFINITY]).is_err());
        let err = decode_vector_blob("concept-x", b"[1,0,0]").unwrap_err();
        assert!(matches!(err, StoreError::Backend(_)), "{err:?}");
        assert!(err.to_string().contains("concept-x"), "{err}");
    }

    /// #55: a pre-#55 text row decodes to the same vector the old codec wrote,
    /// so the transcode is exact.
    #[cfg(feature = "store-sqlite")]
    #[test]
    fn legacy_text_rows_decode_exactly() {
        let v = sample_vec(1024, 0.29);
        let text = encode_vector(&v).unwrap().into_bytes();
        assert_eq!(decode_legacy_text_blob("c", &text).unwrap(), v);
        assert!(decode_legacy_text_blob("c", &[0xff, 0xfe]).is_err());
    }
}
