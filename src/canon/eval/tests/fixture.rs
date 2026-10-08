use super::*;
use crate::config::ScoringWeights;
use crate::daemon::score::rescore;
use crate::store::GraphStore;

fn rest_sid() -> SessionId {
    SessionId::from("session-rest-api")
}

/// Injected clock for the `session-rest-api` fixture (F8): every
/// planted timestamp is 2026-08-10T09:00–09:55Z, so the cycle's `now`
/// must sit *after* the session — the store adapters age their cutoffs
/// against the caller's clock, and the module-level `ts()` predates the
/// fixture by a year (it would cut every structural edge away).
fn fixture_now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 10, 10, 0, 0).unwrap()
}

fn rewind_canonicals(snap: &mut crate::types::GraphSnapshot) {
    for c in snap.concepts.iter_mut() {
        if c.canonization_status == CanonizationStatus::Canonical {
            c.canonization_status = CanonizationStatus::None;
            c.blast_radius = None;
        }
    }
}

fn find_id(graph: &Graph, content: &str) -> NodeId {
    graph
        .concepts()
        .find(|c| c.content == content)
        .unwrap_or_else(|| panic!("{content} present"))
        .id
}

fn hops_for<'a>(
    events: impl IntoIterator<Item = &'a CanonizationEvent>,
    id: NodeId,
) -> Vec<(CanonizationStatus, CanonizationStatus)> {
    events
        .into_iter()
        .filter(|e| e.node_id == id)
        .map(|e| (e.from_status, e.to_status))
        .collect()
}

#[tokio::test]
async fn rest_api_user_schema_progresses_three_hops_with_audit() {
    let mut snap = crate::fixtures::load_snapshot("session-rest-api").unwrap();
    rewind_canonicals(&mut snap);
    let graph = Graph::from_snapshot(snap.clone()).unwrap();
    let store = crate::store::MemoryStore::new();
    store.seed(snap).unwrap();

    let us = find_id(&graph, "user schema");
    assert_eq!(status_of(&graph, us), CanonizationStatus::None);

    let scores = ScoreTable {
        epoch: graph.epoch(),
        ranked: rescore(&graph, &ScoringWeights::default()),
    };
    let graph = RwLock::new(graph);
    let mut p = params();
    p.min_peer_count = crate::Config::default().canonization_min_peer_count;

    let (tx, mut rx) = crate::daemon::events::event_channel();
    let mut ev = Evaluator::new();
    let mut all_committed = Vec::new();
    for _ in 0..3 {
        let outcome = eval_cycle(&mut ev, &graph, &store, &scores, &tx, &p, fixture_now())
            .await
            .unwrap();
        all_committed.extend(outcome.transitions().cloned());
    }

    assert_eq!(status_of(&graph.read(), us), CanonizationStatus::Canonical);
    match graph.read().node(us) {
        Some(Node::Concept(c)) => {
            assert_eq!(c.blast_radius, Some(8), "Stage 3 must stamp measured blast");
            assert!(c.last_demotion_time.is_none());
        }
        other => panic!("user schema must be a concept, got {other:?}"),
    }

    let graph_events = graph.read().canonization_events().to_vec();
    let us_hops = hops_for(&graph_events, us);
    assert_eq!(
        us_hops,
        vec![
            (CanonizationStatus::None, CanonizationStatus::Candidate),
            (CanonizationStatus::Candidate, CanonizationStatus::Venerable),
            (CanonizationStatus::Venerable, CanonizationStatus::Canonical),
        ],
        "one row per hop; a skipped audit would drop a pair: {us_hops:?}"
    );
    assert_eq!(
        hops_for(&all_committed, us),
        us_hops,
        "outcome transitions must match the in-graph audit"
    );
    let store_hops = hops_for(
        &store
            .load_session(&rest_sid())
            .await
            .unwrap()
            .canonization_events,
        us,
    );
    assert_eq!(store_hops, us_hops, "store audit must have one row per hop");

    let emitted = {
        let mut out = Vec::new();
        while let Ok(DaemonEvent::Canonized { event }) = rx.try_recv() {
            out.push(event);
        }
        out
    };
    assert_eq!(
        hops_for(&emitted, us),
        us_hops,
        "emit_canonized must fire once per hop"
    );
}

