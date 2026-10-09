//! Execution and settlement: [`WriteCtx`] runs one job through the ordinary
//! write path, and the lane worker settles its receipt.
//!
//! Invariants kept here:
//!
//! * **No `Memory` handle in a worker.** [`WriteCtx`] holds `Arc` clones of
//!   the shared state, so `Memory`'s `Drop` stays reachable.
//! * **Intent consumption rides the commit's write lock** ([`WriteCtx::run`]),
//!   so a write and its `ConsumeWriteIntent` always flush in one batch.
//! * **Index mirroring is graph read, then index write** ([`mirror_concepts`]),
//!   the daemon GC's order; `Memory`'s synchronous writes call the same
//!   function.
//! * **No graph lock across `.await`**, and **no `.await` between a completed
//!   graph write and its settle**, so an aborted worker always means "not
//!   written".
//! * **Per-job fence check**: a lane drained after a lost lease refuses each
//!   job and leaves its durable intent for the session's current holder.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde_json::json;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::{
    model_safe_failure, settle_one, AppliedSummary, Job, JobPayload, ReceiptAnswer, WriteKind,
    WritePipeline, MAX_RECEIPT_IDS,
};
use crate::embed::Embedder;
use crate::graph::action::{
    record_action_with_embeddings as graph_record_action_embedded, Action, ActionEmbeddings,
};
use crate::graph::derive::{derive as graph_derive, ParentOf};
use crate::graph::hybrid;
use crate::graph::index::InvertedIndex;
use crate::graph::Graph;
use crate::store::vector_source::VectorCandidates;
use crate::store::GraphStore;
use crate::types::{
    AgentId, ConceptType, EmbeddingContract, LamboError, MatchStrategy, Node, NodeId, SessionId,
    WriteIntentOutcome,
};

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
pub(super) fn truncate_ids(ids: &[NodeId]) -> Vec<String> {
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
    pub(super) tag: &'static str,
    pub(super) at: DateTime<Utc>,
}

/// The receipt sentence for an applied derive — one function, because the
/// receipt's copy and the durable intent outcome's copy (written inside the
/// commit lock by the [`hybrid::CommitHook`]) must be the same sentence.
///
/// Applied ≠ embedded (J3-R3-1): only the hybrid strategy can embed, so only
/// it reports the count — and its sentence carries the number, so a write
/// applied without its vector is never read as an unqualified success.
pub(super) fn derive_sentence(
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
pub(super) fn action_sentence(
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
    /// The vector-candidate source a background job's hybrid derive and
    /// record-action embed gate are given (#27's caller-side seam): the twin
    /// of `Memory::vector_candidates`, built by the same constructor
    /// ([`VectorCandidates::for_holder`], #8) so the two cannot diverge.
    pub(crate) fn vector_candidates(&self) -> VectorCandidates<'_> {
        VectorCandidates::for_holder(self.store.as_ref(), &self.graph)
    }

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
    pub(super) async fn run(
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
                        hybrid::derive_with(
                            self.graph.clone(),
                            self.vector_candidates(),
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
                let vector_search = self.vector_candidates().available();
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

impl WritePipeline {
    pub(super) fn spawn_worker(&self, agent: AgentId) -> JoinHandle<()> {
        let ctx = self.ctx.clone();
        let lanes = self.lanes.clone();
        let receipts = self.receipts.clone();
        let counters = self.counters.clone();
        let settled = self.settled.clone();
        let clock = self.clock.clone();
        let observed = self.observed.clone();
        let apply_latency = self.apply_latency.clone();
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
                        if let Err(e) = &raw_outcome
                            && !matches!(e, LamboError::EmbedUnavailable(_))
                        {
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
                        apply_latency.lock().record(job.admitted.elapsed());
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
}
