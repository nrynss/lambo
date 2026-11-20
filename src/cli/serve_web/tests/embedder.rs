//! #247: the embedder loads on the first recall that embeds a query, once,
//! or never with `[web] embedder = false`; a failed load degrades the
//! recall and is retried by the next one.
//!
//! [`Deferred`] stands in for the candle adapter: it embeds only after its
//! `prepare` ran (an embed before that is an error the tests would see as a
//! degraded recall), counts every `prepare` and query embed, and can park
//! its `prepare` until a test releases it, or fail it.

use super::*;
use crate::cli::serve_web::embedder::OFF_BY_CONFIGURATION;
use std::sync::atomic::AtomicBool;

/// A fixture embedder with a deferred, countable, parkable, failable load.
#[derive(Clone)]
struct Deferred {
    inner: FixtureEmbedder,
    prepares: Arc<AtomicUsize>,
    queries: Arc<AtomicUsize>,
    loaded: Arc<AtomicBool>,
    /// `prepare` waits for one permit when parked.
    park: Option<Arc<tokio::sync::Semaphore>>,
    /// How many of the next `prepare`s fail.
    failures: Arc<parking_lot::Mutex<usize>>,
}

impl Deferred {
    fn new() -> Self {
        Self {
            inner: FixtureEmbedder::new(),
            prepares: Arc::default(),
            queries: Arc::default(),
            loaded: Arc::default(),
            park: None,
            failures: Arc::default(),
        }
    }

    fn parked(mut self) -> (Self, Arc<tokio::sync::Semaphore>) {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        self.park = Some(gate.clone());
        (self, gate)
    }

    fn failing(self, n: usize) -> Self {
        *self.failures.lock() = n;
        self
    }

    fn ready(&self) -> Result<(), crate::embed::EmbedError> {
        if self.loaded.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(crate::embed::EmbedError::Unavailable(
                "embedded before prepare".into(),
            ))
        }
    }
}

#[async_trait]
impl crate::embed::Embedder for Deferred {
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.ready()?;
        self.inner.embed(text).await
    }
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.queries.fetch_add(1, Ordering::SeqCst);
        self.ready()?;
        self.inner.embed_query(text).await
    }
    async fn prepare(&self) -> Result<(), crate::embed::EmbedError> {
        self.prepares.fetch_add(1, Ordering::SeqCst);
        if let Some(park) = &self.park {
            park.acquire().await.expect("park open").forget();
        }
        let failing = {
            let mut left = self.failures.lock();
            let failing = *left > 0;
            *left = left.saturating_sub(1);
            failing
        };
        if failing {
            return Err(crate::embed::EmbedError::Backend("weights missing".into()));
        }
        self.loaded.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn is_prepared(&self) -> bool {
        self.loaded.load(Ordering::SeqCst)
    }
}

/// A portal over a seeded session on a store that searches by vector (so
/// recall's vector leg runs), embedding through `embedder`, with `web`.
async fn portal(
    session: &str,
    embedder: Deferred,
    web: &crate::config::WebConfig,
) -> (Arc<AppState>, SocketAddr, tokio::task::JoinHandle<()>) {
    let store = seed(session).await;
    let mut backends = backends_with_store(Box::new(VectorSearch(Shared(store))));
    backends.embedder = Box::new(embedder);
    let state = state_with_web(backends, session, None, web);
    let (addr, handle) = spawn(state.clone()).await;
    (state, addr, handle)
}

/// The recall's `vector_degraded` lines about its query embedding: what the
/// gate decides. [`VectorSearch`] claims vector search over a store that
/// cannot answer the read, so a recall whose embed succeeded still carries
/// the store's own "vector read failed" line, after the embed; it is left
/// out here.
fn degraded(body: &serde_json::Value) -> Vec<String> {
    body["response_annotations"]
        .as_array()
        .expect("response_annotations")
        .iter()
        .filter(|a| a["kind"] == "vector_degraded")
        .map(|a| a["text"].as_str().expect("text").to_string())
        .filter(|text| !text.contains("the store's vector read failed"))
        .collect()
}

/// Wait until `n` recalls are waiting on the gate's single flight.
async fn until_arrived(state: &AppState, n: u64) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while state.embedder.arrivals() < n {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "{n} recalls never reached the gate ({} did)",
            state.embedder.arrivals()
        )
    });
}

