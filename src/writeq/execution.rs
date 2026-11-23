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
//!   written". (Test builds only: a worker waiting at an armed test pause is
//!   the exception, and `abort_workers` refuses to run then; see
//!   `test_pause`.)
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
    hold_one, model_safe_failure, settle_one, AppliedSummary, Job, JobPayload, ReceiptAnswer,
    ReceiptId, WriteKind, WritePipeline, MAX_RECEIPT_IDS,
};
use crate::embed::Embedder;
use crate::graph::action::{record_action_against, Action, ActionEmbeddings};
use crate::graph::derive::{derive_cited, ParentOf};
use crate::graph::hybrid;
use crate::graph::index::InvertedIndex;
use crate::graph::Graph;
use crate::store::vector_source::VectorCandidates;
use crate::store::GraphStore;
use crate::types::{
    AgentId, ConceptType, EmbeddingContract, LamboError, MatchStrategy, Node, NodeId,
    PinnedCitations, SessionId, WriteIntentOutcome,
};

/// Everything the background worker needs, and **nothing more** — in
/// particular not a handle on [`crate::Memory`].
///
/// That is the point of this struct rather than an `Arc<Memory>`: a worker
/// holding its owner would make the owner un-droppable by its own background
/// tasks, and `Memory`'s `Drop` is load-bearing (it aborts tasks and lets the
/// lease lapse). Every field here is already an `Arc` inside `Memory`, so this
/// is clones of shared state, not a second copy of anything.
#[derive(Clone)]
pub(crate) struct WriteCtx {
    pub(crate) session: SessionId,
    pub(crate) clock: crate::daemon::Clock,
    pub(crate) contradiction_evidence_min_age: std::time::Duration,
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
    /// An optional call ledger this pipeline appends durable-intent
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

/// When — and as what — a job's durable intent is consumed at commit.
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

/// What the lane worker does at a write's commit, run by [`WriteCtx::run`]
/// **inside the commit's critical section**, under the graph write lock,
/// beside the intent's consume ([`WorkerCommit::at_commit`]): it resets
/// `Lanes::lookup_streak` (#346) and records the write as consumed and not
/// yet settled (`Graph::mark_consumed_unsettled`, #345). The worker's failure
/// consume records it too, in its own consume's lock hold.
///
/// **The record (#345 re-review).** A worker can panic after its commit and
/// before its settle; its receipt stays unsettled, and the close sets it
/// aside. So a dead-lettered receipt is forgotten only once its write
/// settles (`Graph::settled_write`, in the worker's settle), not at its
/// consume, and the flush's note skips a put only when its write is consumed
/// and no longer in the record, that is, settled (Lean:
/// `Drain.settlePrune_never_vouches_dropped`; pruning at the consume,
/// `Drain.keptAtConsume_vouches_after_panic`). The replay passes no
/// `WorkerCommit`: its writes are earlier processes', whose puts are not in
/// this process's log.
///
/// The streak counts first-attempt lookup timeouts "with nothing applied
/// between", and a write is applied at its commit. Reset there, the commit
/// and the reset are one step: the commit is visible to no one until the
/// graph lock is released, after the reset, and a timeout's lanes-lock hold
/// falls before both or after both. Reset later (the `Ok` arm, or the settle
/// block before that), a timeout another lane met in between counted toward
/// the old run (Lean: `Drain.streak_okArm_counts_across_commit`,
/// `Drain.streak_old_counts_across_commit`; the fix,
/// `Drain.streakNew_eq_runSince`).
///
/// Lock order: graph write, then lanes, as admission nests them
/// (`WritePipeline::admit`); no path takes the lanes lock and then the
/// graph's. The replay passes none: its own streak is local to the replay.
#[derive(Clone)]
pub(super) struct WorkerCommit(Arc<parking_lot::Mutex<super::Lanes>>);

impl WorkerCommit {
    /// At the commit of the job with receipt `receipt`, under the graph write
    /// lock `g` is borrowed from.
    fn at_commit(&self, g: &mut Graph, receipt: &str) {
        g.mark_consumed_unsettled(receipt);
        self.0.lock().lookup_streak = 0;
    }
}

/// The receipt sentence for an applied derive — one function, because the
/// receipt's copy and the durable intent outcome's copy (written inside the
/// commit lock by the [`hybrid::CommitHook`]) must be the same sentence.
///
/// Applied ≠ embedded: only the hybrid strategy can embed, so only
/// it reports the count — and its sentence carries the number, so a write
/// applied without its vector is never read as an unqualified success.
pub(super) fn derive_sentence(
    strategy: MatchStrategy,
    submitted: usize,
    outcome: &crate::graph::derive::DeriveOutcome,
) -> String {
    derive_sentence_of(strategy, submitted, outcome, false)
}

/// [`derive_sentence`], naming an image derive as one (#22: "derived 1 image
/// concept: ..."). An image derive applies only under `Hybrid`.
pub(super) fn derive_sentence_of(
    strategy: MatchStrategy,
    submitted: usize,
    outcome: &crate::graph::derive::DeriveOutcome,
    image: bool,
) -> String {
    let sentence = derive_sentence_core(strategy, submitted, outcome, image);
    // #36 PR 4: what the call's citations added, only when they added
    // something, so a derive without them reads exactly as before. #19(b):
    // contradictions likewise.
    let mut sentence = sentence;
    if outcome.cited > 0 {
        sentence = format!("{sentence}; {} citation(s) recorded", outcome.cited);
    }
    if outcome.contradicted > 0 {
        sentence = format!(
            "{sentence}; {} contradiction(s) recorded",
            outcome.contradicted
        );
    }
    // #193: said only when a merge guard refused a hit, so a write that
    // merged (or had nothing near) reads as before.
    let guarded = outcome.merge_guarded;
    if !guarded.is_empty() {
        sentence = format!(
            "{sentence}; {} similarity merge(s) refused ({} for differing identifiers, {} for \
             `against`)",
            guarded.identifiers + guarded.contradicts,
            guarded.identifiers,
            guarded.contradicts
        );
    }
    // #198: said only when the trigger flagged a pair, so every other
    // receipt reads as before.
    if outcome.suspected > 0 {
        sentence = format!(
            "{sentence}; {} possible undeclared supersession(s) flagged",
            outcome.suspected
        );
    }
    sentence
}

fn derive_sentence_core(
    strategy: MatchStrategy,
    submitted: usize,
    outcome: &crate::graph::derive::DeriveOutcome,
    image: bool,
) -> String {
    if image {
        return format!(
            "derived {} image concept(s): {} created ({} embedded), {} matched existing",
            submitted,
            outcome.created.len(),
            outcome.embedded,
            outcome.matched.len()
        );
    }
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

#[cfg(test)]
type ReportGate = (
    crate::types::NodeId,
    std::sync::mpsc::Sender<()>,
    std::sync::mpsc::Receiver<()>,
);
#[cfg(test)]
pub(crate) static REPORT_GATE: std::sync::Mutex<Option<ReportGate>> = std::sync::Mutex::new(None);

impl WriteCtx {
    fn stale_report(
        &self,
        graph: &Graph,
        targets: &[crate::types::PinnedReference],
    ) -> Option<crate::daemon::contradiction::StaleReport> {
        if targets.is_empty() {
            return None;
        }
        #[cfg(test)]
        {
            let gate = {
                let mut gate = REPORT_GATE.lock().unwrap();
                if gate
                    .as_ref()
                    .is_some_and(|(id, _, _)| targets.iter().any(|p| p.target == *id))
                {
                    gate.take()
                } else {
                    None
                }
            };
            if let Some((_, entered, release)) = gate {
                entered.send(()).unwrap();
                release
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap();
            }
        }
        let mut report = crate::daemon::contradiction::stale_report(
            graph,
            targets.iter().map(|p| p.target),
            self.contradiction_evidence_min_age,
            (self.clock)(),
        );
        report.bound_for_receipt();
        Some(report)
    }
    /// The vector-candidate source a background job's hybrid derive and
    /// record-action embed gate are given (#27's caller-side seam): the twin
    /// of `Memory::vector_candidates`, built by the same constructor
    /// ([`VectorCandidates::for_holder`], #8) so the two cannot diverge.
    pub(crate) fn vector_candidates(&self) -> VectorCandidates<'_> {
        VectorCandidates::for_holder(self.store.as_ref(), &self.graph)
    }

    /// The source a background job's hybrid derive is given: the twin of
    /// `Memory::derive_vector_candidates`, built by the same constructor
    /// ([`VectorCandidates::for_holder_derive`]).
    pub(crate) fn derive_vector_candidates(&self) -> VectorCandidates<'_> {
        VectorCandidates::for_holder_derive(self.store.as_ref(), &self.graph)
    }

