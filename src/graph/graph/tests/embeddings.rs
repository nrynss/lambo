//! Embedding contracts and vectors: overrides, re-embed and fill.

use super::*;

#[test]
fn set_embedding_is_ordered_durable_metadata() {
    let mut g = Graph::new(sid());
    let embedding = crate::types::EmbeddingContract {
        kind: "fixture".into(),
        model: Some("v1".into()),
        dim: 1024,
    };
    let epoch = g.epoch();
    g.stamp_embedding(embedding.clone()).unwrap();
    assert!(g.epoch() > epoch);
    let batch = g.drain_log();
    assert_eq!(
        batch.mutations,
        vec![Mutation::SetEmbedding {
            session_id: sid(),
            embedding: Some(embedding.clone()),
        }]
    );

    let epoch = g.epoch();
    g.stamp_embedding(embedding).unwrap();
    assert_eq!(g.epoch(), epoch, "an identical contract is a no-op");
    assert!(g.drain_log().is_empty());
}

#[test]
fn embedding_contract_cannot_change_while_vectors_remain() {
    let (mut g, interaction, _) = small_graph();
    let fixture = crate::types::EmbeddingContract {
        kind: "fixture".into(),
        model: Some("v1".into()),
        dim: 1024,
    };
    g.stamp_embedding(fixture.clone()).unwrap();
    let mut vector_concept = concept(99, interaction, "vector-bearing");
    vector_concept.embedding = Some(vec![0.0; 1024]);
    g.insert_concept(vector_concept, interaction).unwrap();
    g.drain_log();
    let before = g.snapshot();
    let mut corrupt_reload = before.clone();
    corrupt_reload.embedding = None;
    assert!(
        Graph::from_snapshot(corrupt_reload).is_err(),
        "load rejects vectors whose contract was lost"
    );

    let other = crate::types::EmbeddingContract {
        kind: "bedrock".into(),
        model: Some("titan-v2".into()),
        dim: 1024,
    };
    assert!(g.stamp_embedding(other.clone()).is_err());
    assert!(g.replace_embedding_without_vectors(Some(other)).is_err());
    assert!(g.replace_embedding_without_vectors(None).is_err());
    assert_eq!(g.snapshot(), before);
    assert!(g.drain_log().is_empty());
}

#[test]
fn h1_operator_override_is_limited_to_same_width_same_kind_vector_aliases() {
    let (mut g, interaction, _) = small_graph();
    let old = crate::types::EmbeddingContract {
        kind: "bge_m3".into(),
        model: Some("old-alias.gguf".into()),
        dim: 1024,
    };
    g.stamp_embedding(old).unwrap();
    let mut vector_concept = concept(100, interaction, "vector-bearing override");
    vector_concept.embedding = Some(vec![0.0; 1024]);
    g.insert_concept(vector_concept, interaction).unwrap();
    g.drain_log();

    let cross_kind = crate::types::EmbeddingContract {
        kind: "bedrock".into(),
        model: Some("titan-v2".into()),
        dim: 1024,
    };
    let before = g.snapshot();
    let err = g
        .replace_embedding_with_operator_override(cross_kind)
        .unwrap_err();
    assert!(
        err.to_string().contains("stored concept vectors remain"),
        "{err}"
    );
    assert_eq!(g.snapshot(), before, "a refused override must be atomic");

    let wrong_width = crate::types::EmbeddingContract {
        kind: "bge_m3".into(),
        model: Some("new-alias.gguf".into()),
        dim: 512,
    };
    assert!(g
        .replace_embedding_with_operator_override(wrong_width)
        .is_err());
    assert_eq!(
        g.snapshot(),
        before,
        "a refused width change must be atomic"
    );

    let alias = crate::types::EmbeddingContract {
        kind: "bge_m3".into(),
        model: Some("new-alias.gguf".into()),
        dim: 1024,
    };
    g.replace_embedding_with_operator_override(alias.clone())
        .unwrap();
    assert_eq!(g.embedding(), Some(&alias));
    assert!(matches!(
        g.drain_log().mutations.as_slice(),
        [Mutation::SetEmbedding {
            embedding: Some(contract),
            ..
        }] if contract == &alias
    ));
}