#[tokio::test]
async fn rest_api_api_layer_reaches_venerable_never_canonical() {
    let mut snap = crate::fixtures::load_snapshot("session-rest-api").unwrap();
    let api_id = snap
        .concepts
        .iter()
        .find(|c| c.content == "api layer")
        .expect("api layer present")
        .id;
    {
        let api = snap
            .concepts
            .iter_mut()
            .find(|c| c.id == api_id)
            .expect("api layer present");
        // Start at Candidate so Stage 2 is the first hop; blast 1
        // must refuse Canonical.
        api.canonization_status = CanonizationStatus::Candidate;
    }
    let graph = Graph::from_snapshot(snap.clone()).unwrap();
    let store = crate::store::MemoryStore::new();
    store.seed(snap).unwrap();
    let scores = ScoreTable {
        epoch: graph.epoch(),
        ranked: rescore(&graph, &ScoringWeights::default()),
    };
    let graph = RwLock::new(graph);
    let (tx, _rx) = crate::daemon::events::event_channel();
    let mut ev = Evaluator::new();
    for _ in 0..3 {
        eval_cycle(
            &mut ev,
            &graph,
            &store,
            &scores,
            &tx,
            &params(),
            fixture_now(),
        )
        .await
        .unwrap();
    }
    assert_eq!(
        status_of(&graph.read(), api_id),
        CanonizationStatus::Venerable
    );
    let graph_events = graph.read().canonization_events().to_vec();
    let hops = hops_for(&graph_events, api_id);
    assert!(
        hops.iter().any(|&(f, t)| f == CanonizationStatus::Candidate
            && t == CanonizationStatus::Venerable),
        "api layer must hop to Venerable: {hops:?}"
    );
    assert!(
        hops.iter()
            .all(|&(_, t)| t != CanonizationStatus::Canonical),
        "api layer blast=1 must never become Canonical: {hops:?}"
    );
}

/// Exit criterion "same test green against SQLite once T3.6 lands":
/// the stage predicates and eval cycle are store-agnostic; running the
/// three-hop progression against SqliteStore proves the SQL structural
/// queries (blast_radius, interaction_span) yield the same verdict.
#[cfg(feature = "store-sqlite")]
#[tokio::test]
async fn sqlite_three_hop_progression_matches_memory() {
    use crate::store::{GraphStore, SqliteStore};

    let mut snap = crate::fixtures::load_snapshot("session-rest-api").unwrap();
    rewind_canonicals(&mut snap);
    let graph = Graph::from_snapshot(snap.clone()).unwrap();
    let store = SqliteStore::connect("sqlite::memory:").unwrap();
    store.init_schema().await.unwrap();
    store.seed(&snap).await.unwrap();

    let us = find_id(&graph, "user schema");
    assert_eq!(status_of(&graph, us), CanonizationStatus::None);

    let scores = ScoreTable {
        epoch: graph.epoch(),
        ranked: rescore(&graph, &ScoringWeights::default()),
    };
    let graph = RwLock::new(graph);
    let mut p = params();
    p.min_peer_count = crate::Config::default().canonization_min_peer_count;

    let (tx, _rx) = crate::daemon::events::event_channel();
    let mut ev = Evaluator::new();
    for i in 0..3 {
        // Advance the clock per cycle as production does, so each hop
        // gets a distinct occurred_at and the SQL audit orders by it.
        let now = fixture_now() + chrono::Duration::seconds(60 * i as i64);
        eval_cycle(&mut ev, &graph, &store, &scores, &tx, &p, now)
            .await
            .unwrap();
    }
    assert_eq!(status_of(&graph.read(), us), CanonizationStatus::Canonical);

    let graph_events = graph.read().canonization_events().to_vec();
    let us_hops = hops_for(&graph_events, us);
    assert_eq!(
        us_hops,
        vec![
            (CanonizationStatus::None, CanonizationStatus::Candidate),
            (CanonizationStatus::Candidate, CanonizationStatus::Venerable),
            (CanonizationStatus::Venerable, CanonizationStatus::Canonical),
        ],
        "SQLite structural queries must yield the same progression: {us_hops:?}"
    );

    let reloaded = store.load_session(&rest_sid()).await.unwrap();
    assert_eq!(
        hops_for(&reloaded.canonization_events, us),
        us_hops,
        "SQLite canonization_events must match the in-graph audit"
    );
}
/// Rewrite every `session_id` on a fixture snapshot to `new_sid`, so a
/// live adapter test runs in its own namespace instead of the shared
/// `session-rest-api` one (whose rows persist on the CockroachDB
/// cluster and would otherwise break the conformance roundtrip check).
#[cfg(feature = "store-cockroach")]
fn rest_to_sid(snap: &mut crate::types::GraphSnapshot, new_sid: SessionId) {
    snap.session_id = new_sid.clone();
    for i in snap.interactions.iter_mut() {
        i.session_id = new_sid.clone();
    }
    for c in snap.concepts.iter_mut() {
        c.session_id = new_sid.clone();
    }
    for e in snap.edges.iter_mut() {
        e.session_id = new_sid.clone();
    }
    for s in snap.synonyms.iter_mut() {
        s.session_id = new_sid.clone();
    }
    for r in snap.reservations.iter_mut() {
        r.session_id = new_sid.clone();
    }
    for ev in snap.canonization_events.iter_mut() {
        ev.session_id = new_sid.clone();
    }
}