    /// Run one job through the ordinary write path.
    ///
    /// `consume`, when present, consumes the job's durable intent **inside the
    /// same write-lock critical section as the commit**: for the hybrid
    /// path via [`hybrid::CommitHook`], for the canonical and action paths
    /// inline under the guard the graph write already holds. The flush drain
    /// takes that same lock, so the applied mutations and the
    /// `ConsumeWriteIntent` always travel in one batch — one store
    /// transaction — and a crash can never leave the write durable beside a
    /// still-unconsumed intent (the double-apply this design excludes).
    ///
    /// `worker`, when present (the lane worker's), runs in that same critical
    /// section ([`WorkerCommit`]: the lookup-streak reset, #346, and the
    /// consumed-unsettled record, #345).
    pub(super) async fn run(
        &self,
        job: &Job,
        consume: Option<ConsumeStamp>,
        worker: Option<&WorkerCommit>,
    ) -> Result<AppliedSummary, LamboError> {
        match &job.payload {
            JobPayload::Derive {
                concepts, pairs, ..
            }
            | JobPayload::DeriveImage {
                concepts, pairs, ..
            } => {
                // #36 PR 4: what the ack accepted; the commit re-resolves it.
                let no_citations = PinnedCitations::default();
                let (pinned, repo_scope) = match &job.payload {
                    JobPayload::Derive {
                        citations,
                        repo_scope,
                        ..
                    } => (citations, repo_scope.as_deref()),
                    _ => (&no_citations, None),
                };
                // #22: an image derive is this derive with one concept's
                // vector supplied (`hybrid::derive_with`'s `supplied`).
                let supplied = match &job.payload {
                    JobPayload::DeriveImage { supplied, .. } => Some(supplied),
                    _ => None,
                };
                let image = supplied.is_some();
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
                }
                .with_repo_scope(repo_scope);
                let strategy = self.match_strategy;
                let submitted = borrowed.len();
                if image && strategy != MatchStrategy::Hybrid {
                    // The call path refuses this; a replay in a process
                    // configured `Canonical` meets it here. Session-wide
                    // configuration, so it is not a fact about this intent.
                    return Err(LamboError::Config(
                        "an image derive needs match_strategy = \"hybrid\": its concept is \
                         found only through its supplied vector"
                            .into(),
                    ));
                }
                let report_slot = std::sync::Arc::new(parking_lot::Mutex::new(None));
                let outcome = match strategy {
                    MatchStrategy::Hybrid => {
                        let receipt = job.receipt.to_string();
                        let report_slot = report_slot.clone();
                        let targets = pinned.against.clone();
                        let ctx = self.clone();
                        let destination = job.receipt_destination.clone();
                        let worker = worker.cloned();
                        let on_commit: Option<hybrid::CommitHook> = Some(Box::new(
                            move |g: &mut Graph, outcome: &crate::graph::derive::DeriveOutcome| {
                                if let Some(worker) = &worker {
                                    worker.at_commit(g, &receipt);
                                }
                                if let Some(stamp) = consume {
                                    g.consume_write_intent(
                                        receipt,
                                        WriteIntentOutcome {
                                            tag: stamp.tag.into(),
                                            summary: super::receipts::durable_summary(
                                                &derive_sentence_of(
                                                    strategy, submitted, outcome, image,
                                                ),
                                                destination.as_ref(),
                                            ),
                                            consumed_at: stamp.at,
                                        },
                                    );
                                }
                                *report_slot.lock() = ctx.stale_report(g, &targets);
                            },
                        ));
                        hybrid::derive_with(
                            self.graph.clone(),
                            self.derive_vector_candidates(),
                            self.embedder.as_ref(),
                            &self.embedding,
                            job.interaction,
                            &job.agent,
                            &borrowed,
                            &parent_of,
                            self.max_cooccurrence_per_derive,
                            self.semantic_match_threshold,
                            supplied,
                            pinned,
                            on_commit,
                        )
                        .await?
                    }
                    MatchStrategy::Canonical => {
                        let mut g = self.graph.write();
                        let outcome = derive_cited(
                            &mut g,
                            job.interaction,
                            &job.agent,
                            &borrowed,
                            &parent_of,
                            self.max_cooccurrence_per_derive,
                            pinned,
                        )?;
                        // Same lock hold as the commit — see `run`'s doc.
                        if let Some(worker) = worker {
                            worker.at_commit(&mut g, &job.receipt.to_string());
                        }
                        if let Some(stamp) = consume {
                            g.consume_write_intent(
                                job.receipt.to_string(),
                                WriteIntentOutcome {
                                    tag: stamp.tag.into(),
                                    summary: super::receipts::durable_summary(
                                        &derive_sentence(strategy, submitted, &outcome),
                                        job.receipt_destination.as_ref(),
                                    ),
                                    consumed_at: stamp.at,
                                },
                            );
                        }
                        *report_slot.lock() = self.stale_report(&g, &pinned.against);
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
                    kind: job.payload.kind(),
                    summary: derive_sentence_of(strategy, submitted, &outcome, image),
                    created,
                    matched,
                    created_count: outcome.created.len(),
                    matched_count: outcome.matched.len(),
                    semantic_merged: Some(outcome.semantic_merged.len()),
                    reinforced: Some(outcome.reinforced),
                    edges: None,
                    embedded,
                    stale_report: report_slot.lock().take(),
                    replacement:
                        crate::graph::suspected::replacement::ReplacementReport::for_receipt(
                            &outcome.replacement_candidates,
                        ),
                })
            }
            JobPayload::Action {
                legacy_text_pins,
                repo_scope,
                action,
                produces,
                modifies,
                depends_on,
                depends_on_pins,
                against,
            } => {
                let p: Vec<&str> = produces.iter().map(String::as_str).collect();
                let m: Vec<&str> = modifies.iter().map(String::as_str).collect();
                let d: Vec<&str> = depends_on.iter().map(String::as_str).collect();
                let act = Action {
                    repo_scope: repo_scope.as_deref(),
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
                let (outcome, stale_report) = {
                    let mut g = self.graph.write();
                    // Stamp the space this call's vectors live in, or verify the
                    // stamp already there. Same rule as `hybrid::derive`'s
                    // commit phase: a vector must never land in a session with
                    // no declared contract.
                    // #36 PR 4: an entry that named a concept by id at the
                    // ack must name the same one now. #19(b): so must each
                    // `against` target, re-resolved in the same transaction,
                    // before the contract is stamped.
                    // Legacy intents' None text slots were deliberately
                    // unpinned. Refresh only those under the commit lock;
                    // their id-shaped slots retain the original ack pins.
                    let replay_pins = if *legacy_text_pins {
                        crate::graph::action::legacy_replay_pins(&g, &act, depends_on_pins)?
                    } else {
                        depends_on_pins.clone()
                    };
                    let outcome = record_action_against(
                        &mut g,
                        job.interaction,
                        &job.agent,
                        &act,
                        &embeddings,
                        &replay_pins,
                        against,
                        (!embeddings.is_empty()).then(|| self.embedding.clone()),
                    )?;
                    // Same lock hold as the commit — see `run`'s doc.
                    if let Some(worker) = worker {
                        worker.at_commit(&mut g, &job.receipt.to_string());
                    }
                    if let Some(stamp) = consume {
                        g.consume_write_intent(
                            job.receipt.to_string(),
                            WriteIntentOutcome {
                                tag: stamp.tag.into(),
                                summary: super::receipts::durable_summary(
                                    &action_sentence(self.match_strategy, &outcome),
                                    job.receipt_destination.as_ref(),
                                ),
                                consumed_at: stamp.at,
                            },
                        );
                    }
                    let report = self.stale_report(&g, against);
                    (outcome, report)
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
                    stale_report,
                    replacement: None,
                })
            }
        }
    }
}

