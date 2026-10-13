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
            against: None,
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

/// Write `content` to `mem` as `agent`, delivered to `deliver_to` (as a
/// layered session's scoped write is), and wait for it to apply. Straight
/// through the pipeline, so no tool response drains it on the way.
async fn write_through(
    mem: &Memory,
    agent: &AgentId,
    content: &str,
    deliver_to: Option<&crate::types::SessionId>,
) -> ReceiptId {
    let submitted = mem
        .derive_cited_async_scoped(
            agent,
            &[(content, ConceptType::Logic)],
            &ParentOf::none(),
            &[],
            None,
            &crate::surface::resolve::NoForeignScopes,
            deliver_to,
        )
        .await
        .expect("submit");
    let answer = mem
        .pipeline()
        .wait(agent, submitted.receipt, crate::writeq::RECEIPT_WAIT_MAX)
        .await;
    assert_eq!(answer.tag(), "applied");
    submitted.receipt
}

/// The piggyback's one budget is shared across pipelines: ten settled
/// receipts (five per session) come back eight on one response and the
/// rest on the next, with the count of what waits. Review L3: a pipeline
/// with receipts waiting always gets a slot, so ten of this session's and
/// one of the layer's put the layer's on the first response.
#[tokio::test]
async fn the_piggyback_budget_is_shared_across_pipelines() {
    let (s, general, _source) = project_and_general("budget").await;
    let agent = AgentId::new("agent-z");
    let own = s.memory().session().clone();
    for i in 0..5 {
        write_through(s.memory(), &agent, &format!("project fact {i}"), None).await;
        write_through(
            general.memory(),
            &agent,
            &format!("general ruling {i}"),
            Some(&own),
        )
        .await;
    }
    let (taken, left) = s.take_receipts(&agent);
    assert_eq!(taken.len(), crate::writeq::MAX_PIGGYBACK_RECEIPTS);
    assert_eq!(taken.len() + left, 10);
    let (rest, left) = s.take_receipts(&agent);
    assert_eq!((rest.len(), left), (2, 0));

    let mut own_ids = Vec::new();
    for i in 0..10 {
        own_ids.push(write_through(s.memory(), &agent, &format!("more fact {i}"), None).await);
    }
    let layer_id = write_through(general.memory(), &agent, "one more ruling", Some(&own)).await;
    let (taken, left) = s.take_receipts(&agent);
    assert_eq!(taken.len(), crate::writeq::MAX_PIGGYBACK_RECEIPTS);
    assert_eq!(left, 3);
    assert!(
        taken.iter().any(|(id, _)| *id == layer_id),
        "the layer's one receipt is not starved by this session's ten"
    );
    s.mem.close().await.expect("close");
    general.mem.close().await.expect("close");
}

/// Review H1: two projects layered with one `general`, their agents
/// sharing one id, both writing to `general` with `scope`, and a direct
/// writer at `general` itself under the same id. Each server hands out
/// only the receipts of writes it sent: neither project sees the other's
/// (whose failure detail could quote that project's input), and `general`'s
/// own server sees only its own direct write. A receipt id from the other
/// project is `forbidden` through `lambo_stats`.
#[tokio::test]
async fn two_projects_sharing_an_agent_id_get_only_their_own_layer_receipts() {
    let alpha_own = server("alpha-proj").await;
    let beta_own = server("beta-proj").await;
    let general = server("general").await;
    let general = general.clone().serving_a_layer(true);
    let (alpha, _a) = layered(&alpha_own, &general);
    let (beta, _b) = layered(&beta_own, &general);
    let agent = AgentId::new("agent-a");

    // Through the pipelines, tagged as each server's tools tag them, so no
    // tool response drains them before the check.
    let ids = [
        write_through(
            general.memory(),
            &agent,
            "alpha's ruling",
            Some(alpha.memory().session()),
        )
        .await,
        write_through(
            general.memory(),
            &agent,
            "beta's ruling",
            Some(beta.memory().session()),
        )
        .await,
        write_through(general.memory(), &agent, "curated ruling", None).await,
    ];
    let only = |s: &LamboServer, want: ReceiptId| {
        let (taken, left) = s.take_receipts(&agent);
        let got: Vec<ReceiptId> = taken.iter().map(|(id, _)| *id).collect();
        assert_eq!((got, left), (vec![want], 0), "{}", s.memory().session());
    };
    only(&alpha, ids[0]);
    only(&beta, ids[1]);
    only(&general, ids[2]);

    // Through the tools: alpha's scoped write is tagged for alpha, and by
    // id it is forbidden to beta and to general's own server.
    let ack = call(
        &alpha,
        "lambo_derive",
        derive_args("alpha again", "logic", Some("general")),
    )
    .await;
    let id = receipt_of(&ack);
    for (s, state) in [
        (&beta, "forbidden"),
        (&general, "forbidden"),
        (&alpha, "applied"),
    ] {
        let out = call_raw(
            s,
            "lambo_stats",
            json!({"agent_id": "agent-a", "receipt": id}),
        )
        .await;
        assert_eq!(
            structured(&out)["receipt"]["state"],
            json!(state),
            "{}",
            s.memory().session()
        );
    }
    for s in [&alpha_own, &beta_own, &general] {
        s.mem.close().await.expect("close");
    }
}

