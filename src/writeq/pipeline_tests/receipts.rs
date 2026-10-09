//! Receipt answers, retention and the completion line.

use super::*;

#[tokio::test]
async fn an_applied_receipt_reports_what_the_write_did() {
    let rig = Rig::fixture("wq-applied");
    let agent = AgentId::new("agent-a");
    let first = rig.derive(&agent, "user schema").await;
    assert_eq!(first.answer.tag(), "pending", "the ack precedes the write");

    let settled = rig
        .pipeline
        .wait(&agent, first.receipt, RECEIPT_WAIT_MAX)
        .await;
    let ReceiptAnswer::Applied(s) = settled else {
        panic!("expected applied, got {settled:?}");
    };
    assert_eq!(s.created_count, 1);
    assert_eq!(s.matched_count, 0);
    assert_eq!(s.kind, WriteKind::Derive);

    // A re-derive matches instead of creating — the metric-2 distinction,
    // now observable on the receipt.
    let second = rig.derive(&agent, "user schema").await;
    let settled = rig
        .pipeline
        .wait(&agent, second.receipt, RECEIPT_WAIT_MAX)
        .await;
    let ReceiptAnswer::Applied(s) = settled else {
        panic!("expected applied, got {settled:?}");
    };
    assert_eq!(s.created_count, 0);
    assert_eq!(s.matched_count, 1);
}

/// Issue #16 §2 on the J3 asynchronous path: concepts are created at apply
/// time, after the ack, so the receipt is the only place the caller learns
/// what the write did. A `parent_of` end in neither `concepts` nor the graph
/// is created there, and the receipt must count it as embedded. Before the
/// fix this settled "2 created (1 embedded)" over a parent with no vector.
#[tokio::test]
async fn an_applied_receipt_counts_an_embedded_parent_of_end() {
    let rig = Rig::hybrid_persisting("wq-parent-of-embedded", Arc::new(FixtureEmbedder::new()));
    let agent = AgentId::new("agent-a");
    let interaction = rig.interaction(&agent);
    let submitted = rig
        .pipeline
        .submit_derive(
            agent.clone(),
            interaction,
            vec![("user schema".to_string(), ConceptType::Entity)],
            vec![("document:src.md".to_string(), "user schema".to_string())],
        )
        .await;
    let settled = rig
        .pipeline
        .wait(&agent, submitted.receipt, RECEIPT_WAIT_MAX)
        .await;
    let ReceiptAnswer::Applied(s) = settled else {
        panic!("expected applied, got {settled:?}");
    };
    assert_eq!(s.created_count, 2);
    assert_eq!(
        s.embedded,
        Some(2),
        "the receipt must count the parent_of end as embedded: {}",
        s.summary
    );
    assert!(
        s.summary.contains("2 created (2 embedded)"),
        "the receipt sentence: {}",
        s.summary
    );
    assert!(
        rig.graph.read().concepts().all(|c| c.embedding.is_some()),
        "every concept the write created carries a vector"
    );
}

/// `record_action` embeds the concepts it creates under the hybrid strategy
/// (bef53e6), so its receipt must say how many, as a derive's does. It used
/// to report `embedded: None` under a stale "never embeds by design"
/// comment, which made an action write's vectors invisible on the one
/// surface an agent reads after the ack.
#[tokio::test]
async fn an_applied_action_receipt_reports_its_embedded_count() {
    let rig = Rig::hybrid_persisting("wq-action-embedded", Arc::new(FixtureEmbedder::new()));
    let agent = AgentId::new("agent-a");
    let interaction = rig.interaction(&agent);
    let submitted = rig
        .pipeline
        .submit_action(
            agent.clone(),
            interaction,
            "ran the migration".to_string(),
            vec!["schema v2".to_string()],
            Vec::new(),
            vec!["schema v1".to_string()],
        )
        .await;
    let settled = rig
        .pipeline
        .wait(&agent, submitted.receipt, RECEIPT_WAIT_MAX)
        .await;
    let ReceiptAnswer::Applied(s) = settled else {
        panic!("expected applied, got {settled:?}");
    };
    assert_eq!(s.kind, WriteKind::RecordAction);
    assert_eq!(s.created_count, 3);
    assert_eq!(
        s.embedded,
        Some(3),
        "a hybrid record_action embeds what it creates, and the receipt must \
         say so: {}",
        s.summary
    );
    assert!(
        s.summary.contains("3 concept(s) created (3 embedded)"),
        "the receipt sentence: {}",
        s.summary
    );
    assert!(rig.graph.read().concepts().all(|c| c.embedding.is_some()));
}

