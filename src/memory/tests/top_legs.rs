//! #256: a recall's top per-leg scores and its provisional confidence,
//! through `Memory`: the maxima are over every phase-1 candidate (returned
//! or not), the combined value is absent rather than guessed, and a layered
//! recall reports the caller's own session's values, never a layer's.

use super::layered::{eg2_contract, eg2_seeded, seeded, seeded_vectors};
use super::*;
use crate::embed::calibration::CalibrationMode::{Builtin, Off};
use crate::memory::LayerRef;
use crate::recall::confidence::{noisy_or, BASIS};
use crate::recall::detail::DetailedRecall;
use crate::recall::layered::LayerTopLegs;
use crate::recall::query_vector::QueryBy;
use crate::types::TopLegs;

const QUERY: &str = "auth tokens";
/// The keyword leg's best match for [`QUERY`].
const KEYWORD_HIT: &str = "auth tokens rotate weekly";
/// Shares no word with [`QUERY`]; seeded at cosine 0.70 to it.
const VECTOR_HIT: &str = "zebra crossing paint";
/// EG2's null median (`evidence/issue-87-calibration/runs/`).
const EG2_NULL_MEDIAN: f64 = 0.632;

fn top_k(k: usize) -> RecallQuery {
    RecallQuery {
        top_k: k,
        ..query(QUERY)
    }
}

/// The best keyword and vector scores among the hits a recall returned.
fn best_returned(r: &DetailedRecall) -> TopLegs {
    let best = |pick: fn(&crate::recall::candidates::LegScores) -> Option<f64>| {
        r.hits
            .iter()
            .filter_map(|h| r.legs.get(&h.node_id).and_then(pick))
            .reduce(f64::max)
    };
    TopLegs {
        bm25: best(|l| l.keyword),
        vector_cosine: best(|l| l.vector),
    }
}

/// An EG2 session (a contract with a calibration row) whose store searches
/// vectors, holding the keyword leg's best match and, apart from it, the
/// vector leg's.
async fn two_leg_session(
    session: &str,
    mode: crate::embed::calibration::CalibrationMode,
) -> Memory {
    let mem = eg2_seeded(
        session,
        &[
            (KEYWORD_HIT, ConceptType::Logic),
            (VECTOR_HIT, ConceptType::Observation),
            ("auth middleware", ConceptType::Entity),
        ],
        true,
        mode,
        true,
    )
    .await;
    crate::test_util::seed_layer_match(&mem, VECTOR_HIT, QUERY).await;
    mem
}

/// The top legs are the maxima over every candidate a leg scored, not over
/// the returned hits: with `top_k = 1` one hit comes back, and the leg whose
/// best candidate was cut still reports that candidate's score.
#[tokio::test]
async fn top_legs_are_the_maxima_over_candidates_the_recall_did_not_return() {
    let mem = two_leg_session("top-legs-cut", Builtin).await;

    // Every candidate returned: the per-hit legs name each leg's best.
    let wide = mem.recall_detailed(top_k(10)).await.unwrap();
    let content = |r: &DetailedRecall, id: NodeId| {
        r.hits
            .iter()
            .find(|h| h.node_id == id)
            .map(|h| h.content.clone())
    };
    let by = |pick: fn(&crate::recall::candidates::LegScores) -> Option<f64>| {
        wide.legs
            .iter()
            .filter_map(|(id, l)| pick(l).map(|s| (*id, s)))
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .expect("the leg scored a candidate")
    };
    let (keyword_id, bm25) = by(|l| l.keyword);
    let (vector_id, cosine) = by(|l| l.vector);
    assert_eq!(content(&wide, keyword_id).as_deref(), Some(KEYWORD_HIT));
    assert_eq!(content(&wide, vector_id).as_deref(), Some(VECTOR_HIT));
    assert!((cosine - 0.70).abs() < 1e-4, "seeded at 0.70: {cosine}");
    let expected = TopLegs {
        bm25: Some(bm25),
        vector_cosine: Some(cosine),
    };
    assert_eq!(wide.top_legs, expected);
    assert_eq!(best_returned(&wide), expected, "premise: nothing was cut");

    // One hit returned: a leg's best candidate is not among the hits, and
    // the top legs are unchanged.
    let narrow = mem.recall_detailed(top_k(1)).await.unwrap();
    assert_eq!(narrow.hits.len(), 1, "{:?}", narrow.hits);
    let returned = best_returned(&narrow);
    assert!(
        returned.bm25 != expected.bm25 || returned.vector_cosine != expected.vector_cosine,
        "premise: the one hit cannot be both legs' best: {returned:?}"
    );
    assert_eq!(narrow.top_legs, expected);

    // The combined value is the noisy-OR of those two at EG2's null median,
    // and says it is provisional.
    let confidence = narrow
        .confidence
        .clone()
        .expect("both legs, a measured row");
    assert_eq!(
        Some(confidence.value),
        noisy_or(bm25, cosine, EG2_NULL_MEDIAN)
    );
    assert_eq!(confidence.basis, BASIS);
    assert!(confidence.provisional);
    assert_eq!(wide.confidence, narrow.confidence);

    // The public projection carries both.
    let public = mem.recall(top_k(1)).await.unwrap();
    assert_eq!(public.top_legs, expected);
    assert_eq!(public.confidence, narrow.confidence);
    mem.close().await.unwrap();
}

