use super::*;
use crate::embed::{EmbedError, Embedder, FixtureEmbedder};
use crate::store::{GraphStore, SqliteStore, StoreConfig, StoreKind};
use crate::types::EmbeddingContract;
use std::sync::atomic::{AtomicBool, Ordering};

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
        self.inner.embed(text).await
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
        embedder_cfg: Default::default(),
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
    )
    .build()
    .await
    .expect("the holder attaches");
    let _listener = endpoint.bind().expect("bind the holder endpoint");

    // The would-be second writer, with a model whose drop we can see.
    let dropped = Arc::new(AtomicBool::new(false));
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

    drop(proxy);
    holder.close().await.expect("holder close");
}
