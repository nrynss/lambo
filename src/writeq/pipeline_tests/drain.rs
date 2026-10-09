//! Drain and replay: quiesce, clean-close bursts and durable intents.

use super::*;

/// The quiesce must not leave a receipt answering `pending` forever in a
/// process that is exiting.
#[tokio::test]
async fn quiesce_settles_everything_it_could_not_apply() {
    let rig = Rig::fixture("wq-quiesce");
    let agent = AgentId::new("agent-a");
    let submitted = rig.derive(&agent, "tail concept").await;
    let deferred = rig.pipeline.quiesce().await;
    let answer = rig.pipeline.lookup(&agent, submitted.receipt);
    assert!(
        answer.is_settled(),
        "close must leave no receipt pending: {answer:?}"
    );
    // Either it drained inside the budget (applied) or it was deferred to a
    // durable intent and said so — never `pending`, and never a silent loss.
    match answer {
        ReceiptAnswer::Applied(_) => assert_eq!(deferred, 0),
        ReceiptAnswer::IntentRecorded => assert_eq!(deferred, 1),
        other => panic!("unexpected {other:?}"),
    }
    // And the queue is sealed against anything new.
    let after = rig.derive(&agent, "after close").await;
    assert!(after.dropped(), "{:?}", after.answer);
}

/// **The founding invariant, at the shape J3-R1-1 first measured it** —
/// one agent bursting far past what its single-consumer lane can drain in
/// a close's budget. At `528ade6` this exact shape abandoned **61 of 80
/// acked writes**; three estimator revisions later it still abandoned at
/// other shapes (J3-R2-1, J3-R3-1, J3-R3-2). Under durable intents the
/// invariant holds **by construction**: every acked write is applied, or a
/// durable intent the next serve applies, or was refused at the door —
/// never failed, never silently lost, whatever the drain arithmetic says.
#[tokio::test]
async fn one_agents_burst_never_loses_an_acked_write_at_a_clean_close() {
    const EMBED: Duration = Duration::from_millis(100);
    const BURST: usize = 80;

    let calls = Arc::new(AtomicUsize::new(0));
    let rig = Rig::hybrid(
        "wq-lane-drain",
        Arc::new(SlowEmbedder {
            delay: EMBED,
            inner: FixtureEmbedder::new(),
            calls: calls.clone(),
        }),
    );
    let agent = AgentId::new("agent-a");

    let mut receipts = Vec::with_capacity(BURST);
    for i in 0..BURST {
        receipts.push(rig.derive(&agent, &format!("burst concept {i}")).await);
    }
    let counters = rig.pipeline.counters();
    let accepted = counters.accepted();
    assert!(accepted > 0, "the queue must admit something");
    assert!(
        counters.dropped() > 0,
        "an {BURST}-deep burst must cross the {WRITE_QUEUE_LANE_MAX}-job fair-share cap"
    );
    assert_eq!(
        accepted + counters.dropped(),
        BURST as u64,
        "every submission is either accepted or refused"
    );

    let deferred = rig.pipeline.quiesce().await;

    // 64 accepted × 100 ms against a 2 s budget: some MUST defer — this
    // burst is deliberately larger than the budget can drain, because that
    // is the regime the old design abandoned writes in.
    assert!(
        deferred > 0,
        "the burst must outrun the budget for this test to prove anything"
    );
    assert_eq!(counters.abandoned(), 0, "a clean close abandons NOTHING");
    assert_eq!(counters.failed(), 0, "deferral is not failure");
    assert_eq!(
        counters.applied() + deferred as u64,
        accepted,
        "acked ⇒ applied ∨ durable intent (applied={}, deferred={deferred})",
        counters.applied(),
    );
    assert_eq!(counters.outstanding(), 0);

    // The truth table, per receipt: applied, deferred to a durable intent,
    // or refused at the door. Never pending, never failed, never silent.
    let mut durable = 0usize;
    for submitted in &receipts {
        let answer = rig.pipeline.lookup(&agent, submitted.receipt);
        match answer {
            ReceiptAnswer::Applied(_) => {}
            ReceiptAnswer::IntentRecorded => durable += 1,
            ReceiptAnswer::Dropped(_) => assert!(
                answer.describe().contains("nothing was written"),
                "{}",
                answer.describe()
            ),
            other => panic!("an acked write ended {other:?}"),
        }
    }
    assert_eq!(durable, deferred, "one intent_durable receipt per deferral");

    // And the log carries exactly one unconsumed intent per deferred
    // write — what the close's final flush would persist.
    let log = rig.graph.write().drain_log();
    let puts: Vec<String> = log
        .mutations
        .iter()
        .filter_map(|m| match m {
            crate::types::Mutation::PutWriteIntent { intent } => Some(intent.receipt.clone()),
            _ => None,
        })
        .collect();
    let consumed: Vec<String> = log
        .mutations
        .iter()
        .filter_map(|m| match m {
            crate::types::Mutation::ConsumeWriteIntent { receipt, .. } => Some(receipt.clone()),
            _ => None,
        })
        .collect();
    let unconsumed = puts.iter().filter(|r| !consumed.contains(r)).count();
    assert_eq!(unconsumed, deferred);
}

