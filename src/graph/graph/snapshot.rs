//! Materialization: [`Graph::from_snapshot`] (the load path) and
//! [`Graph::snapshot`] (the full, deterministically ordered view a store
//! round-trip writes).
//!
//! Loading seeds state without touching the mutation log (a loaded session's
//! history is already durable), resumes the deployment-lifetime epoch and GC
//! mark (issues #17, #29), and runs [`Graph::assert_invariants`] before
//! returning.

use std::collections::{HashMap, HashSet};

use super::{invariant, normalize_weight, EdgeKey, Graph};
use crate::types::{
    Concept, Edge, GcMark, GraphSnapshot, Interaction, LamboError, Node, NodeId, Synonym,
};

impl Graph {
    /// Materialize a session snapshot into RAM. Seeds state without emitting
    /// mutations (the history is already durable) and verifies every §5.7
    /// invariant before returning.
    ///
    /// A zero-interaction snapshot is a valid **empty** graph (adve-review
    /// GRAPH-6 — "expected exactly one chain head, found 0" was rejecting
    /// fresh sessions), and duplicate natural-key edges are rejected rather than
    /// silently merged via reinforcement (GRAPH-7 — the loaded graph must equal
    /// the stored snapshot; reinforcement is a write-path semantic, not a load
    /// one).
    ///
    /// The mutation epoch **resumes** from [`GraphSnapshot::mutation_epoch`]
    /// (issue #17): the counter is durable accounting, not process state.
    /// Restarting it at 0 on every writer start is what kept GC's
    /// `gc_interval` sweep — and through `gc_survived` every Swarm Stage 1
    /// promotion — permanently out of reach in a low-write deployment. The
    /// log stays empty either way: loading counts nothing new.
    pub fn from_snapshot(snap: GraphSnapshot) -> Result<Self, LamboError> {
        let sid = snap.session_id.clone();
        let mut g = Self::new(sid.clone());

        g.root_goal = snap.root_goal;
        g.created_at = snap.created_at;
        g.closed_at = snap.closed_at;
        g.embedding = snap.embedding;

        for i in &snap.interactions {
            if i.session_id != sid {
                return Err(invariant(format!(
                    "interaction {} session {} != snapshot {}",
                    i.id, i.session_id, sid
                )));
            }
            g.nodes.insert(i.id, Node::Interaction(i.clone()));
        }
        for c in &snap.concepts {
            if c.session_id != sid {
                return Err(invariant(format!(
                    "concept {} session {} != snapshot {}",
                    c.id, c.session_id, sid
                )));
            }
            g.nodes.insert(c.id, Node::Concept(c.clone()));
        }

        // Rebuild the temporal chain by walking `previous_id` links.
        let mut next_of: HashMap<NodeId, NodeId> = HashMap::new();
        let mut heads: Vec<NodeId> = Vec::new();
        for i in &snap.interactions {
            match i.previous_id {
                None => heads.push(i.id),
                Some(prev) => {
                    if !g.nodes.contains_key(&prev) {
                        return Err(invariant(format!(
                            "interaction {} previous {} missing",
                            i.id, prev
                        )));
                    }
                    if next_of.insert(prev, i.id).is_some() {
                        return Err(invariant(format!(
                            "interaction {} has two successors (fork in temporal chain)",
                            prev
                        )));
                    }
                }
            }
        }
        if snap.interactions.is_empty() {
            // GRAPH-6: a zero-interaction snapshot is a valid empty graph, not a
            // malformed chain. A non-empty snapshot with a forked/absent chain
            // still fails below; concepts without Derives edges still fail
            // assert_invariants.
            g.temporal_chain = Vec::new();
        } else {
            if heads.len() != 1 {
                return Err(invariant(format!(
                    "expected exactly one chain head, found {}",
                    heads.len()
                )));
            }
            let mut chain = Vec::with_capacity(snap.interactions.len());
            let mut visited: HashSet<NodeId> = HashSet::with_capacity(snap.interactions.len());
            let mut cur = heads[0];
            loop {
                if !visited.insert(cur) {
                    return Err(invariant("cycle in temporal chain"));
                }
                chain.push(cur);
                match next_of.get(&cur) {
                    Some(&next) => cur = next,
                    None => break,
                }
            }
            if chain.len() != snap.interactions.len() {
                return Err(invariant(format!(
                    "temporal chain covers {} of {} interactions",
                    chain.len(),
                    snap.interactions.len()
                )));
            }
            g.temporal_chain = chain;
        }

        // GRAPH-7: duplicate (source, target, edge_type) in one snapshot must be
        // rejected up front — record_edge would silently reinforce, leaving a
        // loaded graph that disagrees with the stored snapshot.
        let mut seen_edge_keys: HashSet<EdgeKey> = HashSet::with_capacity(snap.edges.len());
        for e in &snap.edges {
            let key = (e.source, e.target, e.edge_type);
            if !seen_edge_keys.insert(key) {
                return Err(invariant(format!(
                    "duplicate natural-key edge ({}, {}, {:?}) in snapshot",
                    key.0, key.1, key.2
                )));
            }
            let weight = normalize_weight(e.weight)?;
            let mut e = e.clone();
            e.weight = weight;
            g.record_edge(e)?;
        }
        for s in &snap.synonyms {
            if s.session_id != sid {
                return Err(invariant(format!(
                    "synonym session {} != snapshot {}",
                    s.session_id, sid
                )));
            }
            g.synonyms
                .insert(s.source_key.clone(), s.canonical_key.clone());
        }
        for r in &snap.reservations {
            if r.session_id != sid {
                return Err(invariant(format!(
                    "reservation session {} != snapshot {}",
                    r.session_id, sid
                )));
            }
            g.reservations.push(r.clone());
        }
        for ev in &snap.canonization_events {
            if ev.session_id != sid {
                return Err(invariant(format!(
                    "canonization event session {} != snapshot {}",
                    ev.session_id, sid
                )));
            }
            g.canonization_events.push(ev.clone());
        }

        g.assert_invariants()?;
        // Issue #17: resume the durable mutation accounting AFTER the
        // invariant pass so `epoch` is the last thing a caller could observe
        // mid-construction. `Graph::new` started it at 0; the snapshot's value
        // is the deployment's count through its last durable mutation.
        g.epoch = snap.mutation_epoch;
        // Issue #29: GC's sweep accounting resumes with it, so a restart does
        // not measure the next sweep from 0 (which swept once per restart and
        // bumped every `gc_survived` once the lifetime count passed the
        // interval).
        // The reset flag is writer-side only (stored marks are always
        // `false`): a snapshot that somehow carries it must not make this
        // fresh writer replace the stored time on its first flush.
        g.gc_mark = GcMark {
            last_gc_at_reset: false,
            ..snap.gc_mark
        };
        Ok(g)
    }

