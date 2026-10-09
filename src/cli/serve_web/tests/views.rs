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
