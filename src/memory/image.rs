//! Image derives (#22, design section 5): one concept whose vector is
//! supplied, synchronously or acknowledged before the apply.
//!
//! Both entry points do the same call-path work first ([`Memory::prepare_image`]):
//! the preconditions (the `Hybrid` strategy and a store with vector search),
//! the caption, id and type rules, and the vector itself. A server-side image
//! is embedded **here, before the ack**, so the write queue, the durable
//! intent and replay carry only the vector and its provenance, never the
//! bytes (design section 5.1). A client vector is checked against the live
//! contract here (design section 3.3, point 1). What reaches the write path is
//! a [`SuppliedVector`], which `hybrid::derive_with` takes as it is.

use crate::embed::{EmbedError, Modalities};
use crate::graph::derive::{DeriveOutcome, ParentOf};
use crate::graph::hybrid::{self, HYBRID_IO_TIMEOUT};
use crate::graph::image::{self, ImageDerive, ImagePayload};
use crate::surface::validate::{check_size, require_nonempty};
use crate::types::{
    check_supplied_values, AgentId, ConceptType, EmbeddingSource, LamboError, MatchStrategy,
    SourceModality, SuppliedVector, VectorOrigin,
};
use crate::writeq::Submitted;

use super::Memory;

/// An image derive after the call-path work: the concept, its pairs, and the
/// vector it carries.
struct PreparedImage {
    concept_type: ConceptType,
    pairs: Vec<(String, String)>,
    supplied: SuppliedVector,
}

impl Memory {
    /// Derive one image concept (#22), synchronously: on return the concept
    /// is in the graph, as with [`Memory::derive_as`].
    ///
    /// The concept's content is `"{caption} [image:{id}]"` and its vector is
    /// the image's, not the caption's: the server embeds
    /// [`ImagePayload::Bytes`] with `Embedder::embed_image`, and an
    /// [`ImagePayload::Vector`] is taken as submitted once its declared
    /// contract equals the live one. The concept never semantic-merges, and no
    /// text concept merges into it; the same caption and id derived again
    /// matches it (design section 4.4). If a text concept already holds the
    /// same caption and id (its canonical key), the derive is refused with
    /// [`LamboError::ImageIdTaken`]: derive the image under another id.
    ///
    /// Refused with [`LamboError::Config`] before anything is written: a
    /// strategy other than `Hybrid`, a store without vector search, an
    /// embedder that does not embed images (bytes only), a blank or oversized
    /// caption, an invalid image id, an `Observation` type, and a submitted
    /// vector whose contract, width, finiteness or norm is wrong. An embedder
    /// failure is [`LamboError::EmbedUnavailable`] or [`LamboError::Embed`],
    /// as for a text derive.
    pub async fn derive_image_as(
        &self,
        agent: &AgentId,
        image: ImageDerive<'_>,
    ) -> Result<DeriveOutcome, LamboError> {
        // Held across the image embed and the derive, as `derive_as` holds it.
        let _writing = self.begin_write().await?;
        let event_time = image.event_time;
        let prepared = self.prepare_image(image).await?;
        let content = prepared.supplied.content.clone();
        let concepts = [(content.as_str(), prepared.concept_type)];
        let pairs: Vec<(&str, &str)> = prepared
            .pairs
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let parent_of = if pairs.is_empty() {
            ParentOf::none()
        } else {
            ParentOf::from_pairs(&pairs)
        };
        let prompt = hybrid::derive_prompt([content.as_str()]);
        let interaction = self.begin_interaction_full(agent, Some(prompt), event_time)?;
        let outcome = hybrid::derive_with(
            self.graph.clone(),
            self.derive_vector_candidates(),
            self.embedder.as_ref(),
            &self.embedding,
            interaction,
            agent,
            &concepts,
            &parent_of,
            self.config.max_cooccurrence_per_derive,
            self.config.semantic_match_threshold,
            Some(&prepared.supplied),
            None,
        )
        .await?;
        let mut touched = outcome.created.clone();
        touched.extend(outcome.matched.iter().copied());
        self.mirror_concepts(&touched);
        self.daemon.wake();
        Ok(outcome)
    }

