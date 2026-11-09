//! #233: the Elasticsearch Inference API adapter against a mock server, plus
//! one ignored live test.

use super::*;
use httpmock::prelude::*;

const ID: &str = ".jina-embeddings-v3";
const PATH: &str = "/_inference/text_embedding/.jina-embeddings-v3";

/// A fake key, built at runtime and only ever sent to a local mock server.
fn fake_key() -> String {
    format!("fake-{}", "elastic-key-xyzzy")
}

/// Deliberately non-unit, so a test proves normalization happened.
fn ok_response(dim: usize) -> serde_json::Value {
    serde_json::json!({ "text_embedding": [{ "embedding": vec![3.0f32; dim] }] })
}

fn norm(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

fn embedder(server: &MockServer) -> ElasticEmbedder {
    ElasticEmbedder::new(&server.base_url(), ID, 1024).unwrap()
}

fn has_authorization(r: &HttpMockRequest) -> bool {
    r.headers
        .as_deref()
        .unwrap_or_default()
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("authorization"))
}

/// Serve `body` with `status` on the endpoint and return the embed error.
async fn error_for(status: u16, body: &str) -> EmbedError {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path(PATH);
        then.status(status).body(body);
    });
    embedder(&server).embed("anything").await.unwrap_err()
}

/// The document role sends `input_type: INGEST`, the query role `SEARCH`,
/// each with a one-element `input` array, to the inference path; both come
/// back unit-norm.
///
/// Mutation: swap the two constants, or send `input` as a string -> red.
#[tokio::test]
async fn roles_send_ingest_and_search_and_normalize() {
    let server = MockServer::start();
    let ingest = server.mock(|when, then| {
        when.method(POST).path(PATH).json_body(serde_json::json!({
            "input": ["user schema"], "input_type": "INGEST"
        }));
        then.status(200).json_body(ok_response(1024));
    });
    let search = server.mock(|when, then| {
        when.method(POST).path(PATH).json_body(serde_json::json!({
            "input": ["who owns the schema"], "input_type": "SEARCH"
        }));
        then.status(200).json_body(ok_response(1024));
    });
    let e = embedder(&server);
    let doc = e.embed("user schema").await.unwrap();
    let query = e.embed_query("who owns the schema").await.unwrap();
    ingest.assert();
    search.assert();
    for v in [doc, query] {
        assert_eq!(v.len(), 1024);
        assert!((norm(&v) - 1.0).abs() < 1e-4, "{}", norm(&v));
    }
}

/// `send_input_type = false` omits the field for both roles (the exact body
/// match proves its absence) and the contract model says so.
#[tokio::test]
async fn input_type_can_be_omitted_for_both_roles() {
    let server = MockServer::start();
    let bare = server.mock(|when, then| {
        when.method(POST)
            .path(PATH)
            .json_body(serde_json::json!({ "input": ["user schema"] }));
        then.status(200).json_body(ok_response(1024));
    });
    let e = embedder(&server).with_send_input_type(false);
    e.embed("user schema").await.unwrap();
    e.embed_query("user schema").await.unwrap();
    bare.assert_hits(2);
    assert_eq!(e.model_identity(), ".jina-embeddings-v3;input_type=off");
    assert_eq!(embedder(&server).model_identity(), ID);
}

/// A base path (a proxy prefix, with or without a trailing slash) is kept
/// and the id is one percent-encoded segment.
#[tokio::test]
async fn a_base_path_prefix_is_kept_and_the_id_is_encoded() {
    let server = MockServer::start();
    let prefixed = server.mock(|when, then| {
        when.method(POST).path(format!("/es-proxy{PATH}"));
        then.status(200).json_body(ok_response(4));
    });
    for base in [server.url("/es-proxy"), server.url("/es-proxy/")] {
        let e = ElasticEmbedder::new(&base, ID, 4).unwrap();
        e.embed("anything").await.unwrap();
    }
    prefixed.assert_hits(2);
    let e = ElasticEmbedder::new("https://my-cluster.es.example.com", "my e5%model?x#y", 4);
    // Whitespace is refused, so check the encoding on an id without it.
    assert!(e.is_err());
    let e = ElasticEmbedder::new("https://my-cluster.es.example.com", "e5%v?x#y", 4).unwrap();
    assert_eq!(
        e.url.as_str(),
        "https://my-cluster.es.example.com/_inference/text_embedding/e5%25v%3Fx%23y"
    );
}

