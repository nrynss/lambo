//! Close and drain: tail custody, bounded and cancelled closes, the
//! writers gate and the keep-warm ordering.

use super::*;

/// **L82-1, end to end.** A burst left un-flushed at SIGTERM must drain
/// inside `close()`'s window against a store that charges a real cluster's
/// per-statement latency.
///
/// Both halves run so the test *shows* the finding as well as the fix. With
/// the pre-L82-1 cost model — one round-trip per mutation, which is what
/// both adapters' `for m in &batch.mutations` loop bought — the close blows
/// `CLOSE_FLUSH_GRACE` exactly as the live run did. With the planned-
/// statement model it finishes in a fraction of it.
///
/// The cost model is `store::batch`'s own plan, so this test pins the
/// *arithmetic*, not the adapters' use of it: `store::batch`'s tests pin
/// that the plan is small, and `cockroach::sql_shape_is_a_multi_row_upsert`
/// pins that one planned step really is one statement. The three together
/// are what close the loop — no local test can reach a cluster.
///
/// Time is paused, so the sleeps are simulated and the test is instant.
#[tokio::test(start_paused = true)]
async fn an_at_cap_burst_drains_within_the_close_window() {
    let rtt = Duration::from_millis(30);

    // The regression, reproduced: per-mutation round-trips cannot drain.
    let slow = Arc::new(RoundTripStore::new(
        Arc::new(MemoryStore::new()),
        CostModel::PerMutation,
        rtt,
    ));
    let mem = memory_on(slow.clone(), "l82-1-per-mutation").await;
    at_cap_burst(&mem).await;
    let undrained = mem.stats().log_depth;
    assert!(
        undrained >= 700,
        "the burst must leave a realistic tail, got {undrained}"
    );
    assert!(
        tokio::time::timeout(crate::mcp::serve::CLOSE_FLUSH_GRACE, mem.close())
            .await
            .is_err(),
        "per-mutation round-trips must NOT fit the close window — if this stops timing out \
             the cost model no longer reflects what the pre-L82-1 adapters did, and the other \
             half of this test proves nothing"
    );

    // The fix: the same tail, planned into statements.
    let fast = Arc::new(RoundTripStore::new(
        Arc::new(MemoryStore::new()),
        CostModel::PerPlannedStatement,
        rtt,
    ));
    let mem = memory_on(fast.clone(), "l82-1-planned").await;
    at_cap_burst(&mem).await;
    let start = tokio::time::Instant::now();
    tokio::time::timeout(crate::mcp::serve::CLOSE_FLUSH_GRACE, mem.close())
        .await
        .expect("close must fit the grace window")
        .expect("close must succeed");
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "the tail drained in {elapsed:?}; it must be a handful of round-trips, not a \
             close call against the window"
    );
    assert!(
        fast.round_trips() < 40,
        "the whole session cost {} round-trips — the burst must plan into statements, not \
             rows (L82-1)",
        fast.round_trips()
    );
}

/// Issue #13: the embedder keep-warm is stopped when the transport returns,
/// **before** the close starts, not after it.
///
/// `run_and_close` is handed the task's abort handle and must use it ahead
/// of `close_bounded`; `serve` aborts the same task again after the close
/// as a backstop, so the post-close abort alone would still pass every
/// other test. What tells the orders apart is a close that takes time: this
/// one pays 30 ms per statement, so while it is in flight the handed task
/// must already be gone. The ordering is observed at 1 ms, with the close
/// demonstrably unfinished, so aborting after the close fails it.
#[tokio::test(start_paused = true)]
async fn the_keep_warm_is_stopped_before_the_close_starts() {
    struct DropFlag(Arc<AtomicBool>);
    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    let store = Arc::new(RoundTripStore::new(
        Arc::new(MemoryStore::new()),
        CostModel::PerPlannedStatement,
        Duration::from_millis(30),
    ));
    let mem = Arc::new(memory_on(store.clone(), "i13-keep-warm-before-close").await);
    at_cap_burst(&mem).await;

    let gone = Arc::new(AtomicBool::new(false));
    let keep_warm = {
        let flag = DropFlag(Arc::clone(&gone));
        tokio::spawn(async move {
            let _flag = flag;
            std::future::pending::<()>().await
        })
    };
    let handles = vec![keep_warm.abort_handle()];
    let pump = tokio::spawn(async {});
    let closing = tokio::spawn(async move {
        crate::mcp::serve::run_and_close(
            mem,
            async { Ok(()) },
            pump,
            &handles,
            &crate::mcp::serve::EarlyShutdown::unarmed(),
            &crate::mcp::serve::ShutdownProgress::new(),
        )
        .await
    });

    tokio::time::sleep(Duration::from_millis(1)).await;
    assert!(
        !closing.is_finished(),
        "control: the close must still be in flight, or this test observes nothing"
    );
    assert!(
        gone.load(Ordering::SeqCst),
        "the keep-warm must be stopped before the close starts, so a touch cannot \
             start (or sit in flight) across the final drain"
    );
    closing
        .await
        .expect("run_and_close does not panic")
        .expect("the burst drains inside the close window");
}

