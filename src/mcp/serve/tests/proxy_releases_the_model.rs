use super::*;
use crate::embed::{EmbedError, Embedder, FixtureEmbedder};
use crate::mcp::SessionEndpoint;
use crate::store::{GraphStore, SqliteStore, StoreConfig, StoreKind};
use crate::types::EmbeddingContract;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// The fixture embedder, plus a flag set when the last owner drops it.
struct DropFlagged {
    inner: FixtureEmbedder,
    dropped: Arc<AtomicBool>,
}

impl Drop for DropFlagged {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl Embedder for DropFlagged {
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        self.embed_as(text, crate::test_util::TextRole::Document)
            .await
    }
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        self.embed_as(text, crate::test_util::TextRole::Query).await
    }
    fn modalities(&self) -> crate::embed::Modalities {
        self.inner.modalities()
    }
    async fn embed_image(
        &self,
        image: crate::embed::ImageInput<'_>,
    ) -> Result<Vec<f32>, EmbedError> {
        self.inner.embed_image(image).await
    }
    async fn prepare(&self) -> Result<(), EmbedError> {
        self.inner.prepare().await
    }
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        self.inner.as_any()
    }
}

impl DropFlagged {
    /// The `embed` behaviour above, in either text role (#22: a wrapper
    /// forwards the role, so its inner embedder sees what the caller asked).
    async fn embed_as(
        &self,
        text: &str,
        role: crate::test_util::TextRole,
    ) -> Result<Vec<f32>, EmbedError> {
        role.embed(&self.inner, text).await
    }
}

fn backends(db: &Path, embedder: Box<dyn Embedder>) -> ResolvedBackends {
    let path = db.to_str().expect("utf-8 temp path").to_string();
    ResolvedBackends {
        store: Box::new(SqliteStore::connect(&path).expect("sqlite connect")),
        embedder,
        store_cfg: StoreConfig {
            kind: StoreKind::Sqlite,
            dsn: None,
            path: Some(path),
            vector_dim: None,
        },
        embedder_cfg: crate::embed::EmbedderConfig {
            // #176: tests that count embeds (and every other hand-built
            // ResolvedBackends here) turn the canary off; Warn is the
            // resolved-config default, not what a unit test wants.
            canary: crate::embed::canary::CanaryMode::Off,
            ..Default::default()
        },
        embedding: EmbeddingContract {
            kind: "fixture".into(),
            model: None,
            dim: 1024,
        },
        allow_embedding_mismatch: false,
        config: crate::Config::default(),
    }
}

#[tokio::test]
async fn a_proxy_does_not_retain_the_embedder() {
    let dir = crate::test_util::ScratchDir::short("13p");
    let db = dir.join("store.db");
    SqliteStore::connect(db.to_str().unwrap())
        .unwrap()
        .init_schema()
        .await
        .expect("provision the scratch store");
    // A private (0700, ours) endpoint directory, as `dial_dir` demands,
    // inside the per-test tempdir: never the operator's runtime dir.
    let ep_dir = dir.join("run");
    let session = "issue13-proxy-drop";
    let probe_backends = backends(&db, Box::new(FixtureEmbedder::new()));
    let endpoint = SessionEndpoint::resolve_in(&ep_dir, session, &probe_backends.store_cfg)
        .expect("endpoint fits");
    drop(probe_backends);

    // The holder: takes the lease, publishes the endpoint, binds it.
    let holder_opts = ServeOptions::new(session, "agent-holder");
    let holder = serve_builder(
        &holder_opts,
        backends(&db, Box::new(FixtureEmbedder::new())),
        Some(&endpoint),
        None,
        EarlyShutdown::unarmed(),
        None,
    )
    .build()
    .await
    .expect("the holder attaches");
    let _listener = endpoint.bind().expect("bind the holder endpoint");

    // The would-be second writer, with a model whose drop we can see, and
    // the process-wide calibration `serve` hands its builder (#32 PR 3).
    let dropped = Arc::new(AtomicBool::new(false));
    let calibration = crate::writeq::EmbedderCalibration::new();
    let proxy_opts = ServeOptions::new(session, "agent-proxy");
    let builder = serve_builder(
        &proxy_opts,
        backends(
            &db,
            Box::new(DropFlagged {
                inner: FixtureEmbedder::new(),
                dropped: Arc::clone(&dropped),
            }),
        ),
        Some(&endpoint),
        None,
        EarlyShutdown::unarmed(),
        Some(calibration.clone()),
    );
    assert!(
        !dropped.load(Ordering::SeqCst),
        "control: the builder owns it"
    );

    let role = resolve_role(&proxy_opts, builder, Some(&endpoint), &None)
        .await
        .expect("the election resolves");
    let proxy = match role {
        Role::Proxy(proxy) => proxy,
        Role::Holder(_) => panic!("the second agent must lose the lease and proxy"),
    };
    assert!(
        dropped.load(Ordering::SeqCst),
        "a proxying serve must not keep its resolved embedder alive (with candle on \
                 Metal that is ~1.1 GB of weights it never embeds with)"
    );
    // Nor may the calibration: a proxy builds no pipeline, so it never
    // probes, and the calibration's key is weak.
    assert_eq!(calibration.probes(), 0, "a proxy never probes");

    drop(proxy);
    holder.close().await.expect("holder close");
}

