//! Builder and attach: Level B construction, the live embedding contract
//! and the attach line.

use super::*;

/// Formatted log text with the capturing subscriber's ANSI escapes removed.
///
/// `tracing_subscriber::fmt` styles the field NAME, so a raw capture reads
/// `\x1b[3mpromotion_policy\x1b[0m\x1b[2m=\x1b[0mSwarm` and a plain
/// `contains("promotion_policy=Swarm")` never matches even when the field is
/// there. Asserting on the stripped text keeps the assertion about the log
/// line rather than about the formatter's styling.
pub(super) fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // CSI: ESC '[' … final byte in 0x40..=0x7e.
        if chars.next() != Some('[') {
            continue;
        }
        for b in chars.by_ref() {
            if ('\u{40}'..='\u{7e}').contains(&b) {
                break;
            }
        }
    }
    out
}

/// **P2-b.** The attach line names the live promotion policy, beside
/// `match_strategy`.
///
/// This is the operator's only in-process answer to "which policy is this
/// run using?", and it matters most when the configuration is *valid*: a
/// `lambo.toml` saying `Solo` under a stale exported
/// `LAMBO_PROMOTION_POLICY=Swarm` runs `Swarm`, correctly and silently.
/// Without this field the only remaining evidence is days of absent
/// canonization events — the exact diagnosis dead-end the selector exists
/// to end. Both policies are asserted, because a hardcoded literal would
/// pass a one-policy test.
#[tokio::test]
async fn the_attach_line_names_the_live_promotion_policy() {
    for policy in crate::canon::PromotionPolicy::ALL {
        let (logs, _guard) = capture_logs(tracing::Level::INFO);
        let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let mem = Memory::builder()
            .session(format!("attach-{}", policy.as_str().to_ascii_lowercase()))
            .agent("agent-a")
            .config(Config {
                promotion_policy: policy,
                ..Config::default()
            })
            .flush_interval(Duration::from_secs(3_600))
            .store(store)
            .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
            .embedding_contract(contract("fixture", 1024))
            .build()
            .await
            .expect("build");

        // The capturing subscriber writes ANSI, which lands *between* the
        // field name and its `=`, so assert on the plain text.
        let logged = strip_ansi(&logs.contents());
        assert!(
            logged.contains("Memory session attached"),
            "the attach line must be captured: {logged}"
        );
        assert!(
            logged.contains(&format!("promotion_policy={}", policy.as_str())),
            "the attach line must name the live policy: {logged}"
        );
        // Its neighbour is the reason it belongs here: `match_strategy` is
        // on this line so an operator can see which enum-valued knob won,
        // and `promotion_policy` is the other one.
        assert!(
            logged.contains("match_strategy="),
            "still beside match_strategy: {logged}"
        );
        mem.close().await.expect("close");
    }
}

// -- build / Level B ----------------------------------------------------

#[tokio::test]
async fn build_requires_session_agent_store_embedder_and_contract() {
    let err = Memory::builder().build().await.unwrap_err();
    assert!(err.to_string().contains("session"), "{err}");

    let err = Memory::builder().session("s").build().await.unwrap_err();
    assert!(err.to_string().contains("agent"), "{err}");

    let err = Memory::builder()
        .session("s")
        .agent("a")
        .build()
        .await
        .unwrap_err();
    assert!(err.to_string().contains("store"), "{err}");

    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let err = Memory::builder()
        .session("s")
        .agent("a")
        .store(store.clone())
        .build()
        .await
        .unwrap_err();
    assert!(err.to_string().contains("embedder"), "{err}");

    let err = Memory::builder()
        .session("s")
        .agent("a")
        .store(store)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .build()
        .await
        .unwrap_err();
    assert!(err.to_string().contains("embedding_contract"), "{err}");
}

#[tokio::test]
async fn fresh_session_is_stamped_with_the_live_embedding_contract() {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let mem = memory_on(store, "stamp-me").await;
    let stamped = mem.graph().read().embedding().cloned();
    assert_eq!(stamped, Some(contract("fixture", 1024)));
    mem.close().await.unwrap();
}