/// `n` concurrent recalls, each with its own query (no query-cache help).
fn burst(addr: SocketAddr, n: usize) -> Vec<tokio::task::JoinHandle<HttpResponse>> {
    (0..n)
        .map(|i| {
            tokio::spawn(async move {
                request(addr, "GET", &format!("/api/recall?q=user%20schema%20{i}")).await
            })
        })
        .collect()
}

/// (a) Building the state and serving every non-recall route never loads
/// the embedder; nor does a recall on a store that cannot search by vector,
/// which needs no query vector.
#[tokio::test]
async fn startup_and_browsing_never_load_the_embedder() {
    let embedder = Deferred::new();
    let (prepares, queries) = (embedder.prepares.clone(), embedder.queries.clone());
    let (state, addr, handle) = portal(
        "t247-browse",
        embedder,
        &crate::config::WebConfig::default(),
    )
    .await;
    for path in [
        "/",
        "/healthz",
        "/app.js",
        "/api/session",
        "/api/pulse",
        "/api/stats",
        "/api/events",
        "/api/graph",
        "/api/inspect?focus=user%20schema",
    ] {
        let r = request(addr, "GET", path).await;
        assert_eq!(r.status, 200, "GET {path}: {}", r.body);
    }
    let info = get_json(addr, "/api/session").await;
    assert_eq!(info["embedder"], "fixture", "the kind only: {info}");
    assert_eq!(info["vector_search"], true, "lazy is not off: {info}");
    assert_eq!(
        prepares.load(Ordering::SeqCst),
        0,
        "browsing loaded the model"
    );
    assert_eq!(queries.load(Ordering::SeqCst), 0);
    assert_eq!(state.embedder.prepares(), 0);
    handle.abort();

    // No vector search: recall embeds nothing, so it loads nothing either.
    let store = seed("t247-novec").await;
    let embedder = Deferred::new();
    let prepares = embedder.prepares.clone();
    let mut backends = backends_on(store);
    backends.embedder = Box::new(embedder);
    let (addr, handle) = spawn(state_from_backends(backends, "t247-novec", None)).await;
    let body = get_json(addr, "/api/recall?q=user%20schema").await;
    assert!(degraded(&body).is_empty(), "{body}");
    assert_eq!(prepares.load(Ordering::SeqCst), 0);
    handle.abort();
}

/// (b) The first recalls, arriving together, share ONE load; every one of
/// them then embeds with the loaded model, and later recalls load nothing.
#[tokio::test]
async fn concurrent_first_recalls_load_the_embedder_once() {
    let (embedder, park) = Deferred::new().parked();
    let (prepares, queries) = (embedder.prepares.clone(), embedder.queries.clone());
    let (state, addr, handle) =
        portal("t247-once", embedder, &crate::config::WebConfig::default()).await;

    let recalls = burst(addr, 4);
    until_arrived(&state, 4).await;
    assert_eq!(prepares.load(Ordering::SeqCst), 1, "one load in flight");
    park.add_permits(16);
    for recall in recalls {
        let r = recall.await.expect("recall task");
        assert_eq!(r.status, 200, "{}", r.body);
        let body: serde_json::Value = serde_json::from_str(&r.body).expect("json");
        assert!(degraded(&body).is_empty(), "the vector leg ran: {body}");
    }
    assert_eq!(prepares.load(Ordering::SeqCst), 1, "exactly one load");
    assert_eq!(state.embedder.prepares(), 1);
    assert_eq!(queries.load(Ordering::SeqCst), 4, "each query embedded");

    let later = get_json(addr, "/api/recall?q=auth%20middleware").await;
    assert!(degraded(&later).is_empty(), "{later}");
    assert_eq!(prepares.load(Ordering::SeqCst), 1, "the model is kept");
    handle.abort();
}

