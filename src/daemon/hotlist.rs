//! Daemon hot list — spec §9, P4 T4.2.
//!
//! A bounded priority queue of graph nodes whose conditions need surfacing in
//! `recall()` (force-inclusion, spec §8). Entry conditions retained:
//! **conflict detected**, **high-risk modification**, **drift detected**,
//! **stale session**. The daemon loop (T4.6) keeps the list equal to the
//! current cycle's fresh detector hits ([`HotList::retain_conditions`]); the
//! per-entry predicate ([`ConditionCheck`]) remains available for
//! recall-time re-validation (T5.3).
//!
//! ## Priority rule (T4.2 design decision)
//!
//! Entries are ranked by `(severity desc, recency desc, node id asc)`:
//!
//! 1. **severity** — the condition kind. Conflict is the most actionable
//!    (the demo trigger; active multi-agent interference), then high-risk
//!    modification (an active hazard), then drift (latent quality loss), then
//!    stale session (informational). [`Condition::severity`] assigns the
//!    numeric order.
//! 2. **recency** — insertion sequence, newest first. A fresh event outranks
//!    an old one of the same kind: the old one is closer to aging out of its
//!    detection window.
//! 3. **node id** — deterministic tie-break so ordering is stable.
//!
//! Eviction on overflow removes the **lowest** priority entry (oldest of the
//! least-severe condition); `recall()` consumes from the **highest**.
//!
//! ## One entry per (node, condition)
//!
//! Re-inserting an entry for a `(node, condition)` pair already present
//! **refreshes** it (new payload, new predicate, new recency) instead of
//! duplicating. The daemon re-runs detectors every cycle; without this the
//! list would fill with duplicates of a persisting condition. The bound
//! therefore caps distinct `(node, condition)` pairs.
//!
//! ## Re-validation
//!
//! Each entry carries the predicate that decides whether its condition still
//! holds, as a closure over the current graph ([`ConditionCheck`]). The
//! detector modules (T4.3 conflict, T4.4 drift) build the predicate at insert
//! time from the same graph logic and `now` they used to detect; the hot list
//! only evaluates it. Since T4.6 the daemon loop *does not* evaluate these
//! predicates each cycle — it diffs the list against the fresh detection
//! pass ([`HotList::retain_conditions`]), which replaces the old
//! O(hot_len × graph) per-cycle re-validation scan and guarantees no
//! captured-`now` ghosts linger (T4.6 finding 2). [`HotList::revalidate`]
//! stays public for recall (T5.3) to re-check an entry at read time.
//! It takes `&Graph` explicitly — rather than stashing a graph handle inside
//! the hot list — so recall never takes a hidden lock on top of the one it
//! already holds (the daemon's `RwLock<Graph>` is not reentrant; spec §6.4
//! lock discipline).

use std::cmp::Ordering;
use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use crate::graph::Graph;
use crate::types::{AgentId, NodeId};

/// Spec §9 `hot_list_max`; mirrors [`crate::config::Config::hot_list_max`]'s
/// default (1000). `Config` drives the daemon's construction; this const is
/// the module-level default for [`HotList::new`].
pub const HOT_LIST_MAX: usize = 1000;

/// Hot-list entry conditions (spec §9). Declared in priority order — see
/// [`Condition::severity`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Condition {
    /// ≥2 active agents with edges to the node and a recent write (T4.3).
    Conflict,
    /// A high-risk modification touched the node.
    HighRiskModification,
    /// The node drifted from every root goal (T4.4).
    Drift,
    /// The node's session has gone stale.
    StaleSession,
}

impl Condition {
    /// Priority severity: `Conflict > HighRiskModification > Drift >
    /// StaleSession`. Higher = more urgent on the hot list.
    pub fn severity(self) -> u8 {
        match self {
            Condition::Conflict => 3,
            Condition::HighRiskModification => 2,
            Condition::Drift => 1,
            Condition::StaleSession => 0,
        }
    }
}

/// Per-condition payload — what recall (T5.3) renders and what a re-validation
/// predicate reads. The conflict payload carries everything the demo sentence
/// needs: the agent(s) involved and how long ago the write happened
/// ("Agent A wrote to it eleven seconds ago").
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HotListPayload {
    /// Conflicting multi-agent write on [`HotListEntry::node`].
    Conflict {
        /// Agents with edges to the node (≥2), the conflicting writers.
        agents: Vec<AgentId>,
        /// Age of the most recent qualifying write, in seconds, at detection.
        seconds_ago: u64,
    },
    /// A high-risk modification touched the node.
    HighRisk { reason: String },
    /// The node drifted from any root goal.
    Drift {
        /// Shortest path length (weighted hops) to the nearest root goal.
        hops: u64,
        /// The root goal node the path terminates at (nil when no path).
        root: NodeId,
    },
    /// The node's session has been inactive this long, in seconds.
    Stale { seconds_inactive: u64 },
}

