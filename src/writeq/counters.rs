//! Write-queue accounting: [`WriteQueueCounters`] and the replay block
//! reason.
//!
//! The queue keeps its **own** counters and never touches
//! [`crate::ledger::LedgerCounters`]. One expression,
//! [`WriteQueueCounters::outstanding`], serves both the live gauge and the
//! shutdown count. Normal execution and replay count separately: this
//! session's own jobs move `accepted`, `applied`, `failed` and `deferred`;
//! durable intents from a previous process move only `replayed` and
//! `replay_owed`, so `outstanding` stays exact. Who moves which counter:
//!
//! | counter | moved by |
//! |---|---|
//! | `accepted`, `dropped_*` | admission (`admission.rs`) |
//! | `applied`, `failed`, `abandoned` | the lane worker (`execution.rs`) |
//! | `deferred` | the close drain (`drain.rs`) |
//! | `replayed`, `replay_owed`, `replay_blocked` | the intent replay (`replay.rs`) |

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::time::Duration;

/// Applied writes the apply-latency window keeps (#11).
///
/// A recent window rather than a lifetime histogram, so the percentiles track
/// an embedder that slows down; 256 is several minutes of a busy rig's writes
/// and a few KiB of memory.
pub const APPLY_LATENCY_WINDOW: usize = 256;

/// Apply latency of this pipeline's recent **applied** writes, from admission
/// to settle: queueing behind earlier writes in the lane plus the write's own
/// service time, which is what a caller waiting on a receipt experiences (#11).
///
/// Failures are not recorded, for the reason they are not sampled into the
/// observed rate (J3-R2-2): a fast failure is not evidence about how long a
/// write takes to land.
#[derive(Debug, Default)]
pub(super) struct ApplyLatency {
    samples: VecDeque<Duration>,
}

impl ApplyLatency {
    pub(super) fn record(&mut self, latency: Duration) {
        if self.samples.len() == APPLY_LATENCY_WINDOW {
            self.samples.pop_front();
        }
        self.samples.push_back(latency);
    }

    /// Nearest-rank percentiles over the window, or `None` before the first
    /// applied write.
    pub(super) fn summary(&self) -> Option<ApplyLatencySummary> {
        if self.samples.is_empty() {
            return None;
        }
        let mut sorted: Vec<Duration> = self.samples.iter().copied().collect();
        sorted.sort_unstable();
        let rank = |p: usize| sorted[(sorted.len() * p).div_ceil(100).max(1) - 1];
        Some(ApplyLatencySummary {
            samples: sorted.len(),
            p50: rank(50),
            p90: rank(90),
            max: sorted[sorted.len() - 1],
        })
    }
}

/// Percentiles of [`APPLY_LATENCY_WINDOW`] recent apply latencies, for
/// `lambo_stats` (#11): enough to check a deployment's derive latency against
/// [`RECEIPT_WAIT_MAX`](super::RECEIPT_WAIT_MAX) without joining the ledger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApplyLatencySummary {
    /// Applied writes in the window.
    pub samples: usize,
    pub p50: Duration,
    pub p90: Duration,
    pub max: Duration,
}

/// Why the durable-intent replay last stopped without draining (J3-R2R-8).
///
/// `replay_owed` is a *level* — an operator cannot tell "draining" from
/// "wedged" from it alone. This names the class of the error that ended the
/// last replay, so a single poll answers the question two polls-and-a-memory
/// used to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReplayBlockReason {
    /// The replay drained, was never owed, or has not been attempted this attach.
    #[default]
    None,
    /// The embedder answered transiently (or not at all) past
    /// [`EMBEDDER_SICK_THRESHOLD`](super::EMBEDDER_SICK_THRESHOLD) — it is
    /// presumed sick. This is "wedged", not "draining".
    Embedder,
    /// A non-embedder, session-wide fault (a store error, a lost lease, a
    /// config error) ended the loop.
    Other,
    /// An image derive intent (#22) reached the head of the backlog in a
    /// process that cannot apply one: its match strategy is not `hybrid`, or
    /// its store has no vector search. The replay stops there rather than
    /// skip it, to keep the lane order (a later intent could otherwise
    /// create the image's canonical key as text first); restarting with the
    /// hybrid strategy and a vector-search store drains it.
    ImageConfig,
}

