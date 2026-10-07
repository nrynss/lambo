//! Admission and lanes: per-agent FIFO, the lane, aggregate and byte
//! bounds, sealing and fencing.

use super::*;

/// **Per-agent FIFO under interleaving.** Two agents submit alternately;
/// each agent's own writes must apply in that agent's submission order.
///
/// The `Temporal` chain is pinned by construction **for writes an agent
/// sends one after another**, which is what this test submits: the
/// interaction is opened on the call path, so a sequential caller's chain
/// position is fixed before its lane position is. Two calls the same agent
/// has in flight *simultaneously* are not covered — `begin_interaction_full`
/// and `admit`'s `lanes.lock()` are two critical sections with no ordering
/// between them across threads (J3-R1-10). So what this test has to prove
/// is the other half: the *drain* order within a lane, which is what
/// decides which of two identical concepts is `created` and which is
/// `matched`.
#[tokio::test]
async fn each_agents_writes_drain_in_that_agents_submission_order() {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let rig = Rig::new(
        "wq-fifo",
        Arc::new(HeldEmbedder {
            gate: gate.clone(),
            inner: FixtureEmbedder::new(),
            calls: calls.clone(),
        }),
    );
    // Canonical strategy never embeds, so the gate only holds the probe.
    // Release its whole allowance (PROBE_EMBEDS, not PROBE_CONCURRENCY —
    // the probe warms up and times a serial leg before its concurrent one)
    // so calibration lands.
    gate.add_permits(PROBE_EMBEDS);

    let a = AgentId::new("agent-a");
    let b = AgentId::new("agent-b");
    // Interleaved submission: a1, b1, a2, b2, a3, b3. Each agent derives
    // the SAME content twice, so ordering is visible as created-then-
    // matched rather than the reverse.
    let mut a_receipts = Vec::new();
    let mut b_receipts = Vec::new();
    for i in 0..3 {
        a_receipts.push(rig.derive(&a, &format!("a-concept-{}", i / 2)).await);
        b_receipts.push(rig.derive(&b, &format!("b-concept-{}", i / 2)).await);
    }

    for (owner, r) in a_receipts
        .iter()
        .map(|r| (&a, r))
        .chain(b_receipts.iter().map(|r| (&b, r)))
    {
        let answer = rig.pipeline.wait(owner, r.receipt, RECEIPT_WAIT_MAX).await;
        assert_eq!(answer.tag(), "applied", "{answer:?}");
    }

    // a-concept-0 is submitted twice in a row (i = 0, 1): the first must be
    // the create and the second the match. Reversed drain order would swap
    // them, and nothing else in the system would notice.
    let created_first = matches!(
        rig.pipeline.lookup(&a, a_receipts[0].receipt),
        ReceiptAnswer::Applied(ref s) if s.created_count == 1 && s.matched_count == 0
    );
    let matched_second = matches!(
        rig.pipeline.lookup(&a, a_receipts[1].receipt),
        ReceiptAnswer::Applied(ref s) if s.created_count == 0 && s.matched_count == 1
    );
    assert!(created_first, "agent-a's first write must be the create");
    assert!(matched_second, "agent-a's second write must be the match");

    // And agent-b's lane is unaffected by having been interleaved into it.
    let b_created = matches!(
        rig.pipeline.lookup(&b, b_receipts[0].receipt),
        ReceiptAnswer::Applied(ref s) if s.created_count == 1
    );
    assert!(
        b_created,
        "interleaving across agents must not reorder a lane"
    );
}

/// **A sealed queue refuses and counts it** — the `DropReason::Closed`
/// path, which is what this test always exercised. Renamed from
/// `a_burst_past_the_bound_drops_and_counts_it`, which named a property it
/// skipped: sealing is not the count bound, and because `Closed` used to
/// ride `dropped_queue_full`'s counter the assertion below passed without
/// the bound ever binding (J3-R1-5). The real bounds are exercised by the
/// three tests that follow.
#[tokio::test]
async fn a_sealed_queue_refuses_and_counts_it() {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let rig = Rig::new(
        "wq-sealed",
        Arc::new(HeldEmbedder {
            gate: gate.clone(),
            inner: FixtureEmbedder::new(),
            calls: calls.clone(),
        }),
    );
    let calibration = calibrate_through_gate(&rig, &gate).await;
    assert_eq!(calibration.bound, WRITE_QUEUE_MAX);

    rig.pipeline.seal();
    let agent = AgentId::new("agent-a");
    let refused = rig.derive(&agent, "dropped concept").await;
    assert!(refused.dropped(), "{:?}", refused.answer);
    assert_eq!(refused.answer.tag(), "dropped");
    assert!(
        refused.answer.describe().contains("nothing was written"),
        "a drop must say plainly that nothing was written: {}",
        refused.answer.describe()
    );
    assert!(
        refused.answer.describe().contains("session is closing"),
        "a sealed refusal must name its own reason, not the bound's: {}",
        refused.answer.describe()
    );
    let counters = rig.pipeline.counters();
    assert_eq!(counters.dropped(), 1);
    // J3-R1-8: a closing refusal is counted apart from a bound refusal, and
    // `dropped()` is still their sum.
    assert_eq!(counters.dropped_closed(), 1);
    assert_eq!(counters.dropped_queue_full(), 0);
    assert_eq!(counters.dropped_queue_bytes(), 0);
    assert_eq!(
        counters.accepted(),
        0,
        "a refused admission must never enter `accepted` — the whole gauge rests on it"
    );
    assert_eq!(counters.outstanding(), 0);
    // The receipt is still fetchable: a drop is an answer, not a silence.
    assert_eq!(
        rig.pipeline.lookup(&agent, refused.receipt).tag(),
        "dropped"
    );
}