/// Re-validation predicate: `true` while the entry's condition still holds
/// against the current graph.
pub type ConditionCheck = Arc<dyn Fn(&Graph) -> bool + Send + Sync>;

/// One hot-list entry: the node, its condition kind, the payload recall
/// renders, and the predicate that decides whether the condition still holds.
///
/// Construct with [`HotListEntry::new`]. `seq` (insertion recency) is owned
/// by the [`HotList`]; `holds` is internal to re-validation.
pub struct HotListEntry {
    /// The hot node.
    pub node: NodeId,
    /// Why it is hot.
    pub condition: Condition,
    /// Condition-specific data for recall / re-validation.
    pub payload: HotListPayload,
    seq: u64,
    holds: ConditionCheck,
}

impl HotListEntry {
    /// Build an entry. `holds` is evaluated by [`HotList::revalidate`] on
    /// every recall; it must return `false` once the condition stops holding.
    pub fn new(
        node: NodeId,
        condition: Condition,
        payload: HotListPayload,
        holds: impl Fn(&Graph) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            node,
            condition,
            payload,
            seq: 0,
            holds: Arc::new(holds),
        }
    }

    /// The node this entry refers to.
    pub fn node(&self) -> NodeId {
        self.node
    }

    /// The condition kind.
    pub fn condition(&self) -> Condition {
        self.condition
    }

    /// The condition payload.
    pub fn payload(&self) -> &HotListPayload {
        &self.payload
    }
}

// `holds` is a closure, so `Debug` is hand-written; `Clone`/`PartialEq` are
// deliberately not implemented (the predicate cannot be compared).
impl fmt::Debug for HotListEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HotListEntry")
            .field("node", &self.node)
            .field("condition", &self.condition)
            .field("payload", &self.payload)
            .field("seq", &self.seq)
            .finish_non_exhaustive()
    }
}

/// Priority comparator: severity desc, recency desc, node id asc.
/// `entries` stays sorted so `entries[0]` is the highest-priority entry and
/// `pop()` (eviction) removes the lowest.
fn higher_priority(a: &HotListEntry, b: &HotListEntry) -> Ordering {
    b.condition
        .severity()
        .cmp(&a.condition.severity())
        .then_with(|| b.seq.cmp(&a.seq))
        .then_with(|| a.node.0.cmp(&b.node.0))
}

/// Bounded priority queue of hot nodes (spec §9).
///
/// `max` entries at most — by default [`HOT_LIST_MAX`]; overflow evicts the
/// lowest-priority entry. All operations are synchronous and lock-free; the
/// only graph interaction is [`HotList::revalidate`], which takes the graph
/// as a parameter.
pub struct HotList {
    max: usize,
    /// Priority-descending (`entries[0]` = highest priority).
    entries: Vec<HotListEntry>,
    /// Monotonic insertion sequence — recency rank.
    next_seq: u64,
}

impl HotList {
    /// A hot list bounded at [`HOT_LIST_MAX`] (spec §9 `hot_list_max=1000`).
    pub fn new() -> Self {
        Self::with_max(HOT_LIST_MAX)
    }

    /// A hot list with a custom bound (tests / `Config::hot_list_max`).
    pub fn with_max(max: usize) -> Self {
        Self {
            max,
            entries: Vec::new(),
            next_seq: 0,
        }
    }

    /// Insert (or refresh) an entry. If the `(node, condition)` pair is
    /// already present it is replaced — payload, predicate, and recency all
    /// refresh — so a persisting condition never duplicates.
    ///
    /// Returns the entry evicted by the bound, if any (`None` while the list
    /// stays within `max`). Discarding the result silently drops the evicted
    /// entry, so the compiler enforces it as `#[must_use]`: bind or `let _ =`
    /// the value explicitly if you don't need it.
    #[must_use]
    pub fn insert(&mut self, entry: HotListEntry) -> Option<HotListEntry> {
        let mut entry = entry;
        entry.seq = self.next_seq;
        self.next_seq += 1;

        match self
            .entries
            .iter_mut()
            .find(|e| e.node == entry.node && e.condition == entry.condition)
        {
            // Refresh in place; re-sort so the new recency takes effect.
            Some(existing) => *existing = entry,
            None => self.entries.push(entry),
        }
        self.entries.sort_by(higher_priority);

        // Enforce the bound: evict the lowest-priority entry from the tail.
        let mut evicted = None;
        while self.entries.len() > self.max {
            evicted = self.entries.pop();
        }
        evicted
    }

