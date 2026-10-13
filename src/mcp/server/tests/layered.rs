//! #33 PR 3: a layered session's `lambo_recall`, on the server alone (the
//! serve's registry is replaced by a test [`LayerSource`]; the real one is
//! driven end to end in `mcp::serve`'s registry tests).

use std::sync::Weak;

use super::*;
use crate::mcp::server::layers::{
    LayerDown, LayerHandle, LayerRead, LayerSource, LiveLayer, ServerLayers,
};
use crate::mcp::server::trace::{with_caller, CallAuth};
use crate::recall::layered::{DEFAULT_LAYER_MIN_SIMILARITY, DEFAULT_LAYER_WEIGHT};
use crate::surface::session::{
    parse_addressed, HostedSessions, SessionAuthority, SessionCapabilities, SessionGrant,
    SessionScope,
};

pub(super) const PROJECT_FACT: &str =
    "worktree cleanup removes the target directory of a merged branch";
pub(super) const GENERAL_RULING: &str =
    "worktree branches are always rebased onto origin main first";

/// A layer source holding one session, which a test can take away (as a
/// detach does) or put back.
pub(super) struct Fixed(pub(super) parking_lot::Mutex<Option<Arc<Memory>>>);

impl LayerSource for Fixed {
    fn live_layer(&self, session: &str, _read: LayerRead) -> Result<LiveLayer, LayerDown> {
        let mem = self.0.lock().clone().ok_or(LayerDown::Transient)?;
        if mem.session().as_str() != session {
            return Err(LayerDown::Transient);
        }
        Ok(LiveLayer {
            mem,
            _in_flight: None,
        })
    }
}

/// `own` layered with `layer` through a [`Fixed`] source, which is returned
/// so the test owns it: the server holds it weakly, as it holds a registry.
pub(super) fn layered(own: &LamboServer, layer: &LamboServer) -> (LamboServer, Arc<Fixed>) {
    let source = Arc::new(Fixed(parking_lot::Mutex::new(Some(Arc::clone(
        layer.memory(),
    )))));
    let weak: Weak<Fixed> = Arc::downgrade(&source);
    let weak: Weak<dyn LayerSource> = weak;
    let server = own.clone().with_layers(ServerLayers {
        handles: vec![LayerHandle::new(
            layer.memory().session().as_str(),
            DEFAULT_LAYER_WEIGHT,
            DEFAULT_LAYER_MIN_SIMILARITY,
            weak,
        )],
        authority: None,
    });
    (server, source)
}

/// A project session and a `general` session, each holding one fact about
/// worktrees, and the project server layered with `general`.
pub(super) async fn project_and_general(tag: &str) -> (LamboServer, LamboServer, Arc<Fixed>) {
    let own = server(&format!("{tag}-proj")).await;
    let general = server("general").await;
    derive(&own, PROJECT_FACT).await;
    derive(&general, GENERAL_RULING).await;
    let (layered, source) = layered(&own, &general);
    (layered, general, source)
}

pub(super) async fn derive(s: &LamboServer, content: &str) {
    call(
        s,
        "lambo_derive",
        json!({"agent_id": "agent-a", "concepts": [{"content": content, "concept_type": "logic"}]}),
    )
    .await;
}

async fn recall(s: &LamboServer, args: serde_json::Value) -> CallToolResult {
    let mut args = args;
    args["agent_id"] = json!("agent-a");
    call(s, "lambo_recall", args).await
}

pub(super) fn structured(out: &CallToolResult) -> &serde_json::Value {
    out.structured_content.as_ref().expect("structured content")
}

fn hit_sessions(out: &CallToolResult) -> Vec<String> {
    structured(out)["hits"]
        .as_array()
        .expect("hits")
        .iter()
        .map(|h| {
            h["session"]
                .as_str()
                .expect("every hit names its session")
                .to_string()
        })
        .collect()
}

