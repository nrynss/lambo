//! Asynchronous write pipeline and write receipts (J3).
//!
//! # The rule
//!
//! A write may be acknowledged **before** it has been applied only when its
//! result does not gate the caller's next action. `derive` and `record_action`
//! qualify: a warm `derive` is 27 ms of which 22 to 27 ms is the embedding call
//! (`dev-diary/lambo-for-mooshik/J-multi-client.md` §Measurements; J3-R3-5
//! corrected this line's earlier "22 to 25 ms" misquote of that section),
//! durability
//! was *already* asynchronous (the write-behind log returns long before
//! anything reaches disk), and neither outcome is something the agent branches
//! on. **`reserve` never qualifies** — its result *is* the caller's next
//! action, and an asynchronous reservation has two agents editing while each
//! believes it holds the lock.
//!
//! # Shape
//!
//! 1. **The synchronous part stays on the call path.** Validation resolves
//!    against the graph, and the interaction node is opened here too (see
//!    *Ordering* below). What moves off the call path is the embedder wait, not
//!    the round trip: the round trip is 0.31 to 0.48 ms on the rig and is not
//!    worth removing.
//! 2. **Embed, canonicalize and insert in the background**, through the
//!    ordinary [`crate::graph::hybrid::derive`] /
//!    [`crate::graph::action::record_action`] path. Dedup is therefore
//!    unaffected: embedding still precedes insertion, so the vector is present
//!    when matching happens.
//! 3. **The ack carries a [`ReceiptId`]**, against which the outcome is stored.
//!    Receipts are delivered two ways — piggybacked on that agent's next tool
//!    response, and fetched by id — and the fetch doubles as **opt-in
//!    synchrony**: an agent that needs its write applied waits on the receipt,
//!    which restores read-your-writes on demand without charging every agent
//!    for it. There is no `await` flag and no MCP notification (a notification
//!    lands in a client log rather than in the model's context, which is the
//!    exact failure workstream J exists to fix).
//!
//! # Ordering
//!
//! **Scope first, because the strong sentence used to come first and its
//! retraction came nine lines later** (J3-R2-6): everything in this section is
//! a claim about **one agent's writes sent one after another**. Two calls one
//! agent has in flight *simultaneously* are outside it, for the reason spelled
//! out below.
//!
//! The interaction is opened **synchronously, on the call path**, before the
//! job is queued. `begin_interaction_full` takes the graph write
//! lock only briefly and never awaits, so this is cheap — and for a sequential
//! caller it makes submission order *be* `Temporal`-chain order by
//! construction. That is strictly stronger than ordering the drain: the chain
//! no longer depends on drain order at all, so an out-of-order drain cannot
//! corrupt it. Since J1 the chain is session-wide (see
//! `Memory::begin_interaction_full`), so "one agent's writes apply in submission
//! order" is read off the chain by filtering it on `agent_id`.
//!
//! Per-agent FIFO is **still** enforced in the drain, for a second reason:
//! insertion order decides which of two identical concepts is `created` and
//! which is `matched`, and that distinction is reported in the receipt. Each
//! agent gets its own lane with a single consumer, so a lane drains in
//! submission order; lanes run concurrently, because interleaving *across*
//! agents is fine.
//!
//! **The scope of both promises, stated once more where the mechanism is**
//! (J3-R1-10): one agent's *sequential* submissions. The chain position is pinned
//! by `begin_interaction_full` and the
//! lane position by the `lanes.lock()` inside `WritePipeline::admit`, and
//! those are two critical sections with no ordering between them across
//! threads. So for two `lambo_derive` calls one agent has in flight *at the
//! same time*, the chain order and the drain order can disagree — task A can
//! open its interaction first and enqueue second. The consequence is confined
//! to created/matched attribution between those two calls, and a caller that
//! fires two writes concurrently has asserted no order for them to keep;
//! closing the window would mean opening the interaction under the lane lock,
//! which nests the graph write lock inside it. What must not happen is claiming
//! more than that, which the first version of this section did.
//!
//! # Backpressure — fairness and memory, never durability (the J3 redesign)
//!
//! Three review rounds produced five falsified estimator axes — width, warmth,
//! length, failure shape, concurrency scaling — every one a P1, because the
//! durability invariant ("no acked write is silently abandoned") was **coupled
//! to an estimator's correctness**: a clean close had a deadline and the
//! deadline's arithmetic rested on a measured rate. The series does not
//! converge; an estimator is wrong in as many ways as the workload has
//! covariates (`dev-diary/lambo-for-mooshik/J3-durability-redesign.md`).
//!
//! **Durable intents cut the coupling.** Every accepted job is recorded as a
//! [`crate::types::Mutation::PutWriteIntent`] at admission, so at a clean
//! close acked ⇒ (applied ∨ durable intent) **by construction** — the next
//! serve replays the remainder. Being wrong about the drain now costs a
//! deferral or a refusal, never a loss.
//!
//! Admission therefore guards only what admission can honestly guard:
//! **memory** (the aggregate bound [`WRITE_QUEUE_MAX`], derived from the
//! receipt store's cap, and the byte cap [`WRITE_QUEUE_MAX_BYTES`]) and
//! **fairness** (the per-lane bound [`WRITE_QUEUE_LANE_MAX`], one agent's
//! share of the queue). Both are static and generous, derived at their
//! constants from structural facts — not from a rate, because J3's five axes
//! are what happens when a rate is asked to carry an invariant.
//!
//! The probe and the observed rate survive as **telemetry**: the probe still
//! measures two input sizes and publishes the slower
//! ([`Calibration::probe_serial_items_per_sec`]), real write service times
//! still take over after [`OBSERVED_MIN_SAMPLES`] completed writes, and the
//! ratio between them ([`Calibration::probe_optimism`]) remains the payload's
//! self-diagnosing comparison (J3-R2-4). None of it sizes a bound any more.
//! The drop policy is fixed regardless — bound, drop, log once, count in
//! `lambo_stats`.
//!
//! # Accounting (the `ledger_queued_lines` lesson, re-derived)
//!
//! This module keeps its **own** counters and never touches
//! [`crate::ledger::LedgerCounters`], so the ledger's
//! `accepted − written − write_failed` keeps its exclusivity argument intact:
//! no new class enters the ledger's `accepted`. The queue mirrors that
//! discipline deliberately — a queue-full or byte-cap reject never enters
//! [`WriteQueueCounters::accepted`], so
//! `outstanding = accepted − applied − failed − deferred` is one expression
//! serving both the live gauge and the shutdown count, and cannot drift between
//! them. `abandoned` is a **label on a subset of `failed`**, not a fourth term:
//! an abandoned job is settled `failed`, and counting it twice is exactly the
//! mistake `adve-review-mooshik-I-round3.md`'s flip D maps. `deferred` **is** a
//! term — a close-deferred job settled `intent_durable` left this process's
//! custody without being applied or failed — and this line omitted it (J3
//! round-1 N5). The drift was inside the section whose whole thesis is that
//! there must be **one** expression, which is the reminder that a thesis does
//! not enforce itself: [`WriteQueueCounters::outstanding`] is the authority and
//! this sentence is a copy of it.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex as PlMutex;
use serde_json::json;
use tokio::sync::{watch, Notify, Semaphore};
use tokio::task::JoinHandle;

