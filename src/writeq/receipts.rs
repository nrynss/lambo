//! Write receipts: the id, the answer taxonomy, and the receipt store.
//!
//! Memory owns receipt **state** (this module); the MCP server owns
//! **delivery** (the piggyback on the next tool response and the
//! fetch-by-id `lambo_stats(receipt=...)`), reading it through
//! [`WritePipeline::lookup`], [`WritePipeline::wait`],
//! [`WritePipeline::take_piggyback`] and [`WritePipeline::mark_delivered`].
//!
//! Invariants kept here:
//!
//! * every non-answer is a *specific* non-answer ([`ReceiptAnswer`] has no
//!   "unknown");
//! * an **unsettled** receipt is never expired and never evicted
//!   ([`Receipts::expire`], [`Receipts::evict`]);
//! * settle first, sweep second ([`settle_one`]), so a sweep can never take
//!   an outcome with it;
//! * receipts are scoped to the agent that created them (J1).

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::str::FromStr;
use std::sync::atomic::Ordering;
use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::Mutex as PlMutex;

use super::{WritePipeline, WRITE_QUEUE_DRAIN_BUDGET};
use crate::graph::hybrid::HYBRID_IO_TIMEOUT;
use crate::surface::limits::MAX_CONCEPTS_PER_DERIVE;
use crate::types::{AgentId, LamboError};

/// How long a **settled** receipt's outcome is held, measured **from the
/// settle**, not from the issue.
///
/// Above the [`MEASURED_WORST_FLUSH_LAG_SECS`] worst `flush_lag` observed on the
/// rig (§Measurements), because that is the window in which a write is applied
/// in RAM but not yet durable — and a receipt that expired inside it would
/// leave the widened crash window unauditable from the surface that exists to
/// describe it. Keying the window on the settle rather than the issue is what
/// makes that comparison the right one: the applied-but-not-durable window
/// *starts* when the write applies.
///
/// An **unsettled** receipt never expires at all (J3-R1-3). Nothing caps how
/// long a job sits in a lane, so issue-time expiry could — and did, measured —
/// answer `expired` about a job that was still running, after which
/// `settle_one` discarded its outcome.
pub const RECEIPT_RETENTION: Duration = Duration::from_secs(300);

/// Build-time invariant: the durable intent record's retention (the
/// cross-restart receipt window — `applied_after_restart` is answerable only
/// while the consumed row survives) is the SAME window as the in-RAM receipt
/// retention. Two numbers here would mean a receipt whose answer changes
/// depending on whether a restart happened to intervene.
const _: () = assert!(
    RECEIPT_RETENTION.as_secs() == crate::types::WRITE_INTENT_RETENTION.as_secs(),
    "RECEIPT_RETENTION and WRITE_INTENT_RETENTION are one window; see types::WRITE_INTENT_RETENTION"
);

/// The worst `flush_lag` measured on this rig, in seconds (§Measurements).
///
/// A constant rather than a sentence for the same reason
/// [`MEASURED_LOCAL_EMBEDDER_RPS`](super::MEASURED_LOCAL_EMBEDDER_RPS) is one: it lets the relation below be a
/// build invariant.
pub const MEASURED_WORST_FLUSH_LAG_SECS: u64 = 227;

/// Build-time invariant: a receipt must outlive the applied-but-not-durable
/// window, or the crash window J3 widened is unauditable from the surface that
/// describes it.
///
/// **This replaces a false guard** (J3-R1-3). The old one asserted
/// `RETENTION > HYBRID_IO_TIMEOUT + WRITE_QUEUE_DRAIN_BUDGET` and stated the
/// conclusion "…so a receipt could not expire while its own write is still
/// running" — which it did not prove, because expiry keyed on *issue* time and
/// the drain budget is a projection of queue residency rather than a bound on
/// it. That property is now structural ([`Receipts::expire`] skips unsettled
/// entries) and pinned by test, so the guard here is free to assert the
/// relation that actually decides this number.
const _: () = assert!(
    RECEIPT_RETENTION.as_secs() > MEASURED_WORST_FLUSH_LAG_SECS,
    "RECEIPT_RETENTION must exceed the worst flush_lag measured on the rig — a receipt that \
     expired inside the applied-but-not-durable window would leave the crash window J3 widened \
     unauditable from the surface that exists to describe it",
);

