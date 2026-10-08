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

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::{Mutex as PlMutex, RwLock};
use serde_json::json;
use tokio::sync::{watch, Notify, Semaphore};
use tokio::task::JoinHandle;

use crate::cli::caps::{MAX_CONCEPTS_PER_DERIVE, MAX_CONTENT_BYTES};
use crate::embed::Embedder;
use crate::graph::action::{
    record_action_with_embeddings as graph_record_action_embedded, Action, ActionEmbeddings,
};
use crate::graph::derive::{derive as graph_derive, ParentOf};
use crate::graph::hybrid;
use crate::graph::index::InvertedIndex;
use crate::graph::Graph;
use crate::store::GraphStore;
use crate::types::{
    AgentId, ConceptType, EmbeddingContract, LamboError, MatchStrategy, Node, NodeId, SessionId,
    WriteIntent, WriteIntentOutcome, WriteIntentPayload,
};

// ---------------------------------------------------------------------------
// Constants — every one of them derived, at the constant, from something else
// in the tree or from a measurement in the phase doc.
// ---------------------------------------------------------------------------

/// How long a clean `close()` drains the queue before **deferring** the
/// remainder (`WritePipeline::quiesce`).
///
/// Under J3's durable intents this stops being a durability deadline: whatever
/// does not drain inside the budget survives as a durable intent and the next
/// serve applies it, so this constant prices *latency at shutdown against
/// promptness of the write*, nothing more. (Its earlier career — sizing
/// admission through a rate projection so the queue "could not admit more than
/// shutdown will wait for" — produced J3-R1-1, J3-R2-1, J3-R3-1 and J3-R3-2 in
/// turn, one falsified estimator axis each; the redesign retired that role.)
///
/// Two seconds, and the ceiling on that choice is `close()`'s own budget:
/// `lambo serve` wraps `Memory::close` in
/// `crate::mcp::serve::CLOSE_FLUSH_GRACE` (8 s), out of which
/// `SHUTDOWN_GRACE + CLOSE_GRACE ≤ SHUTDOWN_BUDGET` is sized. The quiesce runs
/// **in series before** the existing final flush, so it is carved out of that
/// 8 s rather than added on top — the same reasoning `LEASE_RELEASE_GRACE`
/// used. A quarter of the window is the largest slice that leaves the flush,
/// which is the step that actually delivers durability, the majority of it.
pub const WRITE_QUEUE_DRAIN_BUDGET: Duration = Duration::from_secs(2);

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

/// Build-time invariant: the quiesce cannot become the reason a `close()` blows
/// the deadline `serve` gives it.
const _: () = assert!(
    WRITE_QUEUE_DRAIN_BUDGET.as_secs() * 4 <= crate::mcp::serve::CLOSE_FLUSH_GRACE.as_secs(),
    "WRITE_QUEUE_DRAIN_BUDGET must stay at or under a quarter of CLOSE_FLUSH_GRACE — the write \
     queue quiesce runs in series BEFORE the final flush, so it is carved out of close()'s \
     budget, not added to it",
);

/// Build-time invariant: a zero drain budget would defer every acked write at
/// every close — safe under durable intents, but a silent behaviour cliff an
/// edit should have to acknowledge. (The old reason here — a divide-by-zero in
/// `PROBE_CLAMP_RPS`, which used to divide by this — went away when the clamp
/// stopped being budget-derived, J3 redesign.)
const _: () = assert!(
    WRITE_QUEUE_DRAIN_BUDGET.as_secs() > 0,
    "WRITE_QUEUE_DRAIN_BUDGET must be at least one whole second — a zero budget silently turns \
     every clean close into a full deferral",
);

/// **Deleted roles, recorded where they lived** (J3 redesign): this block used
/// to define `DRAIN_PROJECTION_SHARE` (the share of the drain budget an
/// admission bound could project a *rate* against), `WRITE_QUEUE_LANE_MIN` (the
/// floor under a rate-derived lane bound), `WRITE_QUEUE_MIN` (the unmeasured
/// aggregate floor), `PROBE_LANE_CEILING` and `PROBE_AGGREGATE_CEILING` (the
/// probe-era caps on rate-derived bounds — the aggregate one derived per-lane
/// and applied across lanes, which is J3-R3-2: 13 of 16 acked writes abandoned
/// from eight concurrent agents up). All five existed to make an estimated
/// rate safe to build a durability invariant on. No parameter can do that —
/// five falsified axes in three rounds are the evidence — so under durable
/// intents the bounds are static ([`WRITE_QUEUE_LANE_MAX`],
/// [`WRITE_QUEUE_MAX`]) and the whole family is deleted rather than re-derived
/// under the new role: fairness needs a share, not a projection. J3-R3-2 is
/// closed by this deletion, argument above.
///
/// The **per-lane fair-share bound**: what one agent may hold outstanding.
///
/// [`WRITE_QUEUE_MAX`] bounds the whole queue (a memory cap, derived from the
/// receipt store); this divides it by [`MAX_CONCURRENT_RECEIPT_WAITS`] — the
/// constant that already declares how many concurrent callers the receipt
/// surface is designed for — so one agent can take at most a 1/16 share of the
/// queue before its own lane refuses it. A fairness rule built from two
/// structural constants, not a drain estimate: being wrong here costs one
/// agent a refusal it can retry, never a durability loss (its accepted writes
/// are durable intents), and never another agent's starvation (15/16 of the
/// queue remains for everyone else). Generous by design — at 64 it is 16× the
/// old probe-era lane ceiling — because depth now prices only apply-latency
/// and close-deferral, both visible on receipts.
pub const WRITE_QUEUE_LANE_MAX: usize = WRITE_QUEUE_MAX / MAX_CONCURRENT_RECEIPT_WAITS;

/// Build-time invariant: the fair-share division must leave a usable lane.
const _: () = assert!(
    WRITE_QUEUE_LANE_MAX >= 1 && WRITE_QUEUE_LANE_MAX <= WRITE_QUEUE_MAX,
    "WRITE_QUEUE_LANE_MAX must be at least one job and no more than the whole queue — shrink \
     MAX_RETAINED_RECEIPTS or grow MAX_CONCURRENT_RECEIPT_WAITS far enough and one lane's fair \
     share rounds to zero, which is an outage, not fairness",
);

/// The queue's aggregate admission bound — **derived from receipt retention,
/// not from throughput.**
///
/// (J3 round-1 N3: this headline read "Upper clamp on the measured bound" for a
/// bound that stopped being measured and started *being* the bound at the
/// estimator demotion. There is nothing left for it to clamp.)
///
/// Every outstanding job holds a `Pending` receipt, and receipt eviction is
/// oldest-first, so a queue deeper than [`MAX_RETAINED_RECEIPTS`] could evict
/// the receipt of a write that is still running — which would answer `expired`
/// about a job in flight, breaking the one promise the whole taxonomy rests on.
/// A quarter of the retention capacity is the clamp: it leaves 3x headroom for
/// *settled* receipts to accumulate behind the outstanding ones, which is what
/// an agent reading its piggybacks is doing.
///
/// This replaced a "where a probe stops being credible" framing that was
/// **measured wrong**: it put the credible ceiling at 128 items/s on the theory
/// that a deployment parallelising twice as well as the phase doc's 4-wide
/// recall figure (4 / 64 ms ≈ 62 items/s) would land there. Probing this
/// machine's own llama.cpp BGE-M3 measured **110 to 141 items/s**, above that
/// ceiling — so the "clamp" would have been the operative bound on a perfectly
/// ordinary local embedder while claiming to be an implausibility guard. The
/// retention derivation has no such problem: it is a property of this module,
/// not a guess about hardware, and at 1024 it sits 7x above the measured rate.
pub const WRITE_QUEUE_MAX: usize = MAX_RETAINED_RECEIPTS / 4;

/// Build-time invariant: the fair-share numerator is a real queue.
///
/// **This replaces a vacuous guard** (J3-R1-7): `(N / 4) * 4 <= N` holds for
/// every `usize` under integer division, so the old assertion could not fail
/// for any value of `MAX_RETAINED_RECEIPTS` and proved none of the property its
/// message claimed. The property the old message reached for —
/// "oldest-first eviction cannot discard the receipt of a write that is still
/// running" — is no longer an arithmetic claim at all: [`Receipts::evict`] and
/// [`Receipts::expire`] both **skip unsettled entries** outright, and
/// `a_running_jobs_receipt_neither_expires_nor_loses_its_outcome` pins it.
const _: () = assert!(
    WRITE_QUEUE_MAX >= MAX_CONCURRENT_RECEIPT_WAITS,
    "WRITE_QUEUE_MAX must cover at least one job per concurrent caller the receipt surface is \
     designed for, or the fair-share lane bound rounds to zero",
);

