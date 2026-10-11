//! The canonical-key index (#102): every non-`Observation` concept, by its
//! canonical key, smallest id first.
//!
//! Before it, [`crate::graph::canonical::canonicalize`] and
//! [`Graph::insert_concept`]'s `UNIQUE (session_id, canonical_key)` gate each
//! scanned every node, so a hybrid commit (which canonicalizes each item again
//! under the write lock) paid `O(session size)` per item while holding the
//! lock. The index answers both in `O(1)`.
//!
//! It holds exactly the concepts those two scans considered: `Observation`s
//! are left out because canonicalization never matches one (GRAPH-1) and the
//! UNIQUE gate exempts them (spec errata 2026-08-11). The ids under one key
//! are kept sorted by their UUID, so the first is the scan's
//! `min_by_key(|c| c.id.0)` pick. A key normally holds one id; it can hold
//! more only for a loaded snapshot that already carried a duplicate, and then
//! the lowest id still wins, as before.
//!
//! Maintained wherever a concept enters or leaves `nodes`: `insert_concept`,
//! `remove_node`, `from_snapshot` and a transaction rollback. Every other
//! write to a concept (status, counters, vectors) leaves its key and type
//! alone. RAM-only and derived, so it is never snapshotted; it is checked by
//! [`Graph::assert_invariants`].

use super::Graph;
use crate::types::{Concept, ConceptType, NodeId};

impl Graph {
    /// The concept canonicalization matches for `key`: the non-`Observation`
    /// concept carrying it, the lowest id if a loaded snapshot holds two.
    pub fn concept_by_key(&self, key: &str) -> Option<NodeId> {
        self.concept_keys
            .get(key)
            .and_then(|ids| ids.first())
            .copied()
    }

    /// A non-`Observation` concept other than `id` that carries `key`.
    pub(super) fn key_holder_other_than(&self, key: &str, id: NodeId) -> Option<NodeId> {
        self.concept_keys
            .get(key)
            .and_then(|ids| ids.iter().find(|other| **other != id))
            .copied()
    }

    /// Add `c` to the index (a no-op for an `Observation`).
    pub(super) fn index_key(&mut self, c: &Concept) {
        if c.concept_type == ConceptType::Observation {
            return;
        }
        let ids = self
            .concept_keys
            .entry(c.canonical_key.clone())
            .or_default();
        if let Err(at) = ids.binary_search_by(|probe| probe.0.cmp(&c.id.0)) {
            ids.insert(at, c.id);
        }
    }

    /// Remove `c` from the index (a no-op for an `Observation`).
    pub(super) fn unindex_key(&mut self, c: &Concept) {
        if c.concept_type == ConceptType::Observation {
            return;
        }
        if let Some(ids) = self.concept_keys.get_mut(&c.canonical_key) {
            ids.retain(|id| *id != c.id);
            if ids.is_empty() {
                self.concept_keys.remove(&c.canonical_key);
            }
        }
    }

    /// The index rebuilt from `nodes`, for [`Graph::assert_invariants`].
    pub(super) fn key_index_drift(&self) -> Option<String> {
        let mut expected: std::collections::HashMap<&str, Vec<NodeId>> =
            std::collections::HashMap::new();
        for c in self.concepts() {
            if c.concept_type != ConceptType::Observation {
                expected
                    .entry(c.canonical_key.as_str())
                    .or_default()
                    .push(c.id);
            }
        }
        for ids in expected.values_mut() {
            ids.sort_by_key(|id| id.0);
        }
        let same = expected.len() == self.concept_keys.len()
            && expected
                .iter()
                .all(|(key, ids)| self.concept_keys.get(*key) == Some(ids));
        (!same).then(|| {
            format!(
                "canonical-key index holds {} keys but the concepts carry {}",
                self.concept_keys.len(),
                expected.len()
            )
        })
    }
}
