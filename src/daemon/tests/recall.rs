//! Recall through the daemon: goldens, the recall cache and vector
//! degradation.

use super::*;

/// The P5 entry reproduces T5.3's golden context block end to end: same
/// fixture snapshot, same planted T4.3-shaped conflict entry, rescored
/// table, pinned clock — through `Daemon::recall` (the actual entry, not
/// the bespoke pipeline). Also proves cache hit + epoch invalidation
/// through the entry.
#[cfg(feature = "fixtures")]
#[tokio::test]
async fn recall_entry_reproduces_context_golden() {
    use crate::config::RecallWeights;
    use crate::daemon::score::rescore;
    use crate::fixtures;
    use crate::recall::cache::RecallCache;

    // T5.3's pinned clock: base + 60 minutes (its ts(60)).
    let now = Utc.timestamp_opt(1_752_000_000, 0).unwrap() + chrono::Duration::minutes(60);
    let snap = fixtures::load_snapshot("session-rest-api").unwrap();
    let graph = Arc::new(RwLock::new(Graph::from_snapshot(snap.clone()).unwrap()));
    let index = Arc::new(RwLock::new(InvertedIndex::from_snapshot(&snap)));
    let daemon = Daemon::new(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_secs(3600),
    )
    .with_clock(Arc::new(move || now))
    .with_index(index);
    daemon.scores.write().ranked = rescore(&graph.read(), &ScoringWeights::default());
    daemon.scores.write().epoch = graph.read().epoch();

    // Plant the T4.3-shaped conflict on "user schema" (001001), written
    // 11s before `now` (30s window) — identical to T5.3's golden test.
    let us = NodeId("f0000000-0000-4000-8000-000000001001".parse().unwrap());
    let agents = vec![AgentId::from("agent-a"), AgentId::from("agent-b")];
    let writer = AgentId::from("agent-a");
    let write_at = now - chrono::Duration::seconds(11);
    let entry = crate::daemon::hotlist::HotListEntry::new(
        us,
        Condition::Conflict,
        crate::daemon::hotlist::HotListPayload::Conflict {
            agents: agents.clone(),
            writer: writer.clone(),
            seconds_ago: 999, // stale sentinel: revalidate must rebuild
        },
        move |_, now| {
            let secs = (now - write_at).num_seconds();
            if (0..=30).contains(&secs) {
                Some(crate::daemon::hotlist::HotListPayload::Conflict {
                    agents: agents.clone(),
                    writer: writer.clone(),
                    seconds_ago: secs as u64,
                })
            } else {
                None
            }
        },
    );
    let _ = daemon.hot.write().insert(entry);

    let store = fixtures::load_store("session-rest-api").unwrap();
    let mut cache = RecallCache::new();
    let query = RecallQuery {
        query: "update user schema".into(),
        top_k: 5,
        max_tokens: 500,
        traversal_depth: 2,
    };
    let session = SessionId::from("session-rest-api");
    let golden = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/recall-context-golden.txt"
    ))
    .expect("golden context fixture present");

    let result = daemon
        .recall(
            &session,
            query.clone(),
            &store,
            None,
            RecallWeights::default(),
            &mut cache,
        )
        .await;
    assert_eq!(
        result.context, golden,
        "entry must reproduce the golden block"
    );
    assert_eq!(cache.len(), 1, "first call populates the cache");

    // Cache hit: identical second call does not grow the cache.
    let again = daemon
        .recall(
            &session,
            query.clone(),
            &store,
            None,
            RecallWeights::default(),
            &mut cache,
        )
        .await;
    assert_eq!(again.context, golden);
    assert_eq!(cache.len(), 1, "cache hit: no new key inserted");

    // Epoch invalidation: any mutation bumps the epoch -> miss -> new key.
    // The new interaction must link to the fixture's chain tail
    // (insert_interaction enforces previous_id = current tail).
    let tail = graph
        .read()
        .interactions()
        .max_by_key(|i| i.created_at)
        .expect("fixture has interactions")
        .id;
    graph
        .write()
        .insert_interaction(Interaction {
            event_time: None,
            id: NodeId::new(),
            session_id: session.clone(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: None,
            previous_id: Some(tail),
            created_at: now,
        })
        .unwrap();
    // The loop's rescore catches up to the new epoch within one tick; the
    // entry's cache guard only skips caching while scores lag, so once
    // caught up the new epoch key is stored.
    daemon.scores.write().ranked = rescore(&graph.read(), &ScoringWeights::default());
    daemon.scores.write().epoch = graph.read().epoch();
    let _ = daemon
        .recall(
            &session,
            query,
            &store,
            None,
            RecallWeights::default(),
            &mut cache,
        )
        .await;
    assert_eq!(
        cache.len(),
        2,
        "epoch bump invalidates: new key inserted on miss"
    );
}

