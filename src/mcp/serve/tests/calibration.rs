use super::*;
use crate::embed::{EmbedError, Embedder, FixtureEmbedder};
use crate::store::{MemoryStore, StoreConfig};
use crate::types::EmbeddingContract;
use crate::writeq::EmbedderCalibration;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The fixture embedder, counting every embed, optionally slowed.
struct Counting {
    inner: FixtureEmbedder,
    delay: Duration,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Embedder for Counting {
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        self.inner.embed(text).await
    }
}

fn backends(delay: Duration, calls: &Arc<AtomicUsize>) -> ResolvedBackends {
    ResolvedBackends {
        store: Box::new(MemoryStore::new()),
        embedder: Box::new(Counting {
            inner: FixtureEmbedder::new(),
            delay,
            calls: Arc::clone(calls),
        }),
        store_cfg: StoreConfig {
            kind: Default::default(),
            dsn: None,
            path: None,
            vector_dim: None,
        },
        embedder_cfg: Default::default(),
        embedding: EmbeddingContract {
            kind: "fixture".into(),
            model: None,
            dim: 1024,
        },
        allow_embedding_mismatch: false,
        config: crate::Config {
            backend_flush_interval: Duration::from_secs(3_600),
            ..crate::Config::default()
        },
    }
}

async fn probe_landed(mem: &Memory) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while mem.pipeline().calibration().is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "the probe never landed"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// Two sessions built from one serve builder (the shape PR 4's template
/// builder takes: one resolve, one embedder, a clone per session) fire one
/// probe between them.
#[tokio::test]
async fn sessions_from_the_serve_builder_share_one_probe() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calibration = EmbedderCalibration::new();
    let template = serve_builder(
        &ServeOptions::new("serve-cal-a", "agent-a"),
        backends(Duration::ZERO, &calls),
        None,
        None,
        EarlyShutdown::unarmed(),
        Some(calibration.clone()),
    );
    let a = template.clone().build().await.expect("a attaches");
    probe_landed(&a).await;
    let probed = calls.load(Ordering::SeqCst);
    assert_eq!(probed, crate::writeq::PROBE_EMBEDS);

    let b = template
        .session("serve-cal-b")
        .build()
        .await
        .expect("b attaches");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        probed,
        "the second session fires no probe embed"
    );
    assert_eq!(calibration.probes(), 1);
    assert!(b.pipeline().calibration().is_some(), "and reads the probe");
    a.close().await.expect("close a");
    b.close().await.expect("close b");
}

/// Stage 2 aborts the shared probe (serve hands `run_and_close` the
/// calibration's abort handles beside the keep-warm's), so a probe still
/// running when the transport stops does not run across the close.
#[tokio::test]
async fn stage_two_aborts_the_shared_probe() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calibration = EmbedderCalibration::new();
    let mem = serve_builder(
        &ServeOptions::new("serve-cal-stage-2", "agent-a"),
        backends(Duration::from_millis(50), &calls),
        None,
        None,
        EarlyShutdown::unarmed(),
        Some(calibration.clone()),
    )
    .build()
    .await
    .expect("attach");
    let mem = Arc::new(mem);
    assert!(
        mem.pipeline().calibration().is_none(),
        "the probe must still be running for this test to mean anything"
    );
    let handles = calibration.abort_handles();
    assert_eq!(handles.len(), 1, "one probe to abort");
    let out = run_and_close(
        Arc::clone(&mem),
        async { Ok(()) },
        tokio::spawn(async {}),
        &handles,
        &EarlyShutdown::unarmed(),
        &ShutdownProgress::new(),
    )
    .await;
    assert!(out.is_ok(), "{out:?}");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let after = calls.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(calls.load(Ordering::SeqCst), after, "the probe stopped");
    assert!(after < crate::writeq::PROBE_EMBEDS, "it was cut short");
    assert!(mem.pipeline().calibration().is_none());
}