#[tokio::test]
async fn a_layered_recall_merges_both_sessions_and_names_every_hits_session() {
    let (s, general, _source) = project_and_general("merge").await;
    let out = recall(&s, json!({"query": "worktree branches"})).await;
    assert_eq!(out.is_error, Some(false), "{out:?}");
    let sessions = hit_sessions(&out);
    assert!(sessions.contains(&"merge-proj".to_string()), "{sessions:?}");
    assert!(sessions.contains(&"general".to_string()), "{sessions:?}");
    let context = structured(&out)["context"].as_str().expect("context");
    assert!(context.contains(GENERAL_RULING), "{context}");
    assert!(context.contains(PROJECT_FACT), "{context}");
    // The merge renders a layer hit's session in its block; the wire takes
    // the merged context verbatim, as content[0] too.
    assert!(context.contains(", from general)"), "{context}");
    match &out.content[0] {
        ContentBlock::Text(t) => assert_eq!(t.text, context),
        other => panic!("expected text, got {other:?}"),
    }
    s.mem.close().await.expect("close");
    general.mem.close().await.expect("close");
}

/// An unlayered session's answer has 0.5.0's shape exactly: no `session`
/// on any hit, the same three structured keys and six hit keys, and
/// `get_info` unchanged; a layered session's `get_info` only appends.
#[tokio::test]
async fn an_unlayered_session_keeps_the_0_5_0_shape() {
    let plain = server("plain-proj").await;
    derive(&plain, PROJECT_FACT).await;
    let out = recall(&plain, json!({"query": "worktree"})).await;
    let payload = structured(&out);
    let mut keys: Vec<&str> = payload
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort();
    assert_eq!(keys, ["context", "hits", "warnings"]);
    let hits = payload["hits"].as_array().expect("hits");
    assert!(!hits.is_empty());
    for hit in hits {
        let mut keys: Vec<&str> = hit
            .as_object()
            .expect("hit")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "blast_radius",
                "concept_type",
                "content",
                "is_canonical",
                "node_id",
                "score"
            ],
            "{hit}"
        );
    }
    // A scope naming the session itself is the same plain recall.
    let scoped = recall(&plain, json!({"query": "worktree", "scope": "plain-proj"})).await;
    assert_eq!(scoped.is_error, Some(false), "{scoped:?}");
    assert!(structured(&scoped)["hits"][0].get("session").is_none());

    let plain_info = plain.get_info().instructions.expect("instructions");
    assert!(!plain_info.contains("layer"), "{plain_info}");
    let general = server("general").await;
    let (layered, _source) = layered(&plain, &general);
    let layered_info = layered.get_info().instructions.expect("instructions");
    assert!(layered_info.starts_with(&plain_info), "only appended");
    assert!(
        layered_info.contains("also searches its shared layers: general"),
        "{layered_info}"
    );
    assert!(
        layered_info.contains("scope = \"general\""),
        "{layered_info}"
    );
    plain.mem.close().await.expect("close");
    general.mem.close().await.expect("close");
}

#[tokio::test]
async fn scope_reads_one_session_and_names_only_this_sessions_scopes() {
    let (s, general, _source) = project_and_general("scope").await;
    let out = recall(&s, json!({"query": "worktree", "scope": "general"})).await;
    assert_eq!(out.is_error, Some(false), "{out:?}");
    let sessions = hit_sessions(&out);
    assert!(!sessions.is_empty());
    assert!(sessions.iter().all(|s| s == "general"), "{sessions:?}");
    // Read alone, a layer's blocks carry no `from` meta: it is that
    // session's own plain answer.
    assert!(!structured(&out)["context"]
        .as_str()
        .unwrap()
        .contains("from general"));

    let out = recall(&s, json!({"query": "worktree", "scope": "scope-proj"})).await;
    let sessions = hit_sessions(&out);
    assert!(!sessions.is_empty());
    assert!(sessions.iter().all(|s| s == "scope-proj"), "{sessions:?}");

    let out = recall(&s, json!({"query": "worktree", "scope": "someone-elses"})).await;
    assert_eq!(out.is_error, Some(true));
    let text = text_of(&out);
    assert!(
        text.contains("scope must name this session or one of its layers: scope-proj, general"),
        "{text}"
    );
    assert!(
        !text.contains("someone-elses"),
        "the value sent is not quoted"
    );

    let long = "x".repeat(16_385);
    let out = recall(&s, json!({"query": "worktree", "scope": long})).await;
    assert_eq!(out.is_error, Some(true), "an oversized scope is refused");
    s.mem.close().await.expect("close");
    general.mem.close().await.expect("close");
}

