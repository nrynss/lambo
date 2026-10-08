//! Shutdown and custody: `Memory::close`, `Drop`, and the two custody guards
//! that make a cancelled or failed close lose nothing.
//!
//! All of the shutdown orchestration is in [`Memory::close`], in one function,
//! so its order can be read top to bottom. The steps, in order, with the name
//! each one logs under:
//!
//! 1. `serialize`: serialize on `close_state` (a concurrent close parks
//!    here); latch `closed`;
//! 2. `replay_stop`: abort the write queue's probe, stop the intent replay
//!    (abort and join);
//! 3. `queue_quiesce`: drain the write queue (bounded; what is left is
//!    deferred as `intent_durable`; its workers are aborted and joined);
//! 4. `writers_gate`: take the writers gate's write side (waits out
//!    in-flight writes);
//! 5. `heartbeat_abort`: abort the lease heartbeat; a fenced handle stops
//!    here, drops its tail and does not release the lease;
//! 6. `producer_joins`: stop canonization, then the daemon (abort **and
//!    join**, each in a [`HandleCustody`]);
//! 7. `flush_join`: stop the flush task and join it (in a [`HandleCustody`]);
//! 8. `final_drain`: under one graph write lock, close the access ledger
//!    (#30), apply its accesses, drain the log plus the dirty accesses into
//!    one batch held by a [`TailCustody`]; a degraded session errors here,
//!    before the empty-log shortcut;
//! 9. `final_flush`: flush the tail with [`final_flush`] (timeout + panic
//!    containment); skipped for an empty tail;
//! 10. `lease_release`: only after a durable tail, release the lease and
//!     latch success.
//!
//! # Step logging (#40)
//!
//! Every step logs `close: step N/10 <name> started` and `close: step N/10
//! <name> finished in M ms` at INFO, with the session. A step whose future
//! is dropped before it finishes (an outer timeout abandoned the close) logs
//! `close: step N/10 <name> abandoned after M ms` at WARN, so an abandoned
//! close names the step it was abandoned in. Inside `lambo serve` these
//! lines sit inside shutdown stage 3 (`src/mcp/serve/stages.rs`).
//!
//! `lambo serve` stops its own keep-warm task *before* calling `close()`
//! (`stop_before_close`, #13); that ordering lives on the serve side and is
//! pinned by `memory::tests::shutdown::the_keep_warm_is_stopped_before_the_close_starts`.

use std::sync::atomic::Ordering;
use std::time::Instant;

use parking_lot::{Mutex as PlMutex, RwLock};
use tokio::task::JoinHandle;

use super::Memory;
use crate::graph::Graph;
use crate::store::flush::{panic_message, CatchUnwindPoll, FLUSH_ATTEMPT_TIMEOUT};
use crate::store::GraphStore;
use crate::types::{LamboError, MutationBatch, StoreError};

impl Drop for Memory {
    /// Abort any task [`Memory::close`] did not stop, and say so if a tail dies
    /// with this handle.
    ///
    /// This is the leak guard, not the shutdown path: dropping without `close`
    /// abandons the tail (see `close`'s drain), so it warns. After a successful
    /// `close` every handle is already `None` and this is a no-op.
    ///
    /// **Two ways to lose a tail, not one** (R2-2, amended by R3-1/R4-1).
    /// Keying the warning on task handles still being `Some` catches the
    /// never-closed handle but is blind to the *closed-and-failed* one: a
    /// `close()` that **failed** has reaped all three handles, so `leaked` is
    /// false — while the mutations it kept are sitting in the log, about to be
    /// dropped in silence. Precisely the case `close`'s "retry after failure"
    /// contract asks the owner to act on, so it must not go out quietly. The
    /// log is therefore checked too, whatever the handles say.
    ///
    /// A **cancelled** `close()` is the third shape (R4-1): `HandleCustody`
    /// has put the handles *back*, so `leaked` is true and the first branch
    /// fires — but its count can understate the loss, because a tail drained
    /// by the flush task before the cancellation lives in that task's
    /// `pending`, not in the log this counts. The first message says so
    /// rather than pretending the log count is the whole story.
    fn drop(&mut self) {
        // No-op if a successful `close()` already released the slot (R2-4).
        self.unregister_once();
        // Stop the lease heartbeat so a leaked handle stops refreshing — its
        // lease then lapses at TTL and the session becomes takeable (T8.6). The
        // lease itself is NOT released here: `Drop` cannot `await` the store, and
        // a handle dropped without a clean close is the crash-shaped path where
        // expiry is the right release. A successful `close()` already released it.
        self.abort_heartbeat();
        // J3: the background write workers and the calibration probe hold `Arc`
        // clones of the graph, so a dropped handle's workers would keep writing
        // into a graph nobody will flush. Aborted without a join and without
        // settling their receipts — `Drop` cannot await, and a process that is
        // going away has nobody to answer. This is the same shape as the tail
        // this method warns about: a handle dropped without a clean close loses
        // its un-applied writes, which is exactly what the receipt for one says
        // when the next process is asked (`restart_lost`).
        self.pipeline.abort_all_sync();
        let mut leaked = false;
        for handle in [&self.daemon_handle, &self.flush_handle, &self.canon_handle] {
            if let Some(handle) = handle.lock().take() {
                handle.abort();
                leaked = true;
            }
        }
        let undrained = self.graph.read().log_len();
        if leaked {
            tracing::warn!(
                session = %self.session,
                mutations = undrained,
                "Memory dropped with live background tasks (never closed, or a close() was \
                 cancelled): tasks aborted and {undrained} un-flushed mutations in the log were \
                 discarded — after a cancelled close(), mutations held in the flush task's \
                 buffer are lost as well and are not in this count"
            );
        } else if undrained > 0 {
            tracing::warn!(
                session = %self.session,
                mutations = undrained,
                "Memory dropped after a close() that did not finish: {undrained} un-flushed \
                 mutations were discarded. close() returned an error (or was cancelled) and \
                 kept that tail in the log for a retry that never came."
            );
        }
    }
}

