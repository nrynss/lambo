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
    use crate::recall::cache::SharedRecallCache;

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
    let cache = SharedRecallCache::new();
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
            &cache,
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
            &cache,
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
            &cache,
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
    use crate::recall::cache::SharedRecallCache;
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
    let cache = SharedRecallCache::new();
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
            &cache,
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
            &cache,
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
    use crate::recall::cache::SharedRecallCache;
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
    let cache = SharedRecallCache::new();
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
            &cache,
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
            &cache,
        )
        .await;
    assert_eq!(cache.len(), 1, "same epoch -> cache hit, no new key");
    assert!(
        aged.context.contains("wrote to it 16 seconds ago"),
        "age refreshed on cache hit: {}",
        aged.context
    );
    assert!(
        daemon.hot_list().read().contains(us),
        "a live entry stays on the hot list"
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
            &cache,
        )
        .await;
    assert_eq!(cache.len(), 1, "same epoch -> cache hit");
    assert!(
        !lapsed.context.contains("wrote to it"),
        "lapsed entry's warning dropped: {}",
        lapsed.context
    );
    // The recall path itself evicts, through `HotList::revalidate_members`:
    // dropping the line alone would also pass for a read that rebuilds
    // payloads without evicting.
    assert!(
        !daemon.hot_list().read().contains(us),
        "the lapsed entry must be evicted from the hot list by recall"
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
    use crate::recall::cache::SharedRecallCache;

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
    let cache = SharedRecallCache::new();
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
            &cache,
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
            &cache,
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
            &cache,
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
    use crate::recall::cache::SharedRecallCache;

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
    let cache = SharedRecallCache::new();
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
            &cache,
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
            &cache,
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
    use crate::recall::cache::SharedRecallCache;

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
    let cache = SharedRecallCache::new();
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
            &cache,
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
    let cache = crate::recall::cache::SharedRecallCache::new();
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
            &cache,
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

use crate::store::Capabilities;
use crate::types::{EmbeddingContract, MutationBatch};
use async_trait::async_trait;

/// A `VECTOR_SEARCH` store with only the items `recall_detailed` touches:
/// the capability probe and the checked read, which refuses with E2E-6's
/// contract race when `race` is set and finds nothing otherwise. Everything
/// else is unreachable on this path.
struct StubVectorStore {
    race: bool,
}
#[async_trait]
impl crate::store::GraphStore for StubVectorStore {
    async fn init_schema(&self) -> Result<(), StoreError> {
        panic!("stub: init_schema")
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::VECTOR_SEARCH
    }
    fn vector_dimensions(&self) -> Option<usize> {
        Some(1024)
    }
    async fn flush(&self, _batch: &MutationBatch, _token: Option<u64>) -> Result<(), StoreError> {
        panic!("stub: flush")
    }
    async fn load_session(
        &self,
        _session: &SessionId,
    ) -> Result<crate::types::GraphSnapshot, StoreError> {
        panic!("stub: load_session")
    }
    async fn keyword_candidates(
        &self,
        _session: &SessionId,
        _tokens: &[String],
        _limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        panic!("stub: keyword_candidates")
    }
    async fn vector_candidates(
        &self,
        _session: &SessionId,
        _embedding: &[f32],
        _limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        panic!("stub: vector_candidates")
    }
    async fn vector_candidates_checked(
        &self,
        _session: &SessionId,
        _embedding: &[f32],
        _expected_contract: &EmbeddingContract,
        _limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        if !self.race {
            return Ok(Vec::new());
        }
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
        panic!("stub: blast_radius")
    }
    async fn interaction_span(
        &self,
        _session: &SessionId,
        _node: NodeId,
        _min_age: Duration,
        _now: DateTime<Utc>,
    ) -> Result<crate::types::InteractionSpan, StoreError> {
        panic!("stub: interaction_span")
    }
    async fn interaction_span_with_window(
        &self,
        _session: &SessionId,
        _node: NodeId,
        _min_age: Duration,
        _now: DateTime<Utc>,
        _window: Duration,
    ) -> Result<crate::types::InteractionSpan, StoreError> {
        panic!("stub: interaction_span_with_window")
    }
    async fn record_canonization(
        &self,
        _event: &crate::types::CanonizationEvent,
        _token: Option<u64>,
    ) -> Result<(), StoreError> {
        panic!("stub: record_canonization")
    }
    async fn acquire_lease(
        &self,
        _session: &SessionId,
        _holder: &crate::store::lease::LeaseHolder,
        _ttl: Duration,
    ) -> Result<crate::store::lease::LeaseOutcome, StoreError> {
        panic!("stub: acquire_lease")
    }
    async fn refresh_lease(
        &self,
        _session: &SessionId,
        _holder: &crate::store::lease::LeaseHolder,
        _ttl: Duration,
    ) -> Result<crate::store::lease::LeaseOutcome, StoreError> {
        panic!("stub: refresh_lease")
    }
    async fn release_lease(
        &self,
        _session: &SessionId,
        _holder: &crate::store::lease::LeaseHolder,
    ) -> Result<(), StoreError> {
        panic!("stub: release_lease")
    }
    async fn write_flush_stats(
        &self,
        _session: &SessionId,
        _stats: &crate::store::SessionFlushStats,
    ) -> Result<(), StoreError> {
        panic!("stub: write_flush_stats")
    }
    async fn read_flush_stats(
        &self,
        _session: &SessionId,
    ) -> Result<Option<crate::store::SessionFlushStats>, StoreError> {
        panic!("stub: read_flush_stats")
    }
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
    use crate::recall::cache::SharedRecallCache;
    use crate::recall::detail::AnnotationKind;

    let (graph, _cid) = locked_graph_with_one_concept();
    let daemon = Daemon::new(graph, ScoringWeights::default(), Duration::from_secs(3600));
    let cache = SharedRecallCache::new();
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
            &StubVectorStore { race: true },
            Some((&vec![0.0; 1024], &contract)),
            RecallWeights::default(),
            &cache,
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
    assert!(
        result.warnings.contains(&degraded[0].text),
        "the caller's warnings carry the same line: {:?}",
        result.warnings
    );
}

/// #87 through `Daemon::recall_detailed`: the recent leg scores at the
/// calibrated value only when the vector leg runs under a contract with a
/// built-in row and `calibration` is on. Every other case keeps BGE-M3's
/// `RECENT_SCORE`: a contract without a row, `calibration = "off"`, no query
/// embedding, and a vector read that failed.
#[tokio::test]
async fn recent_leg_follows_the_vector_contracts_calibration() {
    use crate::config::RecallWeights;
    use crate::embed::calibration::{self, CalibrationMode};
    use crate::recall::cache::SharedRecallCache;
    use crate::recall::candidates::RECENT_SCORE;

    let eg2 = calibration::BUILTIN[0];
    let eg2_contract = EmbeddingContract {
        kind: eg2.kind.into(),
        model: Some(eg2.model.into()),
        dim: eg2.dim,
    };
    let other = EmbeddingContract {
        kind: "bge_m3".into(),
        model: None,
        dim: 1024,
    };
    let recent_of = |mode: CalibrationMode, contract: Option<EmbeddingContract>, race: bool| async move {
        let (graph, cid) = locked_graph_with_one_concept();
        let config = crate::Config {
            calibration: mode,
            ..crate::Config::default()
        };
        let daemon = Daemon::from_config(graph, &config);
        let query = RecallQuery {
            // No index and no keyword: the recent leg is the only source.
            query: "zzz".into(),
            top_k: 5,
            max_tokens: 500,
            traversal_depth: 0,
        };
        let vector = vec![0.0; contract.as_ref().map_or(1, |c| c.dim)];
        let result = daemon
            .recall_detailed(
                &sid(),
                query,
                &StubVectorStore { race },
                contract.as_ref().map(|c| (vector.as_slice(), c)),
                RecallWeights::default(),
                &SharedRecallCache::new(),
            )
            .await;
        result.legs.get(&cid).and_then(|l| l.recent)
    };

    assert_eq!(
        recent_of(CalibrationMode::Builtin, Some(eg2_contract.clone()), false).await,
        Some(eg2.recent_score)
    );
    assert!(eg2.recent_score > RECENT_SCORE);
    for (mode, contract, race) in [
        (CalibrationMode::Off, Some(eg2_contract.clone()), false),
        (CalibrationMode::Builtin, Some(other), false),
        (CalibrationMode::Builtin, None, false),
        (CalibrationMode::Builtin, Some(eg2_contract), true),
    ] {
        assert_eq!(
            recent_of(mode, contract.clone(), race).await,
            Some(RECENT_SCORE),
            "{mode} {contract:?} race={race}"
        );
    }
}

/// #19(c) end to end through `Daemon::recall_detailed`: a newer
/// contradiction of the fixture's Canonical "user schema" renders the
/// contradiction line on it, force-includes it, and pulls the contradicting
/// concept into the result (it is not even in the keyword index) with its
/// companion line. Pinned by `fixtures/recall-context-contradiction-golden.txt`.
#[cfg(feature = "fixtures")]
#[tokio::test]
async fn recall_renders_a_contradiction_and_pulls_in_its_source() {
    use crate::config::RecallWeights;
    use crate::daemon::score::rescore;
    use crate::fixtures;
    use crate::recall::cache::SharedRecallCache;

    // After the fixture's last write (2026-08-10T09:55:00Z).
    let now = Utc.with_ymd_and_hms(2026, 8, 10, 10, 30, 0).unwrap();
    let snap = fixtures::load_snapshot("session-rest-api").unwrap();
    let session = SessionId::from("session-rest-api");
    let us = NodeId("f0000000-0000-4000-8000-000000001001".parse().unwrap());
    let mut g = Graph::from_snapshot(snap.clone()).unwrap();
    let tail = g
        .interactions()
        .max_by_key(|i| i.created_at)
        .expect("fixture has interactions")
        .id;
    let declared = now - chrono::Duration::minutes(2);
    let ix = NodeId("f0000000-0000-4000-8000-00000000c001".parse().unwrap());
    g.insert_interaction(Interaction {
        event_time: None,
        id: ix,
        session_id: session.clone(),
        agent_id: AgentId::from("agent-c"),
        prompt_text: None,
        previous_id: Some(tail),
        created_at: declared,
    })
    .unwrap();
    let by = NodeId("f0000000-0000-4000-8000-00000000c002".parse().unwrap());
    g.insert_concept(
        Concept {
            id: by,
            session_id: session.clone(),
            content: "accounts replaced the users table".into(),
            canonical_key: "accounts replaced the users table".into(),
            concept_type: ConceptType::Observation,
            origin_interaction: ix,
            origin_agent: AgentId::from("agent-c"),
            created_at: declared,
            access_count: 0,
            last_accessed: None,
            gc_survived: 0,
            canonization_status: CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: None,
            embedding: None,
            human_confirmed: 0,
            embedding_source: None,
            chunk_group_id: None,
        },
        ix,
    )
    .unwrap();
    g.upsert_edge(Edge {
        event_time: None,
        id: NodeId("f0000000-0000-4000-8000-00000000c003".parse().unwrap()),
        session_id: session.clone(),
        source: by,
        target: us,
        edge_type: EdgeType::Contradicts,
        weight: 1.0,
        reinforcements: 1,
        created_at: declared,
        last_reinforced: declared,
    })
    .unwrap();
    let graph = Arc::new(RwLock::new(g));
    // The fixture's own index: the new concept is not in it.
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
    let hits = crate::daemon::contradiction::insert_contradictions(
        &mut daemon.hot.write(),
        &graph.read(),
        Duration::from_secs(60),
        now,
    );
    assert_eq!(hits.len(), 1, "user schema is contradicted");

    let store = fixtures::load_store("session-rest-api").unwrap();
    let cache = SharedRecallCache::new();
    let query = RecallQuery {
        query: "update user schema".into(),
        top_k: 2,
        max_tokens: 500,
        traversal_depth: 2,
    };
    let result = daemon
        .recall_detailed(
            &session,
            query,
            &store,
            None,
            RecallWeights::default(),
            &cache,
        )
        .await;

    let golden = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/recall-context-contradiction-golden.txt"
    ))
    .expect("contradiction golden present");
    assert_eq!(result.context, golden, "context:\n{}", result.context);

    let kinds = |id: NodeId| -> Vec<AnnotationKind> {
        result
            .detailed
            .iter()
            .find(|h| h.node_id == id)
            .map(|h| h.annotations.iter().map(|a| a.kind).collect())
            .unwrap_or_default()
    };
    assert!(kinds(us).contains(&AnnotationKind::Contradiction));
    assert_eq!(kinds(by), vec![AnnotationKind::Contradicts]);
    // Two ranked hits, plus the force-included contradicted hit and its
    // companion, neither of which takes a top_k slot.
    assert_eq!(result.hits.len(), 4);
    assert!(result
        .warnings
        .iter()
        .any(|w| w.starts_with("⚠ Contradicted at") && w.contains("agent-c")));
}
