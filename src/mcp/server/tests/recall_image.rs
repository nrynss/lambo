//! #22 PR 6: `lambo_recall` by `image` or `query_vector` over the wire: the
//! Dresscode "close to the one you dismissed" path, the one-of rule, the
//! optional text, the client-vector opt-in, AC4 with no data echo, the caps,
//! the named deployment refusals, and what the ledger keeps.

use super::*;
use crate::test_util::dresscode::{client_query_vector, derive_wardrobe, DISMISSED_LABEL};

const SECRET_MODEL: &str = "client-declared-model-label";

fn accepting() -> Config {
    Config {
        accept_client_vectors: true,
        ..Config::default()
    }
}

/// The Dresscode query photo ("a similar outfit"), base64 for the wire.
fn similar_png_b64() -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .encode(crate::test_util::dresscode::similar_query_png())
}

fn by_image(data: String) -> serde_json::Value {
    json!({"agent_id": "agent-a", "image": {"mime": "image/png", "data": data}})
}

fn by_vector(values: Vec<f32>, contract: serde_json::Value) -> serde_json::Value {
    json!({"agent_id": "agent-a", "query_vector": {"values": values, "contract": contract}})
}

/// [`image_server`] whose holder ranks the vector leg in its own graph, so
/// a recall by image or vector finds what was derived without a flush.
async fn recall_server(session: &str, config: Config) -> LamboServer {
    server_with_parts(
        session,
        Arc::new(crate::test_util::GraphRanked(
            crate::test_util::VectorSearchable(Arc::new(MemoryStore::new())),
        )),
        Arc::new(FixtureEmbedder::new()),
        fixture_contract(),
        config,
    )
    .await
}

fn live_contract() -> serde_json::Value {
    json!({"kind": "fixture", "dim": 1024})
}

/// Refused at the door: an error result with no hits. Returns the text.
async fn refused(s: &LamboServer, args: serde_json::Value) -> String {
    let out = call_raw(s, "lambo_recall", args).await;
    assert_eq!(out.is_error, Some(true), "{out:?}");
    assert!(out.structured_content.is_none(), "{out:?}");
    text_of(&out)
}

fn top_content(out: &CallToolResult) -> String {
    out.structured_content.as_ref().expect("recall payload")["hits"][0]["content"]
        .as_str()
        .expect("a top hit")
        .to_owned()
}

/// The demo path: the look dismissed for Onam is what a photo of a similar
/// outfit, and the same look as a client vector, recall first, with no text.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_similar_photo_or_its_vector_recalls_the_dismissed_look_first() {
    let s = recall_server("mcp-recall-by-image", accepting()).await;
    derive_wardrobe(&s.mem).await;
    s.mem.settle_daemon().await;

    let out = call(&s, "lambo_recall", by_image(similar_png_b64())).await;
    assert_eq!(out.is_error, Some(false), "{out:?}");
    assert_eq!(top_content(&out), "look dismissed for Onam [image:look2]");
    assert!(
        text_of(&out).contains("look dismissed for Onam [image:look2]"),
        "the context block names it: {out:?}"
    );

    let out = call(
        &s,
        "lambo_recall",
        by_vector(client_query_vector(), live_contract()),
    )
    .await;
    assert_eq!(out.is_error, Some(false), "{out:?}");
    assert_eq!(top_content(&out), "look dismissed for Onam [image:look2]");

    // Text beside the image is allowed; blank text is no text.
    let mut args = by_image(similar_png_b64());
    args["query"] = json!("   ");
    let out = call(&s, "lambo_recall", args).await;
    assert_eq!(top_content(&out), "look dismissed for Onam [image:look2]");
    s.mem.close().await.expect("close");
}

#[tokio::test]
async fn at_most_one_of_image_or_query_vector_and_text_otherwise() {
    let s = image_server("mcp-recall-by-one-of", accepting()).await;
    let mut both = by_image(similar_png_b64());
    both["query_vector"] = json!({"values": client_query_vector(), "contract": live_contract()});
    assert_eq!(
        refused(&s, both).await,
        "send at most one of image or query_vector"
    );
    // Neither: the text is required, as before, and the refusal names the
    // two alternatives (a missing `query` is this tool error now, not
    // serde's "missing field").
    for args in [
        json!({"agent_id": "agent-a"}),
        json!({"agent_id": "agent-a", "query": "  "}),
    ] {
        assert_eq!(
            refused(&s, args).await,
            "query must be a non-empty string (or send image or query_vector)"
        );
    }
    s.mem.close().await.expect("close");
}