/// With a key every request carries `Authorization: ApiKey <key>` (not
/// `Bearer`); without one no `Authorization` header is sent.
#[tokio::test]
async fn sends_api_key_only_when_configured() {
    let key = fake_key();
    let server = MockServer::start();
    let authed = server.mock(|when, then| {
        when.method(POST)
            .path(PATH)
            .header("authorization", format!("ApiKey {key}"));
        then.status(200).json_body(ok_response(1024));
    });
    embedder(&server)
        .with_api_key(&key)
        .unwrap()
        .embed("user schema")
        .await
        .unwrap();
    authed.assert();

    let server = MockServer::start();
    let with_auth = server.mock(|when, then| {
        when.method(POST).path(PATH).matches(has_authorization);
        then.status(500).body("an Authorization header was sent");
    });
    let without = server.mock(|when, then| {
        when.method(POST).path(PATH);
        then.status(200).json_body(ok_response(1024));
    });
    embedder(&server).embed("user schema").await.unwrap();
    with_auth.assert_hits(0);
    without.assert();
}

/// A wrong width names both numbers.
#[tokio::test]
async fn a_wrong_width_names_both_numbers() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path(PATH);
        then.status(200).json_body(ok_response(384));
    });
    let err = embedder(&server).embed("anything").await.unwrap_err();
    assert!(matches!(err, EmbedError::Backend(_)), "{err:?}");
    let msg = err.to_string();
    assert!(msg.contains("returned 384 dims, expected 1024"), "{msg}");
}

/// Zero-norm and non-finite vectors are refused.
#[tokio::test]
async fn zero_norm_and_non_finite_vectors_are_refused() {
    for (body, want) in [
        (
            serde_json::json!({ "text_embedding": [{ "embedding": [0.0, 0.0, 0.0, 0.0] }] }),
            "zero-norm",
        ),
        // f32 overflow: 1e39 parses to infinity.
        (
            serde_json::json!({ "text_embedding": [{ "embedding": [1e39, 0.0, 0.0, 0.0] }] }),
            "non-finite",
        ),
        // Finite components whose squared sum overflows.
        (
            serde_json::json!({ "text_embedding": [{ "embedding": [3e38, 3e38, 0.0, 0.0] }] }),
            "non-finite",
        ),
    ] {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path(PATH);
            then.status(200).json_body(body.clone());
        });
        let err = ElasticEmbedder::new(&server.base_url(), ID, 4)
            .unwrap()
            .embed("anything")
            .await
            .unwrap_err();
        assert!(matches!(err, EmbedError::Backend(_)), "{err:?}");
        assert!(err.to_string().contains(want), "{body}: {err}");
    }
}

/// A byte or bit endpoint is refused with the reason, not as a parse error.
#[tokio::test]
async fn byte_and_bit_embeddings_are_refused_with_the_reason() {
    for field in ["text_embedding_bytes", "text_embedding_bits"] {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path(PATH);
            then.status(200)
                .json_body(serde_json::json!({ field: [{ "embedding": [1, -2, 3, 4] }] }));
        });
        let err = ElasticEmbedder::new(&server.base_url(), ID, 4)
            .unwrap()
            .embed("anything")
            .await
            .unwrap_err();
        assert!(matches!(err, EmbedError::Backend(_)), "{err:?}");
        let msg = err.to_string();
        assert!(
            msg.contains("byte or bit") && msg.contains("element_type is float"),
            "{msg}"
        );
    }
}

