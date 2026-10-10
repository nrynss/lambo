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

/// #32 review L3: one member's panic does not cancel the others. The
/// sibling, still pending when the panic lands, runs to completion, and only
/// then does the panic reach the caller.
#[tokio::test(start_paused = true)]
async fn a_panicking_member_does_not_cancel_its_siblings() {
    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sibling = {
        let finished = Arc::clone(&finished);
        async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            finished.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    };
    let panicking = async {
        tokio::time::sleep(Duration::from_millis(10)).await;
        panic!("a member's close panicked");
    };
    let futures: Vec<std::pin::Pin<Box<dyn Future<Output = ()> + Send>>> =
        vec![Box::pin(panicking), Box::pin(sibling)];
    let joined = tokio::spawn(join_all(futures)).await;
    let err = joined.expect_err("the panic still reaches the caller");
    assert!(err.is_panic(), "{err:?}");
    let payload = err.into_panic();
    assert_eq!(
        payload.downcast_ref::<&str>().copied(),
        Some("a member's close panicked")
    );
    assert!(
        finished.load(std::sync::atomic::Ordering::SeqCst),
        "the sibling ran to completion before the panic was resumed"
    );
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
            || {},
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
        // #32 review L2: with more than one session, each outcome line names
        // its session, in set order.
        let outcomes: Vec<String> = lines
            .iter()
            .map(|l| plain(l))
            .filter(|l| l.contains("lambo serve: session closed, tail durable"))
            .collect();
        assert!(outcomes[0].contains("session=serve-set-a"), "{outcomes:?}");
        assert!(outcomes[1].contains("session=serve-set-b"), "{outcomes:?}");
    }

    /// #32 review M1: stages 3 and 4 stand on their own, for a detach (design
    /// §3.4). `close_sessions` closes the session and aborts its pump, and
    /// logs neither stage 1 nor stage 2: those are the process's.
    #[tokio::test]
    async fn close_sessions_runs_only_the_per_session_stages() {
        let (logs, _guard) = capture_logs(tracing::Level::INFO);
        let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let a = mem(&store, "serve-detach-only").await;
        let pump = tokio::spawn(std::future::pending::<()>());
        let set = [SessionClose {
            mem: &a,
            event_pump: &pump,
        }];
        let progress = ShutdownProgress::for_session("serve-detach-only");
        let out = close_sessions(&set, &EarlyShutdown::unarmed(), &progress)
            .await
            .report();
        assert!(out.is_ok(), "{out:?}");
        assert!(is_closed(&a), "the session is closed");
        let joined = tokio::time::timeout(Duration::from_secs(5), pump)
            .await
            .expect("an aborted pump ends");
        assert!(joined.unwrap_err().is_cancelled());

        let lines: Vec<String> = logs.lines().iter().map(|l| plain(l)).collect();
        let stages: Vec<&String> = lines
            .iter()
            .filter(|l| l.contains("lambo serve: shutdown stage "))
            .collect();
        assert_eq!(stages.len(), 4, "{stages:?}");
        for (line, needle) in stages.iter().zip([
            "shutdown stage 3/7 session_close started",
            "shutdown stage 3/7 session_close finished in ",
            "shutdown stage 4/7 event_pump_abort started",
            "shutdown stage 4/7 event_pump_abort finished in ",
        ]) {
            assert!(line.contains(needle), "{line}");
            assert!(line.contains("session=serve-detach-only"), "{line}");
        }
        let closed: Vec<&String> = lines
            .iter()
            .filter(|l| l.contains("lambo serve: session closed, tail durable"))
            .collect();
        assert_eq!(closed.len(), 1, "{lines:?}");
        // A set of one logs the outcome line a single-session serve always
        // has: no `session` field (#32 review L2).
        assert!(!closed[0].contains("session="), "{}", closed[0]);
    }
}