/// A reservation transition (RAM-local: no Mutation kind exists) bumps
/// the epoch, so a same-query recall misses and re-renders the
/// reservation line (P5 phase-close finding).
#[cfg(feature = "fixtures")]
#[tokio::test]
async fn recall_reservation_transition_invalidates_cache_and_renders() {
    use crate::config::RecallWeights;
    use crate::daemon::score::rescore;
    use crate::fixtures;
    use crate::recall::cache::RecallCache;
    use crate::types::Reservation;

    let now = Utc.timestamp_opt(1_752_000_000, 0).unwrap() + chrono::Duration::minutes(60);
    let snap = fixtures::load_snapshot("session-rest-api").unwrap();
    let graph = Arc::new(RwLock::new(Graph::from_snapshot(snap.clone()).unwrap()));
    let index = Arc::new(RwLock::new(InvertedIndex::from_snapshot(&snap)));
    let daemon = Daemon::new(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_secs(3600),
    )
    .with_clock(Arc::new(move || now))
    .with_index(index);
    daemon.scores.write().ranked = rescore(&graph.read(), &ScoringWeights::default());
    daemon.scores.write().epoch = graph.read().epoch();

    let store = fixtures::load_store("session-rest-api").unwrap();
    let mut cache = RecallCache::new();
    let session = SessionId::from("session-rest-api");
    let query = RecallQuery {
        query: "update user schema".into(),
        top_k: 5,
        max_tokens: 500,
        traversal_depth: 2,
    };
    let _ = daemon
        .recall(
            &session,
            query.clone(),
            &store,
            None,
            RecallWeights::default(),
            &mut cache,
        )
        .await;
    assert_eq!(cache.len(), 1);

    // Reserve a node in the expanded set (user schema, 001001).
    let us = NodeId("f0000000-0000-4000-8000-000000001001".parse().unwrap());
    graph.write().set_reservation(Reservation {
        session_id: session.clone(),
        node_id: us,
        agent_id: AgentId::from("agent-a"),
        expires_at: now + chrono::Duration::seconds(60),
    });

    daemon.scores.write().ranked = rescore(&graph.read(), &ScoringWeights::default());
    daemon.scores.write().epoch = graph.read().epoch();
    let with_res = daemon
        .recall(
            &session,
            query,
            &store,
            None,
            RecallWeights::default(),
            &mut cache,
        )
        .await;
    assert_eq!(
        cache.len(),
        2,
        "reservation transition bumps epoch -> cache miss"
    );
    assert!(
        with_res.context.contains("Reserved by agent-a")
            || with_res
                .warnings
                .iter()
                .any(|w| w.contains("Reserved by agent-a")),
        "reservation line rendered; warnings: {:?}",
        with_res.warnings
    );
}