/// Exactly one entry: none, two, or no `text_embedding` at all is refused.
#[tokio::test]
async fn exactly_one_entry_is_accepted() {
    let one = serde_json::json!({ "embedding": [1.0, 0.0, 0.0, 0.0] });
    for (body, want) in [
        (
            serde_json::json!({ "text_embedding": [] }),
            "returned 0 text_embedding entries",
        ),
        (
            serde_json::json!({ "text_embedding": [one.clone(), one.clone()] }),
            "returned 2 text_embedding entries",
        ),
        (
            serde_json::json!({ "sparse_embedding": [] }),
            "without a text_embedding field",
        ),
    ] {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path(PATH);
            then.status(200).json_body(body.clone());
        });
        let err = ElasticEmbedder::new(&server.base_url(), ID, 4)
            .unwrap()
            .embed("anything")
            .await
            .unwrap_err();
        assert!(matches!(err, EmbedError::Backend(_)), "{err:?}");
        assert!(err.to_string().contains(want), "{body}: {err}");
    }
}

/// A 2xx body that does not parse is not quoted (an echoing endpoint could
/// put the key in it); the error names the parse failure's class and place.
#[tokio::test]
async fn an_unparseable_success_body_is_not_quoted() {
    let key = fake_key();
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path(PATH);
        then.status(200)
            .json_body(serde_json::json!({ "text_embedding": key.clone() }));
    });
    let err = embedder(&server)
        .with_api_key(&key)
        .unwrap()
        .embed("anything")
        .await
        .unwrap_err();
    assert!(matches!(err, EmbedError::Backend(_)), "{err:?}");
    let msg = err.to_string();
    assert!(
        msg.contains("unparseable JSON") && msg.contains("line 1"),
        "{msg}"
    );
    assert!(!msg.contains("xyzzy"), "{msg}");
}

/// The status rule: the J3 table, with `task_type` and `input_type` 400s
/// refined, and `input_type` only when it was sent.
#[test]
fn the_status_rule_refines_two_400s() {
    let rule = elastic_status_rule;
    let task = r#"{"error":{"type":"status_exception","reason":"Incompatible task_type, the requested type [text_embedding] does not match the model type [sparse_embedding]"},"status":400}"#;
    let input = r#"{"error":{"type":"validation_exception","reason":"Validation Failed: 1: Invalid input_type [SEARCH];"},"status":400}"#;
    let v = rule(400, task, true);
    assert_eq!(v.class, EmbedStatusClass::PermanentConfig);
    assert_eq!(v.hint, Some(TASK_TYPE_HINT));
    let v = rule(400, input, true);
    assert_eq!(v.class, EmbedStatusClass::PermanentConfig);
    assert_eq!(v.hint, Some(INPUT_TYPE_HINT));
    // Case and spelling: "Input Type" with a space still counts.
    assert_eq!(
        rule(
            400,
            r#"{"error":{"reason":"Input Type [SEARCH] is not supported"}}"#,
            true
        )
        .hint,
        Some(INPUT_TYPE_HINT)
    );
    // A root_cause reason counts as well as the top-level one.
    let root_only = r#"{"error":{"root_cause":[{"reason":"unrelated"},{"reason":"input_type is not allowed"}],"type":"x","reason":"failed"}}"#;
    assert_eq!(rule(400, root_only, true).hint, Some(INPUT_TYPE_HINT));
    // Not sent: the input_type hint would point at a setting already applied.
    assert_eq!(rule(400, input, false).class, EmbedStatusClass::Content);
    assert_eq!(rule(400, input, false).hint, None);
    // Any other 400 is a content refusal, any other status the J3 table.
    assert_eq!(
        rule(400, "too many tokens", true).class,
        EmbedStatusClass::Content
    );
    for code in [401u16, 403, 404, 301, 413, 408, 429, 500, 503, 418] {
        assert_eq!(
            rule(code, task, true).class,
            bge_m3::classify_status(code),
            "{code}"
        );
        assert_eq!(rule(code, input, true).hint, None, "{code}");
    }
}

