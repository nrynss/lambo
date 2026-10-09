//! #22 PR 4: `lambo_derive_image` over the wire: the one-of payload, the
//! base64 and size caps, the client-vector opt-in, AC4 (a mismatched
//! contract refused with no data echo), the image-id collision on the
//! receipt, and what the ledger keeps.

use super::*;

const SECRET_MODEL: &str = "client-declared-model-label";

/// Wait on a write's receipt through the shipped `lambo_stats` surface and
/// return the receipt object.
async fn settled_receipt(s: &LamboServer, ack: &CallToolResult) -> serde_json::Value {
    let receipt = ack.structured_content.as_ref().expect("ack payload")["receipt"]
        .as_str()
        .expect("the ack carries a receipt")
        .to_string();
    let out = call_raw(
        s,
        "lambo_stats",
        json!({
            "agent_id": "agent-a",
            "receipt": receipt,
            "wait_ms": crate::writeq::RECEIPT_WAIT_MAX.as_millis() as u64,
        }),
    )
    .await;
    out.structured_content.expect("stats payload")["receipt"].clone()
}

fn image_args(caption: &str, image_id: &str, label: &str) -> serde_json::Value {
    json!({
        "agent_id": "agent-a",
        "caption": caption,
        "concept_type": "resource",
        "image_id": image_id,
        "image": {"mime": "image/png", "data": png_b64(label)},
    })
}

fn vector_args(values: Vec<f32>, contract: serde_json::Value) -> serde_json::Value {
    json!({
        "agent_id": "agent-a",
        "caption": "red silk saree",
        "concept_type": "resource",
        "image_id": "r17",
        "vector": {"values": values, "contract": contract},
    })
}

fn fixture_vector(label: &str) -> Vec<f32> {
    FixtureEmbedder::new().embed_sync(label)
}

fn image_concepts(s: &LamboServer) -> Vec<crate::types::Concept> {
    s.mem
        .graph()
        .read()
        .concepts()
        .filter(|c| c.embedding_source.is_some())
        .cloned()
        .collect()
}

/// Refused at the door: an error result with no receipt, and nothing in the
/// graph. Returns the text the caller reads.
async fn refused(s: &LamboServer, args: serde_json::Value) -> String {
    let before = s.mem.stats().node_count;
    let out = call_raw(s, "lambo_derive_image", args).await;
    assert_eq!(out.is_error, Some(true), "{out:?}");
    assert!(
        out.structured_content.is_none(),
        "no receipt was issued: {out:?}"
    );
    assert_eq!(s.mem.stats().node_count, before, "nothing was written");
    text_of(&out)
}

/// The image path end to end: acked with a receipt, applied as one image
/// concept whose vector is the image's, and found by a text recall.
#[tokio::test]
async fn an_image_derive_is_acked_applied_and_recalled_by_text() {
    let s = image_server("mcp-image-bytes", Config::default()).await;
    let ack = call(
        &s,
        "lambo_derive_image",
        image_args("render 17", "r17", "red silk saree"),
    )
    .await;
    assert_eq!(ack.is_error, Some(false), "{ack:?}");
    assert!(
        text_of(&ack).contains("accepted 1 image concept"),
        "{ack:?}"
    );
    let receipt = settled_receipt(&s, &ack).await;
    assert_eq!(receipt["state"], json!("applied"), "{receipt}");
    assert_eq!(receipt["kind"], json!("lambo_derive_image"), "{receipt}");

    let [c] = image_concepts(&s).try_into().expect("one image concept");
    assert_eq!(c.content, "render 17 [image:r17]");
    assert_eq!(c.embedding, Some(fixture_vector("red silk saree")));
    let source = c.embedding_source.expect("source");
    assert_eq!(source.origin, crate::types::VectorOrigin::Server);

    let recall = call(
        &s,
        "lambo_recall",
        json!({"agent_id": "agent-a", "query": "red silk saree"}),
    )
    .await;
    assert!(text_of(&recall).contains("[image:r17]"), "{recall:?}");
    s.mem.close().await.expect("close");
}