/// (c) `[web] embedder = false`: nothing loads or embeds, recall still
/// answers from its keyword and recent legs, says the vector leg is off by
/// configuration (not failed), and the session reports no vector search.
#[tokio::test]
async fn embedder_off_never_loads_and_recall_says_so() {
    let embedder = Deferred::new();
    let (prepares, queries) = (embedder.prepares.clone(), embedder.queries.clone());
    let web = crate::config::WebConfig {
        embedder: Some(false),
        ..Default::default()
    };
    let (state, addr, handle) = portal("t247-off", embedder, &web).await;

    let body = get_json(addr, "/api/recall?q=user%20schema").await;
    assert_eq!(degraded(&body), [OFF_BY_CONFIGURATION], "{body}");
    assert!(!OFF_BY_CONFIGURATION.contains("failed"));
    let context = body["context"].as_str().expect("context");
    assert!(context.contains(OFF_BY_CONFIGURATION), "{context}");
    assert!(
        context.contains("user schema"),
        "the keyword leg still finds the concept: {context}"
    );
    assert!(!body["hits"].as_array().expect("hits").is_empty(), "{body}");

    let info = get_json(addr, "/api/session").await;
    assert_eq!(info["embedder"], "fixture", "{info}");
    assert_eq!(info["vector_search"], false, "{info}");
    assert_eq!(get_json(addr, "/api/pulse").await["vector_search"], false);

    assert_eq!(prepares.load(Ordering::SeqCst), 0, "never loaded");
    assert_eq!(queries.load(Ordering::SeqCst), 0, "never embedded");
    assert_eq!(state.embedder.prepares(), 0);
    handle.abort();
}

/// (d) A failed load degrades the recall exactly as a failed embed does,
/// and is not remembered: the next recall loads again, and succeeds.
#[tokio::test]
async fn a_failed_load_degrades_and_the_next_recall_retries() {
    let embedder = Deferred::new().failing(1);
    let (prepares, queries) = (embedder.prepares.clone(), embedder.queries.clone());
    let (_state, addr, handle) =
        portal("t247-fail", embedder, &crate::config::WebConfig::default()).await;

    let first = get_json(addr, "/api/recall?q=user%20schema").await;
    assert_eq!(
        degraded(&first),
        ["recall: query embedding failed (backend: weights missing); vector leg skipped"],
        "{first}"
    );
    assert!(
        first["context"]
            .as_str()
            .expect("context")
            .contains("user schema"),
        "{first}"
    );
    assert_eq!(prepares.load(Ordering::SeqCst), 1);
    assert_eq!(
        queries.load(Ordering::SeqCst),
        0,
        "no embed without a model"
    );

    let second = get_json(addr, "/api/recall?q=user%20schema").await;
    assert!(degraded(&second).is_empty(), "the retry loaded: {second}");
    assert_eq!(prepares.load(Ordering::SeqCst), 2, "retried once");
    assert_eq!(queries.load(Ordering::SeqCst), 1);
    handle.abort();
}

/// (d) Recalls waiting on a load that fails take its outcome rather than
/// queueing one more load each: one attempt, every recall degraded, none
/// hangs. The next recall tries again.
#[tokio::test]
async fn recalls_waiting_on_a_failed_load_share_its_outcome() {
    let (embedder, park) = Deferred::new().failing(usize::MAX).parked();
    let prepares = embedder.prepares.clone();
    let (state, addr, handle) = portal(
        "t247-fail-burst",
        embedder,
        &crate::config::WebConfig::default(),
    )
    .await;

    let recalls = burst(addr, 4);
    until_arrived(&state, 4).await;
    park.add_permits(16);
    for recall in recalls {
        let r = tokio::time::timeout(Duration::from_secs(10), recall)
            .await
            .expect("a recall behind a failed load must not hang")
            .expect("recall task");
        assert_eq!(r.status, 200, "{}", r.body);
        let body: serde_json::Value = serde_json::from_str(&r.body).expect("json");
        assert_eq!(
            degraded(&body),
            ["recall: query embedding failed (backend: weights missing); vector leg skipped"],
            "{body}"
        );
    }
    assert_eq!(prepares.load(Ordering::SeqCst), 1, "one shared attempt");

    let again = get_json(addr, "/api/recall?q=user%20schema").await;
    assert_eq!(degraded(&again).len(), 1, "{again}");
    assert_eq!(prepares.load(Ordering::SeqCst), 2, "a new recall retries");
    handle.abort();
}

/// The gate is the only portal source that loads the embedder: startup
/// (`run`) and every route reach the model only through it.
#[test]
fn only_the_gate_loads_the_embedder() {
    for (name, src) in PRODUCTION_SOURCES {
        let src = src.split("#[cfg(all(test").next().unwrap_or(src);
        let calls = src.matches(".prepare()").count();
        if *name == "serve_web/embedder.rs" {
            assert_eq!(calls, 1, "{name}: the gate's one prepare");
        } else {
            assert_eq!(calls, 0, "{name} loads the embedder outside the gate");
        }
    }
}