/// Custody of a background task's [`JoinHandle`] while `close()` stops and
/// reaps it — R3-1.
///
/// `close()` used to lift each handle out of its slot (`slot.lock().take()`)
/// and then `await` it as a bare local. That await is the long one — the flush
/// join is what `close`'s "worst case ≈ 2 minutes" measures, and an external
/// [`timeout`](tokio::time::timeout) around `close()` is the posture its own
/// docs invite. Dropping the future there dropped the local `JoinHandle`, which
/// **detaches** the task rather than stopping it: the flush loop kept running,
/// kept its `pending` buffer — which holds the tail, the log having already
/// been drained into it — and kept writing the session through its own `Arc`s.
/// The slot was left `None`, so the retried `close()` skipped the join, drained
/// an empty log, took the empty-log shortcut and returned `Ok(())` over a tail
/// that was neither durable nor anywhere [`Drop`]'s R2-2 warning could see it
/// (the log was empty because the zombie held the batch). COH-6 clause 13 — "a
/// retained batch is never silently lost" — by the same route.
///
/// So a handle is never a bare local either. This guard owns it from the take
/// until [`HandleCustody::join`] sees the task actually finish, and its `Drop`
/// returns an un-reaped handle to its slot. A `JoinHandle` whose poll was
/// cancelled is re-awaitable, so the retry re-joins *that* task, waits out its
/// in-flight attempt and collects its `requeue_pending` (COH-6): the tail is
/// back on the log before step 3 drains, and the empty-log shortcut is never
/// reached with a live flush task behind it.
///
/// **All three handles, not only the flush one.** The daemon and canonization
/// handles are `abort()`ed before their join, and `abort()` is a synchronous
/// fire — cancellation cannot land between the take and the abort, because
/// there is no await between them. What the abort does *not* buy is that the
/// task has stopped: tokio cancels an already-running task at its next
/// `.await`, so an aborted producer can still finish a synchronous stretch, and
/// that stretch can append to the graph log. Only the join proves it is over.
/// Detached at its join, such a task is left running while the retry goes
/// straight to the drain — the same `Ok(())`-over-a-lost-mutation shape as the
/// flush case, through a narrower window. Same guard, same reason.
///
/// Like `TailCustody`, the `parking_lot` guard is taken for one statement and
/// never across an `.await` (§6.4): `join` holds nothing while it waits.
///
/// **Composition with `TailCustody`.** `close()` drops each of these
/// explicitly once its join has returned, so at most one custody guard is ever
/// live and the two never overlap: a cancellation at step 2 restores a handle
/// and no tail exists yet; a cancellation at step 4 restores the tail and every
/// handle is already reaped. Both orders end the same way — every guard is a
/// local declared *after* `_quiesced` and the `close_state` guard, so both run
/// before the retry can enter `close()` at all. R2-1's rule ("the tail is back
/// on the log before `close_state` releases") is unchanged, and R3-1's is its
/// twin one step earlier.
pub(super) struct HandleCustody<'a> {
    pub(super) slot: &'a PlMutex<Option<JoinHandle<()>>>,
    /// `None` once [`HandleCustody::join`] has reaped the task — that is what
    /// tells `Drop` there is nothing to hand back.
    pub(super) handle: Option<JoinHandle<()>>,
}

impl<'a> HandleCustody<'a> {
    /// Lift the handle out of `slot`. The slot stays empty only for as long as
    /// this guard lives.
    fn take(slot: &'a PlMutex<Option<JoinHandle<()>>>) -> Self {
        let handle = slot.lock().take();
        Self { slot, handle }
    }

    /// Signal cancellation. Synchronous, so no cancellation of `close()` can
    /// land between this and the [`HandleCustody::join`] that follows it.
    fn abort(&self) {
        if let Some(handle) = &self.handle {
            handle.abort();
        }
    }

    /// Wait for the task to finish; `None` if the slot was already empty (an
    /// earlier `close()` reaped it).
    ///
    /// Custody ends only when the join **returns**. Cancelled mid-poll, the
    /// handle is still owned here and `Drop` puts it back.
    async fn join(&mut self) -> Option<Result<(), tokio::task::JoinError>> {
        let outcome = self.handle.as_mut()?.await;
        self.handle = None;
        Some(outcome)
    }
}