#[tokio::test]
async fn exactly_one_of_image_or_vector_is_required() {
    let s = image_server(
        "mcp-image-one-of",
        Config {
            accept_client_vectors: true,
            ..Config::default()
        },
    )
    .await;
    let neither = json!({"agent_id": "agent-a", "caption": "x", "concept_type": "entity"});
    assert_eq!(
        refused(&s, neither).await,
        "send exactly one of image or vector"
    );
    let mut both = image_args("x", "a1", "x");
    both["vector"] =
        json!({"values": fixture_vector("x"), "contract": {"kind": "fixture", "dim": 1024}});
    assert_eq!(
        refused(&s, both).await,
        "send exactly one of image or vector"
    );
    s.mem.close().await.expect("close");
}

/// The base64 cap holds before any decoding (stdio has no transport cap),
/// and no refusal quotes the payload.
#[tokio::test]
async fn base64_and_image_refusals_never_echo_the_payload() {
    use base64::Engine as _;
    let s = image_server("mcp-image-b64", Config::default()).await;
    let with_data = |mime: &str, data: String| {
        json!({
            "agent_id": "agent-a",
            "caption": "x",
            "concept_type": "entity",
            "image": {"mime": mime, "data": data},
        })
    };

    let over = "QUJD".repeat(crate::surface::image::MAX_IMAGE_B64_LEN / 4 + 1);
    let text = refused(&s, with_data("image/png", over)).await;
    assert!(text.contains("2796204-character limit"), "{text}");
    assert!(!text.contains("QUJD"), "{text}");

    let text = refused(&s, with_data("image/png", "%%not-base64%%".into())).await;
    assert_eq!(
        text,
        "image.data is not valid base64 (standard alphabet, padded)"
    );

    // Valid base64 of something that is not an image.
    let junk = base64::engine::general_purpose::STANDARD.encode(b"plain text, no magic");
    let text = refused(&s, with_data("image/png", junk)).await;
    assert!(text.contains("not a PNG, JPEG or WebP"), "{text}");

    // A declared type that is not the sniffed one, and one off the list.
    let png = png_b64("x");
    let text = refused(&s, with_data("image/jpeg", png.clone())).await;
    assert!(
        text.contains("declared image/jpeg but the bytes are image/png"),
        "{text}"
    );
    let text = refused(&s, with_data("image/svg+xml;LONG-CLIENT-TEXT", png.clone())).await;
    assert!(!text.contains("LONG-CLIENT-TEXT"), "{text}");
    assert!(!text.contains(&png[..32]), "{text}");
    s.mem.close().await.expect("close");
}

#[tokio::test]
async fn caption_id_and_parent_of_follow_the_image_rules() {
    let s = image_server("mcp-image-fields", Config::default()).await;
    let text = refused(&s, image_args("red [image:zzz]", "a1", "x")).await;
    assert!(text.contains("caption may not contain"), "{text}");
    for bad in ["Red17", "red-17", "red_17", ""] {
        let text = refused(&s, image_args("red", bad, "x")).await;
        assert!(text.starts_with("image_id m"), "{bad:?}: {text}");
        assert!(bad.is_empty() || !text.contains(bad), "{text}");
    }
    let text = refused(&s, image_args("   ", "a1", "x")).await;
    assert!(text.contains("caption"), "{text}");
    let mut pairs = image_args("red", "a1", "x");
    pairs["parent_of"] = json!([{"parent": " ", "child": "red [image:a1]"}]);
    assert!(refused(&s, pairs).await.contains("parent_of"));
    s.mem.close().await.expect("close");
}

