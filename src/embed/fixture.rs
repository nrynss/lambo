//! Deterministic offline embedder for tests (no network).
//!
//! Default width is 1024 (matches common BGE/Cockroach demos). Width is configurable —
//! dim is not a global product constant; store×embedder resolution enforces schema match.

use async_trait::async_trait;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use super::math::cosine;
use super::{EmbedError, Embedder, ImageInput, Modalities};

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
/// re-deriving thresholds (always at `DEFAULT_FIXTURE_DIM`).
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

/// The PNG `tEXt` keyword [`FixtureEmbedder::embed_image`] reads its label
/// from (#22, design section 9).
pub const IMAGE_LABEL_KEYWORD: &str = "lambo-label";

impl FixtureEmbedder {
    /// The label a fixture image carries: the text of the first PNG `tEXt`
    /// chunk whose keyword is [`IMAGE_LABEL_KEYWORD`].
    ///
    /// Found by a byte search for `tEXtlambo-label\0`, with no PNG decoding
    /// and no CRC check. The chunk's own length field bounds the label, so a
    /// truncated or lying chunk yields `None`, never an out-of-bounds read.
    /// A label that is empty, blank or not UTF-8 is `None` too.
    pub fn image_label(bytes: &[u8]) -> Option<&str> {
        let marker: Vec<u8> = [b"tEXt".as_slice(), IMAGE_LABEL_KEYWORD.as_bytes(), b"\0"].concat();
        let at = bytes.windows(marker.len()).position(|w| w == marker)?;
        // The 4-byte big-endian length sits just before the chunk type.
        let len_at = at.checked_sub(4)?;
        let len = u32::from_be_bytes(bytes.get(len_at..at)?.try_into().ok()?) as usize;
        let data_start = at + 4;
        let label_start = data_start + IMAGE_LABEL_KEYWORD.len() + 1;
        let data_end = data_start.checked_add(len)?;
        let label = std::str::from_utf8(bytes.get(label_start..data_end)?).ok()?;
        (!label.trim().is_empty()).then_some(label)
    }

    /// The deterministic image vector: exactly [`Self::embed_sync`] of the
    /// image's label when it has one (the **same** vector a bare text query
    /// for that label gets, so "a text query recalls the image" is exact), and
    /// otherwise a hash-seeded unit vector derived from the image's SHA-256,
    /// far from every text.
    pub fn embed_image_sync(&self, image: ImageInput<'_>) -> Vec<f32> {
        match Self::image_label(image.bytes()) {
            Some(label) => self.embed_sync(label),
            None => {
                let hex: String = image.sha256().iter().map(|b| format!("{b:02x}")).collect();
                // A NUL-prefixed seed: no validated text content can be it.
                self.embed_sync(&format!("\u{0}fixture-image:{hex}"))
            }
        }
    }
}

/// A minimal PNG carrying `label` in a `tEXt` chunk keyed
/// [`IMAGE_LABEL_KEYWORD`], for tests (#22, design section 9).
///
/// Signature, a 1x1 greyscale `IHDR`, the `tEXt` chunk and `IEND`, each chunk
/// with a correct CRC. It has no `IDAT`, so it is a header-valid PNG, not a
/// decodable one, which is all `surface::image::validate` checks and all
/// [`FixtureEmbedder::embed_image`] reads. Built in code so no binary file is
/// committed.
pub fn png_with_label(label: &str) -> Vec<u8> {
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        let len = u32::try_from(data.len()).expect("a test label fits a PNG chunk");
        out.extend_from_slice(&len.to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let crc = crc32(&out[start..]);
        out.extend_from_slice(&crc.to_be_bytes());
    }
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    // width 1, height 1, bit depth 8, greyscale, deflate, adaptive, no interlace.
    chunk(&mut png, b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 0, 0, 0, 0]);
    let text = [IMAGE_LABEL_KEYWORD.as_bytes(), b"\0", label.as_bytes()].concat();
    chunk(&mut png, b"tEXt", &text);
    chunk(&mut png, b"IEND", &[]);
    png
}