/// STORE-1 / Level B: attaching with a different embedder kind, model or
/// dim than the session was written with must refuse, not mix vector spaces.
#[tokio::test]
async fn session_attach_rejects_embedder_kind_model_and_dim_mismatch() {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());

    // Write a session stamped `fixture` / dim 1024.
    let mem = memory_on(store.clone(), "contracted").await;
    mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
        .await
        .unwrap();
    mem.close().await.unwrap();

    let attach = |c: EmbeddingContract| {
        let store = store.clone();
        async move {
            Memory::builder()
                .session("contracted")
                .agent("agent-b")
                .store(store)
                .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
                .embedding_contract(c)
                .build()
                .await
        }
    };

    let err = attach(contract("bge_m3", 1024)).await.unwrap_err();
    assert!(err.to_string().contains("kind"), "{err}");

    let err = attach(contract("fixture", 512)).await.unwrap_err();
    assert!(err.to_string().contains("dim"), "{err}");

    let err = attach(EmbeddingContract {
        kind: "fixture".into(),
        model: Some("other.gguf".into()),
        dim: 1024,
    })
    .await
    .unwrap_err();
    assert!(err.to_string().contains("model"), "{err}");

    // The matching contract still attaches.
    let ok = attach(contract("fixture", 1024)).await.unwrap();
    ok.close().await.unwrap();
}

#[tokio::test]
async fn h1_same_width_model_change_refuses_by_default_and_explicit_override_persists() {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let old = EmbeddingContract {
        kind: "fixture".into(),
        model: Some("fixture-model-v1".into()),
        dim: 1024,
    };
    let live = EmbeddingContract {
        kind: "fixture".into(),
        model: Some("fixture-model-renamed".into()),
        dim: 1024,
    };

    let first = Memory::builder()
        .session("h1-model-rename")
        .agent("operator")
        .store(store.clone())
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(old)
        .flush_interval(Duration::from_secs(3_600))
        .build()
        .await
        .unwrap();
    first.close().await.unwrap();

    let attach = |allow| {
        let store = store.clone();
        let live = live.clone();
        async move {
            Memory::builder()
                .session("h1-model-rename")
                .agent("operator")
                .store(store)
                .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
                .embedding_contract(live)
                .allow_embedding_mismatch(allow)
                .flush_interval(Duration::from_secs(3_600))
                .build()
                .await
        }
    };

    let err = attach(false).await.unwrap_err();
    let text = err.to_string();
    assert!(text.contains("fixture-model-v1"), "{text}");
    assert!(text.contains("fixture-model-renamed"), "{text}");
    assert!(text.contains("--allow-embedding-mismatch"), "{text}");

    let migrated = attach(true).await.unwrap();
    assert_eq!(migrated.embedding_contract(), &live);
    migrated.close().await.unwrap();
    let stored = store
        .load_session(&SessionId::new("h1-model-rename"))
        .await
        .unwrap();
    assert_eq!(stored.embedding, Some(live));
}

/// A degenerate cadence must fail `build()` with a `Config` error BEFORE
/// the single-writer lease is ever acquired, AND without leaking a held
/// lease. `validate()` runs at the runtime entry (memory.rs build) on the
/// merged config ahead of `store.acquire_lease`, so a zero cadence can
/// neither reach a spawned `tokio::interval` nor leave a lease held. The
/// follow-up build on the same store/session/agent with a valid config
/// must therefore succeed: a validate-after-lease regression would have
/// leaked a held lease and wedged it (`acquire_lease` -> `Held`).
#[tokio::test]
async fn build_rejects_zero_cadence_before_acquiring_the_lease() {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let bad = Config {
        gc_interval: 0,
        ..Config::default()
    };
    let err = Memory::builder()
        .session("bad-cadence")
        .agent("agent-a")
        .config(bad)
        .store(store.clone())
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .build()
        .await
        .expect_err("a zero cadence must fail build()");
    // A Config error — not Conflict/Store/Held — is consistent with validate()
    // rejecting before the lease; the second build below is what proves no
    // lease was leaked.
    assert!(
        matches!(&err, LamboError::Config(_)),
        "must fail closed as a Config error before the lease, got: {err:?}"
    );

    // Follow-up build on the same store/session/agent with a VALID config.
    // If validate()-after-lease had leaked a held lease for this session
    // ("bad-cadence"), acquire_lease would return Held and wedge this
    // build — so its success genuinely proves no lease was leaked.
    let second = Memory::builder()
        .session("bad-cadence")
        .agent("agent-a")
        .config(Config::default())
        .store(store)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .build()
        .await
        .expect("a valid second build must succeed; a leaked lease would wedge it");
    second.close().await.unwrap();
}
