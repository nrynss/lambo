//! The write-behind mutation log and its accounting: the epoch, draining a
//! batch for the flush, re-queueing a drained batch at shutdown, durable write
//! intents (J3), and GC's sweep mark and clock (issue #29).
//!
//! Every change to graph state reaches the log through the root's
//! `append_mutation`, which also bumps the epoch, under the caller's write
//! lock. The batch [`Graph::drain_log`] returns is in chronological order and
//! is replayed in order, never re-sorted (the replay contract in
//! `src/graph/mod.rs`). Two deliberate exceptions, each explained on its
//! method: write intents are appended here directly, without an epoch bump,
//! and GC mark changes never enter the log at all.

use super::Graph;
use crate::types::{GcMark, Mutation, MutationBatch, WriteIntent, WriteIntentOutcome};

impl Graph {
    /// Number of mutations currently awaiting flush.
    pub fn log_len(&self) -> usize {
        self.mutation_log.len()
    }

    /// `MutationEpoch` — bumps once per appended mutation; unchanged by reads and
    /// by draining. Recall caches key on this (spec §8). Two kinds of logged
    /// mutation are deliberately not counted, because neither changes what the
    /// graph says: write intents ([`Graph::record_write_intent`]) and read-access
    /// bookkeeping ([`Graph::record_accesses`], issue #30).
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Drain the ordered mutation log into a batch (T3.4's flush input).
    ///
    /// The batch is in **chronological** write order. §2.4's phase grouping
    /// (nodes -> edges -> deletions -> transitions) holds within a single logical
    /// write, not across the batch. Replay in order — never re-sort.
    ///
    /// The batch is stamped with [`Graph::epoch`] at drain time
    /// ([`MutationBatch::mutation_epoch`]): an absolute watermark the store
    /// persists in the same transaction as the batch, so the counter and the
    /// content it counts land atomically — the durable watermark is never
    /// behind the count of durable mutations, though it may run ahead of it by
    /// the RAM-local epoch bumps (reservations, synonyms) the stamp carries
    /// (issue #17).
    pub fn drain_log(&mut self) -> MutationBatch {
        MutationBatch {
            mutations: std::mem::take(&mut self.mutation_log),
            mutation_epoch: self.epoch,
            gc_mark: self.gc_mark,
        }
    }

    /// GC's durable sweep accounting (issue #29); see [`GcMark`].
    pub fn gc_mark(&self) -> GcMark {
        self.gc_mark
    }

    /// Record a completed GC sweep: the next interval is measured from
    /// `epoch_after` and the `gc_max_interval` clock restarts at `at`.
    ///
    /// Not a mutation (no epoch bump, no log entry): the mark rides the next
    /// drained batch's stamp. The caller (the daemon) calls this inside the
    /// same write guard as the sweep, so the mark and the sweep's mutations
    /// drain — and so persist — together.
    pub fn record_gc_sweep(&mut self, epoch_after: u64, at: chrono::DateTime<chrono::Utc>) {
        self.gc_mark = self.gc_mark.merge(GcMark {
            last_gc_epoch: epoch_after,
            last_gc_at: Some(at),
            last_gc_at_reset: false,
        });
    }

    /// Exclude `n` epoch bumps from GC's session-mutation measure by advancing
    /// [`GcMark::last_gc_epoch`] by `n` (NEW-2), never past [`Graph::epoch`].
    ///
    /// GC's deferred survivor-bump drains use this: their own `UpsertNode`s
    /// advance the epoch, and crediting them as session writes made GC
    /// self-sustaining on an idle session. It is only for writes that **did**
    /// advance the epoch, called under the same write guard that appended
    /// them, with `n` equal to the bumps they caused.
    ///
    /// A write that does not advance the epoch needs no exemption and must not
    /// call this: the measure is `epoch - last_gc_epoch`, so such a write is
    /// already invisible to it, and "exempting" it would cancel real session
    /// writes out of `gc_interval` and the idle floor and suppress sweeps.
    /// (Issue #30 records accesses without advancing the epoch, so it is in
    /// this category.)
    ///
    /// The watermark is clamped to the current epoch: a watermark ahead of the
    /// epoch would hide the next writes from the measure until the epoch caught
    /// up. An `n` that would overshoot is a caller bug (debug-asserted).
    pub fn exempt_from_gc_measure(&mut self, n: u64) {
        let advanced = self.gc_mark.last_gc_epoch.saturating_add(n);
        debug_assert!(
            advanced <= self.epoch,
            "exempt_from_gc_measure({n}) would move last_gc_epoch {} past epoch {}: \
             only exempt bumps that were actually appended",
            self.gc_mark.last_gc_epoch,
            self.epoch
        );
        self.gc_mark.last_gc_epoch = advanced.min(self.epoch).max(self.gc_mark.last_gc_epoch);
    }

    /// Re-anchor the `gc_max_interval` clock at `at` after the stored
    /// `last_gc_at` was found in the future (issue #29: a forward wall-clock
    /// jump persisted a future sweep time, and the max-merge would otherwise
    /// keep it — disabling the time trigger until real time caught up).
    ///
    /// Not a mutation; rides the next drained batch like every mark change.
    /// Sets [`GcMark::last_gc_at_reset`] so the flush carry and the store
    /// accept this one regression of `last_gc_at` instead of max-merging it
    /// away. `last_gc_epoch` is untouched. The caller (the daemon) decides when
    /// a stored time is "in the future" — see
    /// [`crate::daemon::gc::gc_clock_ahead`].
    pub fn reanchor_gc_clock(&mut self, at: chrono::DateTime<chrono::Utc>) {
        self.gc_mark.last_gc_at = Some(at);
        self.gc_mark.last_gc_at_reset = true;
    }

    /// Start the `gc_max_interval` clock for a session that has never swept
    /// (issue #29): a no-op once [`GcMark::last_gc_at`] is set. The first timed
    /// sweep is then due a full interval after a writer first observed the
    /// session, never immediately on attach.
    pub fn anchor_gc_clock(&mut self, at: chrono::DateTime<chrono::Utc>) {
        if self.gc_mark.last_gc_at.is_none() {
            self.gc_mark.last_gc_at = Some(at);
        }
    }

    /// Re-append already-drained mutations to the **front** of the log,
    /// preserving chronological order (T8.1 shutdown drain).
    ///
    /// The write-behind flush task owns its `pending` buffer, so a batch it
    /// drained but has not yet made durable — most importantly one RETAINED
    /// after exhausted retries — is invisible to [`Graph::drain_log`]. On
    /// shutdown the task hands that buffer back here so `Memory::close`'s
    /// final `drain_log` can see it and flush it (COH-6). A hard
    /// `JoinHandle::abort()` would drop it with the task.
    ///
    /// Front, not back: everything in `mutations` was appended to the log
    /// **before** anything still in it, so prepending is what restores
    /// chronological order — the `src/graph/mod.rs` "replay in order, never
    /// re-sort" contract.
    ///
    /// The epoch is **not** bumped: these mutations were counted when they
    /// were first appended, the graph state they describe is already applied,
    /// and re-counting them would needlessly invalidate every recall cache
    /// entry at shutdown.
    ///
    /// This is the only re-entry point into the log and it is deliberately
    /// narrow: it takes mutations that this graph already produced. Feeding it
    /// anything else would put mutations in the log that the in-RAM graph does
    /// not reflect.
    pub fn push_front_log(&mut self, mutations: Vec<Mutation>) {
        if mutations.is_empty() {
            return;
        }
        self.mutation_log.splice(0..0, mutations);
    }

    /// Append a durable write intent to the log (J3 — appended at ack, so the
    /// write-behind drain and the close-time final flush carry it exactly as
    /// they carry every other mutation).
    ///
    /// The epoch is deliberately **not** bumped: an intent is not graph state —
    /// no node, edge, or recall-visible fact changes — and bumping would force
    /// a concurrent hybrid commit to replan against a graph that has not
    /// changed, and invalidate every recall cache entry for a record recall
    /// cannot see. The mutation log is the only thing touched.
    pub fn record_write_intent(&mut self, intent: WriteIntent) {
        debug_assert_eq!(intent.session_id, self.session_id);
        self.mutation_log.push(Mutation::PutWriteIntent { intent });
    }

    /// Append the consumption of a write intent (J3).
    ///
    /// **Must be called in the same write-lock critical section as the commit
    /// of the mutations the intent produced.** The flush loop's drain takes
    /// this same lock, so mutations appended under one hold always travel in
    /// one batch — and a batch is one store transaction, which is what makes
    /// "the applied write is durable" and "the intent is consumed" a single
    /// fact. Consuming under a *separate* hold opens the window this design
    /// exists to close: a flush between the two commits the applied mutations,
    /// the process dies, and the next serve replays an intent whose write is
    /// already durable — the double-apply.
    ///
    /// Epoch not bumped, for [`Graph::record_write_intent`]'s reason.
    pub fn consume_write_intent(&mut self, receipt: String, outcome: WriteIntentOutcome) {
        self.mutation_log.push(Mutation::ConsumeWriteIntent {
            session_id: self.session_id.clone(),
            receipt,
            outcome,
        });
    }
}
