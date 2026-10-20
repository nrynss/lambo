//! Daemon hot list — spec §9, P4 T4.2.
//!
//! A bounded priority queue of graph nodes whose conditions need surfacing in
//! `recall()` (force-inclusion, spec §8). Entry conditions retained:
//! **conflict detected**, **high-risk modification**, **drift detected**,
//! **stale session**, plus #19(c)'s **contradiction**. The daemon loop (T4.6) keeps the list equal to the
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
//! ## The contradiction budget (#196)
//!
//! Contradictions share the top severity tier, persist until newer evidence
//! outweighs them, and since #179 every live one is an entry, so a store
//! with many could fill the whole list and evict every high-risk, drift and
//! stale warning. They are held to a budget instead: at most
//! [`HotList::contradiction_max`] contradiction entries, by default half
//! the bound ([`default_contradiction_max`]). Over it, the lowest-priority
//! contradictions go first; the overall bound then evicts as before. The
//! rest of the list is reserved for the other kinds: no number of
//! contradictions can evict them from it. Conflicts are not counted; they
//! outrank every other kind anyway. Under the budget nothing is evicted
//! that the bound alone would not evict. The daemon inserts a cycle's
//! contradictions oldest-declared first, so a full budget keeps the most
//! recently declared. A contradiction dropped by the budget carries no
//! recall warning until it fits again, exactly as one dropped by the bound.
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
//! holds, as a closure over the current graph **and the caller's `now`**
//! ([`ConditionCheck`]). The detector modules (T4.3 conflict, T4.4 drift,
//! T4.6 stale/high-risk) build the predicate at insert time from the same
//! per-node graph logic they used to detect; the hot list only evaluates it.
//!
//! The predicate takes `now` as a **parameter** and returns the recomputed
//! payload (XP-3). A predicate that captured `now` by move re-derived its
//! recency window from a frozen instant, so a `Conflict`/`HighRisk` entry
//! re-validated `true` forever against an unchanged graph and served a frozen
//! `seconds_ago` — the exact API T5.3 is told to call, rendering "wrote to it
//! eleven seconds ago" five minutes later. Handing `now` in and taking the
//! payload out makes both halves impossible to get wrong.
//!
//! Predicates are **per-node** (CONC-5): each re-check costs one node's
//! neighborhood, not a whole-graph detection pass. Recall force-including ten
//! hot nodes therefore costs ten neighborhood walks under the graph lock, not
//! ten full scans.
//!
//! Since T4.6 the daemon loop *does not* evaluate these predicates each cycle —
//! it diffs the list against the fresh detection pass
//! ([`HotList::retain_conditions`]), which replaces the old
//! O(hot_len × graph) per-cycle re-validation scan (T4.6 finding 2).
//! [`HotList::revalidate`] is recall's (T5.3) read-time path, run through
//! [`HotList::revalidate_members`] by the daemon's recall entry before
//! assembly.
//! It takes `&Graph` explicitly — rather than stashing a graph handle inside
//! the hot list — so recall never takes a hidden lock on top of the one it
//! already holds (the daemon's `RwLock<Graph>` is not reentrant; spec §6.4
//! lock discipline).

use chrono::{DateTime, Utc};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use crate::graph::Graph;
use crate::types::NodeId;

/// Spec §9 `hot_list_max`; mirrors [`crate::config::Config::hot_list_max`]'s
/// default (1000). `Config` drives the daemon's construction; this const is
/// the module-level default for [`HotList::new`].
pub const HOT_LIST_MAX: usize = 1000;

/// The contradiction budget for a hot list bounded at `max` when none is
/// configured (#196): half the bound, rounded down, so the other half is
/// reserved for the other kinds.
pub fn default_contradiction_max(max: usize) -> usize {
    max / 2
}

/// Hot-list entry conditions (spec §9, plus #19(c)). Declared in priority
/// order, `Contradiction` last but sharing `Conflict`'s tier — see
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
    /// A node is contradicted by a newer assertion (#19(c); any status since
    /// #179, [`crate::daemon::contradiction`]). Hot-list only: it emits no
    /// `DaemonEvent` (the §6.1 enum is frozen).
    Contradiction,
}

