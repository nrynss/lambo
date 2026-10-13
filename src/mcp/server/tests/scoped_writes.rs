//! #33 PR 4: `scope` on the writes, `lambo_reserve` and `lambo_saints`,
//! on the server alone (the serve's registry is replaced by a test
//! [`LayerSource`], as in `layered`).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Weak;

use super::layered::{
    derive, layered, project_and_general, structured, GENERAL_RULING, PROJECT_FACT,
};
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
use crate::writeq::ReceiptId;

/// The concepts `s`'s graph holds, by content.
fn contents(s: &LamboServer) -> Vec<String> {
    s.memory()
        .graph()
        .read()
        .concepts()
        .map(|c| c.content.clone())
        .collect()
}

/// The node id of `content` in `s`.
fn id_of(s: &LamboServer, content: &str) -> NodeId {
    s.memory()
        .graph()
        .read()
        .concepts()
        .find(|c| c.content == content)
        .unwrap_or_else(|| panic!("{content:?} is in {}", s.memory().session()))
        .id
}

fn receipt_of(out: &CallToolResult) -> String {
    structured(out)["receipt"]
        .as_str()
        .expect("the ack carries a receipt")
        .to_string()
}

fn derive_args(content: &str, concept_type: &str, scope: Option<&str>) -> serde_json::Value {
    let mut args = json!({
        "agent_id": "agent-a",
        "concepts": [{"content": content, "concept_type": concept_type}],
    });
    if let Some(scope) = scope {
        args["scope"] = json!(scope);
    }
    args
}

fn action_args(
    action: &str,
    field: &str,
    entries: &[&str],
    scope: Option<&str>,
) -> serde_json::Value {
    let mut args = json!({"agent_id": "agent-a", "action": action, field: entries});
    if let Some(scope) = scope {
        args["scope"] = json!(scope);
    }
    args
}

