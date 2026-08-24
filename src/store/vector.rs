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

/// The precondition [`encode_vector`] enforces, callable on its own by an
/// adapter that does **not** encode the vector it is about to use.
///
/// # Why this is separate (B-E2E-R2-3)
///
/// E2E-F9 put the zero-norm refusal in the codec because that is where all
/// three sqlx adapters meet. That covers every write path, and it covers the
/// **query** path on the pg family only because those adapters encode the
/// probe before binding it. SQLite never encodes its probe: it hands it to
/// `rank_by_cosine`, and [`crate::embed::cosine`] clamps the denominator with
/// `.max(1e-12)`, so a zero probe scored every row a plausible `0.0` and
/// returned candidates in tie-break order. One contract-violating input, a
/// loud refusal on Postgres and a silent meaningless ranking on SQLite, which
/// is the exact sentence E2E-F9 was filed under.
///
/// So SQLite's `vector_candidates_checked` calls this at the same point in the
/// sequence where the pg family encodes its probe: after the limit checks,
/// before the store is read. Same input, same error, same place, all three
/// adapters.
///
/// Non-finite elements are refused here too, not only zero norms: that is the
/// other half of what encoding the probe was implicitly enforcing on the pg
/// family, and leaving it out would close half of one divergence and keep the
/// other.
pub fn ensure_is_an_embedding(v: &[f32]) -> Result<(), StoreError> {
    if let Some(bad) = v.iter().find(|x| !x.is_finite()) {
        return Err(StoreError::Backend(format!(
            "embedding contains non-finite value {bad} (at index {:?})",
            v.iter().position(|x| !x.is_finite())
        )));
    }
    if !v.is_empty() {
        // Accumulated in f32 on purpose: this is the arithmetic pgvector and
        // Cockroach do, so a vector whose norm underflows to zero for them is
        // refused here rather than becoming a NaN score there.
        let norm_sq: f32 = v.iter().map(|x| x * x).sum();
        // `<= 0.0 || is_nan()` rather than `!(norm_sq > 0.0)`: exactly the same
        // set of refused values, without the negated partial-ord comparison
        // clippy refuses under -D warnings.
        if norm_sq <= 0.0 || norm_sq.is_nan() {
            return Err(StoreError::Backend(format!(
                "embedding has zero norm over {} dimensions, which violates the unit-norm \
                 output contract of Embedder::embed. A vector with no direction has no \
                 cosine to anything: pgvector scores it NaN against every row and \
                 CockroachDB scores it a flat 0.5, so the same data would rank differently \
                 on the two stores. Refusing to write or query it",
                v.len(),
            )));
        }
    }
    Ok(())
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
}