/// **The invariant at content sizes the probe never sampled** (J3-R2-1's
/// regime, re-pinned under durable intents). An embedder whose per-job
/// cost is first-order in input length is the shape that falsified two
/// generations of estimator; the invariant must not care. At 512 B (the
/// band lambo's own dogfood concepts occupy) and at 8 KiB (beyond anything
/// any probe leg measures): acked ⇒ applied ∨ durable intent, abandoned 0,
/// failed 0 — whatever the drain arithmetic would have said.
#[tokio::test]
async fn a_burst_of_concepts_larger_than_the_probes_text_loses_nothing_at_a_clean_close() {
    const BURST: usize = 800;

    for content_bytes in [512, PROBE_TEXT_BYTES * 8] {
        let calls = Arc::new(AtomicUsize::new(0));
        let rig = Rig::hybrid(
            &format!("wq-content-{content_bytes}"),
            Arc::new(LengthProportionalEmbedder {
                per_5_bytes: Duration::from_micros(200),
                inner: FixtureEmbedder::new(),
                calls: calls.clone(),
            }),
        );
        let agent = AgentId::new("agent-a");

        let mut receipts = Vec::with_capacity(BURST);
        for i in 0..BURST {
            let content = format!("{i:04} {}", "x".repeat(content_bytes - 5));
            receipts.push(rig.derive(&agent, &content).await);
        }
        let counters = rig.pipeline.counters();
        let accepted = counters.accepted();
        assert!(
            accepted > 0,
            "the queue must admit something at {content_bytes} B"
        );
        assert!(
            counters.dropped() > 0,
            "an {BURST}-deep burst must cross the {WRITE_QUEUE_LANE_MAX}-job fair share \
                 at {content_bytes} B"
        );
        assert_eq!(
            accepted + counters.dropped(),
            BURST as u64,
            "every submission is either accepted or refused"
        );

        let deferred = rig.pipeline.quiesce().await;

        assert_eq!(
            counters.abandoned(),
            0,
            "at {content_bytes}-byte concepts a clean close abandons NOTHING"
        );
        assert_eq!(counters.failed(), 0, "deferral is not failure");
        assert_eq!(
            counters.applied() + deferred as u64,
            accepted,
            "acked ⇒ applied ∨ durable intent at {content_bytes} B (applied={}, \
                 deferred={deferred}, embeds={})",
            counters.applied(),
            calls.load(Ordering::Relaxed),
        );
        assert_eq!(counters.outstanding(), 0);
        let mut durable = 0usize;
        for submitted in &receipts {
            let answer = rig.pipeline.lookup(&agent, submitted.receipt);
            match answer {
                ReceiptAnswer::Applied(_) => {}
                ReceiptAnswer::IntentRecorded => durable += 1,
                ReceiptAnswer::Dropped(_) => assert!(
                    answer.describe().contains("nothing was written"),
                    "{}",
                    answer.describe()
                ),
                other => panic!("an acked write ended {other:?}"),
            }
        }
        assert_eq!(durable, deferred, "one intent_durable receipt per deferral");
    }
}