/// A layer that is not serving (its lease lost, detaching) leaves the
/// stack's answer to this session with one `layer_unavailable` line, and
/// its scope is a retryable refusal.
#[tokio::test]
async fn a_layer_not_serving_degrades_the_stack_and_refuses_its_scope() {
    let (s, general, source) = project_and_general("down").await;
    let taken = source.0.lock().take();
    let out = recall(&s, json!({"query": "worktree"})).await;
    assert_eq!(out.is_error, Some(false), "a layer never fails the primary");
    let sessions = hit_sessions(&out);
    assert!(!sessions.is_empty());
    assert!(sessions.iter().all(|s| s == "down-proj"), "{sessions:?}");
    let warnings = structured(&out)["warnings"].to_string();
    assert!(
        warnings.contains("layer general is unavailable; results are from down-proj only"),
        "{warnings}"
    );

    let out = recall(&s, json!({"query": "worktree", "scope": "general"})).await;
    assert_eq!(out.is_error, Some(true));
    assert!(
        text_of(&out).contains("layer general is unavailable right now; retry shortly"),
        "{}",
        text_of(&out)
    );

    // Back again (re-elected): the next call finds it.
    *source.0.lock() = taken;
    let out = recall(&s, json!({"query": "worktree"})).await;
    assert!(hit_sessions(&out).contains(&"general".to_string()));
    s.mem.close().await.expect("close");
    general.mem.close().await.expect("close");
}

/// The handle holds its source weakly: dropping the source (a registry
/// gone) frees it although the server is alive (no cycle), and the server
/// degrades instead of panicking.
#[tokio::test]
async fn a_dropped_source_is_freed_and_the_layer_is_unavailable() {
    let (s, general, source) = project_and_general("weak").await;
    let weak = Arc::downgrade(&source);
    drop(source);
    assert_eq!(
        weak.strong_count(),
        0,
        "the server kept no strong reference"
    );
    assert!(s.layers().handles[0].live(LayerRead::Merged).is_err());
    let out = recall(&s, json!({"query": "worktree"})).await;
    assert_eq!(out.is_error, Some(false));
    assert!(structured(&out)["warnings"]
        .to_string()
        .contains("layer general is unavailable"));
    s.mem.close().await.expect("close");
    general.mem.close().await.expect("close");
}