/// Every write tool, `lambo_reserve` and `lambo_saints` act on the layer a
/// `scope` names and on this session without one (or with its own name);
/// the scoped ack names the session and says writes to different sessions
/// are not ordered.
#[tokio::test]
async fn each_tool_acts_on_its_scope_and_on_this_session_without_one() {
    let (s, general, _source) = project_and_general("each").await;

    // lambo_derive.
    let scoped = call(
        &s,
        "lambo_derive",
        derive_args("rebase before merge", "logic", Some("general")),
    )
    .await;
    assert_eq!(scoped.is_error, Some(false), "{scoped:?}");
    assert_eq!(structured(&scoped)["session"], json!("general"));
    assert!(
        text_of(&scoped).contains(
            "written to session 'general'; writes to different sessions have no ordering \
             between them"
        ),
        "{}",
        text_of(&scoped)
    );
    let own = call(
        &s,
        "lambo_derive",
        derive_args("prune target dirs", "logic", None),
    )
    .await;
    assert_eq!(own.is_error, Some(false), "{own:?}");
    assert!(structured(&own).get("session").is_none());
    assert!(!text_of(&own).contains("written to session"));
    let named = call(
        &s,
        "lambo_derive",
        derive_args("keep worktrees short", "logic", Some("each-proj")),
    )
    .await;
    assert!(
        structured(&named).get("session").is_none(),
        "its own name is this session"
    );
    assert!(contents(&general).contains(&"rebase before merge".to_string()));
    assert!(!contents(&s).contains(&"rebase before merge".to_string()));
    for c in ["prune target dirs", "keep worktrees short"] {
        assert!(contents(&s).contains(&c.to_string()), "{c}");
        assert!(!contents(&general).contains(&c.to_string()), "{c}");
    }

    // lambo_record_action.
    let scoped = call(
        &s,
        "lambo_record_action",
        action_args(
            "set the global merge policy",
            "produces",
            &["merge policy"],
            Some("general"),
        ),
    )
    .await;
    assert_eq!(scoped.is_error, Some(false), "{scoped:?}");
    assert_eq!(structured(&scoped)["session"], json!("general"));
    assert!(contents(&general).contains(&"set the global merge policy".to_string()));
    assert!(!contents(&s).contains(&"set the global merge policy".to_string()));
    let own = call(
        &s,
        "lambo_record_action",
        action_args("ran the project sweep", "produces", &["sweep log"], None),
    )
    .await;
    assert_eq!(own.is_error, Some(false), "{own:?}");
    assert!(contents(&s).contains(&"ran the project sweep".to_string()));
    assert!(!contents(&general).contains(&"ran the project sweep".to_string()));

    // lambo_reserve: the layer's node, in the layer.
    let ruling = id_of(&general, GENERAL_RULING);
    let reserved = call(
        &s,
        "lambo_reserve",
        json!({"agent_id": "agent-a", "node_id": ruling.0.to_string(), "scope": "general"}),
    )
    .await;
    assert_eq!(reserved.is_error, Some(false), "{reserved:?}");
    assert!(text_of(&reserved).contains("in session 'general'"));
    assert_eq!(structured(&reserved)["session"], json!("general"));
    assert_eq!(
        general
            .memory()
            .graph()
            .read()
            .reservation(ruling)
            .map(|r| r.agent_id.0.clone()),
        Some("agent-a".to_string())
    );
    assert!(s.memory().graph().read().reservation(ruling).is_none());
    // Another agent contends for it in the layer...
    let contended = call(
        &s,
        "lambo_reserve",
        json!({"agent_id": "agent-b", "node_id": ruling.0.to_string(), "scope": "general"}),
    )
    .await;
    assert_eq!(contended.is_error, Some(true), "{contended:?}");
    // ...and releases nothing it does not hold; the holder releases it.
    let released = call(
        &s,
        "lambo_reserve",
        json!({"agent_id": "agent-a", "node_id": ruling.0.to_string(), "scope": "general",
               "release": true}),
    )
    .await;
    assert_eq!(released.is_error, Some(false), "{released:?}");
    assert!(general
        .memory()
        .graph()
        .read()
        .reservation(ruling)
        .is_none());
    let fact = id_of(&s, PROJECT_FACT);
    let own = call(
        &s,
        "lambo_reserve",
        json!({"agent_id": "agent-a", "node_id": fact.0.to_string()}),
    )
    .await;
    assert_eq!(own.is_error, Some(false), "{own:?}");
    assert!(!text_of(&own).contains("in session"));
    assert!(s.memory().graph().read().reservation(fact).is_some());

    // lambo_saints: the layer's list names the layer.
    let saints = call(
        &s,
        "lambo_saints",
        json!({"agent_id": "agent-a", "scope": "general"}),
    )
    .await;
    assert_eq!(saints.is_error, Some(false), "{saints:?}");
    assert!(
        text_of(&saints).contains("in session 'general'"),
        "{}",
        text_of(&saints)
    );
    assert_eq!(structured(&saints)["session"], json!("general"));
    let saints = call(&s, "lambo_saints", json!({"agent_id": "agent-a"})).await;
    assert!(
        text_of(&saints).contains("in session 'each-proj'"),
        "{}",
        text_of(&saints)
    );
    assert!(structured(&saints).get("session").is_none());

    s.mem.close().await.expect("close");
    general.mem.close().await.expect("close");
}

/// `lambo_derive_image` with `scope` embeds and writes into the layer.
#[tokio::test]
async fn an_image_derive_is_written_to_its_scope() {
    let own = image_server("img-proj", Config::default()).await;
    let general = image_server("general", Config::default()).await;
    let (s, _source) = layered(&own, &general);
    let args = |scope: Option<&str>, id: &str| {
        let mut args = json!({
            "agent_id": "agent-a",
            "caption": "the shared architecture diagram",
            "concept_type": "resource",
            "image_id": id,
            "image": {"mime": "image/png", "data": png_b64(id)},
        });
        if let Some(scope) = scope {
            args["scope"] = json!(scope);
        }
        args
    };
    let scoped = call(&s, "lambo_derive_image", args(Some("general"), "g1")).await;
    assert_eq!(scoped.is_error, Some(false), "{scoped:?}");
    assert_eq!(structured(&scoped)["session"], json!("general"));
    let unscoped = call(&s, "lambo_derive_image", args(None, "p1")).await;
    assert_eq!(unscoped.is_error, Some(false), "{unscoped:?}");
    let has = |s: &LamboServer, id: &str| {
        contents(s)
            .iter()
            .any(|c| c.ends_with(&format!("[image:{id}]")))
    };
    assert!(has(&general, "g1") && !has(&own, "g1"));
    assert!(has(&own, "p1") && !has(&general, "p1"));
    own.mem.close().await.expect("close");
    general.mem.close().await.expect("close");
}