/// How [`AdverseStore::flush`] misbehaves — the two failure modes the
/// background flush path is armored against (STORE-2 timeout + panic
/// containment) plus a plain delay for the concurrency tests.
#[derive(Clone, Copy, Debug)]
enum FlushBehaviour {
    /// Never returns: the hung backend `FLUSH_ATTEMPT_TIMEOUT` exists for.
    Hang,
    /// Hangs the **first** flush and delegates every later one: a store
    /// that is unresponsive when the caller gives up on `close()` and
    /// healthy when it retries (R2-1).
    HangOnce,
    /// Unwinds inside the flush: the panicking adapter `CatchUnwindPoll`
    /// exists for.
    Panic,
    /// Succeeds, slowly.
    Delay(Duration),
}

/// Store double for `close()`'s step-4 armor (T81-2) and for the concurrent
/// -close test (T81-6). Everything except `flush` delegates.
struct AdverseStore {
    inner: Arc<dyn GraphStore>,
    behaviour: FlushBehaviour,
    flush_calls: AtomicUsize,
    flush_completed: AtomicBool,
    /// Armed for [`FlushBehaviour::HangOnce`]; disarmed by the first flush.
    hang_armed: AtomicBool,
}

impl AdverseStore {
    fn new(inner: Arc<dyn GraphStore>, behaviour: FlushBehaviour) -> Self {
        Self {
            inner,
            behaviour,
            flush_calls: AtomicUsize::new(0),
            flush_completed: AtomicBool::new(false),
            hang_armed: AtomicBool::new(true),
        }
    }

    fn flush_calls(&self) -> usize {
        self.flush_calls.load(Ordering::SeqCst)
    }