/// The recent leg is not a top leg: a query that matches nothing still
/// returns the session's recent concepts, and reports a keyword leg that ran
/// and matched nothing, no cosine and no confidence.
#[tokio::test]
async fn a_keyword_only_recall_reports_the_keyword_leg_and_no_confidence() {
    let mem = seeded(
        "top-legs-keyword",
        &[
            (KEYWORD_HIT, ConceptType::Logic),
            (VECTOR_HIT, ConceptType::Observation),
        ],
    )
    .await;
    let r = mem.recall_detailed(query(QUERY)).await.unwrap();
    assert!(r.top_legs.bm25.is_some_and(|s| s > 0.0), "{:?}", r.top_legs);
    assert_eq!(r.top_legs.bm25, best_returned(&r).bm25);
    assert_eq!(r.top_legs.vector_cosine, None, "no vector search");
    assert_eq!(r.confidence, None, "one leg is not the two-leg basis");

    let none = mem.recall_detailed(query("qqqq wwww")).await.unwrap();
    assert!(
        !none.hits.is_empty() && none.legs.values().all(|l| l.keyword.is_none()),
        "premise: recent-leg hits only: {:?}",
        none.legs
    );
    assert_eq!(
        none.top_legs,
        TopLegs {
            bm25: Some(0.0),
            vector_cosine: None,
        }
    );
    assert_eq!(none.confidence, None);
    mem.close().await.unwrap();
}

/// Both legs run, but no null median applies: the per-leg scores are
/// reported and the combined value is absent, never guessed.
#[tokio::test]
async fn the_confidence_is_absent_without_a_measured_null_median() {
    // A contract with no calibration row.
    let unmeasured = seeded_vectors(
        "top-legs-unmeasured",
        &[
            (KEYWORD_HIT, ConceptType::Logic),
            (VECTOR_HIT, ConceptType::Observation),
        ],
    )
    .await;
    crate::test_util::seed_layer_match(&unmeasured, VECTOR_HIT, QUERY).await;
    let r = unmeasured.recall_detailed(query(QUERY)).await.unwrap();
    assert!(r.top_legs.bm25.is_some_and(|s| s > 0.0));
    assert!(r.top_legs.vector_cosine.is_some());
    assert_eq!(r.confidence, None);
    unmeasured.close().await.unwrap();

    // A measured contract with the built-in rows switched off.
    let off = two_leg_session("top-legs-off", Off).await;
    let r = off.recall_detailed(query(QUERY)).await.unwrap();
    assert!(r.top_legs.bm25.is_some() && r.top_legs.vector_cosine.is_some());
    assert_eq!(r.confidence, None);
    off.close().await.unwrap();

    // A recall by a client vector: the medians were measured on text
    // queries, so the legs are reported alone.
    let on = two_leg_session("top-legs-by-vector", Builtin).await;
    let values = FixtureEmbedder::with_dimensions(eg2_contract().dim)
        .unwrap()
        .embed_sync(QUERY);
    let r = on
        .recall_by_detailed(
            query(QUERY),
            QueryBy::Vector {
                values,
                declared: eg2_contract(),
            },
        )
        .await
        .unwrap();
    assert!(r
        .top_legs
        .vector_cosine
        .is_some_and(|c| (c - 0.70).abs() < 1e-4));
    assert_eq!(r.confidence, None);
    on.close().await.unwrap();
}