    /// Re-validate every entry for `node` against the current graph, dropping
    /// those whose condition no longer holds (spec §9: conditions re-validated
    /// on each `recall()` — stale entries drop out then, not on a timer).
    ///
    /// Returns `true` iff the node is still on the hot list (≥1 entry
    /// survived). Recall uses this to decide force-inclusion.
    pub fn revalidate(&mut self, graph: &Graph, node: NodeId) -> bool {
        let mut any_valid = false;
        self.entries.retain(|e| {
            if e.node == node {
                if (e.holds)(graph) {
                    any_valid = true;
                    true
                } else {
                    false
                }
            } else {
                true
            }
        });
        any_valid
    }
    /// Drop every entry whose `(node, condition)` is not in `fresh` (T4.6
    /// finding 2). The daemon loop calls this once per cycle with the
    /// current detection pass's hits, so the hot list always equals the
    /// *current* condition set: a HighRisk entry whose 30s window elapsed —
    /// or any condition that stopped being detected — is evicted here,
    /// deterministically, without evaluating captured-`now` predicates.
    pub fn retain_conditions(&mut self, fresh: &HashSet<(Condition, NodeId)>) {
        self.entries
            .retain(|e| fresh.contains(&(e.condition, e.node)));
    }

    /// Is `node` on the hot list (without re-validating)?
    pub fn contains(&self, node: NodeId) -> bool {
        self.entries.iter().any(|e| e.node == node)
    }

    /// Highest-priority entry, without removing it (recall consumes the list
    /// via [`HotList::iter`]; entries persist until their condition stops
    /// holding).
    pub fn peek(&self) -> Option<&HotListEntry> {
        self.entries.first()
    }

    /// Entries in priority order (highest first).
    pub fn iter(&self) -> impl Iterator<Item = &HotListEntry> {
        self.entries.iter()
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when the list holds no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The bound.
    pub fn max(&self) -> usize {
        self.max
    }
}

impl Default for HotList {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for HotList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HotList")
            .field("max", &self.max)
            .field("entries", &self.entries)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn nid(id: u64) -> NodeId {
        NodeId(Uuid::from_u64_pair(1, id))
    }

    fn sid() -> crate::types::SessionId {
        crate::types::SessionId::from("t4.2-hotlist")
    }

    /// An entry whose predicate is a plain flag (mocked "condition state").
    fn flagged_entry(
        node: NodeId,
        condition: Condition,
        payload: HotListPayload,
        flag: Arc<std::sync::atomic::AtomicBool>,
    ) -> HotListEntry {
        HotListEntry::new(node, condition, payload, move |_| {
            flag.load(std::sync::atomic::Ordering::SeqCst)
        })
    }

    fn conflict_payload(seconds_ago: u64) -> HotListPayload {
        HotListPayload::Conflict {
            agents: vec![AgentId::from("agent-a"), AgentId::from("agent-b")],
            seconds_ago,
        }
    }

    fn entry(node: NodeId, condition: Condition) -> HotListEntry {
        HotListEntry::new(node, condition, conflict_payload(5), |_| true)
    }

    // ------------------------------------------------------------------
    // Bound
    // ------------------------------------------------------------------

    #[test]
    fn default_bound_is_hot_list_max() {
        let list = HotList::new();
        assert_eq!(list.max(), HOT_LIST_MAX);
        assert_eq!(HOT_LIST_MAX, 1000);
    }

    #[test]
    fn overflow_evicts_lowest_priority_and_keeps_bound() {
        // All same condition → priority is purely recency (seq desc):
        // inserting n5..n1 yields [n5, n4, n3]; n1, n2 are evicted.
        let mut list = HotList::with_max(3);
        for i in 1..=3 {
            assert!(list.insert(entry(nid(i), Condition::Conflict)).is_none());
        }
        assert_eq!(list.len(), 3);

        let evicted_4th = list.insert(entry(nid(4), Condition::Conflict));
        assert_eq!(evicted_4th.unwrap().node, nid(1), "oldest evicted first");

        let evicted_5th = list.insert(entry(nid(5), Condition::Conflict));
        assert_eq!(evicted_5th.unwrap().node, nid(2));

        assert_eq!(list.len(), 3);
        let order: Vec<NodeId> = list.iter().map(|e| e.node).collect();
        assert_eq!(order, vec![nid(5), nid(4), nid(3)]);
        assert!(!list.contains(nid(1)));
        assert!(!list.contains(nid(2)));
        assert!(list.contains(nid(3)));
    }