impl Drop for HandleCustody<'_> {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            *self.slot.lock() = Some(handle);
        }
    }
}

/// Custody of the tail between `close()`'s drain (step 3) and the moment it is
/// durable (step 4) — R2-1.
///
/// Between those two points the mutations exist **only** as a local inside
/// `close()`: they are out of the graph log and the flush task that owned the
/// other copy has already been joined. `close()` is an ordinary future, so a
/// caller that wraps it in `tokio::time::timeout` or drops it out of a
/// `select!` — the posture `close`'s own "How long it can take" section invites,
/// and which this crate's own shutdown test uses — destroys that local mid-flush.
/// The tail then existed nowhere: the log was empty, so the *next* `close()`
/// drained nothing, took the empty-log shortcut and returned `Ok(())` over
/// mutations nobody ever wrote.
///
/// So the batch is never a bare local. This guard owns it from the drain until
/// [`TailCustody::durable`] is called, and its `Drop` — which runs on
/// cancellation exactly as it runs on the error path — hands it back to the
/// front of the log. Cancel a `close()` and the tail is where it started, for
/// the retry (or for `Drop`'s R2-2 warning) to find.
///
/// `Drop` is synchronous and takes the `parking_lot` write lock for one
/// statement, never across an `.await` (§6.4). Re-appending a batch whose flush
/// may have partly landed is the same bet the failure path already makes: a
/// mutation batch is replayed, and replay is idempotent by the `src/graph/mod.rs`
/// contract.
pub(super) struct TailCustody<'a> {
    pub(super) graph: &'a RwLock<Graph>,
    pub(super) batch: MutationBatch,
    /// Set by [`TailCustody::durable`]; suppresses the hand-back.
    pub(super) durable: bool,
}

impl<'a> TailCustody<'a> {
    fn new(graph: &'a RwLock<Graph>, batch: MutationBatch) -> Self {
        Self {
            graph,
            batch,
            durable: false,
        }
    }

    fn batch(&self) -> &MutationBatch {
        &self.batch
    }

    fn len(&self) -> usize {
        self.batch.len()
    }

    fn is_empty(&self) -> bool {
        self.batch.is_empty()
    }

    /// The store took it: end custody, so `Drop` does not put a durable batch
    /// back on the log (which would flush it twice and leave `log_depth`
    /// claiming an undurable tail).
    fn durable(&mut self) {
        self.durable = true;
    }
}

impl Drop for TailCustody<'_> {
    fn drop(&mut self) {
        if self.durable {
            return;
        }
        // `push_front_log` is a no-op on an empty batch, so the empty-log and
        // degraded-with-empty-log paths cost nothing here.
        self.graph
            .write()
            .push_front_log(std::mem::take(&mut self.batch.mutations));
    }
}

/// How many steps [`Memory::close`] logs; the denominator in every line.
const CLOSE_STEPS: u8 = 10;

/// One logged step of [`Memory::close`] (#40): `started` when it is made,
/// `finished in N ms` from [`CloseStep::done`], and `abandoned after N ms` at
/// WARN if it is dropped first, which is what an outer timeout or a second
/// signal does to a close in flight.
pub(super) struct CloseStep<'a> {
    session: &'a crate::types::SessionId,
    number: u8,
    name: &'static str,
    started: Instant,
    done: bool,
}

impl<'a> CloseStep<'a> {
    fn start(session: &'a crate::types::SessionId, number: u8, name: &'static str) -> Self {
        tracing::info!(
            session = %session,
            step = number,
            step_name = name,
            "close: step {number}/{CLOSE_STEPS} {name} started"
        );
        Self {
            session,
            number,
            name,
            started: Instant::now(),
            done: false,
        }
    }

    fn done(mut self) {
        self.done = true;
        let elapsed_ms = self.started.elapsed().as_millis();
        tracing::info!(
            session = %self.session,
            step = self.number,
            step_name = self.name,
            elapsed_ms,
            "close: step {}/{CLOSE_STEPS} {} finished in {elapsed_ms} ms",
            self.number,
            self.name,
        );
    }
}

impl Drop for CloseStep<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let elapsed_ms = self.started.elapsed().as_millis();
        tracing::warn!(
            session = %self.session,
            step = self.number,
            step_name = self.name,
            elapsed_ms,
            "close: step {}/{CLOSE_STEPS} {} abandoned after {elapsed_ms} ms (the close was \
             dropped before this step finished)",
            self.number,
            self.name,
        );
    }
}

