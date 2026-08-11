//! Deterministic offline embedder for tests (no network).
//!
//! Default width is 1024 (matches common BGE/Cockroach demos). Width is configurable —
//! dim is not a global product constant; store×embedder resolution enforces schema match.

use async_trait::async_trait;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use super::math::cosine;
use super::{EmbedError, Embedder};

/// Documented near/far pairs for tests (T1.3 / T7.2) at the **default** dim (1024):
/// - NEAR_A / NEAR_B: cosine ≥ 0.85 (same seed family)
/// - FAR: cosine with NEAR_A well below 0.85
pub const NEAR_A: &str = "register user";
pub const NEAR_B: &str = "create account";
pub const FAR: &str = "quantum chromodynamics lattice gauge";

/// Fixture texts that must be near each other under [`FixtureEmbedder`].
pub const NEAR_PAIR: (&str, &str) = (NEAR_A, NEAR_B);

/// Default fixture width (demo convenience, not a schema law).
pub const DEFAULT_FIXTURE_DIM: usize = 1024;

/// Public helper so downstream tests can assert the near/far contract without
/// re-deriving thresholds (always at [`DEFAULT_FIXTURE_DIM`]).
pub fn near_far_contract() -> (f32, f32) {
    let e = FixtureEmbedder::new();
    let near = cosine(&e.embed_sync(NEAR_PAIR.0), &e.embed_sync(NEAR_PAIR.1));
    let far = cosine(&e.embed_sync(NEAR_A), &e.embed_sync(FAR));
    (near, far)
}

/// Hash-seeded unit vectors. Related phrases share a base seed so they land near each other.
///
/// **Stability:** uses [`std::collections::hash_map::DefaultHasher`], which is **not**
/// guaranteed stable across Rust releases. Prefer asserting relative geometry
/// (near/far) over absolute component equality across rustc versions.
#[derive(Debug, Clone)]
pub struct FixtureEmbedder {
    dim: usize,
}

impl Default for FixtureEmbedder {
    fn default() -> Self {
        Self::new()
    }
}

impl FixtureEmbedder {
    /// Default 1024-d fixture embedder.
    pub fn new() -> Self {
        Self {
            dim: DEFAULT_FIXTURE_DIM,
        }
    }

    /// Fixture embedder with an explicit width (`dim > 0`).
    pub fn with_dimensions(dim: usize) -> Result<Self, EmbedError> {
        if dim == 0 {
            return Err(EmbedError::Unavailable(
                "fixture embedder dim must be > 0".into(),
            ));
        }
        Ok(Self { dim })
    }

    fn seed_for(text: &str) -> u64 {
        let norm = text.trim().to_lowercase();
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

    pub fn embed_sync(&self, text: &str) -> Vec<f32> {
        let seed = Self::seed_for(text);
        let mut v = Vec::with_capacity(self.dim);
        let mut state = seed;
        for i in 0..self.dim {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state = state.wrapping_add(i as u64 + 1);
            let x = ((state % 10_000) as f32 / 10_000.0) * 2.0 - 1.0;
            v.push(x);
        }
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
        self.dim
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        Ok(self.embed_sync(text))
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
        assert_eq!(a.len(), DEFAULT_FIXTURE_DIM);
    }

    #[test]
    fn near_pair_above_threshold() {
        let e = FixtureEmbedder::new();
        let a = e.embed_sync(NEAR_A);
        let b = e.embed_sync(NEAR_B);
        let sim = cosine(&a, &b);
        assert!(
            sim >= 0.85,
            "near pair cosine {sim} should be >= 0.85 (NEAR_A={NEAR_A:?}, NEAR_B={NEAR_B:?})"
        );
    }

    #[test]
    fn far_pair_below_threshold() {
        let e = FixtureEmbedder::new();
        let a = e.embed_sync(NEAR_A);
        let f = e.embed_sync(FAR);
        let sim = cosine(&a, &f);
        assert!(
            sim < 0.85,
            "far pair cosine {sim} should be < 0.85 (NEAR_A vs FAR)"
        );
    }

    #[test]
    fn unit_norm() {
        let e = FixtureEmbedder::new();
        let v = e.embed_sync("anything");
        let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((n - 1.0).abs() < 1e-5, "norm={n}");
    }

    #[test]
    fn near_far_contract_helper() {
        let (near, far) = near_far_contract();
        assert!(near >= 0.85);
        assert!(far < 0.85);
    }

    #[test]
    fn custom_dim() {
        let e = FixtureEmbedder::with_dimensions(64).unwrap();
        assert_eq!(e.dimensions(), 64);
        assert_eq!(e.embed_sync("x").len(), 64);
        assert!(FixtureEmbedder::with_dimensions(0).is_err());
    }
}