/// #295: how long an agent's worker waits, after its oldest held write met
/// an outage `attempts` times in a row, before it retries that write: one
/// second, doubling per failed attempt, at most
/// [`HYBRID_IO_TIMEOUT`](crate::graph::hybrid::HYBRID_IO_TIMEOUT) (30 s).
/// The wait runs from the end of the failed attempt, and nothing starts a
/// retry sooner: not a new write of the agent, not a refused one. An attempt
/// itself can take up to `HYBRID_IO_TIMEOUT`, so a write that never embeds
/// costs its agent one attempt per cycle of at most 60 s (the wait plus the
/// attempt); a write that blinked is retried a second later, receipts
/// waiting on it or not.
pub(crate) fn held_retry_backoff(attempts: u32) -> std::time::Duration {
    let first = std::time::Duration::from_secs(1);
    let max = crate::graph::hybrid::HYBRID_IO_TIMEOUT;
    first
        .checked_mul(1u32 << attempts.saturating_sub(1).min(16))
        .map_or(max, |d| d.min(max))
}

/// Test-only: wait at `point` if a test armed a pause there (#346), else
/// return at once without awaiting.
///
/// **What it costs.** While a worker waits here, a test build has an
/// `.await` between a committed graph write and its settle, so the worker's
/// rule "no `.await` between a completed graph write and its settle"
/// ("aborted means not written") does not hold for that worker: aborted
/// there, it would leave a write that applied with an unsettled receipt,
/// which a close would set aside and answer `intent_durable` or
/// `close_unflushed`. So `Lanes::test_paused` is set for the wait, and
/// `WritePipeline::abort_workers` (a close) and `abort_all_sync` (a dropped
/// `Memory`, unless the test is already panicking) panic while it is set: a
/// test must notify `resume` before it closes or drops the session. Unarmed (every other test, and
/// every non-test build, which has no pause at all), nothing here awaits.
#[cfg(test)]
async fn test_pause(lanes: &parking_lot::Mutex<super::Lanes>, point: super::admission::PausePoint) {
    let pause = {
        let mut l = lanes.lock();
        if l.test_pause.as_ref().is_some_and(|p| p.point == point) {
            l.test_paused = true;
            l.test_pause.take()
        } else {
            None
        }
    };
    if let Some(pause) = pause {
        pause.reached.notify_one();
        pause.resume.notified().await;
        lanes.lock().test_paused = false;
    }
}