/// Ids listed in one retained receipt, before it switches to a count.
///
/// [`MAX_CONCEPTS_PER_DERIVE`], and defined from it: that is what the door
/// admits per call, so listing more would be listing `parent_of` fan-out an
/// agent did not name as a concept.
pub const MAX_RECEIPT_IDS: usize = MAX_CONCEPTS_PER_DERIVE;

/// Retained receipts, oldest **settled** one evicted first.
///
/// **The memory arithmetic, recomputed honestly (J3-R1-6).** The figure quoted
/// here was "a summary plus at most `MAX_RECEIPT_IDS` × 36-byte node ids
/// ≈ 2.4 KiB, so 4096 of them is ≈ 9.4 MiB", and it counted **one of two id
/// lists**: [`AppliedSummary`] carries `created` *and* `matched`, each
/// truncated at [`MAX_RECEIPT_IDS`]. Corrected, at the door's own worst case:
/// `2 × MAX_RECEIPT_IDS` = 128 ids, each a 36-char UUID `String` (36 bytes of
/// text plus a 24-byte header) ≈ 7.5 KiB, plus the one-line summary ≈ 8 KiB per
/// receipt — so **≈ 31 MiB, not ~10 MiB**. A plain `derive` cannot reach it
/// (its `created` and `matched` together cannot exceed
/// [`MAX_CONCEPTS_PER_DERIVE`], so ≈ 16 MiB is the realistic ceiling), but
/// `record_action`'s three resource lists and `derive`'s `parent_of` fan-out
/// can both push `created` past 64 on their own.
///
/// The corrected figure does **not** move the constant, and **the memory budget
/// is now the reason, not the sanity check** (J3 round-1 N4). What this
/// paragraph used to say was: "4096 is driven by `PROBE_CLAMP_RPS > 3 ×
/// MEASURED_LOCAL_EMBEDDER_RPS`, which needs `WRITE_QUEUE_MAX ≥ 424` and
/// therefore `MAX_RETAINED_RECEIPTS ≥ 1696`. The memory budget is the sanity
/// check on that, not its source." That inequality was a **live build
/// assertion** against a measured llama.cpp rate, so the estimator this branch
/// deleted was still sizing both surviving bounds at compile time — structural
/// in kind, measured in magnitude — and a future edit shrinking the receipt cap
/// for memory reasons would have failed the build citing a rationale the branch
/// declares retired. [`PROBE_CLAMP_RPS`](super::PROBE_CLAMP_RPS) no longer derives from
/// [`WRITE_QUEUE_MAX`](super::WRITE_QUEUE_MAX), so that chain is cut and the derivation stands on its
/// own two feet:
///
/// * **4096 is what the memory budget allows** — ≈ 31 MiB of worst-case
///   receipts, computed above, against a process that already holds an entire
///   session graph in RAM. A cost worth naming and paying.
/// * **[`WRITE_QUEUE_MAX`](super::WRITE_QUEUE_MAX) is a quarter of it**, for the eviction-safety reason
///   at the top of that constant: 3× headroom so settled receipts accumulating
///   behind the outstanding ones can never evict a running job's receipt.
///
/// The time bound alone could not do this job — [`RECEIPT_RETENTION`] against
/// `serve`'s own sustained abuse bound ([`crate::mcp::DEFAULT_RATE_LIMIT_RPS`],
/// 50/s) is 15 000 receipts. Historical note kept because the number is
/// unchanged and someone will wonder: 4096 rather than the 1024 this started at,
/// because at 1024 the *then* rate-derived clamp sat below an ordinary local
/// embedder's throughput. That reason is retired; the figure survives on the
/// memory argument, which is why no constant moved when the reason changed.
pub const MAX_RETAINED_RECEIPTS: usize = 4096;

