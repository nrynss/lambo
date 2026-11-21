//! #256: a recall's top per-leg scores and its provisional confidence,
//! through `Memory`: the maxima are over every phase-1 candidate (returned
//! or not), the combined value is absent rather than guessed, and a layered
//! recall reports the caller's own session's values, never a layer's.

use super::layered::{eg2_contract, eg2_seeded, seeded, seeded_vectors, slow_session};
use super::*;
use crate::embed::calibration::CalibrationMode::{Builtin, Off};
use crate::memory::LayerRef;
use crate::recall::confidence::{noisy_or, BASIS, KEYWORD_ONLY_BASIS};
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
        exact_name_match: false,
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
        exact_name_match: false,
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
/// and matched nothing and no cosine. With no vector leg the confidence is
/// the keyword-only basis (#256's acceptance), `S / (S + 5)`.
#[tokio::test]
async fn a_keyword_only_recall_reports_the_keyword_leg_and_a_keyword_only_confidence() {
    let mem = seeded(
        "top-legs-keyword",
        &[
            (KEYWORD_HIT, ConceptType::Logic),
            (VECTOR_HIT, ConceptType::Observation),
        ],
    )
    .await;
    let r = mem.recall_detailed(query(QUERY)).await.unwrap();
    let bm25 = r.top_legs.bm25.expect("the keyword leg ran");
    assert!(bm25 > 0.0, "{:?}", r.top_legs);
    assert_eq!(r.top_legs.bm25, best_returned(&r).bm25);
    assert_eq!(r.top_legs.vector_cosine, None, "no vector search");
    assert!(!r.top_legs.exact_name_match);
    assert_keyword_only(&r, bm25);

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
            exact_name_match: false,
        }
    );
    assert_keyword_only(&none, 0.0);
    mem.close().await.unwrap();
}

/// The keyword leg's best is read before phase 1 cuts the index search to
/// its over-fetch cap (`top_k x 4`), so a library caller's `top_k` of 0, a
/// cap of 0, reports the same `bm25` and keyword-only confidence as
/// `top_k = 1`, not a leg that matched nothing.
#[tokio::test]
async fn a_zero_top_k_reports_the_same_bm25_as_top_k_one() {
    let mem = seeded(
        "top-legs-zero-k",
        &[
            (KEYWORD_HIT, ConceptType::Logic),
            ("auth middleware", ConceptType::Entity),
            (VECTOR_HIT, ConceptType::Observation),
        ],
    )
    .await;
    let one = mem.recall_detailed(top_k(1)).await.unwrap();
    let zero = mem.recall_detailed(top_k(0)).await.unwrap();
    assert!(
        zero.hits.is_empty() && zero.legs.values().all(|l| l.keyword.is_none()),
        "premise: a cap of 0 keeps no keyword candidate: {:?}",
        zero.legs
    );
    let bm25 = one.top_legs.bm25.expect("the keyword leg ran");
    assert!(bm25 > 0.0, "{:?}", one.top_legs);
    assert_eq!(zero.top_legs.bm25, one.top_legs.bm25);
    assert_eq!(zero.top_legs, one.top_legs);
    assert_keyword_only(&zero, bm25);
    assert_eq!(zero.confidence, one.confidence);
    mem.close().await.unwrap();

    // With both legs, a `top_k` of 0 asks the vector leg for no neighbours:
    // `bm25` holds, `vector_cosine` is null, and the basis is keyword-only.
    let mem = two_leg_session("top-legs-zero-k-two", Builtin).await;
    let one = mem.recall_detailed(top_k(1)).await.unwrap();
    let zero = mem.recall_detailed(top_k(0)).await.unwrap();
    assert!(one.top_legs.vector_cosine.is_some(), "{:?}", one.top_legs);
    assert_eq!(
        one.confidence.as_ref().map(|c| c.basis.as_str()),
        Some(BASIS)
    );
    assert_eq!(zero.top_legs.bm25, one.top_legs.bm25);
    assert_eq!(zero.top_legs.vector_cosine, None, "{:?}", zero.top_legs);
    assert_keyword_only(&zero, one.top_legs.bm25.expect("keyword leg"));
    mem.close().await.unwrap();
}

