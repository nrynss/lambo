//! Image concepts (#22): the request shape of an image derive and the pure
//! rules that turn it into one concept with a supplied vector.
//!
//! An image concept is an ordinary concept whose vector was **supplied**
//! rather than embedded from its text (design sections 4 and 5). Its content
//! is the caption plus a suffix Lambo builds, `"{caption} [image:{id}]"`, so
//! two images with one caption stay two concepts and the same image derived
//! again canonical-matches. The image bytes are never stored: the server
//! embeds them on the call path and keeps only the vector and an
//! [`EmbeddingSource`](crate::types::EmbeddingSource).
//!
//! `Memory::derive_image_as` and `Memory::derive_image_async_as` are the
//! entry points; this module holds what they share and nothing that does
//! I/O.
//!
//! # The image id (design R8, decided here)
//!
//! An id is 1 to 64 bytes of `[a-z0-9]`. The design proposed
//! `[a-z0-9_-]`, and named the risk that canonicalization might not keep the
//! suffix whole. It does not: [`crate::graph::canonical::normalize_tokens`]
//! splits on `-` and `_`, then drops stopwords and stems each piece, so
//! `r17-a_b` and `r17_a-b` would share a key, as would `red-shoes` and
//! `red-shoe`, and two different images would collapse into one concept.
//! Without `-` and `_`, `[image:<id>]` is one token that ends in `]`, which
//! no stemmer rule and no stopword touches, so the id survives into the key
//! exactly (pinned by `the_suffix_survives_canonicalization_whole`).
//! Lowercase only, because canonicalization folds case.

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use crate::embed::ImageInput;
use crate::types::{ConceptType, EmbeddingContract};

/// What an image concept's suffix starts with. A caption may not contain it,
/// so a content holds exactly one suffix, at its end.
pub const IMAGE_SUFFIX_OPEN: &str = "[image:";

/// The longest image id, in bytes.
pub const MAX_IMAGE_ID_BYTES: usize = 64;

/// How many hex characters of a digest a default image id takes.
pub const DEFAULT_IMAGE_ID_HEX: usize = 16;

/// Where an image concept's vector comes from.
pub enum ImagePayload<'a> {
    /// Validated image bytes (`crate::surface::image::validate`), which the
    /// server embeds on the call path with `Embedder::embed_image`. The bytes
    /// are dropped once the vector exists.
    Bytes(ImageInput<'a>),
    /// A vector a client computed, with the contract it declares. Accepted
    /// only when the declared contract is exactly the live one.
    Vector {
        /// The vector, at the contract's width. Renormalized on the call path.
        values: Vec<f32>,
        /// The embedding space the client says the vector is in.
        declared: EmbeddingContract,
    },
}

impl std::fmt::Debug for ImagePayload<'_> {
    /// Never prints the bytes or the vector: both are user data.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bytes(image) => f.debug_tuple("Bytes").field(image).finish(),
            Self::Vector { values, declared } => f
                .debug_struct("Vector")
                .field("len", &values.len())
                .field("declared", declared)
                .finish(),
        }
    }
}

/// One image derive: the request `Memory::derive_image_as` takes.
#[derive(Debug)]
pub struct ImageDerive<'a> {
    /// What the image is, in words. It is the concept's text: the keyword
    /// leg, canonicalization and recall's display read it; the vector does
    /// not (design section 2.3).
    pub caption: &'a str,
    /// The concept's type. `Observation` is refused (see
    /// [`check_image_concept_type`]).
    pub concept_type: ConceptType,
    /// The caller's id for the image, 1 to 64 bytes of `[a-z0-9]`. When
    /// `None`, the first 16 hex characters of the image's sha256 (bytes) or
    /// of the normalized vector's little-endian bytes (vector).
    pub image_id: Option<&'a str>,
    /// The image bytes or a client vector.
    pub payload: ImagePayload<'a>,
    /// `(parent, child)` hierarchy pairs, as `lambo_derive` takes them.
    pub parent_of: &'a [(&'a str, &'a str)],
    /// The fact's about-time, as `Memory::derive_for_ingest_as` takes it.
    pub event_time: Option<DateTime<Utc>>,
}