/// Longest a caller may block waiting for its own write to apply: 34 s.
///
/// **Long enough for any one write at the head of its lane** (#11). A write's
/// own I/O, every embed and vector lookup of its derive or `record_action`,
/// runs under one [`HYBRID_IO_TIMEOUT`] deadline, and nothing after it awaits,
/// so a job a worker has picked up settles, applied or failed, within that
/// bound. On top of it sit the two drain budgets of the J3 reasoning
/// (J3-R2R-5): a close's quiesce drains for one whole budget, and the second
/// is slack for the worker to pick the job up. So a wait that runs out
/// answers `pending` only when other writes were queued ahead in the same
/// lane, and that answer is then the honest one.
///
/// It was 4 s, the two budgets alone. On the Metal rig a 3 to 4 concept
/// derive's apply reached 4.6 s (p90 2 s) because each concept is embedded
/// with the whole call's text, so a caller asking for read-your-writes was
/// told `pending` about a healthy write about to land.
///
/// What a longer bound costs: a wait returns the moment its receipt settles,
/// so only a wait on a slow write holds its slot longer. The population of
/// waits is still capped by [`MAX_CONCURRENT_RECEIPT_WAITS`], which is what
/// bounds the proxy's in-flight burst (the J2-R2-7 residual), and one agent
/// holds at most [`MAX_RECEIPT_WAITS_PER_AGENT`] of them.
///
/// A waiting call does not extend a shutdown, though not because the
/// transport drops it: hub and proxy endpoint sessions stay connected until
/// `serve`'s last stage. It is because the close settles every receipt this
/// process holds: `quiesce` drains, then `abort_workers` settles what is
/// left `intent_durable` and wakes every waiter, so those waits return with
/// it. A `pending_replay` id, owed to a replay the close stops, is not
/// settled; its wait returns once the pipeline is sealed and its workers are
/// aborted (#11 review P3-2). Nothing in shutdown joins a waiting call either:
/// `serve`'s final stage cancels the MCP services without awaiting their
/// tool tasks.
pub const RECEIPT_WAIT_MAX: Duration =
    Duration::from_secs(HYBRID_IO_TIMEOUT.as_secs() + 2 * WRITE_QUEUE_DRAIN_BUDGET.as_secs());

/// Build-time invariant: a wait shorter than the queue's own admission promise
/// would make the opt-in-synchrony surface useless by construction.
const _: () = assert!(
    RECEIPT_WAIT_MAX.as_secs() >= 2 * WRITE_QUEUE_DRAIN_BUDGET.as_secs(),
    "RECEIPT_WAIT_MAX must be at least twice WRITE_QUEUE_DRAIN_BUDGET — a close's quiesce drains \
     for one whole budget, so a wait of one budget would expire on jobs the quiesce is still \
     retiring",
);

/// Build-time invariant: a wait covers one write's whole I/O bound (#11).
const _: () = assert!(
    RECEIPT_WAIT_MAX.as_secs() > HYBRID_IO_TIMEOUT.as_secs(),
    "RECEIPT_WAIT_MAX must exceed HYBRID_IO_TIMEOUT — a wait shorter than one write's own I/O \
     bound answers pending about a write at the head of its lane that is still healthy",
);

/// Concurrent receipt waits this process will hold at once.
///
/// **This is the J2-R2-7 / J2-R3-3 coupled residual's bound, and it is here
/// rather than in the proxy because this is the surface that creates the
/// population.** A waiting `lambo_stats(receipt=…, wait_ms=…)` call is a
/// long-lived in-flight request — there is no `lambo_receipt` tool, which is
/// what this docstring named twice (J3-R2-8); the eighth tool is the deviation
/// §J3 argues, and `lambo_stats` is the surface that shipped instead — so
/// through a proxy it occupies an entry in the pump's `inflight`
/// list for the whole wait — and `answer_lost` writes one un-raced frame per
/// in-flight id to a client that may not be reading. So the waiting surface,
/// not the queue bound, is what lengthens that burst: a non-waiting ack returns
/// immediately and never enters the list.
///
/// Both ends are bounded: [`RECEIPT_WAIT_MAX`] caps how long one wait holds a
/// slot, and this caps how many exist. Half of
/// `crate::mcp::proxy::INFLIGHT_DEPTH_WARN` is left for ordinary traffic, so
/// receipt waits alone cannot be what trips the depth warning.
pub const MAX_CONCURRENT_RECEIPT_WAITS: usize = 16;

/// Receipt waits one agent may hold at once: half of
/// [`MAX_CONCURRENT_RECEIPT_WAITS`] (#11 review P3-1).
///
/// The slots are shared by every agent on the pipeline, and a wait over the
/// cap answers at once instead of waiting. With no share, one agent issuing
/// sixteen waits on a slow write turned every other agent's wait into an
/// immediate `pending` for as long as [`RECEIPT_WAIT_MAX`], which #11 raised
/// from 4 s to 34 s. Half, not less, because one `agent_id` is often several
/// clients: the dogfood protocol names an agent by its model, so parallel
/// subagents of one model share an id, and an 8-wide fan-out of waits on one
/// id is ordinary traffic (`i1_a_days_worth_of_concurrent_lines_all_parse`
/// drives exactly that). Half is still the property the finding asks for:
/// whatever one agent does, every other agent keeps eight slots. A wait over
/// the share is answered the way a wait over the global cap is: at once, with
/// the receipt's current state. `agent_id` is caller-asserted, so this is
/// fairness between well-behaved agents, not a defence against one that
/// varies its id; the global cap is what bounds the population either way.
pub const MAX_RECEIPT_WAITS_PER_AGENT: usize = MAX_CONCURRENT_RECEIPT_WAITS / 2;

