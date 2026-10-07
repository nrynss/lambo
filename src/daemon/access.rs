//! Read-access accounting (issue #30) — the write half of spec §9's
//! `frequency` dimension.
//!
//! Recall and inspect are reads: they run under the graph **read** lock (or
//! none), and must not take the write lock per hit or per call. So a read only
//! *notes* what it returned here, in an [`AccessLedger`] — a leaf
//! `parking_lot::Mutex` around a per-concept delta map, held for the length of
//! a few hash inserts and never while any other lock is acquired. The
//! session's daemon cycle (one tick, default 1s) then takes the whole map and
//! applies it in ONE write-guard section through [`Graph::record_accesses`],
//! which emits one narrow `RecordAccess` per touched concept into the
//! write-behind log.
//! Recall pays a hash insert per hit; the graph pays one brief write section
//! per tick, however many recalls ran in it.
//!
//! ## What is an access
//!
//! Decided in `dev-diary/notes/issue-30-access-count.md`; in short:
//!
//! * every **concept** hit a recall returns to its caller, whether or not its
//!   block fit the token budget (the MCP and HTTP payloads carry every hit's
//!   content, so the caller received it) — once per recall per concept;
//! * a cache-served recall is still a recall, so it counts the same;
//! * an inspect counts its **focus** only (not the neighbourhood);
//! * reader processes (`lambo recall`, `serve-web`) do not count: they hold no
//!   lease and cannot write. A proxied call executes in the holder, so it
//!   counts once there.
//!
//! ## Why accesses do not advance the mutation epoch
//!
//! See [`Graph::record_accesses`]: GC's `gc_interval` trigger, the recall
//! cache key and hybrid replanning all key on the epoch, and none of them is
//! about reads. The access updates still ride the normal flush, so they are
//! durable without a separate dirty set or a schema change.
//!
//! ## Loss bound
//!
//! Notes live in RAM until the next daemon cycle applies them (≤ one tick),
//! then in the mutation log until the next flush (≤ `backend_flush_interval`),
//! the same bound every write-behind mutation has. `Memory::close` applies
//! what is left before its final drain, so a clean shutdown loses nothing; a
//! crash loses at most the last tick-plus-flush window of counts, which is a
//! frequency signal, not an acknowledged write.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use parking_lot::{Mutex, RwLock};

use crate::graph::Graph;
use crate::types::NodeId;

/// One concept's not-yet-applied accesses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Pending {
    count: u32,
    last: DateTime<Utc>,
}

/// Coalescing buffer between the read path and the graph (see module docs).
///
/// Interior-mutable and `Sync`: the owner shares one `Arc<AccessLedger>`
/// between its read paths and its daemon.
#[derive(Debug, Default)]
pub struct AccessLedger {
    pending: Mutex<HashMap<NodeId, Pending>>,
}