/// **J3 durable intents, the write-side half.** Every accepted job's
/// intent enters the mutation log AT ADMISSION — before the job is visible
/// to a worker — and its consumption is appended when the job applies,
/// with the applied outcome, strictly after its put. This ordering is what
/// the admit-side graph⊃lanes lock nesting exists for: a log that could
/// carry a consume ahead of its put would let one flush transaction
/// consume an intent whose put it never wrote.
#[tokio::test]
async fn an_accepted_write_puts_a_durable_intent_and_applying_consumes_it() {
    let rig = Rig::fixture("wq-intent-lifecycle");
    let agent = AgentId::new("agent-a");
    let submitted = rig.derive(&agent, "user schema").await;
    assert!(!submitted.dropped());
    let settled = rig
        .pipeline
        .wait(&agent, submitted.receipt, RECEIPT_WAIT_MAX)
        .await;
    assert_eq!(settled.tag(), "applied");

    let log = rig.graph.write().drain_log();
    let receipt = submitted.receipt.to_string();
    let put_at = log.mutations.iter().position(|m| {
        matches!(m, crate::types::Mutation::PutWriteIntent { intent } if intent.receipt == receipt && intent.outcome.is_none())
    });
    let consume_at = log.mutations.iter().position(|m| {
        matches!(m, crate::types::Mutation::ConsumeWriteIntent { receipt: r, outcome, .. } if *r == receipt && outcome.tag == "applied")
    });
    let put_at = put_at.expect("the ack must have put a durable intent in the log");
    let consume_at = consume_at.expect("the apply must have consumed the intent, tagged applied");
    assert!(
        put_at < consume_at,
        "the put ({put_at}) must precede its consume ({consume_at}) in the log"
    );
    // And the consume carries the SAME sentence the receipt carries, so
    // the durable record and the live answer can never tell two stories.
    let ReceiptAnswer::Applied(summary) = settled else {
        unreachable!()
    };
    match &log.mutations[consume_at] {
        crate::types::Mutation::ConsumeWriteIntent { outcome, .. } => {
            assert_eq!(outcome.summary, summary.summary);
        }
        _ => unreachable!(),
    }
}