/// `close()`'s step-4 store attempt, armored exactly like a background one
/// (T81-2).
///
/// The flush loop protects every `store.flush` twice — `FLUSH_ATTEMPT_TIMEOUT`
/// (STORE-2) and [`CatchUnwindPoll`] — and both rationales apply verbatim to the
/// final flush, which runs against the same caller-supplied adapter:
///
/// * **Timeout.** Without it a hung store hangs `close()` forever, and the
///   handle's tail is stuck behind a call that will never return. The same
///   constant, not a close-specific one: this is one `store.flush` attempt on
///   the same store, so the bound STORE-2 chose for an attempt is the bound
///   here. `close` makes exactly one attempt (no retry ladder), so 30s is the
///   whole of it.
/// * **Panic containment.** Without it a panicking adapter unwinds out of
///   `close` *after* `closed` latched and the log drained — the tail would be
///   unrecoverable even for a caller that catches the panic. Contained, it is
///   an ordinary error and the caller's batch goes back on the log.
///
/// Dropping the timed-out future is safe for the same reason it is in the loop:
/// the adapter only borrows `&MutationBatch`, which the caller still owns.
pub(super) async fn final_flush(
    store: &dyn GraphStore,
    batch: &MutationBatch,
    token: Option<u64>,
) -> Result<(), StoreError> {
    let attempt = async {
        match CatchUnwindPoll(async { store.flush(batch, token).await }).await {
            Ok(result) => result,
            Err(payload) => {
                let message = panic_message(&payload);
                tracing::error!(
                    panic = %message,
                    "close: store.flush panicked during the final flush; treating it as a failed \
                     flush (the tail returns to the graph log)"
                );
                Err(StoreError::Backend(format!(
                    "close: store flush panicked: {message}"
                )))
            }
        }
    };
    match tokio::time::timeout(FLUSH_ATTEMPT_TIMEOUT, attempt).await {
        Ok(result) => result,
        Err(_elapsed) => Err(StoreError::Backend(format!(
            "close: store flush timed out after {FLUSH_ATTEMPT_TIMEOUT:?}"
        ))),
    }
}

