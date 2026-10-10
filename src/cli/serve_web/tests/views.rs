//! Store-call accounting for the portal's reads (#4 PR 1).
//!
//! A [`LoadCounting`] store wraps the shared in-RAM store and counts every
//! `load_session` and `preflight_schema`, so a test can say exactly how many
//! full session loads a request costs.

use super::*;
use std::future::Future;
use std::pin::{pin, Pin};
use std::sync::atomic::AtomicBool;
use std::task::{Context, Poll, Waker};
use tokio::sync::Semaphore;

/// [`Shared`], counting `load_session` and `preflight_schema` calls. `fail`
/// makes every load answer a backend error. A [`LoadCounting::park`]ed session's loads wait for a
/// [`LoadCounting::release`], so a test decides exactly when each load
/// finishes instead of racing a timer.
#[derive(Clone)]
struct LoadCounting {
    inner: Shared,
    loads: Arc<AtomicUsize>,
    preflights: Arc<AtomicUsize>,
    fail: Arc<AtomicBool>,
    parked: Arc<parking_lot::Mutex<std::collections::HashMap<SessionId, Arc<Semaphore>>>>,
}

impl LoadCounting {
    fn new(store: Arc<MemoryStore>) -> Self {
        Self {
            inner: Shared(store),
            loads: Arc::new(AtomicUsize::new(0)),
            preflights: Arc::new(AtomicUsize::new(0)),
            fail: Arc::new(AtomicBool::new(false)),
            parked: Arc::default(),
        }
    }

    fn loads(&self) -> usize {
        self.loads.load(Ordering::SeqCst)
    }

    /// From now on, every load of `session` waits for a [`Self::release`].
    fn park(&self, session: &str) {
        self.parked
            .lock()
            .insert(SessionId::new(session), Arc::new(Semaphore::new(0)));
    }