/// The 400 refinements read only the reasons of the Elastic error JSON, so
/// text the body echoes elsewhere cannot pick a hint.
///
/// Mutation: match the whole body instead of `error_reasons` -> red.
#[test]
fn the_status_rule_reads_only_error_reasons() {
    let rule = elastic_status_rule;

    // A content 400 echoing input text that happens to say "input_type" (and
    // "task_type") outside error.reason: no hint, the J3 content class.
    let echo = r#"{"error":{"root_cause":[{"type":"illegal_argument_exception","reason":"input is too long"}],"type":"illegal_argument_exception","reason":"input is too long","input":["how is input_type set? Incompatible task_type"]},"status":400}"#;
    let v = rule(400, echo, true);
    assert_eq!(v.class, EmbedStatusClass::Content);
    assert_eq!(v.hint, None);

    // An input_type refusal that also mentions "task type" gets the
    // input_type hint, not the task_type one.
    let both_input = r#"{"error":{"reason":"input_type [SEARCH] is not supported for task type text_embedding on this service"},"status":400}"#;
    let v = rule(400, both_input, true);
    assert_eq!(v.class, EmbedStatusClass::PermanentConfig);
    assert_eq!(v.hint, Some(INPUT_TYPE_HINT));
    // An Incompatible task_type reason that also mentions input_type gets the
    // task_type hint: the wrong endpoint is checked first.
    let both_task = r#"{"error":{"reason":"Incompatible task_type, the requested type [text_embedding] does not match the model type [embedding]; input_type ignored"},"status":400}"#;
    assert_eq!(rule(400, both_task, true).hint, Some(TASK_TYPE_HINT));

    // The live Elastic Cloud Serverless body (2026-11-09), for an embedding
    // and a sparse_embedding endpoint, whether or not input_type was sent.
    for model_type in ["embedding", "sparse_embedding"] {
        let reason = format!(
            "Incompatible task_type, the requested type [text_embedding] does not match the \
             model type [{model_type}]"
        );
        let live = serde_json::json!({
            "error": {
                "root_cause": [{ "type": "status_exception", "reason": reason }],
                "type": "status_exception",
                "reason": reason,
            },
            "status": 400,
        })
        .to_string();
        for sent in [true, false] {
            let v = rule(400, &live, sent);
            assert_eq!(v.class, EmbedStatusClass::PermanentConfig, "{model_type}");
            assert_eq!(v.hint, Some(TASK_TYPE_HINT), "{model_type}");
        }
    }

    // A bare mention of task_type without "Incompatible" is not the task_type
    // refusal.
    assert_eq!(
        rule(400, r#"{"error":{"reason":"task_type is fine"}}"#, false).hint,
        None
    );

    // A non-JSON 400, or JSON without reasons, naming both words: no hint,
    // classed as the J3 table classes a 400.
    for body in [
        "Incompatible task_type; input_type [SEARCH] is not supported",
        r#"{"error":"Incompatible task_type; input_type refused"}"#,
        r#"{"error":{"reason":7,"root_cause":"input_type"}}"#,
        r#"{"message":"input_type refused"}"#,
    ] {
        let v = rule(400, body, true);
        assert_eq!(v.class, bge_m3::classify_status(400), "{body}");
        assert_eq!(v.hint, None, "{body}");
    }
}

/// Every status class lands on its variant, end to end, with the hints.
#[tokio::test]
async fn status_classes_drive_the_error_variant() {
    let err = error_for(400, "input is too long").await;
    assert!(matches!(err, EmbedError::Backend(_)), "{err:?}");
    assert!(err.to_string().contains("refused this content"), "{err}");

    let err = error_for(
        400,
        r#"{"error":{"reason":"input_type [INGEST] not supported"}}"#,
    )
    .await;
    assert!(matches!(err, EmbedError::Backend(_)), "{err:?}");
    let msg = err.to_string();
    assert!(
        msg.contains("send_input_type = false") && msg.contains("re-embedded"),
        "{msg}"
    );
    assert!(msg.contains("permanent configuration error"), "{msg}");

    let err = error_for(400, r#"{"error":{"reason":"Incompatible task_type"}}"#).await;
    assert!(matches!(err, EmbedError::Backend(_)), "{err:?}");
    let msg = err.to_string();
    assert!(
        msg.contains("sparse_embedding") && msg.contains("task type embedding"),
        "{msg}"
    );

    for code in [401u16, 403, 404] {
        let err = error_for(code, "no").await;
        assert!(matches!(err, EmbedError::Backend(_)), "{code}: {err:?}");
        assert!(
            err.to_string().contains("permanent configuration error"),
            "{err}"
        );
    }
    // 418 is a status the J3 table does not name (Unclassified, so
    // transient and logged); 425 is named transient.
    for code in [408u16, 418, 425, 429, 500, 503] {
        let err = error_for(code, "busy").await;
        assert!(matches!(err, EmbedError::Unavailable(_)), "{code}: {err:?}");
    }
    // The rest of the content class: a statement about this input.
    for code in [413u16, 415, 422] {
        let err = error_for(code, "no").await;
        assert!(matches!(err, EmbedError::Backend(_)), "{code}: {err:?}");
        assert!(err.to_string().contains("refused this content"), "{err}");
    }
}

/// A redirect is never followed, and its target is not quoted.
///
/// Mutation: build the client without `Policy::none()` -> red.
#[tokio::test]
async fn a_redirect_is_not_followed() {
    let key = fake_key();
    for code in [301u16, 302, 307, 308] {
        let server = MockServer::start();
        let target = server.mock(|when, then| {
            when.path(format!("/elsewhere{PATH}"));
            then.status(200).json_body(ok_response(1024));
        });
        server.mock(|when, then| {
            when.method(POST).path(PATH);
            then.status(code)
                .header("location", server.url(format!("/elsewhere{PATH}")))
                .body(format!("moved to {}", server.url("/elsewhere")));
        });
        let err = embedder(&server)
            .with_api_key(&key)
            .unwrap()
            .embed("anything")
            .await
            .unwrap_err();
        target.assert_hits(0);
        assert!(matches!(err, EmbedError::Backend(_)), "{code}: {err:?}");
        let msg = err.to_string();
        assert!(msg.contains(&code.to_string()), "{msg}");
        assert!(!msg.contains("elsewhere") && !msg.contains(&key), "{msg}");
    }
}

/// A transport failure is transient, and the message carries no userinfo.
#[tokio::test]
async fn a_transport_failure_is_unavailable() {
    let err = ElasticEmbedder::new("http://127.0.0.1:9/base", ID, 1024)
        .unwrap()
        .embed("anything")
        .await
        .unwrap_err();
    assert!(matches!(err, EmbedError::Unavailable(_)), "{err:?}");
    assert!(
        err.to_string()
            .contains("Elasticsearch unreachable at http://127.0.0.1:9/base/_inference"),
        "{err}"
    );
}

/// A 2xx whose body the connection cuts off mid-read is a transport failure,
/// so `Unavailable` and the write keeps its durable intent; the partial body
/// is not quoted.
///
/// Mutation: map `BodyReadError::Cut` to `Backend` -> red.
#[tokio::test]
async fn a_success_body_cut_off_mid_read_is_unavailable() {
    let partial = br#"{"text_embedding":[{"embedding":[0.123456,"#.to_vec();
    let head =
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 100000\r\n\r\n"
            .to_string();
    let base = crate::test_util::serve_raw_http_once(head, vec![partial]).await;
    let err = ElasticEmbedder::new(&base, ID, 4)
        .unwrap()
        .embed("anything")
        .await
        .unwrap_err();
    assert!(matches!(err, EmbedError::Unavailable(_)), "{err:?}");
    let msg = err.to_string();
    assert!(msg.contains("was cut off"), "{msg}");
    assert!(
        !msg.contains("0.123456"),
        "the partial body is not quoted: {msg}"
    );
}

/// The cap grows with the width and never drops under the floor.
#[test]
fn the_success_body_cap_has_a_floor_and_grows_with_dim() {
    use bge_m3::{success_body_cap, SUCCESS_BODY_FLOOR};
    assert_eq!(success_body_cap(4), SUCCESS_BODY_FLOOR);
    assert_eq!(success_body_cap(4096), SUCCESS_BODY_FLOOR);
    assert_eq!(success_body_cap(1 << 20), 64 << 20);
    assert_eq!(success_body_cap(usize::MAX), usize::MAX);
}

/// A 2xx body over the cap is refused as `Backend` without quoting it,
/// whether `Content-Length` announces it or it streams chunked.
///
/// Mutation: read with `resp.bytes()` again -> red (the padded body parses
/// and embeds).
#[tokio::test]
async fn an_oversized_success_body_is_refused_unquoted() {
    let key = fake_key();
    let cap = bge_m3::success_body_cap(4);
    // Valid JSON padded past the cap, ending in the key: a parse would
    // succeed and a quote would show it.
    let big = format!(
        "{{\"text_embedding\":[{{\"embedding\":[1,2,3,4]}}],\"pad\":\"{}{key}\"}}",
        "x".repeat(cap)
    );
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path(PATH);
        then.status(200)
            .header("content-type", "application/json")
            .body(big.clone());
    });
    let announced = ElasticEmbedder::new(&server.base_url(), ID, 4)
        .unwrap()
        .with_api_key(&key)
        .unwrap()
        .embed("anything")
        .await
        .unwrap_err();

    let head =
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\n\r\n"
            .to_string();
    let chunks = big
        .as_bytes()
        .chunks(64 * 1024)
        .map(|c| {
            let mut framed = format!("{:x}\r\n", c.len()).into_bytes();
            framed.extend_from_slice(c);
            framed.extend_from_slice(b"\r\n");
            framed
        })
        .chain(std::iter::once(b"0\r\n\r\n".to_vec()))
        .collect();
    let base = crate::test_util::serve_raw_http_once(head, chunks).await;
    let streamed = ElasticEmbedder::new(&base, ID, 4)
        .unwrap()
        .embed("anything")
        .await
        .unwrap_err();

    for err in [announced, streamed] {
        assert!(matches!(err, EmbedError::Backend(_)), "{err:?}");
        let msg = err.to_string();
        assert!(msg.contains(&format!("over {cap} bytes")), "{msg}");
        assert!(msg.contains("body not shown"), "{msg}");
        assert!(!msg.contains(&key) && !msg.contains("xxxx"), "{msg}");
    }
}