/// `r`'s confidence is the keyword-only basis at keyword score `bm25`.
fn assert_keyword_only(r: &DetailedRecall, bm25: f64) {
    let c = r.confidence.clone().expect("the keyword leg ran");
    assert_eq!(c.basis, KEYWORD_ONLY_BASIS);
    assert_eq!(c.basis, "keyword-only-v0");
    assert!(c.provisional);
    assert_eq!(c.value, bm25 / (bm25 + 5.0), "{c:?}");
}

/// Both legs run, but no null median applies: the per-leg scores are
/// reported and the two-leg value is absent, never guessed (nor replaced by
/// the keyword-only one, which is for a recall with no vector leg).
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

    // A measured (non-reference) contract with the built-in rows off.
    let off = two_leg_session("top-legs-off", Off).await;
    let r = off.recall_detailed(query(QUERY)).await.unwrap();
    assert!(r.top_legs.bm25.is_some() && r.top_legs.vector_cosine.is_some());
    assert_eq!(r.confidence, None);
    off.close().await.unwrap();

    // A recall by a client vector: the medians were measured on text
    // queries, so the legs are reported alone.
    let on = two_leg_session("top-legs-by-vector", Builtin).await;
    let by_vector = |text: &str| {
        let values = FixtureEmbedder::with_dimensions(eg2_contract().dim)
            .unwrap()
            .embed_sync(QUERY);
        (
            query(text),
            QueryBy::Vector {
                values,
                declared: eg2_contract(),
            },
        )
    };
    let (q, by) = by_vector(QUERY);
    let r = on.recall_by_detailed(q, by).await.unwrap();
    assert!(r
        .top_legs
        .vector_cosine
        .is_some_and(|c| (c - 0.70).abs() < 1e-4));
    assert!(r.top_legs.bm25.is_some_and(|s| s > 0.0));
    assert_eq!(r.confidence, None);
    // With no text, the keyword leg does not run: `bm25` is null.
    let (q, by) = by_vector("");
    let r = on.recall_by_detailed(q, by).await.unwrap();
    assert_eq!(r.top_legs.bm25, None, "{:?}", r.top_legs);
    assert!(!r.top_legs.exact_name_match);
    assert!(r
        .top_legs
        .vector_cosine
        .is_some_and(|c| (c - 0.70).abs() < 1e-4));
    assert_eq!(r.confidence, None);
    on.close().await.unwrap();
}

/// A session under the BGE-M3 reference contract (`bge_m3`, server default
/// model, 1024 dims) whose store searches vectors, under `calibration`.
async fn bge_session(
    session: &str,
    calibration: crate::embed::calibration::CalibrationMode,
) -> Memory {
    let contract = EmbeddingContract {
        kind: "bge_m3".into(),
        model: None,
        dim: 1024,
    };
    assert!(crate::embed::calibration::is_reference(&contract));
    let mem = Memory::builder()
        .session(session)
        .agent("agent-a")
        .config(crate::config::Config {
            daemon_tick_interval: Duration::from_secs(3_600),
            calibration,
            ..crate::config::Config::default()
        })
        .flush_interval(Duration::from_secs(3_600))
        .store(std::sync::Arc::new(crate::test_util::GraphRanked(
            crate::test_util::VectorSearchable(std::sync::Arc::new(MemoryStore::new())),
        )) as std::sync::Arc<dyn GraphStore>)
        .embedder(
            std::sync::Arc::new(FixtureEmbedder::with_dimensions(contract.dim).unwrap())
                as std::sync::Arc<dyn crate::embed::Embedder>,
        )
        .embedding_contract(contract)
        .build()
        .await
        .expect("build");
    mem.derive(
        &[
            (KEYWORD_HIT, ConceptType::Logic),
            (VECTOR_HIT, ConceptType::Observation),
        ],
        &ParentOf::none(),
    )
    .await
    .unwrap();
    mem.settle_daemon().await;
    crate::test_util::seed_layer_match(&mem, VECTOR_HIT, QUERY).await;
    mem
}

