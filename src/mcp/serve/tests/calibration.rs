use super::*;
use crate::embed::{EmbedError, Embedder, FixtureEmbedder};
use crate::store::{MemoryStore, StoreConfig};
use crate::types::EmbeddingContract;
use crate::writeq::EmbedderCalibration;
use std::sync::atomic::{AtomicUsize, Ordering};

/// What a test sees of its embedder: the embed count, and a gate that
/// holds every embed after the first until released (P3-5: a probe is
/// "still running" by construction, not by outrunning a delay).
#[derive(Clone)]
struct Probe {
    calls: Arc<AtomicUsize>,
    gate: Option<Arc<tokio::sync::Semaphore>>,
}

impl Probe {
    fn ungated() -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            gate: None,
        }
    }

    fn gated() -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            gate: Some(Arc::new(tokio::sync::Semaphore::new(1))),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Open the gate for good (a closed semaphore's `acquire` fails at once).
    fn release(&self) {
        if let Some(gate) = &self.gate {
            gate.close();
        }
    }

    /// Wait until the probe's second embed is parked on the gate.
    async fn parked(&self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while self.calls() < 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "the probe never reached the gate"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
}

/// The fixture embedder, counting every embed and honouring the gate.
struct Counting {
    inner: FixtureEmbedder,
    probe: Probe,
}

#[async_trait::async_trait]
impl Embedder for Counting {
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        self.probe.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.probe.gate
            && let Ok(permit) = gate.acquire().await
        {
            permit.forget();
        }
        self.inner.embed(text).await
    }
}

fn backends(probe: &Probe) -> ResolvedBackends {
    ResolvedBackends {
        store: Box::new(MemoryStore::new()),
        embedder: Box::new(Counting {
            inner: FixtureEmbedder::new(),
            probe: probe.clone(),
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
    let probe = Probe::ungated();
    let calibration = EmbedderCalibration::new();
    let template = serve_builder(
        &ServeOptions::new("serve-cal-a", "agent-a"),
        backends(&probe),
        None,
        None,
        EarlyShutdown::unarmed(),
        Some(calibration.clone()),
    );
    let a = template.clone().build().await.expect("a attaches");
    probe_landed(&a).await;
    let probed = probe.calls();
    assert_eq!(probed, crate::writeq::PROBE_EMBEDS);

    let b = template
        .session("serve-cal-b")
        .build()
        .await
        .expect("b attaches");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        probe.calls(),
        probed,
        "the second session fires no probe embed"
    );
    assert_eq!(calibration.probes(), 1);
    assert!(b.pipeline().calibration().is_some(), "and reads the probe");
    a.close().await.expect("close a");
    b.close().await.expect("close b");
}

/// The process tasks `serve` builds its stage-2 and stage-5 abort lists
/// from, holding the process's calibration and no task of their own (the
/// keep-warm, heartbeat and poller have their own tests).
fn process_tasks(calibration: &EmbedderCalibration) -> ProcessTasks {
    ProcessTasks {
        heartbeat: None,
        keep_warm: None,
        refusal_poller: None,
        calibration: calibration.clone(),
    }
}

/// A session over `probe`'s embedder, built from the serve builder with the
/// process's calibration.
async fn attach(session: &str, probe: &Probe, calibration: &EmbedderCalibration) -> Arc<Memory> {
    let mem = serve_builder(
        &ServeOptions::new(session, "agent-a"),
        backends(probe),
        None,
        None,
        EarlyShutdown::unarmed(),
        Some(calibration.clone()),
    )
    .build()
    .await
    .expect("attach");
    Arc::new(mem)
}

/// Stage 2 aborts the shared probe, so a probe still running when the
/// transport stops does not run across the close. The list is
/// [`ProcessTasks::stop_before_close`], the one `serve` hands
/// `run_and_close_sessions` (review P3-1: dropping the calibration from it
/// fails this test).
#[tokio::test]
async fn stage_two_aborts_the_shared_probe() {
    let probe = Probe::gated();
    let calibration = EmbedderCalibration::new();
    let tasks = process_tasks(&calibration);
    let mem = attach("serve-cal-stage-2", &probe, &calibration).await;
    probe.parked().await;
    let handles = tasks.stop_before_close();
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
    let at_close = probe.calls();
    probe.release();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(probe.calls(), at_close, "the probe stopped at stage 2");
    assert!(mem.pipeline().calibration().is_none());
}

/// Stage 5 ([`ProcessTasks::stop`]) aborts the shared probe too, on any
/// path that reaches it with the probe still running.
#[tokio::test]
async fn stage_five_aborts_the_shared_probe() {
    let probe = Probe::gated();
    let calibration = EmbedderCalibration::new();
    let tasks = process_tasks(&calibration);
    let mem = attach("serve-cal-stage-5", &probe, &calibration).await;
    probe.parked().await;
    tasks.stop();
    let at_stop = probe.calls();
    probe.release();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(probe.calls(), at_stop, "the probe stopped at stage 5");
    assert!(mem.pipeline().calibration().is_none());
    mem.close().await.expect("close");
}
