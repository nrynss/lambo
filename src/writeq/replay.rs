//! Durable-intent replay: applying, at attach, the writes a previous process
//! acked and did not apply.
//!
//! Spawned by `MemoryBuilder::build_attach` after the pipeline exists; stopped
//! (abort **and join**) by `close()` before the quiesce, and aborted without a
//! join by `Drop`. Unconsumed intents stay durable for the next serve.
//!
//! Invariants kept here (the full argument is on [`WritePipeline::spawn_replay`]):
//!
//! * strictly one intent at a time, never through admission, in
//!   [`replay_order`]: each process's intents in `lane_seq` (admission) order,
//!   processes interleaved by `issued_ms`;
//! * nothing is consumed before one liveness embed succeeds;
//! * a content refusal, an input refusal or a malformed receipt id consumes
//!   the intent as `failed`; an outage, any other store error, a lost lease
//!   or an id in another receipt format leaves it unconsumed; an outage below
//!   [`EMBEDDER_SICK_THRESHOLD`] consecutive transients, or an id in another
//!   format, defers that intent's agent for the rest of the pass, so no later
//!   write of it overtakes the one left behind, and one more pass retries
//!   the deferred agents; past the threshold, or on any non-embedder error,
//!   the loop ends;
//! * replay moves only `replayed` / `replay_owed` / `replay_blocked`, never this
//!   session's own `accepted`/`applied`/`failed`.

use std::collections::{BTreeMap, HashSet};
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
use crate::types::{AgentId, LamboError, WriteIntent, WriteIntentOutcome};

/// How many **consecutive transient** embed failures within one attach end the
/// durable-intent replay loop, leaving the rest of the backlog durable (the
/// sequential decision rule, in the shape the design doc's "termination
/// measure" asks for).
///
/// Each replayed intent's status-classifier outcome is a Bernoulli draw
/// (`Transient` vs `Content`). A **content** rejection is absorbing — it is
/// permanent for *that input*, is consumed as `failed` immediately, and never
/// counts toward the embedder-sickness evidence. A **transient** draw is
/// evidence that the embedder is sick; it is left durable (never consumed),
/// increments a running streak, and defers that intent's agent for the rest
/// of the pass (#295: its later intents are not run, so none can apply
/// before it; a second pass retries it once, the streak carried over). The
/// streak resets to zero on any applied
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
/// doc's as-built disposition.
pub const EMBEDDER_SICK_THRESHOLD: usize = 3;

/// Consecutive vector-candidate lookup timeouts at replay that stop it, the
/// last one left durable (#125). One timeout is a fact about that write (its
/// lookup is slow, every time) and settles it `failed`; this many in a row,
/// with nothing applied between, is a fact about the store, and the rest of
/// the backlog waits for the next serve. Same value and reading as
/// [`EMBEDDER_SICK_THRESHOLD`]: two in a row do not stop a healthy replay, and
/// a slow store costs at most two settled writes per attach.
pub const REPLAY_LOOKUP_TIMEOUT_LIMIT: usize = EMBEDDER_SICK_THRESHOLD;

/// The order the replay applies durable intents in (#295).
///
/// `load_session` returns a session's intents sorted by (`issued_ms`,
/// `lane_seq`). `issued_ms` is the issuing process's wall clock, so a clock
/// that stepped back between two admissions put the later one first.
/// `lane_seq` is the process's admission counter, so within one process
/// (one receipt epoch) it is the order to keep. This keeps the loaded order's
/// choice of which process each position belongs to, so different processes'
/// intents interleave exactly as loaded (by wall clock: their counters are
/// not comparable), and fills each process's positions with its own intents
/// in `lane_seq` order. While no clock stepped back that is the loaded order
/// unchanged. An intent whose receipt id does not parse has no epoch and
/// keeps its position.
///
/// The ordering lives here, the one place that depends on it, so every store
/// adapter gets it from the same code; the rows carry no epoch column, so an
/// adapter's `ORDER BY` could not express it.
///
/// Modelled by `fixedLoad` in `formal/LamboProofs/Drain/Spec/Replay.lean`
/// (`fixedLoad_sorted`, `fixedLoad_epochs`, `fixedLoad_agrees`).
pub(crate) fn replay_order(intents: Vec<WriteIntent>) -> Vec<WriteIntent> {
    let epochs: Vec<Option<u64>> = intents
        .iter()
        .map(|i| ReceiptId::from_str(&i.receipt).ok().map(|r| r.epoch()))
        .collect();
    let mut positions: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    for (at, epoch) in epochs.iter().enumerate() {
        if let Some(epoch) = epoch {
            positions.entry(*epoch).or_default().push(at);
        }
    }
    let mut rows: Vec<Option<WriteIntent>> = intents.into_iter().map(Some).collect();
    let mut out: Vec<Option<WriteIntent>> = (0..rows.len()).map(|_| None).collect();
    for (at, epoch) in epochs.iter().enumerate() {
        if epoch.is_none() {
            out[at] = rows[at].take();
        }
    }
    for at in positions.values() {
        let mut own: Vec<WriteIntent> = at.iter().filter_map(|&p| rows[p].take()).collect();
        // Stable: two rows with one sequence number (which one process never
        // issues) keep their loaded order.
        own.sort_by_key(|i| i.lane_seq);
        for (&p, row) in at.iter().zip(own) {
            out[p] = Some(row);
        }
    }
    out.into_iter().flatten().collect()
}