/// Defence in depth: a grant that reaches the session but not its layer
/// (which the serve's start check refuses) gets a generic refusal, which
/// names no session; a grant that reaches both is served; a call with no
/// grant (the J2 endpoint) is served.
#[tokio::test]
async fn a_grant_refused_its_layer_gets_a_generic_refusal() {
    let (s, general, _source) = project_and_general("authz").await;
    let hosted = HostedSessions::new(
        ["authz-proj", "general"].map(|n| parse_addressed(n).unwrap()),
        [],
    );
    let narrow = SessionGrant::new(
        "narrow",
        SessionScope::new([parse_addressed("authz-proj").unwrap()], false, None),
        SessionCapabilities::default(),
    );
    let wide = SessionGrant::new(
        "wide",
        SessionScope::new([], true, None),
        SessionCapabilities::default(),
    );
    let authority = Arc::new(SessionAuthority::with_credentials(
        [
            (
                crate::mcp::SecretToken::new(["fake", "narrow", "pr3"].join("-")).unwrap(),
                narrow.clone(),
            ),
            (
                crate::mcp::SecretToken::new(["fake", "wide", "pr3"].join("-")).unwrap(),
                wide.clone(),
            ),
        ],
        hosted,
    ));
    let s = s.clone().with_layers(ServerLayers {
        authority: Some(authority),
        ..s.layers().clone()
    });
    let params = |scope: Option<&str>| super::super::params::RecallParams {
        agent_id: "agent-a".into(),
        query: "worktree".into(),
        scope: scope.map(str::to_string),
        ..Default::default()
    };
    for scope in [None, Some("general")] {
        let out = with_caller(
            None,
            Some(CallAuth::Http(Some(Arc::new(narrow.clone())))),
            s.recall_impl(params(scope)),
        )
        .await;
        assert_eq!(out.is_error, Some(true), "{scope:?}");
        let text = text_of(&out);
        assert!(text.contains("cannot read every session"), "{text}");
        assert!(!text.contains("general"), "names no session: {text}");
    }
    let out = with_caller(
        None,
        Some(CallAuth::Http(Some(Arc::new(wide)))),
        s.recall_impl(params(None)),
    )
    .await;
    assert_eq!(out.is_error, Some(false), "{out:?}");
    // Review L2: no request parts at all (the J2 endpoint) is allowed;
    // HTTP parts without a grant are refused, for the stack and a scope.
    let out = with_caller(None, None, s.recall_impl(params(Some("general")))).await;
    assert_eq!(out.is_error, Some(false), "no parts: the J2 endpoint");
    for scope in [None, Some("general")] {
        let out = with_caller(
            None,
            Some(CallAuth::Http(None)),
            s.recall_impl(params(scope)),
        )
        .await;
        assert_eq!(out.is_error, Some(true), "parts without a grant: {scope:?}");
        assert!(text_of(&out).contains("cannot read every session"));
    }
    s.mem.close().await.expect("close");
    general.mem.close().await.expect("close");
}

/// The ledger's recall line for a layered session: every hit names its
/// session, `session_hits` counts them per session, a layer hit carries
/// its own legs (from `hit_legs` by position), and `scope` is recorded
/// when sent. An unlayered session's line has none of the three.
#[tokio::test]
async fn the_ledger_line_names_each_hits_session_and_counts_them() {
    let dir = ledger_dir("layered");
    let path = dir.join("calls.jsonl");
    let own = server_with_ledger("ledger-proj", &path).await;
    let ledger = Arc::clone(own.ledger().expect("ledger"));
    let general = server("general").await;
    derive(&own, PROJECT_FACT).await;
    derive(&general, GENERAL_RULING).await;
    let (s, _source) = layered(&own, &general);
    recall(&s, json!({"query": "worktree branches"})).await;
    recall(&s, json!({"query": "worktree", "scope": "general"})).await;
    recall(&own, json!({"query": "worktree"})).await;
    let lines = read_ledger(&ledger, 4);
    let layered_line = &lines[1];
    assert_eq!(layered_line["tool"], json!("lambo_recall"));
    let hits = layered_line["hits"].as_array().expect("hits");
    let mut counted = std::collections::BTreeMap::<String, u64>::new();
    for hit in hits {
        *counted
            .entry(hit["session"].as_str().expect("session").to_string())
            .or_default() += 1;
    }
    assert_eq!(layered_line["session_hits"], json!(counted));
    assert!(counted.contains_key("general") && counted.contains_key("ledger-proj"));
    let general_hit = hits
        .iter()
        .find(|h| h["session"] == json!("general"))
        .expect("a general hit");
    assert!(
        general_hit["legs"]["bm25"].is_number(),
        "a layer hit's legs come from hit_legs: {general_hit}"
    );
    assert!(layered_line.get("scope").is_none());

    let scoped = &lines[2];
    assert_eq!(scoped["scope"], json!("general"));
    assert_eq!(
        scoped["session_hits"]["general"],
        json!(scoped["hit_count"])
    );

    let plain = &lines[3];
    assert!(plain.get("session_hits").is_none(), "{plain}");
    assert!(plain.get("scope").is_none(), "{plain}");
    assert!(plain["hits"][0].get("session").is_none(), "{plain}");
    ledger.shutdown();
    own.mem.close().await.expect("close");
    general.mem.close().await.expect("close");
}