const _: () = assert!(
    MAX_RECEIPT_WAITS_PER_AGENT >= 1 && MAX_RECEIPT_WAITS_PER_AGENT < MAX_CONCURRENT_RECEIPT_WAITS,
    "an agent's share of the receipt-wait slots must allow a wait and leave room for others",
);

/// One agent's claim on its [`MAX_RECEIPT_WAITS_PER_AGENT`] share, released
/// when the wait ends however it ends (including a cancelled call).
struct AgentWaitShare<'a> {
    waits: &'a PlMutex<HashMap<AgentId, usize>>,
    agent: &'a AgentId,
}

impl<'a> AgentWaitShare<'a> {
    fn claim(waits: &'a PlMutex<HashMap<AgentId, usize>>, agent: &'a AgentId) -> Option<Self> {
        let mut held = waits.lock();
        let count = held.entry(agent.clone()).or_insert(0);
        if *count >= MAX_RECEIPT_WAITS_PER_AGENT {
            return None;
        }
        *count += 1;
        Some(Self { waits, agent })
    }
}

impl Drop for AgentWaitShare<'_> {
    fn drop(&mut self) {
        let mut held = self.waits.lock();
        if let Some(count) = held.get_mut(self.agent) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                held.remove(self.agent);
            }
        }
    }
}

/// Settled receipts piggybacked on one tool response.
///
/// The rest stay queued for the next response and the note says how many, so a
/// burst of finished writes cannot turn one tool result into a wall of text.
/// Eight is one screen of one-line notes.
pub const MAX_PIGGYBACK_RECEIPTS: usize = 8;

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
    pub(super) epoch: u64,
    /// Issue time, Unix milliseconds.
    pub(super) issued_ms: i64,
    /// Per-process monotonic counter, from 1.
    pub(super) seq: u64,
}

impl ReceiptId {
    /// Wire prefix. Versioned so a later format change is detectable rather
    /// than silently mis-parsed as this one.
    const PREFIX: &'static str = "lwr1";