/// Issue #31: the fixture embedder, standing in for an adapter whose weights
/// load in `prepare` (the candle adapter). Counts the loads and notes any
/// embed that arrived before one.
#[derive(Default)]
struct LoadCounting {
    inner: FixtureEmbedder,
    loads: AtomicUsize,
    embeds: AtomicUsize,
    embedded_unloaded: AtomicBool,
}

impl LoadCounting {
    fn note_embed(&self) {
        self.embeds.fetch_add(1, Ordering::SeqCst);
        if self.loads.load(Ordering::SeqCst) == 0 {
            self.embedded_unloaded.store(true, Ordering::SeqCst);
        }
    }
}

#[async_trait::async_trait]
impl Embedder for LoadCounting {
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        self.note_embed();
        self.inner.embed(text).await
    }
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        self.note_embed();
        self.inner.embed_query(text).await
    }
    fn modalities(&self) -> crate::embed::Modalities {
        self.inner.modalities()
    }
    async fn embed_image(
        &self,
        image: crate::embed::ImageInput<'_>,
    ) -> Result<Vec<f32>, EmbedError> {
        self.note_embed();
        self.inner.embed_image(image).await
    }
    async fn prepare(&self) -> Result<(), EmbedError> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        self.inner.prepare().await
    }
    fn is_prepared(&self) -> bool {
        self.loads.load(Ordering::SeqCst) > 0
    }
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        self.inner.as_any()
    }
}

