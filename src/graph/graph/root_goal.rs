//! Session metadata: the root goal (spec §9 drift anchor) and the one reading
//! of its shape ([`root_goal_texts`]), the session's logical clock, direct
//! synonyms, and advisory reservations (spec §11).
//!
//! The root goal is durable (`Mutation::SetRootGoal`) and promotes the
//! concepts it names through the canonization write gate. Synonyms and
//! reservations are RAM-local: they have no `Mutation` kind, round-trip
//! through the snapshot only, and bump the epoch directly when they change
//! what recall or hybrid planning would see.

use super::Graph;
use crate::types::{CanonizationEvent, CanonizationStatus, Mutation, Node, NodeId, Reservation};

impl Graph {
    /// Declare (or replace) a direct synonym mapping. RAM-local: synonyms have no
    /// `Mutation` kind and round-trip through the snapshot only. A changed
    /// mapping still bumps the epoch because it changes canonicalization results
    /// observed by hybrid planning and recall.
    pub fn declare_synonym(&mut self, source_key: &str, canonical_key: &str) {
        let changed = self.synonyms.get(source_key).map(String::as_str) != Some(canonical_key);
        if changed {
            self.synonyms
                .insert(source_key.to_string(), canonical_key.to_string());
            self.epoch += 1;
        }
    }

    /// Declare the session's root goal (spec §9 drift anchor).
    ///
    /// Spec §9: "Root goal nodes are automatically `Venerable`" — **every**
    /// concept the goal names (matched by `content` or `canonical_key`) is
    /// promoted to `Venerable` through the T2.1 mutation path
    /// ([`Graph::apply_canonization_transition`]: audit row +
    /// `Mutation::CanonizationTransition`), so the promotion is durable and
    /// visible to the §10 state machine — not a bare field flip. The goal itself
    /// is recorded as `Mutation::SetRootGoal` (XP-8), so it survives a reload.
    ///
    /// ## Accepted goal shapes ([`root_goal_texts`], ALGO-6)
    ///
    /// A bare string, **an array of strings** (spec §6.1's own example is a
    /// list), or the `{content, key}` object form. Anything else is stored but
    /// names no concept. A multi-goal session promotes all matches
    /// **id-ascending** — the previous code took the first `HashMap` match,
    /// which is iteration-order dependent and therefore nondeterministic under
    /// multiple matches (ALGO-12).
    ///
    /// The §10 state machine has no `Venerable -> Venerable` or
    /// `Canonical -> Venerable` edge, so a goal concept that is already
    /// `Venerable` or `Canonical` is left untouched (a `Canonical` root goal
    /// is strictly stronger protection); clearing the goal (`None`) stores
    /// the clear and never demotes.
    ///
    /// `occurred_at` is **logical time** — the session's newest interaction
    /// timestamp ([`Graph::logical_now`]) — not `Utc::now()`: this is otherwise
    /// a wholly logical-time write path (`record_action` takes no clock), and a
    /// wall-clock stamp made the audit trail non-monotonic against the rows
    /// around it (ALGO-12).
    pub fn set_root_goal(&mut self, goal: Option<serde_json::Value>) {
        let texts = root_goal_texts(goal.as_ref());
        if !texts.is_empty() {
            let occurred_at = self.logical_now();
            let mut matches: Vec<NodeId> = self
                .nodes
                .iter()
                .filter_map(|(id, n)| match n {
                    Node::Concept(c)
                        if texts
                            .iter()
                            .any(|t| c.content == *t || c.canonical_key == *t) =>
                    {
                        Some(*id)
                    }
                    _ => None,
                })
                .collect();
            matches.sort_by_key(|id| id.0);
            for cid in matches {
                let status = match self.nodes.get(&cid) {
                    Some(Node::Concept(c)) => c.canonization_status,
                    _ => continue,
                };
                if matches!(
                    status,
                    CanonizationStatus::None | CanonizationStatus::Candidate
                ) {
                    let event = CanonizationEvent {
                        id: NodeId::new(),
                        session_id: self.session_id.clone(),
                        node_id: cid,
                        from_status: status,
                        to_status: CanonizationStatus::Venerable,
                        blast_radius: None,
                        last_demotion_time: None,
                        occurred_at,
                    };
                    // The only rejection modes are invariant violations that
                    // cannot occur here (the concept exists; the pair is a
                    // legal §10 edge), so the promotion is best-effort.
                    let _ = self.apply_canonization_transition(event);
                }
            }
        }
        // XP-8: the goal is durable. A reload without this replayed an empty
        // goal, which silently disabled drift detection and emptied GC's
        // root-goal exclusion. The mutation also bumps the epoch, so T5.4's
        // recall cache cannot serve results computed against the old goal.
        if self.root_goal != goal {
            self.root_goal = goal.clone();
            self.append_mutation(Mutation::SetRootGoal {
                session_id: self.session_id.clone(),
                goal,
            });
        }
    }