/// An error body echoing the key (raw, JSON-quoted, masked) never carries
/// it; the rest of the body is kept.
#[tokio::test]
async fn an_error_body_echoing_the_key_never_carries_it() {
    let key = fake_key();
    for code in [401u16, 400, 429, 418] {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path(PATH);
            then.status(code).body(format!(
                "invalid key: ApiKey {key} {} ({}...{})",
                serde_json::json!({ "key": key }),
                &key[..10],
                &key[key.len() - 10..]
            ));
        });
        let err = embedder(&server)
            .with_api_key(&key)
            .unwrap()
            .embed("anything")
            .await
            .unwrap_err()
            .to_string();
        assert!(!err.contains(&key), "{code}: {err}");
        assert!(!err.contains(&key[..10]), "{code}: {err}");
        assert!(!err.contains(&key[key.len() - 10..]), "{code}: {err}");
        assert!(err.contains("invalid key"), "{code}: {err}");
    }
}

/// `Debug` never shows the key; it shows that one is set.
#[test]
fn debug_never_shows_the_key() {
    let key = fake_key();
    let e = ElasticEmbedder::new("https://my-cluster.es.example.com", ID, 1024)
        .unwrap()
        .with_api_key(&key)
        .unwrap();
    let shown = format!("{e:?}");
    assert!(!shown.contains(&key), "{shown}");
    assert!(shown.contains("ApiKey (value not shown)"), "{shown}");
    let plain = ElasticEmbedder::new("https://my-cluster.es.example.com", ID, 1024).unwrap();
    assert!(format!("{plain:?}").contains("authorization: None"));
}

