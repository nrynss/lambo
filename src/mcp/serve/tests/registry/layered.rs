//! #33 PR 3: layered recall through the real registry, router and guards:
//! two pinned sessions in one serve over one store and the fixture
//! embedder, one layered with the other.

use super::*;
use crate::mcp::serve::registry::{DetachReason, ForcedState};
use crate::mcp::server::layers::{LayerDown, LayerRead, LayerSource};

const PROJECT: &str = "lay-proj";
const GENERAL: &str = "general";
const PROJECT_FACT: &str = "worktree cleanup removes the target directory of a merged branch";
const GENERAL_RULING: &str = "worktree branches are always rebased onto origin main first";

/// `[serve]` with `PROJECT` layered with `GENERAL`, resolved as a serve
/// pinning both.
fn project_layered_with_general() -> crate::config::ServeLayers {
    let file = crate::LamboFile::from_toml_str(&format!(
        "[serve]\nsessions = [\"{PROJECT}\", \"{GENERAL}\"]\nlayers = [\"{GENERAL}\"]\n"
    ))
    .expect("parse");
    let layers = file
        .serve
        .resolve_layers(&file.serve.sessions)
        .expect("resolve");
    assert_eq!(layers.stack(PROJECT)[0].session, GENERAL);
    assert!(layers.stack(GENERAL).is_empty());
    layers
}

/// A `DetachSession` registry hosting both sessions with the layer stack,
/// nothing attached yet.
fn layered_registry(backends: ResolvedBackends) -> Arc<SessionRegistry> {
    layered_registry_at(backends, 0)
}

/// [`layered_registry`] whose sessions each have `session_rps` (0: none).
fn layered_registry_at(backends: ResolvedBackends, session_rps: u32) -> Arc<SessionRegistry> {
    let opts = ServeOptions::new(PROJECT, "agent-a");
    let early = EarlyShutdown::unarmed();
    let store_cfg = backends.store_cfg.clone();
    let template = super::builder::serve_builder(
        &opts,
        backends,
        None,
        None,
        early.clone(),
        Some(crate::writeq::EmbedderCalibration::new()),
    );
    SessionRegistry::new(
        vec![PROJECT.to_string(), GENERAL.to_string()],
        Some(PROJECT.to_string()),
        LeaseLossPolicy::DetachSession,
        Some(SessionAttacher {
            template,
            store_cfg,
            ledger: None,
            max_sessions: 32,
            host_check: HostCheck::Loopback,
            agent: "agent-a".into(),
            session_rps,
            layers: project_layered_with_general(),
            // The implicit `local` credential the router's guard resolves
            // (`serve_router`), so a layered call's per-layer check runs on
            // the real grant from the request's parts.
            authority: Some(authority_for(&{
                let mut opts = ServeOptions::new(PROJECT, "agent-a");
                opts.sessions = vec![PROJECT.to_string(), GENERAL.to_string()];
                opts.transport = Transport::Http;
                opts
            })),
        }),
        early,
        RegistryBounds::pinned_only(),
        None,
    )
}

async fn started_registry() -> Arc<SessionRegistry> {
    let registry = layered_registry(backends_over(
        Box::new(crate::test_util::GraphRanked(
            crate::test_util::VectorSearchable(Arc::new(MemoryStore::new())),
        )),
        fast_config(1_000),
    ));
    attach_or_hold(&registry, PROJECT).await;
    attach_or_hold(&registry, GENERAL).await;
    registry.mark_started();
    registry
}

fn path(session: &str) -> String {
    format!("/mcp/s/{session}")
}

async fn recall(
    addr: SocketAddr,
    session: &str,
    sid: &str,
    args: serde_json::Value,
) -> serde_json::Value {
    let mut args = args;
    args["agent_id"] = serde_json::json!("agent-a");
    call(addr, &path(session), sid, "lambo_recall", args).await
}

