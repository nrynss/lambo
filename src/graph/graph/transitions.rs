//! Concept status and counters: the spec §10 canonization write gate
//! ([`Graph::apply_canonization_transition`] and its audit trail), GC
//! survivor bookkeeping and human confirmation.
//!
//! Each change is applied to the concept in RAM and appended to the mutation
//! log through `append_mutation`, under the caller's write lock, like every
//! other graph mutation.

use super::{invariant, not_found, Graph};
use crate::types::{
    CanonizationEvent, CanonizationStatus, Concept, LamboError, Mutation, Node, NodeId, StoreError,
};

impl Graph {
    /// Apply a canonization transition to a concept and record it. Concept must
    /// exist; status and blast radius are set from the event. The event is
    /// appended to the session's audit trail and emitted as a mutation.
    ///
    /// Write-gate validation (adve-review GRAPH-4): `from_status` must equal the
    /// concept's **current** status — a fabricated audit row is rejected with a
    /// typed invariant error — and the pair must be an edge of the spec §10
    /// state machine (`legal_canonization_transition`; stage skips, downgrades
    /// and self-loops are rejected). A demotion event additionally carries the
    /// concept's new `last_demotion_time` (COH-3, spec §10 "Demotion sets
    /// `last_demotion_time`"); non-demotion events leave that field untouched.
    pub fn apply_canonization_transition(
        &mut self,
        event: CanonizationEvent,
    ) -> Result<(), LamboError> {
        if event.session_id != self.session_id {
            return Err(invariant(format!(
                "canonization event session {} != graph {}",
                event.session_id, self.session_id
            )));
        }
        let concept = match self.nodes.get_mut(&event.node_id) {
            Some(Node::Concept(c)) => c,
            _ => {
                return Err(not_found(format!(
                    "concept {} for canonization",
                    event.node_id
                )))
            }
        };
        if concept.canonization_status != event.from_status {
            return Err(invariant(format!(
                "canonization transition for {} claims {:?} -> {:?} but the concept's \
                 current status is {:?} (fabricated transition rejected)",
                event.node_id, event.from_status, event.to_status, concept.canonization_status
            )));
        }
        if !legal_canonization_transition(event.from_status, event.to_status) {
            return Err(invariant(format!(
                "illegal canonization transition {:?} -> {:?} for concept {} \
                 (spec §10 state machine)",
                event.from_status, event.to_status, event.node_id
            )));
        }
        concept.canonization_status = event.to_status;
        concept.blast_radius = event.blast_radius;
        // COH-3: a demotion event always carries Some (the concept's new
        // last_demotion_time); a non-demotion event's None must not clobber a
        // previously demoted concept's value.
        if let Some(t) = event.last_demotion_time {
            concept.last_demotion_time = Some(t);
        }
        self.canonization_events.push(event.clone());
        self.append_mutation(Mutation::CanonizationTransition { event });
        Ok(())
    }

    /// GC survivor bookkeeping (T4.5, spec §9 step 5): increment `gc_survived`
    /// on every surviving concept — canonization Stage 1's input.
    ///
    /// Missing ids are skipped; the count is **saturating** (`i32` is the
    /// schema column type — 2^31 GC cycles would otherwise overflow). Each
    /// bump is emitted as an `UpsertNode` mutation so the durable store
    /// mirrors the counter (spec §2.4 log contract; the store's upsert
    /// replaces the row in place).
    pub fn bump_gc_survived(&mut self, concept_ids: &[NodeId]) -> usize {
        let mut bumped = 0;
        let mut updates: Vec<Concept> = Vec::new();
        for &id in concept_ids {
            let Some(Node::Concept(c)) = self.nodes.get_mut(&id) else {
                continue;
            };
            c.gc_survived = c.gc_survived.saturating_add(1);
            bumped += 1;
            updates.push(c.clone());
        }
        for c in updates {
            self.append_mutation(Mutation::UpsertNode {
                node: Node::Concept(c),
            });
        }
        bumped
    }

    /// Record one explicit human confirmation of a concept (C2, spec §3.2's
    /// "Human Confirmed" term).
    ///
    /// The counter is **saturating** (`i32` is the schema column type) and the
    /// bump is emitted as an `UpsertNode` mutation so the durable store mirrors
    /// it, exactly like [`Self::bump_gc_survived`]. Returns the new count; a
    /// missing id or a non-concept node is [`StoreError::NotFound`] — there is
    /// no silent skip here, because a confirmation the graph cannot apply must
    /// fail loudly rather than be lost.
    ///
    /// No agent write path reaches this method: it is called only through
    /// [`crate::Memory::confirm_human`], the human-in-the-loop surface the
    /// solo score's second term reads.
    pub fn confirm_human(&mut self, node: NodeId) -> Result<i32, LamboError> {
        let Some(Node::Concept(c)) = self.nodes.get_mut(&node) else {
            return Err(LamboError::Store(StoreError::NotFound(format!(
                "confirm_human: no concept {node}"
            ))));
        };
        c.human_confirmed = c.human_confirmed.saturating_add(1);
        let confirmed = c.human_confirmed;
        let updated = c.clone();
        self.append_mutation(Mutation::UpsertNode {
            node: Node::Concept(updated),
        });
        Ok(confirmed)
    }

    pub fn canonization_events(&self) -> &[CanonizationEvent] {
        &self.canonization_events
    }
}

/// Legal spec §10 state-machine edges (adve-review GRAPH-4): Stage 1 promotes
/// `None -> Candidate`, Stage 2 promotes to `Venerable` (from `None` or
/// `Candidate` — the two stages evaluate independent evidence), Stage 3 promotes
/// `Venerable -> Canonical`, and demotion returns `Canonical -> None` (spec §10:
/// demotion nulls `blast_radius` and sets `last_demotion_time`). Everything else
/// — stage skips, downgrades, and self-loops — is rejected at the
/// [`Graph::apply_canonization_transition`] write gate.
fn legal_canonization_transition(from: CanonizationStatus, to: CanonizationStatus) -> bool {
    matches!(
        (from, to),
        (CanonizationStatus::None, CanonizationStatus::Candidate)
            | (CanonizationStatus::None, CanonizationStatus::Venerable)
            | (CanonizationStatus::Candidate, CanonizationStatus::Venerable)
            | (CanonizationStatus::Venerable, CanonizationStatus::Canonical)
            | (CanonizationStatus::Canonical, CanonizationStatus::None)
    )
}