    /// Full session materialization for `load_session` / store round-trips.
    ///
    /// Deterministic ordering: interactions in temporal chain order, concepts and
    /// edges sorted by id, synonyms sorted by `source_key`.
    pub fn snapshot(&self) -> GraphSnapshot {
        let interactions: Vec<Interaction> = self
            .temporal_chain
            .iter()
            .filter_map(|id| match self.nodes.get(id) {
                Some(Node::Interaction(i)) => Some(i.clone()),
                _ => None,
            })
            .collect();
        let mut concepts: Vec<Concept> = self
            .nodes
            .values()
            .filter_map(|n| match n {
                Node::Concept(c) => Some(c.clone()),
                _ => None,
            })
            .collect();
        let mut edges: Vec<Edge> = self.edges.values().cloned().collect();
        let mut synonyms: Vec<Synonym> = self
            .synonyms
            .iter()
            .map(|(src, canon)| Synonym {
                session_id: self.session_id.clone(),
                source_key: src.clone(),
                canonical_key: canon.clone(),
            })
            .collect();
        // Interactions were collected in temporal chain order above — do NOT sort
        // them by id (adve-review T2.1 S4): the chain order is the documented
        // contract, and random v4 UUIDs would silently destroy it.
        concepts.sort_by_key(|c| c.id.0);
        edges.sort_by_key(|e| e.id.0);
        synonyms.sort_by(|a, b| a.source_key.cmp(&b.source_key));

        GraphSnapshot {
            session_id: self.session_id.clone(),
            root_goal: self.root_goal.clone(),
            created_at: self.created_at,
            closed_at: self.closed_at,
            interactions,
            concepts,
            edges,
            synonyms,
            reservations: self.reservations.clone(),
            canonization_events: self.canonization_events.clone(),
            embedding: self.embedding.clone(),
            // Write intents are not graph state: they pass through this
            // graph's mutation log (`record_write_intent` /
            // `consume_write_intent`) into the store, and only
            // `load_session` materializes them. A RAM snapshot therefore has
            // none to offer — the store is their single home.
            write_intents: Vec::new(),
            // The mutation accounting is graph state (issue #17): a seeded or
            // otherwise re-materialized session must resume the epoch, not
            // restart it, or GC's `gc_interval` would measure per-process
            // mutations again.
            mutation_epoch: self.epoch,
            // A snapshot is a stored view: the writer-side re-anchor flag
            // never leaves the graph this way (only `drain_log` carries it).
            gc_mark: GcMark {
                last_gc_at_reset: false,
                ..self.gc_mark
            },
        }
    }
}