    /// [`Memory::derive_image_as`] acknowledged before the apply (J3), as
    /// [`Memory::derive_async_as`] is for text.
    ///
    /// Everything [`Memory::derive_image_as`] refuses is refused here, on the
    /// call path, including the image embed itself: the ack waits on it (one
    /// image per call bounds that, design R7). What is queued, and recorded as
    /// the durable intent, is the vector, never the bytes. What moves to the
    /// receipt is what needs the graph at apply: the session stamp check under
    /// the commit lock, and the live-contract check a replay runs.
    pub async fn derive_image_async_as(
        &self,
        agent: &AgentId,
        image: ImageDerive<'_>,
    ) -> Result<Submitted, LamboError> {
        let _writing = self.begin_write().await?;
        let event_time = image.event_time;
        let prepared = self.prepare_image(image).await?;
        let content = prepared.supplied.content.clone();
        {
            // The pre-pass `derive_async_as` runs under `Hybrid`, so the
            // errors a caller can fix arrive now, not on a receipt.
            let concepts = [(content.as_str(), prepared.concept_type)];
            let pairs: Vec<(&str, &str)> = prepared
                .pairs
                .iter()
                .map(|(a, b)| (a.as_str(), b.as_str()))
                .collect();
            let parent_of = if pairs.is_empty() {
                ParentOf::none()
            } else {
                ParentOf::from_pairs(&pairs)
            };
            hybrid::validate_limits(&concepts, &parent_of, self.config.semantic_match_threshold)?;
            let g = self.graph.read();
            hybrid::validate_graph_inputs(&g, &parent_of)?;
            hybrid::validate_embed_budget(&g, &concepts, &parent_of, true)?;
        }
        let prompt = hybrid::derive_prompt([content.as_str()]);
        let interaction = self.begin_interaction_full(agent, Some(prompt), event_time)?;
        Ok(self
            .pipeline
            .submit_derive_image(
                agent.clone(),
                interaction,
                vec![(content, prepared.concept_type)],
                prepared.pairs,
                prepared.supplied,
            )
            .await)
    }