/// Live CockroachDB parity: the same three-hop progression, run
/// against the durable CockroachDB adapter. This exercises
/// CockroachDB's `blast_radius` / `interaction_span` / `record_canonization`
/// as the canonization loop's store, proving the SQL structural queries
/// yield the same verdict as MemoryStore. `#[ignore]`d: without
/// `LAMBO_COCKROACH_DSN` this must report as ignored, never skip-as-green.
#[cfg(feature = "store-cockroach")]
#[tokio::test]
#[ignore = "requires LAMBO_COCKROACH_DSN (run live via -- --ignored)"]
async fn cockroach_three_hop_progression_matches_memory() {
    use crate::store::cockroach::CockroachStore;
    use crate::store::{GraphStore, StoreConfig};

    let Some(dsn) = crate::store::StoreConfig::dsn_from_env() else {
        eprintln!(
            "SKIP cockroach_three_hop_progression_matches_memory: LAMBO_COCKROACH_DSN not set"
        );
        return;
    };
    // Isolated namespace so re-runs against the persistent cluster
    // never collide with the conformance suite's `session-rest-api`
    // roundtrip assertion.
    let unique = SessionId::from(format!("conformance-canoneval-{}", Uuid::new_v4()));
    let mut snap = crate::fixtures::load_snapshot("session-rest-api").unwrap();
    rewind_canonicals(&mut snap);
    rest_to_sid(&mut snap, unique.clone());
    let graph = Graph::from_snapshot(snap.clone()).unwrap();
    let store = CockroachStore::new(StoreConfig {
        kind: crate::store::StoreKind::Cockroach,
        dsn: Some(dsn),
        path: None,
        vector_dim: None,
    })
    .unwrap();
    store.init_schema().await.unwrap();
    store.seed(&snap).await.unwrap();

    let us = find_id(&graph, "user schema");
    assert_eq!(status_of(&graph, us), CanonizationStatus::None);

    let scores = ScoreTable {
        epoch: graph.epoch(),
        ranked: rescore(&graph, &ScoringWeights::default()),
    };
    let graph = RwLock::new(graph);
    let mut p = params();
    p.min_peer_count = crate::Config::default().canonization_min_peer_count;

    let (tx, _rx) = crate::daemon::events::event_channel();
    let mut ev = Evaluator::new();
    for i in 0..3 {
        let now = fixture_now() + chrono::Duration::seconds(60 * i as i64);
        eval_cycle(&mut ev, &graph, &store, &scores, &tx, &p, now)
            .await
            .unwrap();
    }
    assert_eq!(status_of(&graph.read(), us), CanonizationStatus::Canonical);

    let graph_events = graph.read().canonization_events().to_vec();
    let us_hops = hops_for(&graph_events, us);
    assert_eq!(
        us_hops,
        vec![
            (CanonizationStatus::None, CanonizationStatus::Candidate),
            (CanonizationStatus::Candidate, CanonizationStatus::Venerable),
            (CanonizationStatus::Venerable, CanonizationStatus::Canonical),
        ],
        "CockroachDB structural queries must yield the same progression: {us_hops:?}"
    );

    let reloaded = store.load_session(&unique).await.unwrap();
    assert_eq!(
        hops_for(&reloaded.canonization_events, us),
        us_hops,
        "CockroachDB canonization_events must match the in-graph audit"
    );
}
