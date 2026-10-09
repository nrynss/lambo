//! Admission and lanes: the bounds a job must pass, and the per-agent FIFO
//! lanes it waits in.
//!
//! Admission guards **memory** ([`WRITE_QUEUE_MAX`], [`WRITE_QUEUE_MAX_BYTES`])
//! and **fairness** ([`WRITE_QUEUE_LANE_MAX`], one agent's share). All three
//! are static. Durability is not admission's job: every accepted job is
//! recorded as a durable intent in the same critical section that queues it.
//!
//! Invariants kept here:
//!
//! * **Lock order: graph write, then lanes**, held together across the accept
//!   branch so the `PutWriteIntent` is in the mutation log before a worker can
//!   see the job. No other site nests these two locks.
//! * **Per-agent FIFO:** one lane per agent, one consumer per lane
//!   ([`super::execution`]); presence in `Lanes::workers` *is* liveness
//!   (or, once sealed, an aborted worker still owed a join; see
//!   `drain::WorkerCustody`).
//! * A refusal never enters [`super::WriteQueueCounters::accepted`]; it is
//!   born settled with a `dropped` receipt.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::Ordering;

use tokio::task::JoinHandle;

use super::{
    Entry, ReceiptAnswer, ReceiptId, WriteKind, WritePipeline, MAX_CONCURRENT_RECEIPT_WAITS,
    MAX_RETAINED_RECEIPTS,
};
use crate::surface::limits::MAX_CONTENT_BYTES;
use crate::types::{AgentId, ConceptType, NodeId, SuppliedVector, WriteIntent, WriteIntentPayload};

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
    pub(super) fn describe(self, bound: usize) -> String {
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

/// A queued write, owned: the call path's borrows are gone by the time this
/// exists, because the background path outlives them.
#[derive(Debug)]
pub(super) struct Job {
    pub(super) receipt: ReceiptId,
    pub(super) agent: AgentId,
    pub(super) interaction: NodeId,
    pub(super) bytes: usize,
    pub(super) payload: JobPayload,
    /// When the job was admitted, for the apply-latency window (#11).
    pub(super) admitted: tokio::time::Instant,
}

#[derive(Debug)]
pub(super) enum JobPayload {
    Derive {
        concepts: Vec<(String, ConceptType)>,
        pairs: Vec<(String, String)>,
    },
    /// An image derive (#22): the derive above, with one concept's vector
    /// supplied. It carries the vector, never image bytes.
    DeriveImage {
        concepts: Vec<(String, ConceptType)>,
        pairs: Vec<(String, String)>,
        supplied: SuppliedVector,
    },
    Action {
        action: String,
        produces: Vec<String>,
        modifies: Vec<String>,
        depends_on: Vec<String>,
    },
}

impl JobPayload {
    pub(super) fn kind(&self) -> WriteKind {
        match self {
            JobPayload::Derive { .. } => WriteKind::Derive,
            JobPayload::DeriveImage { .. } => WriteKind::DeriveImage,
            JobPayload::Action { .. } => WriteKind::RecordAction,
        }
    }

    /// The durable form of this job, exactly as validated (J3 intents).
    pub(super) fn to_intent_payload(&self) -> WriteIntentPayload {
        match self {
            JobPayload::Derive { concepts, pairs } => WriteIntentPayload::Derive {
                concepts: concepts.clone(),
                pairs: pairs.clone(),
            },
            JobPayload::DeriveImage {
                concepts,
                pairs,
                supplied,
            } => WriteIntentPayload::DeriveImage {
                concepts: concepts.clone(),
                pairs: pairs.clone(),
                supplied: supplied.clone(),
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
    pub(super) fn from_intent_payload(p: WriteIntentPayload) -> Self {
        match p {
            WriteIntentPayload::Derive { concepts, pairs } => {
                JobPayload::Derive { concepts, pairs }
            }
            WriteIntentPayload::DeriveImage {
                concepts,
                pairs,
                supplied,
            } => JobPayload::DeriveImage {
                concepts,
                pairs,
                supplied,
            },
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
    ///
    /// An image derive is charged its strings plus four bytes per vector
    /// component and the declared contract's strings (design section 5.1), so
    /// a burst of image derives meets the same byte cap as text.
    pub(super) fn bytes(&self) -> usize {
        fn derive_bytes(concepts: &[(String, ConceptType)], pairs: &[(String, String)]) -> usize {
            concepts.iter().map(|(c, _)| c.len()).sum::<usize>()
                + pairs.iter().map(|(a, b)| a.len() + b.len()).sum::<usize>()
        }
        match self {
            JobPayload::Derive { concepts, pairs } => derive_bytes(concepts, pairs),
            JobPayload::DeriveImage {
                concepts,
                pairs,
                supplied,
            } => {
                derive_bytes(concepts, pairs)
                    + supplied.content.len()
                    + std::mem::size_of::<f32>() * supplied.vector.len()
                    + supplied.contract.kind.len()
                    + supplied.contract.model.as_ref().map_or(0, String::len)
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

#[derive(Default)]
pub(super) struct Lanes {
    /// One FIFO per agent. Single consumer per lane, so a lane drains in
    /// submission order; lanes run concurrently.
    pub(super) queues: HashMap<AgentId, VecDeque<Job>>,
    /// The live worker per lane. Presence *is* liveness: a worker removes its
    /// own entry under this same lock immediately before returning, and an
    /// enqueue spawns one only when the entry is absent — so the
    /// "lane emptied, worker exited, new job arrived" race cannot be entered.
    /// One exception, only once the lanes are sealed: a `close()` cancelled
    /// inside `abort_workers` puts back handles it aborted but had not yet
    /// joined (`drain::WorkerCustody`), so the next `abort_workers` joins
    /// them. Those entries are owed a join, not live consumers.
    pub(super) workers: HashMap<AgentId, JoinHandle<()>>,
    pub(super) queued: usize,
    pub(super) bytes: usize,
    /// Jobs a worker has taken off a lane and not yet settled.
    pub(super) running: usize,
    /// The same count per lane. At most one per lane today (one consumer), but
    /// kept as a count so it stays correct if a lane ever gets more, and
    /// maintained at exactly the two sites that move `running`.
    pub(super) running_per_lane: HashMap<AgentId, usize>,
    /// `true` once the pipeline refuses admission (closing, or fenced).
    pub(super) sealed: bool,
    /// `true` once [`WritePipeline::abort_workers`] has drained the queues and
    /// aborted the workers. From then on nothing can drain: an aborted worker
    /// never runs its `running -= 1`, so `running` stays above zero until the
    /// joins finish and reset it, and nothing will notify `settled`. A
    /// [`WritePipeline::quiesce`] that finds it set (a `close()` retried after
    /// one was cancelled inside `abort_workers`) goes straight to the joins
    /// instead of waiting out [`super::WRITE_QUEUE_DRAIN_BUDGET`]. Never
    /// cleared: lanes stay sealed, so no new worker can make it stale.
    pub(super) workers_aborted: bool,
    /// `true` once [`WritePipeline::abort_workers`] has **settled** every
    /// receipt it held, set under this lock just before its final
    /// `notify_waiters`. Distinct from [`Lanes::workers_aborted`], which is
    /// set a whole join earlier: a receipt wait that ends at close keys on
    /// this one, or it answers `pending` for a queued job the same close is
    /// about to settle `intent_durable` (#11 review round 2, F1). Never
    /// cleared.
    pub(super) settled_at_close: bool,
}

impl Lanes {
    pub(super) fn outstanding(&self) -> usize {
        self.queued + self.running
    }

    /// What a quiesce can still wait for: [`Lanes::outstanding`], or nothing
    /// once the workers were aborted (see [`Lanes::workers_aborted`]).
    pub(super) fn drainable(&self) -> usize {
        if self.workers_aborted {
            0
        } else {
            self.outstanding()
        }
    }

    /// Outstanding jobs **on one lane** — queued plus the one being run by that
    /// lane's own consumer. This is the population [`WRITE_QUEUE_LANE_MAX`]
    /// bounds, and the reason a global gauge could not do the job: the drain is
    /// per-lane and serial (J3-R1-1), which is also why the round-3 defect —
    /// a ceiling derived per-lane and enforced across lanes — was possible at
    /// all. (J3 round-1 N3: this named `Calibration::lane_bound`, which since the
    /// estimator demotion is a field that copies the static constant rather than
    /// a bound of its own.)
    pub(super) fn lane_outstanding(&self, agent: &AgentId) -> usize {
        self.queues.get(agent).map_or(0, VecDeque::len)
            + self.running_per_lane.get(agent).copied().unwrap_or(0)
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

impl WritePipeline {
    /// Outstanding jobs — queued plus running.
    pub fn outstanding(&self) -> usize {
        self.lanes.lock().outstanding()
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
    /// [`PROBE_BUDGET`](super::PROBE_BUDGET) because "a provisional constant is the constant the
    /// spec forbids"; with durability carried by durable intents, a constant
    /// is exactly what a fairness share should be, and the probe is telemetry
    /// nobody has to wait for.)
    pub(super) async fn admit(
        &self,
        agent: AgentId,
        interaction: NodeId,
        payload: JobPayload,
    ) -> Submitted {
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
                        admitted: tokio::time::Instant::now(),
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
}