/// What the worker does next (one write at a time).
enum Next {
    /// Run this write (boxed: a `Job` dwarfs the other variants).
    Run(Box<Job>),
    /// A write taken off the queue while its agent has held writes and is
    /// parked: it joined them, behind, without running.
    Held(ReceiptId),
    /// Nothing to run until the agent's retry is due. A new or refused write
    /// of the agent wakes the worker sooner, but only to hold what it finds
    /// queued: the retry still waits for this instant.
    Wait(tokio::time::Instant),
    Exit,
}

impl WritePipeline {
    /// Test-only (#346): arm a one-shot pause. The next worker whose write
    /// commits notifies the first `Notify` and waits for the second before
    /// its settle block. The test must notify `resume` before it closes the
    /// session: see [`test_pause`].
    #[cfg(test)]
    pub(crate) fn pause_before_settle(&self) -> (Arc<Notify>, Arc<Notify>) {
        self.arm_test_pause(super::admission::PausePoint::BeforeSettle)
    }

    /// Test-only (#346): arm a one-shot pause after a write's commit, once
    /// `WriteCtx::run` has returned and before the worker's `Ok` arm. The
    /// test must notify `resume` before it closes the session: see
    /// [`test_pause`].
    #[cfg(test)]
    pub(crate) fn pause_after_commit(&self) -> (Arc<Notify>, Arc<Notify>) {
        self.arm_test_pause(super::admission::PausePoint::AfterCommit)
    }

