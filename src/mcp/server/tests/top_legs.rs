//! #256: `lambo_recall`'s structured content and its ledger line carry the
//! top per-leg scores, and the provisional confidence as an object that
//! says so, under the same names.

use super::layered::{
    derive, layered, recall, structured, vector_general, GENERAL_RULING, PROJECT_FACT,
};
use super::*;
use crate::recall::confidence::noisy_or;

const KEYWORD_HIT: &str = "auth tokens rotate weekly";
const VECTOR_HIT: &str = "zebra crossing paint";

/// The EmbeddingGemma 2 contract, which has a calibration row and so a
/// measured null median (0.632).
fn eg2_contract() -> EmbeddingContract {
    let row = &crate::embed::calibration::BUILTIN[0];
    EmbeddingContract {
        kind: row.kind.into(),
        model: Some(row.model.into()),
        dim: row.dim,
    }
}

fn sorted_keys(value: &serde_json::Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort();
    keys
}

/// Both legs under a measured contract: `top_legs` holds two numbers that
/// are the legs' best over every candidate (only one hit is returned), and
/// `confidence` is `{value, basis, provisional}`, never a bare number. The
/// ledger line carries the same two keys with the same values.
#[tokio::test]
async fn the_structured_content_and_the_ledger_carry_both_under_one_shape() {
    let dir = ledger_dir("top-legs");
    let path = dir.join("calls.jsonl");
    let contract = eg2_contract();
    let plain = server_with_parts(
        "top-legs-wire",
        Arc::new(crate::test_util::GraphRanked(
            crate::test_util::VectorSearchable(Arc::new(MemoryStore::new())),
        )),
        Arc::new(FixtureEmbedder::with_dimensions(contract.dim).unwrap()),
        contract,
        Config::default(),
    )
    .await;
    let s = LamboServer::with_ledger(Arc::clone(plain.memory()), Ledger::open(&path));
    let ledger = Arc::clone(s.ledger().expect("ledger"));
    s.memory()
        .derive(
            &[
                (KEYWORD_HIT, ConceptType::Logic),
                (VECTOR_HIT, ConceptType::Observation),
            ],
            &ParentOf::none(),
        )
        .await
        .expect("derive");
    // The concept sharing no word with the query sits at cosine 0.70 to it.
    crate::test_util::seed_layer_match(s.memory(), VECTOR_HIT, "auth tokens").await;

    let out = recall(&s, json!({"query": "auth tokens", "top_k": 1})).await;
    assert_eq!(out.is_error, Some(false), "{out:?}");
    let payload = structured(&out);
    assert_eq!(payload["hits"].as_array().expect("hits").len(), 1);
    assert_eq!(
        sorted_keys(payload),
        ["confidence", "context", "hits", "top_legs", "warnings"]
    );

    let top = &payload["top_legs"];
    assert_eq!(sorted_keys(top), ["bm25", "vector_cosine"]);
    let bm25 = top["bm25"].as_f64().expect("a keyword score");
    let cosine = top["vector_cosine"].as_f64().expect("a cosine");
    assert!(bm25 > 0.0);
    assert!((cosine - 0.70).abs() < 1e-4, "{cosine}");

    // Provisional in the shape itself: an object, with its basis named.
    let confidence = &payload["confidence"];
    assert!(confidence.is_object(), "never a bare number: {confidence}");
    assert_eq!(sorted_keys(confidence), ["basis", "provisional", "value"]);
    assert_eq!(confidence["basis"], json!("noisy-or-v0"));
    assert_eq!(confidence["provisional"], json!(true));
    let expected = noisy_or(bm25, cosine, 0.632).expect("finite");
    let value = confidence["value"].as_f64().expect("value");
    assert!((value - expected).abs() < 1e-12, "{value} vs {expected}");
    assert!((0.0..=1.0).contains(&value));

    // The context block and its text content say nothing about either.
    let context = payload["context"].as_str().expect("context");
    assert!(!context.contains("confidence") && !context.contains("top_legs"));
    assert_eq!(out.content.len(), 1, "no warning line was added: {out:?}");

    let lines = read_ledger(&ledger, 1);
    assert_eq!(lines[0]["tool"], json!("lambo_recall"));
    assert_eq!(&lines[0]["top_legs"], top);
    assert_eq!(&lines[0]["confidence"], confidence);
    assert!(lines[0].get("layer_top_legs").is_none());

    // No tool publishes an output schema, so there is none to extend; the
    // input schema (the committed golden) is unchanged.
    let tool = tools(&s)
        .into_iter()
        .find(|t| t.name == "lambo_recall")
        .expect("listed");
    assert!(tool.output_schema.is_none());
    s.mem.close().await.expect("close");
}