/// A scope naming no configured layer is refused with the same `bad_param`
/// on every tool, before anything is written: no receipt, no interaction,
/// neither graph moved.
#[tokio::test]
async fn an_unknown_scope_is_refused_before_the_ack_on_every_tool() {
    let (s, general, _source) = project_and_general("unknown").await;
    let before = (
        s.memory().graph().read().snapshot(),
        general.memory().graph().read().snapshot(),
    );
    let fact = id_of(&s, PROJECT_FACT).0.to_string();
    for (tool, mut args) in [
        ("lambo_derive", derive_args("a fact", "logic", None)),
        (
            "lambo_record_action",
            action_args("an act", "produces", &["x"], None),
        ),
        (
            "lambo_derive_image",
            json!({"agent_id": "agent-a", "caption": "c", "concept_type": "entity",
                   "vector": {"values": [0.0], "contract": {"kind": "fixture", "dim": 1}}}),
        ),
        (
            "lambo_reserve",
            json!({"agent_id": "agent-a", "node_id": fact}),
        ),
        ("lambo_saints", json!({"agent_id": "agent-a"})),
    ] {
        args["scope"] = json!("someone-elses");
        let out = call_raw(&s, tool, args).await;
        assert_eq!(out.is_error, Some(true), "{tool}");
        let text = text_of(&out);
        assert!(
            text.contains(
                "scope must name this session or one of its layers: unknown-proj, general"
            ),
            "{tool}: {text}"
        );
        assert!(
            !text.contains("someone-elses"),
            "{tool}: the value is not quoted"
        );
        assert!(
            out.structured_content.is_none(),
            "{tool}: no ack, no receipt"
        );
    }
    assert_eq!(
        before,
        (
            s.memory().graph().read().snapshot(),
            general.memory().graph().read().snapshot(),
        )
    );
    // On an unlayered session only its own name is a scope.
    let plain = server("unknown-plain").await;
    let out = call_raw(
        &plain,
        "lambo_derive",
        derive_args("a fact", "logic", Some("general")),
    )
    .await;
    assert!(
        text_of(&out).contains("scope must name this session or one of its layers: unknown-plain"),
        "{}",
        text_of(&out)
    );
    plain.mem.close().await.expect("close");
    s.mem.close().await.expect("close");
    general.mem.close().await.expect("close");
}

/// Defence in depth: a grant that reaches the session but not its layer
/// gets the generic refusal on a scoped write (nothing written), and its
/// unscoped writes are served; the receipt drain skips the layer it cannot
/// read.
#[tokio::test]
async fn a_scoped_write_is_refused_for_a_grant_that_does_not_reach_the_layer() {
    let (s, general, _source) = project_and_general("grant").await;
    let hosted = HostedSessions::new(
        ["grant-proj", "general"].map(|n| parse_addressed(n).unwrap()),
        [],
    );
    let narrow = SessionGrant::new(
        "narrow",
        SessionScope::new([parse_addressed("grant-proj").unwrap()], false, None),
        SessionCapabilities::default(),
    );
    let authority = Arc::new(SessionAuthority::with_credentials(
        [(
            crate::mcp::SecretToken::new(["fake", "narrow", "pr4"].join("-")).unwrap(),
            narrow.clone(),
        )],
        hosted,
    ));
    let s = s.clone().with_layers(ServerLayers {
        authority: Some(authority),
        ..s.layers().clone()
    });
    let auth = || Some(CallAuth::Http(Some(Arc::new(narrow.clone()))));
    let before = general.memory().graph().read().snapshot();
    let params = |scope: Option<&str>| super::super::params::DeriveParams {
        agent_id: "agent-a".into(),
        concepts: vec![WireConcept {
            content: "a narrow write".into(),
            concept_type: WireConceptType::Logic,
            depends_on: None,
        }],
        scope: scope.map(str::to_string),
        ..Default::default()
    };
    let out = with_caller(None, auth(), s.derive_impl(params(Some("general")))).await;
    assert_eq!(out.is_error, Some(true));
    let text = text_of(&out);
    assert!(text.contains("cannot read every session"), "{text}");
    assert!(!text.contains("general"), "names no session: {text}");
    assert_eq!(before, general.memory().graph().read().snapshot());
    let out = with_caller(
        None,
        auth(),
        s.saints_impl(super::super::params::SaintsParams {
            agent_id: "agent-a".into(),
            scope: Some("general".into()),
        }),
    )
    .await;
    assert!(text_of(&out).contains("cannot read every session"));

    let out = with_caller(None, auth(), s.derive_impl(params(None))).await;
    assert_eq!(out.is_error, Some(false), "{out:?}");
    let id: ReceiptId = receipt_of(&out).parse().unwrap();
    let pipeline = with_caller(None, auth(), async { s.receipt_pipeline(id) }).await;
    assert!(Arc::ptr_eq(&pipeline, s.memory().pipeline()));
    let (_, left) = with_caller(None, auth(), async {
        s.take_receipts(&AgentId::new("agent-a"))
    })
    .await;
    assert_eq!(left, 0);
    s.mem.close().await.expect("close");
    general.mem.close().await.expect("close");
}