/// **J3 durable intents, the close-side half — the founding invariant, by
/// construction.** A clean close over a queue that cannot drain defers the
/// remainder instead of abandoning it: receipts settle `intent_durable`
/// (never `failed`), the count lands in `deferred` (never `abandoned`),
/// and the log still holds every undrained job's intent, unconsumed, for
/// the close's final flush to persist.
#[tokio::test(start_paused = true)]
async fn a_close_that_cannot_drain_defers_acked_writes_as_durable_intents() {
    // 3 s per embed: slower than the whole drain budget, and slow enough
    // that the probe cannot finish inside PROBE_BUDGET — the floors era,
    // one write per lane, which is exactly the regime where a write CAN be
    // admitted and yet not drain. Four agents, one acked write each.
    const EMBED: Duration = Duration::from_secs(3);
    let calls = Arc::new(AtomicUsize::new(0));
    let rig = Rig::hybrid(
        "wq-close-defers",
        Arc::new(SlowEmbedder {
            delay: EMBED,
            inner: FixtureEmbedder::new(),
            calls: calls.clone(),
        }),
    );
    let agents: Vec<AgentId> = (0..4).map(|i| AgentId::new(format!("agent-{i}"))).collect();
    let mut receipts = Vec::new();
    for (i, agent) in agents.iter().enumerate() {
        let submitted = rig.derive(agent, &format!("burst concept {i}")).await;
        assert!(!submitted.dropped(), "submission {i} must be admitted");
        receipts.push((agent.clone(), submitted.receipt));
    }

    // Every embed needs 3 s of a 2 s budget: nothing can drain, everything
    // MUST defer.
    let deferred = rig.pipeline.quiesce().await;
    let counters = rig.pipeline.counters();
    assert!(deferred > 0, "a 2 s budget cannot cover a 3 s embed");
    assert_eq!(counters.deferred(), deferred as u64);
    assert_eq!(
        counters.abandoned(),
        0,
        "a clean close abandons NOTHING under durable intents"
    );
    assert_eq!(counters.failed(), 0, "deferral is not failure");
    assert_eq!(counters.outstanding(), 0, "every job is accounted for");

    let mut applied = 0usize;
    let mut durable = 0usize;
    for (agent, receipt) in &receipts {
        match rig.pipeline.lookup(agent, *receipt) {
            ReceiptAnswer::Applied(_) => applied += 1,
            ReceiptAnswer::IntentRecorded => durable += 1,
            other => panic!("an acked write ended {other:?} at a clean close"),
        }
    }
    assert_eq!(applied + durable, receipts.len());
    assert_eq!(durable, deferred);

    // The log's word matches the receipts': one unconsumed intent per
    // deferred write, none for the applied ones.
    let log = rig.graph.write().drain_log();
    let puts: Vec<&str> = log
        .mutations
        .iter()
        .filter_map(|m| match m {
            crate::types::Mutation::PutWriteIntent { intent } => Some(intent.receipt.as_str()),
            _ => None,
        })
        .collect();
    let consumed: Vec<&str> = log
        .mutations
        .iter()
        .filter_map(|m| match m {
            crate::types::Mutation::ConsumeWriteIntent { receipt, .. } => Some(receipt.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(puts.len(), receipts.len(), "one intent per acked write");
    assert_eq!(consumed.len(), applied, "one consume per APPLIED write");
    let unconsumed = puts.iter().filter(|r| !consumed.contains(*r)).count();
    assert_eq!(
        unconsumed, durable,
        "every deferred write's intent survives, unconsumed, for the final flush"
    );
}

/// Blocks its thread inside `embed` while `busy` is set (a synchronous
/// stretch, as a large graph commit is), then yields once before answering,
/// so an abort that landed during the stretch takes effect at that yield.
struct BusyEmbedder {
    busy: Arc<AtomicBool>,
    inner: FixtureEmbedder,
    calls: Arc<AtomicUsize>,
    /// Stretches that have ended.
    finished: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Embedder for BusyEmbedder {
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::EmbedError> {
        self.embed_as(text, crate::test_util::TextRole::Document)
            .await
    }
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, crate::EmbedError> {
        self.embed_as(text, crate::test_util::TextRole::Query).await
    }
    fn modalities(&self) -> crate::embed::Modalities {
        self.inner.modalities()
    }
    async fn embed_image(
        &self,
        image: crate::embed::ImageInput<'_>,
    ) -> Result<Vec<f32>, crate::EmbedError> {
        self.inner.embed_image(image).await
    }
}

impl BusyEmbedder {
    /// The `embed` behaviour above, in either text role (#22: a wrapper
    /// forwards the role, so its inner embedder sees what the caller asked).
    async fn embed_as(
        &self,
        text: &str,
        role: crate::test_util::TextRole,
    ) -> Result<Vec<f32>, crate::EmbedError> {
        if self.busy.load(Ordering::SeqCst) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(400));
            self.finished.fetch_add(1, Ordering::SeqCst);
            tokio::task::yield_now().await;
        }
        role.embed(&self.inner, text).await
    }
}

/// R3-1, at the write queue: a `close()` cancelled while
/// [`WritePipeline::abort_workers`] is joining one lane worker must not leave
/// the **other** workers un-aborted, nor let a retried close return before
/// the workers it could not join have stopped. `close()`'s contract is that a
/// cancelled close strands no background task; a worker dropped un-aborted
/// keeps applying its job into a graph the retried close may already have
/// drained, and one dropped un-joined can still finish a synchronous stretch
/// after it.
///
/// A join is only pending while its task is *running*, so both workers are
/// held in a synchronous stretch on their own runtime threads when the abort
/// is polled once and dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_abort_leaves_no_lane_worker_running() {
    let busy = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(AtomicUsize::new(0));
    let rig = Rig::hybrid(
        "wq-cancelled-abort",
        Arc::new(BusyEmbedder {
            busy: busy.clone(),
            inner: FixtureEmbedder::new(),
            calls: calls.clone(),
            finished: finished.clone(),
        }),
    );
    until(
        || rig.pipeline.calibration().is_some(),
        "the probe to finish",
    )
    .await;
    busy.store(true, Ordering::SeqCst);

    // Two lanes, each running inside its own job's embed.
    rig.derive(&AgentId::new("agent-a"), "alpha concept").await;
    rig.derive(&AgentId::new("agent-b"), "beta concept").await;
    until(
        || calls.load(Ordering::SeqCst) >= 2,
        "both jobs inside the embedder",
    )
    .await;

    // Poll the abort once, then drop it: a close() cancelled mid-join.
    {
        let abort = rig.pipeline.abort_workers();
        tokio::pin!(abort);
        let polled = tokio::time::timeout(Duration::ZERO, &mut abort).await;
        assert!(polled.is_err(), "the first join completed inside one poll");
    }

    // The retried close's abort must join what the cancelled one could not:
    // when it returns, both stretches are over.
    rig.pipeline.abort_workers().await;
    assert_eq!(
        finished.load(Ordering::SeqCst),
        2,
        "the retried abort_workers returned while a worker it never joined was still running"
    );

    // An aborted worker is cancelled at the yield after its stretch; a worker
    // that was never aborted applies its job.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let concepts = rig.graph.read().concepts().count();
    assert_eq!(
        concepts, 0,
        "a lane worker kept running after a cancelled abort_workers and applied its job"
    );
}

/// R3-1, at the intent replay: a `close()` cancelled while
/// [`WritePipeline::stop_replay`] is joining the replay task must leave the
/// handle where a retried close finds it, so the retry does not return while
/// the replay is still inside a synchronous stretch that can write the graph.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_stop_replay_leaves_the_replay_joinable() {
    let busy = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(AtomicUsize::new(0));
    let rig = Rig::hybrid(
        "wq-cancelled-stop-replay",
        Arc::new(BusyEmbedder {
            busy: busy.clone(),
            inner: FixtureEmbedder::new(),
            calls: calls.clone(),
            finished: finished.clone(),
        }),
    );
    // Let the calibration probe through first, so only the replay's own
    // embeds (the liveness check, then the intent) are held.
    until(
        || rig.pipeline.calibration().is_some(),
        "the probe to finish",
    )
    .await;
    busy.store(true, Ordering::SeqCst);
    let agent = AgentId::new("agent-a");
    let interaction = rig.interaction(&agent);
    let now = *rig.now.lock();
    // A durable intent "left by a previous process": a foreign epoch.
    let receipt = ReceiptId::new(rig.pipeline.epoch ^ 1, now, 1);
    let intent = crate::types::WriteIntent {
        session_id: rig.graph.read().session_id().clone(),
        receipt: receipt.to_string(),
        agent: agent.clone(),
        interaction,
        lane_seq: 1,
        issued_ms: receipt.issued_ms(),
        payload: crate::types::WriteIntentPayload::Derive {
            concepts: vec![("gamma concept".into(), ConceptType::Entity)],
            pairs: Vec::new(),
        },
        created_at: now,
        outcome: None,
    };
    let Rig {
        pipeline, graph, ..
    } = rig;
    let pipeline = Arc::new(pipeline);
    pipeline.spawn_replay(vec![intent]);

    // The liveness embed, then the intent's own embed, are both entered.
    until(
        || calls.load(Ordering::SeqCst) >= 2,
        "the replay inside its intent's embed",
    )
    .await;

    // Poll the stop once, then drop it: a close() cancelled mid-join.
    {
        let stop = pipeline.stop_replay();
        tokio::pin!(stop);
        let polled = tokio::time::timeout(Duration::ZERO, &mut stop).await;
        assert!(polled.is_err(), "the join completed inside one poll");
    }

    pipeline.stop_replay().await;
    assert_eq!(
        finished.load(Ordering::SeqCst),
        2,
        "the retried stop_replay returned while the replay task was still running"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        graph.read().concepts().count(),
        0,
        "an aborted replay applied its intent"
    );
}