/// Review M2: a caption the content cap cannot hold once the suffix is
/// appended is the caller's error, refused as a parameter that names the
/// real limit, never an opaque configuration error.
#[tokio::test]
async fn a_caption_too_long_for_its_suffix_is_a_bad_param_naming_the_limit() {
    let s = image_server("mcp-image-caption-cap", Config::default()).await;
    // Under the uniform 16384 cap, over the caption's real one.
    let text = refused(&s, image_args(&"c".repeat(16_380), "r17", "x")).await;
    assert!(
        text.contains("caption is 16380 bytes; with a 3-byte image_id it may be at most 16372"),
        "{text}"
    );
    assert!(!text.contains("configuration error"), "{text}");
    let mut default_id = image_args(&"c".repeat(16_360), "r17", "x");
    default_id.as_object_mut().unwrap().remove("image_id");
    let text = refused(&s, default_id).await;
    assert!(text.contains("at most 16359 bytes"), "{text}");

    // At the limit, the derive is accepted and the content is exactly at the cap.
    let at = "c".repeat(crate::surface::image::max_caption_bytes(3));
    let ack = call(&s, "lambo_derive_image", image_args(&at, "r17", "x")).await;
    assert_eq!(ack.is_error, Some(false), "{ack:?}");
    let receipt = settled_receipt(&s, &ack).await;
    assert_eq!(receipt["state"], json!("applied"), "{receipt}");
    let [c] = image_concepts(&s).try_into().expect("one image concept");
    assert_eq!(c.content.len(), crate::surface::limits::MAX_CONTENT_BYTES);
    s.mem.close().await.expect("close");
}

/// Client vectors are the operator's opt-in (design 3.3): with the key off the
/// call is refused and the refusal names it.
#[tokio::test]
async fn a_client_vector_is_refused_unless_the_operator_accepts_them() {
    let s = image_server("mcp-image-vectors-off", Config::default()).await;
    let args = vector_args(
        fixture_vector("red silk saree"),
        json!({"kind": "fixture", "dim": 1024}),
    );
    let text = refused(&s, args).await;
    assert!(
        text.contains("[embedder] accept_client_vectors = true")
            && text.contains("LAMBO_ACCEPT_CLIENT_VECTORS"),
        "{text}"
    );
    s.mem.close().await.expect("close");
}

/// AC4 at the wire: a vector in another space, of the wrong width, or with no
/// direction is refused on the call path with a message that names the rule
/// and never quotes the declared contract or a component.
#[tokio::test]
async fn a_mismatched_client_vector_is_refused_with_no_data_echo() {
    let s = image_server(
        "mcp-image-ac4",
        Config {
            accept_client_vectors: true,
            ..Config::default()
        },
    )
    .await;
    let v = fixture_vector("red silk saree");
    let component = format!("{}", v[0]);

    for (contract, differs) in [
        (
            json!({"kind": "fixture", "model": SECRET_MODEL, "dim": 1024}),
            "(model differs)",
        ),
        (json!({"kind": "bge_m3", "dim": 1024}), "(kind differs)"),
        (json!({"kind": "fixture", "dim": 768}), "(dim differs)"),
    ] {
        let text = refused(&s, vector_args(v.clone(), contract)).await;
        assert!(text.contains(differs), "{text}");
        assert!(
            text.contains("kind=\"fixture\" model=\"\" dim=1024"),
            "{text}"
        );
        assert!(!text.contains(SECRET_MODEL), "{text}");
        assert!(!text.contains(&component), "{text}");
    }
    let live = json!({"kind": "fixture", "dim": 1024});
    let text = refused(&s, vector_args(v[..512].to_vec(), live.clone())).await;
    assert!(text.contains("512 components"), "{text}");
    let text = refused(&s, vector_args(vec![0.0; 1024], live.clone())).await;
    assert!(text.contains("zero norm"), "{text}");
    // A number past f32's range arrives as infinity.
    let mut wide = vector_args(v.clone(), live.clone());
    wide["vector"]["values"][3] = json!(1e300);
    assert!(refused(&s, wide).await.contains("non-finite"));
    assert!(image_concepts(&s).is_empty());

    // The control: the right space is accepted, renormalized, labelled client.
    let scaled: Vec<f32> = v.iter().map(|x| x * 3.0).collect();
    let ack = call(&s, "lambo_derive_image", vector_args(scaled, live)).await;
    assert_eq!(ack.is_error, Some(false), "{ack:?}");
    assert_eq!(settled_receipt(&s, &ack).await["state"], json!("applied"));
    let [c] = image_concepts(&s).try_into().expect("one image concept");
    assert_eq!(c.content, "red silk saree [image:r17]");
    assert_eq!(
        c.embedding_source.expect("source").origin,
        crate::types::VectorOrigin::Client
    );
    let norm: f32 = c
        .embedding
        .expect("vector")
        .iter()
        .map(|x| x * x)
        .sum::<f32>();
    assert!((norm - 1.0).abs() < 1e-4, "renormalized: {norm}");
    s.mem.close().await.expect("close");
}

