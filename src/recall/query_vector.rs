//! Recall by image or by a client-computed query vector (#22 PR 6).
//!
//! A text recall embeds its query with [`Embedder::embed_query`] (through
//! the session's query-embedding cache, #14). A recall *by* something else
//! skips that embed: the vector leg searches with the vector this module
//! produces from a [`QueryBy`], in the same space and under the same checks
//! an image derive uses (design section 3.3):
//!
//! - [`QueryBy::Image`]: validated bytes (`crate::surface::image::validate`)
//!   embedded with [`Embedder::embed_image`]. Images take no prompt prefix
//!   (design 3.2's `lambo-eg2-v1` profile), so the query role and the
//!   document role are the same call for an image; that is why an image
//!   query vector is directly comparable with a stored image concept's.
//! - [`QueryBy::Vector`]: a vector the client computed, with the contract it
//!   declares. Accepted only when that contract equals the live one exactly,
//!   and when the vector has the contract's width, finite components and a
//!   non-zero norm.
//!
//! Either way the result is L2-normalized (idempotent on a unit vector), so
//! a client that truncated without renormalizing still ranks correctly.
//!
//! **Never cached, never logged.** Neither cache sees these vectors: the #14
//! query-embedding cache is keyed by query text and holds only query-role
//! text vectors (design 7.2: an image key would be a digest of a user's
//! image held across requests), and the pipeline recall cache never serves
//! or stores a vector-dependent result (P1-2). No message here quotes the
//! bytes, a component or the declared contract's strings beyond what
//! [`check_supplied_values`] shows to a library caller; the surfaces check
//! with the model-safe `surface::image::check_submitted_vector` first.

use crate::embed::{EmbedError, Embedder, ImageInput, Modalities};
use crate::graph::hybrid::HYBRID_IO_TIMEOUT;
use crate::graph::image::normalize;
use crate::types::{check_supplied_values, EmbeddingContract, LamboError};

/// What a recall's vector leg searches by, instead of its text's embedding.
pub enum QueryBy<'a> {
    /// A validated image, for the configured embedder to embed.
    Image(ImageInput<'a>),
    /// A vector the client computed, with the contract it declares.
    Vector {
        /// The components, at the contract's width.
        values: Vec<f32>,
        /// The embedding space the client says the vector is in.
        declared: EmbeddingContract,
    },
}

impl QueryBy<'_> {
    /// `"image"` or `"vector"`: the payload kind, and the only thing about
    /// it a log or ledger line may carry.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Image(_) => "image",
            Self::Vector { .. } => "vector",
        }
    }
}

impl std::fmt::Debug for QueryBy<'_> {
    /// Never prints the bytes or the vector: both are user data.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Image(image) => f.debug_tuple("Image").field(image).finish(),
            Self::Vector { values, declared } => f
                .debug_struct("Vector")
                .field("len", &values.len())
                .field("declared", declared)
                .finish(),
        }
    }
}

/// The unit query vector `by` names, in `live`'s space.
///
/// Refused with [`LamboError::Config`]: an image when the embedder does not
/// embed images, and a client vector whose declared contract is not `live`,
/// whose width is not `live.dim`, or which has a non-finite component or a
/// zero norm. An embedder failure is [`LamboError::EmbedUnavailable`] (a
/// timeout or an unreachable backend) or [`LamboError::Embed`] (a refusal
/// of this image, or an unusable answer). Unlike a text recall, a failed
/// image embed does not degrade to the keyword and recent legs: the caller
/// asked for what is near this image, and an answer without the vector leg
/// would answer a different question.
pub(crate) async fn resolve(
    by: QueryBy<'_>,
    embedder: &dyn Embedder,
    live: &EmbeddingContract,
) -> Result<Vec<f32>, LamboError> {
    match by {
        QueryBy::Image(input) => {
            if !embedder.modalities().contains(Modalities::IMAGE) {
                return Err(LamboError::Config(
                    "the configured embedder does not embed images; a recall by image needs one \
                     that does, or a client-computed query vector"
                        .into(),
                ));
            }
            let raw =
                match tokio::time::timeout(HYBRID_IO_TIMEOUT, embedder.embed_image(input)).await {
                    Err(_) => {
                        return Err(LamboError::EmbedUnavailable(format!(
                            "the query image embed timed out after {HYBRID_IO_TIMEOUT:?}"
                        )));
                    }
                    Ok(Err(EmbedError::Unsupported(e))) => {
                        return Err(LamboError::Config(format!(
                            "the configured embedder does not embed images ({e})"
                        )));
                    }
                    Ok(Err(e)) if e.is_transient() => {
                        return Err(LamboError::EmbedUnavailable(format!(
                            "the embedder could not be reached for the query image ({e})"
                        )));
                    }
                    Ok(Err(e)) => {
                        return Err(LamboError::Embed(format!(
                            "the embedder refused the query image ({e})"
                        )));
                    }
                    Ok(Ok(vector)) => vector,
                };
            // The adapter's own output, held to the trait's contract.
            check_supplied_values(&raw, live, live).map_err(|e| {
                LamboError::Embed(format!(
                    "the embedder returned an unusable query image vector ({e})"
                ))
            })?;
            tracing::debug!(
                target: "lambo::image",
                mime = %input.mime(),
                bytes = input.bytes().len(),
                "recall by image: embedded the query image"
            );
            Ok(normalize(&raw))
        }
        QueryBy::Vector { values, declared } => {
            check_supplied_values(&values, &declared, live).map_err(LamboError::Config)?;
            Ok(normalize(&values))
        }
    }
}

