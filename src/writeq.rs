//! Asynchronous write pipeline and write receipts (J3).
//!
//! # The rule
//!
//! A write may be acknowledged **before** it is applied only when its result
//! does not gate the caller's next action. `derive` and `record_action`
//! qualify (most of a warm `derive` is the embed, and durability was already
//! asynchronous); **`reserve` never does**, because its result *is* the next
//! action.
//!
//! # Shape and ownership
//!
//! The call path (`Memory::derive_async_as`, `Memory::record_action_async_as`)
//! validates, opens the interaction, and admits the job; this module embeds,
//! canonicalizes and inserts in the background through the ordinary
//! [`crate::graph::hybrid::derive`] / [`crate::graph::action`] path, and stores
//! the outcome against a [`ReceiptId`]. **Memory owns execution and receipt
//! state; MCP owns delivery** (the piggyback and `lambo_stats(receipt=…)`).
//! The synchronous `Memory::derive` / `record_action` surface does not go
//! through here: the async path is additive.
//!
//! # Invariants
//!
//! * **Ordering, scoped to one agent's *sequential* writes.** The interaction
//!   is opened on the call path, so submission order is `Temporal`-chain
//!   order; each agent has one FIFO lane with one consumer, so a lane applies
//!   in submission order and created/matched attribution follows it. Two
//!   writes one agent has in flight *at once* are unordered (the chain and
//!   lane positions are pinned in two critical sections; J3-R1-10).
//! * **Durability by construction, not by estimate.** Every admitted job is
//!   recorded as a durable intent (`PutWriteIntent`) in the same critical
//!   section that queues it, so at a clean close acked ⇒ (applied ∨ durable
//!   intent); what the close drain cannot finish is deferred, and the next
//!   serve replays it (`replay`).
//! * **Admission guards memory and fairness only**, with static bounds
//!   ([`WRITE_QUEUE_MAX`], [`WRITE_QUEUE_MAX_BYTES`], [`WRITE_QUEUE_LANE_MAX`]).
//!   The probe and the observed rate are telemetry (`calibration`).
//! * **Own accounting.** The queue never touches
//!   [`crate::ledger::LedgerCounters`]; a refusal never enters `accepted`, and
//!   [`WriteQueueCounters::outstanding`] is the one expression for both the
//!   live gauge and the shutdown count.
//! * **Locks.** Graph write then lanes at admission (the only nesting of
//!   those two); graph read then index write when mirroring; no graph lock
//!   across `.await`; workers never take `Memory`'s writers gate.
//!
//! # Modules
//!
//! | module | holds |
//! |---|---|
//! | `receipts` | `ReceiptId`, `ReceiptAnswer`, the receipt store, lookup/wait/piggyback |
//! | `calibration` | the startup probe, the observed rate, `Calibration` (telemetry) |
//! | `counters` | `WriteQueueCounters`, `ReplayBlockReason` |
//! | `admission` | the bounds, `DropReason`, `Job`, the per-agent `Lanes`, `admit` |
//! | `execution` | `WriteCtx::run`, index mirroring, the lane worker and its settle |
//! | `drain` | `quiesce` / `abort_workers` for `close()`, `abort_all_sync` for `Drop` |
//! | `replay` | the durable-intent replay spawned at attach |
//!
//! The review chronology behind these rules (J3 rounds 1-3, the estimator
//! redesign) is in `dev-diary/notes/refactor-27-writeq-memory.md` and
//! `dev-diary/lambo-for-mooshik/J3-durability-redesign.md`.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;

use parking_lot::Mutex as PlMutex;
use tokio::sync::{Notify, Semaphore};
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
pub(crate) use execution::{mirror_concepts, ConsumeStamp, WriteCtx};
pub use receipts::{
    AppliedSummary, ReceiptAnswer, ReceiptId, WriteKind, MAX_CONCURRENT_RECEIPT_WAITS,
    MAX_PIGGYBACK_RECEIPTS, MAX_RECEIPT_IDS, MAX_RETAINED_RECEIPTS, MEASURED_WORST_FLUSH_LAG_SECS,
    RECEIPT_RETENTION, RECEIPT_WAIT_MAX,
};
pub use replay::EMBEDDER_SICK_THRESHOLD;

// Crate-internal names the sibling modules reach through `super::`.
use admission::{Job, JobPayload, Lanes};
use calibration::{EmbedderProbe, ObservedRate};
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
    /// The startup calibration probe of this pipeline's embedder (telemetry).
    /// Its own type so #32 PR 3 can share one probe per embedder across every
    /// pipeline in the process (`EmbedderCalibration`, design decision 14).
    probe: EmbedderProbe,
    /// Service time observed on real writes, which **replaces** the probe's
    /// serial figure once [`OBSERVED_MIN_SAMPLES`] have been seen (J3-R1-2).
    observed: Arc<PlMutex<ObservedRate>>,
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
        let probe = EmbedderProbe::spawn(ctx.embedder.clone(), ctx.session.clone());
        Self {
            ctx: Arc::new(ctx),
            lanes: Arc::new(PlMutex::new(Lanes::default())),
            receipts: Arc::new(PlMutex::new(Receipts::default())),
            counters: Arc::new(WriteQueueCounters::default()),
            settled: Arc::new(Notify::new()),
            wait_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_RECEIPT_WAITS)),
            observed: Arc::new(PlMutex::new(ObservedRate::default())),
            probe,
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