impl WritePipeline {
    /// Replay durable write intents left by previous processes, spawned
    /// at attach.
    ///
    /// * **Order**: intents arrive from `load_session` sorted by
    ///   (`issued_ms`, `lane_seq`); [`replay_order`] then puts each issuing
    ///   process's intents in `lane_seq` order within the positions that
    ///   order gave the process, and they are applied strictly one at a time:
    ///   exact admission order within one issuing process (the per-lane
    ///   promise, since a total order refines every lane's) whatever its clock
    ///   did, wall-clock order across crashed processes (#295). An agent whose
    ///   intent is left unconsumed by a transient embedder answer is deferred
    ///   for the rest of the pass, so no later intent of it applies before
    ///   the one left behind (#295). Unless the pass stopped, the replay
    ///   runs once more over what it left owed, so each deferred agent gets
    ///   one more attempt in the attach; what is still owed after that the
    ///   next serve replays in order, and `replay_blocked` names why.
    /// * **Throttling**: replay runs sequentially in the background, with at
    ///   most one write in flight. It bypasses lane admission because each
    ///   intent already paid for it. Fresh writes wait for the producer and
    ///   its unconfirmed spans, so they cannot stage changes against rejected
    ///   replay effects.
    /// * **Idempotency**: consumption rides the same commit lock as the apply
    ///   (see [`WriteCtx::run`](super::WriteCtx::run)), so a `kill -9` mid-replay re-replays exactly
    ///   the intents whose applies did not flush — never one whose apply did.
    /// * **Liveness before anything is consumed**: one embed of
    ///   [`PROBE_TEXT`] gates the loop. If it fails, the task warns and returns
    ///   **without consuming a single intent** — the backlog stays durable and
    ///   the next serve tries again. A dead or hanging embedder at attach
    ///   therefore costs one embed, not one `HYBRID_IO_TIMEOUT` per intent.
    /// * **Failure**: a replay that fails **for its own content** consumes the
    ///   intent with a `failed` outcome, mirroring what the in-session worker
    ///   does, rather than retrying on every restart forever. That is
    ///   [`LamboError::Embed`] (the embedder answered and refused this input),
    ///   [`LamboError::ImageIdTaken`], a refused reference
    ///   ([`LamboError::Reference`]), an
    ///   [input refusal](crate::types::StoreError::is_input_refusal) (a cycle,
    ///   a repeated Observation, a second Hierarchical parent, a pair whose
    ///   ends resolve to one key or concept: #116, #294), a stored payload
    ///   this build cannot decode (#129), and a lookup timeout below
    ///   [`REPLAY_LOOKUP_TIMEOUT_LIMIT`]. So is an intent whose receipt id is
    ///   malformed in this build's own format (#295): it can be neither
    ///   applied nor answered for, and left unconsumed it was skipped, and
    ///   counted owed, on every serve. An id in another format (a newer
    ///   build's) is not settled, which could not be undone: it is
    ///   quarantined, left owed and warned about, and its agent deferred. A
    ///   replay that fails for a reason that says nothing about the intent
    ///   (the embedder unreachable or timed out, any other store error, the
    ///   lease lost, a configuration that cannot apply it) leaves it
    ///   **unconsumed** and ends the loop, except that an embedder outage
    ///   below [`EMBEDDER_SICK_THRESHOLD`] only defers that intent's agent:
    ///   those are conditions a later process can be in a position to fix,
    ///   and settling an acked write `failed` because a dependency blinked
    ///   would lose a write nothing was wrong with.
    /// * **Shutdown**: `close()` aborts this task before the quiesce; whatever
    ///   is still unconsumed stays durable for the next serve.
    pub(crate) fn spawn_replay(
        self: &Arc<Self>,
        intents: Vec<WriteIntent>,
        flush_wake: Option<Arc<tokio::sync::Notify>>,
    ) {
        if intents.is_empty() {
            return;
        }
        let intents = replay_order(intents);
        // Seed the cross-restart answers before the task starts, so a lookup
        // racing the replay sees `pending_replay` rather than `restart_lost`.
        //
        // A **consumed** row older than the retention window is
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
                    // What the replay will do with it, said only where it is
                    // true: a consumed row is not replayed at all.
                    let fate = if intent.outcome.is_some() {
                        "its recorded outcome cannot be looked up by that id"
                    } else if ReceiptId::is_other_version(&intent.receipt) {
                        "it is in another version of the receipt format (a newer build's?), so \
                         the replay leaves it owed (quarantined) and holds its agent's later \
                         intents behind it. Run the build that wrote it to replay it, or delete \
                         the row (`DELETE FROM write_intents WHERE session_id = ... AND receipt = \
                         ...`) to let the agent's later intents through"
                    } else {
                        "it is malformed, so the replay settles it failed"
                    };
                    tracing::warn!(
                        session = %self.ctx.session,
                        receipt = %intent.receipt,
                        agent = %intent.agent,
                        "write intent carries an unparseable receipt id: {fate}"
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
                            ReceiptAnswer::AppliedAfterRestart(summary, None, None)
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
        this.ctx.graph.write().replay_producing = true;
        let producer = ReplayProducer(this.ctx.graph.clone());
        let handle = tokio::spawn(async move {
            let _producer = producer;
            // Step 1 — the liveness gate. Nothing below may
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
            // The sequential decision rule that BOUNDS the loop. A fresh attach
            // starts with a cleared block reason so a drain that never faults
            // reports `null`.
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
            // #295: agents deferred for the rest of this attach, because one of
            // their intents got a transient embedder answer and stays durable.
            // Their later intents are not run, so none of them can apply
            // before it (the per-agent order `WriteIntent::agent` promises);
            // the next serve replays them all in order.
            let mut deferred: HashSet<AgentId> = HashSet::new();
            // #295 review: what a pass leaves owed (every intent of a deferred
            // agent, from the one it was deferred at), in order. Unless the
            // pass stopped, the replay runs once more over it, so each
            // deferred agent gets one more attempt in this serve rather than
            // waiting for the next one. Whether the pass deferred an agent
            // at a transient answer, and how many intents it quarantined,
            // decide the block reason a serve that ends with writes owed
            // reports.
            let mut queue: std::collections::VecDeque<WriteIntent> = pending.into();
            let mut left: Vec<WriteIntent> = Vec::new();
            let mut retried = false;
            let mut deferred_at_outage = false;
            let mut quarantined = 0usize;
            let mut halted = false;
            loop {
                let Some(intent) = queue.pop_front() else {
                    if retried || left.is_empty() {
                        break;
                    }
                    retried = true;
                    deferred.clear();
                    deferred_at_outage = false;
                    quarantined = 0;
                    tracing::info!(
                        session = %this.ctx.session,
                        left = left.len(),
                        "write queue: retrying, once, the durable intents of agents the replay \
                         deferred, from each one's oldest owed intent"
                    );
                    queue = std::mem::take(&mut left).into();
                    continue;
                };
                if this.ctx.lease_lost.load(Ordering::Acquire)
                    || this.lanes.lock().sealed
                    || this.ctx.graph.read().replay_flush.deferred()
                {
                    // This intent and every one not yet reached stay owed.
                    halted = true;
                    break;
                }
                if deferred.contains(&intent.agent) {
                    left.push(intent);
                    continue;
                }
                if ReceiptId::from_str(&intent.receipt).is_err()
                    && ReceiptId::is_other_version(&intent.receipt)
                {
                    // #295 review: an id in another version of the receipt
                    // format (`lwr<digits>.`, not `lwr1.`), as a newer build
                    // writes. Settling it `failed` could not be undone, so it
                    // is quarantined: left owed (counted in `replay_owed`, and
                    // named once per serve by the seeding warning, which says
                    // how to clear it), and its agent deferred so the agent's
                    // later intents do not overtake it.
                    quarantined += 1;
                    deferred.insert(intent.agent.clone());
                    left.push(intent);
                    continue;
                }
                let Ok(receipt) = ReceiptId::from_str(&intent.receipt) else {
                    // #295: settled `failed` like a payload this build cannot
                    // decode (#129). Left unconsumed it was skipped on every
                    // serve and counted in `replay_owed` forever; it cannot be
                    // applied (a job needs its id) or answered for (the
                    // receipt map is keyed by it).
                    let why = "replay after restart was refused (the stored receipt id does not \
                               parse, so this build cannot apply the write or answer for it); \
                               nothing was written"
                        .to_owned();
                    if !this.stage_replay_failure(&intent, None, &why, why.clone()) {
                        halted = true;
                        break;
                    }
                    failed += 1;
                    tracing::warn!(
                        session = %this.ctx.session,
                        receipt = %intent.receipt,
                        "write queue: a durable intent's receipt id does not parse; queued its \
                         failure outcome for flush"
                    );
                    continue;
                };
                let destination = intent.payload.receipt_destination().cloned();
                let (payload, decode_error) =
                    match JobPayload::from_intent_payload(intent.payload.clone()) {
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
                let completion = Arc::new(crate::store::replay_flush::Completion::new(
                    this.clock.clone(),
                ));
                let stamp = ConsumeStamp {
                    replay: Some(completion.clone()),
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
                    None => this.ctx.run(&job, Some(stamp), None).await,
                };
                match result {
                    Ok(summary) => {
                        // Observed health: the embedder just worked. Resets the
                        // consecutive-transient streak.
                        transient_streak = 0;
                        lookup_streak = 0;
                        applied += 1;
                        this.observe_replay_flush(
                            &completion,
                            &intent,
                            Some(receipt),
                            Some(summary),
                            None,
                        );
                    }
                    // #125: a vector-candidate lookup that
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
                        let why = format!(
                            "replay after restart was refused (the vector candidate lookup did \
                             not answer within {:?}, so the write is not retried again); \
                             nothing was written",
                            crate::graph::hybrid::VECTOR_LOOKUP_TIMEOUT
                        );
                        if !this.stage_replay_failure(&intent, Some(receipt), &why, why.clone()) {
                            halted = true;
                            break;
                        }
                        failed += 1;
                        tracing::warn!(
                            session = %this.ctx.session,
                            receipt = %intent.receipt,
                            error = %err,
                            "write queue: a durable intent's vector candidate lookup timed out \
                             again at replay; queued its failure outcome for flush"
                        );
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
                        // #295: skipping only this intent let the agent's next
                        // one apply first, and a later serve then applied
                        // this one after it. Defer the whole agent instead,
                        // as the image-`Config` arm stops for the lane order;
                        // the other agents' intents keep sampling.
                        deferred.insert(intent.agent.clone());
                        deferred_at_outage = true;
                        tracing::warn!(
                            session = %this.ctx.session,
                            receipt = %intent.receipt,
                            agent = %intent.agent,
                            error = %e,
                            streak = transient_streak,
                            "write queue: a durable intent's replay got a transient embedder \
                             answer; it and the rest of this agent's intents stay DURABLE and \
                             unconsumed, so they keep their order: the replay retries them once \
                             after this pass, and what is still owed then waits for the next \
                             serve. The sequential decision rule keeps sampling the embedder's \
                             health on other agents' intents"
                        );
                        left.push(intent);
                        continue;
                    }
                    // Content — absorbing: permanent FOR THIS INPUT. Consume it
                    // as `failed` immediately, exactly as the in-session worker
                    // would. It never counts toward embedder-sickness evidence,
                    // and the refusal proves the embedder just answered (resets
                    // the streak — observed aliveness).
                    //
                    // A cycle refusal (#116) is the same kind of fact:
                    // this write against this graph closes a cycle, every time.
                    // Before, it fell to the session-wide arm below, which left
                    // the intent durable and stopped the replay, so every later
                    // intent stayed `pending_replay` for good. #294: so is every
                    // other input refusal (an Observation derived again, a
                    // second Hierarchical parent, a pair whose ends resolve to
                    // one key or concept), which two writes acked back to back
                    // can meet only at apply. Narrow on purpose: any other
                    // `Invariant` can mean an inconsistent graph or store, and
                    // still stops the replay.
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
                            || matches!(&err, LamboError::Store(e) if e.is_input_refusal()) =>
                    {
                        // An input refusal is decided before any embed (or,
                        // at the commit, without one), so it proves nothing
                        // about the embedder's health; nor does a decode
                        // refusal.
                        if !decode_refused
                            && !matches!(&err, LamboError::Store(_) | LamboError::Reference(_))
                        {
                            transient_streak = 0;
                        }
                        // Same split as the in-session worker's failure
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
                        if !this.stage_replay_failure(&intent, Some(receipt), &why, detail) {
                            halted = true;
                            break;
                        }
                        failed += 1;
                        tracing::warn!(
                            session = %this.ctx.session,
                            receipt = %intent.receipt,
                            error = %e,
                            "write queue: a durable intent's replay was refused; queued its failure outcome for flush"
                        );
                    }
                    // #22: an image intent this process's
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
                }
                // The throttle: yield between jobs so a deep backlog cannot
                // monopolize the runtime between two of the fresh session's
                // polls.
                tokio::task::yield_now().await;
            }
            // #295 review: a serve that ends with intents owed and did not
            // stop says why, rather than reporting no block while a debt
            // stands: `Embedder` when its last pass deferred an agent at a
            // transient answer, `Other` when only quarantined intents hold
            // writes back.
            // A halt (the lease lost, or the session closing) leaves the
            // intent it stopped at, and every one after it, owed: `Other`, the
            // reason a lost lease already has, rather than `null`.
            if halted && this.counters.replay_blocked() == ReplayBlockReason::None {
                this.counters.set_replay_blocked(ReplayBlockReason::Other);
            }
            if !halted
                && !left.is_empty()
                && this.counters.replay_blocked() == ReplayBlockReason::None
            {
                let reason = if deferred_at_outage {
                    ReplayBlockReason::Embedder
                } else {
                    ReplayBlockReason::Other
                };
                this.counters.set_replay_blocked(reason);
                tracing::warn!(
                    session = %this.ctx.session,
                    owed = left.len(),
                    quarantined,
                    reason = ?reason,
                    "write queue: the replay ended with durable intents still owed: agents whose \
                     intent the embedder did not answer at two attempts, or that wait behind an \
                     intent in another version of the receipt format (named at startup); they \
                     stay durable, in order, for the next serve"
                );
            }
            // Replay receipts wait for the store. Wake the flusher after
            // staging so a long configured interval cannot delay those answers
            // when the store is healthy.
            if let Some(wake) = flush_wake {
                wake.notify_one();
            }
            tracing::info!(
                session = %this.ctx.session,
                staged_applied = applied,
                staged_failed = failed,
                deferred_agents = deferred.len(),
                owed = this.counters.replay_owed.load(Ordering::Relaxed),
                "write queue: finished staging durable write intent outcomes"
            );
        });
        *self.replay.lock() = Some(handle);
    }

    fn stage_replay_failure(
        self: &Arc<Self>,
        intent: &WriteIntent,
        receipt: Option<ReceiptId>,
        reason: &str,
        detail: String,
    ) -> bool {
        use crate::store::replay_flush::{Checkpoint, Completion};

        let mut graph = self.ctx.graph.write();
        if graph.replay_flush.deferred() {
            return false;
        }
        let completion = Arc::new(Completion::new(self.clock.clone()));
        self.observe_replay_flush(&completion, intent, receipt, None, Some(detail));
        let start = Checkpoint::capture(&graph);
        graph.consume_replayed_write(
            intent.receipt.clone(),
            WriteIntentOutcome {
                tag: "failed".into(),
                summary: super::receipts::durable_summary(
                    reason,
                    intent.payload.receipt_destination(),
                ),
                consumed_at: (self.clock)(),
            },
            start,
            completion,
        );
        true
    }

    /// Publish terminal receipt and ledger outcomes only after their consume
    /// reaches the store. An abandoned speculative span leaves the seeded
    /// pending answer and the durable replay debt in place.
    fn observe_replay_flush(
        self: &Arc<Self>,
        completion: &crate::store::replay_flush::Completion,
        intent: &WriteIntent,
        receipt: Option<ReceiptId>,
        applied: Option<super::AppliedSummary>,
        failure_detail: Option<String>,
    ) {
        let pipeline = Arc::downgrade(self);
        let stored_receipt = intent.receipt.clone();
        let agent = intent.agent.clone();
        let destination = intent.payload.receipt_destination().cloned();
        completion.on_finish(Box::new(move |outcome| {
            let Some(pipeline) = pipeline.upgrade() else {
                return;
            };
            let Some(outcome) = outcome else {
                if pipeline.counters.replay_blocked() == ReplayBlockReason::None {
                    pipeline
                        .counters
                        .set_replay_blocked(ReplayBlockReason::Other);
                }
                return;
            };
            let rejected = outcome.tag == "failed";
            let (answer, facts) = if rejected {
                let (why, _) = super::receipts::unpack_durable_summary(&outcome.summary);
                let facts =
                    json!({ "error": failure_detail.as_deref().unwrap_or(&why), "replay": true });
                (ReceiptAnswer::Failed(why), facts)
            } else {
                let summary = applied.expect("an applied replay has a commit summary");
                let facts = serde_json::Value::Object(summary.ledger_facts());
                (
                    ReceiptAnswer::AppliedAfterRestart(
                        summary.summary,
                        summary.stale_report,
                        summary.replacement,
                    ),
                    facts,
                )
            };
            if let Some(ledger) = &pipeline.ctx.ledger {
                ledger.append(&crate::ledger::completion_line(
                    &agent.to_string(),
                    &stored_receipt,
                    &outcome.tag,
                    Some(facts),
                ));
            }
            if let Some(receipt) = receipt {
                pipeline.settle_replay(receipt, agent, destination, answer, outcome.consumed_at);
            }
            if !rejected {
                pipeline.counters.replayed.fetch_add(1, Ordering::Relaxed);
            }
            pipeline
                .counters
                .replay_owed
                .fetch_sub(1, Ordering::Relaxed);
            pipeline.settled.notify_waiters();
        }));
    }

    fn settle_replay(
        &self,
        receipt: ReceiptId,
        agent: AgentId,
        destination: Option<crate::types::SessionId>,
        answer: ReceiptAnswer,
        consumed_at: chrono::DateTime<chrono::Utc>,
    ) {
        // Durable rows carry only the summary. The bounded live entry
        // carries the replay report and participates in normal eviction.
        let durable_answer = match &answer {
            ReceiptAnswer::AppliedAfterRestart(summary, ..) => {
                ReceiptAnswer::AppliedAfterRestart(summary.clone(), None, None)
            }
            other => other.clone(),
        };
        if destination.is_some() {
            let mut receipts = self.receipts.lock();
            let now = (self.clock)();
            receipts.entries.insert(
                receipt,
                super::receipts::Entry {
                    agent: agent.clone(),
                    deliver_to: destination
                        .as_ref()
                        .filter(|d| **d != self.ctx.session)
                        .cloned(),
                    settled_at: Some(consumed_at),
                    answer,
                },
            );
            receipts.order.push_back(receipt);
            if destination.is_some() {
                receipts
                    .undelivered
                    .entry(agent.clone())
                    .or_default()
                    .push_back(receipt);
            }
            receipts.expire(now);
            receipts.evict();
        }
        self.restart
            .lock()
            .insert(receipt, (agent, destination, durable_answer));
    }

    /// Stop the replay task: abort **and join**, because an aborted task
    /// can still finish a synchronous stretch — and append to the log — until
    /// the join returns. `close()` calls this before its
    /// final drain so no replay write can land after the drain's last word.
    /// Unconsumed intents stay durable for the next serve.
    pub(crate) async fn stop_replay(&self) {
        let handle = self.replay.lock().take();
        if let Some(handle) = handle {
            handle.abort();
            // Custody until the join returns: a `close()` cancelled
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

/// Release the producer latch on every exit, including an aborted startup.
struct ReplayProducer(Arc<parking_lot::RwLock<crate::graph::Graph>>);

impl Drop for ReplayProducer {
    fn drop(&mut self) {
        self.0.write().replay_producing = false;
    }
}

pub(crate) fn replay_flush_failure(summary: &str, constraint: &str) -> String {
    let (_, destination) = super::receipts::unpack_durable_summary(summary);
    super::receipts::durable_summary(
        &format!(
            "replay after restart was refused (store constraint {constraint}); nothing was written"
        ),
        destination.as_ref(),
    )
}