/// Review M2: a receipt no serving pipeline knows, asked while a layer the
/// call reads is not serving, may be that layer's: `lambo_stats` answers
/// it as unknown for now (`pending`, ask again), not `restart_lost`; once
/// the layer is back, its receipt answers as usual.
#[tokio::test]
async fn a_receipt_asked_while_its_layer_is_down_is_ask_again() {
    let (s, general, source) = project_and_general("down-receipt").await;
    let scoped = call(
        &s,
        "lambo_derive",
        derive_args("a ruling", "logic", Some("general")),
    )
    .await;
    let receipt = receipt_of(&scoped);
    let taken = source.0.lock().take();
    let out = call_raw(
        &s,
        "lambo_stats",
        json!({"agent_id": "agent-a", "receipt": receipt, "wait_ms": 1_000}),
    )
    .await;
    let r = &structured(&out)["receipt"];
    assert_eq!(r["state"], json!("pending"), "{r}");
    assert!(
        r["detail"]
            .as_str()
            .unwrap()
            .contains("layer general is not serving"),
        "{r}"
    );
    assert!(
        structured(&out)["node_count"].is_number(),
        "the stats payload is whole"
    );
    *source.0.lock() = taken;
    let out = call_raw(
        &s,
        "lambo_stats",
        json!({"agent_id": "agent-a", "receipt": receipt}),
    )
    .await;
    assert_eq!(structured(&out)["receipt"]["state"], json!("applied"));
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
    mem: parking_lot::Mutex<Arc<Memory>>,
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
            mem: Arc::clone(&self.mem.lock()),
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
        mem: parking_lot::Mutex::new(Arc::clone(general.memory())),
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

/// #19(b) under #33 PR 4: `against` resolves against the session the
/// write goes to, like a citation. In both directions a cross-session
/// contradiction is refused, naming the reference and the holder, on a
/// derive and on an action, and nothing is written in either session: a
/// project concept from a write scoped to `general`, and a `general`
/// concept from an unscoped write. A contradiction inside the scoped
/// session lands there, and only there.
#[tokio::test]
async fn a_cross_session_contradiction_is_refused_in_both_directions() {
    let (s, general, _source) = project_and_general("cross-against").await;
    let fact = id_of(&s, PROJECT_FACT);
    let ruling = id_of(&general, GENERAL_RULING);
    let before = (
        s.memory().graph().read().snapshot(),
        general.memory().graph().read().snapshot(),
    );
    let contradict = |reference: String, scope: Option<&str>| {
        let mut args = json!({"agent_id": "agent-a", "concepts": [{
            "content": "a newer claim against another", "concept_type": "logic",
            "against": [reference],
        }]});
        if let Some(scope) = scope {
            args["scope"] = json!(scope);
        }
        args
    };
    for (target, holder, scope) in [
        (fact, "cross-against-proj", Some("general")),
        (ruling, "general", None),
    ] {
        for reference in [
            crate::recall::format::short_id(target),
            target.0.to_string(),
        ] {
            let out = call_raw(&s, "lambo_derive", contradict(reference.clone(), scope)).await;
            assert_eq!(out.is_error, Some(true), "{reference} {scope:?}");
            let text = text_of(&out);
            assert!(
                text.contains(&format!(
                    "is in scope \"{holder}\"; cross-session contradictions are not recorded"
                )),
                "{text}"
            );
            assert!(
                text.contains(&reference),
                "the reference is reported: {text}"
            );
            let out = call_raw(
                &s,
                "lambo_record_action",
                action_args("acted against it", "against", &[&reference], scope),
            )
            .await;
            assert_eq!(out.is_error, Some(true), "{reference} {scope:?}");
            let text = text_of(&out);
            assert!(
                text.contains(&format!("is in scope \"{holder}\"")),
                "{text}"
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
    // Within the scoped session, a contradiction lands there.
    let out = call(
        &s,
        "lambo_derive",
        contradict(crate::recall::format::short_id(ruling), Some("general")),
    )
    .await;
    assert_eq!(out.is_error, Some(false), "{out:?}");
    let out = call(
        &s,
        "lambo_record_action",
        action_args(
            "challenged the ruling",
            "against",
            &[&ruling.0.to_string()],
            Some("general"),
        ),
    )
    .await;
    assert_eq!(out.is_error, Some(false), "{out:?}");
    {
        let g = general.memory().graph().read();
        for source in ["a newer claim against another", "challenged the ruling"] {
            let id = g
                .concepts()
                .find(|c| c.content == source)
                .unwrap_or_else(|| panic!("{source} is written in general"))
                .id;
            assert!(
                g.edge_between(id, ruling, crate::types::EdgeType::Contradicts)
                    .is_some(),
                "{source}"
            );
        }
    }
    assert!(s
        .memory()
        .graph()
        .read()
        .edges()
        .all(|e| e.edge_type != crate::types::EdgeType::Contradicts));
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

/// An image embedder that parks each image embed until released, and says
/// when one has started.
struct ParkedImages {
    inner: FixtureEmbedder,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl Embedder for ParkedImages {
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.inner.embed(text).await
    }
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.inner.embed_query(text).await
    }
    fn modalities(&self) -> crate::embed::Modalities {
        self.inner.modalities()
    }
    async fn embed_image(
        &self,
        image: crate::embed::ImageInput<'_>,
    ) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.started.notify_one();
        self.release.notified().await;
        self.inner.embed_image(image).await
    }
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        self.inner.as_any()
    }
}

/// Review H2: a scoped `lambo_derive_image` embeds holding nothing of the
/// layer. While the (parked) embed runs, the layer's `Memory` is held by
/// no one but its source (so no in-flight guard or gate permit), and a re-attach
/// can replace it; the write then lands in the layer as it is after the
/// embed (the re-attached one), with no second rate token.
#[tokio::test]
async fn a_scoped_image_embed_holds_nothing_of_the_layer() {
    let parked = Arc::new(ParkedImages {
        inner: FixtureEmbedder::new(),
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let layer_server = |embedder: Arc<ParkedImages>| {
        server_with_parts(
            "general",
            Arc::new(crate::test_util::VectorSearchable(Arc::new(
                MemoryStore::new(),
            ))),
            embedder,
            fixture_contract(),
            Config::default(),
        )
    };
    let own = image_server("parked-proj", Config::default()).await;
    let old_general = layer_server(Arc::clone(&parked)).await;
    let new_general = layer_server(Arc::clone(&parked)).await;
    let (s, source) = counted(&own, &old_general, false);
    let args = json!({
        "agent_id": "agent-a",
        "caption": "the shared diagram",
        "concept_type": "resource",
        "image_id": "d1",
        "image": {"mime": "image/png", "data": png_b64("d1")},
        "scope": "general",
    });
    let call_fut = {
        let s = s.clone();
        tokio::spawn(async move { call_raw(&s, "lambo_derive_image", args).await })
    };
    parked.started.notified().await;
    // Mid-embed: the call holds no strong reference to the layer's Memory
    // (only the source and this test do), so neither its in-flight guard nor
    // its writers-gate permit, which borrows from it, is held.
    assert_eq!(Arc::strong_count(old_general.memory()), 2);
    // The layer is re-attached (a new Memory) while the embed runs.
    *source.mem.lock() = Arc::clone(new_general.memory());
    parked.release.notify_one();
    let out = call_fut.await.expect("join");
    assert_eq!(out.is_error, Some(false), "{out:?}");
    assert_eq!(
        source.direct.load(Ordering::SeqCst),
        1,
        "one token, not two"
    );
    let has = |s: &LamboServer| contents(s).iter().any(|c| c.ends_with("[image:d1]"));
    let receipt: ReceiptId = receipt_of(&out).parse().unwrap();
    let answer = new_general
        .memory()
        .pipeline()
        .wait(
            &AgentId::new("agent-a"),
            receipt,
            crate::writeq::RECEIPT_WAIT_MAX,
        )
        .await;
    assert_eq!(answer.tag(), "applied");
    assert!(has(&new_general) && !has(&old_general) && !has(&own));
    for s in [&own, &old_general, &new_general] {
        s.mem.close().await.expect("close");
    }
}

/// A source with a second layer whose probe fences the first: so a write
/// scoped to the first passes its writers gate, then (when its reference
/// probe reads the second) the first loses its lease before the submit.
struct FencesOnProbe {
    first: Arc<Memory>,
    second: Arc<Memory>,
}

impl LayerSource for FencesOnProbe {
    fn live_layer(&self, session: &str, read: LayerRead) -> Result<LiveLayer, LayerDown> {
        let mem = if session == self.first.session().as_str() {
            Arc::clone(&self.first)
        } else {
            if read == LayerRead::Probe {
                self.first.simulate_lease_loss();
            }
            Arc::clone(&self.second)
        };
        Ok(LiveLayer {
            mem,
            _in_flight: None,
        })
    }
}

/// Review M3: a layer that loses its lease after the write passed its gate
/// but before the ack is answered as having stopped serving, not with a
/// success ack whose outcome no lookup could reach; nothing is applied.
#[tokio::test]
async fn a_lease_lost_between_the_gate_and_the_ack_is_refused() {
    let own = server("m3-proj").await;
    let general = server("general").await;
    let other = server("other").await;
    let source: Arc<dyn LayerSource> = Arc::new(FencesOnProbe {
        first: Arc::clone(general.memory()),
        second: Arc::clone(other.memory()),
    });
    let handle = |name: &str| {
        LayerHandle::new(
            name,
            DEFAULT_LAYER_WEIGHT,
            DEFAULT_LAYER_MIN_SIMILARITY,
            Arc::downgrade(&source),
        )
    };
    let s = own.clone().with_layers(ServerLayers {
        handles: vec![handle("general"), handle("other")],
        authority: None,
    });
    // A hyphenated id that names nothing makes the core probe the other
    // sessions after its gate: the probe of `other` fences `general`.
    let out = call_raw(
        &s,
        "lambo_record_action",
        action_args(
            "a late act",
            "depends_on",
            &["00000000-0000-4000-8000-000000000001"],
            Some("general"),
        ),
    )
    .await;
    assert_eq!(out.is_error, Some(true), "{out:?}");
    assert!(
        text_of(&out).contains("layer general stopped serving during this call"),
        "{}",
        text_of(&out)
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!contents(&general).contains(&"a late act".to_string()));
    own.mem.close().await.expect("close");
    other.mem.close().await.expect("close");
    let _ = general.mem.close().await;
}

/// Review L1: in a layer, every concept's content (any type) and every
/// parent_of end is checked for relative paths, and a record_action
/// depends_on text naming a concept of the caller's own session is refused
/// as cross-session instead of being minted in the layer. Unscoped, the
/// 0.5.0 text rule stands: text naming a layer concept is minted here.
#[tokio::test]
async fn a_layer_write_checks_every_concept_and_never_copies_the_project_in() {
    let (s, general, _source) = project_and_general("l1").await;
    let before = general.memory().graph().read().snapshot();
    let out = call_raw(
        &s,
        "lambo_derive",
        derive_args("README.md", "logic", Some("general")),
    )
    .await;
    assert!(text_of(&out).contains("concept.content entry \"README.md\" is a relative path"));
    let out = call_raw(
        &s,
        "lambo_derive",
        json!({"agent_id": "agent-a", "scope": "general",
               "concepts": [{"content": "a ruling", "concept_type": "logic"}],
               "parent_of": [{"parent": "a ruling", "child": "src/main.rs"}]}),
    )
    .await;
    assert!(
        text_of(&out).contains("parent_of entry \"src/main.rs\" is a relative path"),
        "{}",
        text_of(&out)
    );
    let out = call_raw(
        &s,
        "lambo_record_action",
        action_args(
            "applied the project fact",
            "depends_on",
            &[PROJECT_FACT],
            Some("general"),
        ),
    )
    .await;
    assert_eq!(out.is_error, Some(true));
    let text = text_of(&out);
    assert!(text.contains("is in scope \"l1-proj\""), "{text}");
    assert!(text.contains(PROJECT_FACT), "{text}");
    assert_eq!(before, general.memory().graph().read().snapshot());
    // Unscoped: text naming general's ruling is this session's own concept.
    let out = call(
        &s,
        "lambo_record_action",
        action_args("followed the ruling", "depends_on", &[GENERAL_RULING], None),
    )
    .await;
    assert_eq!(out.is_error, Some(false), "{out:?}");
    assert!(contents(&s).contains(&GENERAL_RULING.to_string()));
    s.mem.close().await.expect("close");
    general.mem.close().await.expect("close");
}

/// Review L2: a server whose session is itself a layer (`/mcp/s/general`)
/// refuses relative paths on its own writes too; an ordinary one does not.
#[tokio::test]
async fn a_layers_own_server_refuses_relative_paths() {
    let general = server("general").await.serving_a_layer(true);
    let out = call_raw(
        &general,
        "lambo_record_action",
        action_args("edited", "modifies", &["README.md"], None),
    )
    .await;
    assert_eq!(out.is_error, Some(true));
    assert!(
        text_of(&out).contains(
            "modifies entry \"README.md\" is a relative path, refused in scope \"general\""
        ),
        "{}",
        text_of(&out)
    );
    let out = call(
        &general,
        "lambo_record_action",
        action_args("edited", "modifies", &["/abs/README.md"], None),
    )
    .await;
    assert_eq!(out.is_error, Some(false), "{out:?}");
    let plain = server("plain-l2").await;
    let out = call(
        &plain,
        "lambo_record_action",
        action_args("edited", "modifies", &["README.md"], None),
    )
    .await;
    assert_eq!(out.is_error, Some(false), "{out:?}");
    general.mem.close().await.expect("close");
    plain.mem.close().await.expect("close");
}

/// As [`FencesOnProbe`], but the probe only closes the first layer (a
/// clean close's first step, which leaves its write queue draining).
struct ClosesOnProbe {
    first: Arc<Memory>,
    second: Arc<Memory>,
}

impl LayerSource for ClosesOnProbe {
    fn live_layer(&self, session: &str, read: LayerRead) -> Result<LiveLayer, LayerDown> {
        let mem = if session == self.first.session().as_str() {
            Arc::clone(&self.first)
        } else {
            if read == LayerRead::Probe {
                self.first.latch_closed_for_test();
            }
            Arc::clone(&self.second)
        };
        Ok(LiveLayer {
            mem,
            _in_flight: None,
        })
    }
}

/// Second review, item 1: a layer that closes cleanly after the write was
/// admitted still applies it (a clean close drains admitted jobs), so the
/// call is acked, with a warning not to resend and to ask lambo_stats,
/// rather than refused as "nothing was applied".
#[tokio::test]
async fn a_layer_that_closes_after_the_submit_acks_with_a_warning() {
    let own = server("closed-proj").await;
    let general = server("general").await;
    let other = server("other").await;
    let source: Arc<dyn LayerSource> = Arc::new(ClosesOnProbe {
        first: Arc::clone(general.memory()),
        second: Arc::clone(other.memory()),
    });
    let handle = |name: &str| {
        LayerHandle::new(
            name,
            DEFAULT_LAYER_WEIGHT,
            DEFAULT_LAYER_MIN_SIMILARITY,
            Arc::downgrade(&source),
        )
    };
    let s = own.clone().with_layers(ServerLayers {
        handles: vec![handle("general"), handle("other")],
        authority: None,
    });
    let out = call_raw(
        &s,
        "lambo_record_action",
        action_args(
            "an act during close",
            "depends_on",
            &["00000000-0000-4000-8000-000000000002"],
            Some("general"),
        ),
    )
    .await;
    assert_eq!(out.is_error, Some(false), "{out:?}");
    let warnings = structured(&out)["warnings"].to_string();
    assert!(
        warnings.contains("layer general closed while this call was finishing")
            && warnings.contains("do not resend"),
        "{warnings}"
    );
    assert!(text_of(&out).contains("do not resend"));
    let receipt: ReceiptId = receipt_of(&out).parse().unwrap();
    let answer = general
        .memory()
        .pipeline()
        .wait(
            &AgentId::new("agent-a"),
            receipt,
            crate::writeq::RECEIPT_WAIT_MAX,
        )
        .await;
    assert_eq!(answer.tag(), "applied", "the admitted write still applies");
    assert!(contents(&general).contains(&"an act during close".to_string()));
    own.mem.close().await.expect("close");
    other.mem.close().await.expect("close");
    let _ = general.mem.close().await;
}
