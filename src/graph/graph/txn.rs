//! In-place transactions ([`Graph::transaction`], #102) and the recent
//! node-write window ([`Graph::nodes_written_since`], #116).
//!
//! # Transactions
//!
//! Hybrid derive used to stage its commit on a clone of the whole graph and
//! swap it in only once every write had succeeded. That made a failed commit
//! leave the live graph untouched, but it copied every concept vector under
//! the write lock on every derive: 10.7 ms at 10k concepts and 168 ms at 100k
//! (#102).
//!
//! A transaction gets the same atomicity by undoing instead of copying. While
//! one is open, the write path records, the first time it touches each thing,
//! the value it had before: a node, an edge (by id), an unflushed-set entry,
//! an access-dirty flag, the reservations. The scalars it can move (epoch, log
//! length, temporal-chain length, embedding contract, unflushed contract) are
//! saved when it opens. On `Err` or a panic everything is put back, so the
//! graph, its mutation log and its epoch are exactly what they were; on `Ok`
//! the record is dropped. The cost is proportional to what the transaction
//! touched, never to the size of the graph.
//!
//! Covered: the structural write path (`insert_interaction`, `insert_concept`,
//! `upsert_edge`, `remove_edge`, `remove_node`) and `stamp_embedding`, which
//! is everything a derive commit writes. The other mutators (canonization
//! transitions, counters, vector rewrites, root goal, synonyms,
//! reservations) are not transactional; a debug build asserts that none of
//! them logs a mutation inside a transaction.
//!
//! # The node-write window
//!
//! Every node upsert or delete that reaches the log through
//! `append_mutation` is also recorded here with its epoch, in a bounded ring
//! of the most recent [`NODE_WRITE_WINDOW`] writes. A hybrid derive asks it
//! at commit which concepts changed since it planned, so it can check its
//! plan against just those instead of giving up on any change anywhere
//! (#116). The ring cannot answer for an epoch older than what it has
//! evicted; the caller then treats the plan as stale and plans again.
//! RAM-only; a loaded graph starts with an empty ring that answers from its
//! load epoch on.

use std::collections::{HashMap, VecDeque};
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};

use super::Graph;
use crate::types::{Edge, EmbeddingContract, LamboError, Mutation, Node, NodeId, Reservation};

/// How many recent node writes [`Graph::nodes_written_since`] can answer
/// for. A derive's plan-to-commit window spans its embeds; 8,192 node writes
/// is thousands of concurrent derives inside that window. Beyond it a plan
/// is replanned, never trusted.
pub(crate) const NODE_WRITE_WINDOW: usize = 8192;

/// The recent node-write ring (see the module docs).
#[derive(Clone, Debug, Default)]
pub(super) struct NodeWrites {
    /// `(epoch, id)` in epoch order, newest last.
    entries: VecDeque<(u64, NodeId)>,
    /// Every node write at an epoch above this is in `entries`.
    floor: u64,
}

impl NodeWrites {
    fn record(&mut self, epoch: u64, id: NodeId) {
        self.entries.push_back((epoch, id));
        if self.entries.len() > NODE_WRITE_WINDOW
            && let Some((evicted, _)) = self.entries.pop_front()
        {
            self.floor = self.floor.max(evicted);
        }
    }

    fn since(&self, epoch: u64) -> Option<Vec<NodeId>> {
        if epoch < self.floor {
            return None;
        }
        Some(
            self.entries
                .iter()
                .rev()
                .take_while(|(at, _)| *at > epoch)
                .map(|(_, id)| *id)
                .collect(),
        )
    }

    fn truncate_after(&mut self, epoch: u64) {
        while self.entries.back().is_some_and(|(at, _)| *at > epoch) {
            self.entries.pop_back();
        }
    }

    /// Start empty, answering from `epoch` on (a freshly loaded graph).
    pub(super) fn reset(&mut self, epoch: u64) {
        self.entries.clear();
        self.floor = epoch;
    }
}

/// What an open transaction must put back on rollback (see the module docs).
#[derive(Clone, Debug)]
pub(super) struct UndoLog {
    epoch: u64,
    log_len: usize,
    chain_len: usize,
    embedding: Option<EmbeddingContract>,
    unflushed_contract: Option<u64>,
    /// Prior value of each touched node (`None`: it did not exist).
    nodes: HashMap<NodeId, Option<Node>>,
    /// Prior value of each touched edge, by edge id.
    edges: HashMap<NodeId, Option<Edge>>,
    unflushed: HashMap<NodeId, Option<u64>>,
    access_dirty: HashMap<NodeId, bool>,
    reservations: Option<Vec<Reservation>>,
}