    #[test]
    fn zero_bound_evicts_every_insert() {
        let mut list = HotList::with_max(0);
        for i in 1..=3 {
            let evicted = list.insert(entry(nid(i), Condition::Conflict));
            assert_eq!(evicted.unwrap().node, nid(i));
        }
        assert!(list.is_empty());
    }

    #[test]
    fn bound_enforced_under_mixed_severity_overflow() {
        // Fill with high-severity entries, then overflow with the lowest
        // severity: eviction must take the *least severe*, not the oldest.
        let mut list = HotList::with_max(3);
        let _ = list.insert(entry(nid(1), Condition::Conflict));
        let _ = list.insert(entry(nid(2), Condition::Drift));
        let _ = list.insert(entry(nid(3), Condition::StaleSession));
        let evicted = list.insert(entry(nid(4), Condition::StaleSession));
        assert_eq!(
            evicted.unwrap().condition,
            Condition::StaleSession,
            "lowest severity leaves first"
        );
        assert_eq!(list.len(), 3);
    }

    // ------------------------------------------------------------------
    // Priority ordering
    // ------------------------------------------------------------------

    #[test]
    fn severity_outranks_recency() {
        let mut list = HotList::new();
        // Inserted newest-first by severity: Stale (newest), Drift, Conflict (oldest).
        let _ = list.insert(entry(nid(1), Condition::StaleSession));
        let _ = list.insert(entry(nid(2), Condition::Drift));
        let _ = list.insert(entry(nid(3), Condition::Conflict));
        let order: Vec<(NodeId, Condition)> = list.iter().map(|e| (e.node, e.condition)).collect();
        assert_eq!(
            order,
            vec![
                (nid(3), Condition::Conflict),
                (nid(2), Condition::Drift),
                (nid(1), Condition::StaleSession),
            ],
            "Conflict > Drift > Stale regardless of insertion order"
        );
    }

    #[test]
    fn recency_breaks_ties_within_a_condition() {
        let mut list = HotList::new();
        let _ = list.insert(entry(nid(1), Condition::Conflict));
        let _ = list.insert(entry(nid(2), Condition::Conflict));
        // Newer of the same severity ranks higher.
        assert_eq!(list.peek().unwrap().node, nid(2));
    }

    #[test]
    fn refresh_replaces_payload_and_restores_recency() {
        let mut list = HotList::new();
        let _ = list.insert(HotListEntry::new(
            nid(1),
            Condition::Conflict,
            conflict_payload(30),
            |_| true,
        ));
        // A second conflict node, inserted after n1.
        let _ = list.insert(entry(nid(2), Condition::Conflict));
        // Re-detect n1: same node+condition, fresh payload (11s ago).
        let evicted = list.insert(HotListEntry::new(
            nid(1),
            Condition::Conflict,
            conflict_payload(11),
            |_| true,
        ));
        assert!(evicted.is_none(), "refresh must not count as overflow");
        assert_eq!(list.len(), 2, "no duplicate for (node, condition)");
        assert_eq!(
            list.peek().unwrap().payload,
            HotListPayload::Conflict {
                agents: vec![AgentId::from("agent-a"), AgentId::from("agent-b")],
                seconds_ago: 11,
            }
        );
        // Refresh restored n1's recency: it now outranks the later n2.
        let order: Vec<NodeId> = list.iter().map(|e| e.node).collect();
        assert_eq!(order, vec![nid(1), nid(2)]);
    }

    // ------------------------------------------------------------------
    // Re-validation
    // ------------------------------------------------------------------

    #[test]
    fn revalidate_evicts_a_condition_that_stops_holding() {
        let mut list = HotList::new();
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let _ = list.insert(flagged_entry(
            nid(1),
            Condition::Drift,
            conflict_payload(5),
            flag.clone(),
        ));
        assert!(list.contains(nid(1)));

        let g = Graph::new(sid());
        assert!(list.revalidate(&g, nid(1)), "condition still holds");

        // The condition stops holding (e.g. the drift was re-linked).
        flag.store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(!list.revalidate(&g, nid(1)), "node no longer hot");
        assert!(list.is_empty(), "stale entry evicted, not on a timer");
    }

