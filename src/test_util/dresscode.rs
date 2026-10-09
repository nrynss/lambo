//! The Dresscode "close to the one you dismissed" path (#22 PR 6), shared by
//! the recall-by-image tests on every vector source.
//!
//! A wardrobe of three image concepts and some text: a look dismissed for
//! Onam (a PNG labelled [`DISMISSED_LABEL`]), a kept one and an unrelated
//! one. The fixture embeds a labelled PNG as exactly the vector its label's
//! text gets, and the label is case-folded, so [`similar_query_png`] (the
//! label in another case: different bytes, different digest) embeds to the
//! dismissed look's vector: the fixture's stand-in for "a photo of a similar
//! outfit". [`client_query_vector`] is that vector as a client would send
//! it, unnormalized, so the server's renormalization is exercised too.

use crate::embed::{png_with_label, FixtureEmbedder};
use crate::graph::image::{ImageDerive, ImagePayload};
use crate::memory::Memory;
use crate::recall::detail::DetailedRecall;
use crate::types::{AgentId, ConceptType, NodeId, RecallQuery};

/// The dismissed look's label: what the fixture embeds its PNG as.
pub const DISMISSED_LABEL: &str = "red silk saree";

/// The wardrobe's image concepts.
pub struct Wardrobe {
    /// "dismissed for Onam".
    pub dismissed: NodeId,
    /// "kept for Diwali".
    pub kept: NodeId,
    /// An unrelated look.
    pub other: NodeId,
}

async fn derive_look(mem: &Memory, caption: &str, label: &str, id: &str) -> NodeId {
    let png = png_with_label(label);
    let out = mem
        .derive_image_as(
            &AgentId::from("dresscode"),
            ImageDerive {
                caption,
                concept_type: ConceptType::Resource,
                image_id: Some(id),
                payload: ImagePayload::Bytes(
                    crate::surface::image::validate(&png, "image/png").expect("valid png"),
                ),
                parent_of: &[],
                event_time: None,
            },
        )
        .await
        .expect("image derive");
    assert_eq!(out.created.len(), 1, "one new image concept");
    out.created[0]
}

/// Derive the wardrobe (and some text noise) into `mem`, which must be a
/// `Hybrid` session over a store with vector search.
pub async fn derive_wardrobe(mem: &Memory) -> Wardrobe {
    for text in [
        "quantum chromodynamics lattice gauge",
        "billing retries change",
        "user schema",
    ] {
        mem.derive(
            &[(text, ConceptType::Entity)],
            &crate::graph::derive::ParentOf::none(),
        )
        .await
        .expect("text derive");
    }
    let dismissed = derive_look(mem, "look dismissed for Onam", DISMISSED_LABEL, "look2").await;
    let kept = derive_look(mem, "look kept for Diwali", "green linen kurta", "look1").await;
    let other = derive_look(mem, "weekend outfit", "blue denim jacket", "look3").await;
    Wardrobe {
        dismissed,
        kept,
        other,
    }
}

/// A PNG of "a similar outfit": other bytes, the dismissed look's vector.
pub fn similar_query_png() -> Vec<u8> {
    png_with_label("Red Silk Saree")
}

/// The dismissed look's vector as a client computes and sends it: scaled,
/// so not unit length.
pub fn client_query_vector() -> Vec<f32> {
    FixtureEmbedder::new()
        .embed_sync(DISMISSED_LABEL)
        .iter()
        .map(|x| x * 2.5)
        .collect()
}

/// A recall query with no text (`""` means none on the by-vector path).
pub fn imageless_text(top_k: usize) -> RecallQuery {
    RecallQuery {
        query: String::new(),
        top_k,
        max_tokens: 2_000,
        traversal_depth: 1,
    }
}

/// The dismissed look is the vector leg's best hit, at its own vector, and
/// the top hit overall; the kept and unrelated looks rank below it on the
/// vector leg, and no keyword leg fired (there is no text).
///
/// Call after `Memory::settle_daemon` (or on a reader whose daemon scores
/// the loaded graph) so a fresh concept's rank does not race the daemon's
/// structural score table, as `store::sqlite::tests::image_e2e` explains.
pub(crate) fn assert_dismissed_is_the_top_vector_hit(detailed: &DetailedRecall, w: &Wardrobe) {
    let vector = |id: &NodeId| detailed.legs.get(id).and_then(|l| l.vector);
    let best = detailed
        .legs
        .iter()
        .filter_map(|(id, l)| l.vector.map(|v| (*id, v)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .expect("the vector leg fired");
    assert_eq!(
        best.0, w.dismissed,
        "the dismissed look is the top vector-leg hit: {:?}",
        detailed.legs
    );
    assert!(best.1 > 0.99, "at its own vector, got {}", best.1);
    for other in [&w.kept, &w.other] {
        if let Some(v) = vector(other) {
            assert!(v < 0.5, "another look is far from the query: {v}");
        }
    }
    assert!(
        detailed.legs.values().all(|l| l.keyword.is_none()),
        "no text, so no keyword hit: {:?}",
        detailed.legs
    );
    assert_eq!(
        detailed.hits.first().map(|h| h.node_id),
        Some(w.dismissed),
        "the dismissed look is the top hit: {:?}",
        detailed.hits
    );
}