/// Cache hits re-render time-sensitive output: with a mutable clock and
/// no epoch change, a live conflict entry's age refreshes and a lapsed
/// window drops the warning line (spec §9 "conditions re-validated on
/// each recall()" — P5 phase-close finding).
#[cfg(feature = "fixtures")]
#[tokio::test]
async fn recall_cache_hit_rerenders_fresh_warning_lines() {
    use crate::config::RecallWeights;
    use crate::daemon::score::rescore;
    use crate::fixtures;
    use crate::recall::cache::RecallCache;
    use std::sync::Mutex;

    let base = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
    let clock_now = Arc::new(Mutex::new(base));
    let snap = fixtures::load_snapshot("session-rest-api").unwrap();
    let graph = Arc::new(RwLock::new(Graph::from_snapshot(snap.clone()).unwrap()));
    let index = Arc::new(RwLock::new(InvertedIndex::from_snapshot(&snap)));
    let daemon = Daemon::new(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_secs(3600),
    )
    .with_clock(Arc::new({
        let c = clock_now.clone();
        move || *c.lock().unwrap()
    }))
    .with_index(index);
    daemon.scores.write().ranked = rescore(&graph.read(), &ScoringWeights::default());
    daemon.scores.write().epoch = graph.read().epoch();

    // Live conflict entry on user schema: written 11s before `base`,
    // 30s window.
    let us = NodeId("f0000000-0000-4000-8000-000000001001".parse().unwrap());
    let agents = vec![AgentId::from("agent-a"), AgentId::from("agent-b")];
    let write_at = base - chrono::Duration::seconds(11);
    let entry = crate::daemon::hotlist::HotListEntry::new(
        us,
        Condition::Conflict,
        crate::daemon::hotlist::HotListPayload::Conflict {
            agents: agents.clone(),
            writer: AgentId::from("agent-a"),
            seconds_ago: 999, // stale sentinel: revalidate must rebuild
        },
        move |_, now| {
            let secs = (now - write_at).num_seconds();
            if (0..=30).contains(&secs) {
                Some(crate::daemon::hotlist::HotListPayload::Conflict {
                    agents: agents.clone(),
                    writer: AgentId::from("agent-a"),
                    seconds_ago: secs as u64,
                })
            } else {
                None
            }
        },
    );
    let _ = daemon.hot.write().insert(entry);

    let store = fixtures::load_store("session-rest-api").unwrap();
    let mut cache = RecallCache::new();
    let session = SessionId::from("session-rest-api");
    let query = RecallQuery {
        query: "update user schema".into(),
        top_k: 5,
        max_tokens: 500,
        traversal_depth: 2,
    };

    let first = daemon
        .recall(
            &session,
            query.clone(),
            &store,
            None,
            RecallWeights::default(),
            &mut cache,
        )
        .await;
    assert!(
        first.context.contains("wrote to it 11 seconds ago"),
        "age at read time: {}",
        first.context
    );
    assert_eq!(cache.len(), 1);

    // No epoch change; clock advances 5s -> cache HIT, age re-rendered.
    *clock_now.lock().unwrap() = base + chrono::Duration::seconds(5);
    let aged = daemon
        .recall(
            &session,
            query.clone(),
            &store,
            None,
            RecallWeights::default(),
            &mut cache,
        )
        .await;
    assert_eq!(cache.len(), 1, "same epoch -> cache hit, no new key");
    assert!(
        aged.context.contains("wrote to it 16 seconds ago"),
        "age refreshed on cache hit: {}",
        aged.context
    );

    // Window lapses (age 41s > 30s) -> warning line drops, still a hit.
    *clock_now.lock().unwrap() = base + chrono::Duration::seconds(30);
    let lapsed = daemon
        .recall(
            &session,
            query,
            &store,
            None,
            RecallWeights::default(),
            &mut cache,
        )
        .await;
    assert_eq!(cache.len(), 1, "same epoch -> cache hit");
    assert!(
        !lapsed.context.contains("wrote to it"),
        "lapsed entry's warning dropped: {}",
        lapsed.context
    );
}