/// The BGE-M3 reference contract has a measured null median, end to end,
/// and keeps it under `calibration = "off"`, which means "the BGE-M3
/// reference values": for the reference contract, its own.
#[tokio::test]
async fn the_reference_contract_reports_the_two_leg_value_in_either_mode() {
    for (session, mode) in [("top-legs-bge", Builtin), ("top-legs-bge-off", Off)] {
        let mem = bge_session(session, mode).await;
        let r = mem.recall_detailed(query(QUERY)).await.unwrap();
        let bm25 = r.top_legs.bm25.expect("keyword leg");
        let cosine = r.top_legs.vector_cosine.expect("vector leg");
        assert!((cosine - 0.70).abs() < 1e-4, "{cosine}");
        let c = r.confidence.clone().expect("a measured median");
        assert_eq!(c.basis, BASIS, "{mode:?}");
        assert_eq!(
            Some(c.value),
            noisy_or(
                bm25,
                cosine,
                crate::embed::calibration::REFERENCE_NULL_MEDIAN_COSINE
            ),
            "{mode:?}"
        );
        mem.close().await.unwrap();
    }
}

/// A query that is exactly a concept's durable name raises that concept's
/// per-hit keyword score to at least 1.0: the higher of its BM25 and 1.0
/// (#288). `top_legs.bm25` is the index's own best BM25, never the 1.0
/// floor, whether the real score is above every other match or (in a store
/// where the word is everywhere) below 1.0; `exact_name_match` reports the
/// name.
#[tokio::test]
async fn an_exact_name_query_reports_the_raw_bm25_and_the_match() {
    let raw_best = |mem: &Memory, q: &str| {
        let cap = query(q).top_k * crate::recall::candidates::KEYWORD_OVERFETCH;
        mem.index
            .read()
            .search(q, cap)
            .into_iter()
            .map(|s| s.score)
            .reduce(f64::max)
            .expect("the query matches")
    };

    // The named concept's real BM25 is the best match: the old fold took
    // the runner-up, since the named one's slot held 1.0 before #288.
    let mem = seeded(
        "top-legs-name",
        &[
            (KEYWORD_HIT, ConceptType::Logic),
            ("auth middleware", ConceptType::Entity),
            ("tokens expire", ConceptType::Entity),
        ],
    )
    .await;
    let target = mem
        .recall_detailed(query(KEYWORD_HIT))
        .await
        .unwrap()
        .hits
        .iter()
        .find(|h| h.content == KEYWORD_HIT)
        .expect("seeded")
        .node_id;
    mem.declare_name("auth tokens rotate", target).unwrap();
    mem.settle_daemon().await;
    let named = mem
        .recall_detailed(query("auth tokens rotate"))
        .await
        .unwrap();
    let best = raw_best(&mem, "auth tokens rotate");
    assert_eq!(
        named.legs.get(&target).and_then(|l| l.keyword),
        Some(best.max(1.0)),
        "the per-hit keyword leg is raised to at least 1.0, never below its BM25 (#288)"
    );
    let runner_up = named
        .legs
        .iter()
        .filter(|(id, _)| **id != target)
        .filter_map(|(_, l)| l.keyword)
        .reduce(f64::max)
        .expect("another concept matched");
    assert!(best > runner_up, "premise: {best} > {runner_up}");
    assert_eq!(named.top_legs.bm25, Some(best));
    assert!(named.top_legs.exact_name_match);
    // A word no concept holds adds nothing to BM25, and names nothing.
    let reordered = mem
        .recall_detailed(query("auth tokens rotate qqqq"))
        .await
        .unwrap();
    assert_eq!(reordered.top_legs.bm25, Some(best));
    assert!(!reordered.top_legs.exact_name_match);
    // The combined value takes the raw score as well.
    assert_keyword_only(&named, best);
    mem.close().await.unwrap();

    // Every concept holds the word, so its BM25 is small: the 1.0 floor
    // would have been reported as a keyword score no concept had.
    let mem = seeded(
        "top-legs-name-small",
        &[
            ("alpha one", ConceptType::Entity),
            ("alpha two", ConceptType::Entity),
            ("alpha three", ConceptType::Entity),
            ("alpha four", ConceptType::Entity),
        ],
    )
    .await;
    let target = mem
        .recall_detailed(query("alpha one"))
        .await
        .unwrap()
        .hits
        .iter()
        .find(|h| h.content == "alpha one")
        .expect("seeded")
        .node_id;
    mem.declare_name("alpha", target).unwrap();
    mem.settle_daemon().await;
    let named = mem.recall_detailed(query("alpha")).await.unwrap();
    let best = raw_best(&mem, "alpha");
    assert!(
        best < 1.0,
        "premise: every BM25 is under the 1.0 floor: {best}"
    );
    let own = mem
        .index
        .read()
        .search_ranked("alpha")
        .into_iter()
        .find(|s| s.item == target)
        .map(|s| s.score);
    assert!(
        own.is_some_and(|s| s < 1.0),
        "premise: the named concept matched, under the floor: {own:?}"
    );
    // Per hit, the named concept takes the floor: max(BM25, 1.0) is 1.0
    // here: below 1.0, like exactly 1.0 or no hit, the raise and the old
    // overwrite agree.
    assert_eq!(
        named.legs.get(&target).and_then(|l| l.keyword),
        Some(1.0),
        "a BM25 under 1.0 is raised to the 1.0 floor (#288)"
    );
    assert_eq!(named.top_legs.bm25, Some(best));
    assert!(named.top_legs.exact_name_match);
    assert_keyword_only(&named, best);
    mem.close().await.unwrap();
}

