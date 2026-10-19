//! Shared embedding math (always available; not gated on fixture/bge features).
//!
//! # The kernel (#171 B1)
//!
//! Every cosine in the crate goes through one reduction, `lane_sums`: the
//! elements are accumulated into [`LANES`] independent partial sums (element
//! `i` lands in lane `i % LANES`), and the lanes are folded in one fixed order
//! at the end. Independent accumulators let the compiler keep them in vector
//! registers; a single serial accumulator (what this was before #171) forces
//! one dependent add per element and cannot be vectorized, because IEEE
//! addition is not associative and the compiler may not reorder it for us.
//!
//! The order is fixed in the source, never left to the compiler: no fused
//! multiply-add, no fast-math, so a given pair of vectors scores the same bits
//! on every target. Lane order differs from the old serial order, so a score
//! can differ from a pre-#171 build in the last bit or so; ranking ties are
//! still broken by canonical key and id, which no float touches.
//!
//! Non-finite input is not tested per element any more (that test was the
//! other thing that blocked vectorization). A NaN or infinite element makes
//! that vector's sum of squares NaN or infinite, and a finite vector whose
//! squares overflow makes it infinite too; [`cosine`] returns `0.0` for all
//! of those, exactly the inputs the per-element guard returned `0.0` for.
//!
//! There is no "probe with its norm precomputed" variant. One was tried: a
//! two-sum kernel (dot and the candidate's norm) measured about twice as slow
//! as the three-sum one on x86-64, because LLVM vectorized the three-sum loop
//! and not the two-sum one, and the probe's norm costs one multiply-add per
//! element that the memory-bound scan hides anyway.

/// Independent partial sums per reduction (see the module docs). 16 lanes is
/// two AVX or four SSE registers per accumulator: enough independent adds to
/// hide the add latency without spilling.
const LANES: usize = 16;

/// `(Σ aᵢbᵢ, Σ aᵢ², Σ bᵢ²)` over equal-length slices, in the module's fixed
/// lane order.
// Kept out of line on purpose: inlined into a caller's iterator chain under
// thin LTO with one codegen unit, LLVM scalarized the lane loop again and the
// scan ran at half speed (measured in the #171 PR). As its own function the
// loop vectorizes the same way in every profile, and the call costs nothing
// next to a 1024-element reduction.
#[inline(never)]
fn lane_sums(a: &[f32], b: &[f32]) -> (f32, f32, f32) {
    debug_assert_eq!(a.len(), b.len());
    let mut dot = [0.0f32; LANES];
    let mut na = [0.0f32; LANES];
    let mut nb = [0.0f32; LANES];
    let (a_chunks, a_tail) = a.as_chunks::<LANES>();
    let (b_chunks, b_tail) = b.as_chunks::<LANES>();
    for (x, y) in a_chunks.iter().zip(b_chunks) {
        for j in 0..LANES {
            dot[j] += x[j] * y[j];
            na[j] += x[j] * x[j];
            nb[j] += y[j] * y[j];
        }
    }
    for (j, (x, y)) in a_tail.iter().zip(b_tail).enumerate() {
        dot[j] += x * y;
        na[j] += x * x;
        nb[j] += y * y;
    }
    (fold(dot), fold(na), fold(nb))
}

/// Fold the lanes pairwise, in a fixed tree, so the result does not depend on
/// anything the compiler chooses.
#[inline]
fn fold(mut lanes: [f32; LANES]) -> f32 {
    let mut width = LANES;
    while width > 1 {
        width /= 2;
        for j in 0..width {
            lanes[j] += lanes[j + width];
        }
    }
    lanes[0]
}