    /// The session's logical "now": its newest interaction timestamp, falling
    /// back to the newest concept's (a session with concepts but no interactions
    /// is not constructible through the write API) and finally to the epoch
    /// origin. Write paths that need a timestamp but take no clock use this so
    /// the audit trail stays monotonic (ALGO-12).
    pub fn logical_now(&self) -> chrono::DateTime<chrono::Utc> {
        self.interactions()
            .map(|i| i.created_at)
            .chain(self.concepts().map(|c| c.created_at))
            .max()
            .unwrap_or_else(|| chrono::DateTime::from_timestamp_nanos(0))
    }

    /// Advisory soft lock (spec §11). Same-agent re-reservation extends; cross-agent
    /// denial is T2.7's policy — this stores what it is given.
    pub fn set_reservation(&mut self, r: Reservation) {
        if let Some(existing) = self
            .reservations
            .iter_mut()
            .find(|x| x.node_id == r.node_id)
        {
            *existing = r;
        } else {
            self.reservations.push(r);
        }
        // Reservations render into recall context (T2.7 soft-lock line), so a
        // transition must invalidate the epoch-keyed recall cache. Reservations
        // are RAM-local (no Mutation kind), so bump the epoch directly (P5
        // phase-close finding).
        self.epoch += 1;
    }

    pub fn clear_reservation(&mut self, node_id: NodeId) {
        self.reservations.retain(|r| r.node_id != node_id);
        self.epoch += 1;
    }

    pub fn synonyms(&self) -> impl Iterator<Item = (&str, &str)> {
        self.synonyms.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub fn synonym(&self, source_key: &str) -> Option<&str> {
        self.synonyms.get(source_key).map(|s| s.as_str())
    }

    pub fn reservation(&self, node_id: NodeId) -> Option<&Reservation> {
        self.reservations.iter().find(|r| r.node_id == node_id)
    }

    pub fn reservations(&self) -> &[Reservation] {
        &self.reservations
    }

    pub fn root_goal(&self) -> Option<&serde_json::Value> {
        self.root_goal.as_ref()
    }

    /// The concept-naming strings in the session's root goal — see
    /// [`root_goal_texts`]. The single reading of the goal shape, shared by
    /// [`Graph::set_root_goal`], drift detection and GC's exclusion list, so the
    /// three cannot disagree about what "the root goal" names (ALGO-6).
    pub fn root_goal_texts(&self) -> Vec<String> {
        root_goal_texts(self.root_goal.as_ref())
    }
}

/// The concept-naming strings in a `root_goal` value (ALGO-6).
///
/// Accepted shapes, deduplicated and sorted so every consumer sees the same list
/// in the same order:
///
/// * `"launch the product"` — a single goal.
/// * `["launch the product", "ship the API"]` — spec §6.1's own `root_goal`
///   example is a **list**, and the string-only reading silently disabled drift
///   detection, auto-`Venerable` promotion and GC's root-goal exclusion for
///   every array goal. Non-string elements are ignored rather than rejected: the
///   goal is stored either way, so a partially structured goal still anchors the
///   names it does carry.
/// * `{"content": …, "key": …}` — the object form GC already accepted; kept so
///   an existing session's exclusion list does not change meaning.
///
/// Any other shape names no concept (it is still stored — spec §6.1 types
/// `root_goal` as free-form JSON).
pub fn root_goal_texts(goal: Option<&serde_json::Value>) -> Vec<String> {
    let Some(goal) = goal else {
        return Vec::new();
    };
    let mut texts: Vec<String> = match goal {
        serde_json::Value::String(s) => vec![s.clone()],
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        serde_json::Value::Object(map) => ["content", "key"]
            .iter()
            .filter_map(|k| map.get(*k).and_then(|v| v.as_str()).map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    };
    texts.sort();
    texts.dedup();
    texts
}