    pub(super) fn new(epoch: u64, issued: DateTime<Utc>, seq: u64) -> Self {
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
    /// Concepts persisted **with a vector** — *applied ≠ embedded* as a
    /// first-class receipt fact (J3-R3-1). `Some` only under the hybrid
    /// strategy, the one that embeds — for `derive` and, since bef53e6, for
    /// `record_action`. A canonical-strategy write never produces vectors by
    /// design, and an absent key must never read as "zero of something that
    /// was attempted". When `Some(e)` with `e < created_count`, some applied
    /// concepts carry no embedding (capability-absent or a refused merge
    /// target) and are unfindable by semantic recall until re-embedded.
    pub embedded: Option<usize>,
}

/// The answer to "what happened to this receipt?".
///
/// **Eleven** variants, and **none of them is "unknown"** (J3 round-1 F1: this
/// count said "seven" at eight, then at ten; it is checked by a test now rather
/// than restated by hand). Two of the eleven are unsettled —
/// [`ReceiptAnswer::Pending`] and [`ReceiptAnswer::PendingReplay`] — and the
/// other nine are terminal for the process answering. The three the spec calls
/// out by name are [`ReceiptAnswer::Expired`], [`ReceiptAnswer::RestartLost`]
/// and [`ReceiptAnswer::NeverIssued`] — a receipt this process discarded, one
/// another process issued, and one nobody issued; the remaining six are
/// [`ReceiptAnswer::Applied`], [`ReceiptAnswer::AppliedAfterRestart`],
/// [`ReceiptAnswer::Failed`], [`ReceiptAnswer::IntentRecorded`],
/// [`ReceiptAnswer::Dropped`] and [`ReceiptAnswer::Forbidden`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReceiptAnswer {
    /// Admitted, not yet decided. Also what a timed-out wait answers *for a
    /// receipt this process holds* — a replay-owed id answers
    /// [`ReceiptAnswer::PendingReplay`] instead (J3-R2R-5).
    Pending,
    /// Admitted by a **previous** process, recorded as a durable intent, and
    /// still owed a replay (J3 round-1 N8).
    ///
    /// Split from [`ReceiptAnswer::Pending`] because the two are not the same
    /// situation and the taxonomy's own principle is that every non-answer is a
    /// *specific* non-answer. A live `pending` settles in tens of milliseconds
    /// inside this process. This one sits behind a sequential backlog, is
    /// interrupted by `close()`, is re-seeded by the *next* process, and can
    /// therefore be a caller's answer across arbitrarily many processes — so
    /// "ask again" is true but "ask again here, shortly" is not.
    PendingReplay,
    /// Applied through the ordinary path.
    Applied(AppliedSummary),
    /// Attempted and rejected. The write did not happen.
    ///
    /// **The string is the N4 class, not the error** (JE2E-12). This receipt is
    /// the *only* channel a background write's failure has to the model, and it
    /// used to interpolate `LamboError::to_string()` verbatim — so a sqlite
    /// "unable to open database file: /path" or a driver message reached a
    /// model here, where the same error through the synchronous path was
    /// flattened by `tool_err` to "store error (the detail was logged
    /// server-side)". Nothing credential-shaped had a producer on this path, and
    /// `redact_urls` covers `scheme://` tokens on every render — but a store
    /// file path has no `://`, and "no producer today" is a fact about today.
    ///
    /// So it carries what the synchronous path carries, built from the same
    /// `crate::surface::error::err_class` rather than a second match, and the
    /// operator's copy is written at the same site twice over: the `completion`
    /// line's `error` field (operator-facing JSONL) and a `tracing::warn!`.
    /// [`ReceiptAnswer::Dropped`] is deliberately untouched — its string is
    /// `DropReason::describe`, lambo's own text about lambo's own bounds, with
    /// nothing of the environment in it.
    Failed(String),
    /// Applied, but not by the process that acked it: the write survived a
    /// close (or crash-with-flushed-tail) as a durable intent, and a later
    /// serve of the session applied it — or applied it before restarting and
    /// the consumed intent record carried the fact across. The payload is the
    /// applied summary sentence; node ids are not retained across the restart
    /// (recall finds the concepts).
    AppliedAfterRestart(String),
    /// Not applied before this session closed, and **not lost**: the validated
    /// job is recorded as a durable intent that the close's final flush
    /// persists, and the next serve of this session re-attempts it —
    /// idempotently, in submission order (J3 durable intents). Terminal *for
    /// this process*; the next process's answer for this id is
    /// `applied_after_restart` (or `failed`, if the replay is refused for the
    /// content, or `pending_replay` while it is still owed).
    ///
    /// **Read the tense: this is durable as of the mutation log, pending the
    /// close's final flush** (J3 round-1 N7). It is the one answer in the
    /// taxonomy that asserts a future rather than records a past, and it is
    /// forced: `abort_workers` settles every unsettled receipt during the
    /// quiesce, and `close()`'s final flush necessarily runs **after** that
    /// quiesce (`crate::Memory::close`), so the assertion is made before the
    /// write it describes reaches the store. If that flush fails, the process
    /// has told a caller "recorded as a durable intent" about a record that
    /// never landed.
    ///
    /// Why it is left as a prediction rather than deferred until the flush
    /// reports success: the alternative settles receipts *after* the store I/O
    /// that may hang or fail, which means a close that cannot reach its store
    /// leaves callers holding `pending` — an answer that is unsettled, so
    /// waiters keep waiting — where they currently get a specific one. And the
    /// contradiction is transient and **self-correcting in both directions**:
    /// after a restart an id with no durable record answers `restart_lost`
    /// (honest), and an id whose apply *did* land answers
    /// `applied_after_restart` from the consumed row. The only observation
    /// window is a `lambo_stats(receipt=…)` racing the close.
    IntentRecorded,
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

/// The model-facing form of a background write's failure (JE2E-12).
///
/// N4's rule, applied to the async path: the model gets a class and a pointer to
/// the log, never the raw error, which can carry a store URL, a store file path
/// or a driver message. This is the same sentence `mcp::server::tool_err`
/// produces for the synchronous path, built from the same
/// [`crate::surface::error::err_class`] so the two cannot drift.
///
/// Every caller writes the raw error to the operator at the same site — a
/// `completion` ledger line and a `tracing::warn!` — so nothing is lost, only
/// relocated to the audience that can act on it.
pub(super) fn model_safe_failure(err: &LamboError) -> String {
    format!(
        "{} (the detail was logged server-side)",
        crate::surface::error::err_class(err)
    )
}

impl ReceiptAnswer {
    /// Stable machine tag, for `structuredContent` and for tests.
    pub fn tag(&self) -> &'static str {
        match self {
            ReceiptAnswer::Pending => "pending",
            ReceiptAnswer::PendingReplay => "pending_replay",
            ReceiptAnswer::Applied(_) => "applied",
            ReceiptAnswer::Failed(_) => "failed",
            ReceiptAnswer::AppliedAfterRestart(_) => "applied_after_restart",
            ReceiptAnswer::IntentRecorded => "intent_durable",
            ReceiptAnswer::Dropped(_) => "dropped",
            ReceiptAnswer::Expired => "expired",
            ReceiptAnswer::RestartLost => "restart_lost",
            ReceiptAnswer::NeverIssued => "never_issued",
            ReceiptAnswer::Forbidden => "forbidden",
        }
    }

