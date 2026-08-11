//! Shared embedding math (always available; not gated on fixture/bge features).

/// Cosine similarity. Returns 0.0 if either vector is empty or lengths differ
/// (callers must not silently zip-truncate mismatched dims).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        // Guard NaN/Inf pollution from bad backends.
        if !x.is_finite() || !y.is_finite() {
            return 0.0;
        }
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = (na.sqrt() * nb.sqrt()).max(1e-12);
    let c = dot / denom;
    if c.is_finite() {
        c.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_rejects_length_mismatch() {
        assert_eq!(cosine(&[1.0, 0.0], &[1.0]), 0.0);
        assert_eq!(cosine(&[], &[1.0]), 0.0);
    }

    #[test]
    fn cosine_unit_vectors() {
        let a = [1.0f32, 0.0];
        let b = [1.0f32, 0.0];
        assert!((cosine(&a, &b) - 1.0).abs() < 1e-6);
        let c = [0.0f32, 1.0];
        assert!(cosine(&a, &c).abs() < 1e-6);
    }

    #[test]
    fn cosine_nan_returns_zero() {
        assert_eq!(cosine(&[f32::NAN], &[1.0]), 0.0);
    }
}
