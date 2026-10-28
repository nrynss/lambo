//! Durable-intent replay: applying, at attach, the writes a previous process
//! acked and did not apply (J3).
//!
//! Spawned by `MemoryBuilder::build_attach` after the pipeline exists; stopped
//! (abort **and join**) by `close()` before the quiesce, and aborted without a
//! join by `Drop`. Unconsumed intents stay durable for the next serve.
//!
//! Invariants kept here (the full argument is on [`WritePipeline::spawn_replay`]):
//!
//! * strictly one intent at a time, in (`issued_ms`, `lane_seq`) order, never
//!   through admission;
//! * nothing is consumed before one liveness embed succeeds;
//! * a content refusal consumes the intent as `failed`; an outage, a store
//!   error or a lost lease leaves it unconsumed and, past
//!   [`EMBEDDER_SICK_THRESHOLD`] consecutive transients or on any
//!   non-embedder error, ends the loop;
//! * replay moves only `replayed` / `replay_owed` / `replay_blocked`, never this
//!   session's own `accepted`/`applied`/`failed`.

use std::str::FromStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use parking_lot::Mutex as PlMutex;
use serde_json::json;
use tokio::task::JoinHandle;

use super::{
    ConsumeStamp, Job, JobPayload, ReceiptAnswer, ReceiptId, ReplayBlockReason, WritePipeline,
    PROBE_BUDGET, PROBE_TEXT, RECEIPT_RETENTION,
};
use crate::types::{LamboError, WriteIntent, WriteIntentOutcome};

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

/// Consecutive vector-candidate lookup timeouts at replay that stop it, the
/// last one left durable (#125). One timeout is a fact about that write (its
/// lookup is slow, every time) and settles it `failed`; this many in a row,
/// with nothing applied between, is a fact about the store, and the rest of
/// the backlog waits for the next serve. Same value and reading as
/// [`EMBEDDER_SICK_THRESHOLD`]: two in a row do not stop a healthy replay, and
/// a slow store costs at most two settled writes per attach.
pub const REPLAY_LOOKUP_TIMEOUT_LIMIT: usize = EMBEDDER_SICK_THRESHOLD;