/// Settled outcomes from this session's pipeline and the layer's both come
/// back on the next tool response, each exactly once.
#[tokio::test]
async fn receipts_from_both_pipelines_drain_on_the_next_response() {
    let (s, general, _source) = project_and_general("drain").await;
    let own = call_raw(
        &s,
        "lambo_derive",
        derive_args("a project fact", "logic", None),
    )
    .await;
    let scoped = call_raw(
        &s,
        "lambo_derive",
        derive_args("a general ruling", "logic", Some("general")),
    )
    .await;
    let (own_id, scoped_id) = (receipt_of(&own), receipt_of(&scoped));
    let agent = AgentId::new("agent-a");
    for (mem, id) in [(&s.mem, &own_id), (&general.mem, &scoped_id)] {
        let answer = mem
            .pipeline()
            .wait(&agent, id.parse().unwrap(), crate::writeq::RECEIPT_WAIT_MAX)
            .await;
        assert_eq!(answer.tag(), "applied", "{id}");
    }
    let next = call_raw(&s, "lambo_saints", json!({"agent_id": "agent-a"})).await;
    let text = text_of(&next);
    assert!(
        text.contains("write receipts (your earlier writes, now settled):"),
        "{text}"
    );
    assert!(text.contains(&own_id), "{text}");
    assert!(text.contains(&scoped_id), "{text}");
    let again = call_raw(&s, "lambo_saints", json!({"agent_id": "agent-a"})).await;
    assert!(!text_of(&again).contains("write receipts"), "take-once");
    s.mem.close().await.expect("close");
    general.mem.close().await.expect("close");
}

/// The piggyback's one budget is shared across pipelines: ten settled
/// receipts (five per session) come back eight on one response and the
/// rest on the next, with the count of what waits.
#[tokio::test]
async fn the_piggyback_budget_is_shared_across_pipelines() {
    let (s, general, _source) = project_and_general("budget").await;
    // Written through each session's own pipeline directly, so no tool
    // response drains them on the way.
    let agent = AgentId::new("agent-z");
    for i in 0..5 {
        for (mem, content) in [
            (s.memory(), format!("project fact {i}")),
            (general.memory(), format!("general ruling {i}")),
        ] {
            let submitted = mem
                .derive_async_as(
                    &agent,
                    &[(&content, ConceptType::Logic)],
                    &ParentOf::none(),
                    None,
                )
                .await
                .expect("submit");
            let answer = mem
                .pipeline()
                .wait(&agent, submitted.receipt, crate::writeq::RECEIPT_WAIT_MAX)
                .await;
            assert_eq!(answer.tag(), "applied");
        }
    }
    let (taken, left) = s.take_receipts(&agent);
    assert_eq!(taken.len(), crate::writeq::MAX_PIGGYBACK_RECEIPTS);
    assert_eq!(taken.len() + left, 10);
    let (rest, left) = s.take_receipts(&agent);
    assert_eq!((rest.len(), left), (2, 0));
    s.mem.close().await.expect("close");
    general.mem.close().await.expect("close");
}

