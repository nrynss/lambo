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
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;

use parking_lot::Mutex as PlMutex;
use tokio::sync::{watch, Notify, Semaphore};
use tokio::task::JoinHandle;

use crate::types::AgentId;

// One module per responsibility; the struct they all extend,
// `WritePipeline`, stays here so its fields remain private to `writeq`.
mod admission;
mod calibration;
mod counters;
mod drain;
mod execution;
mod receipts;
mod replay;

pub use admission::{
    DropReason, Submitted, WRITE_QUEUE_LANE_MAX, WRITE_QUEUE_MAX, WRITE_QUEUE_MAX_BYTES,
};
pub use calibration::{
    Calibration, CalibrationSource, MEASURED_LOCAL_EMBEDDER_RPS, OBSERVED_EWMA_WEIGHT,
    OBSERVED_MIN_SAMPLES, PROBE_BUDGET, PROBE_CLAMP_RPS, PROBE_CONCURRENCY, PROBE_EMBEDS,
    PROBE_TEXT, PROBE_TEXT_BYTES, PROBE_WARMUP_EMBEDS,
};
pub use counters::{ReplayBlockReason, WriteQueueCounters};
pub use drain::WRITE_QUEUE_DRAIN_BUDGET;
pub(crate) use execution::{mirror_concepts, WriteCtx};
pub use receipts::{
    AppliedSummary, ReceiptAnswer, ReceiptId, WriteKind, MAX_CONCURRENT_RECEIPT_WAITS,
    MAX_PIGGYBACK_RECEIPTS, MAX_RECEIPT_IDS, MAX_RETAINED_RECEIPTS, MEASURED_WORST_FLUSH_LAG_SECS,
    RECEIPT_RETENTION, RECEIPT_WAIT_MAX,
};
pub use replay::EMBEDDER_SICK_THRESHOLD;

// Crate-internal names the sibling modules reach through `super::`.
use admission::{Job, JobPayload, Lanes};
use calibration::{probe_embedder, ObservedRate};
use execution::ConsumeStamp;
use receipts::{model_safe_failure, settle_one, Entry, Receipts};

// ---------------------------------------------------------------------------
// Constants — every one of them derived, at the constant, from something else
// in the tree or from a measurement in the phase doc.
// ---------------------------------------------------------------------------

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
