//! Asynchronous write pipeline and write receipts (J3).
//!
//! # The rule
//!
//! A write may be acknowledged **before** it has been applied only when its
//! result does not gate the caller's next action. `derive` and `record_action`
//! qualify: a warm `derive` is 27 ms of which 22 to 25 ms is the embedding call
//! (`dev-diary/lambo-for-mooshik/J-multi-client.md` §Measurements), durability
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
//! The interaction is opened **synchronously, on the call path**, before the
//! job is queued. [`crate::Memory::begin_interaction_as`] takes the graph write
//! lock only briefly and never awaits, so this is cheap — and it makes
//! submission order *be* `Temporal`-chain order by construction. That is
//! strictly stronger than ordering the drain: the chain no longer depends on
//! drain order at all, so an out-of-order drain cannot corrupt it. Since J1 the
//! chain is session-wide (see `Memory::begin_interaction_as`), so "one agent's
//! writes apply in submission order" is read off the chain by filtering it on
//! `agent_id`.
//!
//! Per-agent FIFO is **still** enforced in the drain, for a second reason:
//! insertion order decides which of two identical concepts is `created` and
//! which is `matched`, and that distinction is reported in the receipt. Each
//! agent gets its own lane with a single consumer, so a lane drains in
//! submission order; lanes run concurrently, because interleaving *across*
//! agents is fine.
//!
//! # Backpressure
//!
//! Asynchrony adds no capacity. The queue bound is therefore derived from a
//! ceiling **measured on the deployment's own embedder** ([`Calibration`]),
//! never from a constant: a hosted or GPU embedder may be slower per call while
//! parallelising far better, at which point this rig's "batching does not pay"
//! result inverts. The drop policy is fixed regardless — bound, drop, log once,
//! count in `lambo_stats`.
//!
//! # Accounting (the `ledger_queued_lines` lesson, re-derived)
//!
//! This module keeps its **own** counters and never touches
//! [`crate::ledger::LedgerCounters`], so the ledger's
//! `accepted − written − write_failed` keeps its exclusivity argument intact:
//! no new class enters the ledger's `accepted`. The queue mirrors that
//! discipline deliberately — a queue-full or byte-cap reject never enters
//! [`WriteQueueCounters::accepted`], so
//! `outstanding = accepted − applied − failed` is one expression serving both
//! the live gauge and the shutdown count, and cannot drift between them.
//! `abandoned` is a **label on a subset of `failed`**, not a fourth term: an
//! abandoned job is settled `failed`, and counting it twice is exactly the
//! mistake `adve-review-mooshik-I-round3.md`'s flip D maps.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::{Mutex as PlMutex, RwLock};
use tokio::sync::{watch, Notify, Semaphore};
use tokio::task::JoinHandle;

use crate::cli::caps::{MAX_CONCEPTS_PER_DERIVE, MAX_CONTENT_BYTES};
use crate::embed::Embedder;
use crate::graph::action::{record_action as graph_record_action, Action};
use crate::graph::derive::{derive as graph_derive, ParentOf};
use crate::graph::hybrid;
use crate::graph::index::InvertedIndex;
use crate::graph::Graph;
use crate::store::GraphStore;
use crate::types::{
    AgentId, ConceptType, EmbeddingContract, LamboError, MatchStrategy, Node, NodeId, SessionId,
};

// ---------------------------------------------------------------------------
// Constants — every one of them derived, at the constant, from something else
// in the tree or from a measurement in the phase doc.
// ---------------------------------------------------------------------------

/// The operator-facing bound on "acked but not yet applied".
///
/// Two things are sized from this single number, and they **must** be the same
/// number:
///
/// * **Admission.** The queue admits a job only while the projected drain time
///   of everything already queued — `outstanding / measured_items_per_sec` —
///   stays inside this budget. That is what makes the bound a function of the
///   measured ceiling rather than a constant.
/// * **Quiesce.** [`WritePipeline::quiesce`] waits this long for the queue to
///   drain during `close()`.
///
/// A queue that admitted more than shutdown is willing to wait for would
/// guarantee abandoned writes at every clean close, so the two are one
/// constant rather than two that can drift.
///
/// Two seconds, and the ceiling on that choice is `close()`'s own budget:
/// `lambo serve` wraps `Memory::close` in
/// [`crate::mcp::serve::CLOSE_FLUSH_GRACE`] (8 s), out of which
/// `SHUTDOWN_GRACE + CLOSE_GRACE ≤ SHUTDOWN_BUDGET` is sized. The quiesce runs
/// **in series before** the existing final flush, so it is carved out of that
/// 8 s rather than added on top — the same reasoning `LEASE_RELEASE_GRACE`
/// used. A quarter of the window is the largest slice that leaves the flush,
/// which is the step that actually delivers durability, the majority of it.
pub const WRITE_QUEUE_DRAIN_BUDGET: Duration = Duration::from_secs(2);

/// Build-time invariant: the quiesce cannot become the reason a `close()` blows
/// the deadline `serve` gives it.
const _: () = assert!(
    WRITE_QUEUE_DRAIN_BUDGET.as_secs() * 4 <= crate::mcp::serve::CLOSE_FLUSH_GRACE.as_secs(),
    "WRITE_QUEUE_DRAIN_BUDGET must stay at or under a quarter of CLOSE_FLUSH_GRACE — the write \
     queue quiesce runs in series BEFORE the final flush, so it is carved out of close()'s \
     budget, not added to it",
);

/// Fallback bound when the calibration probe could not measure anything.
///
/// Sized at [`PROBE_CONCURRENCY`] and *defined* from it: the rig's own
/// parallelism measurement (4 recalls, 380 ms sequential against 64 ms
/// concurrent — §Measurements) is a 4-wide figure, so a floor below that would
/// drop work the deployment has been demonstrated to absorb. It is a floor, not
/// a guess at capacity: a session running on it says so in `lambo_stats`
/// (`write_queue_measured: false`).
pub const WRITE_QUEUE_MIN: usize = PROBE_CONCURRENCY;

/// Where a probe result stops being credible, in items/second.
///
/// This is **not** a capacity choice; it is the point past which the probe is
/// measuring something that is not an embedder. The rig's 4-wide concurrent
/// figure is 4 / 64 ms ≈ 62 items/s, so a deployment parallelising twice as
/// well as this rig lands near 128 — and anything reporting more is a stub, a
/// cache, or [`crate::FixtureEmbedder`], which returns without doing work at
/// all. That last case is not hypothetical: it is what every test in this tree
/// runs on, and it is the reason the clamp exists rather than a hypothetical
/// GPU.
pub const PROBE_MAX_CREDIBLE_RPS: u64 = 128;

/// Upper clamp on the measured bound — [`PROBE_MAX_CREDIBLE_RPS`] sustained for
/// one [`WRITE_QUEUE_DRAIN_BUDGET`]. Defined from both so it cannot drift from
/// either.
pub const WRITE_QUEUE_MAX: usize =
    (PROBE_MAX_CREDIBLE_RPS * WRITE_QUEUE_DRAIN_BUDGET.as_secs()) as usize;

/// Second admission condition: total queued payload bytes.
///
/// The count bound governs realistic traffic; this one governs the adversarial
/// shape, because a *count* is the wrong unit for memory. At the door's own
/// caps a single maximal `derive` retains
/// `MAX_CONCEPTS_PER_DERIVE × MAX_CONTENT_BYTES` = 64 × 16 KiB = 1 MiB of
/// concept text plus up to `MAX_HYBRID_PARENT_PAIRS × 2` = 512 more maximal
/// strings, i.e. 9 MiB — so a count bound of 256 would authorise gigabytes.
/// 16 MiB is `MAX_CONTENT_BYTES × 1024`: a thousand maximal strings, which
/// admits at least one maximal job whole and refuses a second.
pub const WRITE_QUEUE_MAX_BYTES: usize = MAX_CONTENT_BYTES * 1024;

/// Width of the calibration probe.
///
/// Four, because throughput rather than latency is what has to be measured
/// (see [`Calibration`]) and the phase doc's parallelism figure is a 4-wide
/// one. The probe re-measures it per deployment; the 4 fixes only how wide the
/// measurement is taken.
pub const PROBE_CONCURRENCY: usize = 4;

/// Bound on the calibration probe.
///
/// The probe is *spawned*, not awaited, at session build, so this is not
/// startup latency in any real deployment. It is nonetheless the worst case an
/// admission can wait, because admission blocks on the probe's result rather
/// than falling back to a constant bound — so it is bounded, and bounded
/// generously: a cold llama.cpp first token can take seconds, and treating that
/// as "unmeasurable" would put a warm deployment on the floor bound for its
/// whole life.
pub const PROBE_BUDGET: Duration = Duration::from_secs(5);

/// Text the probe embeds. Short, fixed, and content-free: it is measuring the
/// deployment's embedder, not its own input.
pub const PROBE_TEXT: &str = "lambo write queue calibration probe";

/// How long a settled receipt's outcome is held.
///
/// Above the **227 s** worst `flush_lag` observed on the rig (§Measurements),
/// because that is the window in which a write is applied in RAM but not yet
/// durable — and a receipt that expired inside it would leave the widened crash
/// window unauditable from the surface that exists to describe it.
pub const RECEIPT_RETENTION: Duration = Duration::from_secs(300);