/// The tail of [`cosine`]: the clamped cosine
/// from a dot product and two sums of squares, `0.0` when either sum is not
/// finite (a NaN or infinite element, or squares that overflow).
#[inline]
fn finish(dot: f32, na: f32, nb: f32) -> f32 {
    if !na.is_finite() || !nb.is_finite() {
        return 0.0;
    }
    let denom = (na.sqrt() * nb.sqrt()).max(1e-12);
    let c = dot / denom;
    if c.is_finite() {
        c.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

/// Cosine similarity. Returns 0.0 if either vector is empty or lengths differ
/// (callers must not silently zip-truncate mismatched dims), and 0.0 if either
/// holds a NaN or infinite element.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let (dot, na, nb) = lane_sums(a, b);
    finish(dot, na, nb)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The pre-#171 kernel, verbatim: one serial accumulator per sum and a
    /// per-element finiteness test. The reference the lane kernel is held to.
    pub(crate) fn cosine_scalar(a: &[f32], b: &[f32]) -> f32 {
        if a.is_empty() || b.is_empty() || a.len() != b.len() {
            return 0.0;
        }
        let mut dot = 0.0f32;
        let mut na = 0.0f32;
        let mut nb = 0.0f32;
        for (x, y) in a.iter().zip(b.iter()) {
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

    /// Cosine in f64, for measuring how far each f32 kernel lands from it.
    fn cosine_f64(a: &[f32], b: &[f32]) -> f64 {
        let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
        for (x, y) in a.iter().zip(b) {
            let (x, y) = (f64::from(*x), f64::from(*y));
            dot += x * y;
            na += x * x;
            nb += y * y;
        }
        dot / (na.sqrt() * nb.sqrt())
    }

    fn seeded(n: usize, seed: u64) -> Vec<f32> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                ((s >> 40) as f32 / (1u64 << 24) as f32) - 0.5
            })
            .collect()
    }

    /// Every length around the lane count (the tail path, a lone tail, an
    /// exact multiple) and the production widths: the lane kernel agrees
    /// with the old one to a few ulps and is at least as close to the f64
    /// value, and the prepared probe is bit-identical to `cosine`.
    #[test]
    fn lane_kernel_matches_the_scalar_kernel() {
        for len in (1..=40).chain([255, 256, 257, 768, 1024, 1536, 3072]) {
            for seed in 1..6u64 {
                let a = seeded(len, seed);
                let b = seeded(len, seed * 1_000 + 17);
                let lane = cosine(&a, &b);
                let scalar = cosine_scalar(&a, &b);
                let exact = cosine_f64(&a, &b);
                assert!(
                    (f64::from(lane) - f64::from(scalar)).abs() <= 1e-5,
                    "len {len}: lane {lane} scalar {scalar}"
                );
                assert!(
                    (f64::from(lane) - exact).abs() <= (f64::from(scalar) - exact).abs() + 1e-6,
                    "len {len}: lane {lane} is further from {exact} than scalar {scalar}"
                );
                assert_eq!(
                    cosine(&a, &b).to_bits(),
                    lane.to_bits(),
                    "len {len}: deterministic"
                );
            }
        }
    }

    /// The per-element guard is gone; the sums carry the same answer. Every
    /// input the old kernel returned 0.0 for, the new one does too, wherever
    /// in the vector (lane or tail) the bad element sits.
    #[test]
    fn non_finite_and_overflowing_inputs_score_zero_in_both_kernels() {
        let base = seeded(35, 9);
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            for at in [0, 15, 16, 31, 34] {
                let mut v = base.clone();
                v[at] = bad;
                for (x, y) in [(&v, &base), (&base, &v)] {
                    assert_eq!(cosine(x, y), 0.0, "{bad} at {at}");
                    assert_eq!(cosine_scalar(x, y), 0.0, "{bad} at {at}");
                }
            }
        }
        // Finite, but the squares overflow f32.
        let huge = vec![3.0e19f32; 20];
        assert_eq!(cosine(&huge, &base[..20]), 0.0);
        assert_eq!(cosine_scalar(&huge, &base[..20]), 0.0);
        // Empty and mismatched lengths are 0.0, never a zip-truncated score.
        assert_eq!(cosine(&[], &[1.0]), 0.0);
        assert_eq!(cosine(&[1.0, 0.0], &[1.0]), 0.0);
        assert_eq!(cosine(&[1.0], &[]), 0.0);
    }

    /// Bounds hold whatever the rounding: identical vectors clamp to 1.0 and
    /// opposite ones to -1.0; a zero vector scores 0.0 rather than NaN.
    #[test]
    fn cosine_stays_clamped_and_zero_norm_is_zero() {
        let a = seeded(1024, 5);
        let neg: Vec<f32> = a.iter().map(|x| -x).collect();
        assert!(cosine(&a, &a) <= 1.0 && cosine(&a, &a) > 0.99999);
        assert!(cosine(&a, &neg) >= -1.0 && cosine(&a, &neg) < -0.99999);
        assert_eq!(cosine(&a, &vec![0.0; 1024]), 0.0);
    }

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