/// Sanitization clamp on a **reported** rate, in items/second — telemetry
/// hygiene, not a bound (J3 redesign: no rate sizes a bound any more).
///
/// `ObservedRate::items_per_sec` and `rate_of` reach for this when a wall
/// time reads zero or absurd — the `crate::FixtureEmbedder` case, which
/// "measures" ~98 000 items/s by not doing work. One full queue
/// ([`WRITE_QUEUE_MAX`]) per second is comfortably above any real embedder
/// this project has measured (110 to 141 items/s 4-wide, llama.cpp BGE-M3 on
/// CPU at the 35-byte probe text; ~19 to 22 at the 1 KiB one; ≈40–45 serial
/// from the same **22 to 27 ms** embed — J3-R3-5 corrected this line's
/// "22–25 ms" misquote of §Measurements) while still being a number instead of
/// infinity, so a fixture-fast reading stays plottable without pretending to
/// mean something. NOTE for the operator reading
/// `write_queue_serial_items_per_sec` against these reference figures: since
/// round 2 that key publishes the **slower of two probe sizes** (35 B and
/// 1 KiB), so ~18 to 21 items/s is the ordinary reading on this rig, not the
/// 40–45 the short text alone used to show.
/// **Stated on its own terms since J3 round-1 N4.** This used to read
/// `WRITE_QUEUE_MAX as u64`, which tied a telemetry ceiling to an admission cap
/// for no reason — and, through the build assert below, made the admission cap
/// answerable to a *measured embedder rate*: `PROBE_CLAMP_RPS > 3 ×
/// MEASURED_LOCAL_EMBEDDER_RPS` implied `WRITE_QUEUE_MAX ≥ 424` and therefore
/// `MAX_RETAINED_RECEIPTS ≥ 1696`, so both surviving bounds were structural in
/// kind and measured in magnitude, at build time, by the estimator this branch
/// deleted. The value is unchanged (1024, so no reading moves); what changed is
/// that nothing about admission now depends on a number this rig's llama.cpp
/// produced, and the assert below guards telemetry alone.
pub const PROBE_CLAMP_RPS: u64 = 1_024;

/// The fastest embedder throughput measured on this rig, in items/second, at
/// [`PROBE_CONCURRENCY`]: a live llama.cpp BGE-M3 q8_0 on CPU, probed through
/// the release binary over stdio (2026-08-20; the run reported 110, 131 and 141
/// across repeats). Recorded as a constant so the guard below can be a build
/// invariant rather than a sentence.
///
/// **Those repeats were taken at the 35-byte [`PROBE_TEXT`], and that is
/// deliberately what this constant keeps** (J3-R2-1). Probing at
/// [`PROBE_TEXT_BYTES`] the same rig reads ~19 to 22 items/s 4-wide — five times
/// lower — but the guard below wants the *largest* rate a real local embedder
/// can produce, since its job is to keep [`PROBE_CLAMP_RPS`] clear of one.
/// Replacing 141 with the smaller figure would loosen the guard while looking
/// like an update.
pub const MEASURED_LOCAL_EMBEDDER_RPS: u64 = 141;

/// Build-time invariant: the clamp must sit well clear of a real embedder.
///
/// This is the guard the first version of these constants failed. A clamp
/// derived at 128 items/s sat *below* the 141 measured here, so it would have
/// clipped an ordinary local embedder's own reading. Three times the measured
/// rate is the margin.
///
/// **What this guard is about, restated (J3 round-1 N4).** It used to say that
/// violating it would make "the queue bound stop being a per-deployment
/// measurement and become a constant" — which is now the *intended* state, and
/// which is how a live build assertion came to be the thing sizing both
/// surviving bounds. Since [`PROBE_CLAMP_RPS`] no longer derives from
/// [`WRITE_QUEUE_MAX`], this constrains nothing but telemetry hygiene: it keeps
/// the sanitizer clear of any rate a real embedder can produce, so a clamped
/// reading always means "this measurement is not real" and never "this embedder
/// is fast".
const _: () = assert!(
    PROBE_CLAMP_RPS > 3 * MEASURED_LOCAL_EMBEDDER_RPS,
    "PROBE_CLAMP_RPS must stay well above the throughput a real local embedder measures, or the \
     sanitizer would clip a genuine reading and the rate telemetry would report a number no \
     deployment produced. It bounds no admission decision: the bounds are static",
);

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

/// Width of the calibration probe's **concurrent** leg.
///
/// Four, because the phase doc's parallelism figure is a 4-wide one (4 recalls:
/// 380 ms sequential against 64 ms concurrent). The probe re-measures the rate
/// per deployment; the 4 fixes only how wide that leg is taken. It sizes
/// **nothing** — it is the width of a telemetry reading, and the pairing that
/// matters is that the concurrent leg is reported beside the serial one so the
/// two widths are comparable (J3 round-1 N3: this said "It sizes the *aggregate*
/// bound only — the per-lane bound comes from the serial leg", which was true of
/// the deleted estimator and is true of nothing now).
pub const PROBE_CONCURRENCY: usize = 4;

/// Embeds the probe throws away before it starts timing.
///
/// One, and it is the fix for J3-R1-2: the probe fires at session build, the
/// coldest moment in the process's life, and four consecutive runs of the same
/// binary against the same llama-server measured **21.2, 101.0, 150.2 and
/// 134.7 items/s** — a 7× swing in the load-bearing number, every one of them
/// reported as `measured: true`. A discarded first embed pays the model-load
/// cost out of the probe's budget instead of out of the measurement. It is not
/// the whole fix: the observed rate ([`OBSERVED_MIN_SAMPLES`]) is what makes
/// the durability property independent of *which* reading the probe caught.
pub const PROBE_WARMUP_EMBEDS: usize = 1;

/// Embeds one calibration probe performs in total: a discarded warm-up, **two**
/// timed alone (the serial legs, which are the width a lane drains at — one at
/// [`PROBE_TEXT`] and one at [`PROBE_TEXT_BYTES`], J3-R2-1), then
/// [`PROBE_CONCURRENCY`] together.
///
/// Seven rather than six. The seventh is the representative serial leg, and it
/// is best-effort: `probe_embedder` carries on with the short figure when it
/// fails, so the count is a budget input rather than a requirement.
pub const PROBE_EMBEDS: usize = PROBE_WARMUP_EMBEDS + 2 + PROBE_CONCURRENCY;

/// Real writes observed before their measured service time replaces the
/// probe's serial figure.
///
/// [`PROBE_CONCURRENCY`], so the observed rate never rests on fewer embeds than
/// the probe's own leg did. This is J3-R1-2's remediation (a): the worker
/// already has the timings, a lane is single-consumer so those timings *are*
/// serial service time, and it covers the whole of `WriteCtx::run` rather
/// than the embed alone. Once it takes over, a cold probe stops being a
/// sentence the whole session has to live with — including a probe that failed
/// outright and left the session with no rate telemetry at all (J3 round-1 N3:
/// "and floored the bound" named `WRITE_QUEUE_MIN`, which this branch deleted;
/// a failed probe now costs `write_queue_measured: false` and nothing else).
pub const OBSERVED_MIN_SAMPLES: u64 = PROBE_CONCURRENCY as u64;

/// EWMA weight for observed service time, as a divisor: the new sample gets
/// `1 / OBSERVED_EWMA_WEIGHT`.
///
/// [`PROBE_CONCURRENCY`] again, so the average moves most of the way in about
/// one probe's width of samples. A weight of 1 would make the published **rate**
/// track a single slow write and oscillate; a much larger one would keep a warm
/// figure long after the embedder degraded, which is the J3-R1-3 scenario. Both
/// are now telemetry faults rather than admission faults (J3 round-1 N3: this
/// said "would make the bound track a single slow write"), and the reason to
/// smooth is unchanged: an operator diagnosing a slow embedder needs a number
/// that means something, and a 1-weight rate means only "the last write".
pub const OBSERVED_EWMA_WEIGHT: u32 = PROBE_CONCURRENCY as u32;

/// Bound on the calibration probe — **all [`PROBE_EMBEDS`] of its embeds
/// together**.
///
/// The probe is *spawned*, not awaited, at session build — and since the J3
/// redesign **nothing else awaits it either**: admission uses the static caps,
/// so this budget prices only how long the telemetry may take to publish.
/// (Its earlier career as "the worst case an admission can wait" ended with
/// `await_calibration`.)
///
/// Unchanged at 5 s even though the probe takes seven embeds rather than
/// four, one of them at [`PROBE_TEXT_BYTES`]: a deployment too cold to answer
/// seven embeds in 5 s is better served by reporting `unmeasured` and being
/// **corrected by observation** ([`OBSERVED_MIN_SAMPLES`]) than by publishing
/// a number taken while its model was still loading.
///
/// The J3 redesign makes the trade almost free: the probe is telemetry, so a
/// blown budget costs `write_queue_measured: false` and an absent baseline for
/// [`Calibration::probe_optimism`] — never an admission. Measured warm at the
/// release binary against the live BGE-M3, all seven embeds land in ~180 ms of
/// the 5 s.
pub const PROBE_BUDGET: Duration = Duration::from_secs(5);