impl AccessLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Note one read that returned `ids` at `at`. Each distinct id counts
    /// **once** per call, however many times the iterator yields it.
    ///
    /// Takes only the ledger's own mutex — callers may hold the graph read
    /// lock or nothing; it never blocks on, or waits for, the graph.
    pub fn record(&self, ids: impl IntoIterator<Item = NodeId>, at: DateTime<Utc>) {
        let mut seen: Vec<NodeId> = ids.into_iter().collect();
        if seen.is_empty() {
            return;
        }
        seen.sort_unstable_by_key(|id| id.0);
        seen.dedup();
        let mut pending = self.pending.lock();
        for id in seen {
            pending
                .entry(id)
                .and_modify(|p| {
                    p.count = p.count.saturating_add(1);
                    p.last = p.last.max(at);
                })
                .or_insert(Pending { count: 1, last: at });
        }
    }

    /// Concepts with accesses noted but not yet applied.
    pub fn pending(&self) -> usize {
        self.pending.lock().len()
    }

    /// Take everything noted so far as `(id, count, last)` triples, leaving
    /// the ledger empty. Id-ascending, so the apply order is deterministic.
    pub fn take(&self) -> Vec<(NodeId, u32, DateTime<Utc>)> {
        let drained = std::mem::take(&mut *self.pending.lock());
        let mut out: Vec<_> = drained
            .into_iter()
            .map(|(id, p)| (id, p.count, p.last))
            .collect();
        out.sort_unstable_by_key(|(id, _, _)| id.0);
        out
    }

    /// Take everything noted and apply it to `graph` in one write section.
    /// Returns the number of concepts updated. The ledger mutex is released
    /// **before** the graph lock is taken, so the ledger stays a leaf lock.
    pub fn apply(&self, graph: &RwLock<Graph>) -> usize {
        let batch = self.take();
        if batch.is_empty() {
            return 0;
        }
        graph.write().record_accesses(&batch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        AgentId, CanonizationStatus, Concept, ConceptType, Interaction, Node, SessionId,
    };
    use chrono::TimeZone;
    use uuid::Uuid;

    fn ts(s: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000 + s, 0).unwrap()
    }

    fn nid(hi: u64, lo: u64) -> NodeId {
        NodeId(Uuid::from_u64_pair(hi, lo))
    }

    fn graph_with(n: u64) -> Graph {
        let sid = SessionId::from("issue-30");
        let mut g = Graph::new(sid.clone());
        let i = Interaction {
            event_time: None,
            id: nid(1, 1),
            session_id: sid.clone(),
            agent_id: AgentId::from("a"),
            prompt_text: None,
            previous_id: None,
            created_at: ts(0),
        };
        g.insert_interaction(i).unwrap();
        for k in 1..=n {
            let c = Concept {
                id: nid(2, k),
                session_id: sid.clone(),
                content: format!("concept {k}"),
                canonical_key: format!("concept {k}"),
                concept_type: ConceptType::Entity,
                origin_interaction: nid(1, 1),
                origin_agent: AgentId::from("a"),
                created_at: ts(0),
                access_count: 0,
                last_accessed: None,
                gc_survived: 0,
                canonization_status: CanonizationStatus::None,
                blast_radius: None,
                last_demotion_time: None,
                embedding: None,
                human_confirmed: 0,
                chunk_group_id: None,
            };
            g.insert_concept(c, nid(1, 1)).unwrap();
        }
        g
    }

    fn count(g: &Graph, id: NodeId) -> (i32, Option<DateTime<Utc>>) {
        match g.node(id) {
            Some(Node::Concept(c)) => (c.access_count, c.last_accessed),
            _ => panic!("not a concept"),
        }
    }

    #[test]
    fn one_call_counts_each_concept_once() {
        let ledger = AccessLedger::new();
        ledger.record([nid(2, 1), nid(2, 1), nid(2, 2)], ts(5));
        assert_eq!(
            ledger.take(),
            vec![(nid(2, 1), 1, ts(5)), (nid(2, 2), 1, ts(5))]
        );
        assert_eq!(ledger.pending(), 0, "take empties the ledger");
    }

    #[test]
    fn many_reads_coalesce_into_one_upsert_per_concept() {
        let ledger = AccessLedger::new();
        for k in 0..50 {
            ledger.record([nid(2, 1), nid(2, 2)], ts(k));
        }
        ledger.record([nid(2, 3)], ts(7));
        let graph = RwLock::new(graph_with(3));
        graph.write().drain_log();
        let epoch = graph.read().epoch();

        assert_eq!(ledger.apply(&graph), 3);
        let g = graph.read();
        assert_eq!(count(&g, nid(2, 1)), (50, Some(ts(49))));
        assert_eq!(count(&g, nid(2, 2)), (50, Some(ts(49))));
        assert_eq!(count(&g, nid(2, 3)), (1, Some(ts(7))));
        // 101 reads, 3 log entries: the write volume is per touched concept per
        // apply, not per read or per hit.
        assert_eq!(g.log_len(), 3);
        assert_eq!(g.epoch(), epoch, "accesses never advance the epoch");
    }

    #[test]
    fn apply_on_an_empty_ledger_touches_nothing() {
        let ledger = AccessLedger::new();
        let graph = RwLock::new(graph_with(1));
        graph.write().drain_log();
        assert_eq!(ledger.apply(&graph), 0);
        assert_eq!(graph.read().log_len(), 0);
    }

    #[test]
    fn a_concept_removed_before_the_apply_is_skipped() {
        let ledger = AccessLedger::new();
        ledger.record([nid(2, 1), nid(2, 2)], ts(1));
        let graph = RwLock::new(graph_with(2));
        graph.write().remove_node(nid(2, 2)).unwrap();
        assert_eq!(ledger.apply(&graph), 1);
        assert_eq!(count(&graph.read(), nid(2, 1)).0, 1);
    }

    /// The ledger is a leaf lock: `record` must complete while another thread
    /// holds the graph WRITE lock (a recall must never wait on a GC sweep to
    /// note its hits), and `apply` must not hold the ledger while it waits for
    /// the graph.
    #[test]
    fn record_never_waits_on_the_graph_lock() {
        let ledger = std::sync::Arc::new(AccessLedger::new());
        let graph = std::sync::Arc::new(RwLock::new(graph_with(1)));
        let guard = graph.write();
        let l2 = ledger.clone();
        let t = std::thread::spawn(move || l2.record([nid(2, 1)], ts(1)));
        t.join().unwrap();
        assert_eq!(ledger.pending(), 1);
        drop(guard);

        // `apply` blocked on the graph must leave the ledger free.
        let guard = graph.write();
        let (l3, g3) = (ledger.clone(), graph.clone());
        let applier = std::thread::spawn(move || l3.apply(&g3));
        // Wait until the applier has taken the ledger contents...
        while ledger.pending() != 0 {
            std::thread::yield_now();
        }
        // ...then a new note still goes through while it waits on the graph.
        ledger.record([nid(2, 1)], ts(2));
        assert_eq!(ledger.pending(), 1);
        drop(guard);
        assert_eq!(applier.join().unwrap(), 1);
        assert_eq!(ledger.apply(&graph), 1);
        assert_eq!(count(&graph.read(), nid(2, 1)), (2, Some(ts(2))));
    }
}
