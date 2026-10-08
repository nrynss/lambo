//! Drain coordination: what `Memory::close` and `Memory`'s `Drop` do to the
//! queue.
//!
//! `close()` calls, in this order (see `memory/shutdown.rs`): the probe abort,
//! `stop_replay` (abort **and join**), then [`WritePipeline::quiesce`], and
//! only then takes the writers gate. The order is forced: the gate's write side
//! is held for the rest of `close()`, so a worker that needed the gate could
//! never finish. Workers therefore never touch the gate; sealing the lanes is
//! what stops new jobs.
//!
//! Invariants kept here:
//!
//! * the quiesce is bounded by [`WRITE_QUEUE_DRAIN_BUDGET`]; what does not
//!   drain is **deferred, not lost** — workers are aborted **and joined**, and
//!   every unsettled receipt is settled `intent_durable` (its durable intent
//!   was recorded at admission and the final flush persists it);
//! * `Drop` cannot await, so [`WritePipeline::abort_all_sync`] aborts without
//!   joining and settles nothing.

use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::json;
use tokio::task::JoinHandle;

use parking_lot::Mutex as PlMutex;

use super::{Job, Lanes, ReceiptAnswer, ReceiptId, WritePipeline};
use crate::types::AgentId;

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

impl WritePipeline {
    /// Drain the pipeline for `close()`.
    ///
    /// Called **before** `close()` takes the writers gate, and that order is
    /// forced rather than chosen: the gate's write side is held for the rest of
    /// `close()`, so a worker that had to pass through the gate could never
    /// finish, and a `close()` waiting for it would deadlock. The workers
    /// therefore do not use the gate at all — this quiesce is what makes
    /// "nothing new lands after the drain" true of them.
    ///
    /// Bounded by [`WRITE_QUEUE_DRAIN_BUDGET`]. (This said "which is the same
    /// number admission promised"; admission stopped promising a drain time at
    /// the J3 redesign.) Anything still outstanding when it runs out is
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
        // `drainable`, not `outstanding`: after a cancelled `abort_workers`
        // the aborted workers still count as running but will never settle,
        // so a retried close would otherwise sleep out the whole budget
        // before joining them.
        while self.lanes.lock().drainable() > 0 {
            // `enable()` before the re-check, for the reason in
            // `WritePipeline::wait`: an un-polled `Notified` is not a
            // registered waiter, so a settle landing here would be missed and
            // the quiesce would burn its whole budget on an empty queue.
            let mut notified = Box::pin(self.settled.notified());
            notified.as_mut().enable();
            if self.lanes.lock().drainable() == 0 {
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
            // Set before any await: from here on nothing can drain (the
            // queues empty below, the workers are aborted next), so a quiesce
            // retried after this call is cancelled must not wait for a settle.
            lanes.workers_aborted = true;
            let handles: Vec<(AgentId, JoinHandle<()>)> = lanes.workers.drain().collect();
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
        // Abort every worker before joining any, and keep the handles in
        // custody until each join returns (R3-1, as `Memory::close` does for
        // its own tasks). A join is pending while its worker is mid-poll on
        // another thread, and `close()` may be cancelled right there: joining
        // one at a time used to drop the rest un-aborted, still applying jobs,
        // and a retried close found no handle left to wait for.
        for (_, handle) in &handles {
            handle.abort();
        }
        let mut custody = WorkerCustody {
            lanes: &self.lanes,
            handles,
        };
        while let Some((_, handle)) = custody.handles.last_mut() {
            let _ = handle.await;
            custody.handles.pop();
        }
        drop(custody);
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

/// Custody of lane-worker handles while [`WritePipeline::abort_workers`] joins
/// them: whatever is not yet joined when the future is dropped goes back to
/// `Lanes::workers`, already aborted, so the next `abort_workers` (a retried
/// `close()`) joins it instead of returning past a task that is still running.
/// The twin of `memory::shutdown::HandleCustody`.
struct WorkerCustody<'a> {
    lanes: &'a PlMutex<Lanes>,
    handles: Vec<(AgentId, JoinHandle<()>)>,
}

impl Drop for WorkerCustody<'_> {
    fn drop(&mut self) {
        if self.handles.is_empty() {
            return;
        }
        let mut lanes = self.lanes.lock();
        for (agent, handle) in self.handles.drain(..) {
            lanes.workers.insert(agent, handle);
        }
    }
}