/// #32 review L4: the set-wide close's risky branches, through a store
/// that can slow its lease release (the last step of a clean close) or fail
/// its flush.
#[cfg(all(feature = "store-memory", feature = "embed-fixture"))]
mod folds {
    use super::*;
    use crate::embed::{Embedder, FixtureEmbedder};
    use crate::graph::action::Action;
    use crate::memory::Memory;
    use crate::store::lease::{LeaseHolder, LeaseInfo, LeaseOutcome};
    use crate::store::{Capabilities, GraphStore, MemoryStore};
    use crate::test_util::capture_logs;
    use crate::types::{
        CanonizationEvent, EmbeddingContract, GraphSnapshot, InteractionSpan, MutationBatch,
        NodeId, Scored, SessionId, StoreError,
    };

    /// `MemoryStore`, with a slow lease release or a failing flush.
    struct Store {
        inner: MemoryStore,
        release_delay: Duration,
        /// `Some(label)`: every flush fails with an error naming `label`.
        fail_flush: Option<&'static str>,
    }

    #[async_trait::async_trait]
    impl GraphStore for Store {
        async fn init_schema(&self) -> Result<(), StoreError> {
            self.inner.init_schema().await
        }
        fn capabilities(&self) -> Capabilities {
            self.inner.capabilities()
        }
        fn vector_dimensions(&self) -> Option<usize> {
            self.inner.vector_dimensions()
        }
        async fn flush(&self, batch: &MutationBatch, token: Option<u64>) -> Result<(), StoreError> {
            match self.fail_flush {
                Some(label) => Err(StoreError::Backend(format!("simulated outage on {label}"))),
                None => self.inner.flush(batch, token).await,
            }
        }
        async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
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
        async fn blast_radius(
            &self,
            session: &SessionId,
            node: NodeId,
            min_edge_age: Duration,
            now: chrono::DateTime<chrono::Utc>,
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
            now: chrono::DateTime<chrono::Utc>,
        ) -> Result<InteractionSpan, StoreError> {
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
            holder: &LeaseHolder,
            ttl: Duration,
        ) -> Result<LeaseOutcome, StoreError> {
            self.inner.acquire_lease(session, holder, ttl).await
        }
        async fn read_lease(&self, session: &SessionId) -> Result<Option<LeaseInfo>, StoreError> {
            self.inner.read_lease(session).await
        }
        async fn refresh_lease(
            &self,
            session: &SessionId,
            holder: &LeaseHolder,
            ttl: Duration,
        ) -> Result<LeaseOutcome, StoreError> {
            self.inner.refresh_lease(session, holder, ttl).await
        }
        async fn release_lease(
            &self,
            session: &SessionId,
            holder: &LeaseHolder,
        ) -> Result<(), StoreError> {
            tokio::time::sleep(self.release_delay).await;
            self.inner.release_lease(session, holder).await
        }
    }