/// A retried close after a cancelled [`WritePipeline::abort_workers`] must
/// not spend its drain budget waiting on workers that can never settle. The
/// cancelled call already drained the queues and aborted every worker; an
/// aborted worker never runs its `running -= 1`, so `outstanding()` stays
/// above zero and nothing will ever notify `settled`. The retried
/// [`WritePipeline::quiesce`] must go straight to the joins instead of
/// sleeping out [`WRITE_QUEUE_DRAIN_BUDGET`] first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_quiesce_after_a_cancelled_abort_does_not_wait_out_the_budget() {
    let busy = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(AtomicUsize::new(0));
    let rig = Rig::hybrid(
        "wq-retried-quiesce",
        Arc::new(BusyEmbedder {
            busy: busy.clone(),
            inner: FixtureEmbedder::new(),
            calls: calls.clone(),
            finished: finished.clone(),
        }),
    );
    until(
        || rig.pipeline.calibration().is_some(),
        "the probe to finish",
    )
    .await;
    busy.store(true, Ordering::SeqCst);

    rig.derive(&AgentId::new("agent-a"), "alpha concept").await;
    rig.derive(&AgentId::new("agent-b"), "beta concept").await;
    until(
        || calls.load(Ordering::SeqCst) >= 2,
        "both jobs inside the embedder",
    )
    .await;

    // A close() cancelled inside abort_workers.
    {
        let abort = rig.pipeline.abort_workers();
        tokio::pin!(abort);
        let polled = tokio::time::timeout(Duration::ZERO, &mut abort).await;
        assert!(polled.is_err(), "the first join completed inside one poll");
    }
    assert!(
        rig.pipeline.outstanding() > 0,
        "the aborted workers' jobs still count as running, or this proves nothing"
    );

    // The retried close's quiesce: the stretches end within 400 ms, so a
    // quiesce that goes straight to the joins returns well inside the budget.
    let started = std::time::Instant::now();
    let deferred = rig.pipeline.quiesce().await;
    let took = started.elapsed();
    assert!(
        took < WRITE_QUEUE_DRAIN_BUDGET / 2,
        "the retried quiesce waited {took:?} on workers that were already aborted"
    );
    assert_eq!(
        finished.load(Ordering::SeqCst),
        2,
        "the retried quiesce returned before joining the aborted workers"
    );
    assert_eq!(rig.pipeline.outstanding(), 0);
    assert_eq!(deferred, 2, "both acked jobs are deferred, not lost");
}