/// A query that is a concept's durable name but shares no word with any
/// concept's content: the named concept is absent from the BM25 results, so
/// `bm25` is 0 beside `exact_name_match: true`, and the keyword-only value,
/// which does not take the flag into account, is 0. The name is compared
/// normalized, so a case and spacing variant matches too.
#[tokio::test]
async fn an_exact_name_with_no_word_overlap_reports_zero_bm25_beside_the_match() {
    let mem = seeded(
        "top-legs-name-zero",
        &[
            (KEYWORD_HIT, ConceptType::Logic),
            ("auth middleware", ConceptType::Entity),
        ],
    )
    .await;
    let target = mem
        .recall_detailed(query(KEYWORD_HIT))
        .await
        .unwrap()
        .hits
        .iter()
        .find(|h| h.content == KEYWORD_HIT)
        .expect("seeded")
        .node_id;
    mem.declare_name("quokka", target).unwrap();
    mem.settle_daemon().await;
    for q in ["quokka", "  Quokka "] {
        let cap = query(q).top_k * crate::recall::candidates::KEYWORD_OVERFETCH;
        assert!(
            mem.index.read().search(q, cap).is_empty(),
            "premise: no concept's content holds the name"
        );
        let r = mem.recall_detailed(query(q)).await.unwrap();
        assert_eq!(
            r.legs.get(&target).and_then(|l| l.keyword),
            Some(1.0),
            "premise: ranked as an exact-name match ({q:?})"
        );
        assert_eq!(
            r.top_legs,
            TopLegs {
                bm25: Some(0.0),
                vector_cosine: None,
                exact_name_match: true,
            },
            "{q:?}"
        );
        assert_keyword_only(&r, 0.0);
    }
    mem.close().await.unwrap();
}

/// A layer's own exact-name match is reported in its `layer_top_legs`
/// entry, and does not set the caller's own flag.
#[tokio::test]
async fn a_layers_exact_name_match_is_reported_under_layer_top_legs() {
    let project = seeded(
        "top-legs-name-lp",
        &[("auth middleware checks tokens", ConceptType::Entity)],
    )
    .await;
    let general = seeded("top-legs-name-lg", &[(KEYWORD_HIT, ConceptType::Logic)]).await;
    let target = general
        .recall_detailed(query(KEYWORD_HIT))
        .await
        .unwrap()
        .hits
        .iter()
        .find(|h| h.content == KEYWORD_HIT)
        .expect("seeded")
        .node_id;
    general.declare_name("rotation policy", target).unwrap();
    general.settle_daemon().await;

    let q = query("rotation policy");
    let own = project.recall_detailed(q.clone()).await.unwrap();
    assert!(!own.top_legs.exact_name_match, "premise: no project name");
    let r = project
        .recall_layered(&[LayerRef::new(&general)], q)
        .await
        .unwrap();
    assert_eq!(r.layer_top_legs.len(), 1, "{:?}", r.layer_top_legs);
    let LayerTopLegs { session, top_legs } = &r.layer_top_legs[0];
    assert_eq!(session, general.session());
    assert!(top_legs.exact_name_match, "{top_legs:?}");
    assert!(top_legs.bm25.is_some(), "the layer's keyword leg ran");
    assert_eq!(r.top_legs, own.top_legs);
    assert_eq!(r.confidence, own.confidence);
    project.close().await.unwrap();
    general.close().await.unwrap();
}