    /// Let `n` parked loads of `session` proceed.
    fn release(&self, session: &str, n: usize) {
        let gate = self.parked.lock().get(&SessionId::new(session)).cloned();
        gate.expect("the session is parked").add_permits(n);
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
        let parked = self.parked.lock().get(session).cloned();
        if let Some(gate) = parked {
            gate.acquire().await.expect("never closed").forget();
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
/// given `[web]` bounds; the wrapper's counters and the state stay readable.
async fn serve_counting(
    counting: &LoadCounting,
    session: &str,
    web: &crate::config::WebConfig,
) -> (Arc<AppState>, SocketAddr, tokio::task::JoinHandle<()>) {
    let state = state_with_web(
        backends_with_store(Box::new(counting.clone())),
        session,
        None,
        web,
    );
    let (addr, handle) = spawn(state.clone()).await;
    (state, addr, handle)
}

fn web_ttl_ms(ms: u64) -> crate::config::WebConfig {
    crate::config::WebConfig {
        view_ttl_ms: Some(ms),
        ..Default::default()
    }
}

/// Wait until `n` requests for the served session have queued for its view.
/// With the session's load parked, every one of them arrived before any
/// load finished. The deadline is a hang guard, not a timing assumption.
async fn until_queued(state: &AppState, n: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let queued = state.views.queued(&state.default_session);
        if queued >= n {
            assert_eq!(queued, n, "more requests queued than were sent");
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "only {queued} of {n} requests queued"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// A spawned request's response. A request that started a second load
/// behind the parked one would wait forever; fail it instead of hanging.
async fn answered(poll: tokio::task::JoinHandle<HttpResponse>) -> HttpResponse {
    tokio::time::timeout(Duration::from_secs(30), poll)
        .await
        .expect("the request was answered (did it start another, parked load?)")
        .expect("poll task")
}

/// What the page fetches when it opens, plus a recall: one view serves it
/// all, so one load inside the TTL however many routes are hit.
#[tokio::test]
async fn every_route_inside_one_ttl_shares_one_load() {
    let counting = LoadCounting::new(seed("t4-page").await);
    let (_, addr, handle) = serve_counting(&counting, "t4-page", &web_ttl_ms(60_000)).await;
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
    let counting = LoadCounting::new(seed("t4-burst").await);
    counting.park("t4-burst");
    let (state, addr, handle) = serve_counting(&counting, "t4-burst", &web_ttl_ms(60_000)).await;

    let polls: Vec<_> = (0..8)
        .map(|_| tokio::spawn(async move { request(addr, "GET", "/api/pulse?since=0").await }))
        .collect();
    until_queued(&state, 8).await;
    counting.release("t4-burst", 1);
    for poll in polls {
        let r = answered(poll).await;
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
    let counting = LoadCounting::new(seed("t4-zero").await);
    counting.park("t4-zero");
    let (state, addr, handle) = serve_counting(&counting, "t4-zero", &web_ttl_ms(0)).await;

    let polls: Vec<_> = (0..6)
        .map(|_| tokio::spawn(async move { request(addr, "GET", "/api/pulse").await }))
        .collect();
    until_queued(&state, 6).await;
    counting.release("t4-zero", 1);
    for poll in polls {
        assert_eq!(answered(poll).await.status, 200);
    }
    assert_eq!(counting.loads(), 1, "a concurrent burst is one load");

    counting.release("t4-zero", 1);
    assert_eq!(request(addr, "GET", "/api/pulse").await.status, 200);
    assert_eq!(counting.loads(), 2, "the next request reloads at TTL 0");
    handle.abort();
}

/// The TTL on the cache itself, on tokio's paused clock (design section
/// 10): a view is reused until it is `ttl` old, a write is invisible until
/// then, and the first request at `ttl` reloads and serves it.
#[tokio::test(start_paused = true)]
async fn a_write_is_served_after_the_ttl_and_not_before() {
    let store = seed("t4-ttl").await;
    let counting = LoadCounting::new(store.clone());
    let sid = SessionId::new("t4-ttl");
    let cache = crate::cli::serve_web::views::ViewCache::new(
        [sid.clone()],
        crate::cli::serve_web::views::ViewBounds {
            ttl: Duration::from_millis(300),
            ..bounds(4)
        },
    );
    let contract = backends_on(Arc::new(MemoryStore::new())).embedding;

    let first = cache.view(&counting, &contract, &sid).await.expect("view");
    assert_eq!(first.event_total(), 3);
    promote(&store, "t4-ttl", "auth middleware").await;

    tokio::time::advance(Duration::from_millis(299)).await;
    let within = cache.view(&counting, &contract, &sid).await.expect("view");
    assert!(
        Arc::ptr_eq(&first, &within),
        "inside the TTL the view is reused"
    );
    assert_eq!(within.event_total(), 3, "the write is not served yet");
    assert_eq!(counting.loads(), 1);

    tokio::time::advance(Duration::from_millis(1)).await;
    let after = cache.view(&counting, &contract, &sid).await.expect("view");
    assert_eq!(after.event_total(), 6, "at the TTL the write is served");
    assert_eq!(counting.loads(), 2);
}

/// The same through the pulse route, on the real clock. Only "served after
/// the TTL" is asserted: how old the view is when a second request lands
/// depends on the machine, which the paused-clock test above pins instead.
#[tokio::test]
async fn the_pulse_serves_a_write_once_the_ttl_has_passed() {
    let store = seed("t4-ttl-route").await;
    let counting = LoadCounting::new(store.clone());
    let ttl = Duration::from_millis(300);
    let (_, addr, handle) = serve_counting(&counting, "t4-ttl-route", &web_ttl_ms(300)).await;

    let before = get_json(addr, "/api/pulse?since=0").await;
    assert_eq!(before["events"]["total"], 3, "{before}");
    promote(&store, "t4-ttl-route", "auth middleware").await;

    tokio::time::sleep(ttl + Duration::from_millis(50)).await;
    let after = get_json(addr, "/api/pulse?since=0").await;
    assert_eq!(after["events"]["total"], 6, "after the TTL: {after}");
    assert_eq!(after["stats"]["canonization_events"], 6, "{after}");
    assert_eq!(counting.loads(), 2);
    handle.abort();
}

/// A failed load answers the requests that joined it, is not cached, and the
/// next request retries.
#[tokio::test]
async fn a_failed_load_is_not_cached() {
    let counting = LoadCounting::new(seed("t4-fail").await);
    counting.park("t4-fail");
    counting.fail.store(true, Ordering::SeqCst);
    let (state, addr, handle) = serve_counting(&counting, "t4-fail", &web_ttl_ms(60_000)).await;

    let polls: Vec<_> = (0..4)
        .map(|_| tokio::spawn(async move { request(addr, "GET", "/api/pulse").await }))
        .collect();
    until_queued(&state, 4).await;
    counting.release("t4-fail", 1);
    for poll in polls {
        let r = answered(poll).await;
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
    counting.release("t4-fail", 1);
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
    let (_, addr, handle) = serve_counting(&counting, "t4-erase", &web_ttl_ms(0)).await;
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
            sessions: Vec::new(),
            port: 0,
            bind: Ipv4Addr::LOCALHOST.into(),
            auth_token: None,
            allowed_hosts: Vec::new(),
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
        recall_concurrency: 4,
    }
}

/// Beyond `max_loaded_sessions` the least recently used view is dropped;
/// a request for it reloads, and its freshness tracker survives eviction.
#[tokio::test]
async fn the_least_recently_used_view_is_evicted_and_reloads() {
    let counting = LoadCounting::new(two_sessions().await);
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

/// Poll `fut` once with a no-op waker. The tests below step several
/// requests by hand so the interleaving is the one written, not a timer's.
fn poll_once<F: Future>(fut: Pin<&mut F>) -> Poll<F::Output> {
    fut.poll(&mut Context::from_waker(Waker::noop()))
}

/// Drive `fut` to completion, yielding to the runtime between polls.
async fn drive<F: Future>(mut fut: Pin<&mut F>) -> F::Output {
    for _ in 0..10_000 {
        if let Poll::Ready(out) = poll_once(fut.as_mut()) {
            return out;
        }
        tokio::task::yield_now().await;
    }
    panic!("the future never finished");
}

/// Two sessions sharing `bounds(1)`: `t4-a` from [`seed`], and `t4-b`
/// (one concept) in the same store.
async fn two_sessions() -> Arc<MemoryStore> {
    let store = seed("t4-a").await;
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
    store
}

/// The eviction race (#4 PR 1 review L1): A's load finishes, and before A's
/// queued joiner takes the gate, B's load finishes and evicts A
/// (`max_loaded_sessions = 1`). The joiner must load A again, not answer an
/// error for a healthy session.
#[tokio::test]
async fn a_joiner_whose_view_was_evicted_loads_it_again() {
    let counting = LoadCounting::new(two_sessions().await);
    counting.park("t4-a");
    counting.park("t4-b");
    let (a, b) = (SessionId::new("t4-a"), SessionId::new("t4-b"));
    let cache = crate::cli::serve_web::views::ViewCache::new([a.clone(), b.clone()], bounds(1));
    let contract = backends_on(Arc::new(MemoryStore::new())).embedding;

    let mut loader = pin!(cache.view(&counting, &contract, &a));
    let mut joiner = pin!(cache.view(&counting, &contract, &a));
    let mut other = pin!(cache.view(&counting, &contract, &b));
    assert!(
        poll_once(loader.as_mut()).is_pending(),
        "A's load is parked"
    );
    assert!(
        poll_once(joiner.as_mut()).is_pending(),
        "the joiner queues on A's gate"
    );
    assert!(poll_once(other.as_mut()).is_pending(), "B's load is parked");

    counting.release("t4-a", 1);
    drive(loader.as_mut()).await.expect("A loads");
    counting.release("t4-b", 1);
    drive(other.as_mut()).await.expect("B loads");
    assert!(
        !cache.is_loaded(&a),
        "B's load evicted A before the joiner ran"
    );

    counting.release("t4-a", 1);
    let view = drive(joiner.as_mut())
        .await
        .expect("an evicted view is reloaded, not an error");
    assert_eq!(view.counts.concepts, 3);
    assert_eq!(counting.loads(), 3, "the joiner loaded A once more");
}

/// SQLite loads one session at a time whatever `[web] load_concurrency`
/// says (its portal pool is one connection); other stores take the value.
#[test]
fn sqlite_forces_one_load_at_a_time() {
    use crate::cli::serve_web::views::ViewBounds;
    let web = crate::config::WebConfig {
        load_concurrency: Some(4),
        recall_concurrency: Some(3),
        ..Default::default()
    };
    let sqlite = ViewBounds::resolve(&web, StoreKind::Sqlite);
    assert_eq!(sqlite.load_concurrency, 1, "SQLite is forced to 1");
    assert_eq!(
        sqlite.recall_concurrency, 3,
        "only the load bound is forced"
    );
    for kind in [StoreKind::Memory, StoreKind::Postgres, StoreKind::Cockroach] {
        assert_eq!(
            ViewBounds::resolve(&web, kind).load_concurrency,
            4,
            "{kind:?}"
        );
    }
}

/// The H1 mismatch warning is printed once per session, at its first load
/// that sees the mismatch, not on every reload; each session gets its own.
#[tokio::test]
async fn the_mismatch_warning_is_printed_once_per_session() {
    let counting = LoadCounting::new(two_sessions().await);
    let (a, b) = (SessionId::new("t4-a"), SessionId::new("t4-b"));
    let cache = crate::cli::serve_web::views::ViewCache::new(
        [a.clone(), b.clone()],
        crate::cli::serve_web::views::ViewBounds {
            ttl: Duration::ZERO,
            ..bounds(4)
        },
    );
    let stored = backends_on(Arc::new(MemoryStore::new())).embedding;
    let configured = EmbeddingContract {
        dim: stored.dim + 1,
        ..stored.clone()
    };

    for _ in 0..3 {
        let view = cache.view(&counting, &configured, &a).await.expect("a");
        assert_eq!(view.embedding.status, "mismatch");
    }
    assert_eq!(counting.loads(), 3, "TTL 0: every view is a fresh load");
    assert_eq!(
        cache.mismatch_warnings(&a),
        1,
        "one warning for a, not three"
    );
    assert_eq!(cache.mismatch_warnings(&b), 0, "b has not been loaded");

    cache.view(&counting, &configured, &b).await.expect("b");
    assert_eq!(cache.mismatch_warnings(&b), 1, "b gets its own warning");
    assert_eq!(cache.mismatch_warnings(&a), 1);

    // A compatible session prints nothing.
    let fresh = crate::cli::serve_web::views::ViewCache::new([a.clone()], bounds(4));
    fresh.view(&counting, &stored, &a).await.expect("a");
    assert_eq!(fresh.mismatch_warnings(&a), 0);
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
    // Recall over the view ranks exactly what `lambo recall` ranks: nothing
    // on the path reads the clock (a reader's daemon score is 0, BM25 sums
    // in sorted term order, session recency is anchored to graph
    // timestamps). The page's hits arrive as JSON text, and serde_json's
    // float parse (no `float_roundtrip`) can land one ULP off the f64 it
    // printed. So the CLI's hits take the same trip through a JSON string,
    // and the two arrays must then be equal with no tolerance.
    let cli_hits: serde_json::Value =
        serde_json::from_str(&serde_json::to_string(&cli.hits).unwrap()).unwrap();
    assert_eq!(page["hits"], cli_hits, "the hits are the CLI's");
    assert_eq!(
        page["response_annotations"],
        serde_json::to_value(&cli.response_annotations).unwrap()
    );
    handle.abort();
}

/// At `recall_concurrency` recalls in flight, the next recall waits two
/// seconds for a permit and then gets 503 with `Retry-After: 1` and
/// `no-store`; once a permit is free, recall answers again.
#[tokio::test]
async fn a_saturated_recall_bound_answers_503_with_retry_after() {
    let store = seed("t4-busy").await;
    let state = state_with_web(
        backends_on(store),
        "t4-busy",
        None,
        &crate::config::WebConfig {
            recall_concurrency: Some(1),
            ..Default::default()
        },
    );
    let (addr, handle) = spawn(state.clone()).await;

    let held = state.views.recall_permit().await.expect("the one permit");
    let started = std::time::Instant::now();
    let busy = request(addr, "GET", "/api/recall?q=user%20schema").await;
    assert_eq!(busy.status, 503, "{}", busy.body);
    assert!(
        started.elapsed() >= Duration::from_millis(1_900),
        "the request waits for a permit before refusing"
    );
    let headers = busy.headers.to_lowercase();
    assert!(headers.contains("retry-after: 1"), "{headers}");
    assert!(headers.contains("cache-control: no-store"), "{headers}");
    assert!(busy.body.contains("retry shortly"), "{}", busy.body);
    // A bad query is still refused first, without waiting for a permit.
    let bad = request(addr, "GET", "/api/recall?q=").await;
    assert_eq!(bad.status, 400, "{}", bad.body);

    drop(held);
    let ok = request(addr, "GET", "/api/recall?q=user%20schema").await;
    assert_eq!(ok.status, 200, "{}", ok.body);
    handle.abort();
}

/// A recall waiting on a slow load holds no recall permit (review L5): the
/// load is bounded by the load semaphore, and the permit covers embed and
/// pipeline work only. With one permit and the session's load parked, the
/// permit stays free for another recall until the load finishes.
#[tokio::test]
async fn a_recall_waiting_on_a_slow_load_holds_no_recall_permit() {
    let counting = LoadCounting::new(seed("t4-slow").await);
    counting.park("t4-slow");
    let (state, addr, handle) = serve_counting(
        &counting,
        "t4-slow",
        &crate::config::WebConfig {
            recall_concurrency: Some(1),
            ..Default::default()
        },
    )
    .await;

    let slow =
        tokio::spawn(async move { request(addr, "GET", "/api/recall?q=user%20schema").await });
    until_queued(&state, 1).await;
    let permit = tokio::time::timeout(Duration::from_millis(500), state.views.recall_permit())
        .await
        .expect("the one recall permit is free while the load is parked")
        .expect("a permit");
    drop(permit);

    counting.release("t4-slow", 1);
    let r = answered(slow).await;
    assert_eq!(r.status, 200, "{}", r.body);
    handle.abort();
}

/// [`FixtureEmbedder`] counting query-role embeds.
struct CountingEmbedder {
    inner: FixtureEmbedder,
    queries: Arc<AtomicUsize>,
}

#[async_trait]
impl crate::embed::Embedder for CountingEmbedder {
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.inner.embed(text).await
    }
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.queries.fetch_add(1, Ordering::SeqCst);
        self.inner.embed_query(text).await
    }
}

/// #14 on the portal: a repeated recall query is served from the session's
/// query-embedding cache, with the same answer.
#[tokio::test]
async fn a_repeated_recall_query_embeds_once() {
    let store = seed("t4-qcache").await;
    let embeds = Arc::new(AtomicUsize::new(0));
    let mut backends = backends_with_store(Box::new(VectorSearch(Shared(store))));
    backends.embedder = Box::new(CountingEmbedder {
        inner: FixtureEmbedder::new(),
        queries: embeds.clone(),
    });
    let (addr, handle) = spawn(state_from_backends(backends, "t4-qcache", None)).await;

    let first = get_json(addr, "/api/recall?q=user%20schema").await;
    assert_eq!(embeds.load(Ordering::SeqCst), 1, "the first recall embeds");
    let second = get_json(addr, "/api/recall?q=user%20schema").await;
    assert_eq!(
        embeds.load(Ordering::SeqCst),
        1,
        "a repeated query is served from the session's query cache"
    );
    assert_eq!(first["context"], second["context"]);
    get_json(addr, "/api/recall?q=auth%20middleware").await;
    assert_eq!(embeds.load(Ordering::SeqCst), 2, "a new query embeds");
    handle.abort();
}

/// The query-embedding cache is per session, never process-wide (#32
/// decision 13): two served sessions never share one.
#[test]
fn each_session_has_its_own_query_cache() {
    let (a, b) = (SessionId::new("t4-qa"), SessionId::new("t4-qb"));
    let cache = crate::cli::serve_web::views::ViewCache::new([a.clone(), b.clone()], bounds(4));
    let (qa, qb) = (cache.queries(&a).unwrap(), cache.queries(&b).unwrap());
    assert!(!std::ptr::eq(qa, qb), "one query cache per session");
    let contract = EmbeddingContract {
        kind: "fixture".into(),
        model: None,
        dim: 4,
    };
    qa.lock()
        .insert("shared text", &contract, vec![1.0; 4].into());
    assert!(
        qb.lock().get("shared text", &contract).is_none(),
        "a query embedded for one session is not visible to another"
    );
    assert!(cache.queries(&SessionId::new("t4-unserved")).is_none());
}