/// `lambo_stats` with a receipt finds it in the pipeline that issued it:
/// a scoped write's receipt waits on the layer's pipeline and answers
/// `applied` with the layer's node ids. An id no pipeline issued is
/// answered by this session's, as before.
#[tokio::test]
async fn lambo_stats_routes_a_receipt_to_its_pipeline_by_epoch() {
    let (s, general, _source) = project_and_general("route").await;
    let scoped = call_raw(
        &s,
        "lambo_derive",
        derive_args("routed ruling", "logic", Some("general")),
    )
    .await;
    let receipt = receipt_of(&scoped);
    let id: ReceiptId = receipt.parse().unwrap();
    assert!(general.memory().pipeline().knows_receipt(id));
    assert!(!s.memory().pipeline().knows_receipt(id));
    assert!(Arc::ptr_eq(
        &s.receipt_pipeline(id),
        general.memory().pipeline()
    ));
    let waited = call_raw(
        &s,
        "lambo_stats",
        json!({"agent_id": "agent-a", "receipt": receipt, "wait_ms": 30_000}),
    )
    .await;
    let payload = structured(&waited);
    assert_eq!(payload["receipt"]["state"], json!("applied"), "{payload}");
    let created = payload["receipt"]["created"][0]
        .as_str()
        .expect("created id");
    assert_eq!(id_of(&general, "routed ruling").0.to_string(), created);
    // Delivered explicitly, so not piggybacked again.
    assert!(!text_of(&waited).contains("write receipts"));
    let next = call_raw(&s, "lambo_saints", json!({"agent_id": "agent-a"})).await;
    assert!(!text_of(&next).contains(&receipt), "{}", text_of(&next));

    let own = call(&s, "lambo_derive", derive_args("own fact", "logic", None)).await;
    let own_id: ReceiptId = receipt_of(&own).parse().unwrap();
    assert!(Arc::ptr_eq(
        &s.receipt_pipeline(own_id),
        s.memory().pipeline()
    ));
    let foreign: ReceiptId = "lwr1.00000000000000ab.1.1".parse().unwrap();
    assert!(Arc::ptr_eq(
        &s.receipt_pipeline(foreign),
        s.memory().pipeline()
    ));
    let lost = call_raw(
        &s,
        "lambo_stats",
        json!({"agent_id": "agent-a", "receipt": foreign.to_string()}),
    )
    .await;
    assert_eq!(structured(&lost)["receipt"]["state"], json!("restart_lost"));
    s.mem.close().await.expect("close");
    general.mem.close().await.expect("close");
}

/// A source over one layer that counts reads by kind and can refuse a
/// direct one as over its rate.
struct Counting {
    mem: Arc<Memory>,
    direct: AtomicUsize,
    other: AtomicUsize,
    limited: bool,
}

impl LayerSource for Counting {
    fn live_layer(&self, _session: &str, read: LayerRead) -> Result<LiveLayer, LayerDown> {
        if read == LayerRead::Direct {
            self.direct.fetch_add(1, Ordering::SeqCst);
            if self.limited {
                return Err(LayerDown::RateLimited);
            }
        } else {
            self.other.fetch_add(1, Ordering::SeqCst);
        }
        Ok(LiveLayer {
            mem: Arc::clone(&self.mem),
            _in_flight: None,
        })
    }
}

fn counted(
    own: &LamboServer,
    general: &LamboServer,
    limited: bool,
) -> (LamboServer, Arc<Counting>) {
    let source = Arc::new(Counting {
        mem: Arc::clone(general.memory()),
        direct: AtomicUsize::new(0),
        other: AtomicUsize::new(0),
        limited,
    });
    let weak: Weak<Counting> = Arc::downgrade(&source);
    let weak: Weak<dyn LayerSource> = weak;
    let s = own.clone().with_layers(ServerLayers {
        handles: vec![LayerHandle::new(
            "general",
            DEFAULT_LAYER_WEIGHT,
            DEFAULT_LAYER_MIN_SIMILARITY,
            weak,
        )],
        authority: None,
    });
    (s, source)
}

/// A scoped write spends exactly one token from the layer's own rate (a
/// direct lookup); an unscoped write spends none (its receipt drain is not
/// a direct read). A layer over its rate refuses the scoped write,
/// retryable, and writes nothing.
#[tokio::test]
async fn a_scoped_write_spends_one_layer_token() {
    let own = server("rate-proj").await;
    let general = server("general").await;
    let (s, source) = counted(&own, &general, false);
    call_raw(&s, "lambo_derive", derive_args("unscoped", "logic", None)).await;
    assert_eq!(source.direct.load(Ordering::SeqCst), 0);
    call_raw(
        &s,
        "lambo_derive",
        derive_args("scoped", "logic", Some("general")),
    )
    .await;
    assert_eq!(source.direct.load(Ordering::SeqCst), 1);
    call_raw(
        &s,
        "lambo_record_action",
        action_args("scoped act", "produces", &["x"], Some("general")),
    )
    .await;
    assert_eq!(source.direct.load(Ordering::SeqCst), 2);
    assert!(
        source.other.load(Ordering::SeqCst) > 0,
        "the drain read the layer"
    );

    let (s, source) = counted(&own, &general, true);
    let before = general.memory().graph().read().snapshot();
    let out = call_raw(
        &s,
        "lambo_derive",
        derive_args("limited", "logic", Some("general")),
    )
    .await;
    assert_eq!(out.is_error, Some(true));
    assert!(
        text_of(&out).contains(
            "lambo_derive: layer general is over its request rate; retry shortly, or call \
             without scope to act on rate-proj"
        ),
        "{}",
        text_of(&out)
    );
    assert_eq!(source.direct.load(Ordering::SeqCst), 1);
    assert_eq!(before, general.memory().graph().read().snapshot());
    own.mem.close().await.expect("close");
    general.mem.close().await.expect("close");
}