    /// `true` once the answer can no longer change.
    pub fn is_settled(&self) -> bool {
        !matches!(self, ReceiptAnswer::Pending | ReceiptAnswer::PendingReplay)
    }

    /// A unique index per variant, with **no `_` arm** — the mechanical
    /// exhaustiveness that makes the "Eleven variants" docstring true by
    /// construction (J3-R2R-4): a twelfth variant is a compile error here
    /// before it can drift out of the taxonomy tests.
    pub fn ordinal(&self) -> usize {
        match self {
            ReceiptAnswer::Pending => 0,
            ReceiptAnswer::PendingReplay => 1,
            ReceiptAnswer::Applied(_) => 2,
            ReceiptAnswer::Failed(_) => 3,
            ReceiptAnswer::AppliedAfterRestart(_) => 4,
            ReceiptAnswer::IntentRecorded => 5,
            ReceiptAnswer::Dropped(_) => 6,
            ReceiptAnswer::Expired => 7,
            ReceiptAnswer::RestartLost => 8,
            ReceiptAnswer::NeverIssued => 9,
            ReceiptAnswer::Forbidden => 10,
        }
    }

    /// One line, addressed to the model that will read it.
    pub fn describe(&self) -> String {
        match self {
            ReceiptAnswer::Pending => "pending — admitted, not yet applied; ask again".into(),
            ReceiptAnswer::PendingReplay => "pending — admitted by an EARLIER serve of this \
                                             session and recorded as a durable intent; still \
                                             owed a replay, behind a backlog replayed one write \
                                             at a time. It may settle in this process or in a \
                                             later one; ask again, and recall rather than \
                                             re-deriving."
                .into(),
            ReceiptAnswer::Applied(s) => format!("applied — {}", s.summary),
            ReceiptAnswer::Failed(why) => format!("FAILED, nothing was written — {why}"),
            ReceiptAnswer::AppliedAfterRestart(summary) => format!(
                "applied after a restart — {summary} (confirmed from the durable intent \
                 record; recall to see the concepts)"
            ),
            ReceiptAnswer::IntentRecorded => "not applied before this session closed — the \
                                              validated write is recorded as a DURABLE INTENT \
                                              and the next serve of this session will \
                                              RE-ATTEMPT it, idempotently and in submission \
                                              order. Fetch this receipt id there to learn the \
                                              outcome, or recall."
                .into(),
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

pub(super) struct Entry {
    pub(super) agent: AgentId,
    /// When the answer became terminal, and therefore when
    /// [`RECEIPT_RETENTION`] starts. `None` while the write is queued or
    /// running — and an entry with `None` here is **never** expired and never
    /// evicted (J3-R1-3).
    pub(super) settled_at: Option<DateTime<Utc>>,
    pub(super) answer: ReceiptAnswer,
}

impl Entry {
    /// Settle this entry, stamping the retention clock. Returns `false` when it
    /// was already settled, so no outcome can be overwritten by a later sweep.
    pub(super) fn settle(&mut self, answer: ReceiptAnswer, now: DateTime<Utc>) -> bool {
        if self.answer.is_settled() {
            return false;
        }
        self.answer = answer;
        self.settled_at = Some(now);
        true
    }
}

#[derive(Default)]
pub(super) struct Receipts {
    pub(super) entries: HashMap<ReceiptId, Entry>,
    /// Issue order, so eviction is oldest-first and an evicted id is always
    /// older than everything still held. That is what lets eviction collapse
    /// into `expired` instead of becoming a fourth answer.
    pub(super) order: VecDeque<ReceiptId>,
    /// Settled receipts not yet piggybacked, per agent, in settle order.
    pub(super) undelivered: HashMap<AgentId, VecDeque<ReceiptId>>,
    pub(super) highest_seq: u64,
}

impl Receipts {
    /// Drop **settled** entries whose retention window has passed, oldest
    /// first.
    ///
    /// **An unsettled entry is skipped, never expired** (J3-R1-3). Nothing caps
    /// how long a job sits in a lane, so the old issue-time sweep could answer
    /// `expired` about a job that was still running — measured, with
    /// `outstanding = 1` — after which [`settle_one`] discarded its outcome
    /// because it swept before it settled. Skipped ids are pushed back in
    /// order, so `order` stays issue-ordered and the sweep stays O(popped).
    pub(super) fn expire(&mut self, now: DateTime<Utc>) {
        let cutoff = match chrono::Duration::from_std(RECEIPT_RETENTION) {
            Ok(d) => now - d,
            // Unreachable for a 300 s constant; a saturating fallback beats a
            // panic in a sweep that runs on every lookup.
            Err(_) => return,
        };
        let mut unsettled: Vec<ReceiptId> = Vec::new();
        while let Some(&oldest) = self.order.front() {
            match self.entries.get(&oldest) {
                // An id in `order` with no entry is already gone; drop the
                // bookkeeping.
                None => {
                    self.order.pop_front();
                }
                Some(entry) => match entry.settled_at {
                    None => {
                        self.order.pop_front();
                        unsettled.push(oldest);
                    }
                    Some(settled) if settled < cutoff => {
                        self.order.pop_front();
                        self.forget(&oldest);
                    }
                    // Issue order is settle order only loosely, but the first
                    // entry still inside its window is where the cheap sweep
                    // has to stop: anything behind it is younger by issue and
                    // will be reached by a later sweep or by `evict`.
                    Some(_) => break,
                },
            }
        }
        for id in unsettled.into_iter().rev() {
            self.order.push_front(id);
        }
    }

    pub(super) fn forget(&mut self, id: &ReceiptId) {
        if let Some(entry) = self.entries.remove(id)
            && let Some(q) = self.undelivered.get_mut(&entry.agent)
        {
            q.retain(|x| x != id);
            if q.is_empty() {
                self.undelivered.remove(&entry.agent);
            }
        }
    }

    /// Evict oldest-**settled**-first down to [`MAX_RETAINED_RECEIPTS`].
    ///
    /// **Unsettled entries are skipped here too.** The count side used to rest
    /// on an arithmetic argument — `WRITE_QUEUE_MAX ≤ MAX_RETAINED_RECEIPTS / 4`
    /// — which bounds the outstanding *set* but not the number of receipts
    /// issued while one job is parked: refusals get receipts as well, so a
    /// sustained drop storm could push a running write's `Pending` entry out of
    /// the newest quarter. Skipping it makes the property structural, in the
    /// same move as [`Receipts::expire`]. The scan can always find a victim,
    /// because unsettled entries are bounded by the admission bound
    /// (`≤ WRITE_QUEUE_MAX`, a quarter of this cap); if it somehow cannot, the
    /// store grows rather than dropping a live receipt, and `receipts_retained`
    /// says so.
    pub(super) fn evict(&mut self) {
        let mut unsettled: Vec<ReceiptId> = Vec::new();
        while self.entries.len() > MAX_RETAINED_RECEIPTS {
            match self.order.pop_front() {
                Some(oldest) => match self.entries.get(&oldest) {
                    Some(entry) if entry.settled_at.is_none() => unsettled.push(oldest),
                    _ => self.forget(&oldest),
                },
                None => break,
            }
        }
        for id in unsettled.into_iter().rev() {
            self.order.push_front(id);
        }
    }
}

/// Record one outcome against its receipt.
///
/// **Settle first, sweep second** (J3-R1-3). The first version expired before it
/// looked the entry up, so a receipt swept while its own job was still running
/// took the job's outcome with it: the counters moved, and no receipt recorded
/// what happened. The sweep now runs after, and cannot touch an entry settled
/// this instant because [`RECEIPT_RETENTION`] is measured from the settle.
pub(super) fn settle_one(
    receipts: &PlMutex<Receipts>,
    id: &ReceiptId,
    answer: ReceiptAnswer,
    now: DateTime<Utc>,
) {
    let mut r = receipts.lock();
    if let Some(entry) = r.entries.get_mut(id)
        && entry.settle(answer, now)
    {
        let agent = entry.agent.clone();
        r.undelivered.entry(agent).or_default().push_back(*id);
    }
    r.expire(now);
}

impl WritePipeline {
    /// Receipts currently held.
    pub fn receipts_retained(&self) -> usize {
        self.receipts.lock().entries.len()
    }

    pub(super) fn next_receipt(&self) -> ReceiptId {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let id = ReceiptId::new(self.epoch, (self.clock)(), seq);
        let mut r = self.receipts.lock();
        r.highest_seq = r.highest_seq.max(seq);
        id
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
        drop(r);
        // J3: a receipt from a previous process whose fate the durable intent
        // record carries answers its truth — `pending_replay` while its replay
        // is owed, `applied_after_restart`/`failed` once decided — instead of
        // falling through to `restart_lost`.
        if let Some((owner, answer)) = self.restart.lock().get(&id) {
            if owner != agent {
                return ReceiptAnswer::Forbidden;
            }
            return answer.clone();
        }
        if id.epoch != self.epoch {
            return ReceiptAnswer::RestartLost;
        }
        let r = self.receipts.lock();
        if id.seq > r.highest_seq {
            return ReceiptAnswer::NeverIssued;
        }
        ReceiptAnswer::Expired
    }

    /// Wait for a receipt to settle — the opt-in synchrony surface.
    ///
    /// `budget` is clamped to [`RECEIPT_WAIT_MAX`], and concurrent waits are
    /// capped ([`MAX_CONCURRENT_RECEIPT_WAITS`], of which one agent holds at
    /// most [`MAX_RECEIPT_WAITS_PER_AGENT`]); the bounds exist because a
    /// waiting call occupies a proxy in-flight slot for its whole duration.
    /// A wait that runs out returns [`ReceiptAnswer::Pending`] for a receipt
    /// this process holds, or [`ReceiptAnswer::PendingReplay`] for a replay-owed
    /// id — either is honest, and neither is a failure.
    pub async fn wait(&self, agent: &AgentId, id: ReceiptId, budget: Duration) -> ReceiptAnswer {
        let budget = budget.min(RECEIPT_WAIT_MAX);
        // The agent's share first (#11 review P3-1), so an agent at its share
        // never takes a global slot another agent could have had.
        let Some(_share) = AgentWaitShare::claim(&self.waits_per_agent, agent) else {
            tracing::debug!(
                session = %self.ctx.session,
                agent = %agent,
                "write queue: this agent already holds {MAX_RECEIPT_WAITS_PER_AGENT} receipt \
                 waits; answering without waiting"
            );
            return self.lookup(agent, id);
        };
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
            // Closed (#11 review P3-2): once the lanes are sealed and the
            // workers aborted, nothing in this process can settle the id.
            // The close already settled every receipt it held and woke this
            // loop; a `pending_replay` id is owed to a replay the close
            // stopped, so without this its wait ran on against a closed
            // session to its own deadline.
            if self.closed_to_settles() {
                return answer;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.lookup(agent, id);
            }
        }
    }

    /// `true` once nothing in this process can settle another receipt: the
    /// lanes are sealed and [`WritePipeline::abort_workers`] has drained and
    /// aborted the workers. The replay loop stops at the seal as well.
    fn closed_to_settles(&self) -> bool {
        let lanes = self.lanes.lock();
        lanes.sealed && lanes.workers_aborted
    }

    /// Take one receipt out of the piggyback queue because it has just been
    /// delivered **explicitly**, in the response to a fetch of that id.
    ///
    /// J3-R1-9: `answered` wraps every tool, so a `lambo_stats(receipt = R)`
    /// used to carry both the explicit `receipt` block for R *and* a piggyback
    /// note naming R — one model reading its own write outcome twice in one
    /// message. Take-once is unaffected: the receipt is delivered exactly once
    /// either way, and this is the delivery.
    pub fn mark_delivered(&self, agent: &AgentId, id: ReceiptId) {
        let mut r = self.receipts.lock();
        let empty = match r.undelivered.get_mut(agent) {
            Some(queue) => {
                queue.retain(|held| held != &id);
                queue.is_empty()
            }
            None => false,
        };
        if empty {
            r.undelivered.remove(agent);
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
}