/// A recall refused with a warning (here a `top_k` over the vector
/// candidate limit, on a session whose two legs would otherwise both run)
/// runs no leg: through `Memory`, both legs are null, no name matched, and
/// there is no confidence, in the detailed answer, the public one and its
/// serialized form.
#[tokio::test]
async fn a_refused_recall_reports_no_leg_and_no_confidence() {
    let mem = two_leg_session("top-legs-refused", Builtin).await;
    let q = top_k(crate::store::MAX_VECTOR_CANDIDATE_LIMIT + 1);
    let r = mem.recall_detailed(q.clone()).await.unwrap();
    assert!(
        r.hits.is_empty() && r.warnings.iter().any(|w| w.contains("exceeds maximum")),
        "premise: refused with a warning: {:?}",
        r.warnings
    );
    assert_eq!(r.top_legs, TopLegs::default());
    assert_eq!(r.confidence, None);
    let public = mem.recall(q).await.unwrap();
    assert_eq!(public.top_legs, TopLegs::default());
    assert_eq!(public.confidence, None);
    let wire = serde_json::to_value(&public).unwrap();
    assert_eq!(
        wire["top_legs"],
        serde_json::json!({"bm25": null, "exact_name_match": false, "vector_cosine": null})
    );
    assert!(wire.get("confidence").is_none(), "{wire}");
    mem.close().await.unwrap();
}

/// A dependency question answered by graph traversal runs no leg: both
/// legs are null, no name matched, and there is no confidence.
#[tokio::test]
async fn a_dispatched_dependency_question_reports_no_leg() {
    let mem = slow_session(
        std::sync::Arc::new(MemoryStore::new()),
        "top-legs-dispatch",
        "fixture",
    )
    .await;
    mem.derive(
        &[
            ("weekly token rotation ruling", ConceptType::Constraint),
            ("rotated the prod tokens", ConceptType::Resource),
        ],
        &ParentOf::from_pairs(&[("weekly token rotation ruling", "rotated the prod tokens")]),
    )
    .await
    .unwrap();
    mem.settle_daemon().await;
    let r = mem
        .recall_detailed(RecallQuery {
            traversal_depth: 1,
            ..query("what depends on weekly token rotation ruling")
        })
        .await
        .unwrap();
    assert!(
        r.response_annotations
            .iter()
            .any(|a| a.kind == crate::recall::detail::AnnotationKind::Traversal),
        "premise: dispatched: {:?}",
        r.response_annotations
    );
    assert_eq!(r.top_legs, TopLegs::default());
    assert_eq!(r.confidence, None);
    mem.close().await.unwrap();
}

/// Embeds documents, and refuses every query embed.
#[derive(Debug)]
struct QueryEmbedFails(FixtureEmbedder);

#[async_trait]
impl crate::embed::Embedder for QueryEmbedFails {
    fn dimensions(&self) -> usize {
        self.0.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.0.embed(text).await
    }
    async fn embed_query(&self, _text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        Err(crate::embed::EmbedError::Unavailable(
            "simulated embedder outage".into(),
        ))
    }
}

