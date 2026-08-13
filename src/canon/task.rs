//! The canonization evaluation loop (T6.4, spec §10) — the owner of
//! `canonization_eval_interval`.
//!
//! Spec §10: "**Evaluation** — every `canonization_eval_interval=60s`, at
//! most `canonization_eval_batch_size=50` Venerable nodes per cycle". This
//! task is the "every 60s". Without it the stage predicates and
//! [`Evaluator::eval_cycle`] were a library nothing scheduled: the knob was
//! dead config, no node ever transitioned in an assembled process, and the
//! demo's `canonization_events` table stayed empty.
//!
//! ## Why a task of its own
//!
//! The daemon cycle (`src/daemon/mod.rs`) is deliberately **synchronous** —
//! its whole body runs under short lock scopes inside one `catch_unwind`, and
//! it performs no I/O. A canonization cycle must await the store (spec §10's
//! own predicates are `interaction_span` / `blast_radius` queries), so it
//! cannot live inside that body. It gets the [`FlushTask`]-shaped treatment
//! instead: its own interval loop, its own `spawn`-once handle, sharing the
//! daemon's graph, score table and event channel by `Arc`.
//!
//! [`FlushTask`]: crate::store::flush::FlushTask
//!
//! ## Lock discipline (spec §6.4)
//!
//! The task never takes a lock itself. [`Evaluator::eval_cycle`] takes the
//! graph lock in short synchronous scopes (gather / apply) and holds none
//! across its store I/O; because `parking_lot` guards are `!Send`, the
//! compiler proves that here — this task `tokio::spawn`s the cycle future.
//!
//! ## Panic containment
//!
//! Same argument as the daemon loop (CONC-4) and the flush loop: a panic in
//! one cycle is logged and the loop continues. Without it a single panicking
//! store call would stop canonization for the process's lifetime, silently.
//!
//! ## Shutdown
//!
//! The handle stops the loop by `abort()`, which cancels the cycle future at
//! whatever await it is parked on. That is safe here for two reasons: no
//! `parking_lot` guard is ever live across an await (the cycle takes the graph
//! lock only in synchronous scopes — the compiler enforces it, since a `!Send`
//! guard held across an await would make this future un-`spawn`able), and a
//! hop whose phase-4 `record_canonization` is cancelled is already committed
//! to the graph, its audit, and the write-behind log, so the next flush
//! carries it to the store (deduped on event id). Cancellation can lose the
//! immediacy of the durable write, never the transition.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use tokio::sync::Notify;

use crate::canon::{EvalParams, Evaluator};
use crate::config::Config;
use crate::daemon::events::EventSender;
use crate::daemon::{Clock, Daemon, ScoreTable};
use crate::graph::Graph;
use crate::store::flush::CatchUnwindPoll;
use crate::store::GraphStore;

/// State shared between the running loop and the caller's handle.
#[derive(Debug, Default)]
struct Shared {
    /// Check-and-set by `spawn`: exactly one loop may run per task.
    started: AtomicBool,
    /// Cycles that ran to completion (a panicking cycle does not count).
    cycles: AtomicU64,
    /// Cycles that ended in [`crate::canon::EvalError`].
    failures: AtomicU64,
}

/// The `canonization_eval_interval` loop. Spawn with
/// [`CanonizationTask::spawn`]; the returned handle is the only way to stop
/// it (abort).
pub struct CanonizationTask {
    graph: Arc<RwLock<Graph>>,
    store: Arc<dyn GraphStore>,
    /// The daemon's score table (Stage 1's P90 population). Read per cycle,
    /// never written here.
    scores: Arc<RwLock<ScoreTable>>,
    events: EventSender,
    params: EvalParams,
    interval: Duration,
    clock: Clock,
    wake: Arc<Notify>,
    shared: Arc<Shared>,
}