/// A recall by image reads one session (the library's layered recall is
/// by text), and a layered session says its layers were not searched.
#[tokio::test]
async fn a_recall_by_image_on_a_layered_session_says_the_layers_were_not_searched() {
    let own = image_server("image-proj", Config::default()).await;
    let general = image_server("general", Config::default()).await;
    derive(&own, PROJECT_FACT).await;
    let (s, _source) = layered(&own, &general);
    let out = recall(
        &s,
        json!({"image": {"mime": "image/png", "data": png_b64(PROJECT_FACT)}}),
    )
    .await;
    assert_eq!(out.is_error, Some(false), "{out:?}");
    assert!(structured(&out)["warnings"]
        .to_string()
        .contains("its layers (general) were not searched"));
    assert!(hit_sessions(&out).iter().all(|s| s == "image-proj"));
    own.mem.close().await.expect("close");
    general.mem.close().await.expect("close");
}

/// Byte identity for an unlayered session against the pre-#33 body
/// (main 92c0f4af, v0.5.0's plus #130's `truncated_in_context`),
/// reproduced here as the reference, at a budget that fits and one that
/// truncates. The frozen v0.5.0 golden is
/// `an_unlayered_recall_matches_the_frozen_v0_5_0_projection`. The second recall of the
/// same query is served from the session's recall cache, so both read one
/// answer.
#[tokio::test]
async fn an_unlayered_recall_is_byte_identical_to_the_0_5_0_body() {
    let plain = server("bytes-proj").await;
    derive(&plain, PROJECT_FACT).await;
    derive(&plain, GENERAL_RULING).await;
    for max_tokens in [500, 20] {
        let query = crate::types::RecallQuery {
            query: "worktree branches".into(),
            top_k: 5,
            max_tokens,
            traversal_depth: 2,
        };
        let detailed = plain.memory().recall_detailed(query).await.expect("recall");
        // The 0.5.0 body, verbatim in substance.
        let first_truncated = detailed.truncated_hit().is_some();
        let result: crate::types::RecallResult = detailed.into();
        let warnings: Vec<String> = result.warnings.iter().map(|w| redact_urls(w)).collect();
        let hits: Vec<_> = result
            .hits
            .iter()
            .enumerate()
            .map(|(i, h)| {
                let mut hit = json!({
                    "node_id": h.node_id.0.to_string(),
                    "content": h.content,
                    "concept_type": h.concept_type,
                    "score": h.score,
                    "is_canonical": h.is_canonical,
                    "blast_radius": h.blast_radius,
                });
                if i == 0
                    && first_truncated
                    && let Some(map) = hit.as_object_mut()
                {
                    map.insert("truncated_in_context".into(), json!(true));
                }
                hit
            })
            .collect();
        let expected = json!({"context": result.context, "hits": hits, "warnings": warnings});

        let out = plain
            .recall_impl(super::super::params::RecallParams {
                agent_id: "agent-a".into(),
                query: "worktree branches".into(),
                top_k: Some(5),
                max_tokens: Some(max_tokens),
                traversal_depth: Some(2),
                ..Default::default()
            })
            .await;
        assert_eq!(
            serde_json::to_string(structured(&out)).unwrap(),
            serde_json::to_string(&expected).unwrap(),
            "max_tokens {max_tokens}"
        );
        match &out.content[0] {
            ContentBlock::Text(t) => assert_eq!(t.text, result.context),
            other => panic!("expected text, got {other:?}"),
        }
    }
    plain.mem.close().await.expect("close");
}

/// A source whose layer is down for a fixed reason.
struct Down(LayerDown);

impl LayerSource for Down {
    fn live_layer(&self, _session: &str, _read: LayerRead) -> Result<LiveLayer, LayerDown> {
        Err(self.0)
    }
}