/// Hybrid `derive` embeds nothing when the store has no `VECTOR_SEARCH`, and
/// says "(0 embedded)". A hybrid `record_action` on the same store follows
/// the same rule: nothing embedded, no contract stamped, and a receipt that
/// counts zero rather than vectors the store cannot search.
#[tokio::test]
async fn a_hybrid_action_receipt_without_vector_search_embeds_nothing() {
    let mut rig = Rig::fixture("wq-action-no-vector");
    Arc::get_mut(&mut rig.pipeline.ctx)
        .expect("sole owner at build")
        .match_strategy = MatchStrategy::Hybrid;
    let agent = AgentId::new("agent-a");
    let interaction = rig.interaction(&agent);
    let submitted = rig
        .pipeline
        .submit_action(
            agent.clone(),
            interaction,
            "ran the migration".to_string(),
            vec!["schema v2".to_string()],
            Vec::new(),
            Vec::new(),
        )
        .await;
    let settled = rig
        .pipeline
        .wait(&agent, submitted.receipt, RECEIPT_WAIT_MAX)
        .await;
    let ReceiptAnswer::Applied(s) = settled else {
        panic!("expected applied, got {settled:?}");
    };
    assert_eq!(s.created_count, 2);
    assert_eq!(s.embedded, Some(0), "{}", s.summary);
    assert_eq!(
        s.summary,
        "recorded action: 2 concept(s) created (0 embedded), 1 edge(s)"
    );
    let g = rig.graph.read();
    assert!(g.concepts().all(|c| c.embedding.is_none()));
    assert!(g.embedding().is_none(), "no contract stamped");
}