// -----------------------------------------------------------------------
// K2: Graph::reembed_all
// -----------------------------------------------------------------------

fn embed_contract(kind: &str, model: &str, dim: usize) -> crate::types::EmbeddingContract {
    crate::types::EmbeddingContract {
        kind: kind.into(),
        model: Some(model.into()),
        dim,
    }
}

/// Two vector-bearing concepts under a stamped bge_m3 contract, log drained,
/// ready for a candle-space migration.
fn two_concept_graph() -> (Graph, NodeId, NodeId) {
    let mut g = Graph::new(sid());
    let i = interaction(1, None, 0);
    let iid = i.id;
    g.insert_interaction(i).unwrap();
    let c1 = concept(1, iid, "user schema");
    let c2 = concept(2, iid, "auth middleware");
    let (id1, id2) = (c1.id, c2.id);
    g.insert_concept(c1, iid).unwrap();
    g.insert_concept(c2, iid).unwrap();
    g.stamp_embedding(embed_contract("bge_m3", "old.gguf", 1024))
        .unwrap();
    g.drain_log();
    (g, id1, id2)
}

#[test]
fn k2_reembed_all_rewrites_vectors_then_swaps_contract_in_order() {
    let (mut g, c1, c2) = two_concept_graph();
    let target = embed_contract("candle", "BAAI/bge-m3@main sha256:abcd1234", 1024);
    g.reembed_all(
        // Deliberately not sorted by id: input order must not matter for
        // correctness (the CLI sorts anyway), only the batch ORDER does.
        vec![(c2, vec![0.5_f32; 1024]), (c1, vec![0.25_f32; 1024])],
        target.clone(),
    )
    .unwrap();

    // The vectors live in the node map immediately (RAM-ahead-of-disk is
    // the documented contract until the caller flushes).
    let embedded = |g: &Graph, id: NodeId| match g.node(id) {
        Some(crate::types::Node::Concept(c)) => c.embedding.clone().unwrap(),
        other => panic!("expected concept {id}, got {other:?}"),
    };
    assert_eq!(embedded(&g, c1), vec![0.25_f32; 1024]);
    assert_eq!(embedded(&g, c2), vec![0.5_f32; 1024]);
    assert_eq!(g.embedding(), Some(&target));

    // The drained batch carries every UpsertNode BEFORE the trailing
    // SetEmbedding — that order is what makes one flushed batch take the
    // durable session old-consistent -> new-consistent atomically.
    assert!(matches!(
        g.drain_log().mutations.as_slice(),
        [
            Mutation::UpsertNode { .. },
            Mutation::UpsertNode { .. },
            Mutation::SetEmbedding {
                embedding: Some(c),
                ..
            }
        ] if c == &target
    ));
}

/// The backfill twin. `re-embed` refuses when the live contract is already
/// the stored one — correct for a migration, and the reason the 2026-09-01
/// dogfood session had no way to repair 555 concepts that were missing
/// vectors inside their own current space.
#[test]
fn embed_missing_fills_null_vectors_without_touching_the_contract() {
    let (mut g, c1, c2) = two_concept_graph();
    let live = embed_contract("bge_m3", "old.gguf", 1024);

    // Both concepts start with no vector; back-fill only c2, so the
    // assertion below distinguishes "filled" from "filled everything".
    let filled = g
        .embed_missing(vec![(c2, vec![0.75_f32; 1024])], &live)
        .unwrap();
    assert_eq!(filled, 1);

    let embedded = |g: &Graph, id: NodeId| match g.node(id) {
        Some(crate::types::Node::Concept(c)) => c.embedding.clone(),
        other => panic!("expected concept {id}, got {other:?}"),
    };
    assert_eq!(embedded(&g, c2), Some(vec![0.75_f32; 1024]));
    assert_eq!(embedded(&g, c1), None, "untargeted concepts are untouched");
    assert_eq!(g.embedding(), Some(&live), "the contract did not move");

    // No trailing SetEmbedding: the contract is unchanged, so emitting one
    // would make a repair look like a migration to anything reading the log.
    assert!(matches!(
        g.drain_log().mutations.as_slice(),
        [Mutation::UpsertNode { .. }]
    ));
}