fn sessions_of(result: &serde_json::Value) -> Vec<Option<String>> {
    result["structuredContent"]["hits"]
        .as_array()
        .expect("hits")
        .iter()
        .map(|h| {
            h.get("session")
                .and_then(|s| s.as_str())
                .map(str::to_string)
        })
        .collect()
}

fn session_of(registry: &SessionRegistry, id: &str) -> Arc<super::session::AttachedSession> {
    registry
        .attached()
        .into_iter()
        .find(|s| s.id().as_str() == id)
        .unwrap_or_else(|| panic!("{id} attached"))
}

/// The design's end-to-end shape: `/mcp/s/lay-proj` reads its own session
/// and `general`, every hit naming its session; `general` itself is
/// unlayered and its hits name none; `scope` reads one session; `get_info`
/// names the layer on the layered session only.
#[tokio::test]
async fn two_sessions_in_one_serve_answer_a_layered_recall_end_to_end() {
    let registry = started_registry().await;
    let addr = serve_router(&registry, 32).await;
    let (proj, init_proj) = initialize(addr, &path(PROJECT)).await;
    let (general, init_general) = initialize(addr, &path(GENERAL)).await;
    let proj_info = init_proj["instructions"].as_str().unwrap();
    assert!(
        proj_info.contains("also searches its shared layers: general"),
        "{proj_info}"
    );
    assert!(
        !init_general["instructions"]
            .as_str()
            .unwrap()
            .contains("shared layers"),
        "a layer is unlayered"
    );

    derive(addr, &path(PROJECT), &proj, &[PROJECT_FACT]).await;
    derive(addr, &path(GENERAL), &general, &[GENERAL_RULING]).await;
    let live = registry
        .live_layer(GENERAL, LayerRead::Merged)
        .expect("live general");
    crate::test_util::seed_layer_match(&live.mem, GENERAL_RULING, "worktree branches").await;
    drop(live);

    let merged = recall(
        addr,
        PROJECT,
        &proj,
        serde_json::json!({"query": "worktree branches"}),
    )
    .await;
    let sessions = sessions_of(&merged);
    assert!(sessions.iter().all(Option::is_some), "{merged}");
    assert!(sessions.contains(&Some(PROJECT.into())), "{merged}");
    assert!(sessions.contains(&Some(GENERAL.into())), "{merged}");
    let context = merged["structuredContent"]["context"].as_str().unwrap();
    assert!(context.contains(", from general)"), "{context}");

    let own = recall(
        addr,
        GENERAL,
        &general,
        serde_json::json!({"query": "worktree"}),
    )
    .await;
    let sessions = sessions_of(&own);
    assert!(!sessions.is_empty());
    assert!(sessions.iter().all(Option::is_none), "{own}");
    assert!(
        !own.to_string().contains(PROJECT_FACT),
        "general never reads the project"
    );

    let scoped = recall(
        addr,
        PROJECT,
        &proj,
        serde_json::json!({"query": "worktree", "scope": GENERAL}),
    )
    .await;
    let sessions = sessions_of(&scoped);
    assert!(!sessions.is_empty());
    assert!(
        sessions.iter().all(|s| s.as_deref() == Some(GENERAL)),
        "{scoped}"
    );

    for session in registry.close_set().await {
        session.mem.close().await.expect("close");
    }
}