/// Build-time invariant: a receipt must outlive its own job under every bound
/// in the tree, or "expired" would be reachable for a job still running.
const _: () = assert!(
    RECEIPT_RETENTION.as_secs()
        > hybrid::HYBRID_IO_TIMEOUT.as_secs() + WRITE_QUEUE_DRAIN_BUDGET.as_secs(),
    "RECEIPT_RETENTION must exceed the longest a single job can take (HYBRID_IO_TIMEOUT) plus \
     the longest it can wait to start (WRITE_QUEUE_DRAIN_BUDGET), or a receipt could expire \
     while its own write is still running",
);

/// Ids listed in one retained receipt, before it switches to a count.
///
/// [`MAX_CONCEPTS_PER_DERIVE`], and defined from it: that is what the door
/// admits per call, so listing more would be listing `parent_of` fan-out an
/// agent did not name as a concept.
pub const MAX_RECEIPT_IDS: usize = MAX_CONCEPTS_PER_DERIVE;

/// Retained receipts, oldest evicted first.
///
/// Derived from a stated ~2.5 MiB retention budget at the door's own worst
/// case: a receipt holds a summary plus at most
/// [`MAX_RECEIPT_IDS`] × 36-byte node ids ≈ 2.4 KiB, so 1024 of them is
/// ≈ 2.4 MiB. The time bound alone could not do this job —
/// [`RECEIPT_RETENTION`] against `serve`'s own sustained abuse bound
/// ([`crate::mcp::DEFAULT_RATE_LIMIT_RPS`], 50/s) is 15 000 receipts.
pub const MAX_RETAINED_RECEIPTS: usize = 1024;

/// Longest a caller may block waiting for its own write to apply.
///
/// Two drain budgets: a job admitted at the instant the queue was full is
/// projected to *start* at ≈ one budget, so the second budget is its own
/// service time plus slack. A wait that runs out answers `pending` — which is
/// one of the honest answers, not a failure.
pub const RECEIPT_WAIT_MAX: Duration = Duration::from_secs(4);

/// Build-time invariant: a wait shorter than the queue's own admission promise
/// would make the opt-in-synchrony surface useless by construction.
const _: () = assert!(
    RECEIPT_WAIT_MAX.as_secs() >= 2 * WRITE_QUEUE_DRAIN_BUDGET.as_secs(),
    "RECEIPT_WAIT_MAX must be at least twice WRITE_QUEUE_DRAIN_BUDGET — the queue admits work \
     it projects to drain within one budget, so a wait of one budget would time out on the very \
     jobs the wait exists for",
);

/// Concurrent receipt waits this process will hold at once.
///
/// **This is the J2-R2-7 / J2-R3-3 coupled residual's bound, and it is here
/// rather than in the proxy because this is the surface that creates the
/// population.** A waiting `lambo_receipt` call is a long-lived in-flight
/// request, so through a proxy it occupies an entry in the pump's `inflight`
/// list for the whole wait — and `answer_lost` writes one un-raced frame per
/// in-flight id to a client that may not be reading. So the waiting surface,
/// not the queue bound, is what lengthens that burst: a non-waiting ack returns
/// immediately and never enters the list.
///
/// Both ends are bounded: [`RECEIPT_WAIT_MAX`] caps how long one wait holds a
/// slot, and this caps how many exist. Half of
/// [`crate::mcp::proxy::INFLIGHT_DEPTH_WARN`] is left for ordinary traffic, so
/// receipt waits alone cannot be what trips the depth warning.
pub const MAX_CONCURRENT_RECEIPT_WAITS: usize = 16;

/// Build-time invariant tying the two ceilings together, so neither can be
/// moved without the other being considered.
const _: () = assert!(
    MAX_CONCURRENT_RECEIPT_WAITS * 2 <= crate::mcp::proxy::INFLIGHT_DEPTH_WARN,
    "MAX_CONCURRENT_RECEIPT_WAITS must leave half of INFLIGHT_DEPTH_WARN for ordinary traffic — \
     a waiting lambo_receipt holds a proxy inflight slot, and answer_lost writes one un-raced \
     frame per slot (J2-R2-7, J2-R3-3)",
);

/// Settled receipts piggybacked on one tool response.
///
/// The rest stay queued for the next response and the note says how many, so a
/// burst of finished writes cannot turn one tool result into a wall of text.
/// Eight is one screen of one-line notes.
pub const MAX_PIGGYBACK_RECEIPTS: usize = 8;

// ---------------------------------------------------------------------------
// Receipt ids
// ---------------------------------------------------------------------------

/// A write receipt id — **self-describing on purpose**.
///
/// It carries the issuing process's epoch, the issue time and a sequence
/// number, and those three fields are what let a lookup answer *expired*,
/// *restart-lost* and *never-issued* distinctly instead of collapsing all three
/// into "unknown" (§J3: "expired must not read as unknown, and restart-lost
/// must not either"). Nothing has to be remembered about an id after its
/// outcome is discarded, because the id itself says when it was issued and by
/// which process.
///
/// It is **not a capability**: possession of an id does not authorise reading
/// its outcome. The receipt store scopes every held receipt to the agent that
/// created it (J1), and a lookup by a different agent is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ReceiptId {
    /// Random per-process value. A foreign epoch is what makes `restart_lost`
    /// distinguishable from `expired` without keeping any history.
    epoch: u64,
    /// Issue time, Unix milliseconds.
    issued_ms: i64,
    /// Per-process monotonic counter, from 1.
    seq: u64,
}

impl ReceiptId {
    /// Wire prefix. Versioned so a later format change is detectable rather
    /// than silently mis-parsed as this one.
    const PREFIX: &'static str = "lwr1";

    fn new(epoch: u64, issued: DateTime<Utc>, seq: u64) -> Self {
        Self {
            epoch,
            issued_ms: issued.timestamp_millis(),
            seq,
        }
    }

    /// The issuing process's epoch.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Issue time in Unix milliseconds.
    pub fn issued_ms(&self) -> i64 {
        self.issued_ms
    }

    /// The per-process sequence number.
    pub fn seq(&self) -> u64 {
        self.seq
    }
}

impl fmt::Display for ReceiptId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}.{:016x}.{:x}.{:x}",
            Self::PREFIX,
            self.epoch,
            self.issued_ms,
            self.seq
        )
    }
}

impl FromStr for ReceiptId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = || format!("{s:?} is not a lambo receipt id");
        let mut parts = s.split('.');
        if parts.next() != Some(Self::PREFIX) {
            return Err(bad());
        }
        let epoch = parts
            .next()
            .and_then(|p| u64::from_str_radix(p, 16).ok())
            .ok_or_else(bad)?;
        let issued_ms = parts
            .next()
            .and_then(|p| i64::from_str_radix(p, 16).ok())
            .ok_or_else(bad)?;
        let seq = parts
            .next()
            .and_then(|p| u64::from_str_radix(p, 16).ok())
            .ok_or_else(bad)?;
        if parts.next().is_some() {
            return Err(bad());
        }
        Ok(Self {
            epoch,
            issued_ms,
            seq,
        })
    }
}

// ---------------------------------------------------------------------------
// Outcomes and answers
// ---------------------------------------------------------------------------

/// Which write a receipt belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteKind {
    Derive,
    RecordAction,
}

impl WriteKind {
    /// The tool name this kind was acked by.
    pub fn tool(self) -> &'static str {
        match self {
            WriteKind::Derive => "lambo_derive",
            WriteKind::RecordAction => "lambo_record_action",
        }
    }
}

/// What an applied write did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedSummary {
    pub kind: WriteKind,
    /// One-line human summary, the same sentence the synchronous path returns.
    pub summary: String,
    /// Node ids created, truncated to [`MAX_RECEIPT_IDS`].
    pub created: Vec<String>,
    /// Node ids matched to existing concepts, truncated to [`MAX_RECEIPT_IDS`].
    pub matched: Vec<String>,
    /// The **true** number created, which may exceed `created.len()` because
    /// the list is truncated at [`MAX_RECEIPT_IDS`]. Carried separately so a
    /// truncated list can never read as a short one, and because this is the
    /// number I1's metric 2 counts.
    pub created_count: usize,
    /// The true number matched, for the same reason.
    pub matched_count: usize,
    /// Hybrid semantic merges — `derive` only.
    ///
    /// **This and the two fields below are I1's DOGFOOD metric 2 fact set,
    /// relocated rather than dropped.** Before J3 they rode the ledger call
    /// line, which J3's ack can no longer carry: at ack time the write has not
    /// happened. Keeping them on the receipt means the metric-2 distinction
    /// (`semantic_merged`, a similarity merge that adds no `Derives` edge,
    /// against `matched`, a re-derive that does) is still recoverable — from
    /// the receipt instead of from the line.
    pub semantic_merged: Option<usize>,
    /// Duplicate natural-key writes that reinforced an existing edge —
    /// `derive` only.
    pub reinforced: Option<usize>,
    /// Edges newly inserted — `record_action` only. `None` for `derive`, which
    /// reports [`AppliedSummary::reinforced`] instead; a zero here would claim
    /// a derive wrote no edges, which is not what it means.
    pub edges: Option<usize>,
}