impl CanonizationTask {
    /// Build the loop from its parts. `interval` is
    /// `Config::canonization_eval_interval`; `params` is
    /// [`EvalParams::from_config`].
    pub fn new(
        graph: Arc<RwLock<Graph>>,
        store: Arc<dyn GraphStore>,
        scores: Arc<RwLock<ScoreTable>>,
        events: EventSender,
        params: EvalParams,
        interval: Duration,
    ) -> Self {
        Self {
            graph,
            store,
            scores,
            events,
            params,
            interval,
            clock: Arc::new(chrono::Utc::now),
            wake: Arc::new(Notify::new()),
            shared: Arc::new(Shared::default()),
        }
    }

    /// The assembled-process entry point: take the graph, the store and the
    /// running [`Daemon`]'s score table + event channel, and read every knob
    /// from `config`.
    ///
    /// Sharing the daemon's `EventSender` (not a raw `broadcast::Sender`) is
    /// required, not incidental: the channel's publication counter is what
    /// the daemon's ring-eviction re-arm path (CONC-2/NEW-3) measures, so a
    /// `Canonized` burst published outside that counter would evict a held
    /// `Conflict` invisibly.
    pub fn from_daemon(
        graph: Arc<RwLock<Graph>>,
        store: Arc<dyn GraphStore>,
        daemon: &Daemon,
        config: &Config,
    ) -> Self {
        Self::new(
            graph,
            store,
            daemon.score_table(),
            daemon.event_sender(),
            EvalParams::from_config(config),
            config.canonization_eval_interval,
        )
    }

    /// Use `clock` as each cycle's `now` instead of the wall clock — the
    /// whole cycle, store queries included (see
    /// [`crate::store::GraphStore::blast_radius`]), reads this one instant.
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// Spawn the interval loop and return its handle (abort = stop).
    ///
    /// Call `spawn` **exactly once** per task — a second call panics, like
    /// `Daemon::spawn` and `FlushTask::spawn`. Two loops would each carry
    /// their own round-robin cursors and interleave promotions unpredictably.
    /// Takes `&self` so the caller keeps this handle for
    /// [`CanonizationTask::cycles`] / [`CanonizationTask::wake`].
    ///
    /// The first cycle runs one `interval` after spawn (the fresh interval's
    /// immediate tick is consumed), matching the flush task: a cycle at t=0
    /// would evaluate a session the daemon has not scored yet.
    pub fn spawn(&self) -> tokio::task::JoinHandle<()> {
        self.shared
            .started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .expect("CanonizationTask::spawn called twice — exactly one loop may run");
        let loop_state = CanonizationLoop {
            graph: self.graph.clone(),
            store: self.store.clone(),
            scores: self.scores.clone(),
            events: self.events.clone(),
            params: self.params.clone(),
            interval: self.interval,
            clock: self.clock.clone(),
            wake: self.wake.clone(),
            shared: self.shared.clone(),
            evaluator: Evaluator::new(),
        };
        tokio::spawn(async move { loop_state.run().await })
    }

    /// Wake the loop for an immediate cycle (tests; the P8 seam for "evaluate
    /// now" after a bulk import).
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    /// Cycles that ran to completion since [`CanonizationTask::spawn`].
    pub fn cycles(&self) -> u64 {
        self.shared.cycles.load(Ordering::Acquire)
    }

    /// Cycles that ended in an error (the outcome they had committed is in
    /// the log, not here).
    pub fn failures(&self) -> u64 {
        self.shared.failures.load(Ordering::Acquire)
    }
}

/// Task-owned loop state (moved into the spawned task).
struct CanonizationLoop {
    graph: Arc<RwLock<Graph>>,
    store: Arc<dyn GraphStore>,
    scores: Arc<RwLock<ScoreTable>>,
    events: EventSender,
    params: EvalParams,
    interval: Duration,
    clock: Clock,
    wake: Arc<Notify>,
    shared: Arc<Shared>,
    /// Round-robin cursors live here, across cycles — that is the whole point
    /// of the ring (spec §10 "anti-starvation preserved").
    evaluator: Evaluator,
}