/// A key that is not a header value is refused without being quoted.
#[test]
fn a_key_that_is_not_a_header_value_is_refused() {
    let bad = format!("{}\nsecond-line", fake_key());
    let err = ElasticEmbedder::new("https://my-cluster.es.example.com", ID, 1024)
        .unwrap()
        .with_api_key(&bad)
        .unwrap_err();
    assert!(matches!(err, EmbedError::Unavailable(_)), "{err:?}");
    assert!(!err.to_string().contains("xyzzy"), "{err}");
}

/// The ApiKey goes only over https or http to loopback.
#[test]
fn the_key_is_sent_only_over_https_or_loopback_http() {
    let key = fake_key();
    for (url, host) in [
        (
            "http://my-cluster.es.example.com",
            "my-cluster.es.example.com",
        ),
        ("http://10.0.0.5:9200", "10.0.0.5"),
        ("http://localhost.example.com", "localhost.example.com"),
    ] {
        let err = ElasticEmbedder::new(url, ID, 1024)
            .unwrap()
            .with_api_key(&key)
            .expect_err(url);
        let msg = err.to_string();
        assert!(
            msg.contains(host) && msg.contains("ApiKey") && msg.contains("https"),
            "{msg}"
        );
        assert!(!msg.contains(&key), "{msg}");
    }
    for url in [
        "https://my-cluster.es.example.com",
        "http://127.0.0.1:9200",
        "http://localhost:9200",
        "http://[::1]:9200",
    ] {
        ElasticEmbedder::new(url, ID, 1024)
            .unwrap()
            .with_api_key(&key)
            .unwrap_or_else(|e| panic!("{url}: {e}"));
    }
}