/// `general` is shared by every project, so a write into it refuses bare
/// relative paths (record_action targets, derive Resources) before the
/// ack; absolute paths and prose are accepted, and the same relative path
/// without scope is the project's own.
#[tokio::test]
async fn a_relative_path_is_refused_in_a_layer() {
    let (s, general, _source) = project_and_general("paths").await;
    let before = general.memory().graph().read().snapshot();
    for (field, entry) in [
        ("produces", "src/main.rs"),
        ("modifies", "README.md"),
        ("depends_on", "../lambo/AGENTS.md"),
    ] {
        let out = call_raw(
            &s,
            "lambo_record_action",
            action_args("edited a file", field, &[entry], Some("general")),
        )
        .await;
        assert_eq!(out.is_error, Some(true), "{field}");
        let text = text_of(&out);
        assert!(
            text.contains(&format!(
                "{field} entry {entry:?} is a relative path, refused in scope \"general\""
            )),
            "{text}"
        );
        assert!(text.contains("or use an absolute path"), "{text}");
    }
    let out = call_raw(
        &s,
        "lambo_derive",
        derive_args("Cargo.toml", "resource", Some("general")),
    )
    .await;
    assert_eq!(out.is_error, Some(true));
    assert!(text_of(&out).contains("concept.content entry \"Cargo.toml\" is a relative path"));
    assert_eq!(before, general.memory().graph().read().snapshot());

    for entry in [
        "/Users/me/lambo/src/main.rs",
        "the merge policy",
        "nrynss/lambo#147",
    ] {
        let out = call(
            &s,
            "lambo_record_action",
            action_args("set a shared rule", "produces", &[entry], Some("general")),
        )
        .await;
        assert_eq!(out.is_error, Some(false), "{entry}: {out:?}");
    }
    let out = call(
        &s,
        "lambo_record_action",
        action_args("edited", "modifies", &["src/main.rs"], None),
    )
    .await;
    assert_eq!(out.is_error, Some(false), "{out:?}");
    assert!(contents(&s).contains(&"src/main.rs".to_string()));
    s.mem.close().await.expect("close");
    general.mem.close().await.expect("close");
}

/// A scoped write resolves its references against the session it writes
/// to: a project concept cited from a write scoped to `general`, and a
/// `general` concept cited from an unscoped write, are refused as
/// cross-session, naming the reference (so the id of the edge that would
/// have crossed sessions is reported) and writing nothing in either
/// session. A citation inside the scoped session is accepted.
#[tokio::test]
async fn a_cross_session_citation_or_edge_is_refused_with_its_id() {
    let (s, general, _source) = project_and_general("cross").await;
    let fact = id_of(&s, PROJECT_FACT);
    let ruling = id_of(&general, GENERAL_RULING);
    let before = (
        s.memory().graph().read().snapshot(),
        general.memory().graph().read().snapshot(),
    );
    let cite = |reference: String, scope: Option<&str>| {
        let mut args = json!({"agent_id": "agent-a", "concepts": [{
            "content": "a decision resting on another", "concept_type": "logic",
            "depends_on": [reference],
        }]});
        if let Some(scope) = scope {
            args["scope"] = json!(scope);
        }
        args
    };
    // Project concept, write scoped to general: refused, naming the project.
    for reference in [
        crate::recall::format::short_id(fact),
        fact.0.to_string(),
        PROJECT_FACT.to_string(),
    ] {
        let out = call_raw(&s, "lambo_derive", cite(reference.clone(), Some("general"))).await;
        assert_eq!(out.is_error, Some(true), "{reference}");
        let text = text_of(&out);
        assert!(
            text.contains("is in scope \"cross-proj\"; cross-session citations are not recorded"),
            "{text}"
        );
        assert!(
            text.contains(&reference),
            "the reference is reported: {text}"
        );
    }
    let out = call_raw(
        &s,
        "lambo_record_action",
        action_args(
            "applied it",
            "depends_on",
            &[&fact.0.to_string()],
            Some("general"),
        ),
    )
    .await;
    assert_eq!(out.is_error, Some(true));
    let text = text_of(&out);
    assert!(text.contains("is in scope \"cross-proj\""), "{text}");
    assert!(text.contains(&fact.0.to_string()), "{text}");
    // General concept, unscoped write: refused, naming general.
    let out = call_raw(&s, "lambo_derive", cite(ruling.0.to_string(), None)).await;
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
    // Within the scoped session, a citation lands.
    let out = call(
        &s,
        "lambo_derive",
        cite(crate::recall::format::short_id(ruling), Some("general")),
    )
    .await;
    assert_eq!(out.is_error, Some(false), "{out:?}");
    let cited_in_general = {
        let g = general.memory().graph().read();
        let citing = g
            .concepts()
            .find(|c| c.content == "a decision resting on another")
            .expect("written in general")
            .id;
        g.edges().any(|e| e.source == citing && e.target == ruling)
    };
    assert!(cited_in_general, "the citation edge stays inside general");
    s.mem.close().await.expect("close");
    general.mem.close().await.expect("close");
}