impl Memory {
    /// Final flush + clean shutdown (spec §6.1): the write queue, the lease
    /// heartbeat, and the three background tasks (daemon, flush,
    /// canonization). The module doc lists the stages in order.
    ///
    /// Idempotent **after success**: once a close has made the tail durable
    /// every later call is `Ok(())` and does nothing.
    ///
    /// ## Concurrent and repeated calls
    ///
    /// The body is serialized. A second caller that arrives while a close is in
    /// flight **parks until it finishes** and then returns its outcome — it
    /// never gets an early `Ok` over an in-flight final flush (which, if it
    /// gated process exit, would let runtime teardown cancel that flush and
    /// lose the tail).
    ///
    /// ## Retry after failure
    ///
    /// A close that fails is **retryable, and says so by staying failed**: the
    /// drained batch goes back to the front of the graph log (where the next
    /// `drain_log` finds it, in order), the failure is returned, and the
    /// success flag is *not* set. Call `close()` again — after the store
    /// recovers — and the same tail is flushed. Repeated calls keep returning
    /// the failure for as long as the tail is undurable; `Ok(())` from
    /// `close()` always means "the tail is written".
    ///
    /// The session is closed to writers from the first call regardless: the
    /// background tasks are stopped and every mutating method is refused, so a
    /// retry re-attempts exactly the same tail rather than a growing one.
    ///
    /// ## Bounding this against the lease — caller's contract (T86-4)
    ///
    /// `close()` aborts the lease heartbeat as soon as it holds the writers
    /// gate (after latching `closed`, stopping the probe and the intent
    /// replay, and draining the write queue; before the background tasks),
    /// because the success paths below release the lease explicitly and it
    /// must not keep being refreshed underneath that release. (This said
    /// "**first** (right after latching `closed`)", which stopped being true
    /// when J3 put the queue drain ahead of the gate.) Until the gate is held
    /// the lease keeps refreshing: through the replay stop and its join, the
    /// quiesce (bounded by `WRITE_QUEUE_DRAIN_BUDGET`) plus the joins of the
    /// workers it aborts, and the wait for the gate's write side, which an
    /// unbounded embedder can stretch indefinitely (the R2-5 paragraph
    /// below). That window has no fixed bound, but extra refresh is the
    /// safe direction: the lease only outlives a close that is still running.
    ///
    /// From that moment the lease is no longer refreshed, so it stays valid
    /// only for its **remaining TTL** — at worst one
    /// [`LEASE_HEARTBEAT_INTERVAL`](crate::store::lease::LEASE_HEARTBEAT_INTERVAL)
    /// short of a full [`LEASE_TTL`](crate::store::lease::LEASE_TTL) (≈30s) if
    /// the last beat landed just before close.
    ///
    /// This method is otherwise **unbounded**: the step-2 flush-task join and the
    /// step-4 final flush each have their own internal timeout ladders, but their
    /// composition has no single wall-clock cap. A `close()` whose final flush
    /// runs longer than that remaining validity therefore lets the lease
    /// **expire while this handle is still flushing its tail**, which can admit a
    /// second writer mid-flush — the exact window the lease exists to close.
    ///
    /// **The `serve` path avoids this by bounding `close()` below the TTL.**
    /// [`crate::mcp::serve`](mod@crate::mcp::serve) caps its close at `CLOSE_GRACE` (10s) inside a
    /// `SHUTDOWN_BUDGET` (15s), and a build-time assertion pins
    /// `LEASE_TTL (45s) > SHUTDOWN_BUDGET`, so the lease is provably still valid
    /// when `release` lands. **A direct library caller gets no such bound** and
    /// **MUST** cap `close()` — e.g. under [`tokio::time::timeout`] — below the
    /// lease's remaining validity, exactly as `serve` does, if two processes may
    /// contend on the session. (Reordering the heartbeat to keep refreshing
    /// across the flush was considered and rejected: it entangles the heartbeat's
    /// lost-lease fence with the mid-close release/flush ordering, and this
    /// close body's ordering is load-bearing for the R2-1/R3-1 cancellation and
    /// custody invariants above. Bounding at the call site is the smaller-risk
    /// contract, and in-repo `serve` is the only production caller.)
    ///
    /// ## Cancellation
    ///
    /// **Dropping this future never destroys the tail** (R2-1). A caller that
    /// wraps `close()` in a [`timeout`](tokio::time::timeout) or drops it out of
    /// a `select!` leaves the drained mutations back at the front of the graph
    /// log — the state a *failed* close leaves, and retryable the same way. The
    /// session stays closed to writers, `succeeded` stays unset, and the next
    /// `close()` re-drains and re-flushes exactly that tail. A cancelled close
    /// can therefore never be followed by an `Ok(())` that did not write it.
    ///
    /// Cancelling mid-flush may still have let the store apply the batch: the
    /// retry replays it, which the `src/graph/mod.rs` replay contract makes
    /// idempotent (the failure path has always made the same bet).
    ///
    /// **Nor does it strand a background task** (R3-1). Cancellation lands on
    /// whichever `.await` this future is parked on, and the longest of those is
    /// the step-2 join — the one an external `timeout` almost always fires in.
    /// A dropped `JoinHandle` *detaches* its task rather than stopping it, so
    /// that used to leave a live flush loop still holding the tail in its own
    /// `pending` buffer while the retry — finding an empty slot and an empty log
    /// — took the shortcut below and returned `Ok(())`. Every handle therefore
    /// travels in a `HandleCustody` guard that returns it to its slot unless
    /// the join actually completed, so a retry re-joins the same task and picks
    /// up the tail it requeues. The invariant that falls out of it —
    /// no success is ever latched over an un-joined flush task — is asserted in
    /// `Memory::latch_success`.
    ///
    /// ## The drain (COH-6)
    ///
    /// `FlushTask` owns its `pending` buffer, so a hard
    /// [`JoinHandle::abort`](tokio::task::JoinHandle::abort) on it would drop
    /// every mutation drained from the log but not yet durable — above all a
    /// batch RETAINED after a failed flush, which sits at the front of that
    /// buffer. So:
    ///
    /// 0. **Shut the writers up — the surface's own, then the tasks'.** Latch
    ///    `closed` so new calls are refused, drain J3's asynchronous write
    ///    queue (see the note at the end of this step), take the write side of
    ///    `Memory::writers`, which waits out every write already in flight on
    ///    a caller task (T81-1), and only then stop the two mutation producers,
    ///    canonization first and the daemon second. It takes both halves for
    ///    "nothing new lands after the drain" to be true of the *surface* and
    ///    not just of the tasks. `abort()` is safe for both tasks: neither
    ///    holds a `parking_lot` guard across an `.await`, and the write-behind
    ///    log carries any canonization hop whose phase-4 record was cancelled.
    ///    Both are then **joined**, not merely aborted: tokio cancels a running
    ///    task at its next `.await`, so until the join returns an aborted
    ///    producer can still finish a synchronous stretch — and append to the
    ///    log (R3-1).
    ///
    ///    **J3's write pipeline is drained inside this step, and BEFORE the
    ///    gate is taken** (`crate::writeq::WritePipeline::quiesce`). The
    ///    order is forced, not chosen: the gate's write side is held for the
    ///    rest of this method, so a background worker that had to pass through
    ///    the gate could never finish and a `close` waiting for it would
    ///    deadlock. The workers therefore never touch the gate — latching
    ///    `closed` is what stops new jobs, and the quiesce is what makes
    ///    "nothing new lands after the drain" true of the workers. Bounded by
    ///    [`crate::writeq::WRITE_QUEUE_DRAIN_BUDGET`]; anything still
    ///    outstanding is **deferred, not lost**: workers are aborted **and
    ///    joined**, each pending receipt is settled `intent_durable`, the count
    ///    lands in `lambo_stats`' `write_queue_deferred`, and the next serve
    ///    replays the durable intents. (This said "abandoned ... settled
    ///    `failed` ... `write_queue_abandoned`", the pre-durable-intent
    ///    behaviour; `abandoned` now counts only jobs refused after a lost
    ///    lease.) The intent replay is stopped, aborted and joined, before the
    ///    quiesce.
    /// 1. [`FlushTask::stop`](crate::store::flush::FlushTask::stop) — the loop finishes its current `cycle()` (an
    ///    in-flight flush and its retry/backoff complete; a post-retry
    ///    `RETAINED_BACKOFF` hold is *not* waited out), re-appends `pending` to
    ///    the **front** of the graph log, and exits.
    /// 2. Await its handle — the task is gone and can no longer take the graph
    ///    lock, so step 3 races nothing. The handle is held in a
    ///    `HandleCustody` guard for the whole of that await, so a cancelled
    ///    join returns it to its slot instead of detaching the task (R3-1).
    /// 3. Take the graph lock, `drain_log()`, release. The batch is handed
    ///    straight to a `TailCustody` guard, which returns it to the log if
    ///    this future is dropped before step 4 makes it durable (R2-1).
    /// 4. `store.flush(&batch)` directly, with **no lock held**, armored like
    ///    every background attempt is: a `FLUSH_ATTEMPT_TIMEOUT` bound
    ///    (STORE-2) and panic containment, so a hung or panicking adapter
    ///    yields an error instead of wedging or unwinding out of `close`.
    ///    Its result is this method's result; on failure the batch is returned
    ///    to the log (see *Retry after failure*).
    ///
    /// A retained batch is therefore flushed or surfaced — never silently lost.
    ///
    /// ## How long it can take
    ///
    /// Bounded by the flush loop's current cycle (worst case
    /// `FLUSH_ATTEMPT_TIMEOUT × (retries + 1)`), plus the slowest write in
    /// flight at step 0 — the gate waits for it rather than losing it — plus
    /// one [`crate::writeq::WRITE_QUEUE_DRAIN_BUDGET`] for the write-queue
    /// quiesce inside step 0, plus one `FLUSH_ATTEMPT_TIMEOUT` for step 4. That
    /// quiesce budget is *carved out of*
    /// the window `serve` gives `close` rather than added to it (see that
    /// constant), so this does not move the number an operator has sized a
    /// supervisor timeout against.
    ///
    /// Step 0 is itself bounded now (R2-5): every store call a gated write can
    /// be parked in has a timeout — `RETRACT_IO_TIMEOUT` for `retract`'s
    /// durable radius, [`hybrid::HYBRID_IO_TIMEOUT`](crate::graph::hybrid::HYBRID_IO_TIMEOUT) over hybrid `derive`'s
    /// whole embed/query phase. A caller-supplied **embedder** is the one
    /// remaining way to stretch it: `Embedder` carries no bound of its own, so
    /// an adapter that never returns still parks a permit indefinitely. An
    /// owner that needs a hard wall-clock cap on `close()` should wrap it in a
    /// `timeout` — which is safe: a dropped `close()` leaves the tail on the
    /// log for the retry (see *Cancellation*).
    ///
    /// ## When it does not flush
    ///
    /// A session that degraded to `durability="none"` (spec §2.3) stopped all
    /// store I/O by design. `close` does not quietly resurrect it: it skips the
    /// final flush and returns an error saying the tail was not written, rather
    /// than reporting a durability it did not deliver. The tail stays in the
    /// log (so `stats().log_depth` keeps telling the truth) and every later
    /// `close()` returns the same error — a degraded session has no path back
    /// to a durable tail, and saying `Ok` would be a lie.
    ///
    /// **A degraded session errors even when its log is empty** (R2-3). While
    /// degraded the flush task keeps draining the log and dropping what it
    /// drained (STORE-3), so an empty log is that mode's steady state — the
    /// tail was dead-lettered, not written — and an `Ok(())` there would be the
    /// same lie by a quieter route. `degraded()` is therefore checked before
    /// the empty-log shortcut, not after it.
    pub async fn close(&self) -> Result<(), LamboError> {
        // T81-6: one close body at a time. A concurrent second caller parks
        // here and, when it gets in, either sees the success flag or re-runs
        // the (idempotent) shutdown — never an early `Ok` over an in-flight
        // final flush.
        let step = CloseStep::start(&self.session, 1, "serialize");
        let mut succeeded = self.close_state.lock().await;
        if *succeeded {
            step.done();
            return Ok(());
        }

        // 0 — the writers gate (T81-1). Latch first so new writes are refused,
        // then take the write side: it is granted only once every write that
        // slipped in before the latch has finished, so nothing this session
        // acknowledged can still be on its way to the log. Held for the rest of
        // `close` — the drain below must be the last word on the log.
        self.closed.store(true, Ordering::Release);
        step.done();

        // 0a — J3's background write queue, drained BEFORE the writers gate is
        // taken. The order is forced, not chosen: the gate's write side is held
        // for the rest of this method, so a worker that had to pass through the
        // gate could never finish and a close waiting for it would deadlock.
        // The workers therefore do not use the gate; latching `closed` above is
        // what stops new jobs (the enqueue path is a gated write), and this
        // quiesce is what makes "nothing new lands after the drain" true of the
        // workers. Bounded by `WRITE_QUEUE_DRAIN_BUDGET`; anything left over is
        // deferred (receipt `intent_durable`, durable intent replayed by the
        // next serve) rather than waited for.
        let step = CloseStep::start(&self.session, 2, "replay_stop");
        self.pipeline.abort_probe();
        // The intent replay is stopped — aborted AND joined — before the
        // quiesce and therefore well before the final drain: an aborted task
        // can still finish a synchronous stretch that appends to the log until
        // the join returns (R3-1). Whatever it had not yet consumed stays
        // durable for the next serve.
        self.pipeline.stop_replay().await;
        step.done();
        let step = CloseStep::start(&self.session, 3, "queue_quiesce");
        self.pipeline.quiesce().await;
        step.done();

        let step = CloseStep::start(&self.session, 4, "writers_gate");
        let _quiesced = self.writers.write().await;
        step.done();

        // Stop the lease heartbeat before anything else in the shutdown: from
        // here the lease is released explicitly on the success paths below, so
        // it must not keep being refreshed. Aborting is synchronous and the task
        // touches neither the graph nor the tail, so no custody/join is needed.
        let step = CloseStep::start(&self.session, 5, "heartbeat_abort");
        self.abort_heartbeat();

        // T86-2: a fenced handle lost its lease — another writer owns the
        // session now. The final flush this close would otherwise do is exactly
        // the split-brain write the lease exists to prevent, so refuse it: stop
        // the tasks, DROP the tail (it dies with this handle as it would on a
        // crash), and do NOT release the lease (it is not ours to release). Fail
        // closed with the honest refusal rather than a lying `Ok` over a tail we
        // may never make durable. `succeeded` stays false; a retried close hits
        // this same branch (the handles are already reaped) and errors again.
        if self.lease_lost() {
            for slot in [&self.canon_handle, &self.daemon_handle, &self.flush_handle] {
                if let Some(handle) = slot.lock().take() {
                    handle.abort();
                }
            }
            let undrained = self.graph.read().log_len();
            tracing::error!(
                session = %self.session,
                mutations = undrained,
                "close: this handle lost its single-writer lease; refusing to flush the tail \
                 ({undrained} mutations discarded) and NOT releasing the lease — another writer \
                 owns the session"
            );
            step.done();
            return Err(self.lease_lost_error());
        }
        step.done();

        // ...and the two mutation producers off, before the drain. Every
        // handle travels in a `HandleCustody` guard: cancelled on a join, this
        // future must hand the handle back to its slot rather than detach a
        // task that is still able to write (R3-1). `abort()` is synchronous, so
        // it cannot be skipped by a cancellation — but only the join proves the
        // task has actually stopped.
        //
        // Coverage note (R4-3): only the flush handle's custody is pinned by a
        // test. The canon/daemon detach window (an aborted task finishing a
        // synchronous stretch that appends to the log) is real — probed
        // directly in review — but too narrow to exercise deterministically:
        // neither loop has a long synchronous stretch to park in. Custody is
        // applied uniformly anyway because the hazard class is identical and
        // reasoning per-handle about window width is exactly the mistake R3-1
        // caught. Same class of documented blind spot as `begin_write_sync`'s
        // re-check (R2-6) and the flush select's `biased;` (T81-4).
        let step = CloseStep::start(&self.session, 6, "producer_joins");
        let mut canon = HandleCustody::take(&self.canon_handle);
        canon.abort();
        let _ = canon.join().await;
        drop(canon);

        let mut daemon = HandleCustody::take(&self.daemon_handle);
        daemon.abort();
        let _ = daemon.join().await;
        drop(daemon);
        step.done();

        // 1 — graceful stop; the loop returns custody of `pending`.
        let step = CloseStep::start(&self.session, 7, "flush_join");
        self.flush.stop();

        // 2 — join. After this the flush task cannot touch the graph. This is
        // the long await (the whole of `close`'s "worst case ≈ 2 minutes") and
        // so the one an external timeout fires in: dropping the handle here
        // used to leave a zombie flush task holding the tail in its own
        // `pending`, invisible to the retry, to `Drop`'s warning and to the log
        // (R3-1). Custody keeps it re-joinable instead.
        let mut flush = HandleCustody::take(&self.flush_handle);
        if let Some(Err(err)) = flush.join().await
            && !err.is_cancelled()
        {
            tracing::warn!(error = %err, "flush task did not stop cleanly");
        }
        drop(flush);
        step.done();

        // 3 — final drain. Short critical section, guard dies with the block.
        // Accesses noted since the daemon's last cycle (it is stopped now) are
        // applied in the same section, and every access the flush had not yet
        // taken from the graph's dirty set rides the tail after the log, so a
        // clean close loses none (issue #30). The ledger is taken before the
        // graph lock: it stays a leaf. `close` (not `take`) shuts it in the
        // same critical section, so a recall still in flight that finishes
        // after this point is dropped explicitly instead of noting into a
        // ledger nothing will apply again.
        let step = CloseStep::start(&self.session, 8, "final_drain");
        let accesses = self.accesses.close();
        let batch = {
            let mut g = self.graph.write();
            g.record_accesses(&accesses);
            let mut batch = g.drain_log();
            batch.mutations.extend(g.drain_accesses(usize::MAX));
            batch
        };
        // Custody of the drained tail passes to `TailCustody` immediately:
        // from here until it is durable those mutations exist nowhere else,
        // and `close()` is a future its caller may drop (R2-1).
        let mut tail = TailCustody::new(&self.graph, batch);

        // A degraded session errors **before** the empty-log shortcut (R2-3).
        // While degraded the flush task keeps draining the log and DROPPING
        // each batch (STORE-3), so an empty log is the *normal* degraded
        // state, not evidence that anything was written. Checked second, the
        // shortcut turned exactly that state into `Ok(())` — a durability
        // claim over a tail the session had already dead-lettered.
        if self.flush.degraded() {
            let count = tail.len();
            tracing::error!(
                mutations = count,
                session = %self.session,
                "close: session is degraded (durability=\"none\"); the tail was NOT written \
                 ({count} mutations still in the log)",
            );
            // `tail`'s `Drop` puts the batch back on the log: the mutations
            // are no more durable for having been drained, and leaving them
            // there keeps `stats().log_depth` honest about what was lost
            // (T81-5). `succeeded` stays false, so no later `close()` can
            // report `Ok` over this tail.
            let detail = if count == 0 {
                "the log is empty because degraded mode drops what it drains, not because the \
                 tail was written"
                    .to_string()
            } else {
                format!("{count} tail mutations were not flushed")
            };
            step.done();
            return Err(LamboError::Store(StoreError::Backend(format!(
                "close: session {} degraded to durability=\"none\"; {detail}",
                self.session
            ))));
        }

        step.done();

        if tail.is_empty() {
            tracing::info!(
                session = %self.session,
                step = 9,
                step_name = "final_flush",
                "close: step 9/{CLOSE_STEPS} final_flush skipped (empty tail)"
            );
            // Graceful close: hand off the lease now rather than waiting out the
            // TTL, so the next writer takes the session immediately (T8.6).
            let step = CloseStep::start(&self.session, 10, "lease_release");
            self.release_lease_once().await;
            self.latch_success(&mut succeeded);
            step.done();
            return Ok(());
        }

        // 4 — the final flush, no lock held, armored (T81-2). The result is
        // bound out of the `match` scrutinee so the borrow of `tail` ends
        // here rather than spanning the arms.
        let count = tail.len();
        let step = CloseStep::start(&self.session, 9, "final_flush");
        let flushed = final_flush(self.store.as_ref(), tail.batch(), Some(self.lease_token)).await;
        step.done();
        match flushed {
            Ok(()) => {
                // Custody ends: the tail is durable, so it must NOT go back
                // on the log. Nothing awaits between here and the return, so
                // no cancellation can land in this window.
                tail.durable();
                tracing::info!(
                    mutations = count,
                    session = %self.session,
                    "Memory session closed (tail flushed)"
                );
                // Tail is durable: release the lease so the handoff is clean
                // (T8.6). A failed flush (the `Err` arm below) deliberately does
                // NOT release — it keeps the lease for a retry and lets it lapse
                // at TTL if none comes.
                let step = CloseStep::start(&self.session, 10, "lease_release");
                self.release_lease_once().await;
                self.latch_success(&mut succeeded);
                step.done();
                Ok(())
            }
            Err(err) => {
                // T81-5: the batch is NOT lost with the error. `tail`'s `Drop`
                // puts it back at the FRONT of the log — `push_front_log`'s
                // documented purpose — so a retried `close()` (or an owner
                // that fixes the store first) drains and flushes exactly this
                // tail, in order.
                drop(tail);
                tracing::error!(
                    error = %err,
                    mutations = count,
                    session = %self.session,
                    "close: final flush failed; {count} tail mutations returned to the graph log \
                     — retry close() once the store is healthy",
                );
                Err(LamboError::Store(err))
            }
        }
    }