/// Losing `general`'s lease detaches `general` alone: the project keeps
/// serving, its recall answers from itself with one `layer_unavailable`
/// line, and a scope naming the layer is a retryable refusal.
#[tokio::test]
async fn losing_the_layers_lease_detaches_only_the_layer_and_the_project_degrades() {
    let (logs, _guard) = crate::test_util::capture_logs(tracing::Level::INFO);
    let registry = started_registry().await;
    let addr = serve_router(&registry, 32).await;
    let (proj, _) = initialize(addr, &path(PROJECT)).await;
    derive(addr, &path(PROJECT), &proj, &[PROJECT_FACT]).await;

    let lost = session_of(&registry, GENERAL);
    lost.mem.simulate_lease_loss_to("another-writer@host#9");
    drop(lost);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !logs
        .lines()
        .iter()
        .any(|l| l.contains("session detach finished") && l.contains(GENERAL))
    {
        assert!(Instant::now() < deadline, "no detach: {:?}", logs.lines());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(registry.attached().len(), 1);
    assert_eq!(registry.attached()[0].id().as_str(), PROJECT);
    let refused = http(addr, "POST", &path(GENERAL), None, "{}").await;
    assert_eq!(refused.status, 503, "{}", refused.body);

    let degraded = recall(
        addr,
        PROJECT,
        &proj,
        serde_json::json!({"query": "worktree"}),
    )
    .await;
    assert_eq!(degraded["isError"], serde_json::json!(false), "{degraded}");
    let sessions = sessions_of(&degraded);
    assert!(!sessions.is_empty());
    assert!(
        sessions.iter().all(|s| s.as_deref() == Some(PROJECT)),
        "{degraded}"
    );
    let warnings = degraded["structuredContent"]["warnings"].to_string();
    assert!(
        warnings.contains("layer general is unavailable; results are from lay-proj only"),
        "{warnings}"
    );

    let scoped = recall(
        addr,
        PROJECT,
        &proj,
        serde_json::json!({"query": "worktree", "scope": GENERAL}),
    )
    .await;
    assert_eq!(scoped["isError"], serde_json::json!(true), "{scoped}");
    assert!(
        scoped
            .to_string()
            .contains("layer general is unavailable right now"),
        "{scoped}"
    );

    for session in registry.close_set().await {
        session.mem.close().await.expect("close");
    }
}

/// The handle never caches the layer: after `general` is detached and
/// re-attached (a new `Memory`), the project's handle finds the new one,
/// and while it is away the handle finds nothing. And it holds the
/// registry weakly: once the registry is gone, a server still alive finds
/// no layer, and nothing kept the registry alive (no `Arc` cycle).
#[tokio::test]
async fn a_layer_handle_follows_a_re_attached_layer_and_holds_no_strong_reference() {
    let registry = started_registry().await;
    registry.spawn_retry_loop();
    let project_server = session_of(&registry, PROJECT).server.clone();
    let handle = project_server.layers().handles[0].clone();
    // Weak, so this test does not hold the layer's old `Memory` (the
    // re-attach waits for the old handle to go).
    let first = {
        let first = session_of(&registry, GENERAL).mem.clone();
        assert!(Arc::ptr_eq(
            &handle.live(LayerRead::Merged).expect("live").mem,
            &first
        ));
        Arc::downgrade(&first)
    };

    registry.detach(GENERAL, DetachReason::Idle).await;
    assert!(
        handle.live(LayerRead::Merged).is_err(),
        "a detached layer is not served"
    );
    let deadline = Instant::now() + PINNED_RETRY * 3;
    while registry.attached().len() < 2 {
        assert!(Instant::now() < deadline, "general was never re-attached");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let second = session_of(&registry, GENERAL).mem.clone();
    assert!(first.upgrade().is_none(), "the old Memory is gone");
    assert!(Arc::ptr_eq(
        &handle.live(LayerRead::Merged).expect("live again").mem,
        &second
    ));
    drop(second);

    let weak = Arc::downgrade(&registry);
    for session in registry.close_set().await {
        session.mem.close().await.expect("close");
    }
    registry.abort_tasks();
    drop(registry);
    let deadline = Instant::now() + Duration::from_secs(5);
    while weak.strong_count() > 0 {
        assert!(
            Instant::now() < deadline,
            "something kept the registry alive"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        handle.live(LayerRead::Merged).is_err(),
        "no registry, no layer"
    );
    drop(project_server);
}

/// Review L1: a failed or erased layer says so (and an erased one is
/// warned of at start), so a caller is not told to retry what will not
/// come back; held elsewhere, detaching and attaching stay transient.
#[tokio::test]
async fn a_layer_lookup_says_why_the_layer_is_not_served() {
    let registry = started_registry().await;
    let source: &dyn LayerSource = registry.as_ref();
    for (state, down) in [
        (ForcedState::HeldElsewhere, LayerDown::Transient),
        (ForcedState::Detaching, LayerDown::Transient),
        (ForcedState::Erasing, LayerDown::Transient),
        (ForcedState::Failed, LayerDown::Failed),
        (ForcedState::Erased, LayerDown::Erased),
    ] {
        let live = session_of(&registry, GENERAL);
        registry.force_state(GENERAL, state);
        assert_eq!(
            source.live_layer(GENERAL, LayerRead::Direct).err(),
            Some(down),
            "{state:?}"
        );
        let warnings = crate::mcp::serve::layers::erased_layer_warnings(
            &project_layered_with_general(),
            |l| {
                matches!(
                    source.live_layer(l, LayerRead::Merged),
                    Err(LayerDown::Erased)
                )
            },
        );
        assert_eq!(
            warnings.len(),
            usize::from(down == LayerDown::Erased),
            "{warnings:?}"
        );
        if let Some(w) = warnings.first() {
            assert!(
                w.contains("layer \"general\" of \"lay-proj\" is erased"),
                "{w}"
            );
        }
        registry.insert_live(live);
    }
    for session in registry.close_set().await {
        session.mem.close().await.expect("close");
    }
}

/// Review L3: a direct read of a layer (`scope`) draws the layer's own
/// `per_session_rps` token; a merged read does not.
#[tokio::test]
async fn a_direct_layer_read_draws_the_layers_rate_and_a_merged_one_does_not() {
    let registry = layered_registry_at(
        backends_over(Box::new(MemoryStore::new()), fast_config(1_000)),
        1,
    );
    attach_or_hold(&registry, PROJECT).await;
    attach_or_hold(&registry, GENERAL).await;
    let source: &dyn LayerSource = registry.as_ref();
    // The burst is twice the rate: two direct reads pass, the third waits.
    let mut direct = Vec::new();
    for _ in 0..3 {
        direct.push(source.live_layer(GENERAL, LayerRead::Direct).err());
    }
    assert_eq!(direct, [None, None, Some(LayerDown::RateLimited)]);
    for _ in 0..10 {
        assert!(source.live_layer(GENERAL, LayerRead::Merged).is_ok());
    }
    for session in registry.close_set().await {
        session.mem.close().await.expect("close");
    }
}

/// #33 PR 4, end to end through the real registry, router and guards: a
/// write to `/mcp/s/lay-proj` with `scope = "general"` lands in `general`
/// alone, its receipt is found by `lambo_stats` at the project's path
/// (routed to `general`'s pipeline by epoch), get_info says how to write
/// to the layer, and an unknown scope is refused.
#[tokio::test]
async fn a_scoped_write_lands_in_the_layer_end_to_end() {
    let registry = started_registry().await;
    let addr = serve_router(&registry, 32).await;
    let (proj, init) = initialize(addr, &path(PROJECT)).await;
    let info = init["instructions"].as_str().unwrap();
    assert!(
        info.contains("Writes to different sessions have no ordering between them"),
        "{info}"
    );
    assert!(
        info.contains("with scope = \"general\""),
        "get_info names the write scope: {info}"
    );

    let ack = call(
        addr,
        &path(PROJECT),
        &proj,
        "lambo_derive",
        serde_json::json!({"agent_id": "agent-a", "scope": GENERAL,
                           "concepts": [{"content": GENERAL_RULING, "concept_type": "logic"}]}),
    )
    .await;
    assert_eq!(ack["isError"], serde_json::json!(false), "{ack}");
    assert_eq!(
        ack["structuredContent"]["session"],
        serde_json::json!(GENERAL)
    );
    let receipt = ack["structuredContent"]["receipt"]
        .as_str()
        .unwrap()
        .to_string();
    let waited = call(
        addr,
        &path(PROJECT),
        &proj,
        "lambo_stats",
        serde_json::json!({"agent_id": "agent-a", "receipt": receipt, "wait_ms": 10_000}),
    )
    .await;
    assert_eq!(
        waited["structuredContent"]["receipt"]["state"],
        serde_json::json!("applied"),
        "{waited}"
    );
    let has = |id: &str| {
        session_of(&registry, id)
            .mem
            .graph()
            .read()
            .concepts()
            .any(|c| c.content == GENERAL_RULING)
    };
    assert!(has(GENERAL) && !has(PROJECT));

    let refused = call(
        addr,
        &path(PROJECT),
        &proj,
        "lambo_record_action",
        serde_json::json!({"agent_id": "agent-a", "scope": "elsewhere", "action": "x"}),
    )
    .await;
    assert_eq!(refused["isError"], serde_json::json!(true), "{refused}");
    assert!(refused
        .to_string()
        .contains("one of its layers: lay-proj, general"));

    // Review L2: general's own server knows it is a layer, so a direct
    // write there refuses a relative path too; the project's does not.
    let (general_sid, _) = initialize(addr, &path(GENERAL)).await;
    let direct = call(
        addr,
        &path(GENERAL),
        &general_sid,
        "lambo_record_action",
        serde_json::json!({"agent_id": "agent-a", "action": "edited", "modifies": ["README.md"]}),
    )
    .await;
    assert_eq!(direct["isError"], serde_json::json!(true), "{direct}");
    assert!(
        direct
            .to_string()
            .contains("is a relative path, refused in scope"),
        "{direct}"
    );
    let own = call(
        addr,
        &path(PROJECT),
        &proj,
        "lambo_record_action",
        serde_json::json!({"agent_id": "agent-a", "action": "edited", "modifies": ["README.md"]}),
    )
    .await;
    assert_eq!(own["isError"], serde_json::json!(false), "{own}");

    for session in registry.close_set().await {
        session.mem.close().await.expect("close");
    }
}

/// #33 PR 4: a scoped write draws one token from the layer's own
/// `per_session_rps` (as a request to `/mcp/s/general` would); draining
/// receipts and probing references draw none. Driven on the project's
/// server directly, so no request to the project spends the project's own
/// bucket.
#[tokio::test]
async fn a_scoped_write_draws_the_layers_rate() {
    let registry = layered_registry_at(
        backends_over(Box::new(MemoryStore::new()), fast_config(1_000)),
        1,
    );
    attach_or_hold(&registry, PROJECT).await;
    attach_or_hold(&registry, GENERAL).await;
    let server = session_of(&registry, PROJECT).server.clone();
    let out = server
        .derive_impl(crate::mcp::server::DeriveParams {
            repo_scope: None,
            agent_id: "agent-a".into(),
            concepts: vec![crate::mcp::server::WireConcept {
                content: GENERAL_RULING.into(),
                concept_type: crate::mcp::server::WireConceptType::Logic,
                depends_on: None,
                against: None,
            }],
            scope: Some(GENERAL.into()),
            ..Default::default()
        })
        .await;
    assert_eq!(out.is_error, Some(false), "{out:?}");
    let source: &dyn LayerSource = registry.as_ref();
    for read in [LayerRead::Receipts, LayerRead::Probe, LayerRead::Merged] {
        for _ in 0..5 {
            assert!(source.live_layer(GENERAL, read).is_ok(), "{read:?}");
        }
    }
    // The burst is twice the rate of 1: the write drew one token, so one
    // direct read is left and the next is refused.
    assert!(source.live_layer(GENERAL, LayerRead::Direct).is_ok());
    assert_eq!(
        source.live_layer(GENERAL, LayerRead::Direct).err(),
        Some(LayerDown::RateLimited)
    );
    for session in registry.close_set().await {
        session.mem.close().await.expect("close");
    }
}