impl Graph {
    /// Run `f` against this graph as one atomic change: if it returns `Err`
    /// or panics, every change it made through the covered write path (see
    /// the module docs) is undone, the mutation log and epoch included, and
    /// the error or panic is passed on. Cost is proportional to what `f`
    /// touches, not to the graph.
    ///
    /// Transactions do not nest.
    pub fn transaction<T>(
        &mut self,
        f: impl FnOnce(&mut Graph) -> Result<T, LamboError>,
    ) -> Result<T, LamboError> {
        assert!(self.undo.is_none(), "Graph::transaction does not nest");
        self.undo = Some(Box::new(UndoLog {
            epoch: self.epoch,
            log_len: self.mutation_log.len(),
            chain_len: self.temporal_chain.len(),
            embedding: self.embedding.clone(),
            unflushed_contract: self.unflushed_contract,
            nodes: HashMap::new(),
            edges: HashMap::new(),
            unflushed: HashMap::new(),
            access_dirty: HashMap::new(),
            reservations: None,
        }));
        let result = catch_unwind(AssertUnwindSafe(|| f(self)));
        let undo = self
            .undo
            .take()
            .expect("the transaction's undo log is open until here");
        match result {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(err)) => {
                self.rollback(*undo);
                Err(err)
            }
            Err(panic) => {
                self.rollback(*undo);
                resume_unwind(panic)
            }
        }
    }

    /// The nodes upserted or deleted since `epoch`, newest first, possibly
    /// repeating an id; `None` when the window no longer reaches back that
    /// far (see the module docs).
    pub(crate) fn nodes_written_since(&self, epoch: u64) -> Option<Vec<NodeId>> {
        self.node_writes.since(epoch)
    }

    /// Feed the node-write ring; called by `append_mutation` only.
    pub(super) fn note_node_write(&mut self, m: &Mutation) {
        let id = match m {
            Mutation::UpsertNode { node } => node.id(),
            Mutation::DeleteNode { id } => *id,
            _ => return,
        };
        self.node_writes.record(self.epoch, id);
    }

    /// Debug check that a mutation logged inside a transaction is one the
    /// transaction can undo; called by `append_mutation` only.
    pub(super) fn debug_check_undoable(&self, m: &Mutation) {
        let Some(undo) = self.undo.as_deref() else {
            return;
        };
        let covered = match m {
            Mutation::UpsertNode { node } => undo.nodes.contains_key(&node.id()),
            Mutation::DeleteNode { id } => undo.nodes.contains_key(id),
            Mutation::UpsertEdge { edge } => undo.edges.contains_key(&edge.id),
            Mutation::DeleteEdge { id } => undo.edges.contains_key(id),
            Mutation::SetEmbedding { .. } => true,
            _ => false,
        };
        debug_assert!(
            covered,
            "a mutation the open transaction cannot undo was logged: {m:?}"
        );
    }

    /// Record node `id`'s value before its first change in a transaction.
    pub(super) fn undo_node(&mut self, id: NodeId) {
        if let Some(undo) = self.undo.as_deref_mut() {
            undo.nodes
                .entry(id)
                .or_insert_with(|| self.nodes.get(&id).cloned());
        }
    }

    /// Record edge `id`'s value before its first change in a transaction.
    pub(super) fn undo_edge(&mut self, id: NodeId) {
        if let Some(undo) = self.undo.as_deref_mut() {
            undo.edges
                .entry(id)
                .or_insert_with(|| self.edges.get(&id).cloned());
        }
    }

    /// Record `id`'s unflushed-set entry before its first change.
    pub(super) fn undo_unflushed(&mut self, id: NodeId) {
        if let Some(undo) = self.undo.as_deref_mut() {
            undo.unflushed
                .entry(id)
                .or_insert_with(|| self.unflushed.get(&id).copied());
        }
    }

    /// Record `id`'s access-dirty flag and the reservations before a removal.
    pub(super) fn undo_removal_bookkeeping(&mut self, id: NodeId) {
        if let Some(undo) = self.undo.as_deref_mut() {
            undo.access_dirty
                .entry(id)
                .or_insert_with(|| self.access_dirty.contains(&id));
            if undo.reservations.is_none() {
                undo.reservations = Some(self.reservations.clone());
            }
        }
    }

    /// Put back everything `undo` recorded.
    fn rollback(&mut self, undo: UndoLog) {
        // Edges in two passes: take every touched edge out, then put the
        // prior ones back. One pass could drop a restored edge's natural key
        // when another touched edge id shared it (removed, then re-created
        // under a new id, inside the transaction).
        for id in undo.edges.keys() {
            if let Some(current) = self.edges.remove(id) {
                let key = (current.source, current.target, current.edge_type);
                if self.edge_keys.get(&key) == Some(id) {
                    self.edge_keys.remove(&key);
                }
                self.remove_adjacency(current.source, current.target, current.edge_type);
            }
        }
        for (id, prior) in undo.edges {
            if let Some(edge) = prior {
                self.edge_keys
                    .insert((edge.source, edge.target, edge.edge_type), id);
                self.add_adjacency(edge.source, edge.target, edge.edge_type);
                self.edges.insert(id, edge);
            }
        }
        for (id, prior) in undo.nodes {
            if let Some(Node::Concept(current)) = self.nodes.remove(&id) {
                self.unindex_key(&current);
            }
            if let Some(node) = prior {
                if let Node::Concept(c) = &node {
                    self.index_key(c);
                }
                self.nodes.insert(id, node);
            }
        }
        for (id, prior) in undo.unflushed {
            match prior {
                Some(epoch) => self.unflushed.insert(id, epoch),
                None => self.unflushed.remove(&id),
            };
        }
        for (id, was) in undo.access_dirty {
            if was {
                self.access_dirty.insert(id);
            } else {
                self.access_dirty.remove(&id);
            }
        }
        if let Some(reservations) = undo.reservations {
            self.reservations = reservations;
        }
        self.temporal_chain.truncate(undo.chain_len);
        self.mutation_log.truncate(undo.log_len);
        self.epoch = undo.epoch;
        self.embedding = undo.embedding;
        self.unflushed_contract = undo.unflushed_contract;
        self.node_writes.truncate_after(undo.epoch);
    }
}
