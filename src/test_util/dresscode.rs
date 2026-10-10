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

// -- Graded similarity (review test gap, decides M2) ------------------------

/// The cosines the graded looks sit at to the query, best first.
pub const GRADED_COSINES: [f64; 3] = [0.8, 0.5, 0.3];

/// Looks at known cosines to one query vector, plus two unrelated looks
/// derived last so that, if the recent leg ran, they would fill it.
pub struct GradedLooks {
    /// The query as a client sends it: unit direction `d` scaled by 2.5.
    pub query: Vec<f32>,
    /// The looks at [`GRADED_COSINES`] to the query, in that order.
    pub graded: [NodeId; 3],
    /// Two looks orthogonal to the query, derived after everything else.
    pub unrelated: [NodeId; 2],
}

fn unit(v: Vec<f64>) -> Vec<f64> {
    let norm = v.iter().map(|x| x * x).sum::<f64>().sqrt();
    v.into_iter().map(|x| x / norm).collect()
}

fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// `count` unit vectors orthonormal to each other and to everything in
/// `basis`, by Gram-Schmidt over the fixture's vectors for `seed`-derived
/// labels (any independent directions would do).
fn orthonormal(basis: &mut Vec<Vec<f64>>, seed: &str, count: usize) -> Vec<Vec<f64>> {
    let fixture = FixtureEmbedder::new();
    let mut out = Vec::new();
    for i in 0..count {
        let mut v: Vec<f64> = fixture
            .embed_sync(&format!("{seed} {i}"))
            .iter()
            .map(|&x| f64::from(x))
            .collect();
        for b in basis.iter() {
            let p = dot(&v, b);
            v.iter_mut().zip(b).for_each(|(x, y)| *x -= p * y);
        }
        let v = unit(v);
        basis.push(v.clone());
        out.push(v);
    }
    out
}

async fn derive_look_vector(mem: &Memory, caption: &str, id: &str, values: Vec<f32>) -> NodeId {
    let out = mem
        .derive_image_as(
            &AgentId::from("dresscode"),
            ImageDerive {
                caption,
                concept_type: ConceptType::Resource,
                image_id: Some(id),
                payload: ImagePayload::Vector {
                    values,
                    declared: mem.embedding_contract().clone(),
                },
                parent_of: &[],
                event_time: None,
            },
        )
        .await
        .expect("image derive by vector");
    assert_eq!(out.created.len(), 1, "one new image concept");
    out.created[0]
}

/// Derive [`GradedLooks`] into `mem` (a `Hybrid` session over a store with
/// vector search, whose contract's width is the fixture's): with `d` the
/// query's unit direction and `n_i` unit vectors orthonormal to `d` and to
/// each other, the look at cosine `c` stores `c*d + sqrt(1-c^2)*n_i`, which
/// has cosine exactly `c` with the query. The graded looks come first, then
/// one text concept, then the two unrelated looks (stored as `n_4`, `n_5`:
/// cosine 0), so the three most recent interactions hold no graded look.
pub async fn derive_graded_looks(mem: &Memory) -> GradedLooks {
    let mut basis = Vec::new();
    let d = orthonormal(&mut basis, "graded query", 1).remove(0);
    let noise = orthonormal(&mut basis, "graded noise", 5);
    let to_f32 = |v: Vec<f64>| v.into_iter().map(|x| x as f32).collect::<Vec<f32>>();
    let mut graded = Vec::new();
    for (i, c) in GRADED_COSINES.into_iter().enumerate() {
        let s = (1.0 - c * c).sqrt();
        let v: Vec<f64> = d
            .iter()
            .zip(&noise[i])
            .map(|(x, n)| c * x + s * n)
            .collect();
        let id = format!("graded{i}");
        graded.push(derive_look_vector(mem, &format!("graded look {c}"), &id, to_f32(v)).await);
    }
    mem.derive(
        &[("billing retries change", ConceptType::Entity)],
        &crate::graph::derive::ParentOf::none(),
    )
    .await
    .expect("text derive");
    let a = derive_look_vector(
        mem,
        "unrelated look a",
        "unrelated1",
        to_f32(noise[3].clone()),
    )
    .await;
    let b = derive_look_vector(
        mem,
        "unrelated look b",
        "unrelated2",
        to_f32(noise[4].clone()),
    )
    .await;
    GradedLooks {
        query: to_f32(d.iter().map(|x| x * 2.5).collect()),
        graded: [graded[0], graded[1], graded[2]],
        unrelated: [a, b],
    }
}

/// The vector leg scores each graded look at its cosine (within 1e-3, so
/// the query and the stored vectors were normalized), the recent leg did not
/// run (no text), and the final order is the graded looks best first, ahead
/// of both unrelated looks however recently they were derived.
///
/// Call after `Memory::settle_daemon`, as for
/// [`assert_dismissed_is_the_top_vector_hit`].
pub(crate) fn assert_graded_order(detailed: &DetailedRecall, looks: &GradedLooks) {
    for (id, c) in looks.graded.iter().zip(GRADED_COSINES) {
        let v = detailed
            .legs
            .get(id)
            .and_then(|l| l.vector)
            .unwrap_or_else(|| panic!("graded look {c} on the vector leg: {:?}", detailed.legs));
        assert!((v - c).abs() < 1e-3, "graded look {c}: vector leg {v}");
    }
    assert!(
        detailed.legs.values().all(|l| l.recent.is_none()),
        "no text, so no recent leg: {:?}",
        detailed.legs
    );
    let order: Vec<NodeId> = detailed.hits.iter().map(|h| h.node_id).collect();
    assert_eq!(
        order.get(..3),
        Some(&looks.graded[..]),
        "graded looks first, best first: {:?}",
        detailed.hits
    );
    for id in &looks.unrelated {
        if let Some(pos) = order.iter().position(|h| h == id) {
            assert!(pos >= 3, "an unrelated look ranks below every graded one");
        }
    }
}