/// A degraded recall (the query embed failed, or the store's vector read
/// failed) has a null cosine, says why in a warning, and reports the
/// keyword-only confidence.
#[tokio::test]
async fn a_degraded_vector_leg_reports_a_null_cosine_and_a_keyword_only_confidence() {
    // The query embed fails.
    let mem = Memory::builder()
        .session("top-legs-embed-fails")
        .agent("agent-a")
        .store(std::sync::Arc::new(crate::test_util::GraphRanked(
            crate::test_util::VectorSearchable(std::sync::Arc::new(MemoryStore::new())),
        )) as std::sync::Arc<dyn GraphStore>)
        .embedder(std::sync::Arc::new(QueryEmbedFails(FixtureEmbedder::new()))
            as std::sync::Arc<dyn crate::embed::Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .build()
        .await
        .expect("build");
    mem.derive(&[(KEYWORD_HIT, ConceptType::Logic)], &ParentOf::none())
        .await
        .unwrap();
    mem.settle_daemon().await;
    let r = mem.recall_detailed(query(QUERY)).await.unwrap();
    assert!(
        r.warnings
            .iter()
            .any(|w| w.contains("query embedding failed")),
        "{:?}",
        r.warnings
    );
    assert_eq!(r.top_legs.vector_cosine, None);
    let bm25 = r.top_legs.bm25.expect("the keyword leg ran");
    assert!(bm25 > 0.0);
    assert_keyword_only(&r, bm25);
    mem.close().await.unwrap();

    // The store's vector read fails.
    let store = std::sync::Arc::new(super::writes::VectorSearchStore::new(std::sync::Arc::new(
        MemoryStore::new(),
    )));
    let mem = Memory::builder()
        .session("top-legs-read-fails")
        .agent("agent-a")
        .store(store.clone() as std::sync::Arc<dyn GraphStore>)
        .embedder(std::sync::Arc::new(FixtureEmbedder::new())
            as std::sync::Arc<dyn crate::embed::Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .build()
        .await
        .expect("build");
    mem.derive(&[(KEYWORD_HIT, ConceptType::Logic)], &ParentOf::none())
        .await
        .unwrap();
    mem.settle_daemon().await;
    store.fail_vector_reads();
    let r = mem.recall_detailed(query(QUERY)).await.unwrap();
    assert!(
        r.warnings
            .iter()
            .any(|w| w.contains("vector read failed (detail logged)")),
        "{:?}",
        r.warnings
    );
    assert!(r
        .response_annotations
        .iter()
        .any(|a| a.kind == crate::recall::detail::AnnotationKind::VectorDegraded));
    assert_eq!(r.top_legs.vector_cosine, None);
    let bm25 = r.top_legs.bm25.expect("the keyword leg ran");
    assert!(bm25 > 0.0);
    assert_keyword_only(&r, bm25);
    mem.close().await.unwrap();
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

/// A layer that is unavailable has no `layer_top_legs` entry; one that
/// answered a dependency question by traversal has one, with both legs
/// null (it ran none). The primary values stay the caller's own.
#[tokio::test]
async fn layer_top_legs_skip_an_unavailable_layer_and_report_a_traversal() {
    let project = seeded(
        "top-legs-lp",
        &[("auth middleware checks tokens", ConceptType::Entity)],
    )
    .await;

    let closed = seeded("top-legs-closed", &[("auth closed", ConceptType::Entity)]).await;
    closed.close().await.unwrap();
    let own = project.recall_detailed(query(QUERY)).await.unwrap();
    let r = project
        .recall_layered(&[LayerRef::new(&closed)], query(QUERY))
        .await
        .unwrap();
    assert_eq!(r.unavailable_layers, [closed.session().clone()]);
    assert!(r.layer_top_legs.is_empty(), "{:?}", r.layer_top_legs);
    assert_eq!(r.top_legs, own.top_legs);
    assert_eq!(r.confidence, own.confidence);

    let general = slow_session(
        std::sync::Arc::new(MemoryStore::new()),
        "top-legs-lg",
        "fixture",
    )
    .await;
    general
        .derive(
            &[
                ("weekly token rotation ruling", ConceptType::Constraint),
                ("rotated the prod tokens", ConceptType::Resource),
            ],
            &ParentOf::from_pairs(&[("weekly token rotation ruling", "rotated the prod tokens")]),
        )
        .await
        .unwrap();
    general.settle_daemon().await;
    let q = RecallQuery {
        traversal_depth: 1,
        ..query("what depends on weekly token rotation ruling")
    };
    let own = project.recall_detailed(q.clone()).await.unwrap();
    let r = project
        .recall_layered(&[LayerRef::new(&general)], q)
        .await
        .unwrap();
    assert!(
        r.hits
            .iter()
            .any(|h| h.session == *general.session() && h.hit.content == "rotated the prod tokens"),
        "premise: the layer answered by traversal: {:?}",
        r.hits
    );
    assert_eq!(
        r.layer_top_legs,
        [LayerTopLegs {
            session: general.session().clone(),
            top_legs: TopLegs::default(),
        }]
    );
    assert_eq!(r.top_legs, own.top_legs);
    assert_eq!(r.confidence, own.confidence);
    project.close().await.unwrap();
    general.close().await.unwrap();
}