    /// The call-path work both image entry points share. See
    /// [`Memory::derive_image_as`] for what it refuses.
    async fn prepare_image(&self, image: ImageDerive<'_>) -> Result<PreparedImage, LamboError> {
        let config = |e: String| LamboError::Config(e);
        // Design Q16: an image concept is found only through its vector.
        if self.config.match_strategy != MatchStrategy::Hybrid {
            return Err(LamboError::Config(
                "an image derive needs match_strategy = \"hybrid\": an image concept is found \
                 only through its vector, and a caption-only concept is what lambo_derive is for"
                    .into(),
            ));
        }
        if !self.derive_vector_candidates().available() {
            return Err(LamboError::Config(
                "an image derive needs a store with vector search (VECTOR_SEARCH): an image \
                 concept is found only through its vector"
                    .into(),
            ));
        }
        image::check_image_concept_type(image.concept_type).map_err(config)?;
        require_nonempty("caption", image.caption).map_err(config)?;
        check_size("caption", image.caption).map_err(config)?;
        image::check_caption(image.caption).map_err(config)?;
        if let Some(id) = image.image_id {
            image::validate_image_id(id).map_err(config)?;
        }
        for &(parent, child) in image.parent_of {
            for (field, value) in [("parent_of parent", parent), ("parent_of child", child)] {
                require_nonempty(field, value).map_err(config)?;
                check_size(field, value).map_err(config)?;
            }
        }

        // #74: a `parent_of` end that would overflow the embedding context
        // at apply is refused here, before the image embed and the ack.
        image::check_embed_context(
            image.caption,
            image.image_id,
            image.concept_type,
            image.parent_of,
        )
        .map_err(config)?;

        let live = &self.embedding;
        let (vector, default_id, source) = match image.payload {
            ImagePayload::Bytes(input) => {
                if !self.embedder.modalities().contains(Modalities::IMAGE) {
                    return Err(LamboError::Config(
                        "the configured embedder does not embed images; an image derive needs \
                         one that does, or a client-computed vector"
                            .into(),
                    ));
                }
                let raw = self.embed_image_or_refuse(input).await?;
                // The adapter's own output, held to the trait's contract: a
                // wrong width, a NaN or a zero vector is the embedder's
                // failure on this input, not the caller's.
                check_supplied_values(&raw, live, live).map_err(|e| {
                    LamboError::Embed(format!(
                        "the embedder returned an unusable image vector ({e}); nothing was \
                         written"
                    ))
                })?;
                let sha = input.sha256();
                tracing::debug!(
                    target: "lambo::image",
                    session = %self.session,
                    mime = %input.mime(),
                    bytes = input.bytes().len(),
                    sha256_prefix = %image::hex(&sha[..4]),
                    "image derive: embedded on the call path"
                );
                (
                    image::normalize(&raw),
                    image::digest_id(&sha),
                    EmbeddingSource {
                        modality: SourceModality::Image,
                        origin: VectorOrigin::Server,
                        // PR 2 decision: built only from the validated
                        // input's digest, lowercase hex.
                        sha256: Some(image::hex(&sha)),
                        mime: Some(input.mime().into()),
                    },
                )
            }
            ImagePayload::Vector { values, declared } => {
                check_supplied_values(&values, &declared, live).map_err(config)?;
                let vector = image::normalize(&values);
                let id = image::vector_id(&vector);
                (
                    vector,
                    id,
                    EmbeddingSource {
                        modality: SourceModality::Image,
                        origin: VectorOrigin::Client,
                        sha256: None,
                        mime: None,
                    },
                )
            }
        };
        let id = image.image_id.map_or(default_id, str::to_owned);
        let content = image::image_content(image.caption, &id);
        check_size("image concept content (caption and suffix)", &content).map_err(config)?;
        tracing::debug!(
            target: "lambo::image",
            session = %self.session,
            image_id = %id,
            origin = ?source.origin,
            "image derive: vector ready"
        );
        Ok(PreparedImage {
            concept_type: image.concept_type,
            pairs: image
                .parent_of
                .iter()
                .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
                .collect(),
            supplied: SuppliedVector {
                content,
                vector,
                contract: live.clone(),
                source,
            },
        })
    }

    /// Embed an image under [`HYBRID_IO_TIMEOUT`], with the text derive's
    /// error classes: a timeout or an unreachable embedder is
    /// [`LamboError::EmbedUnavailable`], a refusal of this input
    /// [`LamboError::Embed`], and an embedder that does not embed images
    /// [`LamboError::Config`]. Messages never quote the bytes.
    async fn embed_image_or_refuse(
        &self,
        input: crate::embed::ImageInput<'_>,
    ) -> Result<Vec<f32>, LamboError> {
        match tokio::time::timeout(HYBRID_IO_TIMEOUT, self.embedder.embed_image(input)).await {
            Err(_) => Err(LamboError::EmbedUnavailable(format!(
                "the image embed timed out after {HYBRID_IO_TIMEOUT:?}; nothing was written"
            ))),
            Ok(Err(EmbedError::Unsupported(e))) => Err(LamboError::Config(format!(
                "the configured embedder does not embed images ({e}); nothing was written"
            ))),
            Ok(Err(EmbedError::Unreadable(e))) => Err(LamboError::Embed(format!(
                "Lambo could not read the image ({e}); nothing was written"
            ))),
            Ok(Err(e)) if e.is_transient() => Err(LamboError::EmbedUnavailable(format!(
                "the embedder could not be reached for the image ({e}); nothing was written"
            ))),
            Ok(Err(e)) => Err(LamboError::Embed(format!(
                "the embedder refused the image ({e}); nothing was written"
            ))),
            Ok(Ok(vector)) => Ok(vector),
        }
    }
}
