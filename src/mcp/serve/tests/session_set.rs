//! #32 PR 2: the per-session shutdown stages over a set of sessions.

use super::*;

/// Outputs come back in input order, whatever order the futures finish in.
#[tokio::test(start_paused = true)]
async fn join_all_returns_outputs_in_input_order() {
    let out = join_all(
        [30_u64, 10, 20]
            .into_iter()
            .map(|ms| async move {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                ms
            })
            .collect(),
    )
    .await;
    assert_eq!(out, vec![30, 10, 20]);
}

/// The futures run concurrently: two 100 ms waits take 100 ms, not 200, so
/// one stage bound covers a whole set (#32 design §3.5).
#[tokio::test(start_paused = true)]
async fn join_all_runs_the_set_concurrently() {
    let started = tokio::time::Instant::now();
    // Each wait starts when its future is first polled (a bare `sleep`
    // fixes its deadline when created, which would hide a serial join).
    join_all(
        (0..2)
            .map(|_| async { tokio::time::sleep(Duration::from_millis(100)).await })
            .collect(),
    )
    .await;
    assert_eq!(started.elapsed(), Duration::from_millis(100));
}

/// An empty set is done at once.
#[tokio::test]
async fn join_all_of_nothing_is_empty() {
    let out: Vec<()> = join_all(Vec::<std::future::Ready<()>>::new()).await;
    assert!(out.is_empty());
}

#[cfg(all(feature = "store-memory", feature = "embed-fixture"))]
mod closes {
    use super::*;
    use crate::embed::{Embedder, FixtureEmbedder};
    use crate::graph::action::Action;
    use crate::memory::Memory;
    use crate::store::{GraphStore, MemoryStore};
    use crate::test_util::capture_logs;
    use crate::types::EmbeddingContract;

    async fn mem(store: &Arc<dyn GraphStore>, session: &str) -> Arc<Memory> {
        let m = Memory::builder()
            .session(session)
            .agent("agent-a")
            .flush_interval(Duration::from_secs(3_600))
            .store(Arc::clone(store))
            .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
            .embedding_contract(EmbeddingContract {
                kind: "fixture".into(),
                model: None,
                dim: 1024,
            })
            .build()
            .await
            .expect("build");
        Arc::new(m)
    }

    fn is_closed(m: &Memory) -> bool {
        let action = Action {
            event_time: None,
            action: "post-close write",
            produces: &[],
            modifies: &[],
            depends_on: &[],
        };
        m.record_action(&action).is_err()
    }

    /// Every session in the set is closed and its pump aborted, the stage
    /// lines appear once each, and each session's outcome is logged.
    #[tokio::test]
    async fn the_set_wide_close_closes_every_session() {
        let (logs, _guard) = capture_logs(tracing::Level::INFO);
        let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let a = mem(&store, "serve-set-a").await;
        let b = mem(&store, "serve-set-b").await;
        let pump_a = tokio::spawn(std::future::pending::<()>());
        let pump_b = tokio::spawn(std::future::pending::<()>());
        let set = [
            SessionClose {
                mem: &a,
                event_pump: &pump_a,
            },
            SessionClose {
                mem: &b,
                event_pump: &pump_b,
            },
        ];
        let out = run_and_close_sessions(
            &set,
            async { Ok(()) },
            &[],
            &EarlyShutdown::unarmed(),
            &ShutdownProgress::new(),
        )
        .await;
        assert!(out.is_ok(), "{out:?}");
        assert!(is_closed(&a) && is_closed(&b), "both sessions are closed");
        for pump in [pump_a, pump_b] {
            let joined = tokio::time::timeout(Duration::from_secs(5), pump)
                .await
                .expect("an aborted pump ends");
            assert!(joined.unwrap_err().is_cancelled());
        }
        let lines = logs.lines();
        let count = |needle: &str| lines.iter().filter(|l| l.contains(needle)).count();
        assert_eq!(count("shutdown stage 3/7 session_close started"), 1);
        assert_eq!(count("shutdown stage 4/7 event_pump_abort started"), 1);
        assert_eq!(count("lambo serve: session closed, tail durable"), 2);
    }
}