/// A layer that loses its lease while a scoped call holds it: the write is
/// refused as the layer having stopped serving (retryable), never with the
/// store's operator-only lease text, and nothing is applied to it.
struct LosesLease(Arc<Memory>);

impl LayerSource for LosesLease {
    fn live_layer(&self, _session: &str, read: LayerRead) -> Result<LiveLayer, LayerDown> {
        if read == LayerRead::Direct {
            // Found live, then fenced before the call acts on it.
            self.0.simulate_lease_loss();
        }
        Ok(LiveLayer {
            mem: Arc::clone(&self.0),
            _in_flight: None,
        })
    }
}

#[tokio::test]
async fn losing_the_layers_lease_mid_call_is_a_clear_refusal() {
    let own = server("lease-proj").await;
    let general = server("general").await;
    derive(&general, GENERAL_RULING).await;
    let ruling = id_of(&general, GENERAL_RULING).0.to_string();
    let source: Arc<dyn LayerSource> = Arc::new(LosesLease(Arc::clone(general.memory())));
    let s = own.clone().with_layers(ServerLayers {
        handles: vec![LayerHandle::new(
            "general",
            DEFAULT_LAYER_WEIGHT,
            DEFAULT_LAYER_MIN_SIMILARITY,
            Arc::downgrade(&source),
        )],
        authority: None,
    });
    let before = general.memory().graph().read().snapshot();
    for (tool, args) in [
        (
            "lambo_derive",
            derive_args("a ruling", "logic", Some("general")),
        ),
        (
            "lambo_record_action",
            action_args("an act", "produces", &["x"], Some("general")),
        ),
        (
            "lambo_reserve",
            json!({"agent_id": "agent-a", "node_id": ruling, "scope": "general"}),
        ),
    ] {
        let out = call_raw(&s, tool, args).await;
        assert_eq!(out.is_error, Some(true), "{tool}");
        let text = text_of(&out);
        assert!(
            text.contains(&format!(
                "{tool}: layer general stopped serving during this call (it closed or lost its \
                 lease), so nothing was applied to it; retry shortly, or call without scope to \
                 act on lease-proj"
            )),
            "{text}"
        );
        assert!(!text.contains("DELETE"), "no operator SQL: {text}");
    }
    assert_eq!(before, general.memory().graph().read().snapshot());
    // This session is untouched and still writes.
    let out = call(&s, "lambo_derive", derive_args("still mine", "logic", None)).await;
    assert_eq!(out.is_error, Some(false), "{out:?}");
    own.mem.close().await.expect("close");
    let _ = general.mem.close().await;
}