impl CanonizationLoop {
    async fn run(mut self) {
        let mut interval = tokio::time::interval(self.interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Consume the immediate first tick: the first cycle runs one interval
        // after spawn.
        interval.tick().await;
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = self.wake.notified() => {}
            }
            self.cycle().await;
        }
    }

    async fn cycle(&mut self) {
        let now = (self.clock)();
        let scores = self.scores.read().clone();
        let result = CatchUnwindPoll(self.evaluator.eval_cycle(
            &self.graph,
            self.store.as_ref(),
            &scores,
            &self.events,
            &self.params,
            now,
        ))
        .await;

        match result {
            Ok(Ok(outcome)) => {
                if !outcome.is_empty() {
                    tracing::info!(
                        target: "lambo::canon",
                        promotions = outcome.promotions.len(),
                        demotions = outcome.demotions.len(),
                        stage3_batch = outcome.stage3_batch.len(),
                        "canonization cycle committed transitions"
                    );
                }
                self.shared.cycles.fetch_add(1, Ordering::Release);
            }
            Ok(Err(err)) => {
                // The outcome rides the error (the commit-point contract): the
                // hops it names are already in the graph and its audit, and
                // the write-behind log carries them to the store.
                tracing::warn!(
                    target: "lambo::canon",
                    committed = err.outcome.transitions().count(),
                    "CanonizationCycleFailed: {err}"
                );
                self.shared.failures.fetch_add(1, Ordering::Release);
                self.shared.cycles.fetch_add(1, Ordering::Release);
            }
            Err(payload) => {
                tracing::error!(
                    target: "lambo::canon",
                    panic = %crate::store::flush::panic_message(&payload),
                    "CanonizationCyclePanic: canonization cycle panicked; the loop \
                     continues with the next tick"
                );
            }
        }
    }
}

#[cfg(all(test, feature = "store-memory"))]
mod tests {
    use super::*;
    use crate::config::ScoringWeights;
    use crate::daemon::events::event_channel;
    use crate::store::MemoryStore;
    use crate::types::{
        AgentId, CanonizationStatus, Concept, ConceptType, DaemonEvent, Interaction, Mutation,
        MutationBatch, Node, NodeId, Scored, SessionId,
    };
    use chrono::{DateTime, TimeZone, Utc};
    use uuid::Uuid;

    fn ts() -> DateTime<Utc> {
        Utc.timestamp_opt(1_752_000_000, 0).unwrap()
    }

    fn sid() -> SessionId {
        SessionId::from("test-session")
    }

    fn nid(id: u64) -> NodeId {
        NodeId(Uuid::from_u64_pair(2, id))
    }

    fn iid(id: u64) -> NodeId {
        NodeId(Uuid::from_u64_pair(1, id))
    }

    fn interaction() -> Interaction {
        Interaction {
            id: iid(1),
            session_id: sid(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: Some("prompt".into()),
            previous_id: None,
            created_at: ts(),
        }
    }

    fn concept(id: u64) -> Concept {
        Concept {
            id: nid(id),
            session_id: sid(),
            content: format!("c{id}"),
            canonical_key: format!("c{id}"),
            concept_type: ConceptType::Entity,
            origin_interaction: iid(1),
            origin_agent: AgentId::from("agent-a"),
            created_at: ts(),
            access_count: 0,
            last_accessed: None,
            gc_survived: 5,
            canonization_status: CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: None,
            embedding: None,
            chunk_group_id: None,
        }
    }

    /// 20 non-Canonical peers so Stage 1's session gate opens; id 20 is the
    /// only one above P90.
    fn session() -> (Arc<RwLock<Graph>>, MemoryStore, ScoreTable) {
        let mut g = Graph::new(sid());
        g.insert_interaction(interaction()).unwrap();
        for id in 1..=20u64 {
            g.insert_concept(concept(id), iid(1)).unwrap();
        }
        let snap = g.snapshot();
        let store = MemoryStore::new();
        let mut batch = MutationBatch::new();
        for i in snap.interactions {
            batch.push(Mutation::UpsertNode {
                node: Node::Interaction(i),
            });
        }
        for c in snap.concepts {
            batch.push(Mutation::UpsertNode {
                node: Node::Concept(c),
            });
        }
        futures_lite_block(store.flush(&batch)).unwrap();
        let mut ranked: Vec<Scored<NodeId>> = (1..=19).map(|i| Scored::new(nid(i), 0.1)).collect();
        ranked.push(Scored::new(nid(20), 1.0));
        let epoch = g.epoch();
        (
            Arc::new(RwLock::new(g)),
            store,
            ScoreTable { epoch, ranked },
        )
    }

    /// Run a future to completion on the current thread — the fixture builder
    /// is synchronous and MemoryStore's `flush` never yields.
    fn futures_lite_block<F: std::future::Future>(fut: F) -> F::Output {
        let mut fut = Box::pin(fut);
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        loop {
            if let std::task::Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                return v;
            }
        }
    }

