//! #295 review: the in-session hold under admission, the caps and the fence.
//! Each test mirrors a theorem of the event model in
//! `formal/LamboProofs/Drain.lean` (`stepW`, `runW`).

use super::*;
use crate::writeq::execution::held_retry_backoff;

/// An embedder that is down (`EmbedError::Unavailable`) while `down` is set
/// and, while `gated`, parks real content on `gate`; the calibration probe's
/// own texts always answer.
struct Flaky {
    inner: FixtureEmbedder,
    down: AtomicBool,
    gated: AtomicBool,
    gate: tokio::sync::Semaphore,
    entered: AtomicUsize,
    /// Embeds of real content attempted, answered or not.
    calls: AtomicUsize,
}

impl Flaky {
    fn new() -> Arc<Self> {
        Arc::new(Flaky {
            inner: FixtureEmbedder::new(),
            down: AtomicBool::new(true),
            gated: AtomicBool::new(false),
            gate: tokio::sync::Semaphore::new(0),
            entered: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait::async_trait]
impl Embedder for Flaky {
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
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        self.inner.as_any()
    }
}

impl Flaky {
    async fn embed_as(
        &self,
        text: &str,
        role: crate::test_util::TextRole,
    ) -> Result<Vec<f32>, crate::EmbedError> {
        // The calibration probe's texts repeat `PROBE_TEXT`.
        if !text.starts_with(&crate::writeq::PROBE_TEXT[..16]) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.down.load(Ordering::SeqCst) {
                return Err(crate::EmbedError::Unavailable(
                    "llama.cpp unreachable: connection refused".into(),
                ));
            }
            if self.gated.load(Ordering::SeqCst) {
                self.entered.fetch_add(1, Ordering::SeqCst);
                self.gate
                    .acquire()
                    .await
                    .expect("the gate outlives its holders")
                    .forget();
            }
        }
        role.embed(&self.inner, text).await
    }
}

/// The receipts the graph's log consumed as `applied`, in the order the
/// writes applied (the rig has no flush loop, so the log is all there).
fn applied_order(rig: &Rig) -> Vec<String> {
    rig.graph
        .write()
        .drain_log()
        .mutations
        .into_iter()
        .filter_map(|m| match m {
            crate::types::Mutation::ConsumeWriteIntent {
                receipt, outcome, ..
            } if outcome.tag == "applied" => Some(receipt),
            _ => None,
        })
        .collect()
}

/// Submit `n` writes of `agent` and wait until the worker has taken each
/// (its receipt is no longer `pending`).
async fn submit_held(rig: &Rig, agent: &AgentId, n: usize, tag: &str) -> Vec<ReceiptId> {
    let mut ids = Vec::new();
    for i in 0..n {
        let submitted = rig.derive(agent, &format!("{tag} fact {i}")).await;
        assert!(!submitted.dropped(), "{}", submitted.answer.describe());
        ids.push(submitted.receipt);
    }
    for id in &ids {
        until(
            || rig.pipeline.lookup(agent, *id).tag() != "pending",
            "the worker to take the write",
        )
        .await;
    }
    ids
}

/// **Lane wedge, and an agent that only waits**: an outage holds
/// a full lane of an agent's writes; a further write is refused, and its
/// refusal does not tell the agent to wait on receipts that cannot settle.
/// Once the embedder is back, the held writes apply, in order, with no new
/// write: the worker retries them on its backoff timer.
///
/// Mirrors `Drain.retry_settles_head`, `Drain.retry_enabled` and
/// `Drain.stepW_caps_free` (a retry needs no admission and never reads the
/// caps; that it comes due is the worker's timer).
#[tokio::test(start_paused = true)]
async fn held_writes_are_retried_without_a_new_write_even_when_the_lane_is_full() {
    let embedder = Flaky::new();
    let rig = Rig::hybrid("hold-lane-wedge", embedder.clone());
    let agent = AgentId::new("agent-a");
    let ids = submit_held(&rig, &agent, WRITE_QUEUE_LANE_MAX, "held").await;
    for id in &ids {
        assert_eq!(rig.pipeline.lookup(&agent, *id).tag(), "pending_replay");
    }
    let refused = rig.derive(&agent, "one too many").await;
    assert!(refused.dropped(), "the lane is full of held writes");
    let detail = refused.answer.describe();
    assert!(
        !detail.contains("wait for receipts") && detail.contains("retried in the background"),
        "the refusal must not send the agent to wait on held receipts: {detail}"
    );

    embedder.down.store(false, Ordering::SeqCst);
    until(
        || {
            ids.iter()
                .all(|id| rig.pipeline.lookup(&agent, *id).tag() == "applied")
        },
        "the held writes to apply with no new write",
    )
    .await;
    let order = applied_order(&rig);
    let expected: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
    assert_eq!(order, expected, "held writes apply in admission order");
}

/// **Session wedge**: held writes across agents fill the whole
/// queue, so every agent is refused; the held writes are still retried and
/// apply once the embedder is back, with no new write.
#[tokio::test(start_paused = true)]
async fn a_queue_full_of_held_writes_is_still_retried() {
    let embedder = Flaky::new();
    let rig = Rig::hybrid("hold-queue-wedge", embedder.clone());
    let lanes = WRITE_QUEUE_MAX / WRITE_QUEUE_LANE_MAX;
    let mut held = Vec::new();
    for a in 0..lanes {
        let agent = AgentId::new(format!("agent-{a}"));
        let ids = submit_held(&rig, &agent, WRITE_QUEUE_LANE_MAX, &format!("a{a}")).await;
        held.push((agent, ids));
    }
    let other = AgentId::new("agent-late");
    assert!(
        rig.derive(&other, "no room").await.dropped(),
        "the queue is full of held writes"
    );
    embedder.down.store(false, Ordering::SeqCst);
    until(
        || {
            held.iter().all(|(agent, ids)| {
                ids.iter()
                    .all(|id| rig.pipeline.lookup(agent, *id).tag() == "applied")
            })
        },
        "every held write to apply with no new write",
    )
    .await;
}

/// **Held bytes**: a held write's bytes stay counted. A lane of maximal
/// held writes (the longest concept a hybrid derive embeds) keeps every byte
/// in the queue's byte gauge, which the byte cap checks, until each settles;
/// once the embedder is back and they apply, the gauge returns to zero.
///
/// Mirrors `Drain.stepW_caps`.
#[tokio::test(start_paused = true)]
async fn held_writes_keep_their_bytes_counted_until_they_settle() {
    let embedder = Flaky::new();
    let rig = Rig::hybrid("hold-bytes", embedder.clone());
    let agent = AgentId::new("agent-a");
    let mut ids = Vec::new();
    for i in 0..WRITE_QUEUE_LANE_MAX {
        let mut content = format!("maximal held fact {i} ");
        content.push_str(
            &"x".repeat(crate::graph::hybrid::MAX_SINGLE_CONCEPT_CONTEXT_BYTES - content.len()),
        );
        let submitted = rig.derive(&agent, &content).await;
        assert!(!submitted.dropped(), "{}", submitted.answer.describe());
        ids.push((submitted.receipt, content.len()));
    }
    for (id, _) in &ids {
        until(
            || rig.pipeline.lookup(&agent, *id).tag() != "pending",
            "the worker to take the write",
        )
        .await;
        assert_eq!(rig.pipeline.lookup(&agent, *id).tag(), "pending_replay");
    }
    let held_bytes = rig.pipeline.lanes.lock().bytes;
    assert!(
        held_bytes >= ids.iter().map(|(_, n)| n).sum::<usize>(),
        "every held write's bytes stay counted: {held_bytes}"
    );
    embedder.down.store(false, Ordering::SeqCst);
    until(
        || {
            ids.iter()
                .all(|(id, _)| rig.pipeline.lookup(&agent, *id).tag() == "applied")
        },
        "the held writes to apply",
    )
    .await;
    assert_eq!(
        rig.pipeline.lanes.lock().bytes,
        0,
        "and are released as each settles"
    );
}

/// **Lease fence**: the lease fence is checked for every write a retry takes.
/// Two writes are held; the retry runs the first, and the lease is lost
/// while it is running. The first commits (the store refuses that flush);
/// the second, and a later write, are refused without being run.
///
/// Mirrors `Drain.fence_stops_applies`.
#[tokio::test(start_paused = true)]
async fn a_lease_lost_mid_retry_runs_no_later_held_write() {
    let embedder = Flaky::new();
    let rig = Rig::hybrid("hold-fence", embedder.clone());
    let agent = AgentId::new("agent-a");
    let ids = submit_held(&rig, &agent, 2, "held").await;
    embedder.down.store(false, Ordering::SeqCst);
    embedder.gated.store(true, Ordering::SeqCst);
    // A new write of the agent; the retry comes when its backoff is due.
    let third = rig.derive(&agent, "a later fact").await;
    assert!(!third.dropped());
    until(
        || embedder.entered.load(Ordering::SeqCst) > 0,
        "the retry to reach the embedder",
    )
    .await;
    rig.pipeline.ctx.lease_lost.store(true, Ordering::SeqCst);
    embedder.gated.store(false, Ordering::SeqCst);
    embedder.gate.add_permits(16);
    until(
        || {
            rig.pipeline.lookup(&agent, third.receipt).tag() != "pending"
                && rig.pipeline.lookup(&agent, third.receipt).tag() != "pending_replay"
        },
        "the last write to settle",
    )
    .await;
    assert_eq!(rig.pipeline.lookup(&agent, ids[0]).tag(), "applied");
    for id in [ids[1], third.receipt] {
        let answer = rig.pipeline.lookup(&agent, id);
        assert_eq!(answer.tag(), "failed");
        assert!(
            answer.describe().contains("lost its single-writer lease"),
            "{answer:?}"
        );
    }
    assert_eq!(applied_order(&rig), vec![ids[0].to_string()]);
}

/// **Retry backoff**: a new or refused write of an agent with held writes
/// does not start a retry before the backoff has passed. Five writes sent
/// right after the outage are held without a single further embed attempt;
/// before, each one retried the held write at once.
#[tokio::test]
async fn new_writes_do_not_retry_a_held_write_before_its_backoff() {
    let embedder = Flaky::new();
    let rig = Rig::hybrid("hold-no-bypass", embedder.clone());
    let agent = AgentId::new("agent-a");
    let first = submit_held(&rig, &agent, 1, "first").await;
    let attempts = embedder.calls.load(Ordering::SeqCst);
    let started = tokio::time::Instant::now();
    let more = submit_held(&rig, &agent, 5, "more").await;
    assert!(
        started.elapsed() < held_retry_backoff(1),
        "the writes must land inside the first backoff for this test to mean anything"
    );
    for id in first.iter().chain(&more) {
        assert_eq!(rig.pipeline.lookup(&agent, *id).tag(), "pending_replay");
    }
    assert_eq!(
        embedder.calls.load(Ordering::SeqCst),
        attempts,
        "no write may start a retry before the backoff has passed"
    );
}
