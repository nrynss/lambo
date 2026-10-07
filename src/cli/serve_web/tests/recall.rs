//! /api/recall: the context block, typed hits and annotations.

use super::*;

// ---- (b) endpoints against a seeded session ------------------------

#[tokio::test]
async fn recall_endpoint_returns_the_context_block_verbatim() {
    let store = seed("t85-recall").await;
    let state = state_on(store.clone(), "t85-recall");
    let (addr, handle) = spawn(state).await;

    let body = get_json(addr, "/api/recall?q=update%20user%20schema").await;
    let context = body["context"].as_str().expect("context string");

    // H3 single-execution parity: the CLI string is derived from THIS
    // payload's own structured data through the same pub(crate) renderer
    // the CLI uses — no second recall run as an oracle. The endpoint ran
    // recall exactly once; `run_detailed` rendered `context` from the
    // presentation model that `hits` / `response_annotations` serialize,
    // and re-rendering those fields here must reproduce it byte-for-byte.
    let detail: crate::recall::detail::DetailedRecall =
        serde_json::from_value(body.clone()).expect("payload deserializes into the seam");
    let expected = crate::cli::recall::render_cli_text(&detail);
    assert_eq!(
        context, expected,
        "the page's context must equal the CLI renderer's output for the same run\
             \npage:\n{context}\nrenderer:\n{expected}"
    );

    assert!(
        context.contains("user schema"),
        "recall must name the seeded concept: {context}"
    );
    assert!(
        context.contains(", canonical]"),
        "the canonical marker must survive to the page verbatim: {context}"
    );
    assert!(
        context.contains('⚑'),
        "the ⚑ blast-radius warning must survive to the page verbatim: {context}"
    );
    // The structured payload rides beside the verbatim block: every hit
    // the renderer used is present as a card source.
    let hits = body["hits"].as_array().expect("hits array");
    assert!(
        !hits.is_empty(),
        "structured hits must accompany the context"
    );
    for h in hits {
        assert!(
            h["included_in_context"].as_bool().is_some(),
            "every hit carries included_in_context: {h}"
        );
    }

    handle.abort();
}

#[tokio::test]
async fn recall_endpoint_payload_carries_typed_hits_and_warning_parity() {
    let store = seed("t85-recall-h3").await;
    let (addr, handle) = spawn(state_on(store, "t85-recall-h3")).await;

    let body = get_json(addr, "/api/recall?q=update%20user%20schema").await;
    let context = body["context"].as_str().expect("context");
    let hits = body["hits"].as_array().expect("hits").clone();
    let response_annotations = body["response_annotations"]
        .as_array()
        .expect("response_annotations");
    assert!(
        response_annotations.is_empty(),
        "a blended recall has no response-global annotations: {response_annotations:?}"
    );

    // The canonical seeded hit ranks first with its full status and the
    // load-bearing annotation — status from the graph snapshot, never a
    // `is_canonical` reconstruction.
    let top = &hits[0];
    assert_eq!(top["status"], "Canonical", "{top}");
    assert!(
        top["included_in_context"].as_bool().unwrap_or(false),
        "{top}"
    );
    let kinds: Vec<&str> = top["annotations"]
        .as_array()
        .expect("annotations")
        .iter()
        .map(|a| a["kind"].as_str().expect("kind"))
        .collect();
    assert!(
        kinds.contains(&"load_bearing"),
        "the canonical hit owns a load_bearing annotation: {top}"
    );

    // Warning parity (one direction): every typed annotation text appears
    // verbatim in the context — included hits inside their block, response
    // annotations as header lines.
    let mut ann_texts: Vec<String> = Vec::new();
    for h in &hits {
        for a in h["annotations"].as_array().expect("annotations") {
            ann_texts.push(a["text"].as_str().expect("text").to_string());
        }
    }
    for a in response_annotations {
        ann_texts.push(a["text"].as_str().expect("text").to_string());
    }
    assert!(!ann_texts.is_empty(), "seeded recall must carry warnings");
    // Warning-line parity per text (H3 losslessness is per warning LINE,
    // not per distinct text): two hits may legitimately share an
    // identical warning line (e.g. two canonical hits with the same blast
    // radius, or two conflicts with the same writer and age), so the
    // number of times a text renders in `context` must equal the number
    // of annotations carrying that text — never a hard "exactly once".
    // A header-rendered line carries the `⚑ ` prefix when the text itself
    // does not (`render_cli_text`'s `push_header`), so count the line
    // with and without that prefix; a text never renders both ways, since
    // an included block's line is never duplicated into the header.
    for text in &ann_texts {
        let expected = ann_texts.iter().filter(|t| *t == text).count();
        let prefixed = format!("⚑ {text}");
        let actual = context
            .lines()
            .filter(|l| *l == text.as_str() || *l == prefixed.as_str())
            .count();
        assert_eq!(
            actual, expected,
            "warning-line parity: every {text:?} warning line in context must have \
                 one typed counterpart (expected {expected} annotations, found {actual} lines)\n\
                 {context}"
        );
    }

    handle.abort();
}