/// Issue #31: a serve that loses the election never loads its model, and the
/// one that wins has loaded it by the time `resolve_role` hands it the
/// session: before the handshake, before the first recall, and before the
/// write pipeline's calibration probe or intent replay could embed.
#[tokio::test]
async fn a_proxy_never_loads_and_the_holder_loads_before_it_serves() {
    /// An `Embedder` box that lends the test a counter it can still read.
    struct Shared(Arc<LoadCounting>);
    #[async_trait::async_trait]
    impl Embedder for Shared {
        fn dimensions(&self) -> usize {
            self.0.dimensions()
        }
        async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
            self.0.embed(text).await
        }
        async fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
            self.0.embed_query(text).await
        }
        fn modalities(&self) -> crate::embed::Modalities {
            self.0.modalities()
        }
        async fn embed_image(
            &self,
            image: crate::embed::ImageInput<'_>,
        ) -> Result<Vec<f32>, EmbedError> {
            self.0.embed_image(image).await
        }
        async fn prepare(&self) -> Result<(), EmbedError> {
            self.0.prepare().await
        }
        fn is_prepared(&self) -> bool {
            self.0.is_prepared()
        }
        fn as_any(&self) -> Option<&dyn std::any::Any> {
            self.0.as_any()
        }
    }

    let dir = crate::test_util::ScratchDir::short("31p");
    let db = dir.join("store.db");
    SqliteStore::connect(db.to_str().unwrap())
        .unwrap()
        .init_schema()
        .await
        .expect("provision the scratch store");
    let ep_dir = dir.join("run");
    let session = "issue31-proxy-load";
    let probe_backends = backends(&db, Box::new(FixtureEmbedder::new()));
    let endpoint = SessionEndpoint::resolve_in(&ep_dir, session, &probe_backends.store_cfg)
        .expect("endpoint fits");
    drop(probe_backends);

    // The holder, through the real election.
    let holder_model = Arc::new(LoadCounting::default());
    let holder_opts = ServeOptions::new(session, "agent-holder");
    let builder = serve_builder(
        &holder_opts,
        backends(&db, Box::new(Shared(Arc::clone(&holder_model)))),
        Some(&endpoint),
        None,
        EarlyShutdown::unarmed(),
        Some(crate::writeq::EmbedderCalibration::new()),
    );
    assert_eq!(
        holder_model.loads.load(Ordering::SeqCst),
        0,
        "control: building the serve's builder loads nothing"
    );
    let holder = match resolve_role(&holder_opts, builder, Some(&endpoint), &None)
        .await
        .expect("the election resolves")
    {
        Role::Holder(mem) => mem,
        Role::Proxy(_) => panic!("the first agent must take the lease"),
    };
    assert_eq!(
        holder_model.loads.load(Ordering::SeqCst),
        1,
        "the holder loads exactly once, before the session is handed to the transport"
    );
    let _listener = endpoint.bind().expect("bind the holder endpoint");

    // The would-be second writer.
    let proxy_model = Arc::new(LoadCounting::default());
    let proxy_opts = ServeOptions::new(session, "agent-proxy");
    let builder = serve_builder(
        &proxy_opts,
        backends(&db, Box::new(Shared(Arc::clone(&proxy_model)))),
        Some(&endpoint),
        None,
        EarlyShutdown::unarmed(),
        Some(crate::writeq::EmbedderCalibration::new()),
    );
    match resolve_role(&proxy_opts, builder, Some(&endpoint), &None)
        .await
        .expect("the election resolves")
    {
        Role::Proxy(proxy) => drop(proxy),
        Role::Holder(_) => panic!("the second agent must lose the lease and proxy"),
    }
    assert_eq!(
        proxy_model.loads.load(Ordering::SeqCst),
        0,
        "a proxying serve must never load its model (#31)"
    );

    // #31 review L6: make the holder embed for real (a derive embeds the
    // concept, a recall embeds the query), so "nothing embedded before the
    // load" is checked against embeds that happened, not vacuously.
    holder
        .derive(
            &[("user schema", crate::types::ConceptType::Entity)],
            &crate::graph::derive::ParentOf::none(),
        )
        .await
        .expect("the holder derives");
    holder
        .recall(crate::types::RecallQuery {
            query: "user schema".into(),
            top_k: 5,
            max_tokens: 500,
            traversal_depth: 0,
        })
        .await
        .expect("the holder recalls");
    // Review L-D: whether the derive and recall above embed depends on the
    // match strategy and the store's vector leg, so pin the counter with an
    // explicit embed through the holder's own embedder: it must count
    // exactly one more, so the flag below is checked against embeds the
    // counter demonstrably sees.
    let before = holder_model.embeds.load(Ordering::SeqCst);
    holder
        .embedder()
        .embed("issue 31 explicit embed")
        .await
        .expect("the holder's embedder embeds");
    assert_eq!(
        holder_model.embeds.load(Ordering::SeqCst),
        before + 1,
        "the counter sees the holder's embeds"
    );
    holder.close().await.expect("holder close");
    assert!(
        !holder_model.embedded_unloaded.load(Ordering::SeqCst),
        "nothing on the holder may embed before its load"
    );
    assert!(!proxy_model.embedded_unloaded.load(Ordering::SeqCst));
    assert_eq!(holder_model.loads.load(Ordering::SeqCst), 1);
}