    /// The one place `close()` latches success and gives up its registry slot.
    ///
    /// Both success paths — the empty-log shortcut and a completed step-4 flush
    /// — go through here so the R3-1 invariant is asserted once for both: **no
    /// `close()` may report success while a flush `JoinHandle` is still parked
    /// in its slot un-joined.** A parked handle means a live flush task, and a
    /// live flush task may hold the tail in its own `pending` buffer, where an
    /// empty log looks exactly like a written one.
    ///
    /// `HandleCustody` is what *guarantees* it, and the guarantee is a
    /// two-line argument: the slot is emptied only into a custody guard, and
    /// that guard hands the handle back unless the join returned. So `None` at
    /// step 3 means "reaped", never "detached" — the state that made the
    /// shortcut a lie. The assertion is the pin on that reasoning rather than a
    /// second mechanism, hence `debug_assert!` — and it is a *pin only* (R4-2):
    /// a neutered guard leaves the slot `None`, the very state this asserts,
    /// so the assertion cannot fire on the regression that matters. Detachment
    /// is undetectable from here by construction; the enforcement is
    /// `HandleCustody` and the R3-1 regression test's durability assertion,
    /// not this line.
    pub(super) fn latch_success(&self, succeeded: &mut bool) {
        debug_assert!(
            self.flush_handle.lock().is_none(),
            "close() latched success with an un-joined flush task still in its slot: the tail may \
             be sitting in that task's pending buffer (R3-1)"
        );
        *succeeded = true;
        self.unregister_once();
    }
}