#[tokio::test]
async fn recall_endpoint_tiny_budget_excludes_block_but_keeps_its_warning() {
    let store = seed("t85-recall-tiny").await;
    let (addr, handle) = spawn(state_on(store, "t85-recall-tiny")).await;

    // A deliberately tiny budget: no complete block fits, so every hit is
    // excluded — but the Canonical hit's load-bearing warning must still
    // render in the header and stay visible as a typed annotation.
    let body = get_json(addr, "/api/recall?q=update%20user%20schema&max_tokens=1").await;
    let context = body["context"].as_str().expect("context");
    let hits = body["hits"].as_array().expect("hits").clone();

    assert!(!hits.is_empty(), "hits remain present under a tiny budget");
    let top = &hits[0];
    assert_eq!(
        top["included_in_context"].as_bool(),
        Some(false),
        "the canonical hit's complete block must be excluded: {top}"
    );
    let annotations = top["annotations"].as_array().expect("annotations");
    let bearing: Vec<&serde_json::Value> = annotations
        .iter()
        .filter(|a| a["kind"].as_str() == Some("load_bearing"))
        .collect();
    assert_eq!(
        bearing.len(),
        1,
        "exclusion discards the block, not the annotation: {top}"
    );
    let warning = bearing[0]["text"].as_str().expect("text");
    assert!(
        context.contains(warning),
        "the excluded hit's warning must remain in context:\n{context}"
    );
    assert!(
        context.contains("Load-bearing pillar"),
        "the ⚑ line is the retained warning: {context}"
    );

    handle.abort();
}

struct FailingEmbedder;

#[async_trait]
impl crate::embed::Embedder for FailingEmbedder {
    fn dimensions(&self) -> usize {
        1024
    }
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        Err(crate::embed::EmbedError::Unavailable("down".into()))
    }
}

#[tokio::test]
async fn recall_endpoint_reports_vector_degradation_as_response_annotation() {
    let store = seed("t85-recall-degraded").await;
    let mut backends = backends_on(store.clone());

    backends.embedder = Box::new(FailingEmbedder);
    backends.store = Box::new(VectorSearch(Shared(store.clone())));
    let (addr, handle) = spawn(state_from_backends(backends, "t85-recall-degraded", None)).await;
    let body = get_json(addr, "/api/recall?q=update%20user%20schema").await;
    let context = body["context"].as_str().expect("context");
    let annotations = body["response_annotations"]
        .as_array()
        .expect("response_annotations");
    assert_eq!(annotations.len(), 1, "{annotations:?}");
    assert_eq!(annotations[0]["kind"], "vector_degraded", "{annotations:?}");
    assert!(
        context.contains("vector leg skipped"),
        "the degradation must still render in context: {context}"
    );
    assert_eq!(
        annotations[0]["text"],
        "recall: query embedding failed (embedder unavailable: down); vector leg skipped"
    );

    handle.abort();
}

#[tokio::test]
async fn recall_endpoint_structural_payload_carries_traversal_response_annotation() {
    // A structural query that dispatches: seed a dependency chain so
    // "what depends on X" resolves an anchor with dependents.
    let store = seed_chain_around("t85-recall-structural", "the anchor", 3).await;
    let (addr, handle) = spawn(state_on(store, "t85-recall-structural")).await;

    let body = get_json(addr, "/api/recall?q=what%20depends%20on%20the%20anchor").await;
    let context = body["context"].as_str().expect("context");
    let annotations = body["response_annotations"]
        .as_array()
        .expect("response_annotations");
    assert_eq!(
        annotations.len(),
        1,
        "one response-global explanation: {annotations:?}"
    );
    assert_eq!(annotations[0]["kind"], "traversal", "{annotations:?}");
    assert!(
        context.contains("answered by graph traversal"),
        "the traversal explanation renders in context: {context}"
    );
    let hits = body["hits"].as_array().expect("hits");
    assert!(!hits.is_empty(), "structural hits ride in the payload");
    assert!(
        hits.iter()
            .all(|h| h["included_in_context"].as_bool().unwrap_or(false)),
        "all structural hits fit the default budget: {hits:?}"
    );

    handle.abort();
}

#[tokio::test]
async fn recall_endpoint_rejects_a_missing_query_without_touching_the_store() {
    let store = seed("t85-recall-usage").await;
    let (addr, handle) = spawn(state_on(store, "t85-recall-usage")).await;

    let r = request(addr, "GET", "/api/recall").await;
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(r.body.contains("error"), "{}", r.body);

    let blank = request(addr, "GET", "/api/recall?q=%20%20").await;
    assert_eq!(blank.status, 400, "{}", blank.body);

    handle.abort();
}