    /// `true` once a `flush` has actually returned from the backend.
    fn flush_completed(&self) -> bool {
        self.flush_completed.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl GraphStore for AdverseStore {
    async fn init_schema(&self) -> Result<(), StoreError> {
        self.inner.init_schema().await
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    async fn flush(&self, batch: &MutationBatch, token: Option<u64>) -> Result<(), StoreError> {
        self.flush_calls.fetch_add(1, Ordering::SeqCst);
        match self.behaviour {
            FlushBehaviour::Hang => std::future::pending::<()>().await,
            FlushBehaviour::HangOnce => {
                if self.hang_armed.swap(false, Ordering::SeqCst) {
                    std::future::pending::<()>().await
                }
            }
            FlushBehaviour::Panic => panic!("store adapter exploded mid-flush"),
            FlushBehaviour::Delay(d) => tokio::time::sleep(d).await,
        }
        let result = self.inner.flush(batch, token).await;
        self.flush_completed.store(true, Ordering::SeqCst);
        result
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
}

// -- close / drain ------------------------------------------------------

#[tokio::test]
async fn close_flushes_the_tail() {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let mem = memory_on(store.clone(), "tail").await;

    mem.derive(
        &[
            ("user schema", ConceptType::Entity),
            ("must stay backward compatible", ConceptType::Constraint),
        ],
        &ParentOf::none(),
    )
    .await
    .unwrap();
    mem.record_action(&Action {
        event_time: None,
        action: "created migrations/003.sql",
        produces: &["migrations/003.sql"],
        modifies: &[],
        depends_on: &["user schema"],
    })
    .unwrap();

    // Nothing durable yet — the flush interval is an hour.
    assert!(store.load_session(&SessionId::new("tail")).await.is_err());
    assert!(mem.stats().log_depth > 0);

    mem.close().await.unwrap();

    let snap = store.load_session(&SessionId::new("tail")).await.unwrap();
    assert!(snap.concepts.iter().any(|c| c.content == "user schema"));
    assert!(snap
        .concepts
        .iter()
        .any(|c| c.content == "created migrations/003.sql"));
    assert_eq!(snap.interactions.len(), 2, "one interaction per write");
}

/// A session whose flush task is guaranteed to have RETAINED a batch: one
/// attempt per cycle (`retries = 0`), that attempt fails, and the resulting
/// `RETAINED_BACKOFF` hold (10s) keeps any further store call out of the
/// short virtual-time windows these tests drive. Returns the retained count.
async fn retained_batch_session(session: &str, store: Arc<dyn GraphStore>) -> (Memory, usize) {
    let mem = Memory::builder()
        .config(Config {
            backend_flush_retries: 0,
            ..Config::default()
        })
        .session(session)
        .agent("agent-a")
        .flush_interval(Duration::from_millis(100))
        .store(store)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .build()
        .await
        .unwrap();

    mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
        .await
        .unwrap();
    let retained = mem.graph().read().log_len();
    assert!(retained > 0);

    // One cycle: drain the log, attempt once, fail, retain + hold.
    for _ in 0..10 {
        tokio::time::advance(Duration::from_millis(150)).await;
        tokio::task::yield_now().await;
        if mem.graph().read().log_len() == 0 && mem.stats().flush_depth > 0 {
            break;
        }
    }
    assert_eq!(
        mem.graph().read().log_len(),
        0,
        "the flush task drained the log into its pending buffer"
    );
    assert_eq!(
        mem.stats().flush_depth,
        retained,
        "the batch is retained in the task — not durable, and invisible to drain_log"
    );
    (mem, retained)
}

/// The retained-batch case the COH-6 design exists for: the flush task
/// drained the log and failed to persist it, so those mutations live only
/// in the task's `pending` buffer. `close()` must get them back (via the
/// stop signal's push-front) and make them durable — a hard abort would
/// drop them with the task.
#[tokio::test(start_paused = true)]
async fn close_flushes_a_batch_retained_after_a_failed_flush() {
    let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    // Exactly one failing flush; every later attempt (close's) succeeds.
    let store: Arc<dyn GraphStore> = Arc::new(FlakyStore::new(inner.clone(), 1));
    let (mem, _retained) = retained_batch_session("retained", store).await;

    assert!(
        inner
            .load_session(&SessionId::new("retained"))
            .await
            .is_err(),
        "nothing durable yet"
    );

    mem.close().await.unwrap();

    let snap = inner
        .load_session(&SessionId::new("retained"))
        .await
        .unwrap();
    assert!(
        snap.concepts.iter().any(|c| c.content == "user schema"),
        "a retained batch must be flushed by close(), never dropped"
    );
}

/// Pins the push-**front** mechanism itself: returned custody lands at the
/// FRONT of the graph log, so `close()`'s final batch carries the retained
/// mutations *and* everything written after them, as one chronological
/// batch. With a permanently failing store the attempt must also surface as
/// `close()`'s error rather than being swallowed.
#[tokio::test(start_paused = true)]
async fn close_returns_the_retained_batch_to_the_front_of_the_log() {
    let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let flaky = Arc::new(FlakyStore::new(inner, usize::MAX)); // fails forever
    let store: Arc<dyn GraphStore> = flaky.clone();
    let (mem, retained) = retained_batch_session("push-front", store).await;

    // Fresh writes land in the log *after* the retained batch was drained.
    mem.record_action(&Action {
        event_time: None,
        action: "wrote docs/api.md",
        produces: &["docs/api.md"],
        modifies: &[],
        depends_on: &[],
    })
    .unwrap();
    let fresh = mem.graph().read().log_len();
    assert!(fresh > 0);

    let err = mem.close().await.unwrap_err();
    assert!(err.to_string().contains("simulated outage"), "{err}");

    let batches = flaky.batch_lens();
    assert_eq!(
        *batches.last().unwrap(),
        retained + fresh,
        "close() must flush the retained batch AND the later writes as one \
             chronological batch (push-front), not just the later writes: {batches:?}"
    );

    // ...and in that ORDER (T81-3). Length alone let a push-BACK mutant
    // (`splice(0..0, ..)` -> `extend(..)`) survive the whole suite, while
    // it puts an edge upsert ahead of its endpoint's UpsertNode and so
    // fails a conforming adapter's in-order replay.
    let flushed = flaky.batches();
    let first_attempt = flushed.first().unwrap(); // what the task retained
    let final_batch = flushed.last().unwrap();
    assert_eq!(
        first_attempt.len(),
        retained,
        "the failed attempt is exactly the retained batch"
    );
    assert_eq!(
        &final_batch.mutations[..retained],
        &first_attempt.mutations[..],
        "the retained mutations must lead the final batch, in their original \
             order, with the later writes behind them"
    );

    // The premise that order serves (`src/graph/mod.rs`: replay in order,
    // never re-sort): no edge upsert before the nodes it points at.
    let mut seen: HashSet<NodeId> = HashSet::new();
    for m in &final_batch.mutations {
        match m {
            Mutation::UpsertNode { node } => {
                seen.insert(node.id());
            }
            Mutation::UpsertEdge { edge } => {
                assert!(
                    seen.contains(&edge.source) && seen.contains(&edge.target),
                    "edge upsert precedes an endpoint — the batch is not replayable \
                         in order: {m:?}"
                );
            }
            _ => {}
        }
    }
}

// -- close(): armor, retry, concurrency (T81-2 / T81-5 / T81-6) ---------

/// T81-2, timeout arm. A hung backend used to hang `close()` forever —
/// the background path bounds every attempt with `FLUSH_ATTEMPT_TIMEOUT`
/// and step 4 now does too. T81-5: the batch survives the timeout.
#[tokio::test(start_paused = true)]
async fn close_bounds_a_hanging_store_and_keeps_the_tail() {
    let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let store: Arc<dyn GraphStore> = Arc::new(AdverseStore::new(inner, FlushBehaviour::Hang));
    let mem = memory_on(store, "hang").await;

    mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
        .await
        .unwrap();
    let depth = mem.stats().log_depth;
    assert!(depth > 0);

    let err = mem.close().await.unwrap_err();
    assert!(err.to_string().contains("timed out"), "{err}");
    assert!(
        mem.graph().read().log_len() >= depth,
        "a timed-out final flush must return the tail to the log, not drop it"
    );
}

/// T81-2, panic arm. A panicking adapter used to unwind out of `close()`
/// **after** the log was drained — the tail was unrecoverable even for a
/// caller that caught the panic.
#[tokio::test]
async fn close_contains_a_panicking_store_and_keeps_the_tail() {
    let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let store: Arc<dyn GraphStore> = Arc::new(AdverseStore::new(inner, FlushBehaviour::Panic));
    let mem = memory_on(store, "panic").await;

    mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
        .await
        .unwrap();
    let depth = mem.stats().log_depth;

    let err = mem.close().await.unwrap_err();
    assert!(err.to_string().contains("panicked"), "{err}");
    assert!(
        mem.graph().read().log_len() >= depth,
        "a panicking final flush must return the tail to the log, not drop it"
    );
}

/// T81-5: a failed `close()` is retryable and says so. The first close
/// fails (store outage), the tail goes back on the log rather than
/// vanishing with the error, and a second close — after the store
/// recovered — makes exactly that tail durable. `Ok(())` from `close()`
/// always means "the tail is written".
#[tokio::test]
async fn a_failed_close_keeps_the_tail_and_a_later_close_flushes_it() {
    let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    // Exactly one failing flush — and with an hour-long interval, close's
    // is the only flush there is.
    let store: Arc<dyn GraphStore> = Arc::new(FlakyStore::new(inner.clone(), 1));
    let mem = memory_on(store, "close-retry").await;

    mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
        .await
        .unwrap();
    let depth = mem.stats().log_depth;

    let err = mem.close().await.unwrap_err();
    assert!(err.to_string().contains("simulated outage"), "{err}");
    assert!(
        mem.graph().read().log_len() >= depth,
        "a failed close must not drop the drained tail"
    );
    assert!(
        inner
            .load_session(&SessionId::new("close-retry"))
            .await
            .is_err(),
        "nothing durable yet"
    );

    // The store recovered: the retry flushes the tail it kept.
    mem.close().await.unwrap();
    let snap = inner
        .load_session(&SessionId::new("close-retry"))
        .await
        .unwrap();
    assert!(snap.concepts.iter().any(|c| c.content == "user schema"));
    assert_eq!(mem.graph().read().log_len(), 0);

    // Durable now, so it is idempotent again.
    mem.close().await.unwrap();
}

/// R2-1: a **cancelled** `close()` must not destroy the tail.
///
/// `close()` is an ordinary future, and its own rustdoc invites an external
/// bound ("a caller-supplied store can make step 0 arbitrarily long") —
/// `close_completes_when_stop_lands_during_a_long_flush` above wraps it in
/// exactly such a `timeout`. Dropped between the drain and the flush, the
/// batch used to die with the local: the log was empty, the flush task was
/// already joined, and the **second** `close()` drained nothing, latched
/// success and returned `Ok(())` — the documented "Ok means the tail is
/// durable" invariant, violated silently. (The reviewer's probe then found
/// `load_session` returning `SessionNotFound`.)
///
/// With `TailCustody` the drop hands the batch back to the front of the
/// log, so the retry has something to flush — and the empty-log shortcut,
/// which is what turned the loss into an `Ok`, is never reached with a
/// taken-and-lost tail behind it.
#[tokio::test(start_paused = true)]
async fn a_cancelled_close_returns_the_tail_to_the_log_for_the_retry() {
    let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let adverse = Arc::new(AdverseStore::new(inner.clone(), FlushBehaviour::HangOnce));
    let store: Arc<dyn GraphStore> = adverse.clone();
    let mem = memory_on(store, "cancelled-close").await;

    mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
        .await
        .unwrap();
    let depth = mem.stats().log_depth;
    assert!(depth > 0);

    // The caller gives up while the store is hung — well inside step 4's
    // own FLUSH_ATTEMPT_TIMEOUT, so this is a dropped future, not an
    // internally-bounded failure.
    let outcome = tokio::time::timeout(Duration::from_secs(5), mem.close()).await;
    assert!(
        outcome.is_err(),
        "the hung store must outlast the caller's patience"
    );
    assert_eq!(adverse.flush_calls(), 1, "the final flush was in flight");

    assert!(
        mem.graph().read().log_len() >= depth,
        "a cancelled close() must return the drained tail to the log, not drop it with \
             the future"
    );
    assert!(
        inner
            .load_session(&SessionId::new("cancelled-close"))
            .await
            .is_err(),
        "nothing durable yet"
    );

    // The store recovered. The retry must find and flush that tail — the
    // bug returned `Ok(())` here having written nothing at all.
    mem.close().await.unwrap();
    let snap = inner
        .load_session(&SessionId::new("cancelled-close"))
        .await
        .unwrap();
    assert!(
        snap.concepts.iter().any(|c| c.content == "user schema"),
        "Ok(()) from close() must mean the tail is durable"
    );
    assert_eq!(mem.graph().read().log_len(), 0);
}

/// R3-1: a `close()` cancelled at the **step-2 join** must not detach the
/// flush task.
///
/// R2-1 covered the drain-to-flush window; this is the window before it,
/// and the likelier one — the join is the long await (`close`'s own "worst
/// case ≈ 2 minutes" is mostly this), so an external `timeout` fires here.
/// The handle was lifted out of its slot *before* that await, so dropping
/// the future dropped the local: the flush task was detached, not stopped —
/// still running, still holding the whole tail in its `pending` (it had
/// already drained the log into it), still writing through its own `Arc`s.
/// The retry then found `flush_handle == None`, skipped the join, drained an
/// empty log, took the empty-log shortcut, latched success, released the
/// [`ACTIVE_SESSIONS`] slot and returned `Ok(())` over a tail that was not
/// durable. `Drop`'s R2-2 warning was blind to it for the same reason the
/// shortcut was: the log really was empty.
///
/// With `HandleCustody` the cancelled join hands the handle back, so the
/// retry re-joins that same task — a cancelled poll leaves a `JoinHandle`
/// re-awaitable — and the tail is written before any `Ok`.
#[tokio::test(start_paused = true)]
async fn a_close_cancelled_at_the_flush_join_keeps_the_handle_and_reaps_the_task() {
    let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let adverse = Arc::new(AdverseStore::new(inner.clone(), FlushBehaviour::HangOnce));
    let store: Arc<dyn GraphStore> = adverse.clone();
    let mem = Memory::builder()
        .session("cancelled-join")
        .agent("agent-a")
        // Short enough that the background loop is mid-attempt when the
        // caller closes: the cancellation must land on step 2, not step 4.
        .flush_interval(Duration::from_secs(1))
        .store(store)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .build()
        .await
        .unwrap();

    mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
        .await
        .unwrap();

    // Walk the clock to the first tick: the task's attempt is now hung
    // inside the store, with the tail in its `pending` buffer.
    for _ in 0..40 {
        tokio::time::advance(Duration::from_millis(200)).await;
        tokio::task::yield_now().await;
        if adverse.flush_calls() > 0 {
            break;
        }
    }
    assert_eq!(adverse.flush_calls(), 1, "a flush must be in flight");
    assert!(!adverse.flush_completed(), "and still in flight");
    assert_eq!(
        mem.graph().read().log_len(),
        0,
        "the tail is in the task's pending buffer now, not in the log — which is what \
             makes a detached task invisible to everything downstream"
    );

    // The caller gives up well inside the task's own FLUSH_ATTEMPT_TIMEOUT,
    // so `close()` is dropped parked on the join.
    let outcome = tokio::time::timeout(Duration::from_secs(5), mem.close()).await;
    assert!(
        outcome.is_err(),
        "the hung attempt must outlast the caller's patience"
    );
    assert_eq!(adverse.flush_calls(), 1, "step 4 was never reached");
    assert!(
        mem.flush_handle.lock().is_some(),
        "a cancelled step-2 join must return the JoinHandle to its slot: detached, the task \
             runs on and the retry skips the join entirely (R3-1)"
    );
    assert!(
        inner
            .load_session(&SessionId::new("cancelled-join"))
            .await
            .is_err(),
        "nothing durable yet"
    );

    // The store is healthy for the task's next attempt. The retry re-joins
    // that same task, so it waits out the attempt (and gets the tail with
    // it) instead of blessing an empty log.
    tokio::time::timeout(Duration::from_secs(300), mem.close())
        .await
        .expect("the retry must re-join the flush task, not hang")
        .expect("close");

    let snap = inner
        .load_session(&SessionId::new("cancelled-join"))
        .await
        .expect("Ok(()) from close() must mean the tail is durable");
    assert!(snap.concepts.iter().any(|c| c.content == "user schema"));
    assert_eq!(mem.graph().read().log_len(), 0);

    // ...and no zombie behind that `Ok`. Detached, the task's hung attempt
    // times out at FLUSH_ATTEMPT_TIMEOUT and it goes right on flushing —
    // after `close()` returned, which is how the reviewer's probe caught it.
    assert!(
        mem.flush_handle.lock().is_none(),
        "a reaped task leaves its slot empty"
    );
    let after_ok = adverse.flush_calls();
    tokio::time::advance(FLUSH_ATTEMPT_TIMEOUT * 4).await;
    tokio::task::yield_now().await;
    assert_eq!(
        adverse.flush_calls(),
        after_ok,
        "a store call after close() returned Ok means the flush task was never stopped"
    );
}

/// T81-6: a second **concurrent** `close()` must not report `Ok` over an
/// in-flight final flush (a caller gating process exit on that `Ok` would
/// let runtime teardown cancel the flush and lose the tail). Both callers
/// must observe the completed flush.
#[tokio::test(start_paused = true)]
async fn concurrent_closes_both_wait_for_the_one_final_flush() {
    let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let adverse = Arc::new(AdverseStore::new(
        inner.clone(),
        FlushBehaviour::Delay(Duration::from_secs(2)),
    ));
    let store: Arc<dyn GraphStore> = adverse.clone();
    let mem = Arc::new(memory_on(store, "concurrent-close").await);

    mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
        .await
        .unwrap();

    let closer = |mem: Arc<Memory>, adverse: Arc<AdverseStore>| {
        tokio::spawn(async move {
            let result = mem.close().await;
            (result, adverse.flush_completed())
        })
    };
    let first = closer(mem.clone(), adverse.clone());
    let second = closer(mem.clone(), adverse.clone());

    let (first_result, first_saw_flush) = first.await.unwrap();
    let (second_result, second_saw_flush) = second.await.unwrap();
    first_result.expect("first close");
    second_result.expect("second close");
    assert!(
        first_saw_flush && second_saw_flush,
        "no close() may return before the final flush completed"
    );
    assert_eq!(
        adverse.flush_calls(),
        1,
        "the tail is flushed once, not once per caller"
    );

    let snap = inner
        .load_session(&SessionId::new("concurrent-close"))
        .await
        .unwrap();
    assert!(snap.concepts.iter().any(|c| c.content == "user schema"));
}

/// Partial coverage for COH-6 clause 2 — `biased;` + stop-first in the
/// flush loop's `select!` (T81-4).
///
/// The regression that clause guards is a **lost stop permit**: an unbiased
/// `select!` polls in a random start order, so a concurrently ready
/// `interval.tick()` can be polled first, consume-and-drop the stored
/// permit, and `close()`'s join then hangs forever. The tick is
/// concurrently ready exactly when a flush outlasts the interval — which is
/// what this builds: a 5s flush against a 1s interval, `stop()` arriving
/// while that flush is in flight.
///
/// It cannot *prove* the ordering (a lost permit under an unbiased select
/// is probabilistic — pinning it needs loom); what it does pin is that this
/// shutdown shape terminates. A hang fails the test instead of wedging it,
/// because the join is wrapped in a timeout on the (paused) clock.
#[tokio::test(start_paused = true)]
async fn close_completes_when_stop_lands_during_a_long_flush() {
    let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let adverse = Arc::new(AdverseStore::new(
        inner.clone(),
        FlushBehaviour::Delay(Duration::from_secs(5)),
    ));
    let store: Arc<dyn GraphStore> = adverse.clone();
    let mem = Memory::builder()
        .session("slow-flush")
        .agent("agent-a")
        .flush_interval(Duration::from_secs(1))
        .store(store)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .build()
        .await
        .unwrap();

    mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
        .await
        .unwrap();

    // Walk the clock to the first tick and stop as soon as the flush has
    // started — not far enough for it to finish.
    for _ in 0..40 {
        tokio::time::advance(Duration::from_millis(200)).await;
        tokio::task::yield_now().await;
        if adverse.flush_calls() > 0 {
            break;
        }
    }
    assert_eq!(adverse.flush_calls(), 1, "a flush must be in flight");
    assert!(!adverse.flush_completed(), "and still in flight");

    // The join must finish. A lost stop permit would spin the loop until
    // this (virtual) deadline instead.
    tokio::time::timeout(Duration::from_secs(300), mem.close())
        .await
        .expect("close() must not hang when stop lands during an in-flight flush")
        .expect("close");
    assert!(
        adverse.flush_completed(),
        "the in-flight flush ran to completion before the loop exited"
    );

    let snap = inner
        .load_session(&SessionId::new("slow-flush"))
        .await
        .unwrap();
    assert!(snap.concepts.iter().any(|c| c.content == "user schema"));
}

/// A session that has degraded to `durability="none"` on backlog (STORE-3):
/// one mutation of headroom, so the first cycle's drain is already past
/// `backend_log_max`. Returns it degraded, with its log already emptied by
/// that cycle's drain.
async fn degraded_session(session: &str) -> Memory {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let mem = Memory::builder()
        .config(Config {
            backend_flush_retries: 0,
            backend_log_max: 1,
            ..Config::default()
        })
        .session(session)
        .agent("agent-a")
        .flush_interval(Duration::from_millis(100))
        .store(store)
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .build()
        .await
        .unwrap();

    mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
        .await
        .unwrap();
    for _ in 0..20 {
        tokio::time::advance(Duration::from_millis(150)).await;
        tokio::task::yield_now().await;
        if mem.stats().degraded {
            break;
        }
    }
    assert!(mem.stats().degraded, "the session must have degraded");
    mem
}

/// The degraded-close branch (implementer self-flag #6, ruled in scope).
/// A session past `backend_log_max` stopped all store I/O by design;
/// `close()` must say the tail was not written instead of reporting a
/// durability it did not deliver — and must keep saying it.
#[tokio::test(start_paused = true)]
async fn close_refuses_to_claim_durability_for_a_degraded_session() {
    let mem = degraded_session("degraded").await;

    // A write after degradation is still in the log when close drains it.
    mem.record_action(&Action {
        event_time: None,
        action: "wrote docs/api.md",
        produces: &["docs/api.md"],
        modifies: &[],
        depends_on: &[],
    })
    .unwrap();
    assert!(mem.stats().log_depth > 0);

    let err = mem.close().await.unwrap_err();
    assert!(err.to_string().contains("degraded"), "{err}");
    assert!(
        mem.graph().read().log_len() > 0,
        "the un-written tail stays visible in log_depth"
    );
    // No `Ok` over an undurable tail, ever.
    let again = mem.close().await.unwrap_err();
    assert!(again.to_string().contains("degraded"), "{again}");
}

/// R2-3: the same must hold with an **empty** log — the case the ordering
/// bug made `Ok(())`.
///
/// Degraded mode keeps draining the log and DROPPING what it drained
/// (STORE-3, spec §2.3 "none = pure RAM"), so an empty log is that mode's
/// *steady state*: it means the tail was dead-lettered, not written. With
/// the empty-log shortcut ahead of the degraded check, a caller who closed
/// a moment after the drain got `Ok(())` — a durability claim over
/// mutations the session had already thrown away, and one that contradicted
/// the very next `close()` if a single write had landed in between.
#[tokio::test(start_paused = true)]
async fn close_refuses_a_degraded_session_even_when_its_log_is_empty() {
    let mem = degraded_session("degraded-empty").await;

    // Write, then let a degraded cycle drain-and-drop it: log empty, tail
    // gone. Exactly the state the shortcut used to bless.
    mem.record_action(&Action {
        event_time: None,
        action: "wrote docs/api.md",
        produces: &["docs/api.md"],
        modifies: &[],
        depends_on: &[],
    })
    .unwrap();
    assert!(mem.stats().log_depth > 0);
    for _ in 0..20 {
        tokio::time::advance(Duration::from_millis(150)).await;
        tokio::task::yield_now().await;
        if mem.stats().log_depth == 0 {
            break;
        }
    }
    assert_eq!(
        mem.stats().log_depth,
        0,
        "degraded draining drops what it drained (STORE-3)"
    );
    assert_eq!(mem.stats().flush_depth, 0, "and retains nothing");

    let err = mem.close().await.unwrap_err();
    assert!(err.to_string().contains("degraded"), "{err}");
    assert!(
        err.to_string().contains("not because the tail was written"),
        "the error must say why an empty log is not durability: {err}"
    );
    // Still no `Ok`, however often it is asked.
    let again = mem.close().await.unwrap_err();
    assert!(again.to_string().contains("degraded"), "{again}");
}

// -- the writers gate (T81-1) -------------------------------------------

/// The P1 race, as the reviewer demonstrated it, now as a regression test.
///
/// A store double parks `retract`'s `blast_radius` call, so the retraction
/// is suspended *mid-write* — past `ensure_open`, before its mutation.
/// `close()` then starts. Without the writers gate `close()` completed
/// `Ok`, the retract resumed, reported `removed: true`, and its `DeleteNode`
/// sat in the log forever: an acknowledged retraction that resurrects on
/// reattach. With the gate there are only two legal outcomes and both are
/// asserted — `close()` waits and the removal is durable, or the retract is
/// refused with the closed error and nothing was acknowledged.
#[tokio::test]
async fn a_write_in_flight_when_close_starts_is_never_acknowledged_then_lost() {
    let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let parking = Arc::new(ParkingStore::new(inner.clone(), ParkPoint::BlastRadius));
    let store: Arc<dyn GraphStore> = parking.clone();
    let mem = Arc::new(memory_on(store, "write-vs-close").await);

    mem.derive(
        &[
            ("victim", ConceptType::Entity),
            ("survivor", ConceptType::Entity),
        ],
        &ParentOf::none(),
    )
    .await
    .unwrap();

    let entered = parking.entered();
    let retracting = tokio::spawn({
        let mem = mem.clone();
        async move { mem.retract("victim", DryRun::No).await }
    });
    entered.notified().await; // the retract is parked, holding a permit

    let mut closing = tokio::spawn({
        let mem = mem.clone();
        async move { mem.close().await }
    });
    // A real chance to run to completion — the whole shutdown is
    // sub-millisecond here — so this fails loudly if `close()` ever stops
    // waiting for in-flight writers (it is what the P1 bug did).
    assert!(
        tokio::time::timeout(Duration::from_millis(500), &mut closing)
            .await
            .is_err(),
        "close() must not drain while a write is in flight"
    );

    parking.release();
    let report = retracting.await.unwrap();
    closing.await.unwrap().expect("close");

    assert_eq!(
        mem.graph().read().log_len(),
        0,
        "no mutation may be left in the log after close()"
    );
    let snap = inner
        .load_session(&SessionId::new("write-vs-close"))
        .await
        .unwrap();
    assert!(snap.concepts.iter().any(|c| c.content == "survivor"));
    match report {
        Ok(report) => {
            assert!(report.removed);
            assert!(
                !snap.concepts.iter().any(|c| c.content == "victim"),
                "an acknowledged retraction must not resurrect on reattach"
            );
        }
        Err(err) => {
            assert!(err.to_string().contains("closed"), "{err}");
            assert!(
                snap.concepts.iter().any(|c| c.content == "victim"),
                "a refused retraction must not have removed anything"
            );
        }
    }
}

/// R2-6: the **post-acquire** `closed` re-check in `begin_write` — the
/// second half of the gate, and the half a mutant could delete and still
/// pass 539/539.
///
/// The window it covers is real but two instructions wide: a writer loads
/// `closed` as open, `close()` latches it and asks for the write side, and
/// only then does the writer ask for its read permit — which now queues.
/// Without the re-check that write wakes up *after* `close()` has drained,
/// flushed and returned, and appends to a log nobody will ever flush again:
/// T81-1 exactly, by the one route the gate itself opens.
///
/// Held open deterministically here by taking the gate's write side in the
/// test, which is precisely the state `close()` is in between its latch and
/// its own acquisition. The tokio `RwLock` is FIFO-fair, so the queued
/// derive is granted its permit the moment the test's guard drops — with
/// `closed` already latched. Either order of the two waiters gives the same
/// verdict: a write that takes the gate after the latch is refused, never
/// silently appended.
#[tokio::test]
async fn a_write_that_takes_the_gate_after_close_latched_is_refused() {
    let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let mem = Arc::new(memory_on(inner.clone(), "late-permit").await);
    mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
        .await
        .unwrap();
    let baseline = mem.graph().read().log_len();

    // The gate is busy but the session is still OPEN: a writer arriving now
    // passes the entry check and queues for its permit.
    let gate = mem.writers.write().await;

    let deriving = tokio::spawn({
        let mem = mem.clone();
        async move {
            mem.derive(&[("late concept", ConceptType::Entity)], &ParentOf::none())
                .await
        }
    });
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        mem.graph().read().log_len(),
        baseline,
        "the derive must be queued for the permit, not past it"
    );

    let closing = tokio::spawn({
        let mem = mem.clone();
        async move { mem.close().await }
    });
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert!(
        mem.closed.load(Ordering::Acquire),
        "close() must have latched before the permit is handed over — that is \
             the window under test"
    );

    drop(gate);
    let derived = deriving.await.unwrap();
    closing.await.unwrap().expect("close");

    let err = derived.expect_err(
        "a write granted the gate after close() latched must be refused by the \
             post-acquire re-check, not mutate",
    );
    assert!(err.to_string().contains("closed"), "{err}");
    assert_eq!(
        mem.graph().read().log_len(),
        0,
        "and it must not have logged a mutation"
    );
    let snap = inner
        .load_session(&SessionId::new("late-permit"))
        .await
        .unwrap();
    assert!(snap.concepts.iter().any(|c| c.content == "user schema"));
    assert!(
        !snap.concepts.iter().any(|c| c.content == "late concept"),
        "a refused write must reach neither the log nor the store"
    );
}

/// The sync arm of the same barrier (`begin_write_sync`): `try_read` fails
/// while `close()` holds — or is queued for — the write side, and that maps
/// to the closed error rather than to a mutation that would race the drain.
///
/// Its own post-acquire re-check covers a window one instruction wide
/// (`try_read` succeeding between the latch and `close()`'s request) which
/// no single-threaded interleaving can construct; see `begin_write_sync`'s
/// rustdoc. This pins the arm that is reachable.
#[tokio::test]
async fn a_sync_write_is_refused_while_the_gate_is_taken() {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let mem = memory_on(store, "sync-gate").await;
    mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
        .await
        .unwrap();
    let baseline = mem.graph().read().log_len();

    let gate = mem.writers.write().await;
    let err = mem
        .record_action(&Action {
            event_time: None,
            action: "late",
            produces: &[],
            modifies: &[],
            depends_on: &[],
        })
        .unwrap_err();
    assert!(err.to_string().contains("closed"), "{err}");
    assert_eq!(
        mem.graph().read().log_len(),
        baseline,
        "a refused sync write must not have logged a mutation"
    );

    drop(gate);
    mem.close().await.unwrap();
}

/// The other half of the gate: **readers do not take it**, so a long recall
/// cannot hold shutdown hostage. The store parks recall's vector leg; the
/// close must still finish (a gated reader would deadlock it). Reads after
/// close stay refused by `ensure_open`, exactly as before.
#[tokio::test]
async fn close_does_not_wait_for_an_in_flight_read() {
    let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let parking = Arc::new(ParkingStore::new(inner, ParkPoint::VectorCandidates));
    let store: Arc<dyn GraphStore> = parking.clone();
    // Canonical matching, so the one parked call is recall's — hybrid
    // `derive` would otherwise consume the park on its own vector leg.
    let mem = Arc::new(
        Memory::builder()
            .session("read-vs-close")
            .agent("agent-a")
            .flush_interval(Duration::from_secs(3_600))
            .match_strategy(MatchStrategy::Canonical)
            .store(store)
            .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
            .embedding_contract(contract("fixture", 1024))
            .build()
            .await
            .unwrap(),
    );

    mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
        .await
        .unwrap();

    let entered = parking.entered();
    let recalling = tokio::spawn({
        let mem = mem.clone();
        async move {
            mem.recall(RecallQuery {
                query: "user schema".into(),
                top_k: 5,
                max_tokens: 500,
                traversal_depth: 2,
            })
            .await
        }
    });
    entered.notified().await; // the recall is parked inside the store

    tokio::time::timeout(Duration::from_secs(10), mem.close())
        .await
        .expect("close() must not wait for readers")
        .expect("close");

    parking.release();
    recalling
        .await
        .unwrap()
        .expect("the parked recall still returns");

    // Reads after close are refused by `ensure_open`, as before.
    let err = mem
        .recall(RecallQuery {
            query: "user schema".into(),
            top_k: 5,
            max_tokens: 500,
            traversal_depth: 2,
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("closed"), "{err}");
}

/// Every mutating method — async and sync — refuses after close. The sync
/// ones go through the gate's non-blocking arm, so this also pins that a
/// closed session's `try_read` path returns the closed error rather than
/// mutating.
#[tokio::test]
async fn every_mutating_method_is_refused_after_close() {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let mem = memory_on(store, "refuse-all").await;
    let out = mem
        .derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
        .await
        .unwrap();
    let node = out.created[0];
    mem.close().await.unwrap();

    let closed = |err: LamboError| assert!(err.to_string().contains("closed"), "{err}");
    closed(mem.set_root_goal(&["late"]).unwrap_err());
    closed(mem.declare_synonym("a", "b").unwrap_err());
    closed(
        mem.derive(&[("late", ConceptType::Entity)], &ParentOf::none())
            .await
            .unwrap_err(),
    );
    closed(
        mem.record_action(&Action {
            event_time: None,
            action: "late",
            produces: &[],
            modifies: &[],
            depends_on: &[],
        })
        .unwrap_err(),
    );
    closed(mem.demote("late chunk.", "chunk-late").unwrap_err());
    closed(mem.retract("user schema", DryRun::No).await.unwrap_err());
    closed(mem.reserve(node, Duration::from_secs(30)).unwrap_err());
    closed(mem.release(node).unwrap_err());

    assert_eq!(
        mem.graph().read().log_len(),
        0,
        "a refused write must not have logged a mutation"
    );
}

#[tokio::test]
async fn close_is_idempotent_and_the_session_refuses_later_writes() {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let mem = memory_on(store, "closed").await;
    mem.close().await.unwrap();
    mem.close().await.unwrap();

    let err = mem
        .derive(&[("late", ConceptType::Entity)], &ParentOf::none())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("closed"), "{err}");
}