/// A layered recall's primary values are the caller's own session's. The
/// layer's best scores sit under their own key and never move them.
#[tokio::test]
async fn a_layered_recall_reports_the_callers_own_top_legs() {
    const PROJECT_FACT: &str = "auth middleware checks tokens";
    let general = eg2_seeded(
        "top-legs-general",
        // Two concepts, so the layer's BM25 differs from the project's.
        &[
            (KEYWORD_HIT, ConceptType::Logic),
            (VECTOR_HIT, ConceptType::Observation),
        ],
        true,
        Builtin,
        true,
    )
    .await;
    // Cosine 0.70 to the query: the layer's best, above the project's.
    crate::test_util::seed_layer_match(&general, KEYWORD_HIT, QUERY).await;
    let project = eg2_seeded(
        "top-legs-project",
        &[(PROJECT_FACT, ConceptType::Entity)],
        true,
        Builtin,
        true,
    )
    .await;
    // Gives the project's concept its fixture vector (0.70 to another text).
    crate::test_util::seed_layer_match(&project, PROJECT_FACT, "release checklist").await;

    let own = project.recall_detailed(query(QUERY)).await.unwrap();
    let own_cosine = own.top_legs.vector_cosine.expect("the vector leg ran");
    assert!(own.top_legs.bm25.is_some_and(|s| s > 0.0));
    let layer_alone = general.recall_detailed(query(QUERY)).await.unwrap();
    let layer_bm25 = layer_alone.top_legs.bm25.expect("keyword leg");
    assert!(
        own_cosine < 0.69 && own.top_legs.bm25 != Some(layer_bm25),
        "premise: the layer's best scores differ from the project's: {:?} vs {:?}",
        own.top_legs,
        layer_alone.top_legs
    );

    let merged = project
        .recall_layered(
            &[LayerRef::new(&general).with_min_similarity(0.5)],
            query(QUERY),
        )
        .await
        .unwrap();
    assert!(
        merged.hits.iter().any(|h| h.session == *general.session()),
        "premise: the layer's hit merged: {:?}",
        merged.hits
    );
    // Primary: the caller's own, exactly as its plain recall reports them.
    assert_eq!(merged.top_legs, own.top_legs);
    assert_eq!(merged.confidence, own.confidence);
    assert!(merged.confidence.is_some());
    assert_eq!(merged.detail.top_legs, own.top_legs);
    assert_eq!(merged.detail.confidence, own.confidence);
    // The layer's, apart.
    assert_eq!(merged.layer_top_legs.len(), 1);
    let LayerTopLegs { session, top_legs } = &merged.layer_top_legs[0];
    assert_eq!(session, general.session());
    assert_eq!(top_legs.bm25, Some(layer_bm25));
    assert!(top_legs
        .vector_cosine
        .is_some_and(|c| (c - 0.70).abs() < 1e-4));

    // An unlayered call has no layer entry.
    let plain = project.recall_layered(&[], query(QUERY)).await.unwrap();
    assert_eq!(plain.top_legs, own.top_legs);
    assert!(plain.layer_top_legs.is_empty());

    project.close().await.unwrap();
    general.close().await.unwrap();
}
