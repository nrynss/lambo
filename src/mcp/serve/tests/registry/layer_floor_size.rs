//! #254: the layer-size warning, through the real registry. A serve logs it
//! when it has attached a *layer's* session that holds more concepts with
//! vectors than the 12-concept layer its default floor was chosen on: for
//! the layer and never for the session that reads it, from the layer's own
//! count of vectors, and not at all when the operator set the layer's floor.

use super::*;
use crate::mcp::serve::registry::{layer_size_warning, DetachReason};

const PROJECT: &str = "size-proj";
const GENERAL: &str = "general";
/// What every #254 line contains and nothing else in a serve's log does.
const MARK: &str = "concepts with vectors";

/// `[serve]` with `PROJECT` layered with `GENERAL`, plus `extra` TOML.
fn layers(extra: &str) -> crate::config::ServeLayers {
    let file = crate::LamboFile::from_toml_str(&format!(
        "[serve]\nsessions = [\"{PROJECT}\", \"{GENERAL}\"]\nlayers = [\"{GENERAL}\"]\n{extra}"
    ))
    .expect("parse");
    file.serve
        .resolve_layers(&file.serve.sessions)
        .expect("resolve")
}

/// The fixture embedder under the BGE-M3 reference contract's stamp: the
/// warning is decided from the stamped contract, and only a calibrated one
/// has a size to compare a layer with.
fn reference_backends() -> ResolvedBackends {
    let mut backends = backends_over(
        Box::new(crate::test_util::GraphRanked(
            crate::test_util::VectorSearchable(Arc::new(MemoryStore::new())),
        )),
        fast_config(1_000),
    );
    backends.embedding = EmbeddingContract {
        kind: "bge_m3".into(),
        model: None,
        dim: 1024,
    };
    backends
}

/// A `DetachSession` registry hosting both sessions with `layers`, both
/// attached (empty) and the pinned retry running.
async fn started(layers: crate::config::ServeLayers) -> Arc<SessionRegistry> {
    let backends = reference_backends();
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
    let registry = SessionRegistry::new(
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
            session_rps: 0,
            layers,
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
    );
    attach_or_hold(&registry, PROJECT).await;
    attach_or_hold(&registry, GENERAL).await;
    registry.mark_started();
    registry.spawn_retry_loop();
    registry
}

fn mem_of(registry: &SessionRegistry, id: &str) -> Arc<Memory> {
    registry
        .attached()
        .into_iter()
        .find(|s| s.id().as_str() == id)
        .unwrap_or_else(|| panic!("{id} attached"))
        .mem
        .clone()
}

/// Write `n` facts to `session`, one derive each (a concept's vector
/// carries its call's text, so facts of one call would sit close together),
/// and check each carries a vector.
async fn fill(registry: &Arc<SessionRegistry>, addr: SocketAddr, session: &str, n: usize) {
    let path = format!("/mcp/s/{session}");
    let (sid, _) = initialize(addr, &path).await;
    const WORDS: [&str; 16] = [
        "harbour", "lattice", "quorum", "saffron", "glacier", "turbine", "meadow", "cipher",
        "orchard", "basalt", "lantern", "monsoon", "velvet", "pylon", "estuary", "kiln",
    ];
    for word in WORDS.iter().take(n) {
        derive(addr, &path, &sid, &[&format!("{session} {word} {word}")]).await;
    }
    let stats = mem_of(registry, session).stats();
    assert_eq!(
        (stats.concept_count, stats.embedded_concepts),
        (n, n),
        "{session}: every fact is its own concept, with a vector"
    );
}

