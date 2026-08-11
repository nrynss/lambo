//! Deterministic 1024-dim embedder for offline tests (no AWS).

use async_trait::async_trait;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use super::math::cosine;
use super::{EmbedError, Embedder};

/// Documented near/far pairs for tests (T1.3 / T7.2):
/// - NEAR_A / NEAR_B: cosine ≥ 0.85 (same seed family)
/// - FAR: cosine with NEAR_A well below 0.85
pub const NEAR_A: &str = "register user";
pub const NEAR_B: &str = "create account";
pub const FAR: &str = "quantum chromodynamics lattice gauge";

/// Fixture texts that must be near each other under [`FixtureEmbedder`].
pub const NEAR_PAIR: (&str, &str) = (NEAR_A, NEAR_B);

/// Public helper so downstream tests can assert the near/far contract without
/// re-deriving thresholds.
pub fn near_far_contract() -> (f32, f32) {
    let near = cosine(
        &FixtureEmbedder::embed_sync(NEAR_PAIR.0),
        &FixtureEmbedder::embed_sync(NEAR_PAIR.1),
    );
    let far = cosine(
        &FixtureEmbedder::embed_sync(NEAR_A),
        &FixtureEmbedder::embed_sync(FAR),
    );
    (near, far)
}

const DIM: usize = 1024;

/// Hash-seeded unit vectors. Related phrases share a base seed so they land near each other.
///
/// **Stability:** uses [`std::collections::hash_map::DefaultHasher`], which is **not**
/// guaranteed stable across Rust releases. Fixture golden vectors must be regenerated if
/// the toolchain major hash algorithm changes; prefer asserting relative geometry
/// (near/far) over absolute component equality across rustc versions.
#[derive(Debug, Clone, Default)]
pub struct FixtureEmbedder;

impl FixtureEmbedder {
    pub fn new() -> Self {
        Self
    }

    fn seed_for(text: &str) -> u64 {
        // Normalize lightly so "Register User" ≈ "register user"
        let norm = text.trim().to_lowercase();
        // Map known near-pair to the same family seed.
        let family = match norm.as_str() {
            "register user" | "register_user" | "create account" | "create_user" => {
                "family:user-registration"
            }
            other => other,
        };
        let mut h = DefaultHasher::new();
        family.hash(&mut h);
        h.finish()
    }

    pub fn embed_sync(text: &str) -> Vec<f32> {
        let seed = Self::seed_for(text);
        let mut v = Vec::with_capacity(DIM);
        let mut state = seed;
        for i in 0..DIM {
            // xorshift-ish
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state = state.wrapping_add(i as u64 + 1);
            let x = ((state % 10_000) as f32 / 10_000.0) * 2.0 - 1.0;
            v.push(x);
        }
        // Tiny text-dependent perturbation (0.5%) so same-family strings are not
        // bit-identical but cosine stays well above semantic_match_threshold (0.85).
        let mut h = DefaultHasher::new();
        text.trim().to_lowercase().hash(&mut h);
        let tseed = h.finish();
        let mut state = tseed;
        for x in &mut v {
            state ^= state << 13;
            state ^= state >> 7;
            let delta = ((state % 1000) as f32 / 1000.0 - 0.5) * 0.005;
            *x += delta;
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
        for x in &mut v {
            *x /= norm;
        }
        v
    }
}

#[async_trait]
impl Embedder for FixtureEmbedder {
    fn dimensions(&self) -> usize {
        DIM
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        Ok(Self::embed_sync(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn deterministic() {
        let e = FixtureEmbedder::new();
        let a = e.embed("user schema").await.unwrap();
        let b = e.embed("user schema").await.unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 1024);
    }

    #[test]
    fn near_pair_above_threshold() {
        let a = FixtureEmbedder::embed_sync(NEAR_A);
        let b = FixtureEmbedder::embed_sync(NEAR_B);
        let sim = cosine(&a, &b);
        assert!(
            sim >= 0.85,
            "near pair cosine {sim} should be >= 0.85 (NEAR_A={NEAR_A:?}, NEAR_B={NEAR_B:?})"
        );
    }

    #[test]
    fn far_pair_below_threshold() {
        let a = FixtureEmbedder::embed_sync(NEAR_A);
        let f = FixtureEmbedder::embed_sync(FAR);
        let sim = cosine(&a, &f);
        assert!(
            sim < 0.85,
            "far pair cosine {sim} should be < 0.85 (NEAR_A vs FAR)"
        );
    }

    #[test]
    fn unit_norm() {
        let v = FixtureEmbedder::embed_sync("anything");
        let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((n - 1.0).abs() < 1e-5, "norm={n}");
    }

    #[test]
    fn near_far_contract_helper() {
        let (near, far) = near_far_contract();
        assert!(near >= 0.85);
        assert!(far < 0.85);
    }
}