/// CRC-32 (IEEE, reflected, as PNG uses), bitwise. Test-sized inputs only.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[async_trait]
impl Embedder for FixtureEmbedder {
    fn dimensions(&self) -> usize {
        self.dim
    }

    /// Text and images, into one space (#22): see [`Self::embed_image_sync`].
    fn modalities(&self) -> Modalities {
        Modalities::TEXT | Modalities::IMAGE
    }

    async fn embed_image(&self, image: ImageInput<'_>) -> Result<Vec<f32>, EmbedError> {
        Ok(self.embed_image_sync(image))
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        // CON-7: reject empty/whitespace input exactly like BGE — the trait
        // contract forbids embedding the empty string (see `embed/mod.rs`).
        if text.trim().is_empty() {
            return Err(EmbedError::Unavailable(
                "cannot embed empty/whitespace text".into(),
            ));
        }
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

    #[tokio::test]
    async fn rejects_empty_and_whitespace_input() {
        let e = FixtureEmbedder::new();
        for text in ["", "   ", "\t\n"] {
            let err = e.embed(text).await.unwrap_err();
            assert!(
                matches!(err, EmbedError::Unavailable(_)),
                "CON-7: {text:?} must be rejected, got {err:?}"
            );
        }
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
    fn png_with_label_is_a_valid_png_whose_label_the_fixture_reads() {
        let png = png_with_label("red silk saree");
        let input = crate::surface::image::validate(&png, "image/png").expect("header-valid PNG");
        assert_eq!(FixtureEmbedder::image_label(&png), Some("red silk saree"));
        let e = FixtureEmbedder::new();
        assert_eq!(
            e.embed_image_sync(input),
            e.embed_sync("red silk saree"),
            "a labelled image embeds exactly as a bare text query for its label"
        );
        // The IHDR CRC is the well-known one for a 1x1 8-bit greyscale header.
        assert_eq!(&png[29..33], &0x3a7e_9b55u32.to_be_bytes());
    }

    #[tokio::test]
    async fn the_fixture_embeds_images_and_advertises_it() {
        let e = FixtureEmbedder::new();
        assert_eq!(e.modalities(), Modalities::TEXT | Modalities::IMAGE);
        let png = png_with_label("render 17");
        let input = crate::surface::image::validate(&png, "image/png").unwrap();
        let v = e.embed_image(input).await.unwrap();
        assert_eq!(v.len(), DEFAULT_FIXTURE_DIM);
        let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((n - 1.0).abs() < 1e-5, "unit norm, got {n}");
    }

    #[test]
    fn an_unlabelled_image_gets_a_digest_seeded_vector_far_from_text() {
        let e = FixtureEmbedder::new();
        let a = png_with_label("x");
        // Same bytes with the keyword spelled differently: no label.
        let unlabelled: Vec<u8> = {
            let mut b = a.clone();
            let at = b.windows(5).position(|w| w == b"lambo").unwrap();
            b[at] = b'L';
            b
        };
        assert_eq!(FixtureEmbedder::image_label(&unlabelled), None);
        let input = crate::surface::image::validate(&unlabelled, "image/png").unwrap();
        let v = e.embed_image_sync(input);
        assert_eq!(v, e.embed_image_sync(input), "deterministic");
        assert!(cosine(&v, &e.embed_sync("x")) < 0.5);
        assert!(cosine(&v, &e.embed_sync(NEAR_A)) < 0.5);
    }

    #[test]
    fn a_lying_label_length_reads_nothing_and_never_panics() {
        let mut png = png_with_label("abc");
        let at = png.windows(4).position(|w| w == b"tEXt").unwrap();
        png[at - 4..at].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(FixtureEmbedder::image_label(&png), None);
        for cut in 0..png.len() {
            let _ = FixtureEmbedder::image_label(&png[..cut]);
        }
        assert_eq!(FixtureEmbedder::image_label(&png_with_label("   ")), None);
    }

    #[test]
    fn custom_dim() {
        let e = FixtureEmbedder::with_dimensions(64).unwrap();
        assert_eq!(e.dimensions(), 64);
        assert_eq!(e.embed_sync("x").len(), 64);
        assert!(FixtureEmbedder::with_dimensions(0).is_err());
    }
}