/// Review L1: a `scope` naming a failed or erased layer is refused as
/// permanent, not "retry shortly"; one over its rate is retryable.
#[tokio::test]
async fn a_scoped_read_of_a_layer_that_will_not_come_back_says_so() {
    let own = server("why-proj").await;
    for (down, says, not) in [
        (
            LayerDown::Failed,
            "is not retried until the serve restarts",
            "retry shortly",
        ),
        (
            LayerDown::Erased,
            "was erased and will not come back",
            "retry shortly",
        ),
        (
            LayerDown::RateLimited,
            "is over its request rate; retry shortly",
            "operator",
        ),
        (
            LayerDown::Transient,
            "is unavailable right now; retry shortly",
            "operator",
        ),
    ] {
        let source: Arc<dyn LayerSource> = Arc::new(Down(down));
        let s = own.clone().with_layers(ServerLayers {
            handles: vec![LayerHandle::new(
                "general",
                DEFAULT_LAYER_WEIGHT,
                DEFAULT_LAYER_MIN_SIMILARITY,
                Arc::downgrade(&source),
            )],
            authority: None,
        });
        let out = recall(&s, json!({"query": "worktree", "scope": "general"})).await;
        assert_eq!(out.is_error, Some(true), "{down:?}");
        let text = text_of(&out);
        assert!(text.contains(says), "{down:?}: {text}");
        assert!(!text.contains(not), "{down:?}: {text}");
        assert!(
            text.contains("without scope for results from why-proj"),
            "{text}"
        );
    }
    own.mem.close().await.expect("close");
}

/// Review M1: a layer hit's id belongs to the layer, so `lambo_inspect`
/// takes the same `scope`. A `scope = <layer>` recall says so in one
/// warning line; the id it returns is inspected with that scope and is
/// not found without it. `scope` on inspect is resolved as on recall.
#[tokio::test]
async fn a_layer_hit_is_inspected_with_the_same_scope() {
    let (s, general, _source) = project_and_general("insp").await;
    let out = recall(&s, json!({"query": "worktree", "scope": "general"})).await;
    let warnings = structured(&out)["warnings"].as_array().expect("warnings");
    assert!(
        warnings.contains(&json!(
            "recall: these hits are from layer general, and their ids belong to it; pass \
             scope = \"general\" to lambo_inspect to inspect one"
        )),
        "{warnings:?}"
    );
    // No such line on a merged recall or a recall of this session.
    for args in [
        json!({"query": "worktree"}),
        json!({"query": "worktree", "scope": "insp-proj"}),
    ] {
        let out = recall(&s, args).await;
        assert!(!structured(&out)["warnings"]
            .to_string()
            .contains("to lambo_inspect"));
    }
    let id = structured(&out)["hits"][0]["node_id"]
        .as_str()
        .expect("node id")[..8]
        .to_string();

    let inspect = |args: serde_json::Value| {
        let mut args = args;
        args["agent_id"] = json!("agent-a");
        call(&s, "lambo_inspect", args)
    };
    let found = inspect(json!({"focus": id, "scope": "general"})).await;
    assert_eq!(found.is_error, Some(false), "{found:?}");
    assert!(
        text_of(&found).contains(GENERAL_RULING),
        "{}",
        text_of(&found)
    );
    // #36 PR 3: the canonization explanation is the layer's concept's,
    // read from the layer's graph and store, not this session's.
    assert!(
        structured(&found)["admission"]["status"].is_string(),
        "{}",
        structured(&found)
    );
    assert!(
        text_of(&found).contains("canonization ("),
        "{}",
        text_of(&found)
    );
    let missing = inspect(json!({"focus": id})).await;
    assert_eq!(missing.is_error, Some(true), "the id is not this session's");
    let own = inspect(json!({"focus": PROJECT_FACT, "scope": "insp-proj"})).await;
    assert_eq!(own.is_error, Some(false), "{own:?}");
    let refused = inspect(json!({"focus": id, "scope": "elsewhere"})).await;
    assert_eq!(refused.is_error, Some(true));
    assert!(
        text_of(&refused)
            .contains("scope must name this session or one of its layers: insp-proj, general"),
        "{}",
        text_of(&refused)
    );

    let info = s.get_info().instructions.expect("instructions");
    assert!(
        info.contains("pass the same scope (scope = \"general\") to lambo_inspect"),
        "{info}"
    );
    s.mem.close().await.expect("close");
    general.mem.close().await.expect("close");
}