/// Refuse an image id outside 1 to [`MAX_IMAGE_ID_BYTES`] bytes of
/// `[a-z0-9]` (see the module docs for why not `-` or `_`). The message
/// never quotes the id.
pub fn validate_image_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > MAX_IMAGE_ID_BYTES {
        return Err(format!(
            "image_id must be 1 to {MAX_IMAGE_ID_BYTES} bytes ({} given)",
            id.len()
        ));
    }
    if !id.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9')) {
        return Err(
            "image_id may contain only lowercase ASCII letters and digits ([a-z0-9]); \
             canonicalization splits on '-' and '_' and folds case, so other characters \
             could make two images one concept"
                .into(),
        );
    }
    Ok(())
}

/// Refuse a caption that holds an image suffix of its own: Lambo builds the
/// suffix, so a content carries exactly one, at its end.
pub fn check_caption(caption: &str) -> Result<(), String> {
    if caption.contains(IMAGE_SUFFIX_OPEN) {
        return Err(format!(
            "caption may not contain {IMAGE_SUFFIX_OPEN:?}: Lambo appends the image suffix itself"
        ));
    }
    Ok(())
}

/// An image concept may not be an `Observation`: observations are demoted
/// context records that canonicalization never matches, so the same image
/// derived twice would become two concepts instead of one.
pub fn check_image_concept_type(concept_type: ConceptType) -> Result<(), String> {
    if concept_type == ConceptType::Observation {
        return Err(
            "an image concept cannot be an observation: observations never canonical-match, \
             so re-deriving the same image would duplicate it; use entity, logic, constraint \
             or resource"
                .into(),
        );
    }
    Ok(())
}

/// The stored content of an image concept: `"{caption} [image:{id}]"`.
/// `caption` is trimmed, so the suffix is always one space after the text.
pub fn image_content(caption: &str, image_id: &str) -> String {
    format!("{} {IMAGE_SUFFIX_OPEN}{image_id}]", caption.trim())
}

/// Lowercase hex.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// The default id of a server-embedded image: the first
/// [`DEFAULT_IMAGE_ID_HEX`] hex characters of its sha256.
pub(crate) fn digest_id(sha256: &[u8; 32]) -> String {
    hex(&sha256[..DEFAULT_IMAGE_ID_HEX / 2])
}

/// The default id of a submitted vector: the first [`DEFAULT_IMAGE_ID_HEX`]
/// hex characters of the sha256 of the normalized vector's little-endian
/// `f32` bytes, so the same vector maps to the same concept.
pub(crate) fn vector_id(normalized: &[f32]) -> String {
    let mut hasher = Sha256::new();
    for x in normalized {
        hasher.update(x.to_le_bytes());
    }
    digest_id(&hasher.finalize().into())
}

/// How far from 1 an `f64` norm may be for [`normalize`] to treat the vector
/// as already unit: about the rounding an `f32` unit vector carries.
const UNIT_NORM_TOLERANCE: f64 = 1e-6;

/// `values` scaled to unit L2 norm, computed in `f64`. Idempotent: a vector
/// already unit to within `f32` rounding is returned bit for bit, so a
/// correctly normalizing embedder or client is stored exactly as it answered.
/// The caller has already refused a non-finite or zero-norm vector
/// (`SuppliedVector::check`).
pub(crate) fn normalize(values: &[f32]) -> Vec<f32> {
    let norm = values
        .iter()
        .map(|x| f64::from(*x) * f64::from(*x))
        .sum::<f64>()
        .sqrt();
    if (norm - 1.0).abs() <= UNIT_NORM_TOLERANCE {
        return values.to_vec();
    }
    values
        .iter()
        .map(|x| (f64::from(*x) / norm) as f32)
        .collect()
}

#[cfg(test)]
mod tests;