/// Client vectors are the operator's opt-in, for recall as for derive.
#[tokio::test]
async fn a_query_vector_is_refused_unless_the_operator_accepts_them() {
    let s = image_server("mcp-recall-by-vectors-off", Config::default()).await;
    let text = refused(&s, by_vector(client_query_vector(), live_contract())).await;
    assert!(
        text.contains("[embedder] accept_client_vectors = true")
            && text.contains("LAMBO_ACCEPT_CLIENT_VECTORS"),
        "{text}"
    );
    s.mem.close().await.expect("close");
}

/// AC4 for queries: another space, the wrong width, no direction or a
/// non-finite component are refused naming `query_vector` and never quoting
/// the declared contract or a component.
#[tokio::test]
async fn a_mismatched_query_vector_is_refused_with_no_data_echo() {
    let s = image_server("mcp-recall-by-ac4", accepting()).await;
    let v = FixtureEmbedder::new().embed_sync(DISMISSED_LABEL);
    let component = format!("{}", v[0]);
    for (contract, differs) in [
        (
            json!({"kind": "fixture", "model": SECRET_MODEL, "dim": 1024}),
            "(model differs)",
        ),
        (json!({"kind": "bge_m3", "dim": 1024}), "(kind differs)"),
        (json!({"kind": "fixture", "dim": 768}), "(dim differs)"),
    ] {
        let text = refused(&s, by_vector(v.clone(), contract)).await;
        assert!(text.starts_with("query_vector.contract"), "{text}");
        assert!(text.contains(differs), "{text}");
        assert!(
            text.contains("kind=\"fixture\" model=\"\" dim=1024"),
            "{text}"
        );
        assert!(!text.contains(SECRET_MODEL), "{text}");
        assert!(!text.contains(&component), "{text}");
    }
    let text = refused(&s, by_vector(v[..512].to_vec(), live_contract())).await;
    assert!(
        text.contains("query_vector.values has 512 components"),
        "{text}"
    );
    let text = refused(&s, by_vector(vec![0.0; 1024], live_contract())).await;
    assert!(text.contains("zero norm"), "{text}");
    let mut wide = by_vector(v.clone(), live_contract());
    wide["query_vector"]["values"][3] = json!(1e300);
    assert!(refused(&s, wide).await.contains("non-finite"));
    let text = refused(&s, by_vector(vec![0.5; 4097], live_contract())).await;
    assert!(text.contains("over the limit of 4096"), "{text}");
    s.mem.close().await.expect("close");
}

/// The image caps hold before any decoding (stdio has no transport cap),
/// and no refusal quotes the payload.
#[tokio::test]
async fn image_caps_hold_before_decoding_and_never_echo_the_payload() {
    let s = image_server("mcp-recall-by-caps", Config::default()).await;
    let over = "A".repeat(crate::surface::image::MAX_IMAGE_B64_LEN + 4);
    let text = refused(&s, by_image(over)).await;
    assert!(text.contains("over the 2796204-character limit"), "{text}");
    assert!(!text.contains("AAAA"), "{text}");

    let data = similar_png_b64();
    let text = refused(&s, by_image(format!("data:image/png;base64,{data}"))).await;
    assert!(text.contains("without a data: URI prefix"), "{text}");
    assert!(!text.contains(&data[..24]), "{text}");

    let mut jpeg = by_image(data.clone());
    jpeg["image"]["mime"] = json!("image/jpeg");
    let text = refused(&s, jpeg).await;
    assert_eq!(
        text,
        "image: declared image/jpeg but the bytes are image/png"
    );

    let mut odd = by_image(data);
    odd["image"]["mime"] = json!("image/gif; secret=1");
    let text = refused(&s, odd).await;
    assert!(!text.contains("secret"), "{text}");
    s.mem.close().await.expect("close");
}