/// `[recall]`'s URL rules; no refusal quotes the userinfo or query.
#[test]
fn url_rules_match_recall() {
    for (url, want) in [
        ("http://elastic:fake-xyzzy-pw@127.0.0.1:9200", "credentials"),
        ("http://fake-xyzzy-user@127.0.0.1:9200", "credentials"),
        (
            "https://my-cluster.es.example.com/?api_key=fake-xyzzy",
            "query string or fragment",
        ),
        (
            "https://my-cluster.es.example.com/#fake-xyzzy",
            "query string or fragment",
        ),
        ("ftp://my-cluster.es.example.com", "http:// or https://"),
        ("mailto:fake-xyzzy@example.com", "http:// or https://"),
        ("not a url", "not a valid URL"),
    ] {
        let err = ElasticEmbedder::new(url, ID, 1024).unwrap_err();
        assert!(matches!(err, EmbedError::Unavailable(_)), "{err:?}");
        let msg = err.to_string();
        assert!(msg.contains(want), "{url}: {msg}");
        assert!(!msg.contains("xyzzy"), "{url}: {msg}");
    }
}

/// The inference id rules; a refused id is not quoted.
#[test]
fn inference_id_rules() {
    for id in ["", "   ", "a b", "fake-xyzzy/x", ".", "..", "x\ty"] {
        let err = ElasticEmbedder::new("https://my-cluster.es.example.com", id, 1024).unwrap_err();
        assert!(matches!(err, EmbedError::Unavailable(_)), "{id:?}: {err:?}");
        assert!(!err.to_string().contains("xyzzy"), "{err}");
    }
    // Surrounding whitespace is trimmed.
    let e = ElasticEmbedder::new("https://my-cluster.es.example.com", "  e5  ", 1024).unwrap();
    assert_eq!(e.model_identity(), "e5");
    assert!(ElasticEmbedder::new("https://my-cluster.es.example.com", ID, 0).is_err());
}