/// An embedder that cannot embed images still lists the tool when client
/// vectors are on; the image path then names what is missing.
#[tokio::test]
async fn a_text_only_server_with_client_vectors_refuses_bytes_by_name() {
    let s = server_with_parts(
        "mcp-image-text-only",
        Arc::new(crate::test_util::VectorSearchable(Arc::new(
            MemoryStore::new(),
        ))),
        Arc::new(TextOnly(FixtureEmbedder::new())),
        fixture_contract(),
        Config {
            accept_client_vectors: true,
            ..Config::default()
        },
    )
    .await;
    let text = refused(&s, image_args("render 17", "r17", "red silk saree")).await;
    assert!(text.contains("does not embed images"), "{text}");
    let ack = call(
        &s,
        "lambo_derive_image",
        vector_args(
            fixture_vector("red silk saree"),
            json!({"kind": "fixture", "dim": 1024}),
        ),
    )
    .await;
    assert_eq!(settled_receipt(&s, &ack).await["state"], json!("applied"));
    s.mem.close().await.expect("close");
}

#[tokio::test]
async fn the_canonical_strategy_refuses_by_name() {
    let s = image_server(
        "mcp-image-canonical",
        Config {
            match_strategy: crate::types::MatchStrategy::Canonical,
            ..Config::default()
        },
    )
    .await;
    let text = refused(&s, image_args("render 17", "r17", "x")).await;
    assert!(text.contains("match_strategy = \"hybrid\""), "{text}");
    s.mem.close().await.expect("close");
}

/// Review M3: over a store without vector search the tool is still listed
/// (design 6.1 lists on the embedder and the client-vector key), and the
/// refusal names the missing capability instead of a bare class.
#[tokio::test]
async fn a_store_without_vector_search_refuses_by_name() {
    let s = server_with_parts(
        "mcp-image-no-vectors",
        Arc::new(MemoryStore::new()),
        Arc::new(FixtureEmbedder::new()),
        fixture_contract(),
        Config::default(),
    )
    .await;
    assert!(
        tools(&s).iter().any(|t| t.name == "lambo_derive_image"),
        "listed: the embedder embeds images"
    );
    let text = refused(&s, image_args("render 17", "r17", "x")).await;
    assert!(text.contains("VECTOR_SEARCH"), "{text}");
    assert!(!text.contains("logged server-side"), "{text}");
    s.mem.close().await.expect("close");
}

/// The PR 3 carry-over: when a text write has already taken the image's key
/// (here a reference in an action, which the door allows), the image derive's
/// receipt says what happened and how to fix it, naming only the caller's id.
#[tokio::test]
async fn an_image_id_taken_by_text_fails_its_receipt_with_the_fix() {
    let s = image_server("mcp-image-taken", Config::default()).await;
    let action = call(
        &s,
        "lambo_record_action",
        json!({"agent_id": "agent-a", "action": "dismissed the outfit",
               "depends_on": ["render 17 [image:r17]"]}),
    )
    .await;
    assert_eq!(action.is_error, Some(false), "{action:?}");
    let ack = call(
        &s,
        "lambo_derive_image",
        image_args("render 17", "r17", "red silk saree"),
    )
    .await;
    assert_eq!(ack.is_error, Some(false), "acked: {ack:?}");
    let receipt = settled_receipt(&s, &ack).await;
    assert_eq!(receipt["state"], json!("failed"), "{receipt}");
    let detail = receipt["detail"].as_str().expect("detail");
    assert!(detail.contains("choose another image id"), "{detail}");
    assert!(detail.contains("\"r17\""), "{detail}");
    assert!(!detail.contains("logged server-side"), "{detail}");
    assert!(image_concepts(&s).is_empty());
    s.mem.close().await.expect("close");
}