impl WritePipeline {
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
    ///   (see [`WriteCtx::run`](super::WriteCtx::run)), so a `kill -9` mid-replay re-replays exactly
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
                if let Some(o) = &intent.outcome
                    && o.consumed_at < stale_before
                {
                    continue;
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
                let (answer, destination) = match &intent.outcome {
                    None => (
                        ReceiptAnswer::PendingReplay,
                        intent.payload.receipt_destination().cloned(),
                    ),
                    Some(o) => {
                        let (summary, destination) =
                            super::receipts::unpack_durable_summary(&o.summary);
                        let answer = if o.tag == "failed" {
                            ReceiptAnswer::Failed(summary)
                        } else {
                            ReceiptAnswer::AppliedAfterRestart(summary, None)
                        };
                        (
                            answer,
                            destination.or_else(|| intent.payload.receipt_destination().cloned()),
                        )
                    }
                };
                map.insert(id, (intent.agent.clone(), destination, answer));
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
            // Consecutive lookup timeouts (#125); see
            // [`REPLAY_LOOKUP_TIMEOUT_LIMIT`].
            let mut lookup_streak: usize = 0;
            for intent in pending {
                if this.ctx.lease_lost.load(Ordering::Acquire) || this.lanes.lock().sealed {
                    break;
                }
                let Ok(receipt) = ReceiptId::from_str(&intent.receipt) else {
                    continue;
                };
                let destination = intent.payload.receipt_destination().cloned();
                let (payload, decode_error) = match JobPayload::from_intent_payload(intent.payload)
                {
                    Ok(payload) => (payload, None),
                    Err(error) => (
                        JobPayload::Derive {
                            concepts: vec![],
                            pairs: vec![],
                            citations: Default::default(),
                            repo_scope: None,
                        },
                        Some(error),
                    ),
                };
                let job = Job {
                    receipt,
                    agent: intent.agent.clone(),
                    receipt_destination: destination.clone(),
                    interaction: intent.interaction,
                    bytes: 0,
                    payload,
                    admitted: tokio::time::Instant::now(),
                };
                let stamp = ConsumeStamp {
                    tag: "applied_after_restart",
                    at: (this.clock)(),
                };
                // #129: a stored payload this build refuses to decode into a
                // job (an invalid scope, a nested or image `repo_scoped`
                // wrapper) is a fact about this row, settled `failed` below
                // like a content refusal, never a reason to stop the replay.
                let decode_refused = decode_error.is_some();
                let result = match decode_error {
                    Some(error) => Err(error),
                    None => this.ctx.run(&job, Some(stamp)).await,
                };
                let answer = match result {
                    Ok(summary) => {
                        // Observed health: the embedder just worked. Resets the
                        // consecutive-transient streak.
                        transient_streak = 0;
                        lookup_streak = 0;
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
                                Some(serde_json::Value::Object(summary.ledger_facts())),
                            ));
                        }
                        ReceiptAnswer::AppliedAfterRestart(summary.summary, summary.stale_report)
                    }
                    // #125 (Sonnet review): a vector-candidate lookup that
                    // did not answer within its own budget. The embedder answered before it, so
                    // it is no evidence of a sick embedder and never counts
                    // toward that streak. Left durable, a lookup that is slow
                    // every time for this input would hold the backlog on
                    // every restart; so the replay is its last attempt (the
                    // first was in the session that acked it, or it was
                    // deferred there unattempted), and it settles `failed`
                    // with a reason that says so. A run of them with nothing
                    // applied between means the store is slow for everyone:
                    // stop and leave the rest durable, so when the store is
                    // slow at most REPLAY_LOOKUP_TIMEOUT_LIMIT - 1 (two) are
                    // settled failed per attach.
                    Err(err) if err.is_lookup_timeout() => {
                        lookup_streak += 1;
                        if lookup_streak >= REPLAY_LOOKUP_TIMEOUT_LIMIT {
                            this.counters.set_replay_blocked(ReplayBlockReason::Other);
                            tracing::warn!(
                                session = %this.ctx.session,
                                receipt = %intent.receipt,
                                applied,
                                failed,
                                remaining = backlog - applied - failed,
                                "write queue: {REPLAY_LOOKUP_TIMEOUT_LIMIT} durable intents in a \
                                 row timed out in the vector candidate lookup; the store looks \
                                 slow for every write, so THIS intent and the rest of the backlog \
                                 stay DURABLE and unconsumed for the next serve"
                            );
                            break;
                        }
                        failed += 1;
                        this.counters.replay_owed.fetch_sub(1, Ordering::Relaxed);
                        let why = format!(
                            "replay after restart was refused (the vector candidate lookup did \
                             not answer within {:?}, so the write is not retried again); \
                             nothing was written",
                            crate::graph::hybrid::VECTOR_LOOKUP_TIMEOUT
                        );
                        if let Some(ledger) = &this.ctx.ledger {
                            ledger.append(&crate::ledger::completion_line(
                                &job.agent.to_string(),
                                &job.receipt.to_string(),
                                "failed",
                                Some(json!({ "error": &why, "replay": true })),
                            ));
                        }
                        this.ctx.graph.write().consume_write_intent(
                            intent.receipt.clone(),
                            WriteIntentOutcome {
                                tag: "failed".into(),
                                summary: super::receipts::durable_summary(
                                    &why,
                                    destination.as_ref(),
                                ),
                                consumed_at: (this.clock)(),
                            },
                        );
                        tracing::warn!(
                            session = %this.ctx.session,
                            receipt = %intent.receipt,
                            error = %err,
                            "write queue: a durable intent's vector candidate lookup timed out \
                             again at replay; it is settled failed rather than retried on every \
                             restart"
                        );
                        ReceiptAnswer::Failed(why)
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
                    //
                    // A cycle refusal (#116 review M1) is the same kind of fact:
                    // this write against this graph closes a cycle, every time.
                    // Before, it fell to the session-wide arm below, which left
                    // the intent durable and stopped the replay, so every later
                    // intent stayed `pending_replay` for good. Narrow on purpose:
                    // any other `Invariant` can mean an inconsistent graph or
                    // store, and still stops the replay.
                    // #36 PR 4: a refused reference is a fact about this
                    // input against this graph too (re-resolution at apply
                    // reached a different answer, or none): settle `failed`.
                    Err(err)
                        if decode_refused
                            || matches!(
                                &err,
                                LamboError::Embed(_)
                                    | LamboError::ImageIdTaken(_)
                                    | LamboError::Reference(_)
                            )
                            || matches!(&err, LamboError::Store(e) if e.is_cycle_refusal()) =>
                    {
                        // A cycle refusal is decided before any embed, so it
                        // proves nothing about the embedder's health; nor
                        // does a decode refusal.
                        if !decode_refused
                            && !matches!(&err, LamboError::Store(_) | LamboError::Reference(_))
                        {
                            transient_streak = 0;
                        }
                        failed += 1;
                        this.counters.replay_owed.fetch_sub(1, Ordering::Relaxed);
                        // JE2E-12, same split as the in-session worker's failure
                        // arm: the class for the model, the embedder's own words
                        // for the operator. The "replay after restart was
                        // refused; nothing was written" framing is lambo's own
                        // and is the useful half — it says *when* and *whether*
                        // — so it survives on both.
                        // #22 PR 4: an image-id collision keeps its own
                        // model-safe sentence (it names the fix); every other
                        // refusal is its class, as before.
                        let reason = match &err {
                            // Lambo's own sentence about the stored row.
                            LamboError::Config(why) if decode_refused => why.clone(),
                            LamboError::ImageIdTaken(_)
                            | LamboError::Store(_)
                            | LamboError::Reference(_) => {
                                crate::surface::error::model_safe_message(&err)
                            }
                            _ => crate::surface::error::err_class(&err).to_owned(),
                        };
                        let why = format!(
                            "replay after restart was refused ({reason}); nothing was written"
                        );
                        let e = match &err {
                            LamboError::Embed(e) => e.clone(),
                            other => other.to_string(),
                        };
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
                                summary: super::receipts::durable_summary(
                                    &why,
                                    destination.as_ref(),
                                ),
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
                    // #22 review L4: an image intent this process's
                    // configuration cannot apply (not the hybrid strategy, or
                    // no vector search) is a `Config` refusal like any other,
                    // and stops the replay for the same reason: skipping it
                    // would break the lane order, and a later intent could
                    // create the image's canonical key as text first. It gets
                    // its own block reason and log line, because the text
                    // intents behind it would apply and the fix is specific.
                    Err(LamboError::Config(e))
                        if matches!(job.payload, JobPayload::DeriveImage { .. }) =>
                    {
                        this.counters
                            .set_replay_blocked(ReplayBlockReason::ImageConfig);
                        tracing::warn!(
                            session = %this.ctx.session,
                            receipt = %intent.receipt,
                            error = %e,
                            applied,
                            remaining = backlog - applied - failed,
                            "write queue: a durable IMAGE derive intent cannot be replayed by this \
                             process (an image derive needs match_strategy = hybrid and a store \
                             with vector search); it stays DURABLE and unconsumed, and the replay \
                             stops here so the rest of the backlog keeps its order. Restart with \
                             the hybrid strategy and a vector-search store to drain it"
                        );
                        break;
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
                // Durable rows carry only the summary. The bounded live entry
                // carries the replay report and participates in normal eviction.
                let durable_answer = match &answer {
                    ReceiptAnswer::AppliedAfterRestart(summary, _) => {
                        ReceiptAnswer::AppliedAfterRestart(summary.clone(), None)
                    }
                    other => other.clone(),
                };
                if destination.is_some() {
                    let mut receipts = this.receipts.lock();
                    let now = (this.clock)();
                    receipts.entries.insert(
                        receipt,
                        super::receipts::Entry {
                            agent: intent.agent.clone(),
                            deliver_to: destination
                                .as_ref()
                                .filter(|d| **d != this.ctx.session)
                                .cloned(),
                            settled_at: Some(now),
                            answer,
                        },
                    );
                    receipts.order.push_back(receipt);
                    if destination.is_some() {
                        receipts
                            .undelivered
                            .entry(intent.agent.clone())
                            .or_default()
                            .push_back(receipt);
                    }
                    receipts.expire(now);
                    receipts.evict();
                }
                this.restart
                    .lock()
                    .insert(receipt, (intent.agent.clone(), destination, durable_answer));
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
            // Custody until the join returns (R3-1): a `close()` cancelled
            // here hands the aborted handle back to its slot, so the retried
            // close joins it rather than draining past a replay that is still
            // inside a synchronous stretch.
            let mut custody = ReplayCustody {
                slot: &self.replay,
                handle: Some(handle),
            };
            if let Some(handle) = custody.handle.as_mut() {
                let _ = handle.await;
            }
            custody.handle = None;
        }
    }

    /// Abort the replay task without joining — the `Drop` path, which cannot
    /// await (same shape as [`WritePipeline::abort_all_sync`]).
    pub(crate) fn abort_replay_sync(&self) {
        if let Some(handle) = self.replay.lock().take() {
            handle.abort();
        }
    }
}

/// Custody of the replay task's handle while [`WritePipeline::stop_replay`]
/// joins it: returned to its slot, already aborted, if the join is cancelled.
/// The twin of `memory::shutdown::HandleCustody`.
struct ReplayCustody<'a> {
    slot: &'a PlMutex<Option<JoinHandle<()>>>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for ReplayCustody<'_> {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            *self.slot.lock() = Some(handle);
        }
    }
}