    #[test]
    fn revalidate_keeps_valid_entries_and_drops_only_stale_ones() {
        let mut list = HotList::new();
        let ok = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let stale = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let _ = list.insert(flagged_entry(
            nid(1),
            Condition::Conflict,
            conflict_payload(5),
            ok.clone(),
        ));
        let _ = list.insert(flagged_entry(
            nid(1),
            Condition::Drift,
            conflict_payload(5),
            stale.clone(),
        ));
        let _ = list.insert(flagged_entry(
            nid(2),
            Condition::Conflict,
            conflict_payload(5),
            ok.clone(),
        ));

        stale.store(false, std::sync::atomic::Ordering::SeqCst);
        let g = Graph::new(sid());
        assert!(
            list.revalidate(&g, nid(1)),
            "one surviving entry keeps node hot"
        );
        assert_eq!(list.len(), 2);
        let survivors: Vec<Condition> = list
            .iter()
            .filter(|e| e.node == nid(1))
            .map(|e| e.condition)
            .collect();
        assert_eq!(survivors, vec![Condition::Conflict]);
        assert!(list.contains(nid(2)));
    }

    #[test]
    fn revalidate_absent_node_returns_false() {
        let mut list = HotList::new();
        let _ = list.insert(entry(nid(1), Condition::Conflict));
        let g = Graph::new(sid());
        assert!(!list.revalidate(&g, nid(2)), "no entry → not hot");
        assert_eq!(list.len(), 1);
    }
    #[test]
    fn retain_conditions_keeps_only_fresh_pairs() {
        let mut list = HotList::new();
        let _ = list.insert(entry(nid(1), Condition::Conflict));
        let _ = list.insert(entry(nid(1), Condition::Drift));
        let _ = list.insert(entry(nid(2), Condition::Conflict));
        let _ = list.insert(entry(nid(3), Condition::StaleSession));

        // Fresh pass: c1 and c2 conflicts still detected; c1's drift and
        // c3's staleness no longer hold (windows elapsed / condition gone).
        let fresh: HashSet<(Condition, NodeId)> =
            [(Condition::Conflict, nid(1)), (Condition::Conflict, nid(2))]
                .into_iter()
                .collect();
        list.retain_conditions(&fresh);

        let survivors: Vec<(Condition, NodeId)> =
            list.iter().map(|e| (e.condition, e.node)).collect();
        assert_eq!(
            survivors,
            vec![(Condition::Conflict, nid(2)), (Condition::Conflict, nid(1)),],
            "only fresh pairs survive, priority order preserved"
        );
    }

    // ------------------------------------------------------------------
    // Fixture-backed: predicates read real graph state
    // ------------------------------------------------------------------

    #[test]
    fn fixture_graph_predicate_revalidates_against_real_state() {
        use crate::fixtures::load_snapshot;
        use crate::types::EdgeType;

        let snap = load_snapshot("session-rest-api").unwrap();
        let mut g = Graph::from_snapshot(snap).unwrap();
        // Pick the node the fixture derives most recently (a concept).
        let (node, edge_id) = {
            let concept_id = g.concepts().next().unwrap().id;
            let edge = g
                .incident_edges(concept_id)
                .into_iter()
                .find(|e| e.edge_type == EdgeType::Derives)
                .map(|e| e.id)
                .unwrap();
            (concept_id, edge)
        };

        // Condition: "still has a Derives edge from its interaction".
        let mut list = HotList::new();
        let _ = list.insert(HotListEntry::new(
            node,
            Condition::Conflict,
            conflict_payload(11),
            move |g: &Graph| {
                g.incident_edges(node)
                    .iter()
                    .any(|e| e.edge_type == EdgeType::Derives)
            },
        ));

        assert!(list.revalidate(&g, node), "edge present → condition holds");
        // The condition stops holding: the deriving edge is removed.
        g.remove_edge(edge_id).unwrap();
        assert!(!list.revalidate(&g, node), "edge gone → node evicted");
        assert!(list.is_empty());
    }

    #[test]
    fn fixture_conflict_payload_survives_roundtrip_for_rendering() {
        // The demo sentence T5.3 renders — "Agent A wrote to it eleven
        // seconds ago" — must be derivable from the payload alone.
        use crate::types::AgentId;
        let payload = HotListPayload::Conflict {
            agents: vec![AgentId::from("agent-a")],
            seconds_ago: 11,
        };
        let mut list = HotList::new();
        let _ = list.insert(HotListEntry::new(
            nid(9),
            Condition::Conflict,
            payload,
            |_| true,
        ));
        let e = list.peek().unwrap();
        match &e.payload {
            HotListPayload::Conflict {
                agents,
                seconds_ago,
            } => {
                assert_eq!(agents[0].as_str(), "agent-a");
                assert_eq!(*seconds_ago, 11);
            }
            other => panic!("unexpected payload {other:?}"),
        }
        assert_eq!(e.node, nid(9));
    }
}