#[test]
fn embed_missing_refuses_to_overwrite_or_cross_spaces() {
    let (mut g, c1, c2) = two_concept_graph();
    let live = embed_contract("bge_m3", "old.gguf", 1024);
    g.embed_missing(vec![(c1, vec![0.25_f32; 1024])], &live)
        .unwrap();
    let before = g.snapshot();

    // Overwriting an existing vector is a migration, not a backfill.
    let err = g
        .embed_missing(vec![(c1, vec![0.9_f32; 1024])], &live)
        .unwrap_err();
    assert!(
        err.to_string().contains("already carries a vector"),
        "{err}"
    );

    // A different space is a migration too.
    let other = embed_contract("candle", "BAAI/bge-m3@main sha256:abcd1234", 1024);
    let err = g
        .embed_missing(vec![(c2, vec![0.9_f32; 1024])], &other)
        .unwrap_err();
    assert!(err.to_string().contains("that is a migration"), "{err}");

    // Wrong width is refused like everywhere else.
    let err = g
        .embed_missing(vec![(c2, vec![0.9_f32; 8])], &live)
        .unwrap_err();
    assert!(err.to_string().contains("non-finite or has width"), "{err}");

    assert_eq!(
        g.snapshot(),
        before,
        "every refusal left the graph untouched"
    );
}

#[test]
fn k2_reembed_all_refusals_are_atomic_and_named() {
    let (mut g, c1, c2) = two_concept_graph();
    let target = embed_contract("candle", "BAAI/bge-m3@main sha256:abcd1234", 1024);
    let before = g.snapshot();

    // Missing concept: covering 1 of 2 would leave stale-space vectors.
    let err = g
        .reembed_all(vec![(c1, vec![0.5_f32; 1024])], target.clone())
        .unwrap_err();
    assert!(err.to_string().contains("cover 1 of 2"), "{err}");

    // Duplicate id: right list length, but one concept stays uncovered.
    let err = g
        .reembed_all(
            vec![
                (c1, vec![0.5_f32; 1024]),
                (c1, vec![0.5_f32; 1024]),
                (c2, vec![0.5_f32; 1024]),
            ],
            target.clone(),
        )
        .unwrap_err();
    assert!(err.to_string().contains("exactly once"), "{err}");

    // An update that is not a concept in this session.
    let err = g
        .reembed_all(
            vec![(c1, vec![0.5_f32; 1024]), (uid(999), vec![0.5_f32; 1024])],
            target.clone(),
        )
        .unwrap_err();
    assert!(err.to_string().contains("not a concept"), "{err}");

    // Wrong width against the TARGET contract.
    let err = g
        .reembed_all(
            vec![(c1, vec![0.5_f32; 512]), (c2, vec![0.5_f32; 512])],
            target.clone(),
        )
        .unwrap_err();
    assert!(err.to_string().contains("non-finite or has width"), "{err}");

    // Non-finite values would poison hybrid ranking silently.
    let err = g
        .reembed_all(
            vec![(c1, vec![f32::NAN; 1024]), (c2, vec![0.5_f32; 1024])],
            target,
        )
        .unwrap_err();
    assert!(err.to_string().contains("non-finite"), "{err}");

    assert_eq!(g.snapshot(), before, "every refusal must be atomic");
    assert!(
        g.drain_log().is_empty(),
        "no mutation may survive a refusal"
    );
}

#[test]
fn k2_reembed_all_refuses_width_change_and_identical_contract() {
    let (mut g, c1, _) = two_concept_graph();
    let before = g.snapshot();

    // A different width is a fresh session, not a migration.
    let err = g
        .reembed_all(
            vec![(c1, vec![0.5_f32; 512])],
            embed_contract("candle", "m", 512),
        )
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("cannot re-embed session from width"),
        "{err}"
    );

    // The exact current contract is a no-op, and no-ops are refused like
    // everywhere else in the RAM invariant layer.
    let err = g
        .reembed_all(
            vec![(c1, vec![0.5_f32; 1024])],
            embed_contract("bge_m3", "old.gguf", 1024),
        )
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("already carries exactly this contract"),
        "{err}"
    );

    assert_eq!(g.snapshot(), before);
    assert!(g.drain_log().is_empty());
}