/// Review M1: an unlayered session's inspect accepts its own name as
/// `scope` and nothing else; a layer that is down refuses its scope.
#[tokio::test]
async fn inspect_scope_on_an_unlayered_session_and_a_layer_that_is_down() {
    let plain = server("insp-plain").await;
    derive(&plain, PROJECT_FACT).await;
    let ok = call(
        &plain,
        "lambo_inspect",
        json!({"agent_id": "agent-a", "focus": PROJECT_FACT, "scope": "insp-plain"}),
    )
    .await;
    assert_eq!(ok.is_error, Some(false), "{ok:?}");
    let refused = call(
        &plain,
        "lambo_inspect",
        json!({"agent_id": "agent-a", "focus": PROJECT_FACT, "scope": "general"}),
    )
    .await;
    assert!(text_of(&refused).contains("one of its layers: insp-plain"));

    let source: Arc<dyn LayerSource> = Arc::new(Down(LayerDown::Transient));
    let s = plain.clone().with_layers(ServerLayers {
        handles: vec![LayerHandle::new(
            "general",
            DEFAULT_LAYER_WEIGHT,
            DEFAULT_LAYER_MIN_SIMILARITY,
            Arc::downgrade(&source),
        )],
        authority: None,
    });
    let down = call(
        &s,
        "lambo_inspect",
        json!({"agent_id": "agent-a", "focus": "x", "scope": "general"}),
    )
    .await;
    assert!(
        text_of(&down).contains(
            "lambo_inspect: layer general is unavailable right now; retry shortly, or inspect \
             without scope to read insp-plain"
        ),
        "{}",
        text_of(&down)
    );
    plain.mem.close().await.expect("close");
}

/// Review L4: an unlayered recall answers exactly as v0.5.0 did, checked
/// against the tag's projection frozen verbatim in
/// `fixtures/recall_v0_5_0.rs`: the whole result (`content[0]`, the
/// `content[1]` warnings block, `structuredContent`, `isError`) for a
/// recall with a non-empty warning (another agent's reservation) whose
/// blocks fit the budget. The second recall of the query is served from
/// the session's recall cache, so both read one answer. `tools/list` is
/// not covered: `scope` is new in every tool schema but lambo_stats's.
#[tokio::test]
async fn an_unlayered_recall_matches_the_frozen_v0_5_0_projection() {
    let plain = server("frozen-proj").await;
    let ids = derive_created(
        &plain,
        "agent-a",
        json!([{"content": PROJECT_FACT, "concept_type": "logic"}]),
    )
    .await;
    derive(&plain, GENERAL_RULING).await;
    let reserved = call(
        &plain,
        "lambo_reserve",
        json!({"agent_id": "agent-b", "node_id": ids[0]}),
    )
    .await;
    assert_eq!(reserved.is_error, Some(false), "{reserved:?}");
    let query = crate::types::RecallQuery {
        query: "worktree branches".into(),
        top_k: 5,
        max_tokens: 2_000,
        traversal_depth: 2,
    };
    let detailed = plain.memory().recall_detailed(query).await.expect("recall");
    assert!(detailed.truncated_hit().is_none(), "v0.5.0 predates #130");
    let frozen = super::recall_v0_5_0::v0_5_0_recall_result(detailed);
    let out = plain
        .recall_impl(super::super::params::RecallParams {
            agent_id: "agent-a".into(),
            query: "worktree branches".into(),
            top_k: Some(5),
            max_tokens: Some(2_000),
            traversal_depth: Some(2),
            ..Default::default()
        })
        .await;
    let warnings = structured(&out)["warnings"].as_array().expect("warnings");
    assert!(
        !warnings.is_empty(),
        "a warning is part of what is compared"
    );
    assert_eq!(out.content.len(), 2, "content[1] carries the warnings");
    assert_eq!(
        serde_json::to_string(&out).unwrap(),
        serde_json::to_string(&frozen).unwrap()
    );
    plain.mem.close().await.expect("close");
}