use crate::types::{AgentId, LamboError, WriteIntent, WriteIntentOutcome};

mod receipts;

use receipts::{model_safe_failure, settle_one, Entry, Receipts};

mod calibration;

use calibration::{probe_embedder, ObservedRate};

mod counters;

pub use calibration::{
    Calibration, CalibrationSource, MEASURED_LOCAL_EMBEDDER_RPS, OBSERVED_EWMA_WEIGHT,
    OBSERVED_MIN_SAMPLES, PROBE_BUDGET, PROBE_CLAMP_RPS, PROBE_CONCURRENCY, PROBE_EMBEDS,
    PROBE_TEXT, PROBE_TEXT_BYTES, PROBE_WARMUP_EMBEDS,
};
pub use counters::{ReplayBlockReason, WriteQueueCounters};

mod admission;

pub use admission::{
    DropReason, Submitted, WRITE_QUEUE_LANE_MAX, WRITE_QUEUE_MAX, WRITE_QUEUE_MAX_BYTES,
};
use admission::{Job, JobPayload, Lanes};

mod execution;

use execution::ConsumeStamp;

mod drain;

pub use drain::WRITE_QUEUE_DRAIN_BUDGET;
pub(crate) use execution::{mirror_concepts, WriteCtx};
pub use receipts::{
    AppliedSummary, ReceiptAnswer, ReceiptId, WriteKind, MAX_CONCURRENT_RECEIPT_WAITS,
    MAX_PIGGYBACK_RECEIPTS, MAX_RECEIPT_IDS, MAX_RETAINED_RECEIPTS, MEASURED_WORST_FLUSH_LAG_SECS,
    RECEIPT_RETENTION, RECEIPT_WAIT_MAX,
};

