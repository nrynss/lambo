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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::{Mutex as PlMutex, RwLock};
use serde_json::json;
use tokio::sync::{watch, Notify, Semaphore};
use tokio::task::JoinHandle;

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
    WriteIntent, WriteIntentOutcome,
};

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
pub use receipts::{
    AppliedSummary, ReceiptAnswer, ReceiptId, WriteKind, MAX_CONCURRENT_RECEIPT_WAITS,
    MAX_PIGGYBACK_RECEIPTS, MAX_RECEIPT_IDS, MAX_RETAINED_RECEIPTS, MEASURED_WORST_FLUSH_LAG_SECS,
    RECEIPT_RETENTION, RECEIPT_WAIT_MAX,
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
