//! Shared dense-vector (de)serialization for the sqlx-backed adapters
//! (CON-8: flush→load parity — Cockroach and SQLite must round-trip
//! `Concept.embedding` identically).
//!
//! The wire form is the T0.3 "Attempt A" text literal `[x,y,z]` with Rust's
//! shortest-round-trip `f32` `Display` (spike-verified exact at eps=1e-4 over
//! 1024 dims). Cockroach binds the text and casts `$n::VECTOR` server-side and
//! reads it back via `embedding::STRING`; SQLite stores the same text as a
//! BLOB. One codec, one format — no adapter-specific skew.

use crate::types::StoreError;

/// Encode an embedding as the `[x,y,z]` text literal. Rejects non-finite
/// elements (a `NaN`/`Inf` vector is not a legal embedding and Cockroach would
/// reject the literal).
pub fn encode_vector(v: &[f32]) -> Result<String, StoreError> {
    if let Some(bad) = v.iter().find(|x| !x.is_finite()) {
        return Err(StoreError::Backend(format!(
            "embedding contains non-finite value {bad} (at index {:?})",
            v.iter().position(|x| !x.is_finite())
        )));
    }
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
}