/// Under `Canonical` nothing embeds, so the field stays absent rather than
/// reading as a zero of something that was attempted.
#[tokio::test]
async fn a_canonical_action_receipt_has_no_embedded_count() {
    let rig = Rig::fixture("wq-action-canonical");
    let agent = AgentId::new("agent-a");
    let interaction = rig.interaction(&agent);
    let submitted = rig
        .pipeline
        .submit_action(
            agent.clone(),
            interaction,
            "ran the migration".to_string(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .await;
    let settled = rig
        .pipeline
        .wait(&agent, submitted.receipt, RECEIPT_WAIT_MAX)
        .await;
    let ReceiptAnswer::Applied(s) = settled else {
        panic!("expected applied, got {settled:?}");
    };
    assert_eq!(s.embedded, None);
    assert_eq!(
        s.summary,
        "recorded action: 1 concept(s) created, 0 edge(s)"
    );
}

/// **§J3: expired must not read as unknown, and restart-lost must not
/// either.** All four non-answers, each distinct, none of them "unknown".
#[tokio::test]
async fn the_four_non_answers_are_distinguishable() {
    let rig = Rig::fixture("wq-answers");
    let agent = AgentId::new("agent-a");
    let mine = rig.derive(&agent, "held concept").await;
    rig.pipeline
        .wait(&agent, mine.receipt, RECEIPT_WAIT_MAX)
        .await;

    // 1. Another process's epoch.
    let foreign = ReceiptId {
        epoch: mine.receipt.epoch ^ 0xffff_ffff_ffff_ffff,
        issued_ms: mine.receipt.issued_ms,
        seq: mine.receipt.seq,
    };
    assert_eq!(
        rig.pipeline.lookup(&agent, foreign).tag(),
        "restart_lost",
        "a foreign epoch is restart-lost, not unknown"
    );

    // 2. Our epoch, a sequence number never issued.
    let unissued = ReceiptId {
        seq: mine.receipt.seq + 1_000,
        ..mine.receipt
    };
    assert_eq!(rig.pipeline.lookup(&agent, unissued).tag(), "never_issued");

    // 3. Our epoch, issued, retention elapsed.
    // Read into a local first: `parking_lot::Mutex` is not reentrant, and
    // `*m.lock() = *m.lock() + d` keeps the right-hand guard alive across
    // the left-hand acquire — a self-deadlock, which is exactly what this
    // line did on its first outing.
    let base = *rig.now.lock();
    *rig.now.lock() = base
        + chrono::Duration::from_std(RECEIPT_RETENTION).unwrap()
        + chrono::Duration::seconds(1);
    assert_eq!(
        rig.pipeline.lookup(&agent, mine.receipt).tag(),
        "expired",
        "a receipt this process issued and no longer holds is expired"
    );

    // 4. Held, but by another agent (J1 scoping).
    let held = rig.derive(&agent, "second concept").await;
    assert_eq!(
        rig.pipeline
            .lookup(&AgentId::new("agent-b"), held.receipt)
            .tag(),
        "forbidden",
        "a receipt is scoped to the agent that created it"
    );
}

/// Eviction is oldest-**settled**-first, which is what lets it collapse
/// into `expired` rather than becoming a fifth answer — **and it never
/// evicts a receipt whose write is still outstanding** (J3-R1-3). Asserted
/// on the store directly: filling `MAX_RETAINED_RECEIPTS` through the
/// pipeline would be 4096 real writes.
#[test]
fn eviction_is_oldest_settled_first_and_never_takes_a_running_writes_receipt() {
    let base = Utc::now();
    let entry = |seq: u64, settled: bool| Entry {
        agent: AgentId::new("agent-a"),
        settled_at: settled.then_some(base + chrono::Duration::milliseconds(seq as i64)),
        answer: if settled {
            ReceiptAnswer::Failed("settled".into())
        } else {
            ReceiptAnswer::Pending
        },
    };
    let id_at = |seq: u64| ReceiptId {
        epoch: 7,
        issued_ms: base.timestamp_millis() + seq as i64,
        seq,
    };

    // All settled: plain oldest-first.
    let mut r = Receipts::default();
    let mut ids = Vec::new();
    for seq in 1..=(MAX_RETAINED_RECEIPTS as u64 + 8) {
        let id = id_at(seq);
        ids.push(id);
        r.entries.insert(id, entry(seq, true));
        r.order.push_back(id);
        r.highest_seq = seq;
        r.evict();
    }
    assert_eq!(r.entries.len(), MAX_RETAINED_RECEIPTS);
    for evicted in &ids[..8] {
        assert!(!r.entries.contains_key(evicted), "{evicted} survived");
    }
    for held in &ids[8..] {
        assert!(r.entries.contains_key(held), "{held} was evicted early");
    }
    let oldest_held = ids[8];
    assert!(
        ids[..8].iter().all(|e| e.issued_ms < oldest_held.issued_ms),
        "an evicted settled id must be older than everything held, or 'expired' would be a lie"
    );

    // The oldest entry is a write still in flight, and the store is then
    // driven past its cap by refusals — which get receipts of their own, so
    // the count argument that used to protect this ("the outstanding set is
    // always inside the newest quarter") does not hold on its own.
    let mut r = Receipts::default();
    let running = id_at(1);
    r.entries.insert(running, entry(1, false));
    r.order.push_back(running);
    for seq in 2..=(MAX_RETAINED_RECEIPTS as u64 + 8) {
        let id = id_at(seq);
        r.entries.insert(id, entry(seq, true));
        r.order.push_back(id);
        r.highest_seq = seq;
        r.evict();
    }
    assert!(
        r.entries.contains_key(&running),
        "the receipt of a RUNNING write must never be evicted — it would answer 'expired' \
             about a job in flight, which is the one promise the taxonomy rests on"
    );
    assert_eq!(r.entries.len(), MAX_RETAINED_RECEIPTS);
    assert_eq!(
        r.order.front().copied(),
        Some(running),
        "a skipped entry must go back at the FRONT, or `order` stops being issue order"
    );
    assert!(
        !r.entries.contains_key(&id_at(2)),
        "the oldest SETTLED entry is the one that must have gone instead"
    );
}

/// **J3-R1-3, as a test.** A receipt whose job is still queued or running
/// must never answer `expired`, and its outcome must never be discarded
/// when it finally lands: expiry keyed on *issue* time does both, because
/// nothing caps how long a job sits in a lane.
#[tokio::test]
async fn a_running_jobs_receipt_neither_expires_nor_loses_its_outcome() {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let rig = Rig::hybrid(
        "wq-running-receipt",
        Arc::new(HeldEmbedder {
            gate: gate.clone(),
            inner: FixtureEmbedder::new(),
            calls: calls.clone(),
        }),
    );
    calibrate_through_gate(&rig, &gate).await;
    let agent = AgentId::new("agent-a");

    let before = calls.load(Ordering::Relaxed);
    let submitted = rig.derive(&agent, "parked in the embedder").await;
    assert_eq!(submitted.answer.tag(), "pending");
    until(
        || calls.load(Ordering::Relaxed) > before,
        "the worker to reach the embedder",
    )
    .await;
    assert_eq!(rig.pipeline.outstanding(), 1, "the job is still in flight");

    // Retention elapses while the job is parked.
    let base = *rig.now.lock();
    *rig.now.lock() = base
        + chrono::Duration::from_std(RECEIPT_RETENTION).unwrap()
        + chrono::Duration::seconds(1);
    assert_eq!(
        rig.pipeline.lookup(&agent, submitted.receipt).tag(),
        "pending",
        "a receipt for a RUNNING job must never expire out from under it"
    );

    // And when the write lands, its outcome is recorded rather than swept.
    gate.add_permits(8);
    let answer = rig
        .pipeline
        .wait(&agent, submitted.receipt, RECEIPT_WAIT_MAX)
        .await;
    assert_eq!(answer.tag(), "applied", "{answer:?}");
    assert_eq!(
        rig.pipeline.lookup(&agent, submitted.receipt).tag(),
        "applied",
        "the outcome of a write that applied must not be silently discarded"
    );
    assert_eq!(rig.pipeline.counters().applied(), 1);
}

/// J4 proof obligation 5: a durable write intent's applied lifecycle rides
/// the ledger as a `completion` line carrying the metric-2 fact set. Fails
/// on pre-J4 code, where `WriteCtx` had no ledger and no completion line
/// was ever emitted.
#[tokio::test]
async fn j4_a_completion_line_records_the_applied_derive_lifecycle() {
    let dir = crate::test_util::ScratchDir::new("lambo-j4-wq");
    let ledger_path = dir.join("calls.jsonl");
    let ledger = crate::ledger::Ledger::open(&ledger_path);

    let rig = Rig::new_with_ledger(
        "j4-completion",
        Arc::new(FixtureEmbedder::new()),
        Some(Arc::clone(&ledger)),
    );
    let agent = AgentId::new("agent-a");
    let submitted = rig.derive(&agent, "a brand new completion concept").await;
    assert_eq!(
        rig.pipeline
            .wait(&agent, submitted.receipt, RECEIPT_WAIT_MAX)
            .await
            .tag(),
        "applied",
        "the derive must apply before its completion fact is readable"
    );
    ledger.shutdown();

    let text = std::fs::read_to_string(&ledger_path).unwrap();
    let completions: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .filter(|v| v["kind"] == "completion")
        .collect();
    assert!(
        !completions.is_empty(),
        "an applied durable intent must emit a completion line; ledger: {text}"
    );
    let c = completions
        .iter()
        .find(|c| c["receipt"] == submitted.receipt.to_string())
        .unwrap_or_else(|| panic!("no completion line for the receipt; got {completions:?}"));
    assert_eq!(c["state"], "applied");
    assert_eq!(c["agent_id"], "agent-a");
    // Metric 2's fact set: a fresh derive created one concept, matched none.
    assert_eq!(c["created_count"], 1);
    assert_eq!(c["matched_count"], 0);
}

/// **A receipt wait outlasts any one write at the head of its lane** (#11).
///
/// `RECEIPT_WAIT_MAX` was 4 s. On the Metal rig a 3 to 4 concept derive's
/// apply ran to 4.6 s (p90 2 s), so a caller following the read-your-writes
/// protocol could be told `pending` about a healthy write about to land. A
/// write's own I/O is bounded by `HYBRID_IO_TIMEOUT`, so a wait at least that
/// long always ends on that write's terminal answer when nothing is queued
/// ahead of it. Here one embed takes a second under that bound.
#[tokio::test(start_paused = true)]
async fn a_receipt_wait_outlasts_one_write_at_the_head_of_its_lane() {
    let calls = Arc::new(AtomicUsize::new(0));
    let rig = Rig::hybrid(
        "wq-wait-covers-a-write",
        Arc::new(SlowEmbedder {
            delay: crate::graph::hybrid::HYBRID_IO_TIMEOUT - Duration::from_secs(1),
            inner: FixtureEmbedder::new(),
            calls: calls.clone(),
        }),
    );
    let agent = AgentId::new("agent-a");
    let submitted = rig.derive(&agent, "a slow but healthy write").await;
    assert_eq!(submitted.answer.tag(), "pending");
    let answer = rig
        .pipeline
        .wait(&agent, submitted.receipt, Duration::MAX)
        .await;
    assert_eq!(
        answer.tag(),
        "applied",
        "a wait clamped to RECEIPT_WAIT_MAX must cover the write's own I/O bound: {answer:?}"
    );
}

/// **Apply latency is admission to settle, applied writes only** (#11): the
/// queueing behind earlier writes counts, because a receipt waiter waits for
/// it too, and a failure is not recorded.
#[tokio::test(start_paused = true)]
async fn apply_latency_counts_the_queue_wait_and_only_applied_writes() {
    let calls = Arc::new(AtomicUsize::new(0));
    let rig = Rig::hybrid(
        "wq-apply-latency",
        Arc::new(SlowEmbedder {
            delay: Duration::from_millis(100),
            inner: FixtureEmbedder::new(),
            calls: calls.clone(),
        }),
    );
    let agent = AgentId::new("agent-a");
    assert!(rig.pipeline.apply_latency().is_none());
    // Two writes back to back on one lane: the second waits for the first.
    let first = rig.derive(&agent, "first queued write").await;
    let second = rig.derive(&agent, "second queued write").await;
    // A fast failure (stopword-only content is refused before any embed).
    let failed = rig.derive(&agent, "the and of a").await;
    for r in [first.receipt, second.receipt, failed.receipt] {
        rig.pipeline.wait(&agent, r, RECEIPT_WAIT_MAX).await;
    }
    assert_eq!(rig.pipeline.counters().failed(), 1);
    let s = rig.pipeline.apply_latency().expect("two applied writes");
    assert_eq!(s.samples, 2, "the failure is not a latency sample: {s:?}");
    assert!(
        s.max >= Duration::from_millis(200),
        "the second write waited behind the first: {s:?}"
    );
    assert!(s.p50 >= Duration::from_millis(100), "{s:?}");
}

/// **One agent cannot hold every receipt-wait slot** (#11 review P3-1).
///
/// The pipeline holds [`MAX_CONCURRENT_RECEIPT_WAITS`] waits at once, and a
/// wait over the cap answers at once rather than waiting. With no per-agent
/// share, one agent issuing sixteen waits on a slow write made every other
/// agent's wait answer `pending` immediately, for as long as
/// [`RECEIPT_WAIT_MAX`] (34 s since #11). A wait slot is now a fair share:
/// one agent holds at most [`MAX_RECEIPT_WAITS_PER_AGENT`] of them.
#[tokio::test(start_paused = true)]
async fn one_agent_cannot_hold_every_receipt_wait_slot() {
    let calls = Arc::new(AtomicUsize::new(0));
    let rig = Rig::hybrid(
        "wq-wait-fairness",
        Arc::new(SlowEmbedder {
            delay: Duration::from_secs(20),
            inner: FixtureEmbedder::new(),
            calls: calls.clone(),
        }),
    );
    let greedy = AgentId::new("agent-greedy");
    let other = AgentId::new("agent-other");
    let greedy_write = rig
        .derive(&greedy, "a slow write the greedy agent waits on")
        .await;
    let other_write = rig
        .derive(&other, "a slow write another agent waits on")
        .await;
    let budget = Duration::from_secs(5);

    let timed = |agent: AgentId, id: ReceiptId| {
        let pipeline = &rig.pipeline;
        async move {
            let started = tokio::time::Instant::now();
            let answer = pipeline.wait(&agent, id, budget).await;
            (agent, answer, started.elapsed())
        }
    };
    let mut waits = Vec::new();
    for _ in 0..MAX_CONCURRENT_RECEIPT_WAITS {
        waits.push(timed(greedy.clone(), greedy_write.receipt));
    }
    waits.push(timed(other.clone(), other_write.receipt));
    let results = crate::writeq::calibration::futures_join_all(waits).await;

    let waited = |who: &AgentId| {
        results
            .iter()
            .filter(|(agent, answer, elapsed)| {
                agent == who && answer.tag() == "pending" && *elapsed >= budget
            })
            .count()
    };
    assert_eq!(
        waited(&other),
        1,
        "another agent's wait must get a slot however many the first one asked for: {results:?}"
    );
    assert_eq!(
        waited(&greedy),
        MAX_RECEIPT_WAITS_PER_AGENT,
        "one agent holds at most its share; its other waits answer at once: {results:?}"
    );
}