/// Ledger hygiene: the call line says which payload kind was sent and
/// nothing of it; no line carries the base64, the caption or a component.
#[tokio::test]
async fn the_ledger_carries_no_image_payload() {
    let dir = ledger_dir("image-payload");
    let path = dir.join("calls.jsonl");
    let plain = image_server(
        "mcp-image-ledger",
        Config {
            accept_client_vectors: true,
            ..Config::default()
        },
    )
    .await;
    let s = LamboServer::with_ledger(Arc::clone(plain.memory()), Ledger::open(&path));
    let ledger = Arc::clone(s.ledger().expect("ledger attached"));

    let data = png_b64("red silk saree");
    let ack = call(
        &s,
        "lambo_derive_image",
        image_args("render seventeen", "r17", "red silk saree"),
    )
    .await;
    assert_eq!(ack.is_error, Some(false), "{ack:?}");
    let v = fixture_vector("blue linen kurta");
    let ack = call(
        &s,
        "lambo_derive_image",
        json!({
            "agent_id": "agent-a",
            "caption": "render eighteen",
            "concept_type": "resource",
            "vector": {"values": v, "contract": {"kind": "fixture", "dim": 1024}},
        }),
    )
    .await;
    assert_eq!(ack.is_error, Some(false), "{ack:?}");
    // One call line each.
    let lines = read_ledger(&ledger, 2);
    let calls: Vec<_> = lines
        .iter()
        .filter(|l| l["kind"] == json!("call"))
        .collect();
    assert_eq!(calls[0]["payload"], json!("image"), "{}", calls[0]);
    assert_eq!(calls[1]["payload"], json!("vector"), "{}", calls[1]);

    let raw = std::fs::read_to_string(&path).expect("ledger file");
    assert!(!raw.contains(&data[..24]), "no base64 in the ledger");
    assert!(!raw.contains("render seventeen") && !raw.contains("render eighteen"));
    for x in v.iter().take(8) {
        assert!(!raw.contains(&format!("{x}")), "no vector component: {x}");
    }
    ledger.shutdown();
    s.mem.close().await.expect("close");
}

/// Review L4: the params' `Debug` shows sizes, never the base64, the
/// caption, the client's strings or a vector component.
#[test]
fn the_params_debug_redacts_the_payload() {
    use crate::mcp::server::params::DeriveImageParams;
    let data = png_b64("red silk saree");
    let v = fixture_vector("red silk saree");
    for args in [
        image_args("SECRET-CAPTION", "secretid", "red silk saree"),
        json!({"agent_id": "agent-a", "caption": "SECRET-CAPTION", "concept_type": "entity",
               "vector": {"values": v, "contract": {"kind": "fixture", "dim": 1024,
                                                    "model": "SECRET-MODEL"}}}),
    ] {
        let p: DeriveImageParams = serde_json::from_value(args).unwrap();
        let shown = format!("{p:?}");
        assert!(shown.contains("caption_len: 14"), "{shown}");
        for secret in ["SECRET-CAPTION", "secretid", "SECRET-MODEL", &data[..24]] {
            assert!(!shown.contains(secret), "{secret}: {shown}");
        }
        for x in v.iter().take(8) {
            assert!(!shown.contains(&format!("{x}")), "{x}: {shown}");
        }
    }
}