/// CON-7: empty or whitespace text is refused before any request.
#[tokio::test]
async fn empty_text_is_refused_before_the_network() {
    let server = MockServer::start();
    let any = server.mock(|when, then| {
        when.any_request();
        then.status(404);
    });
    let e = embedder(&server);
    for text in ["", "   ", "\t\n"] {
        for err in [
            e.embed(text).await.unwrap_err(),
            e.embed_query(text).await.unwrap_err(),
        ] {
            assert!(
                matches!(err, EmbedError::Unavailable(_)),
                "{text:?}: {err:?}"
            );
        }
    }
    any.assert_hits(0);
}

/// Live check against a real Elasticsearch inference endpoint (#233, the
/// #68 conventions): ignored in ordinary runs, and an explicitly requested
/// run without the URL or id fails rather than skipping. The key, when the
/// cluster needs one, is read from `LAMBO_ES_API_KEY` through `api_key_env`
/// exactly as a deployment would read it.
///
/// ```sh
/// LAMBO_ELASTIC_EMBED_URL=https://my-cluster.es.example.com \
///   LAMBO_ELASTIC_INFERENCE_ID=.jina-embeddings-v3 LAMBO_ELASTIC_EMBED_DIM=1024 \
///   cargo test --features embed-elastic --lib embed::elastic::tests::live -- --ignored --nocapture
/// ```
///
/// with the API key exported in `LAMBO_ES_API_KEY` (omit it for a local,
/// unsecured loopback cluster).
#[tokio::test]
#[ignore]
async fn live_elastic_inference_endpoint() {
    let var = |k: &str| std::env::var(k).ok().filter(|s| !s.trim().is_empty());
    let url = var("LAMBO_ELASTIC_EMBED_URL")
        .expect("live test explicitly requested: set LAMBO_ELASTIC_EMBED_URL to the cluster URL");
    let id = var("LAMBO_ELASTIC_INFERENCE_ID").expect(
        "live test explicitly requested: set LAMBO_ELASTIC_INFERENCE_ID to a text_embedding \
         inference endpoint id",
    );
    let dim = var("LAMBO_ELASTIC_EMBED_DIM").map_or(1024, |d| {
        d.parse().expect("LAMBO_ELASTIC_EMBED_DIM must be a width")
    });
    let cfg = EmbedderConfig {
        kind: crate::embed::EmbedderKind::Elastic,
        dim,
        llama_url: Some(url),
        llama_model: Some(id),
        api_key_env: var("LAMBO_ES_API_KEY").map(|_| "LAMBO_ES_API_KEY".to_string()),
        ..Default::default()
    };
    let e = build(&cfg).unwrap_or_else(|e| panic!("build: {e}"));
    let doc = e
        .embed("the billing service owns the invoices table")
        .await
        .unwrap_or_else(|e| panic!("document role: {e}"));
    let query = e
        .embed_query("who owns invoices")
        .await
        .unwrap_or_else(|e| panic!("query role: {e}"));
    for (role, v) in [("document", &doc), ("query", &query)] {
        assert_eq!(v.len(), dim, "{role}");
        let n = norm(v);
        assert!((n - 1.0).abs() < 1e-4, "{role}: L2 norm {n} should be ~1");
    }
    eprintln!(
        "elastic live: {} dim={dim} doc.query cosine={:.4}",
        e.model_identity(),
        crate::embed::cosine(&doc, &query)
    );
}