// ---------------------------------------------------------------------------
// Constants — every one of them derived, at the constant, from something else
// in the tree or from a measurement in the phase doc.
// ---------------------------------------------------------------------------

/// How many **consecutive transient** embed failures within one attach end the
/// durable-intent replay loop, leaving the rest of the backlog durable
/// (J3-R2R-1 property 3 — the sequential decision rule, in the shape the design
/// doc's "termination measure" asks for).
///
/// Each replayed intent's status-classifier outcome is a Bernoulli draw
/// (`Transient` vs `Content`). A **content** rejection is absorbing — it is
/// permanent for *that input*, is consumed as `failed` immediately, and never
/// counts toward the embedder-sickness evidence. A **transient** draw is
/// evidence that the embedder is sick; it is left durable (never consumed) and
/// increments a running streak. The streak resets to zero on any applied
/// success or any content refusal (each proves the embedder just answered —
/// observed health), so only a run of `EMBEDDER_SICK_THRESHOLD` transients with
/// *nothing* answered in between concludes that the embedder is sick and stops
/// the loop.
///
/// **The two controls this threshold sets, stated (Wald's SPRT, deferred by the
/// design doc until the rule table existed; it now exists, so this is no longer
/// deferred):**
///
/// * **Burn bound** — intents at risk before the decision. At most this many
///   intents are spent as transient probe-embeds per attach; none is consumed
///   (all stay durable) so the cost is time, bounded by this ×
///   [`crate::graph::hybrid::HYBRID_IO_TIMEOUT`] worst case, never durability.
/// * **False-alarm tolerance** — wrongly labeling a healthy embedder sick. We
///   stop only after this many *consecutive* transients with no applied success
///   or content refusal between them, so an embedder that answers — even if it
///   occasionally blips — is never stopped.
///
/// Value: **3**, matching round-2's measured `k=3` and the design doc's
/// reading — enough that two independent one-off transients do not stop a
/// healthy replay, small enough that a hung-but-alive embedder costs three
/// bounded embeds per attach, not the whole backlog. Recorded in the design
/// doc's as-built disposition (J3-R2R-1).
pub const EMBEDDER_SICK_THRESHOLD: usize = 3;

// ---------------------------------------------------------------------------
// The pipeline
// ---------------------------------------------------------------------------

/// The J3 background write pipeline: bounded per-agent FIFO lanes feeding the
/// ordinary write path, plus the receipt store their outcomes land in.
///
/// Lives at **`Memory` level** rather than in the MCP server, so any owner —
/// the CLI included — can ack a write before the embedder. Delivery (the
/// piggyback and the fetch-by-id tool) is the MCP server's job: only `Memory`
/// can produce an outcome, and only the server knows how to render one to a
/// model.
pub struct WritePipeline {
    ctx: Arc<WriteCtx>,
    lanes: Arc<PlMutex<Lanes>>,
    receipts: Arc<PlMutex<Receipts>>,
    counters: Arc<WriteQueueCounters>,
    /// Woken on every settle: receipt waiters and [`WritePipeline::quiesce`].
    settled: Arc<Notify>,
    /// Fair-share cap on concurrent receipt waits (see
    /// [`MAX_CONCURRENT_RECEIPT_WAITS`]).
    wait_slots: Arc<Semaphore>,
    calibration: watch::Receiver<Option<Calibration>>,
    /// Service time observed on real writes, which **replaces** the probe's
    /// serial figure once [`OBSERVED_MIN_SAMPLES`] have been seen (J3-R1-2).
    observed: Arc<PlMutex<ObservedRate>>,
    probe: PlMutex<Option<JoinHandle<()>>>,
    /// Cross-restart receipt answers (J3 durable intents): receipts issued by
    /// **previous** processes whose fate this process knows — from the loaded
    /// intent records at attach (unconsumed → `Pending`; consumed → the stored
    /// outcome) and from this process's own replay as it settles them. Checked
    /// by [`WritePipeline::lookup`] before the epoch fallback, so these ids
    /// answer their truth instead of `restart_lost`. Agent-scoped like the
    /// live store.
    restart: PlMutex<HashMap<ReceiptId, (AgentId, ReceiptAnswer)>>,
    /// The replay task (J3), when this session attached over a durable intent
    /// backlog. Aborted at close — unconsumed intents stay durable for the
    /// next serve.
    replay: PlMutex<Option<JoinHandle<()>>>,
    epoch: u64,
    seq: AtomicU64,
    /// Latched the first time a drop is logged, so a sustained overload logs
    /// once rather than once per call. The count keeps telling the truth in
    /// `lambo_stats`.
    drop_logged: AtomicBool,
    /// Latched the first time observation displaces the probe's serial figure,
    /// so the transition is logged **once** with both numbers (J3-R2-4). It is
    /// a one-way transition ([`ObservedRate::samples`] only ever grows), so a
    /// latch here cannot suppress a second, different flip.
    observed_logged: AtomicBool,
    clock: crate::daemon::Clock,
}