/// Queue accounting. See the module docs for why the shape mirrors
/// [`crate::ledger::LedgerCounters`] rather than reusing it.
#[derive(Debug, Default)]
pub struct WriteQueueCounters {
    /// Jobs the queue took custody of — applied, failed, or still outstanding.
    /// A refused admission never lands here, which is what makes
    /// [`WriteQueueCounters::outstanding`] correct.
    pub(super) accepted: AtomicU64,
    pub(super) applied: AtomicU64,
    pub(super) failed: AtomicU64,
    /// A **label on a subset of `failed`**, never a fourth term in the
    /// subtraction: jobs settled `failed` because the lease was lost before they
    /// ran. (This also named "`close()` ran out of quiesce budget"; since
    /// durable intents such a job is settled `intent_durable` and counted in
    /// `deferred`, not here.)
    pub(super) abandoned: AtomicU64,
    pub(super) dropped_queue_full: AtomicU64,
    pub(super) dropped_queue_bytes: AtomicU64,
    /// Refusals because the session was closing or fenced — **a third drop
    /// class, not a subtraction** (J3-R1-8). It rides its own counter and is
    /// summed into [`WriteQueueCounters::dropped`], so no count vanishes and
    /// the gauge's exclusivity argument is untouched: like the other two, a
    /// refusal never enters `accepted`. Split out because
    /// `write_queue_dropped` is the key the operator reads for "a burst
    /// degraded", and "the embedder is the bottleneck" and "the session is
    /// shutting down and refused a tail" want opposite responses.
    pub(super) dropped_closed: AtomicU64,
    /// Acked writes a clean `close()` did **not** apply and did not lose:
    /// their durable intents survive the close and the next serve of the
    /// session applies them (J3 durable intents). A fourth settle class beside
    /// `applied`/`failed` — never a subset of either — because "your write
    /// will happen, later, in another process" is neither a success nor a
    /// failure and must not be counted as one.
    pub(super) deferred: AtomicU64,
    /// Durable intents from a **previous** process that this session's replay
    /// applied at attach. Not summed into `applied` (those count this
    /// session's own accepted jobs; a replayed intent was never accepted
    /// here), so `outstanding` stays exact.
    pub(super) replayed: AtomicU64,
    /// Durable intents from a previous process that this session found owed and
    /// has **not** paid — the live replay debt (J3 round-1 N1).
    ///
    /// Set to the backlog when the replay task is spawned and decremented as
    /// each intent is applied or settled `failed`. It is the operator-visible
    /// answer to "the embedder was down at attach — what happened to my
    /// writes?": a non-zero value with `write_queue_replayed` not advancing says
    /// the debt is intact, which is the honest state and the one the N1 fix
    /// preserves instead of destroying. Unlike the other counters this one can
    /// go **down as well as up**, because it is a depth, not a total; it is
    /// therefore not a term in `outstanding` (those intents were never accepted
    /// by this process).
    pub(super) replay_owed: AtomicU64,
    /// The reason the last replay stopped without draining (J3-R2R-8), or
    /// [`ReplayBlockReason::None`] when it drained / was never owed. Renders as
    /// the `write_queue_replay_blocked` stat: `null` = draining or idle,
    /// `"embedder"` = sick/wedged, `"other"` = store/lease/config,
    /// `"image_config"` = an image intent this process's configuration cannot
    /// apply (#22).
    pub(super) replay_blocked: AtomicU8,
}

impl WriteQueueCounters {
    pub fn accepted(&self) -> u64 {
        self.accepted.load(Ordering::Relaxed)
    }
    pub fn applied(&self) -> u64 {
        self.applied.load(Ordering::Relaxed)
    }
    pub fn failed(&self) -> u64 {
        self.failed.load(Ordering::Relaxed)
    }
    pub fn abandoned(&self) -> u64 {
        self.abandoned.load(Ordering::Relaxed)
    }
    pub fn dropped_queue_full(&self) -> u64 {
        self.dropped_queue_full.load(Ordering::Relaxed)
    }
    pub fn dropped_queue_bytes(&self) -> u64 {
        self.dropped_queue_bytes.load(Ordering::Relaxed)
    }
    pub fn dropped_closed(&self) -> u64 {
        self.dropped_closed.load(Ordering::Relaxed)
    }
    pub fn deferred(&self) -> u64 {
        self.deferred.load(Ordering::Relaxed)
    }
    pub fn replayed(&self) -> u64 {
        self.replayed.load(Ordering::Relaxed)
    }
    pub fn replay_owed(&self) -> u64 {
        self.replay_owed.load(Ordering::Relaxed)
    }
    pub fn replay_blocked(&self) -> ReplayBlockReason {
        match self.replay_blocked.load(Ordering::Relaxed) {
            0 => ReplayBlockReason::None,
            1 => ReplayBlockReason::Embedder,
            3 => ReplayBlockReason::ImageConfig,
            _ => ReplayBlockReason::Other,
        }
    }
    pub(crate) fn set_replay_blocked(&self, reason: ReplayBlockReason) {
        let disc = match reason {
            ReplayBlockReason::None => 0,
            ReplayBlockReason::Embedder => 1,
            ReplayBlockReason::Other => 2,
            ReplayBlockReason::ImageConfig => 3,
        };
        self.replay_blocked.store(disc, Ordering::Relaxed);
    }

    /// Every refused admission, whatever the bound that refused it. The three
    /// classes are disjoint and this is their sum, so splitting
    /// `dropped_closed` out of `dropped_queue_full` moved no count and lost
    /// none (J3-R1-8).
    pub fn dropped(&self) -> u64 {
        self.dropped_queue_full() + self.dropped_queue_bytes() + self.dropped_closed()
    }

    /// Jobs accepted and not yet settled.
    ///
    /// `accepted − applied − failed − deferred`, and correct **only because**
    /// a refused admission never enters `accepted` — the same exclusivity
    /// argument `ledger_queued_lines` rests on, re-derived here against these
    /// counter sites rather than inherited. `abandoned` is deliberately
    /// absent: an abandoned job is already counted in `failed`, and
    /// subtracting it twice is the drift this one shared expression exists to
    /// prevent. `deferred` IS a term: a close-deferred job settled
    /// `intent_durable` is out of this process's custody without being applied
    /// or failed. `replayed` is not: a replayed intent never entered
    /// `accepted`.
    pub fn outstanding(&self) -> u64 {
        self.accepted()
            .saturating_sub(self.applied())
            .saturating_sub(self.failed())
            .saturating_sub(self.deferred())
    }
}