    fn task(
        graph: Arc<RwLock<Graph>>,
        store: Arc<dyn GraphStore>,
        scores: ScoreTable,
        events: EventSender,
    ) -> CanonizationTask {
        CanonizationTask::new(
            graph,
            store,
            Arc::new(RwLock::new(scores)),
            events,
            EvalParams {
                min_age: Duration::ZERO,
                min_edge_age: Duration::ZERO,
                ..EvalParams::default()
            },
            Duration::from_secs(60),
        )
        .with_clock(Arc::new(ts))
    }

    /// Capturing writer for asserting on emitted tracing events (mirrors the
    /// flush suite's).
    #[derive(Clone)]
    struct BufWriter(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for BufWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for BufWriter {
        type Writer = BufWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// `MemoryStore` whose `record_canonization` can be told to fail or to
    /// panic — the loop's two non-`Ok(Ok(_))` arms (R2-5). Everything else
    /// delegates. Mirrors the flush suite's `PanicStore`.
    struct FaultyStore {
        inner: MemoryStore,
        fail_record: AtomicBool,
        panic_record: AtomicBool,
    }

    impl FaultyStore {
        fn wrapping(inner: MemoryStore) -> Self {
            Self {
                inner,
                fail_record: AtomicBool::new(false),
                panic_record: AtomicBool::new(false),
            }
        }

        fn fail_records(&self, on: bool) {
            self.fail_record.store(on, Ordering::SeqCst);
        }

        fn panic_records(&self, on: bool) {
            self.panic_record.store(on, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl GraphStore for FaultyStore {
        async fn init_schema(&self) -> Result<(), crate::types::StoreError> {
            self.inner.init_schema().await
        }
        fn capabilities(&self) -> crate::store::Capabilities {
            self.inner.capabilities()
        }
        async fn flush(&self, batch: &MutationBatch) -> Result<(), crate::types::StoreError> {
            self.inner.flush(batch).await
        }
        async fn load_session(
            &self,
            session: &SessionId,
        ) -> Result<crate::types::GraphSnapshot, crate::types::StoreError> {
            self.inner.load_session(session).await
        }
        async fn keyword_candidates(
            &self,
            session: &SessionId,
            tokens: &[String],
            limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, crate::types::StoreError> {
            self.inner.keyword_candidates(session, tokens, limit).await
        }
        async fn vector_candidates(
            &self,
            session: &SessionId,
            embedding: &[f32],
            limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, crate::types::StoreError> {
            self.inner
                .vector_candidates(session, embedding, limit)
                .await
        }
        async fn blast_radius(
            &self,
            session: &SessionId,
            node: NodeId,
            min_edge_age: Duration,
            now: DateTime<Utc>,
        ) -> Result<u64, crate::types::StoreError> {
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
        ) -> Result<crate::types::InteractionSpan, crate::types::StoreError> {
            self.inner
                .interaction_span(session, node, min_age, now)
                .await
        }
        async fn record_canonization(
            &self,
            event: &crate::types::CanonizationEvent,
        ) -> Result<(), crate::types::StoreError> {
            if self.panic_record.load(Ordering::SeqCst) {
                panic!("simulated canonization panic (FaultyStore)");
            }
            if self.fail_record.load(Ordering::SeqCst) {
                return Err(crate::types::StoreError::Backend(
                    "record_canonization is down".into(),
                ));
            }
            self.inner.record_canonization(event).await
        }
    }

    /// R2-5: the `Ok(Err(_))` arm. A cycle that returns `EvalError` must be
    /// logged and the loop must keep ticking — a later cycle still runs.
    /// Without containment one unhealthy store would end canonization for the
    /// process's lifetime, silently.
    #[tokio::test(start_paused = true)]
    async fn a_failing_cycle_is_logged_and_the_loop_keeps_ticking() {
        let buf = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sub = tracing_subscriber::fmt()
            .with_writer(BufWriter(buf.clone()))
            .with_max_level(tracing::Level::WARN)
            .finish();
        let _guard = tracing::subscriber::set_default(sub);

        let (graph, store, scores) = session();
        let store = Arc::new(FaultyStore::wrapping(store));
        store.fail_records(true);
        let (tx, _rx) = event_channel();
        let task = task(graph.clone(), store.clone(), scores, tx);
        let handle = task.spawn();

        tokio::time::sleep(Duration::from_secs(61)).await;
        assert_eq!(task.cycles(), 1, "the failing cycle still counts as run");
        assert_eq!(task.failures(), 1);
        assert!(!handle.is_finished(), "the loop must survive a cycle error");
        // The commit point held: the hop is in the graph even though its
        // durable audit write failed.
        assert_eq!(
            graph
                .read()
                .concepts()
                .find(|c| c.id == nid(20))
                .unwrap()
                .canonization_status,
            CanonizationStatus::Candidate,
        );

        // The store recovers; the next tick's cycle succeeds.
        store.fail_records(false);
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(task.cycles(), 2, "the loop kept ticking");
        assert_eq!(task.failures(), 1, "and the later cycle did not fail");
        assert!(!handle.is_finished());

        let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(
            out.contains("CanonizationCycleFailed"),
            "the failure must be logged: {out}"
        );
        assert!(
            out.contains("record_canonization is down"),
            "with the store error attached: {out}"
        );
        handle.abort();
    }

    /// R2-5: the `Err(payload)` arm. A panicking cycle is contained by
    /// `CatchUnwindPoll` — an uncontained one would abort the spawned task and
    /// finish the `JoinHandle` — and the loop continues with the next tick.
    /// A panicking cycle does not count as completed.
    #[tokio::test(start_paused = true)]
    async fn a_panicking_cycle_is_contained_and_the_loop_continues() {
        let buf = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sub = tracing_subscriber::fmt()
            .with_writer(BufWriter(buf.clone()))
            .with_max_level(tracing::Level::ERROR)
            .finish();
        let _guard = tracing::subscriber::set_default(sub);

        let (graph, store, scores) = session();
        let store = Arc::new(FaultyStore::wrapping(store));
        store.panic_records(true);
        let (tx, _rx) = event_channel();
        let task = task(graph.clone(), store.clone(), scores, tx);
        let handle = task.spawn();

        tokio::time::sleep(Duration::from_secs(61)).await;
        assert!(
            !handle.is_finished(),
            "an uncontained panic would have aborted the loop"
        );
        assert_eq!(task.cycles(), 0, "a panicking cycle does not count");
        assert_eq!(task.failures(), 0);
        // The panic landed in phase 4, after the commit point.
        assert_eq!(
            graph
                .read()
                .concepts()
                .find(|c| c.id == nid(20))
                .unwrap()
                .canonization_status,
            CanonizationStatus::Candidate,
        );

        store.panic_records(false);
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(task.cycles(), 1, "the loop continued with the next tick");
        assert!(!handle.is_finished());

        let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(
            out.contains("CanonizationCyclePanic"),
            "the panic must be logged: {out}"
        );
        assert!(
            out.contains("simulated canonization panic (FaultyStore)"),
            "panic payload missing: {out}"
        );
        handle.abort();
    }

    /// F2: the loop exists, consumes `canonization_eval_interval`, and drives
    /// real transitions in a spawned task with the graph behind the
    /// production `Arc<RwLock<Graph>>` — the shape the review found had no
    /// possible caller. A wake (not a sleep) triggers the cycle.
    #[tokio::test]
    async fn spawned_loop_promotes_through_the_shared_graph() {
        let (graph, store, scores) = session();
        let (tx, mut rx) = event_channel();
        let task = task(graph.clone(), Arc::new(store), scores, tx);
        let handle = task.spawn();

        task.wake();
        let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("a Canonized event within 5s")
            .unwrap();
        match event {
            DaemonEvent::Canonized { event } => {
                assert_eq!(event.node_id, nid(20));
                assert_eq!(event.from_status, CanonizationStatus::None);
                assert_eq!(event.to_status, CanonizationStatus::Candidate);
            }
            other => panic!("expected Canonized, got {other:?}"),
        }
        assert_eq!(
            graph
                .read()
                .concepts()
                .find(|c| c.id == nid(20))
                .unwrap()
                .canonization_status,
            CanonizationStatus::Candidate,
            "the transition must be visible through the shared graph handle"
        );
        handle.abort();
    }

    /// The interval knob is genuinely consumed: a task whose interval is long
    /// and which is never woken runs no cycle, and the same task woken runs
    /// exactly the cycles it was woken for.
    #[tokio::test(start_paused = true)]
    async fn cycles_are_driven_by_the_configured_interval() {
        let (graph, store, scores) = session();
        let (tx, _rx) = event_channel();
        let mut task = task(graph, Arc::new(store), scores, tx);
        task.interval = Duration::from_secs(60);
        let handle = task.spawn();

        // A fresh interval's immediate tick is consumed, so nothing has run.
        tokio::time::sleep(Duration::from_secs(59)).await;
        assert_eq!(task.cycles(), 0, "no cycle before the first interval");

        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(task.cycles(), 1, "one cycle at the first interval");

        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(task.cycles(), 2, "one cycle per interval");
        assert_eq!(task.failures(), 0);
        handle.abort();
    }

    /// Single-loop enforcement, mirroring `Daemon::spawn` / `FlushTask::spawn`:
    /// two loops would each carry their own round-robin cursors.
    #[tokio::test]
    #[should_panic(expected = "exactly one loop may run")]
    async fn spawn_twice_panics() {
        let (graph, store, scores) = session();
        let (tx, _rx) = event_channel();
        let task = task(graph, Arc::new(store), scores, tx);
        let _first = task.spawn();
        let _second = task.spawn();
    }

    /// `from_daemon` is the assembled-process wiring: it must take the
    /// daemon's own score table and event channel (a private table would
    /// leave Stage 1 with an empty P90 population forever, and a private
    /// channel would break the re-arm accounting).
    #[tokio::test]
    async fn from_daemon_shares_the_daemon_score_table_and_channel() {
        let (graph, store, _scores) = session();
        let daemon = Daemon::new(
            graph.clone(),
            ScoringWeights::default(),
            Duration::from_secs(3600),
        );
        let config = Config::default();
        let task = CanonizationTask::from_daemon(graph, Arc::new(store), &daemon, &config);
        assert_eq!(task.interval, config.canonization_eval_interval);
        assert_eq!(task.params, EvalParams::from_config(&config));
        assert!(
            Arc::ptr_eq(&task.scores, &daemon.score_table()),
            "the task must read the daemon's table, not a private one"
        );
        // Same channel: a send here reaches a daemon subscriber.
        let mut rx = daemon.events();
        crate::daemon::events::emit_canonized(
            &task.events,
            crate::types::CanonizationEvent {
                id: NodeId::new(),
                session_id: sid(),
                node_id: nid(1),
                from_status: CanonizationStatus::None,
                to_status: CanonizationStatus::Candidate,
                blast_radius: None,
                last_demotion_time: None,
                occurred_at: ts(),
            },
        );
        assert!(matches!(rx.try_recv(), Ok(DaemonEvent::Canonized { .. })));
    }
}