impl fmt::Debug for WritePipeline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let lanes = self.lanes.lock();
        f.debug_struct("WritePipeline")
            .field("epoch", &self.epoch)
            .field("outstanding", &lanes.outstanding())
            .field("bound", &self.bound_snapshot())
            .finish()
    }
}

impl WritePipeline {
    /// Build the pipeline and **spawn** its calibration probe.
    ///
    /// Spawned rather than awaited: the probe measures the deployment's
    /// embedder, and making session build wait for it would put embedder
    /// latency on a startup path J2 has already made latency-sensitive. It
    /// sources **nothing** — the bounds are the static
    /// [`WRITE_QUEUE_LANE_MAX`] / [`WRITE_QUEUE_MAX`] whatever it reads, and
    /// `admit` never consults it (J3 round-1 N3: this docstring used to say the
    /// probe "is nonetheless the only source of the bound — admission awaits its
    /// result rather than falling back to a constant", both halves of which the
    /// estimator demotion made false). It is still bounded by [`PROBE_BUDGET`]
    /// and still always publishes something, for the same reason it survives at
    /// all: the probe/observed pair is the divergence telemetry.
    pub(crate) fn spawn(ctx: WriteCtx, clock: crate::daemon::Clock) -> Self {
        let (tx, rx) = watch::channel(None);
        let embedder = ctx.embedder.clone();
        let session = ctx.session.clone();
        let probe = tokio::spawn(async move {
            let calibration = probe_embedder(embedder.as_ref()).await;
            match calibration.items_per_sec {
                // J3 round-1 N3. These two lines are what an operator reads
                // about their own deployment, and both said the bounds came
                // from the probe. They never did after the estimator demotion —
                // the bounds are static and the second line even named
                // `WRITE_QUEUE_MIN`, "the unmeasured floor", which THIS BRANCH
                // deleted. Provenance first now, rates second, and neither line
                // claims a bound was measured.
                Some(rate) => tracing::info!(
                    session = %session,
                    items_per_sec = rate,
                    serial_items_per_sec = calibration.serial_items_per_sec,
                    bound = calibration.bound,
                    lane_bound = calibration.lane_bound,
                    concurrency = PROBE_CONCURRENCY,
                    "write queue: bounds are static (lane {}, queue {}) and no rate moves them; \
                     the rates below are telemetry measured on this deployment's embedder — the \
                     serial leg 1-wide, the aggregate {}-wide",
                    WRITE_QUEUE_LANE_MAX,
                    WRITE_QUEUE_MAX,
                    PROBE_CONCURRENCY
                ),
                None => tracing::warn!(
                    session = %session,
                    bound = calibration.bound,
                    "write queue: the embedder could not be probed within {:?}, so there is no \
                     rate telemetry this session and lambo_stats reports \
                     write_queue_measured=false. The bounds are unaffected — they are static \
                     (lane {}, queue {}) and never came from the probe. Note what a failed probe \
                     DOES suggest: with match_strategy=hybrid (the default) an embedder that \
                     cannot answer will also fail every derive it cannot answer",
                    PROBE_BUDGET,
                    WRITE_QUEUE_LANE_MAX,
                    WRITE_QUEUE_MAX
                ),
            }
            // A closed receiver means the session went away first; there is
            // nothing to report to and nothing to fix.
            let _ = tx.send(Some(calibration));
        });
        Self {
            ctx: Arc::new(ctx),
            lanes: Arc::new(PlMutex::new(Lanes::default())),
            receipts: Arc::new(PlMutex::new(Receipts::default())),
            counters: Arc::new(WriteQueueCounters::default()),
            settled: Arc::new(Notify::new()),
            wait_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_RECEIPT_WAITS)),
            calibration: rx,
            observed: Arc::new(PlMutex::new(ObservedRate::default())),
            probe: PlMutex::new(Some(probe)),
            restart: PlMutex::new(HashMap::new()),
            replay: PlMutex::new(None),
            epoch: rand_epoch(),
            seq: AtomicU64::new(0),
            drop_logged: AtomicBool::new(false),
            observed_logged: AtomicBool::new(false),
            clock,
        }
    }

    /// Queue counters, for `lambo_stats`.
    pub fn counters(&self) -> &Arc<WriteQueueCounters> {
        &self.counters
    }

    fn bound_snapshot(&self) -> usize {
        WRITE_QUEUE_MAX
    }

    /// Replay durable write intents left by previous processes (J3), spawned
    /// at attach.
    ///
    /// * **Order**: intents arrive from `load_session` sorted by
    ///   (`issued_ms`, `lane_seq`) and are applied strictly one at a time —
    ///   exact admission order within one issuing process (the per-lane
    ///   promise, since a total order refines every lane's), wall-clock order
    ///   across crashed processes.
    /// * **Throttling — the open question, decided here**: replay runs
    ///   sequentially in the background, at most ONE write in flight, and does
    ///   not pass through admission. A restart over a deep backlog therefore
    ///   costs the fresh session at most one embedder slot and brief graph
    ///   locks — it can never *refuse* the fresh session's first calls, which
    ///   admission-routed replay would do (a lane pre-filled with backlog
    ///   answers `lane_full` to the very calls the restart interrupted).
    ///   Admission exists for fairness among live callers; a replayed intent
    ///   already paid for admission in the session that acked it. The cost of
    ///   this choice is that a fresh write can land *before* a replayed intent
    ///   from the same agent — cross-restart interleaving is unordered, which
    ///   is the same scope §Ordering already declares (one agent's sequential
    ///   submissions, within a session).
    /// * **Idempotency**: consumption rides the same commit lock as the apply
    ///   (see [`WriteCtx::run`]), so a `kill -9` mid-replay re-replays exactly
    ///   the intents whose applies did not flush — never one whose apply did.
    /// * **Liveness before anything is consumed** (J3 round-1 N1): one embed of
    ///   [`PROBE_TEXT`] gates the loop. If it fails, the task warns and returns
    ///   **without consuming a single intent** — the backlog stays durable and
    ///   the next serve tries again. A dead or hanging embedder at attach
    ///   therefore costs one embed, not one `HYBRID_IO_TIMEOUT` per intent.
    /// * **Failure**: a replay that fails **for its own content**
    ///   ([`LamboError::Embed`] under the J3-R3-1 contract, a vanished
    ///   interaction, a validation refusal) consumes the intent with a `failed`
    ///   outcome — mirroring what the in-session worker does — rather than
    ///   retrying on every restart forever. A replay that fails for a reason
    ///   that says nothing about the intent (the embedder unreachable or timed
    ///   out, the store failing, the lease lost) leaves it **unconsumed** and
    ///   ends the loop: those are conditions a later process can be in a
    ///   position to fix, and settling an acked write `failed` because a
    ///   dependency blinked is the defect N1 named.
    /// * **Shutdown**: `close()` aborts this task before the quiesce; whatever
    ///   is still unconsumed stays durable for the next serve.
    pub(crate) fn spawn_replay(self: &Arc<Self>, intents: Vec<WriteIntent>) {
        if intents.is_empty() {
            return;
        }
        // Seed the cross-restart answers before the task starts, so a lookup
        // racing the replay sees `pending_replay` rather than `restart_lost`.
        //
        // J3 round-1 F2. A **consumed** row older than the retention window is
        // deliberately NOT seeded. `types::WRITE_INTENT_RETENTION` claimed
        // "expired rows are skipped at load" and no such filter existed in
        // either adapter, so a consumed row that outlived the window still
        // answered `applied_after_restart` — while the same receipt id in a
        // process that had NOT restarted would have been swept to `expired`.
        // That is the exact asymmetry the `RECEIPT_RETENTION ==
        // WRITE_INTENT_RETENTION` assert above exists to forbid ("a receipt's
        // answer must not depend on whether a restart intervened"), pointing
        // the other way. Skipping the row here makes it answer `restart_lost` —
        // a foreign epoch with no record — which is the honest analogue of
        // `expired` for another process's id, and it costs one clock read in one
        // place instead of a cutoff parameter threaded through three adapters'
        // load paths. **Unconsumed rows are seeded and replayed whatever their
        // age**: those are owed, and a debt does not expire.
        let stale_before = (self.clock)()
            - chrono::Duration::from_std(RECEIPT_RETENTION)
                .unwrap_or_else(|_| chrono::Duration::seconds(300));
        {
            let mut map = self.restart.lock();
            for intent in &intents {
                if let Some(o) = &intent.outcome {
                    if o.consumed_at < stale_before {
                        continue;
                    }
                }
                let Ok(id) = ReceiptId::from_str(&intent.receipt) else {
                    tracing::warn!(
                        session = %self.ctx.session,
                        receipt = %intent.receipt,
                        "write intent carries an unparseable receipt id; it will replay but \
                         cannot be looked up"
                    );
                    continue;
                };
                let answer = match &intent.outcome {
                    // J3 round-1 N8: `pending_replay`, not `pending` — this one
                    // is owed to a replay that may not finish in this process.
                    None => ReceiptAnswer::PendingReplay,
                    Some(o) if o.tag == "failed" => ReceiptAnswer::Failed(o.summary.clone()),
                    Some(o) => ReceiptAnswer::AppliedAfterRestart(o.summary.clone()),
                };
                map.insert(id, (intent.agent.clone(), answer));
            }
        }
        let pending: Vec<WriteIntent> = intents
            .into_iter()
            .filter(|i| i.outcome.is_none())
            .collect();
        let backlog = pending.len();
        if backlog == 0 {
            return;
        }
        tracing::info!(
            session = %self.ctx.session,
            backlog,
            "write queue: replaying {backlog} durable write intent(s) left by a previous \
             process, one at a time, in admission order"
        );
        self.counters
            .replay_owed
            .store(backlog as u64, Ordering::Relaxed);
        let this = self.clone();
        let handle = tokio::spawn(async move {
            // J3 round-1 N1, step 1 — the liveness gate. Nothing below may
            // consume an intent until the embedder has answered once, because
            // the failure arm cannot tell "this content is unembeddable" from
            // "there is no embedder right now" any better than the error type
            // lets it, and an outage spanning one attach would otherwise settle
            // the WHOLE backlog `failed` at one HYBRID_IO_TIMEOUT each. Costs
            // one embed of PROBE_TEXT (35 bytes, chosen because every embedder
            // accepts it) against PROBE_BUDGET.
            //
            // On failure the intents are left exactly as they were: durable,
            // unconsumed, `pending_replay`. What the operator sees is this one
            // warn line per attach — not a loop — plus `write_queue_replay_owed`
            // holding the backlog and every receipt answering `pending_replay`.
            // That is the bound on "retry forever without visibility": the retry
            // is one embed per session start, and the debt is on the stats
            // surface until it is paid.
            // J3-R2R-1 property 3 — the sequential decision rule that BOUNDS the
            // loop. A fresh attach starts with a cleared block reason so a drain
            // that never faults reports `null` (J3-R2R-8).
            this.counters.set_replay_blocked(ReplayBlockReason::None);
            let live =
                tokio::time::timeout(PROBE_BUDGET, this.ctx.embedder.embed(PROBE_TEXT)).await;
            if !matches!(live, Ok(Ok(_))) {
                this.counters
                    .set_replay_blocked(ReplayBlockReason::Embedder);
                let why: String = match &live {
                    Err(_) => format!("no answer within {PROBE_BUDGET:?}"),
                    Ok(Err(e)) => e.to_string(),
                    Ok(Ok(_)) => unreachable!("the guard above excluded success"),
                };
                tracing::warn!(
                    session = %this.ctx.session,
                    backlog,
                    error = %why,
                    "write queue: the embedder did not answer a liveness embed, so the durable \
                     intent replay was NOT started — all {backlog} intent(s) stay durable and \
                     unconsumed for the next serve of this session. Nothing was settled failed; \
                     nothing was written."
                );
                return;
            }
            let mut applied = 0usize;
            let mut failed = 0usize;
            // Consecutive-transient embedder-sickness evidence; threshold and
            // rationale at [`EMBEDDER_SICK_THRESHOLD`].
            let mut transient_streak: usize = 0;
            for intent in pending {
                if this.ctx.lease_lost.load(Ordering::Acquire) || this.lanes.lock().sealed {
                    break;
                }
                let Ok(receipt) = ReceiptId::from_str(&intent.receipt) else {
                    continue;
                };
                let job = Job {
                    receipt,
                    agent: intent.agent.clone(),
                    interaction: intent.interaction,
                    bytes: 0,
                    payload: JobPayload::from_intent_payload(intent.payload),
                };
                let stamp = ConsumeStamp {
                    tag: "applied_after_restart",
                    at: (this.clock)(),
                };
                let answer = match this.ctx.run(&job, Some(stamp)).await {
                    Ok(summary) => {
                        // Observed health: the embedder just worked. Resets the
                        // consecutive-transient streak.
                        transient_streak = 0;
                        applied += 1;
                        this.counters.replay_owed.fetch_sub(1, Ordering::Relaxed);
                        this.counters.replayed.fetch_add(1, Ordering::Relaxed);
                        // J4 proof obligation 5: a replayed intent's applied
                        // lifecycle + metric-2 facts ride the ledger — the
                        // re-derivation-savings signal metric 2 previously lost.
                        if let Some(ledger) = &this.ctx.ledger {
                            ledger.append(&crate::ledger::completion_line(
                                &job.agent.to_string(),
                                &job.receipt.to_string(),
                                "applied_after_restart",
                                Some(json!({
                                    "created_count": summary.created_count,
                                    "matched_count": summary.matched_count,
                                })),
                            ));
                        }
                        ReceiptAnswer::AppliedAfterRestart(summary.summary)
                    }
                    // Transient — the status-classifier draw is "embedder
                    // momentarily unwilling / unreachable": evidence of sickness,
                    // NOT a statement about this intent. Leave THIS intent durable
                    // (unconsumed) and accumulate; concluding sickness at the
                    // threshold keeps the rest of the backlog durable too.
                    Err(LamboError::EmbedUnavailable(e)) => {
                        transient_streak += 1;
                        if transient_streak >= EMBEDDER_SICK_THRESHOLD {
                            this.counters
                                .set_replay_blocked(ReplayBlockReason::Embedder);
                            tracing::warn!(
                                session = %this.ctx.session,
                                receipt = %intent.receipt,
                                error = %e,
                                applied,
                                failed,
                                remaining = backlog - applied - failed,
                                "write queue: the embedder answered transiently for \
                                 {EMBEDDER_SICK_THRESHOLD} intents in a row (the sequential \
                                 decision rule's threshold); treating it as SICK. THIS intent and \
                                 the rest of the backlog stay DURABLE and unconsumed for the next \
                                 serve"
                            );
                            break;
                        }
                        tracing::warn!(
                            session = %this.ctx.session,
                            receipt = %intent.receipt,
                            error = %e,
                            streak = transient_streak,
                            "write queue: a durable intent's replay got a transient embedder \
                             answer; it stays DURABLE and unconsumed while the sequential \
                             decision rule keeps sampling for the embedder's health"
                        );
                        continue;
                    }
                    // Content — absorbing: permanent FOR THIS INPUT. Consume it
                    // as `failed` immediately, exactly as the in-session worker
                    // would. It never counts toward embedder-sickness evidence,
                    // and the refusal proves the embedder just answered (resets
                    // the streak — observed aliveness).
                    Err(LamboError::Embed(e)) => {
                        transient_streak = 0;
                        failed += 1;
                        this.counters.replay_owed.fetch_sub(1, Ordering::Relaxed);
                        // JE2E-12, same split as the in-session worker's failure
                        // arm: the class for the model, the embedder's own words
                        // for the operator. The "replay after restart was
                        // refused; nothing was written" framing is lambo's own
                        // and is the useful half — it says *when* and *whether*
                        // — so it survives on both.
                        let why = format!(
                            "replay after restart was refused ({}); nothing was written",
                            crate::surface::error::err_class(&LamboError::Embed(e.clone()))
                        );
                        let detail =
                            format!("replay after restart was refused ({e}); nothing was written");
                        // J4 proof obligation 5: a replayed intent settled
                        // `failed` is a lifecycle fact, visible on the ledger.
                        if let Some(ledger) = &this.ctx.ledger {
                            ledger.append(&crate::ledger::completion_line(
                                &job.agent.to_string(),
                                &job.receipt.to_string(),
                                "failed",
                                Some(json!({ "error": &detail, "replay": true })),
                            ));
                        }
                        // A failure has no commit to ride — consume on its own
                        // (see the worker's failure arm for the argument). The
                        // row holds the model-safe form for the same reason it
                        // does there: a restart answers `failed` out of it.
                        this.ctx.graph.write().consume_write_intent(
                            intent.receipt.clone(),
                            WriteIntentOutcome {
                                tag: "failed".into(),
                                summary: why.clone(),
                                consumed_at: (this.clock)(),
                            },
                        );
                        tracing::warn!(
                            session = %this.ctx.session,
                            receipt = %intent.receipt,
                            error = %e,
                            "write queue: a durable intent's replay was refused; its record says so"
                        );
                        ReceiptAnswer::Failed(why)
                    }
                    // Anything else (store/lease/config) — session-wide and
                    // non-embedder; the next job would hit it too. Stop, leave
                    // everything durable, and name the block reason.
                    Err(e) => {
                        this.counters.set_replay_blocked(ReplayBlockReason::Other);
                        tracing::warn!(
                            session = %this.ctx.session,
                            receipt = %intent.receipt,
                            error = %e,
                            applied,
                            remaining = backlog - applied - failed,
                            "write queue: a durable intent's replay failed for a non-embedder \
                             session-wide reason (store/lease/config); it stays DURABLE and \
                             unconsumed, and the replay stops here so the rest of the backlog \
                             survives too"
                        );
                        break;
                    }
                };
                this.restart
                    .lock()
                    .insert(receipt, (intent.agent.clone(), answer));
                this.settled.notify_waiters();
                // The throttle: yield between jobs so a deep backlog cannot
                // monopolize the runtime between two of the fresh session's
                // polls.
                tokio::task::yield_now().await;
            }
            tracing::info!(
                session = %this.ctx.session,
                applied,
                failed,
                owed = this.counters.replay_owed.load(Ordering::Relaxed),
                "write queue: durable intent replay finished"
            );
        });
        *self.replay.lock() = Some(handle);
    }

    /// Stop the replay task (J3): abort **and join**, because an aborted task
    /// can still finish a synchronous stretch — and append to the log — until
    /// the join returns (the R3-1 lesson). `close()` calls this before its
    /// final drain so no replay write can land after the drain's last word.
    /// Unconsumed intents stay durable for the next serve.
    pub(crate) async fn stop_replay(&self) {
        let handle = self.replay.lock().take();
        if let Some(handle) = handle {
            handle.abort();
            let _ = handle.await;
        }
    }

    /// Abort the replay task without joining — the `Drop` path, which cannot
    /// await (same shape as [`WritePipeline::abort_all_sync`]).
    pub(crate) fn abort_replay_sync(&self) {
        if let Some(handle) = self.replay.lock().take() {
            handle.abort();
        }
    }
}

/// A random per-process epoch, so a receipt from a previous process is
/// recognisable as foreign rather than mistaken for one of ours.
fn rand_epoch() -> u64 {
    let u = uuid::Uuid::new_v4().as_u128();
    (u as u64) ^ ((u >> 64) as u64)
}

#[cfg(test)]
mod tests;

#[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]
mod pipeline_tests;