impl Condition {
    /// Priority severity, highest first: `Conflict` and `Contradiction`
    /// (one tier), then `HighRiskModification`, `Drift`, `StaleSession`.
    /// Higher = more urgent on the hot list. A contradiction shares the
    /// conflict tier (#19(c)): both are warnings an agent should act on
    /// before trusting the node. Within the tier, insertion order decides
    /// (the later insert ranks higher and is evicted last); the daemon cycle
    /// inserts contradictions before conflicts, so on a full list a
    /// persistent contradiction is evicted before a transient conflict.
    pub fn severity(self) -> u8 {
        match self {
            Condition::Conflict | Condition::Contradiction => 3,
            Condition::HighRiskModification => 2,
            Condition::Drift => 1,
            Condition::StaleSession => 0,
        }
    }
}

/// Per-condition payload — recall renders it and a re-validation predicate
/// rebuilds it. Read-side data shared with recall, so it lives in
/// [`crate::types::HotListPayload`]; re-exported here so this path stays valid.
pub use crate::types::HotListPayload;

/// Re-validation predicate: recompute the entry's condition against the current
/// graph **at `now`**.
///
/// `Some(payload)` — the condition still holds, and the payload is recomputed
/// from read-time state; `None` — it has lapsed and the entry is dropped.
///
/// Taking `now` as a parameter rather than capturing it at detection is the
/// whole point (XP-3): a closure that froze `now` re-derived its recency window
/// from a fixed instant, so a `Conflict` or `HighRisk` entry re-validated `true`
/// forever against an unchanged graph and rendered a frozen `seconds_ago` — the
/// demo's "wrote to it eleven seconds ago" would still say eleven seconds five
/// minutes later. Returning the payload (rather than a bool) is what makes the
/// refresh unavoidable: there is no way to re-validate an entry without also
/// updating what it says.
pub type ConditionCheck =
    Arc<dyn Fn(&Graph, DateTime<Utc>) -> Option<HotListPayload> + Send + Sync>;

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
    /// every recall; it must return `None` once the condition stops holding,
    /// and otherwise the payload as of the `now` it was handed.
    pub fn new(
        node: NodeId,
        condition: Condition,
        payload: HotListPayload,
        holds: impl Fn(&Graph, DateTime<Utc>) -> Option<HotListPayload> + Send + Sync + 'static,
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

/// Priority comparator: severity desc, recency desc.
///
/// `seq` is assigned fresh on every insert and strictly monotonic
/// (`next_seq`), and entries are only replaced in place or pushed, so no two
/// live entries share a seq — the seq arm alone is already a total order. The
/// trailing node-id arm is kept so the comparator stays total if that
/// uniqueness invariant ever fails; it never decides today (an unreachable
/// formality, remediation round 2 — the old comment presented it as a live
/// tie-break).
///
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
    /// At most this many `Contradiction` entries (#196); `>= max` means no
    /// budget beyond the bound.
    contradiction_max: usize,
    /// Priority-descending (`entries[0]` = highest priority).
    entries: Vec<HotListEntry>,
    /// Monotonic insertion sequence — recency rank.
    next_seq: u64,
}

impl HotList {
    /// A hot list bounded at [`HOT_LIST_MAX`] (spec §9 `hot_list_max=1000`),
    /// with the default contradiction budget.
    pub fn new() -> Self {
        Self::with_max(HOT_LIST_MAX)
    }

    /// A hot list with a custom bound (tests / `Config::hot_list_max`) and
    /// the default contradiction budget ([`default_contradiction_max`]).
    pub fn with_max(max: usize) -> Self {
        Self::with_budget(max, default_contradiction_max(max))
    }

    /// A hot list bounded at `max` holding at most `contradiction_max`
    /// contradictions (#196, `Config::hot_list_contradiction_max`). A budget
    /// at or above `max` reserves nothing.
    pub fn with_budget(max: usize, contradiction_max: usize) -> Self {
        Self {
            max,
            contradiction_max,
            entries: Vec::new(),
            next_seq: 0,
        }
    }

    /// The contradiction budget (#196).
    pub fn contradiction_max(&self) -> usize {
        self.contradiction_max
    }