/// The answer to "what happened to this receipt?".
///
/// Seven variants, and **none of them is "unknown"**. The three the spec calls
/// out by name are [`ReceiptAnswer::Expired`], [`ReceiptAnswer::RestartLost`]
/// and [`ReceiptAnswer::NeverIssued`] — a receipt this process discarded, one
/// another process issued, and one nobody issued.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReceiptAnswer {
    /// Admitted, not yet decided. Also what a timed-out wait answers.
    Pending,
    /// Applied through the ordinary path.
    Applied(AppliedSummary),
    /// Attempted and rejected. The write did not happen.
    Failed(String),
    /// Never attempted: the queue was full when the ack was issued.
    Dropped(String),
    /// This process issued it and has since discarded its outcome — either the
    /// retention window passed or it was evicted as the oldest held receipt,
    /// which is the same statement about the same id.
    Expired,
    /// A **different** process issued it. Its outcome is unknowable from here:
    /// if the write had not yet been applied it died with that process, exactly
    /// as the write-behind tail does. Same statement as the proxy's
    /// `HUB_LOST_CODE` (-32002).
    RestartLost,
    /// Well-formed, from this process's epoch, but past the highest sequence
    /// number this process has ever issued. Nobody issued it.
    NeverIssued,
    /// Held, but by another agent. Receipts are per-agent scoped (J1).
    Forbidden,
}

impl ReceiptAnswer {
    /// Stable machine tag, for `structuredContent` and for tests.
    pub fn tag(&self) -> &'static str {
        match self {
            ReceiptAnswer::Pending => "pending",
            ReceiptAnswer::Applied(_) => "applied",
            ReceiptAnswer::Failed(_) => "failed",
            ReceiptAnswer::Dropped(_) => "dropped",
            ReceiptAnswer::Expired => "expired",
            ReceiptAnswer::RestartLost => "restart_lost",
            ReceiptAnswer::NeverIssued => "never_issued",
            ReceiptAnswer::Forbidden => "forbidden",
        }
    }

    /// `true` once the answer can no longer change.
    pub fn is_settled(&self) -> bool {
        !matches!(self, ReceiptAnswer::Pending)
    }

    /// One line, addressed to the model that will read it.
    pub fn describe(&self) -> String {
        match self {
            ReceiptAnswer::Pending => "pending — admitted, not yet applied; ask again".into(),
            ReceiptAnswer::Applied(s) => format!("applied — {}", s.summary),
            ReceiptAnswer::Failed(why) => format!("FAILED, nothing was written — {why}"),
            ReceiptAnswer::Dropped(why) => {
                format!("DROPPED before it was attempted, nothing was written — {why}")
            }
            ReceiptAnswer::Expired => format!(
                "expired — this session issued it but no longer holds its outcome \
                 (receipts are kept for {}s); recall to see whether the write is there",
                RECEIPT_RETENTION.as_secs()
            ),
            ReceiptAnswer::RestartLost => "restart-lost — a different serve process issued this \
                                           receipt, so the outcome is UNKNOWN from here: the \
                                           write may or may not have been applied before that \
                                           process ended. Recall before re-deriving."
                .into(),
            ReceiptAnswer::NeverIssued => {
                "never issued — this session has never handed out that receipt id".into()
            }
            ReceiptAnswer::Forbidden => {
                "held by another agent — receipts are scoped to the agent that created them".into()
            }
        }
    }
}

/// Why a job was refused admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropReason {
    /// The count bound, derived from the measured ceiling.
    QueueFull,
    /// The payload-byte bound.
    QueueBytes,
    /// The session is closing or has lost its lease.
    Closed,
}