/// #11 review round 2, F1: a receipt wait that runs while
/// [`WritePipeline::abort_workers`] is still joining its workers must not
/// answer `pending` for a queued job the same close is about to settle
/// `intent_durable`. The seal check that ends a `pending_replay` wait at
/// close used to fire from the moment the workers were marked aborted, a
/// whole join before the settle; the wait has to see the close's final
/// answer.
///
/// Deterministic: the lane's worker is held in a synchronous stretch, so the
/// abort, polled once, sits in its join while the wait starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wait_inside_the_close_join_sees_the_intent_durable_settle() {
    let busy = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(AtomicUsize::new(0));
    let rig = Rig::hybrid(
        "wq-wait-join-window",
        Arc::new(BusyEmbedder {
            busy: busy.clone(),
            inner: FixtureEmbedder::new(),
            calls: calls.clone(),
            finished: finished.clone(),
        }),
    );
    until(
        || rig.pipeline.calibration().is_some(),
        "the probe to finish",
    )
    .await;
    busy.store(true, Ordering::SeqCst);

    // One job running inside its embed, one queued behind it on the lane.
    let agent = AgentId::new("agent-a");
    rig.derive(&agent, "alpha concept").await;
    until(
        || calls.load(Ordering::SeqCst) >= 1,
        "the first job inside the embedder",
    )
    .await;
    let queued = rig.derive(&agent, "beta concept").await;
    assert_eq!(queued.answer.tag(), "pending", "{:?}", queued.answer);

    // The close's abort, polled once: workers marked aborted, join pending.
    let abort = rig.pipeline.abort_workers();
    tokio::pin!(abort);
    let polled = tokio::time::timeout(Duration::ZERO, &mut abort).await;
    assert!(polled.is_err(), "the join completed inside one poll");

    // A wait that starts inside the join window, raced against the rest of
    // the close.
    let (deferred, answer) = tokio::join!(
        abort,
        rig.pipeline.wait(&agent, queued.receipt, RECEIPT_WAIT_MAX)
    );
    assert!(deferred >= 1, "the queued job was deferred by the close");
    assert!(
        matches!(answer, ReceiptAnswer::IntentRecorded),
        "a wait inside the close's join answered before its settle: {answer:?}"
    );
}