/// #36 PR 4 and design-33 section 7: a write resolves its references
/// against its own session. One that names a concept only in a layer is
/// refused by name, and nothing is written in either session.
#[tokio::test]
async fn a_citation_of_a_layer_concept_is_refused_as_cross_session() {
    let (s, general, _source) = project_and_general("cite").await;
    let ruling = general
        .memory()
        .graph()
        .read()
        .concepts()
        .find(|c| c.content == GENERAL_RULING)
        .expect("general holds the ruling")
        .id;
    let before = (
        s.memory().graph().read().snapshot(),
        general.memory().graph().read().snapshot(),
    );
    for reference in [
        crate::recall::format::short_id(ruling),
        ruling.0.to_string(),
        GENERAL_RULING.to_string(),
    ] {
        let out = call_raw(
            &s,
            "lambo_derive",
            json!({"agent_id": "agent-a", "concepts": [{
                "content": "a project decision", "concept_type": "logic",
                "depends_on": [reference],
            }]}),
        )
        .await;
        assert_eq!(out.is_error, Some(true));
        let text = text_of(&out);
        assert!(
            text.contains("is in scope \"general\"; cross-session citations are not recorded"),
            "{text}"
        );
    }
    let out = call_raw(
        &s,
        "lambo_record_action",
        json!({"agent_id": "agent-a", "action": "ran it", "depends_on": [ruling.0.to_string()]}),
    )
    .await;
    assert_eq!(out.is_error, Some(true));
    assert!(
        text_of(&out).contains("is in scope \"general\""),
        "{}",
        text_of(&out)
    );
    assert_eq!(
        before,
        (
            s.memory().graph().read().snapshot(),
            general.memory().graph().read().snapshot(),
        )
    );
    s.mem.close().await.expect("close");
    general.mem.close().await.expect("close general");
}

/// #19(b), the approved layered rule: a contradiction of a layer's concept
/// from the project session is refused as cross-session, by short id, full
/// id and content, on a derive and on an action, and nothing is written in
/// either session. The message says how to challenge it: deliberately, in
/// that scope.
#[tokio::test]
async fn a_contradiction_of_a_layer_concept_is_refused_as_cross_session() {
    let (s, general, _source) = project_and_general("against").await;
    let ruling = general
        .memory()
        .graph()
        .read()
        .concepts()
        .find(|c| c.content == GENERAL_RULING)
        .expect("general holds the ruling")
        .id;
    let before = (
        s.memory().graph().read().snapshot(),
        general.memory().graph().read().snapshot(),
    );
    for reference in [
        crate::recall::format::short_id(ruling),
        ruling.0.to_string(),
        GENERAL_RULING.to_string(),
    ] {
        for (tool, args) in [
            (
                "lambo_derive",
                json!({"agent_id": "agent-a", "concepts": [{
                    "content": "a project finding", "concept_type": "observation",
                    "against": [reference.clone()],
                }]}),
            ),
            (
                "lambo_record_action",
                json!({"agent_id": "agent-a", "action": "ran it",
                       "against": [reference.clone()]}),
            ),
        ] {
            let out = call_raw(&s, tool, args).await;
            assert_eq!(out.is_error, Some(true), "{tool}");
            let text = text_of(&out);
            assert!(
                text.contains(
                    "is in scope \"general\"; cross-session contradictions are not recorded"
                ) && text.contains("write the assertion in scope \"general\" itself"),
                "{tool}: {text}"
            );
        }
    }
    assert_eq!(
        before,
        (
            s.memory().graph().read().snapshot(),
            general.memory().graph().read().snapshot(),
        )
    );
    s.mem.close().await.expect("close");
    general.mem.close().await.expect("close general");
}