/// Unscoped calls on an unlayered session answer exactly as before #33 PR
/// 4: the write acks' text and structured keys, `lambo_reserve` and
/// `lambo_saints`, with no `session` key and no scope line; and a scope
/// naming the session itself answers byte for byte as no scope does.
#[tokio::test]
async fn unscoped_calls_on_an_unlayered_session_are_byte_identical() {
    let plain = server("bytes-plain").await;
    let ack = call(
        &plain,
        "lambo_derive",
        derive_args("a plain fact", "logic", None),
    )
    .await;
    let receipt = receipt_of(&ack);
    let mut keys: Vec<&str> = structured(&ack)
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "agent_id",
            "receipt",
            "receipt_state",
            "summary",
            "warnings"
        ]
    );
    let text = match &ack.content[0] {
        ContentBlock::Text(t) => t.text.clone(),
        other => panic!("expected text, got {other:?}"),
    };
    // The fixed parts of the 0.5.0 ack, in order, and nothing after the
    // attribution line.
    assert!(
        text.starts_with(&format!(
            "accepted 1 concept(s) for background write; validated and ordered, not yet \
             applied\nreceipt {receipt}: "
        )),
        "{text}"
    );
    assert!(text.contains(&format!(
        "call lambo_stats with receipt={receipt} (add wait_ms to block).\n"
    )));
    assert_eq!(
        text.lines().count(),
        4,
        "summary, receipt, how to wait, recorded under: {text}"
    );
    assert!(
        text.lines()
            .last()
            .unwrap()
            .starts_with("Recorded under agent_id"),
        "{text}"
    );

    let action = call(
        &plain,
        "lambo_record_action",
        action_args("did it", "produces", &["README.md"], None),
    )
    .await;
    assert_eq!(
        action.is_error,
        Some(false),
        "relative paths are fine here: {action:?}"
    );
    let mut keys: Vec<&str> = structured(&action)
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "agent_id",
            "receipt",
            "receipt_state",
            "summary",
            "warnings"
        ]
    );
    assert_eq!(
        text_of(&action)
            .lines()
            .filter(|l| l.contains("written to session"))
            .count(),
        0
    );

    // Read tools: the unscoped and own-name answers are the same bytes.
    let saints = |scope: Option<&str>| {
        let mut args = json!({"agent_id": "agent-b"});
        if let Some(scope) = scope {
            args["scope"] = json!(scope);
        }
        call_raw(&plain, "lambo_saints", args)
    };
    let a = saints(None).await;
    let b = saints(Some("bytes-plain")).await;
    assert_eq!(
        serde_json::to_string(&a).unwrap(),
        serde_json::to_string(&b).unwrap()
    );
    let mut keys: Vec<&str> = structured(&a)
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort();
    assert_eq!(keys, ["saints", "summary", "warnings"]);

    let id = id_of(&plain, "a plain fact").0.to_string();
    let reserved = call_raw(
        &plain,
        "lambo_reserve",
        json!({"agent_id": "agent-b", "node_id": id}),
    )
    .await;
    let mut keys: Vec<&str> = structured(&reserved)
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        ["agent_id", "expires_at", "node_id", "summary", "warnings"]
    );
    assert!(text_of(&reserved).starts_with(&format!("reserved {id} until ")));
    assert!(!text_of(&reserved).contains("in session"));

    // And the unscoped piggyback is the pipeline's own, as before.
    let (taken, left) = plain.take_receipts(&AgentId::new("agent-a"));
    assert!(taken.len() <= crate::writeq::MAX_PIGGYBACK_RECEIPTS && left == 0);
    plain.mem.close().await.expect("close");
}

/// The ledger line of a scoped call records `scope`; an unscoped one has
/// no such key.
#[tokio::test]
async fn the_ledger_records_a_writes_scope() {
    let dir = ledger_dir("scoped-writes");
    let path = dir.join("calls.jsonl");
    let own = server_with_ledger("ledger-w-proj", &path).await;
    let ledger = Arc::clone(own.ledger().expect("ledger"));
    let general = server("general").await;
    let (s, _source) = layered(&own, &general);
    call_raw(
        &s,
        "lambo_derive",
        derive_args("scoped", "logic", Some("general")),
    )
    .await;
    call_raw(&s, "lambo_derive", derive_args("unscoped", "logic", None)).await;
    call_raw(
        &s,
        "lambo_saints",
        json!({"agent_id": "agent-a", "scope": "general"}),
    )
    .await;
    let lines = read_ledger(&ledger, 3);
    assert_eq!(lines[0]["scope"], json!("general"), "{}", lines[0]);
    assert!(lines[1].get("scope").is_none(), "{}", lines[1]);
    assert_eq!(lines[2]["scope"], json!("general"), "{}", lines[2]);
    ledger.shutdown();
    own.mem.close().await.expect("close");
    general.mem.close().await.expect("close");
}
