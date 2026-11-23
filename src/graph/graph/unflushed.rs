//! What the durable store has not seen yet (#60): the concepts, and the
//! embedding contract, whose latest write is still in the write-behind
//! pipeline.
//!
//! The Postgres family's holder derive asks the database for the vectors the
//! flush has made durable and ranks only the rest in RAM
//! (`VectorCandidates::StoreAndUnflushed`). That union is exact only if every
//! concept is, at every instant, either readable in the database or in this
//! set. So:
//!
//! * **Entry.** Every mutation reaches the log through `append_mutation`,
//!   which records the epoch of each concept upsert or node delete here (and
//!   of a `SetEmbedding`), under the same write lock. No write can bypass it.
//! * **Exit.** Only [`Graph::mark_durable_through`], called by the flush
//!   **after** `store.flush` returned `Ok` for a batch stamped `epoch`. An
//!   entry whose last write is at or below that stamp was in that batch or an
//!   earlier committed one; a later write has a larger epoch and stays. So
//!   between the commit and the clear a concept is in both places, never in
//!   neither.
//! * **Never durable.** A batch the flush drops instead of committing (a
//!   dead-lettered constraint violation, or a degraded session's drop) is
//!   handed to [`Graph::pin_unflushed`]: its ids stay here for the rest of the
//!   process, because a later, successful stamp does not make them durable.
//!   A later upsert of the same concept replaces the pin with its own epoch,
//!   and is durable once its own batch commits. A degraded session pins
//!   everything it drains, so its set only grows; past about 2,000 entries
//!   the derive source ranks the whole graph for the rest of the process,
//!   and logs that once ([`Graph::first_unflushed_overflow`]).
//!
//! Deletes are recorded too: until the delete is durable the database still
//! returns the row, and the reader has to know that the graph, not the
//! database, is the authority on it.
//!
//! RAM-only, like the access dirty set: a loaded session starts empty
//! (everything it loaded is durable), and nothing here is snapshotted.
//!
//! **Dead-lettered intents (#345).** A dead letter drops the whole batch,
//! the `PutWriteIntent`s in it included, so a write admitted in this process
//! whose put was in that batch has no intent in the store. The flush hands
//! such a batch to [`Graph::note_dead_lettered_intents`] as well, and the
//! close asks [`Graph::intent_dead_lettered`] before it answers a set-aside
//! receipt `intent_durable` (`WritePipeline::settle_deferred`). Every other
//! put this process appended has, by the time a close whose final flush
//! completed settles, been committed: the background flushes and the final
//! one carry the whole log, and a degraded or fenced session's drops make the
//! close answer `close_unflushed` anyway. So the set is exact, not a
//! conservative bound (Lean: `Drain.closeD_durable_iff`).
//!
//! It holds only the receipts whose write has not settled. A consumed write
//! is not necessarily settled: a lane worker can panic after its commit and
//! consume and before `settle_one`, and the close sets its write aside (#345
//! re-review; pruning at the consume vouched for it,
//! `Drain.keptAtConsume_vouches_after_panic`). So:
//!
//! * the worker records a write it consumes as consumed and not yet settled
//!   ([`Graph::mark_consumed_unsettled`], in the consume's lock hold);
//! * its settle forgets the receipt from both sets ([`Graph::settled_write`]);
//! * the note skips a put only when its write is consumed (the consume is in
//!   the dropped batch or still in the log) and no longer in that record,
//!   that is, settled.
//!
//! A dropped put whose write is not settled therefore stays in the set,
//! panics included (Lean: `Drain.settlePrune_never_vouches_dropped`,
//! `Drain.settlePrune_exact`). What stays is one receipt per dead-lettered
//! write held, queued or running, or whose worker panicked, or refused after
//! a lost lease (whose close answers `close_unflushed` anyway); the record
//! holds one per worker between its commit and its settle, plus one per
//! panicked worker (`Drain.settlePrune_unsettled_bounded`).

use std::sync::atomic::{AtomicBool, Ordering};

use super::Graph;
use crate::types::{Mutation, Node, NodeId};

/// A flag a reader under the read lock can set once. A clone of the graph
/// carries its value.
#[derive(Debug, Default)]
pub(super) struct LoggedOnce(AtomicBool);

impl Clone for LoggedOnce {
    fn clone(&self) -> Self {
        Self(AtomicBool::new(self.0.load(Ordering::Relaxed)))
    }
}

/// The epoch a pinned entry carries: above every stamp, so no flush clears it.
const PINNED: u64 = u64::MAX;

impl Graph {
    /// Record `m` (appended at `self.epoch`) as not yet durable. Called by
    /// `append_mutation` only.
    pub(super) fn note_unflushed(&mut self, m: &Mutation) {
        let epoch = self.epoch;
        match m {
            Mutation::UpsertNode {
                node: Node::Concept(c),
            } => {
                self.undo_unflushed(c.id);
                self.unflushed.insert(c.id, epoch);
            }
            Mutation::DeleteNode { id } => {
                self.undo_unflushed(*id);
                self.unflushed.insert(*id, epoch);
            }
            Mutation::SetEmbedding { .. } => self.unflushed_contract = Some(epoch),
            _ => {}
        }
    }