impl DropReason {
    fn describe(self, bound: usize) -> String {
        match self {
            DropReason::QueueFull => format!(
                "the background write queue is full ({bound} outstanding, a bound measured on \
                 this deployment's embedder)"
            ),
            DropReason::QueueBytes => format!(
                "the background write queue is at its {} MiB payload cap",
                WRITE_QUEUE_MAX_BYTES / (1024 * 1024)
            ),
            DropReason::Closed => "the session is closing".into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Calibration
// ---------------------------------------------------------------------------

/// The measured ceiling and the bound derived from it.
///
/// **Throughput, not latency, is what is measured.** The figure that motivated
/// a per-deployment probe is a *parallelism* figure (4 recalls: 380 ms
/// sequential, 64 ms concurrent, 5.94x — §Measurements), and the case the spec
/// names — a hosted embedder that is slower per call but parallelises far
/// better — inverts per-call latency while raising throughput. A bound derived
/// from per-call latency would get that case exactly backwards.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Calibration {
    /// Measured items/second, or `None` when the probe could not measure.
    pub items_per_sec: Option<f64>,
    /// The queue's count bound.
    pub bound: usize,
}

impl Calibration {
    /// The floor, used when the probe failed. Says so, rather than presenting a
    /// number it did not measure.
    pub fn unmeasured() -> Self {
        Self {
            items_per_sec: None,
            bound: WRITE_QUEUE_MIN,
        }
    }

    /// Derive the bound from a measured wall time for [`PROBE_CONCURRENCY`]
    /// concurrent embeds.
    pub fn from_probe(wall: Duration) -> Self {
        let secs = wall.as_secs_f64();
        // A zero or absurd wall time is the FixtureEmbedder case; the clamp
        // below is what it is for, so it must not divide by zero first.
        let rate = if secs <= 0.0 {
            PROBE_MAX_CREDIBLE_RPS as f64
        } else {
            PROBE_CONCURRENCY as f64 / secs
        };
        let credible = rate.min(PROBE_MAX_CREDIBLE_RPS as f64);
        let projected = credible * WRITE_QUEUE_DRAIN_BUDGET.as_secs_f64();
        let bound = (projected.ceil() as usize).clamp(WRITE_QUEUE_MIN, WRITE_QUEUE_MAX);
        Self {
            items_per_sec: Some(rate),
            bound,
        }
    }

    /// `true` when the bound rests on a measurement of this deployment's own
    /// embedder.
    pub fn measured(&self) -> bool {
        self.items_per_sec.is_some()
    }
}

// ---------------------------------------------------------------------------
// Counters
// ---------------------------------------------------------------------------

/// Queue accounting. See the module docs for why the shape mirrors
/// [`crate::ledger::LedgerCounters`] rather than reusing it.
#[derive(Debug, Default)]
pub struct WriteQueueCounters {
    /// Jobs the queue took custody of — applied, failed, or still outstanding.
    /// A refused admission never lands here, which is what makes
    /// [`WriteQueueCounters::outstanding`] correct.
    accepted: AtomicU64,
    applied: AtomicU64,
    failed: AtomicU64,
    /// A **label on a subset of `failed`**, never a fourth term in the
    /// subtraction: jobs settled `failed` because `close()` ran out of quiesce
    /// budget or the lease was lost.
    abandoned: AtomicU64,
    dropped_queue_full: AtomicU64,
    dropped_queue_bytes: AtomicU64,
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

    /// Every refused admission, whatever the bound that refused it.
    pub fn dropped(&self) -> u64 {
        self.dropped_queue_full() + self.dropped_queue_bytes()
    }

    /// Jobs accepted and not yet settled.
    ///
    /// `accepted − applied − failed`, and correct **only because** a refused
    /// admission never enters `accepted` — the same exclusivity argument
    /// `ledger_queued_lines` rests on, re-derived here against these counter
    /// sites rather than inherited. `abandoned` is deliberately absent: an
    /// abandoned job is already counted in `failed`, and subtracting it twice
    /// is the drift this one shared expression exists to prevent.
    pub fn outstanding(&self) -> u64 {
        self.accepted()
            .saturating_sub(self.applied())
            .saturating_sub(self.failed())
    }
}

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

/// A queued write, owned: the call path's borrows are gone by the time this
/// exists, because the background path outlives them.
#[derive(Debug)]
struct Job {
    receipt: ReceiptId,
    agent: AgentId,
    interaction: NodeId,
    bytes: usize,
    payload: JobPayload,
}

#[derive(Debug)]
enum JobPayload {
    Derive {
        concepts: Vec<(String, ConceptType)>,
        pairs: Vec<(String, String)>,
    },
    Action {
        action: String,
        produces: Vec<String>,
        modifies: Vec<String>,
        depends_on: Vec<String>,
    },
}

impl JobPayload {
    fn kind(&self) -> WriteKind {
        match self {
            JobPayload::Derive { .. } => WriteKind::Derive,
            JobPayload::Action { .. } => WriteKind::RecordAction,
        }
    }

    /// Retained payload bytes, for the byte admission condition.
    fn bytes(&self) -> usize {
        match self {
            JobPayload::Derive { concepts, pairs } => {
                concepts.iter().map(|(c, _)| c.len()).sum::<usize>()
                    + pairs.iter().map(|(a, b)| a.len() + b.len()).sum::<usize>()
            }
            JobPayload::Action {
                action,
                produces,
                modifies,
                depends_on,
            } => {
                action.len()
                    + produces
                        .iter()
                        .chain(modifies)
                        .chain(depends_on)
                        .map(String::len)
                        .sum::<usize>()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The background execution context
// ---------------------------------------------------------------------------

/// Everything the background worker needs, and **nothing more** — in
/// particular not a handle on [`crate::Memory`].
///
/// That is the point of this struct rather than an `Arc<Memory>`: a worker
/// holding its owner would make the owner un-droppable by its own background
/// tasks, and `Memory`'s `Drop` is load-bearing (it aborts tasks and lets the
/// lease lapse). Every field here is already an `Arc` inside `Memory`, so this
/// is clones of shared state, not a second copy of anything.
pub(crate) struct WriteCtx {
    pub(crate) session: SessionId,
    pub(crate) graph: Arc<RwLock<Graph>>,
    pub(crate) index: Arc<RwLock<InvertedIndex>>,
    pub(crate) store: Arc<dyn GraphStore>,
    pub(crate) embedder: Arc<dyn Embedder>,
    pub(crate) embedding: EmbeddingContract,
    pub(crate) match_strategy: MatchStrategy,
    pub(crate) max_cooccurrence_per_derive: usize,
    pub(crate) semantic_match_threshold: f64,
    /// The daemon's wake `Notify`, so a background write pokes the daemon
    /// exactly as the synchronous path does.
    pub(crate) daemon_wake: Arc<Notify>,
    /// The single-writer fence, shared with the heartbeat and the flush task.
    pub(crate) lease_lost: Arc<AtomicBool>,
}

/// Mirror concept writes into the inverted index (the `src/graph/mod.rs`
/// contract). Ids that are not concepts are skipped.
///
/// Lock order is **graph read → index write**, matching the daemon's GC sync;
/// taking them the other way round would deadlock against it. Both guards are
/// held together on purpose so a concurrent recall — which reads (graph, index)
/// as a pair — sees an atomic publication.
///
/// A free function so the synchronous path in `Memory` and the background
/// worker here share one implementation: two copies of a lock-order rule is
/// two chances to get it wrong.
pub(crate) fn mirror_concepts(
    graph: &RwLock<Graph>,
    index: &RwLock<InvertedIndex>,
    ids: &[NodeId],
) {
    if ids.is_empty() {
        return;
    }
    let g = graph.read();
    let mut idx = index.write();
    for &id in ids {
        if let Some(Node::Concept(concept)) = g.node(id) {
            idx.add(concept);
        }
    }
}

/// The first [`MAX_RECEIPT_IDS`] ids as strings. The true count travels beside
/// the list in [`AppliedSummary::created_count`] / `matched_count`, so a
/// truncated list is never mistaken for a short one.
fn truncate_ids(ids: &[NodeId]) -> Vec<String> {
    ids.iter()
        .take(MAX_RECEIPT_IDS)
        .map(|n| n.0.to_string())
        .collect()
}

impl WriteCtx {
    /// Run one job through the ordinary write path.
    async fn run(&self, job: &Job) -> Result<AppliedSummary, LamboError> {
        match &job.payload {
            JobPayload::Derive { concepts, pairs } => {
                let borrowed: Vec<(&str, ConceptType)> =
                    concepts.iter().map(|(c, t)| (c.as_str(), *t)).collect();
                let borrowed_pairs: Vec<(&str, &str)> = pairs
                    .iter()
                    .map(|(a, b)| (a.as_str(), b.as_str()))
                    .collect();
                let parent_of = if borrowed_pairs.is_empty() {
                    ParentOf::none()
                } else {
                    ParentOf::from_pairs(&borrowed_pairs)
                };
                let outcome = match self.match_strategy {
                    MatchStrategy::Hybrid => {
                        hybrid::derive(
                            self.graph.clone(),
                            self.store.as_ref(),
                            self.embedder.as_ref(),
                            &self.embedding,
                            job.interaction,
                            &job.agent,
                            &borrowed,
                            &parent_of,
                            self.max_cooccurrence_per_derive,
                            self.semantic_match_threshold,
                        )
                        .await?
                    }
                    MatchStrategy::Canonical => {
                        let mut g = self.graph.write();
                        graph_derive(
                            &mut g,
                            job.interaction,
                            &job.agent,
                            &borrowed,
                            &parent_of,
                            self.max_cooccurrence_per_derive,
                        )?
                    }
                };
                // No `.await` between here and the caller's settle: that is
                // what makes an aborted worker safe to report as "nothing was
                // written". `abort()` lands at an await point, and there is
                // none left, so a job that reached this line always settles.
                let mut touched = outcome.created.clone();
                touched.extend(outcome.matched.iter().copied());
                mirror_concepts(&self.graph, &self.index, &touched);
                self.daemon_wake.notify_one();
                let created = truncate_ids(&outcome.created);
                let matched = truncate_ids(&outcome.matched);
                Ok(AppliedSummary {
                    kind: WriteKind::Derive,
                    summary: format!(
                        "derived {} concept(s): {} created, {} matched existing",
                        borrowed.len(),
                        outcome.created.len(),
                        outcome.matched.len()
                    ),
                    created,
                    matched,
                    created_count: outcome.created.len(),
                    matched_count: outcome.matched.len(),
                    semantic_merged: Some(outcome.semantic_merged.len()),
                    reinforced: Some(outcome.reinforced),
                    edges: None,
                })
            }
            JobPayload::Action {
                action,
                produces,
                modifies,
                depends_on,
            } => {
                let p: Vec<&str> = produces.iter().map(String::as_str).collect();
                let m: Vec<&str> = modifies.iter().map(String::as_str).collect();
                let d: Vec<&str> = depends_on.iter().map(String::as_str).collect();
                let outcome = {
                    let mut g = self.graph.write();
                    graph_record_action(
                        &mut g,
                        job.interaction,
                        &job.agent,
                        &Action {
                            action: action.as_str(),
                            produces: &p,
                            modifies: &m,
                            depends_on: &d,
                        },
                    )?
                };
                let mut touched = outcome.created.clone();
                touched.push(outcome.action_node);
                mirror_concepts(&self.graph, &self.index, &touched);
                self.daemon_wake.notify_one();
                let created = truncate_ids(&outcome.created);
                Ok(AppliedSummary {
                    kind: WriteKind::RecordAction,
                    summary: format!(
                        "recorded action: {} concept(s) created, {} edge(s)",
                        outcome.created.len(),
                        outcome.edges
                    ),
                    created,
                    matched: Vec::new(),
                    created_count: outcome.created.len(),
                    matched_count: 0,
                    semantic_merged: None,
                    reinforced: None,
                    edges: Some(outcome.edges),
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Receipt store
// ---------------------------------------------------------------------------

struct Entry {
    agent: AgentId,
    issued: DateTime<Utc>,
    answer: ReceiptAnswer,
}

#[derive(Default)]
struct Receipts {
    entries: HashMap<ReceiptId, Entry>,
    /// Issue order, so eviction is oldest-first and an evicted id is always
    /// older than everything still held. That is what lets eviction collapse
    /// into `expired` instead of becoming a fourth answer.
    order: VecDeque<ReceiptId>,
    /// Settled receipts not yet piggybacked, per agent, in settle order.
    undelivered: HashMap<AgentId, VecDeque<ReceiptId>>,
    highest_seq: u64,
}

impl Receipts {
    /// Drop entries past [`RECEIPT_RETENTION`], oldest first.
    fn expire(&mut self, now: DateTime<Utc>) {
        let cutoff = match chrono::Duration::from_std(RECEIPT_RETENTION) {
            Ok(d) => now - d,
            // Unreachable for a 300 s constant; a saturating fallback beats a
            // panic in a sweep that runs on every lookup.
            Err(_) => return,
        };
        while let Some(&oldest) = self.order.front() {
            let stale = self
                .entries
                .get(&oldest)
                .map(|e| e.issued < cutoff)
                .unwrap_or(true);
            if !stale {
                break;
            }
            self.order.pop_front();
            self.forget(&oldest);
        }
    }

    fn forget(&mut self, id: &ReceiptId) {
        if let Some(entry) = self.entries.remove(id) {
            if let Some(q) = self.undelivered.get_mut(&entry.agent) {
                q.retain(|x| x != id);
                if q.is_empty() {
                    self.undelivered.remove(&entry.agent);
                }
            }
        }
    }

    /// Evict oldest-first down to [`MAX_RETAINED_RECEIPTS`].
    fn evict(&mut self) {
        while self.entries.len() > MAX_RETAINED_RECEIPTS {
            match self.order.pop_front() {
                Some(oldest) => self.forget(&oldest),
                None => break,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Lanes
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Lanes {
    /// One FIFO per agent. Single consumer per lane, so a lane drains in
    /// submission order; lanes run concurrently.
    queues: HashMap<AgentId, VecDeque<Job>>,
    /// The live worker per lane. Presence *is* liveness: a worker removes its
    /// own entry under this same lock immediately before returning, and an
    /// enqueue spawns one only when the entry is absent — so the
    /// "lane emptied, worker exited, new job arrived" race cannot be entered.
    workers: HashMap<AgentId, JoinHandle<()>>,
    queued: usize,
    bytes: usize,
    /// Jobs a worker has taken off a lane and not yet settled.
    running: usize,
    /// `true` once the pipeline refuses admission (closing, or fenced).
    sealed: bool,
}

impl Lanes {
    fn outstanding(&self) -> usize {
        self.queued + self.running
    }
}

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
    probe: PlMutex<Option<JoinHandle<()>>>,
    epoch: u64,
    seq: AtomicU64,
    /// Latched the first time a drop is logged, so a sustained overload logs
    /// once rather than once per call. The count keeps telling the truth in
    /// `lambo_stats`.
    drop_logged: AtomicBool,
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
    /// latency on a startup path J2 has already made latency-sensitive. It is
    /// nonetheless the only source of the bound — admission awaits its result
    /// rather than falling back to a constant — so it is bounded by
    /// [`PROBE_BUDGET`] and always publishes something.
    pub(crate) fn spawn(ctx: WriteCtx, clock: crate::daemon::Clock) -> Self {
        let (tx, rx) = watch::channel(None);
        let embedder = ctx.embedder.clone();
        let session = ctx.session.clone();
        let probe = tokio::spawn(async move {
            let calibration = probe_embedder(embedder.as_ref()).await;
            match calibration.items_per_sec {
                Some(rate) => tracing::info!(
                    session = %session,
                    items_per_sec = rate,
                    bound = calibration.bound,
                    concurrency = PROBE_CONCURRENCY,
                    "write queue: bound measured on this deployment's embedder"
                ),
                None => tracing::warn!(
                    session = %session,
                    bound = calibration.bound,
                    "write queue: the embedder could not be probed within {:?}; the bound is the \
                     unmeasured floor and lambo_stats reports write_queue_measured=false",
                    PROBE_BUDGET
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
            probe: PlMutex::new(Some(probe)),
            epoch: rand_epoch(),
            seq: AtomicU64::new(0),
            drop_logged: AtomicBool::new(false),
            clock,
        }
    }

    /// Queue counters, for `lambo_stats`.
    pub fn counters(&self) -> &Arc<WriteQueueCounters> {
        &self.counters
    }

    /// The calibration, if the probe has published one.
    pub fn calibration(&self) -> Option<Calibration> {
        *self.calibration.borrow()
    }

    fn bound_snapshot(&self) -> usize {
        self.calibration().map_or(WRITE_QUEUE_MIN, |c| c.bound)
    }

    /// Outstanding jobs — queued plus running.
    pub fn outstanding(&self) -> usize {
        self.lanes.lock().outstanding()
    }

    /// Receipts currently held.
    pub fn receipts_retained(&self) -> usize {
        self.receipts.lock().entries.len()
    }

    /// Await the calibration, bounded by [`PROBE_BUDGET`].
    ///
    /// Admission blocks here rather than using a provisional constant, because
    /// a provisional constant *is* the "never a constant" the spec forbids —
    /// a burst arriving before the probe landed would be bounded by a number
    /// nothing measured.
    async fn await_calibration(&self) -> Calibration {
        let mut rx = self.calibration.clone();
        if let Some(c) = *rx.borrow_and_update() {
            return c;
        }
        let waited = tokio::time::timeout(PROBE_BUDGET, async {
            loop {
                if rx.changed().await.is_err() {
                    return None;
                }
                if let Some(c) = *rx.borrow_and_update() {
                    return Some(c);
                }
            }
        })
        .await;
        match waited {
            Ok(Some(c)) => c,
            // The probe's own budget is the same constant, so an outer timeout
            // here means the probe task itself was lost (aborted with the
            // session). The floor, declared as unmeasured.
            Ok(None) | Err(_) => Calibration::unmeasured(),
        }
    }

    fn next_receipt(&self) -> ReceiptId {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let id = ReceiptId::new(self.epoch, (self.clock)(), seq);
        let mut r = self.receipts.lock();
        r.highest_seq = r.highest_seq.max(seq);
        id
    }

    /// Seal the pipeline against new admissions. Returns the previous state.
    pub(crate) fn seal(&self) -> bool {
        let mut lanes = self.lanes.lock();
        std::mem::replace(&mut lanes.sealed, true)
    }

    /// Admit a job and hand back its receipt, or refuse it.
    async fn admit(&self, agent: AgentId, interaction: NodeId, payload: JobPayload) -> Submitted {
        let calibration = self.await_calibration().await;
        let bytes = payload.bytes();
        let receipt = self.next_receipt();
        let kind = payload.kind();
        let now = (self.clock)();

        let refusal = {
            let mut lanes = self.lanes.lock();
            if lanes.sealed {
                Some(DropReason::Closed)
            } else if lanes.outstanding() >= calibration.bound {
                Some(DropReason::QueueFull)
            } else if lanes.bytes.saturating_add(bytes) > WRITE_QUEUE_MAX_BYTES {
                Some(DropReason::QueueBytes)
            } else {
                lanes.queued += 1;
                lanes.bytes += bytes;
                lanes
                    .queues
                    .entry(agent.clone())
                    .or_default()
                    .push_back(Job {
                        receipt,
                        agent: agent.clone(),
                        interaction,
                        bytes,
                        payload,
                    });
                if !lanes.workers.contains_key(&agent) {
                    let handle = self.spawn_worker(agent.clone());
                    lanes.workers.insert(agent.clone(), handle);
                }
                None
            }
        };

        let mut receipts = self.receipts.lock();
        receipts.expire(now);
        match refusal {
            Some(reason) => {
                match reason {
                    DropReason::QueueBytes => {
                        self.counters
                            .dropped_queue_bytes
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    // A refusal because the session is closing is still a
                    // refusal by a bound, and the count must not vanish: it
                    // rides the count bound's counter, and the receipt says
                    // which it was.
                    DropReason::QueueFull | DropReason::Closed => {
                        self.counters
                            .dropped_queue_full
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
                // Log ONCE — a sustained overload must not turn stderr into the
                // new bottleneck. The counters keep telling the truth.
                if !self.drop_logged.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        session = %self.ctx.session,
                        agent = %agent,
                        bound = calibration.bound,
                        measured = calibration.measured(),
                        "write queue: dropping writes — {}. This message is logged once; the \
                         running count is lambo_stats' write_queue_dropped",
                        reason.describe(calibration.bound)
                    );
                }
                let answer = ReceiptAnswer::Dropped(reason.describe(calibration.bound));
                receipts.entries.insert(
                    receipt,
                    Entry {
                        agent: agent.clone(),
                        issued: now,
                        answer: answer.clone(),
                    },
                );
                receipts.order.push_back(receipt);
                receipts
                    .undelivered
                    .entry(agent)
                    .or_default()
                    .push_back(receipt);
                receipts.evict();
                Submitted {
                    receipt,
                    kind,
                    answer,
                }
            }
            None => {
                self.counters.accepted.fetch_add(1, Ordering::Relaxed);
                receipts.entries.insert(
                    receipt,
                    Entry {
                        agent,
                        issued: now,
                        answer: ReceiptAnswer::Pending,
                    },
                );
                receipts.order.push_back(receipt);
                receipts.evict();
                Submitted {
                    receipt,
                    kind,
                    answer: ReceiptAnswer::Pending,
                }
            }
        }
    }

    /// Queue a `derive`. The interaction is already open (call path).
    pub(crate) async fn submit_derive(
        &self,
        agent: AgentId,
        interaction: NodeId,
        concepts: Vec<(String, ConceptType)>,
        pairs: Vec<(String, String)>,
    ) -> Submitted {
        self.admit(agent, interaction, JobPayload::Derive { concepts, pairs })
            .await
    }

    /// Queue a `record_action`. The interaction is already open (call path).
    pub(crate) async fn submit_action(
        &self,
        agent: AgentId,
        interaction: NodeId,
        action: String,
        produces: Vec<String>,
        modifies: Vec<String>,
        depends_on: Vec<String>,
    ) -> Submitted {
        self.admit(
            agent,
            interaction,
            JobPayload::Action {
                action,
                produces,
                modifies,
                depends_on,
            },
        )
        .await
    }

    fn spawn_worker(&self, agent: AgentId) -> JoinHandle<()> {
        let ctx = self.ctx.clone();
        let lanes = self.lanes.clone();
        let receipts = self.receipts.clone();
        let counters = self.counters.clone();
        let settled = self.settled.clone();
        let clock = self.clock.clone();
        tokio::spawn(async move {
            loop {
                let job = {
                    let mut l = lanes.lock();
                    match l.queues.get_mut(&agent).and_then(VecDeque::pop_front) {
                        Some(job) => {
                            l.queued -= 1;
                            l.bytes = l.bytes.saturating_sub(job.bytes);
                            l.running += 1;
                            job
                        }
                        None => {
                            // Exit decision and liveness bookkeeping happen
                            // under the same lock an enqueue takes, so no job
                            // can be queued against a worker that has already
                            // decided to stop. Dropping our own JoinHandle
                            // here detaches a task that is about to return.
                            l.queues.remove(&agent);
                            l.workers.remove(&agent);
                            return;
                        }
                    }
                };

                // The fence, checked per job rather than once: the lease can be
                // lost while a lane drains, and every job after that must be
                // refused rather than written into a session another writer
                // owns.
                let outcome = if ctx.lease_lost.load(Ordering::Acquire) {
                    counters.abandoned.fetch_add(1, Ordering::Relaxed);
                    Err(format!(
                        "this handle lost its single-writer lease before the write was applied; \
                         nothing was written for session {}",
                        ctx.session
                    ))
                } else {
                    ctx.run(&job).await.map_err(|e| e.to_string())
                };

                // No `.await` from here to the end of the iteration: an
                // `abort()` cannot land between a completed graph write and the
                // settle that reports it, so "aborted" always means "not
                // written".
                {
                    let mut l = lanes.lock();
                    l.running -= 1;
                }
                let answer = match outcome {
                    Ok(summary) => {
                        counters.applied.fetch_add(1, Ordering::Relaxed);
                        ReceiptAnswer::Applied(summary)
                    }
                    Err(why) => {
                        counters.failed.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(
                            session = %ctx.session,
                            agent = %job.agent,
                            receipt = %job.receipt,
                            error = %why,
                            "write queue: a background write failed; the outcome is on its receipt"
                        );
                        ReceiptAnswer::Failed(why)
                    }
                };
                settle_one(&receipts, &job.receipt, answer, (clock)());
                settled.notify_waiters();
            }
        })
    }

    /// Look up a receipt for `agent`.
    ///
    /// Every non-answer is a *specific* non-answer: see [`ReceiptAnswer`].
    pub fn lookup(&self, agent: &AgentId, id: ReceiptId) -> ReceiptAnswer {
        let mut r = self.receipts.lock();
        r.expire((self.clock)());
        if let Some(entry) = r.entries.get(&id) {
            if &entry.agent != agent {
                return ReceiptAnswer::Forbidden;
            }
            return entry.answer.clone();
        }
        if id.epoch != self.epoch {
            return ReceiptAnswer::RestartLost;
        }
        if id.seq > r.highest_seq {
            return ReceiptAnswer::NeverIssued;
        }
        ReceiptAnswer::Expired
    }

    /// Wait for a receipt to settle — the opt-in synchrony surface.
    ///
    /// `budget` is clamped to [`RECEIPT_WAIT_MAX`], and concurrent waits are
    /// capped ([`MAX_CONCURRENT_RECEIPT_WAITS`]); both bounds exist because a
    /// waiting call occupies a proxy in-flight slot for its whole duration.
    /// A wait that runs out returns [`ReceiptAnswer::Pending`] — honest, and
    /// not a failure.
    pub async fn wait(&self, agent: &AgentId, id: ReceiptId, budget: Duration) -> ReceiptAnswer {
        let budget = budget.min(RECEIPT_WAIT_MAX);
        let _slot = match self.wait_slots.clone().try_acquire_owned() {
            Ok(slot) => slot,
            // Refusing the *wait* is not refusing the answer: the current
            // state is still returned, which for an admitted job is `pending`.
            Err(_) => {
                tracing::debug!(
                    session = %self.ctx.session,
                    "write queue: {MAX_CONCURRENT_RECEIPT_WAITS} receipt waits already in \
                     flight; answering without waiting"
                );
                return self.lookup(agent, id);
            }
        };
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            // Register BEFORE reading the state, or a settle landing between
            // the read and the registration is a lost wakeup — and
            // `notify_waiters` wakes only waiters that have already
            // registered, which for a `Notified` means it has been polled.
            // `enable()` is that registration without awaiting: constructing
            // the future is *not* enough, and getting this wrong costs the
            // whole `RECEIPT_WAIT_MAX` on a write that had already landed.
            let mut notified = Box::pin(self.settled.notified());
            notified.as_mut().enable();
            let answer = self.lookup(agent, id);
            if answer.is_settled() {
                return answer;
            }
            if tokio::time::Instant::now() >= deadline {
                return answer;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.lookup(agent, id);
            }
        }
    }

    /// Take up to [`MAX_PIGGYBACK_RECEIPTS`] settled-and-undelivered receipts
    /// for `agent`, plus how many are still waiting.
    ///
    /// Scoped to the agent of the call being answered, which is what makes the
    /// piggyback correct through a shared hub: a proxied call carries its own
    /// caller-asserted `agent_id`, so each caller is handed only its own
    /// receipts even though every call lands in one process.
    ///
    /// Take-once. A response that never reaches its client loses its
    /// piggyback, which is why the fetch-by-id surface exists as well.
    pub fn take_piggyback(&self, agent: &AgentId) -> (Vec<(ReceiptId, ReceiptAnswer)>, usize) {
        let mut r = self.receipts.lock();
        r.expire((self.clock)());
        let mut ids = Vec::new();
        match r.undelivered.get_mut(agent) {
            Some(queue) => {
                while ids.len() < MAX_PIGGYBACK_RECEIPTS {
                    match queue.pop_front() {
                        Some(id) => ids.push(id),
                        None => break,
                    }
                }
            }
            None => return (Vec::new(), 0),
        }
        let taken: Vec<(ReceiptId, ReceiptAnswer)> = ids
            .into_iter()
            .filter_map(|id| r.entries.get(&id).map(|e| (id, e.answer.clone())))
            .collect();
        let remaining = r.undelivered.get(agent).map_or(0, VecDeque::len);
        if remaining == 0 {
            r.undelivered.remove(agent);
        }
        (taken, remaining)
    }

    /// Drain the pipeline for `close()`.
    ///
    /// Called **before** `close()` takes the writers gate, and that order is
    /// forced rather than chosen: the gate's write side is held for the rest of
    /// `close()`, so a worker that had to pass through the gate could never
    /// finish, and a `close()` waiting for it would deadlock. The workers
    /// therefore do not use the gate at all — this quiesce is what makes
    /// "nothing new lands after the drain" true of them.
    ///
    /// Bounded by [`WRITE_QUEUE_DRAIN_BUDGET`], which is the same number
    /// admission promised. Anything still outstanding when it runs out is
    /// **abandoned**: workers are aborted and joined (aborting alone proves
    /// nothing — the R3-1 lesson), every still-pending receipt is settled
    /// `failed` with a session-closed reason, and the count lands in
    /// `lambo_stats`. Better a receipt that says the write did not happen than
    /// one that says `pending` forever in a process that is exiting.
    pub(crate) async fn quiesce(&self) -> usize {
        self.seal();
        let deadline = tokio::time::Instant::now() + WRITE_QUEUE_DRAIN_BUDGET;
        while self.outstanding() > 0 {
            // `enable()` before the re-check, for the reason in
            // `WritePipeline::wait`: an un-polled `Notified` is not a
            // registered waiter, so a settle landing here would be missed and
            // the quiesce would burn its whole budget on an empty queue.
            let mut notified = Box::pin(self.settled.notified());
            notified.as_mut().enable();
            if self.outstanding() == 0 {
                break;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                break;
            }
        }
        let abandoned = self.abort_workers().await;
        if abandoned > 0 {
            tracing::error!(
                session = %self.ctx.session,
                abandoned,
                "write queue: {abandoned} acked write(s) were NOT applied — the queue did not \
                 drain within {:?} of close(); their receipts say so",
                WRITE_QUEUE_DRAIN_BUDGET
            );
        }
        abandoned
    }

    /// Stop every worker and settle whatever is left. Returns how many receipts
    /// this abandoned.
    pub(crate) async fn abort_workers(&self) -> usize {
        self.seal();
        let (handles, orphans) = {
            let mut lanes = self.lanes.lock();
            let handles: Vec<JoinHandle<()>> = lanes.workers.drain().map(|(_, h)| h).collect();
            let drained: Vec<Job> = lanes
                .queues
                .drain()
                .flat_map(|(_, queue)| queue.into_iter())
                .collect();
            let mut orphans = Vec::with_capacity(drained.len());
            for job in drained {
                lanes.queued = lanes.queued.saturating_sub(1);
                lanes.bytes = lanes.bytes.saturating_sub(job.bytes);
                orphans.push(job.receipt);
            }
            (handles, orphans)
        };
        for handle in handles {
            handle.abort();
            let _ = handle.await;
        }
        // Whatever the aborted workers had in flight is now provably not
        // running, so any receipt still `Pending` names a write that did not
        // happen — including the ones that were still queued.
        let mut abandoned = 0usize;
        let now = (self.clock)();
        let session = self.ctx.session.clone();
        {
            let mut r = self.receipts.lock();
            let pending: Vec<ReceiptId> = r
                .entries
                .iter()
                .filter(|(_, e)| !e.answer.is_settled())
                .map(|(id, _)| *id)
                .collect();
            for id in pending.iter().chain(orphans.iter()) {
                if let Some(entry) = r.entries.get_mut(id) {
                    if entry.answer.is_settled() {
                        continue;
                    }
                    entry.answer = ReceiptAnswer::Failed(format!(
                        "session {session} closed before this write was applied; nothing was \
                         written"
                    ));
                    let agent = entry.agent.clone();
                    r.undelivered.entry(agent).or_default().push_back(*id);
                    abandoned += 1;
                }
            }
            let _ = now;
        }
        if abandoned > 0 {
            let n = abandoned as u64;
            self.counters.failed.fetch_add(n, Ordering::Relaxed);
            self.counters.abandoned.fetch_add(n, Ordering::Relaxed);
        }
        {
            let mut lanes = self.lanes.lock();
            lanes.running = 0;
            lanes.queued = 0;
            lanes.bytes = 0;
        }
        self.settled.notify_waiters();
        abandoned
    }

    /// Abort the calibration probe. Called from `Memory`'s `Drop` and from
    /// `close()`: a probe outliving its session is an embed nobody will read.
    pub(crate) fn abort_probe(&self) {
        if let Some(handle) = self.probe.lock().take() {
            handle.abort();
        }
    }

    /// Abort the workers without awaiting them — the `Drop` path, which cannot
    /// await. Receipts are not settled here: a dropped `Memory` never flushes
    /// its tail either, and a process that is going away has nobody to answer.
    pub(crate) fn abort_all_sync(&self) {
        self.abort_probe();
        let mut lanes = self.lanes.lock();
        lanes.sealed = true;
        for (_, handle) in lanes.workers.drain() {
            handle.abort();
        }
    }
}

fn settle_one(
    receipts: &PlMutex<Receipts>,
    id: &ReceiptId,
    answer: ReceiptAnswer,
    now: DateTime<Utc>,
) {
    let mut r = receipts.lock();
    r.expire(now);
    if let Some(entry) = r.entries.get_mut(id) {
        entry.answer = answer;
        let agent = entry.agent.clone();
        r.undelivered.entry(agent).or_default().push_back(*id);
    }
}

/// What a submission handed back: the receipt and its state at ack time.
#[derive(Clone, Debug)]
pub struct Submitted {
    pub receipt: ReceiptId,
    pub kind: WriteKind,
    /// `Pending` when the job was admitted, `Dropped` when it was refused.
    /// A refusal is **not** an error: the call is answered, the receipt says
    /// nothing was written, and the count is in `lambo_stats`.
    pub answer: ReceiptAnswer,
}

impl Submitted {
    /// `true` when the write was refused before it was attempted.
    pub fn dropped(&self) -> bool {
        matches!(self.answer, ReceiptAnswer::Dropped(_))
    }
}

/// Measure the deployment's embedder: [`PROBE_CONCURRENCY`] concurrent embeds
/// of [`PROBE_TEXT`], wall-clocked.
async fn probe_embedder(embedder: &dyn Embedder) -> Calibration {
    let started = tokio::time::Instant::now();
    let mut set = Vec::with_capacity(PROBE_CONCURRENCY);
    for _ in 0..PROBE_CONCURRENCY {
        set.push(embedder.embed(PROBE_TEXT));
    }
    let joined = tokio::time::timeout(PROBE_BUDGET, futures_join_all(set)).await;
    match joined {
        Ok(results) if results.iter().all(Result::is_ok) => {
            Calibration::from_probe(started.elapsed())
        }
        // Either the embedder refused or it did not answer in time. Both mean
        // the same thing here: this deployment's ceiling is not known, and
        // saying so beats inventing a number.
        _ => Calibration::unmeasured(),
    }
}

/// Join a set of futures concurrently without pulling in `futures`.
///
/// `tokio::join!` needs a fixed arity and `JoinSet` needs `'static` futures;
/// these borrow the embedder. Polling them in one future is what makes the
/// measurement a *concurrency* measurement rather than a sequential one.
async fn futures_join_all<F: std::future::Future>(futures: Vec<F>) -> Vec<F::Output> {
    use std::pin::Pin;
    use std::task::Poll;

    let mut pinned: Vec<Pin<Box<F>>> = futures.into_iter().map(Box::pin).collect();
    let mut out: Vec<Option<F::Output>> = (0..pinned.len()).map(|_| None).collect();
    std::future::poll_fn(move |cx| {
        let mut all_done = true;
        for (slot, fut) in out.iter_mut().zip(pinned.iter_mut()) {
            if slot.is_some() {
                continue;
            }
            match fut.as_mut().poll(cx) {
                Poll::Ready(v) => *slot = Some(v),
                Poll::Pending => all_done = false,
            }
        }
        if all_done {
            Poll::Ready(
                out.iter_mut()
                    .map(|s| s.take().expect("all ready"))
                    .collect(),
            )
        } else {
            Poll::Pending
        }
    })
    .await
}

/// A random per-process epoch, so a receipt from a previous process is
/// recognisable as foreign rather than mistaken for one of ours.
fn rand_epoch() -> u64 {
    let u = uuid::Uuid::new_v4().as_u128();
    (u as u64) ^ ((u >> 64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_receipt_id_round_trips_through_its_wire_form() {
        let id = ReceiptId {
            epoch: 0x0123_4567_89ab_cdef,
            issued_ms: 1_755_000_000_123,
            seq: 42,
        };
        let text = id.to_string();
        assert!(text.starts_with("lwr1."), "{text}");
        assert_eq!(text.parse::<ReceiptId>().unwrap(), id);
    }

    #[test]
    fn a_malformed_receipt_id_is_a_parse_error_not_an_answer() {
        // Every one of these must fail to parse rather than resolve to some
        // other session's receipt: a lookup can only classify ids it can read.
        for bad in [
            "",
            "lwr1",
            "lwr2.0.0.0",
            "lwr1.0.0",
            "lwr1.0.0.0.0",
            "lwr1.zz.0.0",
            "not-a-receipt",
        ] {
            assert!(
                bad.parse::<ReceiptId>().is_err(),
                "{bad:?} parsed as a receipt id"
            );
        }
    }

    #[test]
    fn the_measured_bound_is_clamped_at_both_ends() {
        // An instant embedder (the FixtureEmbedder case) is clamped to the
        // credible ceiling, not believed.
        let fast = Calibration::from_probe(Duration::from_nanos(1));
        assert_eq!(fast.bound, WRITE_QUEUE_MAX);
        assert!(fast.measured());

        // A zero wall time must not divide by zero.
        let zero = Calibration::from_probe(Duration::ZERO);
        assert_eq!(zero.bound, WRITE_QUEUE_MAX);

        // A very slow embedder floors at WRITE_QUEUE_MIN rather than at zero:
        // a bound of 0 would refuse every write.
        let slow = Calibration::from_probe(Duration::from_secs(600));
        assert_eq!(slow.bound, WRITE_QUEUE_MIN);
        assert!(slow.measured());

        // The unmeasured fallback says so, which is what lambo_stats reports.
        let none = Calibration::unmeasured();
        assert_eq!(none.bound, WRITE_QUEUE_MIN);
        assert!(!none.measured());
    }

    #[test]
    fn the_bound_tracks_the_measurement_between_the_clamps() {
        // The rig's own figure: PROBE_CONCURRENCY embeds in 64 ms is
        // 4 / 0.064 = 62.5 items/s, and a 2 s drain budget makes that 125.
        let rig = Calibration::from_probe(Duration::from_millis(64));
        assert_eq!(rig.bound, 125);
        assert!(rig.bound > WRITE_QUEUE_MIN && rig.bound < WRITE_QUEUE_MAX);
    }

    /// The `ledger_queued_lines` lesson, re-derived here: the gauge is correct
    /// only because a refused admission never enters `accepted`. This is the
    /// test the alternative formula fails.
    #[test]
    fn outstanding_excludes_refusals_because_they_never_reached_accepted() {
        let c = WriteQueueCounters::default();
        c.accepted.fetch_add(10, Ordering::Relaxed);
        c.applied.fetch_add(4, Ordering::Relaxed);
        c.failed.fetch_add(1, Ordering::Relaxed);
        c.abandoned.fetch_add(1, Ordering::Relaxed);
        c.dropped_queue_full.fetch_add(7, Ordering::Relaxed);
        c.dropped_queue_bytes.fetch_add(3, Ordering::Relaxed);

        assert_eq!(c.outstanding(), 5, "10 accepted - 4 applied - 1 failed");
        assert_eq!(c.dropped(), 10, "refusals are counted, just not subtracted");
        // The two formulas a future edit might reach for, and why each is
        // wrong. Written with `saturating_sub` because the first one *panics*
        // on subtract-with-overflow in a debug build otherwise — which is the
        // strongest form of the argument, and the reason `outstanding()` uses
        // saturating arithmetic rather than relying on the invariant.
        assert_ne!(
            c.accepted()
                .saturating_sub(c.applied())
                .saturating_sub(c.failed())
                .saturating_sub(c.dropped()),
            c.outstanding(),
            "subtracting refusals underflows the gauge — they were never accepted"
        );
        assert_ne!(
            c.accepted()
                .saturating_sub(c.applied())
                .saturating_sub(c.failed())
                .saturating_sub(c.abandoned()),
            c.outstanding(),
            "abandoned is a label on a subset of failed, not a fourth term"
        );
    }

    #[test]
    fn abandoned_is_always_a_subset_of_failed() {
        let c = WriteQueueCounters::default();
        c.failed.fetch_add(3, Ordering::Relaxed);
        c.abandoned.fetch_add(3, Ordering::Relaxed);
        assert!(
            c.abandoned() <= c.failed(),
            "every abandoned job is settled failed, so the label can never exceed the class"
        );
    }

    #[test]
    fn every_answer_has_a_distinct_tag_and_none_of_them_is_unknown() {
        let answers = [
            ReceiptAnswer::Pending,
            ReceiptAnswer::Applied(AppliedSummary {
                kind: WriteKind::Derive,
                summary: "x".into(),
                created: Vec::new(),
                matched: Vec::new(),
                created_count: 0,
                matched_count: 0,
                semantic_merged: Some(0),
                reinforced: Some(0),
                edges: None,
            }),
            ReceiptAnswer::Failed("x".into()),
            ReceiptAnswer::Dropped("x".into()),
            ReceiptAnswer::Expired,
            ReceiptAnswer::RestartLost,
            ReceiptAnswer::NeverIssued,
            ReceiptAnswer::Forbidden,
        ];
        let mut tags: Vec<&str> = answers.iter().map(ReceiptAnswer::tag).collect();
        tags.sort_unstable();
        let mut deduped = tags.clone();
        deduped.dedup();
        assert_eq!(tags, deduped, "two answers share a tag: {tags:?}");
        for a in &answers {
            assert_ne!(a.tag(), "unknown");
            assert!(!a.describe().is_empty());
        }
        // §J3: expired must not read as unknown, and restart-lost must not
        // either. The distinguishing words are in the prose the model reads.
        assert!(ReceiptAnswer::Expired.describe().contains("expired"));
        assert!(ReceiptAnswer::RestartLost
            .describe()
            .contains("different serve process"));
        // Kept word-for-word consistent with the proxy's HUB_LOST_CODE
        // (-32002) wording, which says the same thing about the same hazard.
        assert!(ReceiptAnswer::RestartLost.describe().contains("UNKNOWN"));
        assert!(ReceiptAnswer::RestartLost
            .describe()
            .contains("Recall before re-deriving"));
    }

    #[test]
    fn only_pending_is_unsettled() {
        assert!(!ReceiptAnswer::Pending.is_settled());
        for a in [
            ReceiptAnswer::Failed("x".into()),
            ReceiptAnswer::Dropped("x".into()),
            ReceiptAnswer::Expired,
            ReceiptAnswer::RestartLost,
            ReceiptAnswer::NeverIssued,
            ReceiptAnswer::Forbidden,
        ] {
            assert!(a.is_settled(), "{} should be terminal", a.tag());
        }
    }

    /// The derivations in this module's constants, asserted rather than
    /// asserted-in-prose. The `const _: () = assert!` guards cover the
    /// relationships; these cover the arithmetic a reader would have to redo.
    #[test]
    fn the_constants_say_what_their_docs_say() {
        assert_eq!(WRITE_QUEUE_MIN, PROBE_CONCURRENCY);
        assert_eq!(
            WRITE_QUEUE_MAX,
            (PROBE_MAX_CREDIBLE_RPS * WRITE_QUEUE_DRAIN_BUDGET.as_secs()) as usize
        );
        assert_eq!(WRITE_QUEUE_MAX, 256);
        assert_eq!(WRITE_QUEUE_MAX_BYTES, 16 * 1024 * 1024);
        assert_eq!(MAX_RECEIPT_IDS, MAX_CONCEPTS_PER_DERIVE);
        // Above the worst flush_lag measured on the rig (227 s), which is the
        // applied-but-not-durable window a receipt has to outlive.
        assert!(RECEIPT_RETENTION.as_secs() > 227);
        // The quiesce cannot be why a close() misses the deadline serve gives
        // it. Duplicated from the const assert on purpose: the build guard
        // proves the relation, this proves the numbers a reader is quoted.
        assert_eq!(WRITE_QUEUE_DRAIN_BUDGET.as_secs(), 2);
        assert_eq!(crate::mcp::serve::CLOSE_FLUSH_GRACE.as_secs(), 8);
        assert_eq!(RECEIPT_WAIT_MAX.as_secs(), 4);
        assert_eq!(MAX_CONCURRENT_RECEIPT_WAITS * 2, 32);
        assert_eq!(crate::mcp::proxy::INFLIGHT_DEPTH_WARN, 64);
    }
}

#[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]
mod pipeline_tests {
    use super::*;
    use crate::embed::FixtureEmbedder;
    use crate::types::Interaction;
    use crate::MemoryStore;
    use std::sync::atomic::AtomicUsize;

    /// An embedder that parks until it is released, so a burst can be held in
    /// the queue long enough to observe the bound.
    struct HeldEmbedder {
        gate: Arc<tokio::sync::Semaphore>,
        inner: FixtureEmbedder,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Embedder for HeldEmbedder {
        fn dimensions(&self) -> usize {
            self.inner.dimensions()
        }
        async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::EmbedError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let permit = self
                .gate
                .acquire()
                .await
                .expect("the gate outlives its holders");
            permit.forget();
            self.inner.embed(text).await
        }
    }

    struct Rig {
        pipeline: WritePipeline,
        graph: Arc<RwLock<Graph>>,
        now: Arc<PlMutex<DateTime<Utc>>>,
    }

    impl Rig {
        /// A pipeline over an empty in-RAM session, with a clock this test can
        /// move and an optional embedder gate.
        fn new(session: &str, embedder: Arc<dyn Embedder>) -> Self {
            let session = SessionId::new(session);
            let graph = Arc::new(RwLock::new(Graph::new(session.clone())));
            let index = Arc::new(RwLock::new(InvertedIndex::default()));
            let now = Arc::new(PlMutex::new(Utc::now()));
            let clock_now = now.clone();
            let clock: crate::daemon::Clock = Arc::new(move || *clock_now.lock());
            let ctx = WriteCtx {
                session,
                graph: graph.clone(),
                index,
                store: Arc::new(MemoryStore::new()),
                embedder,
                embedding: EmbeddingContract {
                    kind: "fixture".into(),
                    model: None,
                    dim: 1024,
                },
                match_strategy: MatchStrategy::Canonical,
                max_cooccurrence_per_derive: 10,
                semantic_match_threshold: 0.85,
                daemon_wake: Arc::new(Notify::new()),
                lease_lost: Arc::new(AtomicBool::new(false)),
            };
            Rig {
                pipeline: WritePipeline::spawn(ctx, clock),
                graph,
                now,
            }
        }

        fn fixture(session: &str) -> Self {
            Self::new(session, Arc::new(FixtureEmbedder::new()))
        }

        /// Open an interaction the way the call path does, so a job has
        /// somewhere to hang.
        fn interaction(&self, agent: &AgentId) -> NodeId {
            let id = NodeId::new();
            let mut g = self.graph.write();
            let previous_id = g.temporal_chain().last().copied();
            let session_id = g.session_id().clone();
            g.insert_interaction(Interaction {
                id,
                session_id,
                agent_id: agent.clone(),
                prompt_text: None,
                previous_id,
                created_at: *self.now.lock(),
            })
            .expect("insert interaction");
            id
        }

        async fn derive(&self, agent: &AgentId, content: &str) -> Submitted {
            let interaction = self.interaction(agent);
            self.pipeline
                .submit_derive(
                    agent.clone(),
                    interaction,
                    vec![(content.to_string(), ConceptType::Entity)],
                    Vec::new(),
                )
                .await
        }
    }

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

    /// Eviction is oldest-first, which is what lets it collapse into `expired`
    /// rather than becoming a fifth answer. Asserted on the store directly:
    /// filling `MAX_RETAINED_RECEIPTS` through the pipeline would be 1024 real
    /// writes.
    #[test]
    fn eviction_is_oldest_first_so_an_evicted_id_is_older_than_everything_held() {
        let mut r = Receipts::default();
        let base = Utc::now();
        let mut ids = Vec::new();
        for seq in 1..=(MAX_RETAINED_RECEIPTS as u64 + 8) {
            let id = ReceiptId {
                epoch: 7,
                issued_ms: base.timestamp_millis() + seq as i64,
                seq,
            };
            ids.push(id);
            r.entries.insert(
                id,
                Entry {
                    agent: AgentId::new("agent-a"),
                    issued: base + chrono::Duration::milliseconds(seq as i64),
                    answer: ReceiptAnswer::Pending,
                },
            );
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
            "an evicted id must be older than everything held, or 'expired' would be a lie"
        );
    }

    /// **Per-agent FIFO under interleaving.** Two agents submit alternately;
    /// each agent's own writes must apply in that agent's submission order.
    ///
    /// The `Temporal` chain is pinned by construction (the interaction is
    /// opened on the call path), so what this test has to prove is the other
    /// half: the *drain* order within a lane, which is what decides which of
    /// two identical concepts is `created` and which is `matched`.
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
        // Release it so calibration lands.
        gate.add_permits(PROBE_CONCURRENCY);

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

    /// **Backpressure, visibly.** A burst larger than the measured bound is
    /// dropped, the drops are counted, and the receipts say nothing was
    /// written.
    #[tokio::test]
    async fn a_burst_past_the_bound_drops_and_counts_it() {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let rig = Rig::new(
            "wq-burst",
            Arc::new(HeldEmbedder {
                gate: gate.clone(),
                inner: FixtureEmbedder::new(),
                calls: calls.clone(),
            }),
        );
        // Let the probe through so a bound exists, then close the gate again so
        // real work parks in the queue. `Hybrid` is what embeds; this rig runs
        // `Canonical`, so hold the lane by never releasing after the probe.
        gate.add_permits(PROBE_CONCURRENCY);
        let bound = loop {
            if let Some(c) = rig.pipeline.calibration() {
                break c.bound;
            }
            tokio::task::yield_now().await;
        };
        assert!((WRITE_QUEUE_MIN..=WRITE_QUEUE_MAX).contains(&bound));

        // Seal the pipeline: a sealed queue refuses admission, which is the
        // same refusal path a full one takes and the only one a `Canonical`
        // rig can hold open deterministically (its writes never await, so a
        // real queue drains as fast as it fills).
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
        assert_eq!(rig.pipeline.counters().dropped(), 1);
        assert_eq!(
            rig.pipeline.counters().accepted(),
            0,
            "a refused admission must never enter `accepted` — the whole gauge rests on it"
        );
        assert_eq!(rig.pipeline.counters().outstanding(), 0);
        // The receipt is still fetchable: a drop is an answer, not a silence.
        assert_eq!(
            rig.pipeline.lookup(&agent, refused.receipt).tag(),
            "dropped"
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

    /// The quiesce must not leave a receipt answering `pending` forever in a
    /// process that is exiting.
    #[tokio::test]
    async fn quiesce_settles_everything_it_could_not_apply() {
        let rig = Rig::fixture("wq-quiesce");
        let agent = AgentId::new("agent-a");
        let submitted = rig.derive(&agent, "tail concept").await;
        let abandoned = rig.pipeline.quiesce().await;
        let answer = rig.pipeline.lookup(&agent, submitted.receipt);
        assert!(
            answer.is_settled(),
            "close must leave no receipt pending: {answer:?}"
        );
        // Either it drained inside the budget (applied) or it was abandoned and
        // said so — never `pending`, and never a silent loss.
        match answer {
            ReceiptAnswer::Applied(_) => assert_eq!(abandoned, 0),
            ReceiptAnswer::Failed(ref why) => {
                assert_eq!(abandoned, 1);
                assert!(
                    why.contains("closed before this write was applied"),
                    "{why}"
                );
            }
            other => panic!("unexpected {other:?}"),
        }
        // And the queue is sealed against anything new.
        let after = rig.derive(&agent, "after close").await;
        assert!(after.dropped(), "{:?}", after.answer);
    }
}