/// **`DropReason::LaneFull`, exercised for real** (J3-R1-5): one agent
/// bursting past its own lane's measured depth, behind an embedder slow
/// enough for the queue to hold, with the count bound doing the refusing.
#[tokio::test]
async fn a_burst_past_the_lane_bound_drops_and_counts_it() {
    let calls = Arc::new(AtomicUsize::new(0));
    let rig = Rig::hybrid(
        "wq-lane-full",
        Arc::new(SlowEmbedder {
            delay: Duration::from_millis(100),
            inner: FixtureEmbedder::new(),
            calls: calls.clone(),
        }),
    );
    let agent = AgentId::new("agent-a");
    let first = rig.derive(&agent, "lane concept 0").await;
    assert_eq!(first.answer.tag(), "pending");

    let mut refusals = Vec::new();
    for i in 1..=(WRITE_QUEUE_LANE_MAX + 4) {
        let submitted = rig.derive(&agent, &format!("lane concept {i}")).await;
        if submitted.dropped() {
            refusals.push(submitted);
        }
    }
    assert!(
        !refusals.is_empty(),
        "a burst of {} past the fair-share cap of {} must be refused",
        WRITE_QUEUE_LANE_MAX + 4,
        WRITE_QUEUE_LANE_MAX
    );
    let counters = rig.pipeline.counters();
    assert_eq!(counters.dropped_queue_full() as usize, refusals.len());
    assert_eq!(counters.dropped_closed(), 0, "nothing is closing");
    assert_eq!(counters.dropped_queue_bytes(), 0, "the payloads are tiny");
    assert!(
        counters.accepted() as usize <= WRITE_QUEUE_LANE_MAX,
        "one lane must never be admitted past its fair share: accepted={} cap={}",
        counters.accepted(),
        WRITE_QUEUE_LANE_MAX
    );
    for refused in &refusals {
        let detail = refused.answer.describe();
        assert!(
            detail.contains("lane is full")
                && detail.contains("fair-share")
                && detail.contains("nothing was written"),
            "a lane refusal must name the fair-share cap and say nothing was written \
                 (J3-R3-4): {detail}"
        );
    }
    // A clean close accounts for every admitted job: what fits the budget
    // applies, the rest defers to durable intents — nothing abandons.
    let deferred = rig.pipeline.quiesce().await;
    assert_eq!(counters.abandoned(), 0);
    assert_eq!(counters.failed(), 0);
    assert_eq!(
        counters.applied() + deferred as u64,
        counters.accepted(),
        "acked ⇒ applied ∨ durable intent, at a clean close"
    );
}

/// **`DropReason::QueueFull`, exercised for real** (J3-R1-5): enough lanes,
/// each inside its own bound, to reach the aggregate one. This is the
/// condition that existed before J3-R1-1 and it still has a job — the
/// per-lane bound alone would let N agents queue N x lane_bound writes.
#[tokio::test]
async fn enough_lanes_together_reach_the_aggregate_bound_and_it_counts_them() {
    let calls = Arc::new(AtomicUsize::new(0));
    let rig = Rig::hybrid(
        "wq-aggregate-full",
        Arc::new(SlowEmbedder {
            delay: Duration::from_millis(100),
            inner: FixtureEmbedder::new(),
            calls: calls.clone(),
        }),
    );
    // Enough lanes to overrun the aggregate cap even though no single one
    // overruns its own fair share.
    let lanes = WRITE_QUEUE_MAX / WRITE_QUEUE_LANE_MAX + 2;

    let mut aggregate_refusals = 0usize;
    for lane in 0..lanes {
        let agent = AgentId::new(format!("agent-{lane}"));
        for i in 0..WRITE_QUEUE_LANE_MAX {
            let submitted = rig
                .derive(&agent, &format!("lane {lane} concept {i}"))
                .await;
            if submitted.dropped() {
                let detail = submitted.answer.describe();
                assert!(
                    detail.contains("write queue is full"),
                    "no lane exceeded its own fair share, so every refusal here must be the \
                         aggregate one: {detail}"
                );
                aggregate_refusals += 1;
            }
        }
    }
    assert!(
        aggregate_refusals > 0,
        "{lanes} lanes of {} must reach the aggregate cap of {}",
        WRITE_QUEUE_LANE_MAX,
        WRITE_QUEUE_MAX
    );
    let counters = rig.pipeline.counters();
    assert_eq!(counters.dropped_queue_full() as usize, aggregate_refusals);
    assert!(counters.accepted() <= WRITE_QUEUE_MAX as u64);
    assert_eq!(counters.dropped_closed(), 0);
}

