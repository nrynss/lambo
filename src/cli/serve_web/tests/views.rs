//! Store-call accounting for the portal's reads (#4 PR 1).
//!
//! A [`LoadCounting`] store wraps the shared in-RAM store and counts every
//! `load_session` and `preflight_schema`, so a test can say exactly how many
//! full session loads a request costs.

use super::*;
use std::sync::atomic::AtomicBool;

/// [`Shared`], counting `load_session` and `preflight_schema` calls. `fail`
/// makes every load answer a backend error; `delay` holds each load open so
/// concurrent requests provably overlap it.
#[derive(Clone)]
struct LoadCounting {
    inner: Shared,
    loads: Arc<AtomicUsize>,
    preflights: Arc<AtomicUsize>,
    fail: Arc<AtomicBool>,
    delay: Duration,
}

impl LoadCounting {
    fn new(store: Arc<MemoryStore>) -> Self {
        Self {
            inner: Shared(store),
            loads: Arc::new(AtomicUsize::new(0)),
            preflights: Arc::new(AtomicUsize::new(0)),
            fail: Arc::new(AtomicBool::new(false)),
            delay: Duration::ZERO,
        }
    }

    fn loads(&self) -> usize {
        self.loads.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl GraphStore for LoadCounting {
    async fn init_schema(&self) -> Result<(), StoreError> {
        self.inner.init_schema().await
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    async fn preflight_schema(&self) -> Result<(), StoreError> {
        self.preflights.fetch_add(1, Ordering::SeqCst);
        self.inner.preflight_schema().await
    }
    fn vector_dimensions(&self) -> Option<usize> {
        self.inner.vector_dimensions()
    }
    async fn flush(&self, batch: &MutationBatch, token: Option<u64>) -> Result<(), StoreError> {
        self.inner.flush(batch, token).await
    }
    async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        if self.fail.load(Ordering::SeqCst) {
            return Err(StoreError::Backend("load refused by the test store".into()));
        }
        self.inner.load_session(session).await
    }
    async fn keyword_candidates(
        &self,
        session: &SessionId,
        tokens: &[String],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.inner.keyword_candidates(session, tokens, limit).await
    }
    async fn vector_candidates(
        &self,
        session: &SessionId,
        embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.inner
            .vector_candidates(session, embedding, limit)
            .await
    }
    async fn vector_candidates_checked(
        &self,
        session: &SessionId,
        embedding: &[f32],
        expected_contract: &EmbeddingContract,
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.inner
            .vector_candidates_checked(session, embedding, expected_contract, limit)
            .await
    }
    async fn blast_radius(
        &self,
        session: &SessionId,
        node: NodeId,
        min_edge_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<u64, StoreError> {
        self.inner
            .blast_radius(session, node, min_edge_age, now)
            .await
    }
    async fn interaction_span(
        &self,
        session: &SessionId,
        node: NodeId,
        min_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<crate::types::InteractionSpan, StoreError> {
        self.inner
            .interaction_span(session, node, min_age, now)
            .await
    }
    async fn record_canonization(
        &self,
        event: &CanonizationEvent,
        token: Option<u64>,
    ) -> Result<(), StoreError> {
        self.inner.record_canonization(event, token).await
    }
    async fn acquire_lease(
        &self,
        session: &SessionId,
        holder: &crate::store::lease::LeaseHolder,
        ttl: Duration,
    ) -> Result<crate::store::lease::LeaseOutcome, StoreError> {
        self.inner.acquire_lease(session, holder, ttl).await
    }
    async fn refresh_lease(
        &self,
        session: &SessionId,
        holder: &crate::store::lease::LeaseHolder,
        ttl: Duration,
    ) -> Result<crate::store::lease::LeaseOutcome, StoreError> {
        self.inner.refresh_lease(session, holder, ttl).await
    }
    async fn release_lease(
        &self,
        session: &SessionId,
        holder: &crate::store::lease::LeaseHolder,
    ) -> Result<(), StoreError> {
        self.inner.release_lease(session, holder).await
    }
    async fn write_flush_stats(
        &self,
        session: &SessionId,
        stats: &crate::store::SessionFlushStats,
    ) -> Result<(), StoreError> {
        self.inner.write_flush_stats(session, stats).await
    }
    async fn read_flush_stats(
        &self,
        session: &SessionId,
    ) -> Result<Option<crate::store::SessionFlushStats>, StoreError> {
        self.inner.read_flush_stats(session).await
    }
}

/// The full session loads one request to each data route costs, measured
/// on a fresh server per route so no route's load can serve another's.
async fn loads_per_route(session: &str) -> Vec<(&'static str, usize)> {
    let store = seed(session).await;
    let mut out = Vec::new();
    for path in [
        "/api/pulse",
        "/api/stats",
        "/api/events",
        "/api/session",
        "/api/graph",
        "/api/inspect?focus=user%20schema",
        "/api/recall?q=user%20schema",
    ] {
        let counting = LoadCounting::new(store.clone());
        let state = state_from_backends(
            backends_with_store(Box::new(counting.clone())),
            session,
            None,
        );
        let (addr, handle) = spawn(state).await;
        let r = request(addr, "GET", path).await;
        assert_eq!(r.status, 200, "GET {path}: {}", r.body);
        out.push((path, counting.loads()));
        handle.abort();
    }
    out
}

/// What each route costs in full session loads. `/api/pulse`, the route
/// every open tab polls every 1.5 s, used to load the whole session twice
/// (once for the event tail, once more for the counts); `/api/stats` too.
/// One load now carries both.
#[tokio::test]
async fn each_data_route_costs_the_measured_number_of_session_loads() {
    let loads = loads_per_route("t4-loads").await;
    assert_eq!(
        loads,
        vec![
            ("/api/pulse", 1),
            ("/api/stats", 1),
            ("/api/events", 1),
            ("/api/session", 1),
            ("/api/graph", 1),
            ("/api/inspect?focus=user%20schema", 1),
            ("/api/recall?q=user%20schema", 1),
        ]
    );
}

/// Serve `session` from `store` through a [`LoadCounting`] wrapper with the
/// given `[web]` bounds; the wrapper's counters stay readable.
async fn serve_counting(
    counting: &LoadCounting,
    session: &str,
    web: &crate::config::WebConfig,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    spawn(state_with_web(
        backends_with_store(Box::new(counting.clone())),
        session,
        None,
        web,
    ))
    .await
}

fn web_ttl_ms(ms: u64) -> crate::config::WebConfig {
    crate::config::WebConfig {
        view_ttl_ms: Some(ms),
        ..Default::default()
    }
}

/// What the page fetches when it opens, plus a recall: one view serves it
/// all, so one load inside the TTL however many routes are hit.
#[tokio::test]
async fn every_route_inside_one_ttl_shares_one_load() {
    let counting = LoadCounting::new(seed("t4-page").await);
    let (addr, handle) = serve_counting(&counting, "t4-page", &web_ttl_ms(60_000)).await;
    for path in [
        "/api/session",
        "/api/pulse?since=0",
        "/api/graph",
        "/api/events",
        "/api/stats",
        "/api/inspect?focus=user%20schema",
        "/api/recall?q=user%20schema",
        "/api/pulse?since=3",
    ] {
        let r = request(addr, "GET", path).await;
        assert_eq!(r.status, 200, "GET {path}: {}", r.body);
    }
    assert_eq!(counting.loads(), 1, "one view serves every route");
    handle.abort();
}

/// N tabs polling at once into a stale or unloaded session cause one load
/// (single-flight), and the next poll inside the TTL causes none.
#[tokio::test]
async fn concurrent_pulses_inside_one_ttl_make_one_load() {
    let mut counting = LoadCounting::new(seed("t4-burst").await);
    counting.delay = Duration::from_millis(50);
    let (addr, handle) = serve_counting(&counting, "t4-burst", &web_ttl_ms(60_000)).await;

    let polls: Vec<_> = (0..8)
        .map(|_| tokio::spawn(async move { request(addr, "GET", "/api/pulse?since=0").await }))
        .collect();
    for poll in polls {
        let r = poll.await.expect("poll task");
        assert_eq!(r.status, 200, "{}", r.body);
        let body: serde_json::Value = serde_json::from_str(&r.body).expect("json");
        assert_eq!(body["events"]["total"], 3, "{body}");
    }
    assert_eq!(counting.loads(), 1, "eight concurrent pulses, one load");

    let again = request(addr, "GET", "/api/pulse?since=3").await;
    assert_eq!(again.status, 200);
    assert_eq!(counting.loads(), 1, "a poll inside the TTL loads nothing");
    handle.abort();
}

/// TTL 0 reloads on every request but still collapses a concurrent burst
/// into one load.
#[tokio::test]
async fn a_zero_ttl_reloads_per_request_and_still_single_flights() {
    let mut counting = LoadCounting::new(seed("t4-zero").await);
    counting.delay = Duration::from_millis(50);
    let (addr, handle) = serve_counting(&counting, "t4-zero", &web_ttl_ms(0)).await;

    let polls: Vec<_> = (0..6)
        .map(|_| tokio::spawn(async move { request(addr, "GET", "/api/pulse").await }))
        .collect();
    for poll in polls {
        assert_eq!(poll.await.expect("poll task").status, 200);
    }
    assert_eq!(counting.loads(), 1, "a concurrent burst is one load");

    assert_eq!(request(addr, "GET", "/api/pulse").await.status, 200);
    assert_eq!(counting.loads(), 2, "the next request reloads at TTL 0");
    handle.abort();
}

/// A write is invisible until the view is older than the TTL, then the next
/// request reloads and serves it.
#[tokio::test]
async fn a_write_is_served_after_the_ttl_and_not_before() {
    let store = seed("t4-ttl").await;
    let counting = LoadCounting::new(store.clone());
    let ttl = Duration::from_millis(300);
    let (addr, handle) = serve_counting(&counting, "t4-ttl", &web_ttl_ms(300)).await;

    let before = get_json(addr, "/api/pulse?since=0").await;
    assert_eq!(before["events"]["total"], 3, "{before}");
    let loaded_at = std::time::Instant::now();

    promote(&store, "t4-ttl", "auth middleware").await;
    let within = get_json(addr, "/api/pulse?since=0").await;
    // Only meaningful while the view is still young; a slow machine that
    // already crossed the TTL proves nothing either way here.
    if loaded_at.elapsed() < ttl {
        assert_eq!(
            within["events"]["total"], 3,
            "inside the TTL the view is reused: {within}"
        );
        assert_eq!(counting.loads(), 1);
    }

    tokio::time::sleep(ttl + Duration::from_millis(50)).await;
    let after = get_json(addr, "/api/pulse?since=0").await;
    assert_eq!(after["events"]["total"], 6, "after the TTL: {after}");
    assert_eq!(after["stats"]["canonization_events"], 6, "{after}");
    assert!(counting.loads() >= 2);
    handle.abort();
}

/// A failed load answers the requests that joined it, is not cached, and the
/// next request retries.
#[tokio::test]
async fn a_failed_load_is_not_cached() {
    let mut counting = LoadCounting::new(seed("t4-fail").await);
    counting.delay = Duration::from_millis(50);
    counting.fail.store(true, Ordering::SeqCst);
    let (addr, handle) = serve_counting(&counting, "t4-fail", &web_ttl_ms(60_000)).await;

    let polls: Vec<_> = (0..4)
        .map(|_| tokio::spawn(async move { request(addr, "GET", "/api/pulse").await }))
        .collect();
    for poll in polls {
        let r = poll.await.expect("poll task");
        assert_eq!(r.status, 502, "{}", r.body);
        assert!(
            r.body.contains("load refused by the test store"),
            "{}",
            r.body
        );
        assert!(
            r.headers.to_lowercase().contains("cache-control: no-store"),
            "{}",
            r.headers
        );
    }
    assert_eq!(counting.loads(), 1, "the joiners share the failed load");

    counting.fail.store(false, Ordering::SeqCst);
    let r = request(addr, "GET", "/api/pulse").await;
    assert_eq!(r.status, 200, "the failure is not cached: {}", r.body);
    assert_eq!(counting.loads(), 2);
    handle.abort();
}

/// An erased session (#23) is the empty session on the next view, never its
/// old content for longer than one TTL (design Q9).
#[tokio::test]
async fn an_erased_session_is_empty_on_the_next_view() {
    let store = seed("t4-erase").await;
    let counting = LoadCounting::new(store.clone());
    let (addr, handle) = serve_counting(&counting, "t4-erase", &web_ttl_ms(0)).await;
    let before = get_json(addr, "/api/pulse").await;
    assert_eq!(before["stats"]["concepts"], 3, "{before}");

    crate::cli::erase_session::run(
        store.as_ref(),
        crate::cli::erase_session::Args {
            session: "t4-erase".into(),
            confirm: "t4-erase".into(),
        },
    )
    .await
    .expect("erase");

    for path in ["/api/pulse", "/api/graph", "/api/session"] {
        let r = request(addr, "GET", path).await;
        assert_eq!(r.status, 200, "erased reads as empty, not an error: {path}");
        assert!(
            !r.body.contains("user schema"),
            "{path} served erased content: {}",
            r.body
        );
    }
    let after = get_json(addr, "/api/pulse").await;
    assert_eq!(after["stats"]["concepts"], 0, "{after}");
    assert_eq!(after["events"]["total"], 0, "{after}");
    handle.abort();
}

/// Startup still fails fast on a store whose schema preflight refuses, and
/// loads no session doing it (sessions are lazy).
#[tokio::test]
async fn a_failing_schema_preflight_still_fails_startup() {
    struct NoSchema(LoadCounting);
    #[async_trait]
    impl GraphStore for NoSchema {
        async fn init_schema(&self) -> Result<(), StoreError> {
            self.0.init_schema().await
        }
        fn capabilities(&self) -> Capabilities {
            self.0.capabilities()
        }
        async fn preflight_schema(&self) -> Result<(), StoreError> {
            Err(StoreError::Backend("schema is not provisioned".into()))
        }
        async fn flush(&self, b: &MutationBatch, t: Option<u64>) -> Result<(), StoreError> {
            self.0.flush(b, t).await
        }
        async fn load_session(&self, s: &SessionId) -> Result<GraphSnapshot, StoreError> {
            self.0.load_session(s).await
        }
        async fn keyword_candidates(
            &self,
            s: &SessionId,
            t: &[String],
            l: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.0.keyword_candidates(s, t, l).await
        }
        async fn vector_candidates(
            &self,
            s: &SessionId,
            e: &[f32],
            l: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.0.vector_candidates(s, e, l).await
        }
        async fn vector_candidates_checked(
            &self,
            s: &SessionId,
            e: &[f32],
            c: &EmbeddingContract,
            l: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.0.vector_candidates_checked(s, e, c, l).await
        }
        async fn blast_radius(
            &self,
            s: &SessionId,
            n: NodeId,
            a: Duration,
            now: DateTime<Utc>,
        ) -> Result<u64, StoreError> {
            self.0.blast_radius(s, n, a, now).await
        }
        async fn interaction_span(
            &self,
            s: &SessionId,
            n: NodeId,
            a: Duration,
            now: DateTime<Utc>,
        ) -> Result<crate::types::InteractionSpan, StoreError> {
            self.0.interaction_span(s, n, a, now).await
        }
        async fn record_canonization(
            &self,
            e: &CanonizationEvent,
            t: Option<u64>,
        ) -> Result<(), StoreError> {
            self.0.record_canonization(e, t).await
        }
        async fn acquire_lease(
            &self,
            s: &SessionId,
            h: &crate::store::lease::LeaseHolder,
            t: Duration,
        ) -> Result<crate::store::lease::LeaseOutcome, StoreError> {
            self.0.acquire_lease(s, h, t).await
        }
        async fn refresh_lease(
            &self,
            s: &SessionId,
            h: &crate::store::lease::LeaseHolder,
            t: Duration,
        ) -> Result<crate::store::lease::LeaseOutcome, StoreError> {
            self.0.refresh_lease(s, h, t).await
        }
        async fn release_lease(
            &self,
            s: &SessionId,
            h: &crate::store::lease::LeaseHolder,
        ) -> Result<(), StoreError> {
            self.0.release_lease(s, h).await
        }
    }

    let counting = LoadCounting::new(Arc::new(MemoryStore::new()));
    let err = run(
        backends_with_store(Box::new(NoSchema(counting.clone()))),
        Args {
            session: "t4-preflight".into(),
            port: 0,
            bind: Ipv4Addr::LOCALHOST.into(),
            auth_token: None,
            web: crate::config::WebConfig::default(),
        },
    )
    .await
    .expect_err("a refused preflight fails startup");
    assert!(
        err.to_string().contains("schema is not provisioned"),
        "{err}"
    );
    assert_eq!(counting.loads(), 0, "startup loads no session");
}

// ---- the cache itself, without HTTP ---------------------------------

fn bounds(max_loaded_sessions: usize) -> crate::cli::serve_web::views::ViewBounds {
    crate::cli::serve_web::views::ViewBounds {
        ttl: Duration::from_secs(60),
        max_loaded_sessions,
        load_concurrency: 2,
    }
}

/// Beyond `max_loaded_sessions` the least recently used view is dropped;
/// a request for it reloads, and its freshness tracker survives eviction.
#[tokio::test]
async fn the_least_recently_used_view_is_evicted_and_reloads() {
    let store = seed("t4-a").await;
    // A second session in the same store.
    crate::cli::derive::run(
        backends_on(store.clone()),
        crate::cli::derive::Args {
            session: "t4-b".into(),
            agent: "agent-b".into(),
            content: "billing ledger".into(),
            kind: ConceptKind::Entity,
            parent_of: vec![],
            concept: vec![],
        },
    )
    .await
    .expect("derive b");
    let counting = LoadCounting::new(store);
    let (a, b) = (SessionId::new("t4-a"), SessionId::new("t4-b"));
    let cache = crate::cli::serve_web::views::ViewCache::new([a.clone(), b.clone()], bounds(1));
    let contract = backends_on(Arc::new(MemoryStore::new())).embedding;

    let va = cache.view(&counting, &contract, &a).await.expect("a");
    assert_eq!(va.counts.concepts, 3);
    let age = cache.observe(&a, 41);
    let vb = cache.view(&counting, &contract, &b).await.expect("b");
    assert_eq!(vb.counts.concepts, 1, "each view is its own session");
    assert!(!cache.is_loaded(&a), "a was least recently used");
    assert!(cache.is_loaded(&b));
    assert_eq!(va.counts.concepts, 3, "a held view outlives eviction");

    std::thread::sleep(Duration::from_millis(5));
    let again = cache.view(&counting, &contract, &a).await.expect("a again");
    assert_eq!(again.counts.concepts, 3);
    assert_eq!(counting.loads(), 3, "an evicted view reloads");
    assert!(!cache.is_loaded(&b), "now b is least recently used");
    assert!(
        cache.observe(&a, 41) > age,
        "eviction must not reset a session's freshness"
    );

    // A view inside the TTL is reused, not reloaded.
    cache
        .view(&counting, &contract, &a)
        .await
        .expect("a cached");
    assert_eq!(counting.loads(), 3);
}

/// The view's counts and feed agree with the store's snapshot, and an
/// unserved session is refused without a store call.
#[tokio::test]
async fn a_view_is_one_consistent_load_and_unserved_sessions_load_nothing() {
    let store = seed("t4-view").await;
    let counting = LoadCounting::new(store.clone());
    let sid = SessionId::new("t4-view");
    let cache = crate::cli::serve_web::views::ViewCache::new([sid.clone()], bounds(4));
    let contract = backends_on(Arc::new(MemoryStore::new())).embedding;

    let view = cache.view(&counting, &contract, &sid).await.expect("view");
    let snap = store.load_session(&sid).await.expect("snapshot");
    let from_snapshot = events_from(&snap, 0);
    let from_view = view.events_since(0);
    assert_eq!(
        serde_json::to_value(&from_view).unwrap(),
        serde_json::to_value(&from_snapshot).unwrap(),
        "the view's feed is the snapshot's feed"
    );
    assert_eq!(view.counts.concepts, snap.concepts.len());
    assert_eq!(view.embedding.status, "compatible");

    let other = SessionId::new("t4-elsewhere");
    assert!(cache.view(&counting, &contract, &other).await.is_err());
    assert_eq!(counting.loads(), 1, "an unserved session is never loaded");
}

/// `run_detailed_on` over the portal's view gives the same `context` and
/// `hits` as `lambo recall`'s `run_detailed` on one fixture.
#[tokio::test]
async fn recall_on_a_view_matches_the_cli_recall_byte_for_byte() {
    let store = seed("t4-recall").await;
    let backends = backends_on(store.clone());
    let cli =
        crate::cli::recall::run_detailed(&backends, "t4-recall", "user schema", None, None, None)
            .await
            .expect("cli recall");

    let (addr, handle) = spawn(state_on(store, "t4-recall")).await;
    let page = get_json(addr, "/api/recall?q=user%20schema").await;
    assert_eq!(page["context"], cli.context, "{page}");
    // Scores carry a recency term computed from the wall clock at recall
    // time, so two runs a few milliseconds apart differ in the last bits;
    // everything else in a hit must be identical.
    let mut cli_hits = serde_json::to_value(&cli.hits).unwrap();
    let mut page_hits = page["hits"].clone();
    for (cli_hit, page_hit) in cli_hits
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .zip(page_hits.as_array_mut().unwrap().iter_mut())
    {
        let a = cli_hit["score"].take().as_f64().unwrap();
        let b = page_hit["score"].take().as_f64().unwrap();
        assert!((a - b).abs() < 1e-6, "score {a} vs {b}");
    }
    assert_eq!(page_hits, cli_hits, "the hits are the CLI's");
    assert_eq!(
        page["response_annotations"],
        serde_json::to_value(&cli.response_annotations).unwrap()
    );
    handle.abort();
}