    /// The flush committed every mutation stamped at or below `epoch`
    /// ([`crate::types::MutationBatch::mutation_epoch`]): forget the entries
    /// that batch made durable. Call it only after the store acknowledged the
    /// commit; calling it early opens the window this set exists to close.
    pub(crate) fn mark_durable_through(&mut self, epoch: u64) {
        self.unflushed.retain(|_, last| *last > epoch);
        if self.unflushed_contract.is_some_and(|last| last <= epoch) {
            self.unflushed_contract = None;
        }
    }

    /// The flush dropped `mutations` without committing them: keep what they
    /// touched marked as not durable for the rest of the process.
    pub(crate) fn pin_unflushed(&mut self, mutations: &[Mutation]) {
        for m in mutations {
            match m {
                Mutation::UpsertNode {
                    node: Node::Concept(c),
                } => {
                    self.unflushed.insert(c.id, PINNED);
                }
                Mutation::DeleteNode { id } => {
                    self.unflushed.insert(*id, PINNED);
                }
                Mutation::SetEmbedding { .. } => self.unflushed_contract = Some(PINNED),
                _ => {}
            }
        }
    }

    /// The flush dead-lettered `mutations`: remember the receipt ids of the
    /// write intents they put, which therefore never reached the store
    /// (#345). Call it with the batch the dead letter drops, before or with
    /// [`Self::pin_unflushed`].
    ///
    /// A put whose write was already consumed **and settled** is not noted:
    /// its consume is in `mutations` or still in the log (a put precedes its
    /// consume in log order, and the dropped batch is the flush's only pending
    /// one), and its receipt is no longer consumed-and-unsettled. A consumed
    /// write that has not settled (a worker that panicked after its commit)
    /// is noted: the close sets it aside.
    pub(crate) fn note_dead_lettered_intents(&mut self, mutations: &[Mutation]) {
        let consumed: std::collections::HashSet<&str> = mutations
            .iter()
            .chain(self.mutation_log.iter())
            .filter_map(|m| match m {
                Mutation::ConsumeWriteIntent { receipt, .. } => Some(receipt.as_str()),
                _ => None,
            })
            .collect();
        let puts: Vec<String> = mutations
            .iter()
            .filter_map(|m| match m {
                Mutation::PutWriteIntent { intent }
                    if !consumed.contains(intent.receipt.as_str())
                        || self.consumed_unsettled.contains(&intent.receipt) =>
                {
                    Some(intent.receipt.clone())
                }
                _ => None,
            })
            .collect();
        self.dead_lettered_intents.extend(puts);
    }

    /// A lane worker consumed the intent `receipt` and has not settled its
    /// write yet (#345 re-review). Call it in the consume's lock hold.
    pub(crate) fn mark_consumed_unsettled(&mut self, receipt: &str) {
        self.consumed_unsettled.insert(receipt.to_owned());
    }

    /// The lane worker settled the write `receipt`: a close will not set it
    /// aside, so it leaves the consumed-and-unsettled record and the
    /// dead-lettered set (#345).
    pub(crate) fn settled_write(&mut self, receipt: &str) {
        self.consumed_unsettled.remove(receipt);
        self.dead_lettered_intents.remove(receipt);
    }

    /// Whether the put of the write intent `receipt` was in a batch the
    /// flush dead-lettered (#345): its intent is not in the store, so a close
    /// must not answer it `intent_durable`.
    pub(crate) fn intent_dead_lettered(&self, receipt: &str) -> bool {
        self.dead_lettered_intents.contains(receipt)
    }

    /// Whether the latest write to `id` (an upsert or a delete) is not yet
    /// durable. Tests only: the reader iterates [`Self::unflushed_ids`].
    #[cfg(test)]
    pub(crate) fn is_unflushed(&self, id: &NodeId) -> bool {
        self.unflushed.contains_key(id)
    }

    /// The ids whose latest write is not yet durable: live concepts, and
    /// deleted ones whose delete is still pending.
    pub(crate) fn unflushed_ids(&self) -> impl Iterator<Item = &NodeId> {
        self.unflushed.keys()
    }

    /// How many ids [`Self::unflushed_ids`] yields.
    pub(crate) fn unflushed_len(&self) -> usize {
        self.unflushed.len()
    }

    /// Whether the session's embedding contract has a change the store has
    /// not seen (a fresh session before its first flush, a re-embed): the
    /// store's checked read still compares against the old one.
    pub(crate) fn contract_unflushed(&self) -> bool {
        self.unflushed_contract.is_some()
    }

    /// `true` the first time it is called on this graph, `false` after: the
    /// derive source logs its first whole-graph fallback for an overflowing
    /// set once, not on every derive. Takes `&self` so the reader can call
    /// it under the read lock.
    pub(crate) fn first_unflushed_overflow(&self) -> bool {
        !self
            .unflushed_overflow_logged
            .0
            .swap(true, Ordering::Relaxed)
    }
}