/// The rescore-lag guard (phase-close P5-3): a compute whose daemon scores
/// lag the graph epoch is rendered but NOT cached; once the loop's
/// rescore catches up, the next call caches the fresh-epoch key (R2-1:
/// the skip branch had no direct test).
#[cfg(feature = "fixtures")]
#[tokio::test]
async fn recall_rescore_lag_guard_skips_cache_insert_while_scores_lag() {
    use crate::config::RecallWeights;
    use crate::daemon::score::rescore;
    use crate::fixtures;
    use crate::recall::cache::RecallCache;

    let now = Utc.timestamp_opt(1_752_000_000, 0).unwrap() + chrono::Duration::minutes(60);
    let snap = fixtures::load_snapshot("session-rest-api").unwrap();
    let graph = Arc::new(RwLock::new(Graph::from_snapshot(snap.clone()).unwrap()));
    let index = Arc::new(RwLock::new(InvertedIndex::from_snapshot(&snap)));
    let daemon = Daemon::new(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_secs(3600),
    )
    .with_clock(Arc::new(move || now))
    .with_index(index);
    daemon.scores.write().ranked = rescore(&graph.read(), &ScoringWeights::default());
    daemon.scores.write().epoch = graph.read().epoch();

    let store = fixtures::load_store("session-rest-api").unwrap();
    let mut cache = RecallCache::new();
    let session = SessionId::from("session-rest-api");
    let query = RecallQuery {
        query: "update user schema".into(),
        top_k: 5,
        max_tokens: 500,
        traversal_depth: 2,
    };

    let first = daemon
        .recall(
            &session,
            query.clone(),
            &store,
            None,
            RecallWeights::default(),
            &mut cache,
        )
        .await;
    assert!(!first.context.is_empty());
    assert_eq!(cache.len(), 1, "initial compute cached");

    // Mutation bumps the epoch; the loop's rescore has NOT caught up
    // (scores.epoch still the old one). The compute renders against the
    // lagged table but must NOT be cached under the new epoch key.
    let tail = graph
        .read()
        .interactions()
        .max_by_key(|i| i.created_at)
        .expect("fixture has interactions")
        .id;
    graph
        .write()
        .insert_interaction(Interaction {
            event_time: None,
            id: NodeId::new(),
            session_id: session.clone(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: None,
            previous_id: Some(tail),
            created_at: now,
        })
        .unwrap();
    let lagged = daemon
        .recall(
            &session,
            query.clone(),
            &store,
            None,
            RecallWeights::default(),
            &mut cache,
        )
        .await;
    assert_eq!(
        cache.len(),
        1,
        "lagged-scores compute is NOT cached (P5-3 guard)"
    );
    assert!(!lagged.context.is_empty(), "output still rendered");

    // Rescore catches up; the next call stores the fresh-epoch key.
    daemon.scores.write().ranked = rescore(&graph.read(), &ScoringWeights::default());
    daemon.scores.write().epoch = graph.read().epoch();
    let caught_up = daemon
        .recall(
            &session,
            query,
            &store,
            None,
            RecallWeights::default(),
            &mut cache,
        )
        .await;
    assert_eq!(
        cache.len(),
        2,
        "after rescore the fresh-epoch key is cached"
    );
    assert!(!caught_up.context.is_empty());
}

// P1-2 (GPT5.6sol): vector-dependent results are never cached or served
// from cache. An embedding=Some call must not populate the cache, and a
// later embedding=None call must not share that key's entry.
#[cfg(feature = "fixtures")]
#[tokio::test]
async fn recall_never_caches_vector_dependent_results() {
    use crate::config::RecallWeights;
    use crate::daemon::score::rescore;
    use crate::fixtures;
    use crate::recall::cache::RecallCache;

    let now = Utc.timestamp_opt(1_752_000_000, 0).unwrap() + chrono::Duration::minutes(60);
    let snap = fixtures::load_snapshot("session-rest-api").unwrap();
    let graph = Arc::new(RwLock::new(Graph::from_snapshot(snap.clone()).unwrap()));
    let index = Arc::new(RwLock::new(InvertedIndex::from_snapshot(&snap)));
    let daemon = Daemon::new(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_secs(3600),
    )
    .with_clock(Arc::new(move || now))
    .with_index(index);
    daemon.scores.write().ranked = rescore(&graph.read(), &ScoringWeights::default());
    daemon.scores.write().epoch = graph.read().epoch();

    let store = fixtures::load_store("session-rest-api").unwrap();
    let mut cache = RecallCache::new();
    let session = SessionId::from("session-rest-api");
    let query = RecallQuery {
        query: "update user schema".into(),
        top_k: 5,
        max_tokens: 500,
        traversal_depth: 2,
    };

    // embedding=Some (vector leg participates) -> NEVER cached.
    let emb = vec![0.1f32; 8];
    let contract = crate::types::EmbeddingContract {
        kind: "fixture".into(),
        model: None,
        dim: 8,
    };
    let _ = daemon
        .recall(
            &session,
            query.clone(),
            &store,
            Some((&emb, &contract)),
            RecallWeights::default(),
            &mut cache,
        )
        .await;
    assert_eq!(cache.len(), 0, "vector-dependent result must not be cached");

    // embedding=None (pure keyword+recent) -> cached.
    let _ = daemon
        .recall(
            &session,
            query.clone(),
            &store,
            None,
            RecallWeights::default(),
            &mut cache,
        )
        .await;
    assert_eq!(cache.len(), 1, "keyword+recent result cached");
}

// P2-6 (GPT5.6sol): without an inverted index, the independently gathered
// recent leg still yields candidates (only lexical lookup is lost).
#[cfg(feature = "fixtures")]
#[tokio::test]
async fn recall_without_index_keeps_recent_leg() {
    use crate::config::RecallWeights;
    use crate::daemon::score::rescore;
    use crate::fixtures;
    use crate::recall::cache::RecallCache;

    let now = Utc.timestamp_opt(1_752_000_000, 0).unwrap() + chrono::Duration::minutes(60);
    let snap = fixtures::load_snapshot("session-rest-api").unwrap();
    let graph = Arc::new(RwLock::new(Graph::from_snapshot(snap.clone()).unwrap()));
    // NO index installed.
    let daemon = Daemon::new(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_secs(3600),
    )
    .with_clock(Arc::new(move || now));
    daemon.scores.write().ranked = rescore(&graph.read(), &ScoringWeights::default());
    daemon.scores.write().epoch = graph.read().epoch();

    let store = fixtures::load_store("session-rest-api").unwrap();
    let mut cache = RecallCache::new();
    let session = SessionId::from("session-rest-api");
    let query = RecallQuery {
        query: "zzz".into(), // matches no keyword; recent leg is the only source
        top_k: 5,
        max_tokens: 500,
        traversal_depth: 2,
    };
    let result = daemon
        .recall(
            &session,
            query,
            &store,
            None,
            RecallWeights::default(),
            &mut cache,
        )
        .await;
    assert!(
        !result.hits.is_empty(),
        "no index must still surface recent-leg candidates (P2-6)"
    );
    assert!(
        result
            .warnings
            .iter()
            .any(|w| w.contains("no inverted index")),
        "the missing-index warning is reported"
    );
}

// P2-8 (GPT5.6sol): a caller session that differs from the graph's
// authoritative session is refused, never mixed.
#[cfg(feature = "fixtures")]
#[tokio::test]
async fn recall_rejects_mismatched_session() {
    use crate::config::RecallWeights;
    use crate::daemon::score::rescore;
    use crate::fixtures;

    let now = Utc.timestamp_opt(1_752_000_000, 0).unwrap() + chrono::Duration::minutes(60);
    let snap = fixtures::load_snapshot("session-rest-api").unwrap();
    let graph = Arc::new(RwLock::new(Graph::from_snapshot(snap.clone()).unwrap()));
    let daemon = Daemon::new(
        graph.clone(),
        ScoringWeights::default(),
        Duration::from_secs(3600),
    )
    .with_clock(Arc::new(move || now));
    daemon.scores.write().ranked = rescore(&graph.read(), &ScoringWeights::default());
    daemon.scores.write().epoch = graph.read().epoch();

    let store = fixtures::load_store("session-rest-api").unwrap();
    let mut cache = crate::recall::cache::RecallCache::new();
    let other = SessionId::from("session-drift"); // differs from graph's session
    let query = RecallQuery {
        query: "update user schema".into(),
        top_k: 5,
        max_tokens: 500,
        traversal_depth: 2,
    };
    let result = daemon
        .recall(
            &other,
            query,
            &store,
            None,
            RecallWeights::default(),
            &mut cache,
        )
        .await;
    assert!(
        result.hits.is_empty() && !result.warnings.is_empty(),
        "mismatched session is refused with a warning (P2-8)"
    );
    assert!(
        result
            .warnings
            .iter()
            .any(|w| w.contains("refusing to mix")),
        "refusal names the namespace mix"
    );
}

/// E2E-6: a contract-race `Invariant` from the checked vector read
/// mid-flight must surface a client-visible `vector_degraded` annotation
/// on the detailed result (both the CLI header and the portal's card /
/// verbatim views render `response_annotations`), instead of degrading to
/// a silent keyword-only recall. The vector leg stays fail-closed — the
/// annotation is explanation, not a fallback ranking.
#[tokio::test]
async fn gather_contract_race_annotates_vector_degraded() {
    use crate::config::RecallWeights;
    use crate::recall::cache::RecallCache;
    use crate::recall::detail::AnnotationKind;
    use crate::store::Capabilities;
    use crate::types::{EmbeddingContract, MutationBatch};
    use async_trait::async_trait;

    // The only trait items recall_detailed touches: the capability probe
    // and the checked read. Everything else is unreachable on this path.
    struct ContractRaceStore;
    #[async_trait]
    impl crate::store::GraphStore for ContractRaceStore {
        async fn init_schema(&self) -> Result<(), StoreError> {
            panic!("E2E-6 stub: init_schema")
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::VECTOR_SEARCH
        }
        fn vector_dimensions(&self) -> Option<usize> {
            Some(1024)
        }
        async fn flush(
            &self,
            _batch: &MutationBatch,
            _token: Option<u64>,
        ) -> Result<(), StoreError> {
            panic!("E2E-6 stub: flush")
        }
        async fn load_session(
            &self,
            _session: &SessionId,
        ) -> Result<crate::types::GraphSnapshot, StoreError> {
            panic!("E2E-6 stub: load_session")
        }
        async fn keyword_candidates(
            &self,
            _session: &SessionId,
            _tokens: &[String],
            _limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            panic!("E2E-6 stub: keyword_candidates")
        }
        async fn vector_candidates(
            &self,
            _session: &SessionId,
            _embedding: &[f32],
            _limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            panic!("E2E-6 stub: vector_candidates")
        }
        async fn vector_candidates_checked(
            &self,
            _session: &SessionId,
            _embedding: &[f32],
            _expected_contract: &EmbeddingContract,
            _limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            Err(StoreError::Invariant(
                "vector candidate lookup refused after embedding contract changed: \
                     durable kind=bge_m3 vs live kind=bge_m3"
                    .to_string(),
            ))
        }
        async fn blast_radius(
            &self,
            _session: &SessionId,
            _node: NodeId,
            _min_edge_age: Duration,
            _now: DateTime<Utc>,
        ) -> Result<u64, StoreError> {
            panic!("E2E-6 stub: blast_radius")
        }
        async fn interaction_span(
            &self,
            _session: &SessionId,
            _node: NodeId,
            _min_age: Duration,
            _now: DateTime<Utc>,
        ) -> Result<crate::types::InteractionSpan, StoreError> {
            panic!("E2E-6 stub: interaction_span")
        }
        async fn record_canonization(
            &self,
            _event: &crate::types::CanonizationEvent,
            _token: Option<u64>,
        ) -> Result<(), StoreError> {
            panic!("E2E-6 stub: record_canonization")
        }
        async fn acquire_lease(
            &self,
            _session: &SessionId,
            _holder: &crate::store::lease::LeaseHolder,
            _ttl: Duration,
        ) -> Result<crate::store::lease::LeaseOutcome, StoreError> {
            panic!("E2E-6 stub: acquire_lease")
        }
        async fn refresh_lease(
            &self,
            _session: &SessionId,
            _holder: &crate::store::lease::LeaseHolder,
            _ttl: Duration,
        ) -> Result<crate::store::lease::LeaseOutcome, StoreError> {
            panic!("E2E-6 stub: refresh_lease")
        }
        async fn release_lease(
            &self,
            _session: &SessionId,
            _holder: &crate::store::lease::LeaseHolder,
        ) -> Result<(), StoreError> {
            panic!("E2E-6 stub: release_lease")
        }
        async fn write_flush_stats(
            &self,
            _session: &SessionId,
            _stats: &crate::store::SessionFlushStats,
        ) -> Result<(), StoreError> {
            panic!("E2E-6 stub: write_flush_stats")
        }
        async fn read_flush_stats(
            &self,
            _session: &SessionId,
        ) -> Result<Option<crate::store::SessionFlushStats>, StoreError> {
            panic!("E2E-6 stub: read_flush_stats")
        }
    }

    let (graph, _cid) = locked_graph_with_one_concept();
    let daemon = Daemon::new(graph, ScoringWeights::default(), Duration::from_secs(3600));
    let mut cache = RecallCache::new();
    let query = RecallQuery {
        query: "user schema".into(),
        top_k: 5,
        max_tokens: 500,
        traversal_depth: 2,
    };
    let contract = EmbeddingContract {
        kind: "fixture".into(),
        model: None,
        dim: 1024,
    };
    let result = daemon
        .recall_detailed(
            &sid(),
            query,
            &ContractRaceStore,
            Some((&vec![0.0; 1024], &contract)),
            RecallWeights::default(),
            &mut cache,
        )
        .await;

    let degraded: Vec<&crate::recall::detail::Annotation> = result
        .response_annotations
        .iter()
        .filter(|a| a.kind == AnnotationKind::VectorDegraded)
        .collect();
    assert_eq!(degraded.len(), 1, "exactly one vector_degraded annotation");
    assert!(
        degraded[0].text.contains("embedding contract changed"),
        "the annotation names the refusal: {}",
        degraded[0].text
    );
    assert!(
        degraded[0].text.contains("keyword-only"),
        "the annotation states the fail-closed outcome: {}",
        degraded[0].text
    );
    assert!(
        result.response_annotations.len() == 1,
        "no other response annotations on this path: {:?}",
        result.response_annotations
    );
}