    async fn mem(session: &str, store: Store) -> Arc<Memory> {
        let m = Memory::builder()
            .session(session)
            .agent("agent-a")
            .flush_interval(Duration::from_secs(3_600))
            .store(Arc::new(store) as Arc<dyn GraphStore>)
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

    fn store() -> Store {
        Store {
            inner: MemoryStore::new(),
            release_delay: Duration::ZERO,
            fail_flush: None,
        }
    }

    /// A session whose close fails at its final flush: it has a tail, and
    /// its store refuses every flush.
    async fn failing(session: &'static str) -> Arc<Memory> {
        let m = mem(
            session,
            Store {
                fail_flush: Some(session),
                ..store()
            },
        )
        .await;
        m.record_action(&action("a write the close must flush"))
            .expect("write before the close");
        m
    }

    fn action(what: &'static str) -> Action<'static> {
        Action {
            event_time: None,
            action: what,
            produces: &[],
            modifies: &[],
            depends_on: &[],
        }
    }

    fn is_closed(m: &Memory) -> bool {
        m.record_action(&action("post-close write")).is_err()
    }

    async fn close_set(
        mems: &[&Arc<Memory>],
        transport: Result<(), LamboError>,
    ) -> Result<(), LamboError> {
        let pumps: Vec<_> = mems
            .iter()
            .map(|_| tokio::spawn(std::future::pending::<()>()))
            .collect();
        let set: Vec<SessionClose<'_>> = mems
            .iter()
            .zip(&pumps)
            .map(|(mem, pump)| SessionClose {
                mem,
                event_pump: pump,
            })
            .collect();
        run_and_close_sessions(
            &set,
            async { transport },
            || {},
            &EarlyShutdown::unarmed(),
            &ShutdownProgress::new(),
        )
        .await
    }

    fn outcome_lines(logs: &crate::test_util::CapturedLogs) -> Vec<String> {
        logs.lines()
            .iter()
            .map(|l| plain(l))
            .filter(|l| {
                l.contains("lambo serve: session closed, tail durable")
                    || l.contains("lambo serve: final flush failed")
            })
            .collect()
    }

    /// Stage 3 closes the set concurrently: two closes that each spend one
    /// second releasing their lease take one second together, not two. A
    /// serial loop of `.await`s over the set fails this.
    #[tokio::test(start_paused = true)]
    async fn stage_three_closes_the_set_concurrently() {
        const DELAY: Duration = Duration::from_secs(1);
        let slow = || Store {
            release_delay: DELAY,
            ..store()
        };
        let a = mem("serve-conc-a", slow()).await;
        let b = mem("serve-conc-b", slow()).await;
        let pumps = [
            tokio::spawn(std::future::pending::<()>()),
            tokio::spawn(std::future::pending::<()>()),
        ];
        let set = [
            SessionClose {
                mem: &a,
                event_pump: &pumps[0],
            },
            SessionClose {
                mem: &b,
                event_pump: &pumps[1],
            },
        ];
        let started = tokio::time::Instant::now();
        let out = close_sessions(&set, &EarlyShutdown::unarmed(), &ShutdownProgress::new())
            .await
            .report();
        let elapsed = started.elapsed();
        assert!(out.is_ok(), "{out:?}");
        assert!(is_closed(&a) && is_closed(&b));
        assert!(
            elapsed >= DELAY && elapsed < 2 * DELAY,
            "two {DELAY:?} closes took {elapsed:?}: stage 3 ran them one after the other"
        );
    }

    /// One session's failure does not stop the others: every session is
    /// closed, each outcome is logged in set order with its session, and the
    /// FIRST error is the one returned.
    #[tokio::test]
    async fn the_first_close_error_is_returned_and_every_session_still_closes() {
        let (logs, _guard) = capture_logs(tracing::Level::INFO);
        let good = mem("serve-fold-good", store()).await;
        let bad_b = failing("serve-fold-b").await;
        let bad_c = failing("serve-fold-c").await;

        let out = close_set(&[&good, &bad_b, &bad_c], Ok(())).await;

        let err = out.expect_err("a failed close is reported").to_string();
        assert!(err.contains("simulated outage on serve-fold-b"), "{err}");
        for m in [&good, &bad_b, &bad_c] {
            assert!(is_closed(m), "{}: closed to writers", m.session());
        }
        let lines = outcome_lines(&logs);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(
            lines[0].contains("session closed, tail durable"),
            "{lines:?}"
        );
        assert!(lines[0].contains("session=serve-fold-good"), "{lines:?}");
        assert!(lines[1].contains("final flush failed"), "{lines:?}");
        assert!(lines[1].contains("session=serve-fold-b"), "{lines:?}");
        assert!(lines[2].contains("final flush failed"), "{lines:?}");
        assert!(lines[2].contains("session=serve-fold-c"), "{lines:?}");
    }

    /// A transport error wins over every close outcome, and those outcomes
    /// are not logged (as before the set): but every session is still closed.
    #[tokio::test]
    async fn a_transport_error_wins_and_the_closes_still_run() {
        let (logs, _guard) = capture_logs(tracing::Level::INFO);
        let good = mem("serve-tx-good", store()).await;
        let bad = failing("serve-tx-bad").await;

        let out = close_set(
            &[&good, &bad],
            Err(LamboError::Config("the transport broke".into())),
        )
        .await;

        let err = out
            .expect_err("the transport error is surfaced")
            .to_string();
        assert!(err.contains("the transport broke"), "{err}");
        assert!(is_closed(&good) && is_closed(&bad), "both closes ran");
        let lines = outcome_lines(&logs);
        assert!(lines.is_empty(), "{lines:?}");
    }
}

/// `line` without its ANSI colour sequences, so a field reads `key=value`.
fn plain(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c in chars.by_ref() {
                if c == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// `ShutdownProgress::for_session` (#32 design §3.4): a detach's stage lines
/// read exactly like the process's, plus a `session` field; the process's
/// own lines carry none.
#[test]
fn a_session_progress_names_its_session_and_the_process_progress_does_not() {
    let (logs, _guard) = crate::test_util::capture_logs(tracing::Level::INFO);
    let session = ShutdownProgress::for_session("serve-detach-a");
    session.run(Stage::SessionClose, || {});
    session.complete();
    let detach: Vec<String> = logs.lines().iter().map(|l| plain(l)).collect();
    assert_eq!(detach.len(), 3, "{detach:?}");
    for (line, needle) in detach.iter().zip([
        "lambo serve: shutdown stage 3/7 session_close started",
        "lambo serve: shutdown stage 3/7 session_close finished in ",
        "lambo serve: session detach finished in ",
    ]) {
        assert!(line.contains(needle), "{line}");
        assert!(line.contains("session=serve-detach-a"), "{line}");
    }

    let process = ShutdownProgress::default();
    process.run(Stage::SessionClose, || {});
    process.complete();
    let lines: Vec<String> = logs.lines().iter().map(|l| plain(l)).collect();
    let process_lines = &lines[detach.len()..];
    assert_eq!(process_lines.len(), 3, "{process_lines:?}");
    for line in process_lines {
        assert!(!line.contains("session="), "{line}");
    }
    assert!(
        process_lines[2].contains("lambo serve: shutdown finished in "),
        "{}",
        process_lines[2]
    );
}

/// Stage 6 over a set, with the sessions shared the way #32 PR 4's registry
/// shares them (`Arc<AttachedSession>`, design §3.1).
#[cfg(all(unix, feature = "store-memory", feature = "embed-fixture"))]
mod endpoints {
    use super::*;
    use crate::embed::{Embedder, FixtureEmbedder};
    use crate::mcp::serve::hub::ENDPOINT_RELEASE_GRACE;
    use crate::mcp::serve::session::AttachedSession;
    use crate::mcp::SessionEndpoint;
    use crate::memory::Memory;
    use crate::store::{GraphStore, MemoryStore, StoreConfig, StoreKind};
    use crate::types::EmbeddingContract;

    async fn attached(
        dir: &crate::test_util::ScratchDir,
        session: &str,
    ) -> (Arc<AttachedSession>, SessionEndpoint) {
        let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let mem = Memory::builder()
            .session(session)
            .agent("agent-a")
            .flush_interval(Duration::from_secs(3_600))
            .store(store)
            .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
            .embedding_contract(EmbeddingContract {
                kind: "fixture".into(),
                model: None,
                dim: 1024,
            })
            .build()
            .await
            .expect("build");
        let mem = Arc::new(mem);
        // The store config only feeds the address derivation.
        let store_cfg = StoreConfig {
            kind: StoreKind::Sqlite,
            path: Some(dir.join("s.db").to_str().expect("utf-8").into()),
            ..StoreConfig::default()
        };
        let endpoint = SessionEndpoint::resolve_in(&dir.join("run"), session, &store_cfg)
            .expect("endpoint fits");
        let server = LamboServer::new(Arc::clone(&mem));
        let attached = AttachedSession::attach(
            mem,
            server,
            Some(endpoint.clone()),
            4,
            crate::mcp::serve::session::HostCheck::Loopback,
            0,
        );
        assert!(endpoint.path().exists(), "{session}: the endpoint is bound");
        (Arc::new(attached), endpoint)
    }

    /// #32 review M2 and L4: every session's endpoint is released through a
    /// shared reference while other clones are alive, concurrently, and a
    /// second release (a detach racing the shutdown) is a no-op.
    #[tokio::test]
    async fn stage_six_releases_every_shared_session_once() {
        let dir = crate::test_util::ScratchDir::short("ss");
        let (a, endpoint_a) = attached(&dir, "serve-set-ep-a").await;
        let (b, endpoint_b) = attached(&dir, "serve-set-ep-b").await;
        assert_ne!(endpoint_a.path(), endpoint_b.path());
        // What a router or an in-flight request would hold.
        let held = [Arc::clone(&a), Arc::clone(&b)];

        tokio::time::timeout(
            ENDPOINT_RELEASE_GRACE + Duration::from_secs(2),
            join_all(
                [&a, &b]
                    .into_iter()
                    .map(|session| session.release_endpoint())
                    .collect(),
            ),
        )
        .await
        .expect("stage 6 over the set is bounded by one ENDPOINT_RELEASE_GRACE");

        for endpoint in [&endpoint_a, &endpoint_b] {
            assert!(
                !endpoint.path().exists(),
                "{}: the socket file survived release",
                endpoint.path().display()
            );
            assert!(
                tokio::net::UnixStream::connect(endpoint.path())
                    .await
                    .is_err(),
                "nothing accepts on a released endpoint"
            );
        }

        // Released once: a second call finds no hub and returns at once.
        tokio::time::timeout(Duration::from_millis(100), held[0].release_endpoint())
            .await
            .expect("a second release is a no-op");

        for session in [&a, &b] {
            session.mem.close().await.expect("close");
        }
    }

    /// #32 review L1: a second release racing the first waits for it, so
    /// neither caller returns while the socket is still there.
    #[tokio::test]
    async fn a_racing_second_release_returns_only_once_the_endpoint_is_gone() {
        let dir = crate::test_util::ScratchDir::short("sr");
        let (a, endpoint) = attached(&dir, "serve-set-race").await;
        let first = a.release_endpoint();
        let second = async {
            a.release_endpoint().await;
            endpoint.path().exists()
        };
        let ((), survived) =
            tokio::time::timeout(ENDPOINT_RELEASE_GRACE + Duration::from_secs(2), async {
                tokio::join!(first, second)
            })
            .await
            .expect("both releases are bounded");
        assert!(
            !survived,
            "the second release returned before the socket was removed"
        );
        a.mem.close().await.expect("close");
    }

    /// #32 review L3: a session dropped without stage 6 still stops its
    /// accept loop, through the `Hub`'s `Drop` (#28).
    #[tokio::test]
    async fn a_session_dropped_without_a_release_stops_accepting() {
        let dir = crate::test_util::ScratchDir::short("sd");
        let (a, endpoint) = attached(&dir, "serve-set-drop").await;
        assert!(
            tokio::net::UnixStream::connect(endpoint.path())
                .await
                .is_ok(),
            "a bound endpoint accepts"
        );
        a.mem.close().await.expect("close");
        drop(a);
        let stopped = tokio::time::timeout(Duration::from_secs(2), async {
            while tokio::net::UnixStream::connect(endpoint.path())
                .await
                .is_ok()
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            stopped.is_ok(),
            "the dropped session's accept loop still accepts"
        );
    }
}