/// **`DropReason::QueueBytes` and `WRITE_QUEUE_MAX_BYTES`, exercised at
/// all** (J3-R1-5): before this test nothing touched `lanes.bytes`, and a
/// count is the wrong unit for memory — which is the whole reason the byte
/// bound exists.
#[tokio::test]
async fn a_burst_past_the_byte_cap_drops_and_counts_it() {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let rig = Rig::hybrid(
        "wq-byte-cap",
        Arc::new(HeldEmbedder {
            gate: gate.clone(),
            inner: FixtureEmbedder::new(),
            calls: calls.clone(),
        }),
    );
    // The static count bounds (64 per lane, 1024 aggregate) are far out of
    // reach of the two jobs below, so the only bound that can bind is the
    // byte one. Then the gate closes again, so real jobs park with their
    // payloads still queued.
    let calibration = calibrate_through_gate(&rig, &gate).await;
    assert_eq!(calibration.lane_bound, WRITE_QUEUE_LANE_MAX);
    assert!(
        WRITE_QUEUE_LANE_MAX > 2 && calibration.bound > 2,
        "the count bounds must stay out of reach of the two jobs below: {calibration:?}"
    );

    // Half the cap per job, so the second crosses it whether or not the
    // worker has already taken the first off the lane.
    let chunk = WRITE_QUEUE_MAX_BYTES / 2 + 1;
    let payload = "x".repeat(chunk);
    let agent = AgentId::new("agent-a");
    let mut refusals = Vec::new();
    for _ in 0..4 {
        let submitted = rig.derive(&agent, &payload).await;
        if submitted.dropped() {
            refusals.push(submitted);
        }
    }
    assert!(
        !refusals.is_empty(),
        "4 jobs of {chunk} bytes must cross the {WRITE_QUEUE_MAX_BYTES}-byte cap"
    );
    let counters = rig.pipeline.counters();
    assert_eq!(counters.dropped_queue_bytes() as usize, refusals.len());
    assert_eq!(
        counters.dropped_queue_full(),
        0,
        "the count bounds are clamped wide open here — only the byte cap may refuse"
    );
    assert_eq!(counters.dropped_closed(), 0);
    for refused in &refusals {
        let detail = refused.answer.describe();
        assert!(
            detail.contains("payload cap") && detail.contains("nothing was written"),
            "a byte-cap refusal must name the cap and say nothing was written: {detail}"
        );
    }
    // The accounting is a gauge, not a running total: releasing the lane
    // must return the bytes, or a long-lived session would refuse writes
    // for payloads it applied hours ago.
    gate.add_permits(16);
    until(
        || rig.pipeline.outstanding() == 0,
        "the parked payloads to drain",
    )
    .await;
    let after = rig.derive(&agent, &payload).await;
    assert!(
        !after.dropped(),
        "a drained lane must accept a payload it had room for: {}",
        after.answer.describe()
    );
}

/// A fenced handle (lease lost) must settle its queued writes as `failed`
/// without writing any of them into a session another writer now owns.
#[tokio::test]
async fn a_fenced_pipeline_refuses_every_queued_write() {
    let rig = Rig::fixture("wq-fenced");
    let agent = AgentId::new("agent-a");
    rig.pipeline.ctx.lease_lost.store(true, Ordering::Release);
    let submitted = rig.derive(&agent, "post-fence concept").await;
    let answer = rig
        .pipeline
        .wait(&agent, submitted.receipt, RECEIPT_WAIT_MAX)
        .await;
    assert_eq!(answer.tag(), "failed", "{answer:?}");
    assert!(
        answer.describe().contains("lost its single-writer lease"),
        "{}",
        answer.describe()
    );
    assert_eq!(rig.pipeline.counters().abandoned(), 1);
    assert!(
        rig.pipeline.counters().abandoned() <= rig.pipeline.counters().failed(),
        "abandoned is a subset of failed"
    );
    assert_eq!(
        rig.graph.read().concepts().count(),
        0,
        "a fenced pipeline must not write"
    );
}