    /// Test-only (#345 review): the next worker whose write commits panics
    /// right after `WriteCtx::run` returns, before its settle.
    #[cfg(test)]
    pub(crate) fn panic_after_commit(&self) {
        self.lanes.lock().panic_after_commit = true;
    }

    /// Test-only: whether a worker has taken that panic.
    #[cfg(test)]
    pub(crate) fn test_panicked(&self) -> bool {
        self.lanes.lock().test_panicked
    }

    #[cfg(test)]
    fn arm_test_pause(&self, point: super::admission::PausePoint) -> (Arc<Notify>, Arc<Notify>) {
        let reached = Arc::new(Notify::new());
        let resume = Arc::new(Notify::new());
        self.lanes.lock().test_pause = Some(super::admission::TestPause {
            point,
            reached: reached.clone(),
            resume: resume.clone(),
        });
        (reached, resume)
    }

    /// Spawn `agent`'s lane worker. Called under the lanes lock, so it takes
    /// its wake-up (`Lanes::wake_for`) from the caller rather than the lock.
    pub(super) fn spawn_worker(&self, agent: AgentId, wake: Arc<Notify>) -> JoinHandle<()> {
        let ctx = self.ctx.clone();
        let lanes = self.lanes.clone();
        let worker_commit = WorkerCommit(self.lanes.clone());
        let receipts = self.receipts.clone();
        let counters = self.counters.clone();
        let settled = self.settled.clone();
        let clock = self.clock.clone();
        let observed = self.observed.clone();
        let apply_latency = self.apply_latency.clone();
        tokio::spawn(async move {
            loop {
                // One write per turn, under the lock admission takes, so a
                // held write is counted (held, then running) until it settles.
                let next = {
                    let mut l = lanes.lock();
                    let fenced = ctx.lease_lost.load(Ordering::Acquire);
                    let has_held = l.lane_held(&agent) > 0;
                    // #295: the agent's retry is due once the backoff since its
                    // last failed attempt has passed (`Lanes::held_due`).
                    let due = l
                        .held_due
                        .get(&agent)
                        .copied()
                        .unwrap_or_else(tokio::time::Instant::now);
                    let retry_due = due <= tokio::time::Instant::now();
                    if has_held && (fenced || (retry_due && !l.sealed)) {
                        // The oldest held write first: retried, or refused
                        // below if the lease is gone.
                        let job = l.held.get_mut(&agent).and_then(VecDeque::pop_front);
                        if l.lane_held(&agent) == 0 {
                            l.held.remove(&agent);
                        }
                        match job {
                            Some(job) => {
                                l.running += 1;
                                *l.running_per_lane.entry(agent.clone()).or_insert(0) += 1;
                                Next::Run(Box::new(job))
                            }
                            None => continue,
                        }
                    } else if let Some(job) = l.queues.get_mut(&agent).and_then(VecDeque::pop_front)
                    {
                        l.queued -= 1;
                        if has_held {
                            // Its retry not due yet (or closing): it keeps its
                            // place behind the held writes, counted, unrun.
                            let id = job.receipt;
                            l.held.entry(agent.clone()).or_default().push_back(job);
                            Next::Held(id)
                        } else {
                            l.running += 1;
                            *l.running_per_lane.entry(agent.clone()).or_insert(0) += 1;
                            Next::Run(Box::new(job))
                        }
                    } else if has_held && !l.sealed {
                        Next::Wait(due)
                    } else {
                        // Exit decision and liveness bookkeeping happen under
                        // the same lock an enqueue takes, so no job can be
                        // queued against a worker that has already decided to
                        // stop. Dropping our own JoinHandle here detaches a
                        // task that is about to return. A sealed lane's held
                        // writes are the close's to set aside.
                        l.queues.remove(&agent);
                        l.workers.remove(&agent);
                        l.running_per_lane.remove(&agent);
                        l.wakes.remove(&agent);
                        Next::Exit
                    }
                };
                let job = match next {
                    Next::Exit => return,
                    Next::Held(id) => {
                        hold_one(&receipts, &id);
                        continue;
                    }
                    Next::Wait(due) => {
                        tokio::select! {
                            _ = tokio::time::sleep_until(due) => {}
                            _ = wake.notified() => {}
                        }
                        continue;
                    }
                    Next::Run(job) => *job,
                };
                // A retry of the write this worker held after a failed attempt
                // (not a held write's first attempt).
                let retried = lanes
                    .lock()
                    .held_attempts
                    .get(&agent)
                    .is_some_and(|(id, _)| *id == job.receipt);

                // The fence, checked per write, right before it would run: the
                // lease can be lost while a lane drains or a retry runs, and
                // every write taken after that is refused rather than written
                // into a session another writer owns. A write already running
                // when it is lost can still commit here; the store refuses
                // that flush.
                let (outcome, hold_cause) = if ctx.lease_lost.load(Ordering::Acquire) {
                    counters.abandoned.fetch_add(1, Ordering::Relaxed);
                    // The durable intent is deliberately NOT consumed here: it
                    // is not ours to consume any more. If it flushed before
                    // the fence, the session's current holder replays it
                    // (fenced flushes are refused at the store, so this
                    // process can neither apply nor consume it now); if it
                    // never flushed, it dies with this process's tail. The
                    // message is lambo's own text about its own lease, the
                    // same for the model and the operator.
                    let fenced = format!(
                        "this handle lost its single-writer lease before the write was applied; \
                         this process wrote nothing for session {} — if the write's durable \
                         intent reached the store first, the session's current holder will \
                         apply it",
                        ctx.session
                    );
                    (Err((fenced.clone(), fenced)), None)
                } else {
                    // Timed, because this lane is single-consumer: the wall
                    // clock around one `run` is this deployment's serial
                    // service time, the figure
                    // `write_queue_serial_items_per_sec` publishes. Neither a
                    // fenced refusal nor a failure is sampled: both are fast
                    // and say nothing about work the pipeline retired, and
                    // sampling them would bias the rate upward.
                    let started = tokio::time::Instant::now();
                    let stamp = ConsumeStamp {
                        tag: "applied",
                        at: (clock)(),
                    };
                    let raw_outcome = ctx.run(&job, Some(stamp), Some(&worker_commit)).await;
                    // Test-only (#346): hold a committed write here, after
                    // `run` returned and before the `Ok` arm, while another
                    // worker runs. See `test_pause`.
                    #[cfg(test)]
                    if raw_outcome.is_ok() {
                        test_pause(&lanes, super::admission::PausePoint::AfterCommit).await;
                        // Test-only (#345 review): a panic after the commit.
                        let panic_now = {
                            let mut l = lanes.lock();
                            let armed = std::mem::take(&mut l.panic_after_commit);
                            l.test_panicked |= armed;
                            armed
                        };
                        if panic_now {
                            panic!("test-only panic after a commit, before its settle");
                        }
                    }
                    // Which failures hold the write rather than consume it.
                    // An embedder outage says nothing about the input, so the
                    // write is held (#295). A lookup timeout is the replay's
                    // rule (#125): one is a fact about this write, settled
                    // `failed`; a run of `REPLAY_LOOKUP_TIMEOUT_LIMIT` in a
                    // row with nothing applied between says the store is slow
                    // for everyone, and holds it. Like the replay, which
                    // counts each intent once per pass, the run counts first
                    // attempts only: the retry of a write it held is that
                    // write's last attempt, and settles `failed` if the lookup
                    // times out again, so one input slow every time cannot
                    // hold the lane.
                    let hold_cause = match &raw_outcome {
                        Ok(_) => {
                            // No `lookup_streak` reset here: `run` reset it
                            // inside the commit (`WorkerCommit`, #346). Here is
                            // already past the index mirroring, so a timeout
                            // another lane met in between would have counted
                            // toward the old run (Lean:
                            // `Drain.streak_okArm_counts_across_commit`).
                            observed.lock().sample(started.elapsed().as_secs_f64());
                            None
                        }
                        Err(e) if e.is_lookup_timeout() => {
                            if retried {
                                None
                            } else {
                                let mut l = lanes.lock();
                                l.lookup_streak += 1;
                                (l.lookup_streak >= super::REPLAY_LOOKUP_TIMEOUT_LIMIT)
                                    .then_some("the store's vector lookup is slow for every write")
                            }
                        }
                        Err(LamboError::EmbedUnavailable(_)) => Some("the embedder did not answer"),
                        Err(_) => None,
                    };
                    if let Err(e) = &raw_outcome
                        && hold_cause.is_none()
                    {
                        // A failure has no commit to ride, so it is consumed
                        // here. The row holds the model-safe form: a restart
                        // answers `failed` straight out of its summary, and the
                        // operator's copy is the completion line and the WARN
                        // below.
                        let why = if e.is_lookup_timeout() {
                            format!(
                                "the vector candidate lookup did not answer within {:?}, so the \
                                 write is not retried; nothing was written",
                                crate::graph::hybrid::VECTOR_LOOKUP_TIMEOUT
                            )
                        } else {
                            model_safe_failure(e)
                        };
                        let receipt = job.receipt.to_string();
                        let mut g = ctx.graph.write();
                        g.consume_write_intent(
                            receipt.clone(),
                            WriteIntentOutcome {
                                tag: "failed".into(),
                                summary: super::receipts::durable_summary(
                                    &why,
                                    job.receipt_destination.as_ref(),
                                ),
                                consumed_at: (clock)(),
                            },
                        );
                        // Consumed, not yet settled, in the consume's own
                        // lock hold (`WorkerCommit`, #345).
                        g.mark_consumed_unsettled(&receipt);
                    }
                    // Two strings from here on: the class for the model, the
                    // error for the operator.
                    (
                        raw_outcome.map_err(|e| {
                            let why = if e.is_lookup_timeout() && hold_cause.is_none() {
                                format!(
                                    "the vector candidate lookup did not answer within {:?}, so \
                                     the write is not retried; nothing was written",
                                    crate::graph::hybrid::VECTOR_LOOKUP_TIMEOUT
                                )
                            } else {
                                model_safe_failure(&e)
                            };
                            (why, e.to_string())
                        }),
                        hold_cause,
                    )
                };
                if let (Some(cause), Err((_, detail))) = (hold_cause, &outcome) {
                    // #295: hold this write as the agent's oldest owed one. Its
                    // intent stays unconsumed and its bytes counted; its
                    // receipt answers `pending_replay`. No `.await` from here
                    // to the `continue`.
                    let receipt = job.receipt;
                    let (attempts, held) = {
                        let mut l = lanes.lock();
                        l.running -= 1;
                        if let Some(n) = l.running_per_lane.get_mut(&agent) {
                            *n = n.saturating_sub(1);
                        }
                        let attempts = match l.held_attempts.get(&agent) {
                            Some((id, n)) if *id == receipt => n + 1,
                            _ => 1,
                        };
                        l.held_attempts.insert(agent.clone(), (receipt, attempts));
                        l.held_due.insert(
                            agent.clone(),
                            tokio::time::Instant::now() + held_retry_backoff(attempts),
                        );
                        l.held.entry(agent.clone()).or_default().push_front(job);
                        (attempts, l.lane_held(&agent))
                    };
                    hold_one(&receipts, &receipt);
                    tracing::warn!(
                        session = %ctx.session,
                        agent = %agent,
                        receipt = %receipt,
                        attempts,
                        held,
                        retry_in = ?held_retry_backoff(attempts),
                        error = %detail,
                        "write queue: {cause} for this agent's oldest owed write ({attempts} \
                         attempt(s) in this server); it and the agent's {held} held write(s) answer \
                         pending_replay and are retried in the background, oldest first, so none \
                         applies ahead of it. At close they stay durable intents for the next \
                         serve. A write that never succeeds (too large for the embed budget, say) \
                         holds this agent's writes until a restart"
                    );
                    settled.notify_waiters();
                    continue;
                }

                // Test-only (#346): hold a committed write here, between its
                // commit and its settle block, while another worker runs. See
                // `test_pause` for what that costs and how it is guarded.
                #[cfg(test)]
                if outcome.is_ok() {
                    test_pause(&lanes, super::admission::PausePoint::BeforeSettle).await;
                }

                // No `.await` from here to the end of the iteration: an
                // `abort()` cannot land between a completed graph write and the
                // settle that reports it, so "aborted" always means "not
                // written".
                {
                    let mut l = lanes.lock();
                    l.running -= 1;
                    l.bytes = l.bytes.saturating_sub(job.bytes);
                    if let Some(n) = l.running_per_lane.get_mut(&agent) {
                        *n = n.saturating_sub(1);
                    }
                    // No `lookup_streak` reset here: it is reset at the commit
                    // (`WorkerCommit`, inside `run`). A second reset here would forget
                    // timeouts that came after the commit (Lean:
                    // `Drain.streak_both_forgets_run`).
                    if retried {
                        // The held write that set the backoff has settled; the
                        // writes behind it have had no attempt yet.
                        l.held_attempts.remove(&agent);
                        l.held_due.remove(&agent);
                    }
                }
                let answer = match outcome {
                    Ok(summary) => {
                        counters.applied.fetch_add(1, Ordering::Relaxed);
                        apply_latency.lock().record(job.admitted.elapsed());
                        // A closed acked write's applied lifecycle is
                        // measurable: carry the metric-2 fact set.
                        if let Some(ledger) = &ctx.ledger {
                            ledger.append(&crate::ledger::completion_line(
                                &job.agent.to_string(),
                                &job.receipt.to_string(),
                                "applied",
                                Some(serde_json::Value::Object(summary.ledger_facts())),
                            ));
                        }
                        ReceiptAnswer::Applied(summary)
                    }
                    Err((why, detail)) => {
                        counters.failed.fetch_add(1, Ordering::Relaxed);
                        // The ledger line is operator-facing and gets the raw
                        // error; the receipt is model-facing and gets the class.
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
                // Waiters on the receipt wake first; the graph bookkeeping
                // below is not part of the answer they wait for.
                settled.notify_waiters();
                // Settled: a close will not set it aside, so the graph
                // forgets it as consumed-and-unsettled and as dead-lettered
                // (#345). After `settle_one`, on its own graph lock hold: a
                // panic before here leaves both records, and the close
                // answers a dead-lettered one `close_unflushed`.
                ctx.graph.write().settled_write(&job.receipt.to_string());
            }
        })
    }
}