/// Text the probe's **short** leg embeds — 35 bytes, fixed, and chosen for one
/// property only: **every embedder accepts it.**
///
/// The sentence this used to carry, "it is measuring the deployment's embedder,
/// not its own input", was **false and load-bearing** (J3-R2-1). Input length is
/// a first-order determinant of a transformer's latency, so a rate measured on
/// 35 bytes is a rate for 35-byte writes and nothing else.
///
/// **No per-deployment measured table is stamped here (J3-R2R-6).** The table
/// this docstring once published — "1536 B and up — HTTP 500, 8 of 8" — did not
/// reproduce on the rig it named: re-measured, the refusal actually sat between
/// **2048 B and 3072 B**, and the latencies swung by up to 1.6×. A measured
/// table needs its conditions (the server's batch flags and a date), which is
/// exactly what a code docstring cannot stay in sync with. The *shape* of the
/// argument survives and is the part that is load-bearing:
///
/// * input length is first-order for a transformer's latency, so the short leg
///   measures the embedder on its shortest possible work, and
/// * an embedder has an input ceiling of **its own** (this llama-server refuses
///   somewhere above ~2 KiB on its configured batch), so a probe text large
///   enough to trip it would turn every probe into
///   [`Calibration::unmeasured`] — a worse outcome than an optimistic number.
///
/// So the short leg stays, always answerable, and [`PROBE_TEXT_BYTES`] is
/// measured **beside** it rather than instead of it. The honest reading of the
/// round-2 re-measurement (largest power of two under the smallest refusal) puts
/// the representative leg at 1024 B.
pub const PROBE_TEXT: &str = "lambo write queue calibration probe";

/// Size of the probe's **representative** leg, in bytes: 1024.
///
/// Two bounds pick this number, one from below and one from above.
///
/// * From below, the workload: lambo's own dogfood concepts — the `Logic` and
///   `Constraint` entries `lambo_recall` returns — run **700 to 1500 bytes**, so
///   a leg inside that band measures the embedder on the shape the product
///   actually writes.
/// * From above, the embedder: an embedder has an input ceiling of its own —
///   re-measured on this rig's llama-server, the refusal sits between **2048 B
///   and 3072 B** (see [`PROBE_TEXT`]; it moved off the old 1536 B claim, which
///   is why no code docstring stamps a verdict here). A representative leg must
///   stay clear of a limit it cannot know, so it takes the largest power of two
///   under the smallest refusal measured — 2048 would sit too close to a limit
///   that moved, so 1024 keeps a full power-of-two of headroom.
///
/// The leg is **best effort** on top of that: if this size is refused anyway,
/// `probe_embedder` keeps the short figure and says nothing it did not
/// measure. And the number it produces sizes nothing load-bearing — since the
/// J3 redesign, no probe number does. This leg makes
/// the probe's *published* rate honest, so that the probe-versus-observed
/// comparison in `lambo_stats` is a diagnosis rather than a pair of numbers that
/// disagree for a reason nobody recorded.
pub const PROBE_TEXT_BYTES: usize = 1024;

/// Build-time invariant: the representative leg must actually be bigger than the
/// short one, or the second leg measures the first thing twice and J3-R2-1's
/// diagnosis silently stops being measured.
const _: () = assert!(
    PROBE_TEXT_BYTES > PROBE_TEXT.len(),
    "PROBE_TEXT_BYTES must exceed PROBE_TEXT's own length — the representative leg exists to \
     measure the embedder on input LONGER than the short leg's",
);

/// Build-time invariant: [`probe_text_at`] truncates on a byte index, so the
/// text it repeats has to be ASCII or that truncation can land mid-character.
const _: () = assert!(
    is_ascii(PROBE_TEXT),
    "PROBE_TEXT must be ASCII — probe_text_at truncates by byte index to hit PROBE_TEXT_BYTES \
     exactly, which panics on a non-boundary index",
);

/// `true` when every byte of `s` is ASCII. A `const fn` because the assertion
/// above has to run at build time, and `str::is_ascii` is not const.
const fn is_ascii(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] >= 0x80 {
            return false;
        }
        i += 1;
    }
    true
}

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
/// [`MEASURED_LOCAL_EMBEDDER_RPS`] is one: it lets the relation below be a
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
/// declares retired. [`PROBE_CLAMP_RPS`] no longer derives from
/// [`WRITE_QUEUE_MAX`], so that chain is cut and the derivation stands on its
/// own two feet:
///
/// * **4096 is what the memory budget allows** — ≈ 31 MiB of worst-case
///   receipts, computed above, against a process that already holds an entire
///   session graph in RAM. A cost worth naming and paying.
/// * **[`WRITE_QUEUE_MAX`] is a quarter of it**, for the eviction-safety reason
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

/// Longest a caller may block waiting for its own write to apply.
///
/// Two drain budgets, resting on the one surviving reason (J3-R2R-5 — the
/// redesign retired admission "projections"): a close's quiesce drains for one
/// whole budget, so a wait of one budget would expire on jobs that quiesce is
/// still retiring. The second budget is that quiesce plus slack for the
/// caller's own job's service time. A wait that runs out answers `pending` —
/// which is one of the honest answers, not a failure.
pub const RECEIPT_WAIT_MAX: Duration = Duration::from_secs(4);