#[test]
fn k2_reembed_all_on_an_empty_graph_stamps_the_contract_alone() {
    // Zero concepts trivially satisfy "every concept covered": a fresh
    // (or emptied) session adopting the live space emits exactly the
    // SetEmbedding and nothing else.
    let mut g = Graph::new(sid());
    let target = embed_contract("candle", "BAAI/bge-m3@main sha256:abcd1234", 1024);
    g.reembed_all(Vec::new(), target.clone()).unwrap();
    assert_eq!(g.embedding(), Some(&target));
    assert!(matches!(
        g.drain_log().mutations.as_slice(),
        [Mutation::SetEmbedding {
            embedding: Some(c),
            ..
        }] if c == &target
    ));
}

/// #22 design Q19 at the graph: an image concept's vector is never replaced
/// by a text vector. `reembed_all` refuses while one remains, the dropping
/// variant nulls it (keeping the source) in the same ordered batch, and a
/// backfill refuses to give a vectorless image concept a caption vector.
#[test]
fn re_embed_and_backfill_never_give_an_image_concept_a_text_vector() {
    let (mut g, interaction, _) = small_graph();
    let old = crate::types::EmbeddingContract {
        kind: "bge_m3".into(),
        model: Some("v1".into()),
        dim: 4,
    };
    let new = crate::types::EmbeddingContract {
        kind: "candle".into(),
        ..old.clone()
    };
    g.stamp_embedding(old.clone()).unwrap();
    let source = crate::types::EmbeddingSource {
        modality: crate::types::SourceModality::Image,
        origin: crate::types::VectorOrigin::Server,
        sha256: None,
        mime: None,
    };
    let mut image = concept(200, interaction, "render 17 [image:r17]");
    image.embedding = Some(vec![1.0, 0.0, 0.0, 0.0]);
    image.embedding_source = Some(source.clone());
    let image_id = image.id;
    g.insert_concept(image, interaction).unwrap();
    let texts: Vec<NodeId> = g
        .concepts()
        .filter(|c| c.embedding_source.is_none())
        .map(|c| c.id)
        .collect();
    g.drain_log();
    let text_updates = || -> Vec<(NodeId, Vec<f32>)> {
        texts
            .iter()
            .map(|id| (*id, vec![0.0, 1.0, 0.0, 0.0]))
            .collect()
    };

    let before = g.snapshot();
    let err = g.reembed_all(text_updates(), new.clone()).unwrap_err();
    assert!(err.to_string().contains("1 image vector(s)"), "{err}");
    let mut with_image = text_updates();
    with_image.push((image_id, vec![0.0, 0.0, 1.0, 0.0]));
    assert!(
        g.reembed_all_dropping_image_vectors(with_image, new.clone())
            .is_err(),
        "an update may never target an image concept"
    );
    assert_eq!(g.snapshot(), before, "refusals leave the graph untouched");
    assert!(g.drain_log().is_empty());

    assert_eq!(
        g.reembed_all_dropping_image_vectors(text_updates(), new.clone())
            .unwrap(),
        1
    );
    let Some(Node::Concept(image)) = g.node(image_id) else {
        panic!("still a concept");
    };
    assert_eq!(image.embedding, None);
    assert_eq!(image.embedding_source, Some(source));
    let batch = g.drain_log();
    assert!(
        matches!(batch.mutations.last(), Some(Mutation::SetEmbedding { .. })),
        "the contract swap is last"
    );
    assert_eq!(
        batch
            .mutations
            .iter()
            .filter(|m| matches!(m, Mutation::UpsertNode { .. }))
            .count(),
        texts.len() + 1
    );

    // A second migration: the image has no vector, so nothing blocks it.
    let newer = crate::types::EmbeddingContract {
        kind: "gemini".into(),
        ..new.clone()
    };
    g.reembed_all(text_updates(), newer.clone()).unwrap();
    g.drain_log();

    // The backfill refuses the vectorless image concept.
    let err = g
        .embed_missing(vec![(image_id, vec![0.0, 0.0, 1.0, 0.0])], &newer)
        .unwrap_err();
    assert!(err.to_string().contains("image concept"), "{err}");
    assert!(g.drain_log().is_empty());
}
