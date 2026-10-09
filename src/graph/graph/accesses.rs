//! Read-access bookkeeping (issue #30): applying coalesced accesses to
//! concepts in RAM and handing the access-dirty set to the flush.
//!
//! Accesses never enter the mutation log and never bump the epoch: an access
//! is bookkeeping about a read, not a change to what the graph says.
//! [`Graph::record_accesses`] gives the full reasoning.

use super::Graph;
use crate::types::{Mutation, Node, NodeId};

impl Graph {
    /// Apply coalesced read accesses (issue #30): for each `(id, count, at)`,
    /// `access_count += count` (saturating, `i32` is the column type) and
    /// `last_accessed = max(last_accessed, at)`. Returns how many concepts were
    /// updated.
    ///
    /// The update is in RAM at once (GC and scoring see it); durability is
    /// **deferred**: the concept is marked access-dirty, and the write-behind
    /// flush turns the dirty set into one [`Mutation::RecordAccess`] per
    /// concept, with its absolute values at that moment, via
    /// [`Self::drain_accesses`] — a narrow, monotonic update of the two access
    /// columns (no full-row rewrite, no embedding, no vector-index touch).
    /// Nothing is appended to the mutation log here, so reads can never grow
    /// the log: during a store outage the flush stops draining accesses while
    /// it holds unflushed ones, and the dirty set stays bounded by the concept
    /// count. **The epoch is not bumped** either.
    /// An access is bookkeeping about a read, not a change to what the graph
    /// says: no node, edge, key or status that recall, canonicalization or a
    /// hybrid plan reads changes. Bumping would (a) count reads toward GC's
    /// `gc_interval` trigger, so a read-heavy session would sweep on reads
    /// alone, (b) invalidate every recall-cache entry on every recall, turning
    /// the cache off for the read-mostly case it exists for, and (c) force a
    /// concurrent hybrid commit to replan against a graph nothing changed. The
    /// same reasoning already keeps [`Self::record_write_intent`] off the epoch.
    ///
    /// The durable mutation watermark is unaffected: accesses are never
    /// counted, and the adapters persist `MAX` of whatever stamp a flush
    /// carries.
    ///
    /// Ids that are missing, or that are not concepts (an interaction hit), are
    /// skipped: a concept collected or retracted between the read and this
    /// apply has nothing left to count against. `count == 0` is a no-op for
    /// that id.
    pub fn record_accesses(
        &mut self,
        accesses: &[(NodeId, u32, chrono::DateTime<chrono::Utc>)],
    ) -> usize {
        let mut ordered: Vec<&(NodeId, u32, chrono::DateTime<chrono::Utc>)> =
            accesses.iter().filter(|(_, n, _)| *n > 0).collect();
        ordered.sort_by_key(|(id, _, _)| id.0);
        let mut updated = 0;
        for &(id, count, at) in ordered {
            let Some(Node::Concept(c)) = self.nodes.get_mut(&id) else {
                continue;
            };
            let delta = i32::try_from(count).unwrap_or(i32::MAX);
            c.access_count = c.access_count.saturating_add(delta);
            c.last_accessed = Some(c.last_accessed.map_or(at, |prev| prev.max(at)));
            // Deliberately NOT the log (nor `append_mutation`): see the doc
            // comment. The flush asks for it through `drain_accesses`.
            self.access_dirty.insert(id);
            updated += 1;
        }
        updated
    }

    /// Hand at most `limit` access-dirty concepts to the flush as
    /// [`Mutation::RecordAccess`] (issue #30), each with its **current**
    /// absolute values, in id order (deterministic), and clear them from the
    /// dirty set. Ids that are no longer concepts are dropped from the set
    /// without a mutation. The rest stay dirty for a later call.
    ///
    /// Not part of [`Self::drain_log`] on purpose: the log is replayed in
    /// order and grows with every drain the flush retains, while access
    /// bookkeeping is a per-concept *state* that only needs its latest value
    /// to be durable. The flush loop decides when to take it (never while it
    /// already holds undelivered accesses — see `store::flush`), and
    /// `Memory::close` takes all of it after its final `drain_log`. Values only
    /// ever rise, so a `RecordAccess` taken after a concept upsert in the log
    /// carries values at least as high — the ordering `store::batch` relies on.
    pub fn drain_accesses(&mut self, limit: usize) -> Vec<Mutation> {
        if limit == 0 || self.access_dirty.is_empty() {
            return Vec::new();
        }
        let mut ids: Vec<NodeId> = self.access_dirty.iter().copied().collect();
        ids.sort_unstable_by_key(|id| id.0);
        let mut out = Vec::with_capacity(limit.min(ids.len()));
        for id in ids {
            if out.len() == limit {
                break;
            }
            self.access_dirty.remove(&id);
            if let Some(Node::Concept(c)) = self.nodes.get(&id)
                && let Some(last_accessed) = c.last_accessed
            {
                out.push(Mutation::RecordAccess {
                    session_id: c.session_id.clone(),
                    id,
                    access_count: c.access_count,
                    last_accessed,
                });
            }
        }
        out
    }

    /// Concepts whose applied accesses have not yet been handed to the flush
    /// ([`Self::drain_accesses`]). At most the session's concept count.
    pub fn pending_accesses(&self) -> usize {
        self.access_dirty.len()
    }
}