#[cfg(all(test, feature = "embed-fixture"))]
mod tests {
    use super::*;
    use crate::embed::{png_with_label, FixtureEmbedder};

    fn live() -> EmbeddingContract {
        EmbeddingContract {
            kind: "fixture".into(),
            model: None,
            dim: FixtureEmbedder::new().dimensions(),
        }
    }

    #[tokio::test]
    async fn an_image_resolves_to_its_unit_vector() {
        let png = png_with_label("red silk saree");
        let input = crate::surface::image::validate(&png, "image/png").unwrap();
        let e = FixtureEmbedder::new();
        let v = resolve(QueryBy::Image(input), &e, &live()).await.unwrap();
        assert_eq!(v, e.embed_sync("red silk saree"));
    }

    #[tokio::test]
    async fn a_client_vector_is_checked_and_renormalized() {
        let e = FixtureEmbedder::new();
        let unit = e.embed_sync("red silk saree");
        let scaled: Vec<f32> = unit.iter().map(|x| x * 4.0).collect();
        let v = resolve(
            QueryBy::Vector {
                values: scaled,
                declared: live(),
            },
            &e,
            &live(),
        )
        .await
        .unwrap();
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "renormalized: {norm}");

        let mut other = live();
        other.model = Some("another-model".into());
        let err = resolve(
            QueryBy::Vector {
                values: unit.clone(),
                declared: other,
            },
            &e,
            &live(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, LamboError::Config(_)), "{err:?}");
        for bad in [
            vec![0.0; unit.len()],
            vec![f32::NAN; unit.len()],
            vec![1.0; 3],
        ] {
            let err = resolve(
                QueryBy::Vector {
                    values: bad,
                    declared: live(),
                },
                &e,
                &live(),
            )
            .await
            .unwrap_err();
            assert!(matches!(err, LamboError::Config(_)), "{err:?}");
        }
    }

    /// The fixture's text, with the trait's default (text-only) modalities.
    struct TextOnly(FixtureEmbedder);

    #[async_trait::async_trait]
    impl Embedder for TextOnly {
        fn dimensions(&self) -> usize {
            self.0.dimensions()
        }
        async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
            self.0.embed(text).await
        }
    }

    #[tokio::test]
    async fn an_image_needs_an_image_embedder() {
        let png = png_with_label("x");
        let input = crate::surface::image::validate(&png, "image/png").unwrap();
        let err = resolve(
            QueryBy::Image(input),
            &TextOnly(FixtureEmbedder::new()),
            &live(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, LamboError::Config(_)), "{err:?}");
    }

    #[test]
    fn debug_never_prints_the_payload() {
        let by = QueryBy::Vector {
            values: vec![0.123_456_7; 4],
            declared: live(),
        };
        let shown = format!("{by:?}");
        assert!(shown.contains("len: 4"), "{shown}");
        assert!(!shown.contains("0.123"), "{shown}");
        assert_eq!(by.kind(), "vector");
    }
}