/// No vector leg: the keyword leg is still reported, the cosine is `null`,
/// and there is no `confidence` key at all, in the answer or the ledger.
#[tokio::test]
async fn a_keyword_only_recall_has_a_null_cosine_and_no_confidence_key() {
    let dir = ledger_dir("top-legs-keyword");
    let path = dir.join("calls.jsonl");
    let s = server_with_ledger("top-legs-keyword", &path).await;
    let ledger = Arc::clone(s.ledger().expect("ledger"));
    derive(&s, PROJECT_FACT).await;
    let out = recall(&s, json!({"query": "worktree"})).await;
    let payload = structured(&out);
    assert_eq!(
        sorted_keys(payload),
        ["context", "hits", "top_legs", "warnings"]
    );
    assert!(payload["top_legs"]["bm25"]
        .as_f64()
        .is_some_and(|s| s > 0.0));
    assert!(payload["top_legs"]["vector_cosine"].is_null());

    // A query that matches nothing: the leg ran, so it reports 0.
    let none = recall(&s, json!({"query": "qqqq wwww"})).await;
    assert_eq!(
        structured(&none)["top_legs"],
        json!({"bm25": 0.0, "vector_cosine": null})
    );

    let lines = read_ledger(&ledger, 3);
    assert_eq!(lines[1]["top_legs"], payload["top_legs"]);
    assert!(lines[1].get("confidence").is_none());
    assert_eq!(
        lines[2]["top_legs"],
        json!({"bm25": 0.0, "vector_cosine": null})
    );
    s.mem.close().await.expect("close");
}

/// A layered session: `top_legs` is the caller's own session's (no vector
/// leg here, so a null cosine), and the layer's best scores are apart, under
/// `layer_top_legs`, in the answer and on the ledger line.
#[tokio::test]
async fn a_layered_recall_keeps_the_layers_top_legs_apart() {
    let dir = ledger_dir("top-legs-layered");
    let path = dir.join("calls.jsonl");
    let own = server_with_ledger("top-legs-proj", &path).await;
    let ledger = Arc::clone(own.ledger().expect("ledger"));
    let general = vector_general().await;
    derive(&own, PROJECT_FACT).await;
    derive(&general, GENERAL_RULING).await;
    let (s, _source) = layered(&own, &general);

    let alone = recall(&own, json!({"query": "worktree branches"})).await;
    let merged = recall(&s, json!({"query": "worktree branches"})).await;
    let payload = structured(&merged);
    assert!(
        payload["hits"]
            .as_array()
            .expect("hits")
            .iter()
            .any(|h| h["session"] == json!("general")),
        "premise: the layer's hit merged"
    );
    // Primary: exactly the caller's own session's plain answer.
    assert_eq!(payload["top_legs"], structured(&alone)["top_legs"]);
    assert!(payload["top_legs"]["vector_cosine"].is_null());
    assert!(payload.get("confidence").is_none());
    // The layer's, apart: it holds the 0.70 the primary does not report.
    let layers = payload["layer_top_legs"].as_array().expect("layers");
    assert_eq!(layers.len(), 1);
    assert_eq!(
        sorted_keys(&layers[0]),
        ["bm25", "session", "vector_cosine"]
    );
    assert_eq!(layers[0]["session"], json!("general"));
    assert!(layers[0]["bm25"].as_f64().is_some_and(|s| s > 0.0));
    let cosine = layers[0]["vector_cosine"].as_f64().expect("layer cosine");
    assert!((cosine - 0.70).abs() < 1e-4, "{cosine}");
    assert!(structured(&alone).get("layer_top_legs").is_none());

    let lines = read_ledger(&ledger, 3);
    let line = lines
        .iter()
        .find(|l| l.get("session_hits").is_some())
        .expect("the layered recall's line");
    assert_eq!(line["top_legs"], payload["top_legs"]);
    assert_eq!(line["layer_top_legs"], payload["layer_top_legs"]);
    s.mem.close().await.expect("close");
    general.mem.close().await.expect("close");
}

/// A recall line built from a result with no legs (a refusal, a dependency
/// question answered by traversal) still has the key, with both legs null.
#[test]
fn the_recall_line_always_has_top_legs() {
    let mut detailed = crate::recall::detail::DetailedRecall::warn_only(String::new());
    let facts = recall_facts("q", 8, &detailed);
    assert_eq!(
        facts["top_legs"],
        json!({"bm25": null, "vector_cosine": null})
    );
    assert!(facts.get("confidence").is_none());
    detailed.top_legs.bm25 = Some(2.5);
    detailed.confidence = Some(crate::types::ProvisionalConfidence {
        value: 0.25,
        basis: "noisy-or-v0".into(),
        provisional: true,
    });
    let facts = recall_facts("q", 8, &detailed);
    assert_eq!(
        facts["top_legs"],
        json!({"bm25": 2.5, "vector_cosine": null})
    );
    assert_eq!(
        facts["confidence"],
        json!({"value": 0.25, "basis": "noisy-or-v0", "provisional": true})
    );
}

/// The H3 payload (`/api/recall`'s `hits` and annotations, and now the two
/// #256 keys) written before #256 still deserializes, with the fields empty.
#[test]
fn a_payload_without_the_fields_still_deserializes() {
    let old = json!({"hits": [], "response_annotations": []});
    let detailed: crate::recall::detail::DetailedRecall =
        serde_json::from_value(old).expect("an old H3 payload");
    assert_eq!(detailed.top_legs, crate::types::TopLegs::default());
    assert_eq!(detailed.confidence, None);

    let old = json!({"hits": [], "context": "", "warnings": []});
    let result: crate::types::RecallResult =
        serde_json::from_value(old).expect("an old recall result");
    assert_eq!(result.top_legs, crate::types::TopLegs::default());
    assert_eq!(result.confidence, None);

    // A half-written `top_legs` reads too.
    let partial = json!({"hits": [], "context": "", "warnings": [], "top_legs": {"bm25": 1.5}});
    let result: crate::types::RecallResult = serde_json::from_value(partial).unwrap();
    assert_eq!(result.top_legs.bm25, Some(1.5));
    assert_eq!(result.top_legs.vector_cosine, None);
}