/// Detach pinned `session` and wait for the retry loop to attach it again
/// (through `admit_if`).
async fn re_attach(registry: &Arc<SessionRegistry>, session: &str) {
    registry.detach(session, DetachReason::Idle).await;
    let deadline = Instant::now() + PINNED_RETRY * 5;
    while registry.attached().len() < 2 {
        assert!(Instant::now() < deadline, "{session} was never re-attached");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn size_lines(logs: &crate::test_util::CapturedLogs) -> Vec<String> {
    logs.lines()
        .into_iter()
        .filter(|l| l.contains(MARK))
        .collect()
}

async fn close(registry: Arc<SessionRegistry>) {
    for session in registry.close_set().await {
        session.mem.close().await.expect("close");
    }
    registry.abort_tasks();
}

/// The registry logs the line when it attaches the layer, once, from the
/// layer's own count of vectors, and never for the session that reads the
/// layer, though that session holds more vectors than the layer does.
#[tokio::test]
async fn the_layers_attach_is_warned_of_and_the_consumers_is_not() {
    let (logs, _guard) = crate::test_util::capture_logs(tracing::Level::WARN);
    let registry = started(layers("")).await;
    let addr = serve_router(&registry, 32).await;
    // Empty at the first attach: nothing to say about either session.
    assert_eq!(size_lines(&logs), Vec::<String>::new());

    fill(&registry, addr, GENERAL, 13).await;
    fill(&registry, addr, PROJECT, 16).await;
    // The count is read at attach, not while the layer grows (documented).
    assert_eq!(size_lines(&logs), Vec::<String>::new());

    re_attach(&registry, GENERAL).await;
    let lines = size_lines(&logs);
    assert_eq!(lines.len(), 1, "one line for the layer's attach: {lines:?}");
    assert!(
        lines[0].contains(
            "layer \"general\" holds 13 concepts with vectors, 1.1 times the 12-concept layer \
             its floor 0.5 was chosen on"
        ),
        "{lines:?}"
    );
    assert!(lines[0].contains("WARN"), "{lines:?}");

    // The consumer's attach says nothing, at 16 vectors of its own.
    re_attach(&registry, PROJECT).await;
    let lines = size_lines(&logs);
    assert_eq!(lines.len(), 1, "no line for the consumer: {lines:?}");
    assert!(!lines[0].contains(PROJECT), "{lines:?}");

    close(registry).await;
}

/// A floor the operator set for the layer is their choice of the trade: the
/// same layer, re-attached, is not warned of.
#[tokio::test]
async fn a_layer_whose_floor_the_operator_set_is_not_warned_of() {
    let (logs, _guard) = crate::test_util::capture_logs(tracing::Level::WARN);
    let registry = started(layers(&format!(
        "[[serve.layer]]\nsession = \"{GENERAL}\"\nmin_similarity = 0.5\n"
    )))
    .await;
    let addr = serve_router(&registry, 32).await;
    fill(&registry, addr, GENERAL, 13).await;
    re_attach(&registry, GENERAL).await;
    assert_eq!(mem_of(&registry, GENERAL).stats().embedded_concepts, 13);
    assert_eq!(size_lines(&logs), Vec::<String>::new());
    close(registry).await;
}

/// The decision the registry logs, on its inputs: the layer's spec, the
/// session's stats and its contract. It counts vectors, not concepts (a
/// layer of 500 concepts of which 12 carry a vector is at the chosen size),
/// names only a layer, and respects a configured floor.
#[tokio::test]
async fn the_warning_counts_vectors_and_names_only_a_default_floored_layer() {
    let registry = started(layers("")).await;
    let mem = mem_of(&registry, GENERAL);
    let contract = mem.embedding_contract().clone();
    let config = mem.config().clone();
    let stats = |concepts: usize, embedded: usize| crate::MemoryStats {
        concept_count: concepts,
        embedded_concepts: embedded,
        ..mem.stats()
    };
    let default_floor = layers("");
    let warn = |layers: &crate::config::ServeLayers, session: &str, s: &crate::MemoryStats| {
        layer_size_warning(layers, session, s, &config, &contract)
    };

    // More concepts than the chosen size, but no more vectors: no warning.
    assert_eq!(warn(&default_floor, GENERAL, &stats(500, 12)), None);
    assert_eq!(warn(&default_floor, GENERAL, &stats(500, 0)), None);
    // One more vector, and the line gives the vectors, not the concepts.
    let line = warn(&default_floor, GENERAL, &stats(500, 13)).expect("13 vectors");
    assert!(line.contains("holds 13 concepts with vectors"), "{line}");
    assert!(!line.contains("500"), "{line}");

    // Never for a session that is not a layer, whatever it holds.
    assert_eq!(warn(&default_floor, PROJECT, &stats(5_000, 5_000)), None);
    // Never for a layer whose floor was set, at any value.
    for set in ["0.5", "0.72", "0.9"] {
        let configured = layers(&format!(
            "[[serve.layer]]\nsession = \"{GENERAL}\"\nmin_similarity = {set}\n"
        ));
        assert_eq!(warn(&configured, GENERAL, &stats(5_000, 5_000)), None);
    }
    // A weight alone leaves the floor the default one: still warned of.
    let weighted = layers(&format!(
        "[[serve.layer]]\nsession = \"{GENERAL}\"\nweight = 0.25\n"
    ));
    assert!(warn(&weighted, GENERAL, &stats(13, 13)).is_some());

    drop(mem);
    close(registry).await;
}