    /// Sort, then enforce the contradiction budget and the bound: the
    /// lowest-priority contradictions beyond the budget go first, then the
    /// lowest entries beyond `max`. Returns what was evicted, lowest
    /// priority last.
    fn enforce(&mut self) -> Vec<HotListEntry> {
        self.entries.sort_by(higher_priority);
        let mut evicted = Vec::new();
        let over = self
            .entries
            .iter()
            .filter(|e| e.condition == Condition::Contradiction)
            .count()
            .saturating_sub(self.contradiction_max);
        if over > 0 {
            // The lowest-priority contradictions sit nearest the tail.
            let mut seen = 0usize;
            let budget = self.contradiction_max;
            let (kept, dropped): (Vec<_>, Vec<_>) = std::mem::take(&mut self.entries)
                .into_iter()
                .partition(|e| {
                    if e.condition != Condition::Contradiction {
                        return true;
                    }
                    seen += 1;
                    seen <= budget
                });
            self.entries = kept;
            evicted.extend(dropped);
        }
        let keep = self.entries.len().min(self.max);
        evicted.extend(self.entries.split_off(keep));
        evicted
    }

    /// Insert (or refresh) an entry. If the `(node, condition)` pair is
    /// already present it is replaced — payload, predicate, and recency all
    /// refresh — so a persisting condition never duplicates.
    ///
    /// Returns the entry evicted by the contradiction budget or the bound, if
    /// any (`None` while the list stays within both). Discarding the result silently drops the evicted
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
        // Enforce the budget and the bound (#196). One entry was added, so
        // at most one goes; the lowest-priority is the last evicted.
        self.enforce().pop()
    }

    /// [`HotList::insert`] for many entries at once, in order: the same
    /// final list (each entry gets the next `seq`, a present `(node,
    /// condition)` is refreshed in place, then the contradiction budget and
    /// the bound evict the lowest), sorted once instead of once per entry.
    /// Returns what was evicted.
    ///
    /// Equivalent to inserting one by one because priority is a fixed total
    /// order per entry and a refresh only raises one: an entry evicted at
    /// some step has `max` entries above it (a contradiction: `max` or the
    /// budget's worth of contradictions), and none of them can later be
    /// replaced by one below it.
    /// #179 made every contradiction an entry, so a daemon cycle can insert
    /// hundreds; per-entry insertion re-sorted the whole list each time,
    /// under the hot-list write lock recall also takes.
    pub fn insert_many(
        &mut self,
        entries: impl IntoIterator<Item = HotListEntry>,
    ) -> Vec<HotListEntry> {
        let mut at: std::collections::HashMap<(NodeId, Condition), usize> = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| ((e.node, e.condition), i))
            .collect();
        for mut entry in entries {
            entry.seq = self.next_seq;
            self.next_seq += 1;
            match at.get(&(entry.node, entry.condition)) {
                Some(&i) => self.entries[i] = entry,
                None => {
                    at.insert((entry.node, entry.condition), self.entries.len());
                    self.entries.push(entry);
                }
            }
        }
        self.enforce()
    }

    /// Re-validate every entry for `node` against the current graph **at
    /// `now`**, dropping those whose condition no longer holds and refreshing
    /// the payload of those that do (spec §9: conditions re-validated on each
    /// `recall()` — stale entries drop out then, not on a timer).
    ///
    /// Returns `true` iff the node is still on the hot list (≥1 entry
    /// survived). Recall (T5.3) uses this to decide force-inclusion and then
    /// renders [`HotListEntry::payload`], which this call has just rebuilt
    /// against `now` — so a rendered `seconds_ago` is the age at *read* time,
    /// not at detection time (XP-3).
    ///
    /// `now` is the caller's clock: recall passes its own, so an entry whose
    /// recency window elapsed between detection and this read is dropped here
    /// rather than surviving forever against a captured instant.
    pub fn revalidate(&mut self, graph: &Graph, node: NodeId, now: DateTime<Utc>) -> bool {
        let mut any_valid = false;
        self.entries.retain_mut(|e| {
            if e.node != node {
                return true;
            }
            match (e.holds)(graph, now) {
                Some(payload) => {
                    e.payload = payload;
                    any_valid = true;
                    true
                }
                None => false,
            }
        });
        any_valid
    }

    /// Re-validate the hot entries of every node in `members` **at `now`** and
    /// return the surviving payloads per node, freshly rebuilt (T5.3
    /// force-inclusion, XP-3).
    ///
    /// This is recall's read-time maintenance of the hot list, run by
    /// `Daemon::recall_detailed` over the recall's expanded
    /// members before assembly: each member on the list goes through
    /// [`HotList::revalidate`], so lapsed entries are evicted here and a
    /// survivor's payload is the one its predicate just rebuilt against `now`.
    /// Recall then renders the returned map and never touches the list itself,
    /// which keeps the dependency one-way (daemon → recall).
    ///
    /// The caller passes the same `now` it renders with (reservations, the
    /// context block), and holds the graph read guard before this list's write
    /// guard (module lock order). A node whose entries all lapsed, or that is
    /// not on the list, is absent from the map; duplicate ids in `members` are
    /// re-validated once.
    pub fn revalidate_members(
        &mut self,
        graph: &Graph,
        members: impl IntoIterator<Item = NodeId>,
        now: DateTime<Utc>,
    ) -> HashMap<NodeId, Vec<HotListPayload>> {
        let ids: HashSet<NodeId> = members.into_iter().collect();
        let mut payloads: HashMap<NodeId, Vec<HotListPayload>> = HashMap::new();
        for id in ids {
            if self.contains(id) && self.revalidate(graph, id, now) {
                let surviving: Vec<HotListPayload> = self
                    .iter()
                    .filter(|e| e.node() == id)
                    .map(|e| e.payload().clone())
                    .collect();
                if !surviving.is_empty() {
                    payloads.insert(id, surviving);
                }
            }
        }
        payloads
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
    use crate::types::AgentId;
    use uuid::Uuid;

    fn nid(id: u64) -> NodeId {
        NodeId(Uuid::from_u64_pair(1, id))
    }

    fn sid() -> crate::types::SessionId {
        crate::types::SessionId::from("t4.2-hotlist")
    }

    /// Whole-second timestamp helper.
    fn t(s: i64) -> DateTime<Utc> {
        chrono::TimeZone::timestamp_opt(&Utc, 1_700_000_000 + s, 0).unwrap()
    }

    /// An entry whose predicate is a plain flag (mocked "condition state"),
    /// echoing back the payload it was built with.
    fn flagged_entry(
        node: NodeId,
        condition: Condition,
        payload: HotListPayload,
        flag: Arc<std::sync::atomic::AtomicBool>,
    ) -> HotListEntry {
        let echo = payload.clone();
        HotListEntry::new(node, condition, payload, move |_, _| {
            flag.load(std::sync::atomic::Ordering::SeqCst)
                .then(|| echo.clone())
        })
    }

    fn conflict_payload(seconds_ago: u64) -> HotListPayload {
        HotListPayload::Conflict {
            agents: vec![AgentId::from("agent-a"), AgentId::from("agent-b")],
            writer: AgentId::from("agent-b"),
            seconds_ago,
        }
    }

    fn entry(node: NodeId, condition: Condition) -> HotListEntry {
        let payload = conflict_payload(5);
        let echo = payload.clone();
        HotListEntry::new(node, condition, payload, move |_, _| Some(echo.clone()))
    }

    /// An entry whose predicate re-derives `seconds_ago` from the `now` it is
    /// handed — the shape every real detector uses (XP-3).
    fn clock_reading_entry(node: NodeId, written_at: DateTime<Utc>, window: i64) -> HotListEntry {
        HotListEntry::new(
            node,
            Condition::Conflict,
            conflict_payload(0),
            move |_, at: DateTime<Utc>| {
                let age = (at - written_at).num_seconds();
                (age >= 0 && age <= window).then(|| HotListPayload::Conflict {
                    agents: vec![AgentId::from("agent-a"), AgentId::from("agent-b")],
                    writer: AgentId::from("agent-b"),
                    seconds_ago: age as u64,
                })
            },
        )
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
            |_, _| Some(conflict_payload(30)),
        ));
        // A second conflict node, inserted after n1.
        let _ = list.insert(entry(nid(2), Condition::Conflict));
        // Re-detect n1: same node+condition, fresh payload (11s ago).
        let evicted = list.insert(HotListEntry::new(
            nid(1),
            Condition::Conflict,
            conflict_payload(11),
            |_, _| Some(conflict_payload(11)),
        ));
        assert!(evicted.is_none(), "refresh must not count as overflow");
        assert_eq!(list.len(), 2, "no duplicate for (node, condition)");
        assert_eq!(
            list.peek().unwrap().payload,
            HotListPayload::Conflict {
                agents: vec![AgentId::from("agent-a"), AgentId::from("agent-b")],
                writer: AgentId::from("agent-b"),
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
        assert!(list.revalidate(&g, nid(1), t(0)), "condition still holds");

        // The condition stops holding (e.g. the drift was re-linked).
        flag.store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(!list.revalidate(&g, nid(1), t(0)), "node no longer hot");
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
            list.revalidate(&g, nid(1), t(0)),
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

    /// XP-3: `revalidate` consults the `now` it is handed, and the surviving
    /// entry's payload is rebuilt from it.
    ///
    /// Pre-fix the predicate was `Fn(&Graph) -> bool` over a captured `now`, so
    /// neither half was expressible: the condition could not age out and the
    /// payload could not be refreshed.
    #[test]
    fn revalidate_uses_the_callers_clock_and_refreshes_the_payload() {
        let mut list = HotList::new();
        let written_at = t(1000);
        let _ = list.insert(clock_reading_entry(nid(1), written_at, 30));
        let g = Graph::new(sid());

        // 11s later: holds, and says eleven.
        assert!(list.revalidate(&g, nid(1), t(1011)));
        match &list.peek().unwrap().payload {
            HotListPayload::Conflict { seconds_ago, .. } => assert_eq!(*seconds_ago, 11),
            other => panic!("unexpected payload {other:?}"),
        }

        // 25s later: still holds, and now says twenty-five — not eleven.
        assert!(list.revalidate(&g, nid(1), t(1025)));
        match &list.peek().unwrap().payload {
            HotListPayload::Conflict { seconds_ago, .. } => assert_eq!(
                *seconds_ago, 25,
                "the payload must track read time, not detection time"
            ),
            other => panic!("unexpected payload {other:?}"),
        }

        // Past the window, graph never touched: the entry is gone.
        assert!(!list.revalidate(&g, nid(1), t(1031)));
        assert!(list.is_empty(), "the clock alone must age an entry out");
    }

    #[test]
    fn revalidate_absent_node_returns_false() {
        let mut list = HotList::new();
        let _ = list.insert(entry(nid(1), Condition::Conflict));
        let g = Graph::new(sid());
        assert!(!list.revalidate(&g, nid(2), t(0)), "no entry → not hot");
        assert_eq!(list.len(), 1);
    }

    /// T5.3 force-include, the hot-list half (moved from
    /// `recall::assemble::tests::hot_force_include_keeps_live_and_drops_lapsed`
    /// when #25 moved re-validation out of `assemble`): over recall's expanded
    /// members, a live entry survives with its payload rebuilt at the caller's
    /// `now` (not the stale sentinel it was inserted with), a lapsed one is
    /// evicted from the list, a member that is not hot is absent from the map,
    /// and a hot node that is not a member is left alone.
    #[test]
    fn revalidate_members_keeps_live_and_drops_lapsed() {
        let g = Graph::new(sid());
        let now = t(60);
        let mut list = HotList::new();
        // nid(1): live (write 11s before now, 30s window).
        let _ = list.insert(clock_reading_entry(
            nid(1),
            now - chrono::Duration::seconds(11),
            30,
        ));
        // nid(2): lapsed (write 31s before now).
        let _ = list.insert(clock_reading_entry(
            nid(2),
            now - chrono::Duration::seconds(31),
            30,
        ));
        // nid(4): lapsed too, but not a recall member, so not re-validated.
        let _ = list.insert(clock_reading_entry(
            nid(4),
            now - chrono::Duration::seconds(31),
            30,
        ));

        // nid(3) is a member that is not hot; nid(1) is listed twice.
        let payloads = list.revalidate_members(&g, [nid(1), nid(2), nid(3), nid(1)], now);

        assert_eq!(
            payloads,
            HashMap::from([(nid(1), vec![conflict_payload(11)])]),
            "only the live member, with its payload rebuilt at now (not the 0 sentinel)"
        );
        assert!(list.contains(nid(1)), "the live entry persists");
        assert!(
            !list.contains(nid(2)),
            "the lapsed member's entry was evicted"
        );
        assert!(list.contains(nid(4)), "a non-member is not re-validated");
        assert_eq!(
            list.iter()
                .find(|e| e.node() == nid(1))
                .map(|e| e.payload().clone()),
            Some(conflict_payload(11)),
            "the list holds the read-time payload too"
        );
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

    #[cfg(feature = "fixtures")]
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
            move |g: &Graph, _at: DateTime<Utc>| {
                g.incident_edges(node)
                    .iter()
                    .any(|e| e.edge_type == EdgeType::Derives)
                    .then(|| conflict_payload(11))
            },
        ));

        assert!(
            list.revalidate(&g, node, t(0)),
            "edge present → condition holds"
        );
        // The condition stops holding: the deriving edge is removed.
        g.remove_edge(edge_id).unwrap();
        assert!(!list.revalidate(&g, node, t(0)), "edge gone → node evicted");
        assert!(list.is_empty());
    }

    #[test]
    fn fixture_conflict_payload_survives_roundtrip_for_rendering() {
        // The demo sentence T5.3 renders — "Agent A wrote to it eleven
        // seconds ago" — must be derivable from the payload alone: the
        // `writer` names the subject (ALGO-2), `seconds_ago` the age.
        use crate::types::AgentId;
        let payload = HotListPayload::Conflict {
            agents: vec![AgentId::from("agent-a"), AgentId::from("agent-b")],
            writer: AgentId::from("agent-a"),
            seconds_ago: 11,
        };
        let echo = payload.clone();
        let mut list = HotList::new();
        let _ = list.insert(HotListEntry::new(
            nid(9),
            Condition::Conflict,
            payload,
            move |_, _| Some(echo.clone()),
        ));
        let e = list.peek().unwrap();
        match &e.payload {
            HotListPayload::Conflict {
                agents,
                writer,
                seconds_ago,
            } => {
                assert_eq!(agents[0].as_str(), "agent-a");
                assert_eq!(writer.as_str(), "agent-a");
                assert_eq!(*seconds_ago, 11);
            }
            other => panic!("unexpected payload {other:?}"),
        }
        assert_eq!(e.node, nid(9));
    }

    /// `insert_many` leaves exactly the list one-by-one `insert` would:
    /// order, refreshes and the bound.
    #[test]
    fn insert_many_matches_inserting_one_by_one() {
        let conds = [
            Condition::Contradiction,
            Condition::StaleSession,
            Condition::Conflict,
            Condition::Drift,
            Condition::HighRiskModification,
        ];
        let plan: Vec<(u64, Condition)> = (0..40u64)
            .map(|i| ((i * 7) % 13, conds[(i as usize * 3) % conds.len()]))
            .collect();
        let runs = [0, 3, 8, 100]
            .into_iter()
            .flat_map(|max| [None, Some(0), Some(2), Some(max)].map(|b| (max, b)));
        for (max, budget) in runs {
            let make = || match budget {
                Some(b) => HotList::with_budget(max, b),
                None => HotList::with_max(max),
            };
            let mut one = make();
            one.insert_many(std::iter::once(entry(nid(99), Condition::Drift)));
            let mut seq = make();
            let _ = seq.insert(entry(nid(99), Condition::Drift));
            for &(n, c) in &plan {
                let _ = seq.insert(entry(nid(n), c));
            }
            let evicted_one: Vec<(NodeId, Condition)> = one
                .insert_many(plan.iter().map(|&(n, c)| entry(nid(n), c)))
                .into_iter()
                .map(|e| (e.node, e.condition))
                .collect();
            let shape = |l: &HotList| -> Vec<(NodeId, Condition)> {
                l.iter().map(|e| (e.node, e.condition)).collect()
            };
            assert_eq!(shape(&one), shape(&seq), "max {max} budget {budget:?}");
            assert!(one.len() <= max);
            let contradictions = one
                .iter()
                .filter(|e| e.condition == Condition::Contradiction)
                .count();
            assert!(contradictions <= one.contradiction_max());
            // What the batch evicted is not on the list. (A one-by-one run
            // can evict an entry and take it back on a later refresh, so its
            // eviction log is not comparable.)
            let kept = shape(&one);
            for e in &evicted_one {
                assert!(!kept.contains(e), "max {max}: {e:?} evicted and kept");
            }
        }
    }

    // ------------------------------------------------------------------
    // Contradiction budget (#196)
    // ------------------------------------------------------------------

    fn count(list: &HotList, c: Condition) -> usize {
        list.iter().filter(|e| e.condition == c).count()
    }

    #[test]
    fn the_default_contradiction_budget_is_half_the_bound() {
        assert_eq!(HotList::new().contradiction_max(), HOT_LIST_MAX / 2);
        assert_eq!(HotList::with_max(7).contradiction_max(), 3);
        assert_eq!(default_contradiction_max(0), 0);
    }

    /// A flood of contradictions, inserted after the other kinds and
    /// outranking every one of them, leaves the drift, stale and high-risk
    /// warnings in place: contradictions stop at the budget.
    #[test]
    fn a_flood_of_contradictions_does_not_evict_the_other_kinds() {
        let mut list = HotList::with_max(10);
        let _ = list.insert(entry(nid(1), Condition::Drift));
        let _ = list.insert(entry(nid(2), Condition::StaleSession));
        let _ = list.insert(entry(nid(3), Condition::HighRiskModification));
        let evicted = list.insert_many((100..200).map(|n| entry(nid(n), Condition::Contradiction)));
        assert_eq!(count(&list, Condition::Contradiction), 5);
        assert_eq!(count(&list, Condition::Drift), 1);
        assert_eq!(count(&list, Condition::StaleSession), 1);
        assert_eq!(count(&list, Condition::HighRiskModification), 1);
        assert_eq!(evicted.len(), 95);
        assert!(evicted
            .iter()
            .all(|e| e.condition == Condition::Contradiction));
        // The latest-inserted contradictions are the ones kept.
        let kept: Vec<NodeId> = list
            .iter()
            .filter(|e| e.condition == Condition::Contradiction)
            .map(|e| e.node)
            .collect();
        assert_eq!(kept, (195..200).rev().map(nid).collect::<Vec<_>>());
        // One-by-one: a further contradiction evicts a contradiction, never
        // the drift warning.
        let gone = list
            .insert(entry(nid(500), Condition::Contradiction))
            .unwrap();
        assert_eq!(gone.node, nid(195));
        assert!(list.contains(nid(1)));
    }

    /// The budget caps contradictions only; it never evicts anything while
    /// the list is within it, and conflicts (the same tier) are not counted.
    #[test]
    fn under_the_budget_nothing_changes() {
        let mut list = HotList::with_max(6);
        for n in 1..=3 {
            assert!(list
                .insert(entry(nid(n), Condition::Contradiction))
                .is_none());
        }
        for n in 10..13 {
            assert!(list.insert(entry(nid(n), Condition::Conflict)).is_none());
        }
        assert_eq!(list.len(), 6);
        // Over the bound, the lowest entry goes as before.
        let gone = list.insert(entry(nid(20), Condition::Conflict)).unwrap();
        assert_eq!(gone.node, nid(1));
    }

    /// Spare budget is not reserved for contradictions: other kinds may
    /// fill the whole list, and a later contradiction still takes its place
    /// by priority.
    #[test]
    fn other_kinds_can_use_the_whole_list() {
        let mut list = HotList::with_max(4);
        for n in 1..=4 {
            assert!(list.insert(entry(nid(n), Condition::Drift)).is_none());
        }
        let gone = list
            .insert(entry(nid(9), Condition::Contradiction))
            .unwrap();
        assert_eq!(gone.node, nid(1));
        assert!(list.contains(nid(9)));
    }

    /// A budget at the bound reserves nothing (the pre-#196 behaviour).
    #[test]
    fn a_budget_at_the_bound_reserves_nothing() {
        let mut list = HotList::with_budget(3, 3);
        let _ = list.insert(entry(nid(1), Condition::Drift));
        let _ = list.insert_many((10..13).map(|n| entry(nid(n), Condition::Contradiction)));
        assert_eq!(count(&list, Condition::Contradiction), 3);
        assert!(!list.contains(nid(1)));
    }
}