/// Build-time invariant: a wait shorter than the queue's own admission promise
/// would make the opt-in-synchrony surface useless by construction.
const _: () = assert!(
    RECEIPT_WAIT_MAX.as_secs() >= 2 * WRITE_QUEUE_DRAIN_BUDGET.as_secs(),
    "RECEIPT_WAIT_MAX must be at least twice WRITE_QUEUE_DRAIN_BUDGET — a close's quiesce drains \
     for one whole budget, so a wait of one budget would expire on jobs the quiesce is still \
     retiring",
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

/// Build-time invariant tying the two ceilings together, so neither can be
/// moved without the other being considered.
const _: () = assert!(
    MAX_CONCURRENT_RECEIPT_WAITS * 2 <= crate::mcp::proxy::INFLIGHT_DEPTH_WARN,
    "MAX_CONCURRENT_RECEIPT_WAITS must leave half of INFLIGHT_DEPTH_WARN for ordinary traffic — \
     a waiting lambo_stats(receipt=...) holds a proxy inflight slot, and answer_lost writes one un-raced \
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
    /// `crate::mcp::server::err_class` rather than a second match, and the
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
/// [`crate::mcp::server::err_class`] so the two cannot drift.
///
/// Every caller writes the raw error to the operator at the same site — a
/// `completion` ledger line and a `tracing::warn!` — so nothing is lost, only
/// relocated to the audience that can act on it.
fn model_safe_failure(err: &LamboError) -> String {
    format!(
        "{} (the detail was logged server-side)",
        crate::mcp::server::err_class(err)
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

/// Why a job was refused admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropReason {
    /// **This agent's own lane** is full: the per-agent fair-share cap
    /// ([`WRITE_QUEUE_LANE_MAX`]). The usual refusal.
    LaneFull,
    /// All lanes together are full: the whole-queue memory cap
    /// ([`WRITE_QUEUE_MAX`]).
    QueueFull,
    /// The payload-byte bound.
    QueueBytes,
    /// The session is closing or has lost its lease.
    Closed,
}

impl DropReason {
    /// The receipt's stated reason. **It must name the bound that actually
    /// refused, as what it actually is** — J3-R3-4 caught the previous text
    /// attributing every refusal to "a bound measured … on this deployment's
    /// embedder" in an era where a fixed ceiling was deciding; under the J3
    /// redesign the bounds are static shares and the text says so.
    fn describe(self, bound: usize) -> String {
        match self {
            DropReason::LaneFull => format!(
                "this agent's background write lane is full ({bound} outstanding — the per-agent \
                 fair-share cap, 1/{MAX_CONCURRENT_RECEIPT_WAITS} of the queue; wait for \
                 receipts to settle and resubmit)"
            ),
            DropReason::QueueFull => format!(
                "the background write queue is full ({bound} outstanding — the whole-queue \
                 memory cap; wait for receipts to settle and resubmit)"
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

/// Where the published rate came from (telemetry provenance — since the J3
/// redesign no rate sizes a bound).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CalibrationSource {
    /// Nothing measured: the probe failed or timed out. Reported as
    /// `write_queue_measured: false`.
    Unmeasured,
    /// The calibration probe, at session build.
    Probe,
    /// Real write service times, observed by the lane workers
    /// ([`OBSERVED_MIN_SAMPLES`]) — strictly better evidence than the probe,
    /// because it is this deployment doing this deployment's actual work.
    Observed,
}

impl CalibrationSource {
    /// Stable machine tag for `lambo_stats`' `write_queue_bound_source`.
    pub fn tag(self) -> &'static str {
        match self {
            CalibrationSource::Unmeasured => "unmeasured",
            CalibrationSource::Probe => "probe",
            CalibrationSource::Observed => "observed",
        }
    }
}

/// The measured rates (telemetry) beside the static bounds in force.
///
/// **Throughput, not latency, is what is measured** — the figure that motivated
/// a per-deployment probe is a *parallelism* figure (4 recalls: 380 ms
/// sequential, 64 ms concurrent, 5.94x — §Measurements), and the case the spec
/// names, a hosted embedder that is slower per call but parallelises far
/// better, inverts per-call latency while raising throughput. The probe
/// publishes a serial (1-wide) figure — the width one lane drains at, J3-R1-1
/// — and a [`PROBE_CONCURRENCY`]-wide one, at two input sizes, keeping the
/// slower serial reading (J3-R2-1); real write service times replace the
/// serial figure once [`OBSERVED_MIN_SAMPLES`] land, with the probe's kept
/// beside it for [`Calibration::probe_optimism`] (J3-R2-4).
///
/// **None of these rates sizes a bound** (the J3 redesign). Three rounds spent
/// deriving `lane_bound`/`bound` from these measurements — projected against a
/// budget share, clamped under per-source ceilings — and every round a new
/// workload covariate falsified the derivation (width, warmth, length, failure
/// shape, concurrency scaling). Durable intents carry the durability invariant
/// now, so [`Calibration::lane_bound`] and [`Calibration::bound`] are simply
/// [`WRITE_QUEUE_LANE_MAX`] and [`WRITE_QUEUE_MAX`], whatever the source: a
/// fairness share and a memory cap, reported here so `lambo_stats` shows the
/// bounds in force beside the rates that no longer produce them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Calibration {
    /// Measured **serial** (1-wide) items/second — the rate one lane's single
    /// consumer retires at.
    /// `None` when nothing measured it.
    pub serial_items_per_sec: Option<f64>,
    /// The **probe's** serial figure, kept even after
    /// [`Calibration::serial_items_per_sec`] has been replaced by an observed
    /// one (J3-R2-4).
    ///
    /// Destroying it destroyed the one self-diagnosing comparison the payload
    /// had: *"this deployment's real service time is 4× what the probe
    /// measured"* is the sentence that would have caught J3-R2-1 in
    /// `lambo_stats` instead of at a review's release-binary run, and it is
    /// only sayable while both numbers exist. `None` when no probe landed.
    pub probe_serial_items_per_sec: Option<f64>,
    /// Measured **concurrent** ([`PROBE_CONCURRENCY`]-wide) items/second.
    /// `None` when nothing measured it.
    pub items_per_sec: Option<f64>,
    /// Bound on one agent's lane.
    pub lane_bound: usize,
    /// Bound on all lanes together.
    pub bound: usize,
    /// Where [`Calibration::serial_items_per_sec`] came from.
    pub source: CalibrationSource,
}

impl Calibration {
    /// No measurement at all. Says so, rather than presenting a number it did
    /// not measure; the bounds are the same static ones as everywhere.
    pub fn unmeasured() -> Self {
        Self::from_rates(None, None, None, CalibrationSource::Unmeasured)
    }

    /// Publish the probe's timed legs: one embed **alone** at
    /// [`PROBE_TEXT`], optionally a second alone at [`PROBE_TEXT_BYTES`], then
    /// [`PROBE_CONCURRENCY`] embeds **together**.
    ///
    /// The serial rate published is the **slower** of the two serial legs
    /// (J3-R2-1): they differ only in input length, so the slower one is the
    /// rate for the larger workload, and an honest telemetry figure is the
    /// conservative one. `representative_wall` is `None` when that leg was
    /// refused — which is a real case, not a defensive one: this rig's
    /// llama-server answers 1280 B and refuses 1536 B.
    pub fn from_probe(
        serial_wall: Duration,
        representative_wall: Option<Duration>,
        concurrent_wall: Duration,
    ) -> Self {
        let short = rate_of(1, serial_wall);
        let serial = match representative_wall {
            Some(wall) => short.min(rate_of(1, wall)),
            None => short,
        };
        Self::from_rates(
            Some(serial),
            Some(rate_of(PROBE_CONCURRENCY, concurrent_wall)),
            Some(serial),
            CalibrationSource::Probe,
        )
    }

    /// Replace the serial rate with one observed from real writes, keeping the
    /// probe's concurrent figure **and its serial figure for the comparison**
    /// (J3-R2-4).
    ///
    /// The observed rate is better evidence about the drain than the probe's
    /// serial leg is — it times the whole of `WriteCtx::run` rather than the
    /// embed alone, on the caller's own content rather than the probe's, and it
    /// keeps tracking an embedder that degrades after startup — so it wins
    /// outright rather than being averaged in. Winning is not the same as
    /// erasing: the number it displaced stays on
    /// [`Calibration::probe_serial_items_per_sec`], because the *ratio* between
    /// them is the diagnosis.
    pub fn with_observed_serial(&self, serial_items_per_sec: f64) -> Self {
        Self::from_rates(
            Some(serial_items_per_sec),
            self.items_per_sec,
            self.probe_serial_items_per_sec,
            CalibrationSource::Observed,
        )
    }

    fn from_rates(
        serial: Option<f64>,
        concurrent: Option<f64>,
        probe_serial: Option<f64>,
        source: CalibrationSource,
    ) -> Self {
        // J3 redesign: the rates are published, never projected. The bounds
        // are the static fairness/memory caps for EVERY source — deriving
        // them from the rates is the retired estimator role (five falsified
        // axes; see the module doc and the deletion note at
        // WRITE_QUEUE_LANE_MAX).
        Self {
            serial_items_per_sec: serial,
            probe_serial_items_per_sec: probe_serial,
            items_per_sec: concurrent,
            lane_bound: WRITE_QUEUE_LANE_MAX,
            bound: WRITE_QUEUE_MAX,
            source,
        }
    }

    /// `true` when the bounds rest on a measurement of this deployment's own
    /// embedder — by probe or by observation.
    pub fn measured(&self) -> bool {
        self.source != CalibrationSource::Unmeasured
    }

    /// How many times faster the probe's serial leg read than the rate now in
    /// force, or `None` when there is no pair to compare (J3-R2-4).
    ///
    /// Above one means the probe was **optimistic about this deployment's own
    /// work** — the direction that, while an estimator still gated durability,
    /// abandoned writes (4.0× is what J3-R2-1 measured at the release binary).
    /// Far *below* one is just as diagnostic: an observed rate implausibly
    /// faster than the probe's embed-only figure is evidence of non-work being
    /// sampled, which is how J3-R3-1 read an impossible 0.02–0.05. Both
    /// directions are telemetry now — nothing acts on the ratio, and nothing
    /// needs to: no estimate gates durability any more. The ratio is logged
    /// once when the source flips so a field session says it out loud rather
    /// than leaving it to be derived from two keys.
    pub fn probe_optimism(&self) -> Option<f64> {
        match (self.probe_serial_items_per_sec, self.serial_items_per_sec) {
            (Some(probe), Some(now)) if now > 0.0 && self.source == CalibrationSource::Observed => {
                Some(probe / now)
            }
            _ => None,
        }
    }
}

/// `n` items in `wall`, as items/second. A zero or absurd wall time is the
/// [`crate::FixtureEmbedder`] case; the clamp is what handles it, so this must
/// not divide by zero first.
fn rate_of(n: usize, wall: Duration) -> f64 {
    let secs = wall.as_secs_f64();
    if secs <= 0.0 {
        PROBE_CLAMP_RPS as f64
    } else {
        n as f64 / secs
    }
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
    /// [`EMBEDDER_SICK_THRESHOLD`] — it is presumed sick. This is "wedged",
    /// not "draining".
    Embedder,
    /// A non-embedder, session-wide fault (a store error, a lost lease, a
    /// config error) ended the loop.
    Other,
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
    /// Refusals because the session was closing or fenced — **a third drop
    /// class, not a subtraction** (J3-R1-8). It rides its own counter and is
    /// summed into [`WriteQueueCounters::dropped`], so no count vanishes and
    /// the gauge's exclusivity argument is untouched: like the other two, a
    /// refusal never enters `accepted`. Split out because
    /// `write_queue_dropped` is the key the operator reads for "a burst
    /// degraded", and "the embedder is the bottleneck" and "the session is
    /// shutting down and refused a tail" want opposite responses.
    dropped_closed: AtomicU64,
    /// Acked writes a clean `close()` did **not** apply and did not lose:
    /// their durable intents survive the close and the next serve of the
    /// session applies them (J3 durable intents). A fourth settle class beside
    /// `applied`/`failed` — never a subset of either — because "your write
    /// will happen, later, in another process" is neither a success nor a
    /// failure and must not be counted as one.
    deferred: AtomicU64,
    /// Durable intents from a **previous** process that this session's replay
    /// applied at attach. Not summed into `applied` (those count this
    /// session's own accepted jobs; a replayed intent was never accepted
    /// here), so `outstanding` stays exact.
    replayed: AtomicU64,
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
    replay_owed: AtomicU64,
    /// The reason the last replay stopped without draining (J3-R2R-8), or
    /// [`ReplayBlockReason::None`] when it drained / was never owed. Renders as
    /// the `write_queue_replay_blocked` stat: `null` = draining or idle,
    /// `"embedder"` = sick/wedged, `"other"` = store/lease/config.
    replay_blocked: AtomicU8,
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
            _ => ReplayBlockReason::Other,
        }
    }
    pub(crate) fn set_replay_blocked(&self, reason: ReplayBlockReason) {
        let disc = match reason {
            ReplayBlockReason::None => 0,
            ReplayBlockReason::Embedder => 1,
            ReplayBlockReason::Other => 2,
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

    /// The durable form of this job, exactly as validated (J3 intents).
    fn to_intent_payload(&self) -> WriteIntentPayload {
        match self {
            JobPayload::Derive { concepts, pairs } => WriteIntentPayload::Derive {
                concepts: concepts.clone(),
                pairs: pairs.clone(),
            },
            JobPayload::Action {
                action,
                produces,
                modifies,
                depends_on,
            } => WriteIntentPayload::Action {
                action: action.clone(),
                produces: produces.clone(),
                modifies: modifies.clone(),
                depends_on: depends_on.clone(),
            },
        }
    }

    /// Rehydrate a job payload from its durable form (replay).
    fn from_intent_payload(p: WriteIntentPayload) -> Self {
        match p {
            WriteIntentPayload::Derive { concepts, pairs } => {
                JobPayload::Derive { concepts, pairs }
            }
            WriteIntentPayload::Action {
                action,
                produces,
                modifies,
                depends_on,
            } => JobPayload::Action {
                action,
                produces,
                modifies,
                depends_on,
            },
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
    /// J4. An optional call ledger this pipeline appends durable-intent
    /// **completion** lines to (`applied` / `applied_after_restart` / `failed`
    /// / `deferred`), on the same [`crate::ledger::Ledger::append`] path every
    /// other line rides. `None` for every process run without `--ledger`, and
    /// for every non-serve writer.
    pub(crate) ledger: Option<Arc<crate::ledger::Ledger>>,
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

/// When — and as what — a job's durable intent is consumed at commit (J3).
///
/// `tag` is `"applied"` when the acking process itself applies the job, and
/// `"applied_after_restart"` when a later serve replays it. `at` is the
/// consumer's clock at job start; it stamps [`WriteIntentOutcome::consumed_at`]
/// and therefore starts the consumed row's retention window.
#[derive(Clone, Copy)]
pub(crate) struct ConsumeStamp {
    tag: &'static str,
    at: DateTime<Utc>,
}

/// The receipt sentence for an applied derive — one function, because the
/// receipt's copy and the durable intent outcome's copy (written inside the
/// commit lock by the [`hybrid::CommitHook`]) must be the same sentence.
///
/// Applied ≠ embedded (J3-R3-1): only the hybrid strategy can embed, so only
/// it reports the count — and its sentence carries the number, so a write
/// applied without its vector is never read as an unqualified success.
fn derive_sentence(
    strategy: MatchStrategy,
    submitted: usize,
    outcome: &crate::graph::derive::DeriveOutcome,
) -> String {
    match strategy {
        MatchStrategy::Hybrid => format!(
            "derived {} concept(s): {} created ({} embedded), {} matched existing",
            submitted,
            outcome.created.len(),
            outcome.embedded,
            outcome.matched.len()
        ),
        MatchStrategy::Canonical => format!(
            "derived {} concept(s): {} created, {} matched existing",
            submitted,
            outcome.created.len(),
            outcome.matched.len()
        ),
    }
}

/// The receipt sentence for an applied `record_action` — shared with the
/// intent outcome for [`derive_sentence`]'s reason.
///
/// Under the hybrid strategy `record_action` embeds the concepts it creates
/// (`embed_action_contents`), so its sentence carries the count exactly as
/// [`derive_sentence`]'s does; under `Canonical` nothing embeds and the
/// sentence is unchanged.
fn action_sentence(
    strategy: MatchStrategy,
    outcome: &crate::graph::action::ActionOutcome,
) -> String {
    match strategy {
        MatchStrategy::Hybrid => format!(
            "recorded action: {} concept(s) created ({} embedded), {} edge(s)",
            outcome.created.len(),
            outcome.embedded,
            outcome.edges
        ),
        MatchStrategy::Canonical => format!(
            "recorded action: {} concept(s) created, {} edge(s)",
            outcome.created.len(),
            outcome.edges
        ),
    }
}

impl WriteCtx {
    /// Run one job through the ordinary write path.
    ///
    /// `consume`, when present, consumes the job's durable intent **inside the
    /// same write-lock critical section as the commit** (J3): for the hybrid
    /// path via [`hybrid::CommitHook`], for the canonical and action paths
    /// inline under the guard the graph write already holds. The flush drain
    /// takes that same lock, so the applied mutations and the
    /// `ConsumeWriteIntent` always travel in one batch — one store
    /// transaction — and a crash can never leave the write durable beside a
    /// still-unconsumed intent (the double-apply this design excludes).
    async fn run(
        &self,
        job: &Job,
        consume: Option<ConsumeStamp>,
    ) -> Result<AppliedSummary, LamboError> {
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
                let strategy = self.match_strategy;
                let submitted = borrowed.len();
                let outcome = match strategy {
                    MatchStrategy::Hybrid => {
                        let on_commit: Option<hybrid::CommitHook> = consume.map(|stamp| {
                            let receipt = job.receipt.to_string();
                            Box::new(
                                move |g: &mut Graph,
                                      outcome: &crate::graph::derive::DeriveOutcome| {
                                    g.consume_write_intent(
                                        receipt,
                                        WriteIntentOutcome {
                                            tag: stamp.tag.into(),
                                            summary: derive_sentence(strategy, submitted, outcome),
                                            consumed_at: stamp.at,
                                        },
                                    );
                                },
                            ) as hybrid::CommitHook
                        });
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
                            on_commit,
                        )
                        .await?
                    }
                    MatchStrategy::Canonical => {
                        let mut g = self.graph.write();
                        let outcome = graph_derive(
                            &mut g,
                            job.interaction,
                            &job.agent,
                            &borrowed,
                            &parent_of,
                            self.max_cooccurrence_per_derive,
                        )?;
                        // Same lock hold as the commit — see `run`'s doc.
                        if let Some(stamp) = consume {
                            g.consume_write_intent(
                                job.receipt.to_string(),
                                WriteIntentOutcome {
                                    tag: stamp.tag.into(),
                                    summary: derive_sentence(strategy, submitted, &outcome),
                                    consumed_at: stamp.at,
                                },
                            );
                        }
                        outcome
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
                let embedded = match strategy {
                    MatchStrategy::Hybrid => Some(outcome.embedded),
                    MatchStrategy::Canonical => None,
                };
                Ok(AppliedSummary {
                    kind: WriteKind::Derive,
                    summary: derive_sentence(strategy, submitted, &outcome),
                    created,
                    matched,
                    created_count: outcome.created.len(),
                    matched_count: outcome.matched.len(),
                    semantic_merged: Some(outcome.semantic_merged.len()),
                    reinforced: Some(outcome.reinforced),
                    edges: None,
                    embedded,
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
                let act = Action {
                    event_time: None,
                    action: action.as_str(),
                    produces: &p,
                    modifies: &m,
                    depends_on: &d,
                };
                // Embed BEFORE the write lock: these are model calls, and the
                // commit below is sync. A failure here fails the job with
                // nothing written, exactly as an embed failure fails a derive.
                //
                // Gated on the strategy for the same reason `derive` is: under
                // `Canonical` there is no vector leg at all, and embedding here
                // would stamp a contract on a session that asked for none. And
                // on the store's VECTOR_SEARCH, as hybrid `derive` is: a store
                // that cannot search vectors keeps none, so the write is
                // keyword-only and its receipt says "(0 embedded)".
                let vector_search = self
                    .store
                    .capabilities()
                    .contains(crate::store::Capabilities::VECTOR_SEARCH);
                let embeddings = match self.match_strategy {
                    MatchStrategy::Hybrid if vector_search => {
                        crate::graph::action::embed_action_contents(self.embedder.as_ref(), &act)
                            .await?
                    }
                    MatchStrategy::Hybrid | MatchStrategy::Canonical => ActionEmbeddings::new(),
                };
                let outcome = {
                    let mut g = self.graph.write();
                    // Stamp the space this call's vectors live in, or verify the
                    // stamp already there. Same rule as `hybrid::derive`'s
                    // commit phase: a vector must never land in a session with
                    // no declared contract.
                    if !embeddings.is_empty() {
                        g.stamp_embedding(self.embedding.clone())?;
                    }
                    let outcome = graph_record_action_embedded(
                        &mut g,
                        job.interaction,
                        &job.agent,
                        &act,
                        &embeddings,
                    )?;
                    // Same lock hold as the commit — see `run`'s doc.
                    if let Some(stamp) = consume {
                        g.consume_write_intent(
                            job.receipt.to_string(),
                            WriteIntentOutcome {
                                tag: stamp.tag.into(),
                                summary: action_sentence(self.match_strategy, &outcome),
                                consumed_at: stamp.at,
                            },
                        );
                    }
                    outcome
                };
                let mut touched = outcome.created.clone();
                touched.push(outcome.action_node);
                mirror_concepts(&self.graph, &self.index, &touched);
                self.daemon_wake.notify_one();
                let created = truncate_ids(&outcome.created);
                Ok(AppliedSummary {
                    kind: WriteKind::RecordAction,
                    summary: action_sentence(self.match_strategy, &outcome),
                    created,
                    matched: Vec::new(),
                    created_count: outcome.created.len(),
                    matched_count: 0,
                    semantic_merged: None,
                    reinforced: None,
                    edges: Some(outcome.edges),
                    // Hybrid embeds what it creates (see the embed hop above);
                    // Canonical never does, so the count is absent there, not
                    // zero, for the reason on the field.
                    embedded: match self.match_strategy {
                        MatchStrategy::Hybrid => Some(outcome.embedded),
                        MatchStrategy::Canonical => None,
                    },
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
    /// When the answer became terminal, and therefore when
    /// [`RECEIPT_RETENTION`] starts. `None` while the write is queued or
    /// running — and an entry with `None` here is **never** expired and never
    /// evicted (J3-R1-3).
    settled_at: Option<DateTime<Utc>>,
    answer: ReceiptAnswer,
}

impl Entry {
    /// Settle this entry, stamping the retention clock. Returns `false` when it
    /// was already settled, so no outcome can be overwritten by a later sweep.
    fn settle(&mut self, answer: ReceiptAnswer, now: DateTime<Utc>) -> bool {
        if self.answer.is_settled() {
            return false;
        }
        self.answer = answer;
        self.settled_at = Some(now);
        true
    }
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
    /// Drop **settled** entries whose retention window has passed, oldest
    /// first.
    ///
    /// **An unsettled entry is skipped, never expired** (J3-R1-3). Nothing caps
    /// how long a job sits in a lane, so the old issue-time sweep could answer
    /// `expired` about a job that was still running — measured, with
    /// `outstanding = 1` — after which [`settle_one`] discarded its outcome
    /// because it swept before it settled. Skipped ids are pushed back in
    /// order, so `order` stays issue-ordered and the sweep stays O(popped).
    fn expire(&mut self, now: DateTime<Utc>) {
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
    fn evict(&mut self) {
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
    /// The same count per lane. At most one per lane today (one consumer), but
    /// kept as a count so it stays correct if a lane ever gets more, and
    /// maintained at exactly the two sites that move `running`.
    running_per_lane: HashMap<AgentId, usize>,
    /// `true` once the pipeline refuses admission (closing, or fenced).
    sealed: bool,
}

/// Serial write service time, as an EWMA over real writes.
///
/// A lane has one consumer, so the time [`WriteCtx::run`] takes on it **is**
/// serial service time — better evidence about the drain than the probe's
/// embed-only leg, and it keeps tracking an embedder that degrades after
/// startup rather than freezing the first reading of the session (J3-R1-2).
#[derive(Debug, Default)]
struct ObservedRate {
    mean_secs: f64,
    samples: u64,
}

impl ObservedRate {
    fn sample(&mut self, secs: f64) {
        if !secs.is_finite() || secs < 0.0 {
            return;
        }
        self.samples = self.samples.saturating_add(1);
        if self.samples == 1 {
            self.mean_secs = secs;
            return;
        }
        let weight = 1.0 / OBSERVED_EWMA_WEIGHT as f64;
        self.mean_secs = self.mean_secs * (1.0 - weight) + secs * weight;
    }

    /// items/second, or `None` while the sample count is still under
    /// [`OBSERVED_MIN_SAMPLES`] — until then the probe's figure stands.
    fn items_per_sec(&self) -> Option<f64> {
        if self.samples < OBSERVED_MIN_SAMPLES {
            return None;
        }
        if self.mean_secs <= 0.0 {
            // A fixture-fast deployment. The clamp is what handles it.
            return Some(PROBE_CLAMP_RPS as f64);
        }
        Some(1.0 / self.mean_secs)
    }
}

impl Lanes {
    fn outstanding(&self) -> usize {
        self.queued + self.running
    }

    /// Outstanding jobs **on one lane** — queued plus the one being run by that
    /// lane's own consumer. This is the population [`WRITE_QUEUE_LANE_MAX`]
    /// bounds, and the reason a global gauge could not do the job: the drain is
    /// per-lane and serial (J3-R1-1), which is also why the round-3 defect —
    /// a ceiling derived per-lane and enforced across lanes — was possible at
    /// all. (J3 round-1 N3: this named `Calibration::lane_bound`, which since the
    /// estimator demotion is a field that copies the static constant rather than
    /// a bound of its own.)
    fn lane_outstanding(&self, agent: &AgentId) -> usize {
        self.queues.get(agent).map_or(0, VecDeque::len)
            + self.running_per_lane.get(agent).copied().unwrap_or(0)
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

    /// The calibration in force: the probe's, with its serial rate replaced by
    /// the observed one once enough real writes have been seen.
    ///
    /// `None` only before either has anything to say. The observed leg can
    /// stand alone — a probe that failed publishes nothing, and a session must
    /// still be able to earn a real bound after starting on the floor.
    pub fn calibration(&self) -> Option<Calibration> {
        let probe = *self.calibration.borrow();
        let observed = self.observed.lock().items_per_sec();
        let calibration = match (probe, observed) {
            (Some(probe), Some(rate)) => Some(probe.with_observed_serial(rate)),
            (Some(probe), None) => Some(probe),
            (None, Some(rate)) => Some(Calibration::unmeasured().with_observed_serial(rate)),
            (None, None) => None,
        };
        if let Some(c) = calibration {
            self.log_observed_takeover(&c);
        }
        calibration
    }

    /// Log the probe → observed transition once, with **both** rates and the
    /// ratio between them (J3-R2-4).
    ///
    /// This is the line that would have made J3-R2-1 self-reporting in the
    /// field: an operator who abandoned writes at a clean close had
    /// `bound_source: observed` and the observed rate, and no way to learn that
    /// the burst had been admitted at four times that figure. Both numbers are
    /// in `lambo_stats` now as well; this puts the comparison in the log where
    /// nobody has to be looking at the right moment to see it.
    fn log_observed_takeover(&self, calibration: &Calibration) {
        if calibration.source != CalibrationSource::Observed
            || self.observed_logged.swap(true, Ordering::Relaxed)
        {
            return;
        }
        tracing::info!(
            session = %self.ctx.session,
            probe_serial_items_per_sec = calibration.probe_serial_items_per_sec,
            observed_serial_items_per_sec = calibration.serial_items_per_sec,
            probe_optimism = calibration.probe_optimism(),
            lane_bound = calibration.lane_bound,
            bound = calibration.bound,
            samples = OBSERVED_MIN_SAMPLES,
            "write queue: this deployment's own writes have replaced the startup probe's serial \
             figure. probe_optimism is how many times faster the probe read than the real work; \
             far from one in either direction means the probe and the workload disagree — \
             telemetry only, since durable intents carry the close-drain invariant"
        );
    }

    fn bound_snapshot(&self) -> usize {
        WRITE_QUEUE_MAX
    }

    /// Outstanding jobs — queued plus running.
    pub fn outstanding(&self) -> usize {
        self.lanes.lock().outstanding()
    }

    /// Receipts currently held.
    pub fn receipts_retained(&self) -> usize {
        self.receipts.lock().entries.len()
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
    ///
    /// Admission is instant since the J3 redesign — the bounds are the static
    /// fairness/memory caps, so there is no calibration to await. (The old
    /// `await_calibration` blocked the first burst on the probe for up to
    /// [`PROBE_BUDGET`] because "a provisional constant is the constant the
    /// spec forbids"; with durability carried by durable intents, a constant
    /// is exactly what a fairness share should be, and the probe is telemetry
    /// nobody has to wait for.)
    async fn admit(&self, agent: AgentId, interaction: NodeId, payload: JobPayload) -> Submitted {
        let bytes = payload.bytes();
        let receipt = self.next_receipt();
        let kind = payload.kind();
        let now = (self.clock)();

        // Both count conditions, and the per-lane one first because it is the
        // one that binds in the case J3-R1-1 measured: one busy agent, whose
        // single-consumer lane drains 1-wide however wide the deployment's
        // embedder is.
        //
        // Lock order: **graph write, then lanes** — held together across the
        // accept branch so the durable intent is in the mutation log *before*
        // the job is visible to a worker. A worker pops under the lanes lock
        // and consumes under the graph lock, so with both held here the log
        // can never carry a `ConsumeWriteIntent` ahead of its
        // `PutWriteIntent`. No other site nests these two locks (workers take
        // them strictly in sequence), so the order cannot deadlock.
        let refusal = {
            let mut graph = self.ctx.graph.write();
            let mut lanes = self.lanes.lock();
            if lanes.sealed {
                Some((DropReason::Closed, WRITE_QUEUE_LANE_MAX))
            } else if lanes.lane_outstanding(&agent) >= WRITE_QUEUE_LANE_MAX {
                Some((DropReason::LaneFull, WRITE_QUEUE_LANE_MAX))
            } else if lanes.outstanding() >= WRITE_QUEUE_MAX {
                Some((DropReason::QueueFull, WRITE_QUEUE_MAX))
            } else if lanes.bytes.saturating_add(bytes) > WRITE_QUEUE_MAX_BYTES {
                Some((DropReason::QueueBytes, WRITE_QUEUE_MAX))
            } else {
                // J3 durable intents: the ack's other half. Recorded at
                // admission, through the ordinary write-behind log, so the
                // close-time final flush ("session closed, tail durable")
                // carries it — acked ⇒ (applied ∨ durable intent) at a clean
                // close by construction, independent of any drain estimate.
                graph.record_write_intent(WriteIntent {
                    session_id: self.ctx.session.clone(),
                    receipt: receipt.to_string(),
                    agent: agent.clone(),
                    interaction,
                    lane_seq: receipt.seq(),
                    issued_ms: receipt.issued_ms(),
                    payload: payload.to_intent_payload(),
                    created_at: now,
                    outcome: None,
                });
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
            Some((reason, bound)) => {
                match reason {
                    DropReason::QueueBytes => {
                        self.counters
                            .dropped_queue_bytes
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    // A refusal because the session is closing is still a
                    // refusal by a bound and its count must not vanish — but it
                    // gets its **own** counter rather than riding the count
                    // bound's (J3-R1-8): `write_queue_dropped` is what an
                    // operator reads for "a burst degraded", and a refused
                    // shutdown tail is a different diagnosis with a different
                    // response. Both are summed into `dropped()`.
                    DropReason::Closed => {
                        self.counters.dropped_closed.fetch_add(1, Ordering::Relaxed);
                    }
                    DropReason::LaneFull | DropReason::QueueFull => {
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
                        lane_bound = WRITE_QUEUE_LANE_MAX,
                        bound = WRITE_QUEUE_MAX,
                        "write queue: dropping writes — {}. This message is logged once; the \
                         running count is lambo_stats' write_queue_dropped",
                        reason.describe(bound)
                    );
                }
                let answer = ReceiptAnswer::Dropped(reason.describe(bound));
                receipts.entries.insert(
                    receipt,
                    Entry {
                        agent: agent.clone(),
                        // A refusal is born settled, so its retention window
                        // starts now.
                        settled_at: Some(now),
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
                        // Unsettled, and therefore never expired and never
                        // evicted until it settles (J3-R1-3).
                        settled_at: None,
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
        let observed = self.observed.clone();
        tokio::spawn(async move {
            loop {
                let job = {
                    let mut l = lanes.lock();
                    match l.queues.get_mut(&agent).and_then(VecDeque::pop_front) {
                        Some(job) => {
                            l.queued -= 1;
                            l.bytes = l.bytes.saturating_sub(job.bytes);
                            l.running += 1;
                            *l.running_per_lane.entry(agent.clone()).or_insert(0) += 1;
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
                            l.running_per_lane.remove(&agent);
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
                    // The durable intent is deliberately NOT consumed here: it
                    // is not ours to consume any more. If it flushed before
                    // the fence, the session's current holder replays it
                    // (fenced flushes are refused at the store, so this
                    // process can neither apply nor consume it now); if it
                    // never flushed, it dies with this process's tail.
                    //
                    // JE2E-12: this arm hand-writes its own message and always
                    // did, which is why the finding did not reach it — it is
                    // lambo's own text about lambo's own lease, with nothing of
                    // the environment in it. It carries the same string twice
                    // because the model and the operator want the same words
                    // here; the split exists for the arms where they do not.
                    let fenced = format!(
                        "this handle lost its single-writer lease before the write was applied; \
                         this process wrote nothing for session {} — if the write's durable \
                         intent reached the store first, the session's current holder will \
                         apply it",
                        ctx.session
                    );
                    Err((fenced.clone(), fenced))
                } else {
                    // Timed, because this lane is single-consumer: the wall
                    // clock around one `run` *is* this deployment's serial
                    // service time, embedder warmth and all (J3-R1-2) — the
                    // figure `write_queue_serial_items_per_sec` publishes. It
                    // feeds no admission decision (J3 round-1 N3: this said "the
                    // serial service time the admission bound needs"); what it
                    // is *for* is that an operator watching a slowing embedder
                    // sees the deployment's own writes, not a startup estimate.
                    // A fenced refusal above is deliberately not sampled — it
                    // never entered `run`, and calling it a fast write would
                    // bias the rate upward, which is the dangerous direction.
                    //
                    // **And neither is a failure** (J3-R2-2): the same argument
                    // reaches one step further than it was taken. A write that
                    // fails — embedder error, HYBRID_IO_TIMEOUT, a store error —
                    // fails FAST, and sampling it says "this deployment retires
                    // work quickly" on the evidence of work it did not retire.
                    // Measured on this rig: llama returns HTTP 500 in ~2 ms for
                    // an input it refuses, which is 30x faster than a write it
                    // accepts. The hazard is the recovery, not the outage: an
                    // embedder that fast-fails a burst inflates the bound, then
                    // comes back and services the inflated queue at its real
                    // rate. Only work that went all the way through the pipeline
                    // is evidence about the pipeline.
                    let started = tokio::time::Instant::now();
                    let stamp = ConsumeStamp {
                        tag: "applied",
                        at: (clock)(),
                    };
                    let raw_outcome = ctx.run(&job, Some(stamp)).await;
                    if raw_outcome.is_ok() {
                        observed.lock().sample(started.elapsed().as_secs_f64());
                    } else {
                        // J3-R2R-2 (in-session symmetry). A failed job's intent
                        // must be settled — `run` consumes only at a commit, and
                        // a failure has no commit to ride — but WHICH failures
                        // consume it is the question this arm answers to match
                        // the replay arm. A content refusal
                        // (`LamboError::Embed`) is settled `failed` and consumed,
                        // exactly as the replay arm does. An embedder **outage**
                        // (`LamboError::EmbedUnavailable`) is NOT: it says
                        // nothing was learned about this *input*, so destroying
                        // the durable intent over it would recreate N1's loss on
                        // the path that handles almost every write. Instead the
                        // intent is left unconsumed, and the next serve of the
                        // session re-attempts it. The receipt still says `failed`
                        // (nothing was written by this process), and the deferred
                        // J4 seam is to re-queue with backoff or keep leaving it
                        // unconsumed for replay.
                        if let Err(e) = &raw_outcome {
                            if !matches!(e, LamboError::EmbedUnavailable(_)) {
                                ctx.graph.write().consume_write_intent(
                                    job.receipt.to_string(),
                                    WriteIntentOutcome {
                                        // JE2E-12: the intent row IS the durable
                                        // half of the receipt store — a restart
                                        // answers `failed` straight out of this
                                        // `summary` — so what it holds must be
                                        // what a receipt may say. The operator's
                                        // copy is the completion line and the
                                        // WARN below, both written from `detail`.
                                        tag: "failed".into(),
                                        summary: model_safe_failure(e),
                                        consumed_at: (clock)(),
                                    },
                                );
                            }
                        }
                    }
                    // Two strings from here on: the class for the model, the
                    // error for the operator (JE2E-12).
                    raw_outcome.map_err(|e| (model_safe_failure(&e), e.to_string()))
                };

                // No `.await` from here to the end of the iteration: an
                // `abort()` cannot land between a completed graph write and the
                // settle that reports it, so "aborted" always means "not
                // written".
                {
                    let mut l = lanes.lock();
                    l.running -= 1;
                    if let Some(n) = l.running_per_lane.get_mut(&agent) {
                        *n = n.saturating_sub(1);
                    }
                }
                let answer = match outcome {
                    Ok(summary) => {
                        counters.applied.fetch_add(1, Ordering::Relaxed);
                        // J4 proof obligation 5: a closed acked write's applied
                        // lifecycle is measurable — carry the metric-2 fact set,
                        // which is exactly what a durable-intent replay hid.
                        if let Some(ledger) = &ctx.ledger {
                            ledger.append(&crate::ledger::completion_line(
                                &job.agent.to_string(),
                                &job.receipt.to_string(),
                                "applied",
                                Some(json!({
                                    "created_count": summary.created_count,
                                    "matched_count": summary.matched_count,
                                })),
                            ));
                        }
                        ReceiptAnswer::Applied(summary)
                    }
                    Err((why, detail)) => {
                        counters.failed.fetch_add(1, Ordering::Relaxed);
                        // JE2E-12: the ledger line is operator-facing JSONL and
                        // gets the raw error; the receipt is model-facing and
                        // gets the class. Same fact, two audiences, one site —
                        // which is what keeps the detail from being lost rather
                        // than merely hidden.
                        if let Some(ledger) = &ctx.ledger {
                            ledger.append(&crate::ledger::completion_line(
                                &job.agent.to_string(),
                                &job.receipt.to_string(),
                                "failed",
                                Some(json!({ "error": &detail })),
                            ));
                        }
                        tracing::warn!(
                            session = %ctx.session,
                            agent = %job.agent,
                            receipt = %job.receipt,
                            error = %detail,
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
    /// capped ([`MAX_CONCURRENT_RECEIPT_WAITS`]); both bounds exist because a
    /// waiting call occupies a proxy in-flight slot for its whole duration.
    /// A wait that runs out returns [`ReceiptAnswer::Pending`] for a receipt
    /// this process holds, or [`ReceiptAnswer::PendingReplay`] for a replay-owed
    /// id — either is honest, and neither is a failure.
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
    /// **deferred, not lost** (J3 durable intents): workers are aborted and
    /// joined (aborting alone proves nothing — the R3-1 lesson), every
    /// still-pending receipt is settled `intent_durable`, and the count lands
    /// in `lambo_stats` as `write_queue_deferred`. The jobs themselves were
    /// recorded as durable intents at admission and the close's final flush —
    /// which runs AFTER this quiesce — persists them; the next serve of the
    /// session applies them in order. Acked ⇒ (applied ∨ durable intent) at a
    /// clean close, **by construction**, whatever any drain estimate said.
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
        let deferred = self.abort_workers().await;
        if deferred > 0 {
            tracing::warn!(
                session = %self.ctx.session,
                deferred,
                "write queue: {deferred} acked write(s) did not drain within {:?} of close(); \
                 their durable intents survive the close and the next serve of this session \
                 applies them — receipts say intent_durable",
                WRITE_QUEUE_DRAIN_BUDGET
            );
        }
        deferred
    }

    /// Stop every worker and settle whatever is left as `intent_durable`.
    /// Returns how many receipts this deferred to the next serve.
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
        // running, so any receipt still `Pending` names a write this process
        // will not apply — including the ones that were still queued. Every
        // such job has a durable intent (recorded at admission, in the log the
        // close's final flush persists), so the honest settle is
        // `intent_durable`, not `failed`: the write is deferred to the next
        // serve of this session, not lost.
        let mut deferred = 0usize;
        let now = (self.clock)();
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
                    if entry.settle(ReceiptAnswer::IntentRecorded, now) {
                        let agent = entry.agent.clone();
                        // J4 proof obligation 5: a close-deferred intent is a
                        // lifecycle fact (admitted → deferred), measurable.
                        if let Some(ledger) = &self.ctx.ledger {
                            ledger.append(&crate::ledger::completion_line(
                                &agent.to_string(),
                                &id.to_string(),
                                "deferred",
                                Some(json!({ "reason": "close_drain_exceeded" })),
                            ));
                        }
                        r.undelivered.entry(agent).or_default().push_back(*id);
                        deferred += 1;
                    }
                }
            }
        }
        if deferred > 0 {
            self.counters
                .deferred
                .fetch_add(deferred as u64, Ordering::Relaxed);
        }
        {
            let mut lanes = self.lanes.lock();
            lanes.running = 0;
            lanes.running_per_lane.clear();
            lanes.queued = 0;
            lanes.bytes = 0;
        }
        self.settled.notify_waiters();
        deferred
    }

    /// Abort the calibration probe. Called from `Memory`'s `Drop` and from
    /// `close()`: a probe outliving its session is an embed nobody will read.
    pub(crate) fn abort_probe(&self) {
        if let Some(handle) = self.probe.lock().take() {
            handle.abort();
        }
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
                            crate::mcp::server::err_class(&LamboError::Embed(e.clone()))
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

    /// Abort the workers without awaiting them — the `Drop` path, which cannot
    /// await. Receipts are not settled here: a dropped `Memory` never flushes
    /// its tail either, and a process that is going away has nobody to answer.
    pub(crate) fn abort_all_sync(&self) {
        self.abort_probe();
        self.abort_replay_sync();
        let mut lanes = self.lanes.lock();
        lanes.sealed = true;
        for (_, handle) in lanes.workers.drain() {
            handle.abort();
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
fn settle_one(
    receipts: &PlMutex<Receipts>,
    id: &ReceiptId,
    answer: ReceiptAnswer,
    now: DateTime<Utc>,
) {
    let mut r = receipts.lock();
    if let Some(entry) = r.entries.get_mut(id) {
        if entry.settle(answer, now) {
            let agent = entry.agent.clone();
            r.undelivered.entry(agent).or_default().push_back(*id);
        }
    }
    r.expire(now);
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

/// Measure the deployment's embedder in three legs, all inside one
/// [`PROBE_BUDGET`]:
///
/// 1. **Warm-up** — [`PROBE_WARMUP_EMBEDS`] embeds, timed and **thrown away**.
///    The probe fires at session build, the coldest moment in the process's
///    life; four consecutive runs of the same binary against the same
///    llama-server measured 21.2 → 150.2 items/s, a 7× swing, every one of them
///    reported as a measurement (J3-R1-2).
/// 2. **Serial, short** — one embed of [`PROBE_TEXT`] **alone**, wall-clocked.
///    This is the width a lane drains at, so it is what
///    `write_queue_serial_items_per_sec` reports (J3-R1-1). It is projected into
///    nothing: `project()` was deleted with the estimator, and
///    [`Calibration::lane_bound`] copies [`WRITE_QUEUE_LANE_MAX`] for every
///    source including `Unmeasured` (J3 round-1 N3).
/// 3. **Serial, representative** — one embed of [`PROBE_TEXT_BYTES`] bytes
///    alone, wall-clocked, and **best effort** (J3-R2-1). Input length is a
///    first-order determinant of a transformer's latency, so leg 2 alone
///    measures a rate for 35-byte writes; this leg measures the same words at
///    the size the product's own concepts carry. It is best effort because an
///    embedder may refuse the larger input outright — measured, not
///    hypothesised: this rig's llama-server answers 1280 B and returns HTTP 500
///    at 1536 B — and losing the whole probe to a refused *optional* leg would
///    trade an optimistic number for no number at all.
/// 4. **Concurrent** — [`PROBE_CONCURRENCY`] embeds together, wall-clocked, for
///    the aggregate rate and the parallelism figure an operator reads — for no
///    bound (J3 round-1 N3: "for the aggregate bound"). At
///    the representative size when leg 3 proved the embedder accepts it, at the
///    short one otherwise: the aggregate leg gets the same conservative input as
///    the serial one wherever that is known to be answerable.
///
/// Any **required** leg failing or the budget running out means the same thing:
/// this deployment's rate is not known, and saying so beats inventing a
/// number. Saying so costs nothing but the telemetry (J3 redesign): the
/// difference between `Unmeasured` and `Probe` is `write_queue_measured` and an
/// absent `probe_optimism` baseline — never a bound, and the observed rate
/// still replaces the figure after [`OBSERVED_MIN_SAMPLES`] real writes.
async fn probe_embedder(embedder: &dyn Embedder) -> Calibration {
    let deadline = tokio::time::Instant::now() + PROBE_BUDGET;

    for _ in 0..PROBE_WARMUP_EMBEDS {
        match tokio::time::timeout_at(deadline, embedder.embed(PROBE_TEXT)).await {
            Ok(Ok(_)) => {}
            _ => return Calibration::unmeasured(),
        }
    }

    let serial_started = tokio::time::Instant::now();
    match tokio::time::timeout_at(deadline, embedder.embed(PROBE_TEXT)).await {
        Ok(Ok(_)) => {}
        _ => return Calibration::unmeasured(),
    }
    let serial_wall = serial_started.elapsed();

    let representative = probe_text_at(PROBE_TEXT_BYTES);
    let representative_started = tokio::time::Instant::now();
    // An OPTIONAL leg may never starve the required one that follows it, so it
    // gets at most half of what is left of the budget and the concurrent leg
    // keeps the other half. Without this, an embedder that *hangs* on the
    // larger input (rather than refusing it) would burn the whole of
    // PROBE_BUDGET here and take the probe down with it — turning an
    // improvement into a new way to reach `unmeasured`.
    let representative_deadline =
        representative_started + deadline.saturating_duration_since(representative_started) / 2;
    let representative_wall =
        match tokio::time::timeout_at(representative_deadline, embedder.embed(&representative))
            .await
        {
            Ok(Ok(_)) => Some(representative_started.elapsed()),
            // Refused, errored or out of budget. Keep the short figure and say
            // nothing about a size this embedder would not take.
            _ => {
                tracing::debug!(
                bytes = PROBE_TEXT_BYTES,
                "write queue: the calibration probe's representative leg was not answered; the \
                 serial rate rests on the short text alone"
            );
                None
            }
        };

    let concurrent_text: &str = match representative_wall {
        Some(_) => &representative,
        None => PROBE_TEXT,
    };
    let concurrent_started = tokio::time::Instant::now();
    let mut set = Vec::with_capacity(PROBE_CONCURRENCY);
    for _ in 0..PROBE_CONCURRENCY {
        set.push(embedder.embed(concurrent_text));
    }
    match tokio::time::timeout_at(deadline, futures_join_all(set)).await {
        Ok(results) if results.iter().all(Result::is_ok) => Calibration::from_probe(
            serial_wall,
            representative_wall,
            concurrent_started.elapsed(),
        ),
        _ => Calibration::unmeasured(),
    }
}

/// [`PROBE_TEXT`] repeated to exactly `bytes` bytes.
///
/// The same words, only longer — so the representative leg differs from the
/// short one in **length and nothing else**, which is what makes the pair a
/// measurement of length sensitivity rather than of two unrelated inputs
/// (J3-R2-1). Byte truncation is safe because [`PROBE_TEXT`] is ASCII, which is
/// a build invariant beside the constant rather than a comment here.
fn probe_text_at(bytes: usize) -> String {
    let mut text = PROBE_TEXT.repeat(bytes / PROBE_TEXT.len() + 1);
    text.truncate(bytes);
    text
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
mod tests;

#[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]
mod pipeline_tests;