/// A text-only embedder refuses an image by name (a query vector is then
/// the way in), and a store without vector search refuses both by name.
#[tokio::test]
async fn deployment_preconditions_are_refused_by_name() {
    let s = server_with_parts(
        "mcp-recall-by-text-only",
        Arc::new(crate::test_util::VectorSearchable(Arc::new(
            MemoryStore::new(),
        ))),
        Arc::new(TextOnly(FixtureEmbedder::new())),
        fixture_contract(),
        accepting(),
    )
    .await;
    let text = refused(&s, by_image(similar_png_b64())).await;
    assert!(
        text.contains("does not embed images") && text.contains("query_vector"),
        "{text}"
    );
    let out = call(
        &s,
        "lambo_recall",
        by_vector(client_query_vector(), live_contract()),
    )
    .await;
    assert_eq!(out.is_error, Some(false), "{out:?}");
    s.mem.close().await.expect("close");

    let plain = server_with_config("mcp-recall-by-plain", accepting()).await;
    for args in [
        by_image(similar_png_b64()),
        by_vector(client_query_vector(), live_contract()),
    ] {
        let text = refused(&plain, args).await;
        assert!(text.contains("VECTOR_SEARCH"), "{text}");
    }
    plain.mem.close().await.expect("close");
}

/// The ledger's recall line names the payload kind and nothing of the
/// payload: no base64, no component, no long numeric array.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_ledger_carries_only_the_payload_kind() {
    let dir = ledger_dir("recall-by-image");
    let path = dir.join("calls.jsonl");
    let ledger = Ledger::open(&path);
    let plain = recall_server("mcp-recall-by-ledger", accepting()).await;
    derive_wardrobe(&plain.mem).await;
    let s = LamboServer::with_ledger(Arc::clone(plain.memory()), Arc::clone(&ledger));

    let data = similar_png_b64();
    let v = client_query_vector();
    for args in [
        by_image(data.clone()),
        by_vector(v.clone(), live_contract()),
        json!({"agent_id": "agent-a", "query": "weekend outfit"}),
    ] {
        let out = call(&s, "lambo_recall", args).await;
        assert_eq!(out.is_error, Some(false), "{out:?}");
    }
    let lines = read_ledger(&ledger, 3);
    let by: Vec<_> = lines.iter().map(|l| l["by"].clone()).collect();
    assert_eq!(
        by,
        [json!("image"), json!("vector"), json!(null)],
        "{lines:?}"
    );
    assert_eq!(lines[0]["query"], json!(""), "{lines:?}");

    let raw = std::fs::read_to_string(&path).expect("ledger file");
    assert!(!raw.contains(&data[..24]), "no base64 in the ledger");
    for x in v.iter().take(16) {
        let as_json = serde_json::to_string(x).unwrap();
        assert!(!raw.contains(&as_json), "no vector component: {as_json}");
    }
    ledger.shutdown();
    s.mem.close().await.expect("close");
}

/// The params' `Debug` shows sizes, never the base64 or a component.
#[test]
fn the_recall_params_debug_redacts_the_payload() {
    use crate::mcp::server::params::RecallParams;
    let data = similar_png_b64();
    let p: RecallParams = serde_json::from_value(by_image(data.clone())).unwrap();
    let shown = format!("{p:?}");
    assert!(!shown.contains(&data[..24]), "{shown}");
    let p: RecallParams = serde_json::from_value(by_vector(
        vec![0.123_456_7; 4],
        json!({"kind": "fixture", "model": SECRET_MODEL, "dim": 4}),
    ))
    .unwrap();
    let shown = format!("{p:?}");
    assert!(!shown.contains("0.123"), "{shown}");
    assert!(!shown.contains(SECRET_MODEL), "{shown}");
}

/// `RecallParams: Default`, so a library caller's struct literal names only
/// what it sets and survives the next optional field. The schema is
/// unchanged by it (the tool-list golden pins that).
#[tokio::test]
async fn recall_params_build_from_default() {
    use crate::mcp::server::params::RecallParams;
    let p = RecallParams {
        agent_id: "agent-a".into(),
        query: "weekend outfit".into(),
        ..Default::default()
    };
    assert!(p.image.is_none() && p.query_vector.is_none() && p.top_k.is_none());
    let s = image_server("mcp-recall-params-default", accepting()).await;
    let r = s.recall_impl(p).await;
    assert_eq!(r.is_error, Some(false), "{r:?}");
    s.mem.close().await.expect("close");
}
