//! Contradiction detection (#19(c)): "you believe X, but something newer
//! says not-X".
//!
//! A concept is **contradicted** when all of these hold:
//!
//! 1. it is `Canonical` or `Venerable` (contradicting ordinary memory is
//!    just memory);
//! 2. it has an inbound `Contradicts` edge from another concept of the
//!    session (`by -Contradicts-> node`, written by `against`, #19(b));
//! 3. that edge's **about-time** ([`Edge::about_time`]: the declaring write's
//!    `event_time` when one was given, else its write time) is not after
//!    `now`, and is **strictly later** than the node's latest **eligible
//!    dependent evidence**, or, with none, than the node's own origin
//!    about-time.
//!
//! ## Dating (owner-approved, option A)
//!
//! A contradiction is dated by the write that declares it, on the
//! about-time clock, not by the contradicting concept's creation: reusing an
//! old concept for a new assertion is a new assertion. The evidence side
//! uses the same clock, so a historical ingest that backdates a
//! contradiction to 2018 and its supporting citations to 2020 reads as
//! outweighed, exactly as it happened. Re-sending a contradiction reinforces
//! its edge (`last_reinforced`, weight) and does **not** re-date it: the
//! edge keeps its first `event_time` and `created_at`. To restate a
//! contradiction as new, write a new assertion.
//!
//! ## Eligible dependent evidence (#36)
//!
//! The node's dependents relation ([`crate::types::Edge::dependence`]):
//! `Dependency` sources into it and its `Hierarchical` children, concepts
//! of this graph. One piece of evidence per dependence edge, dated by that
//! edge's about-time. Guards, the same as Stage 2's:
//!
//! * **age**: the edge's about-time is at or before `now - min_age` (the
//!   session's `canonization_edge_min_age`). Evidence younger than that, or
//!   dated in the future, does not count yet.
//! * **own origin**: a dependent written by the node's own origin
//!   interaction never supports it.
//! * **self-suppression**: evidence never counts against the contradiction
//!   it would silence when it was written by the same agent that declared
//!   the contradiction, or when the dependent is the contradicting concept
//!   itself. #19(b) refuses citing and contradicting one concept in one
//!   call, but separate writes can do both; a writer cannot outweigh its own
//!   contradiction by also citing the target.
//!
//! `Contradicts` itself is never evidence (it has no dependent end), and
//! neither are `Causal`, `Derives`, `CoOccurrence` or `Semantic` edges.
//!
//! ## Several contradictions
//!
//! The hot list keeps one entry per `(node, condition)`, so a node carries
//! one payload: the qualifying contradiction with the newest about-time,
//! then the smallest canonical key of `by`, then the smallest id. Others
//! remain visible through `lambo_inspect`.
//!
//! ## Writer
//!
//! The agent acting when the edge was first written
//! ([`crate::daemon::conflict`]'s write-time attribution at `created_at`),
//! falling back to `by`'s origin agent. The same attribution decides
//! self-suppression; when one instant has several candidate agents, evidence
//! sharing any of them is excluded (erring toward the warning).
//!
//! ## Hot list
//!
//! [`insert_contradictions`] refreshes one [`Condition::Contradiction`]
//! entry per hit; its predicate re-runs [`contradiction_at`] at the
//! caller's `now`, so recall drops the entry when the node loses its
//! status, when newer eligible evidence arrives (or ages in), or when the
//! contradicting concept is collected, and otherwise renders the payload
//! as of the read. Unlike conflict, it emits no `DaemonEvent`.

use std::collections::HashSet;
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};

use crate::daemon::conflict::{attribute_at, WriterTimeline};
use crate::daemon::hotlist::{Condition, HotList, HotListEntry, HotListPayload};
use crate::graph::Graph;
use crate::types::{AgentId, CanonizationStatus, Concept, EdgeType, Node, NodeId};

/// One detected contradiction: the contradicted node and the payload fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContradictionHit {
    /// The contradicted (Canonical or Venerable) node.
    pub node: NodeId,
    /// The contradicting concept.
    pub by: NodeId,
    /// The agent that declared the contradiction.
    pub writer: AgentId,
    /// The declaring write's about-time.
    pub at: DateTime<Utc>,
}

fn concept(graph: &Graph, id: NodeId) -> Option<&Concept> {
    match graph.node(id) {
        Some(Node::Concept(c)) => Some(c),
        _ => None,
    }
}

/// Pure check for one node, the per-node primitive shared by [`detect`] and
/// the hot-list predicate. `O(degree + interactions)`: the writer timeline
/// is built once per call, as [`crate::daemon::conflict::conflict_at`] does.
pub fn contradiction_at(
    graph: &Graph,
    node: NodeId,
    min_age: Duration,
    now: DateTime<Utc>,
) -> Option<ContradictionHit> {
    // Cheap exits before the timeline is built.
    let target = concept(graph, node)?;
    if !promoted(target) || !has_inbound_contradiction(graph, node) {
        return None;
    }
    contradiction_at_with(graph, node, min_age, now, &WriterTimeline::of(graph))
}

fn promoted(c: &Concept) -> bool {
    matches!(
        c.canonization_status,
        CanonizationStatus::Canonical | CanonizationStatus::Venerable
    )
}

fn has_inbound_contradiction(graph: &Graph, node: NodeId) -> bool {
    graph
        .in_neighbors_typed(node, EdgeType::Contradicts)
        .into_iter()
        .any(|s| s != node)
}

fn contradiction_at_with(
    graph: &Graph,
    node: NodeId,
    min_age: Duration,
    now: DateTime<Utc>,
    timeline: &WriterTimeline,
) -> Option<ContradictionHit> {
    let target = concept(graph, node)?;
    if !promoted(target) {
        return None;
    }
    let cutoff = now - ChronoDuration::from_std(min_age).unwrap_or(ChronoDuration::MAX);
    // With no eligible evidence, the node's own origin about-time is the
    // baseline a contradiction must be newer than.
    let baseline = match graph.node(target.origin_interaction) {
        Some(Node::Interaction(i)) => i.about_time(),
        _ => target.created_at,
    };

    // Eligible evidence: (about-time, dependent, writers).
    let mut evidence: Vec<(DateTime<Utc>, NodeId, HashSet<AgentId>)> = Vec::new();
    for e in graph.incident_edges(node) {
        let Some((pillar, dependent)) = e.dependence() else {
            continue;
        };
        if pillar != node || dependent == node {
            continue;
        }
        let Some(dep) = concept(graph, dependent) else {
            continue;
        };
        if dep.session_id != target.session_id
            || dep.origin_interaction == target.origin_interaction
        {
            continue;
        }
        let about = e.about_time();
        if about > cutoff {
            continue;
        }
        let writers = attribute_at(graph, e, e.created_at, timeline)
            .map(|w| w.all().cloned().collect())
            .unwrap_or_default();
        evidence.push((about, dependent, writers));
    }

    let mut best: Option<(DateTime<Utc>, &Concept, AgentId)> = None;
    for e in graph.incident_edges(node) {
        if e.edge_type != EdgeType::Contradicts || e.target != node || e.source == node {
            continue;
        }
        let Some(by) = concept(graph, e.source) else {
            continue;
        };
        // Never across sessions (#19(b) refuses such a write; this keeps a
        // hand-built graph from leaking one into recall either).
        if by.session_id != target.session_id {
            continue;
        }
        let at = e.about_time();
        if at > now {
            continue; // not declared yet at the instant examined
        }
        let Some(writers) = attribute_at(graph, e, e.created_at, timeline) else {
            continue;
        };
        let declarers: HashSet<&AgentId> = writers.all().collect();
        let latest = evidence
            .iter()
            .filter(|(_, dependent, ev_writers)| {
                *dependent != by.id && !ev_writers.iter().any(|w| declarers.contains(w))
            })
            .map(|(about, _, _)| *about)
            .fold(baseline, DateTime::max);
        if at <= latest {
            continue;
        }
        let better = match &best {
            None => true,
            Some((best_at, best_by, _)) => at
                .cmp(best_at)
                .then_with(|| best_by.canonical_key.cmp(&by.canonical_key))
                .then_with(|| best_by.id.0.cmp(&by.id.0))
                .is_gt(),
        };
        if better {
            best = Some((at, by, writers.named().clone()));
        }
    }
    best.map(|(at, by, writer)| ContradictionHit {
        node,
        by: by.id,
        writer,
        at,
    })
}

/// Detect every contradicted node (see the module docs). Pure;
/// deterministic, in node-id order. Only targets of a `Contradicts` edge are
/// examined, so a graph without contradictions costs one pass over its
/// edges.
pub fn detect(graph: &Graph, min_age: Duration, now: DateTime<Utc>) -> Vec<ContradictionHit> {
    let mut targets: Vec<NodeId> = graph
        .edges()
        .filter(|e| e.edge_type == EdgeType::Contradicts && e.source != e.target)
        .map(|e| e.target)
        .filter(|&t| concept(graph, t).is_some_and(promoted))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    if targets.is_empty() {
        return Vec::new();
    }
    targets.sort_by_key(|id| id.0);
    let timeline = WriterTimeline::of(graph);
    targets
        .into_iter()
        .filter_map(|t| contradiction_at_with(graph, t, min_age, now, &timeline))
        .collect()
}

/// Detect contradictions and refresh the hot list with one
/// [`Condition::Contradiction`] entry per hit. The predicate re-runs
/// [`contradiction_at`] at the caller's `now` (XP-3), capturing only `node`
/// and `min_age`. Returns the hits, which the daemon loop adds to the
/// cycle's condition set so [`HotList::retain_conditions`] drops an entry
/// whose contradiction stopped holding.
pub fn insert_contradictions(
    hot: &mut HotList,
    graph: &Graph,
    min_age: Duration,
    now: DateTime<Utc>,
) -> Vec<ContradictionHit> {
    let hits = detect(graph, min_age, now);
    for hit in &hits {
        let node = hit.node;
        let holds = move |g: &Graph, at: DateTime<Utc>| {
            contradiction_at(g, node, min_age, at).map(payload_of)
        };
        let _ = hot.insert(HotListEntry::new(
            node,
            Condition::Contradiction,
            payload_of(hit.clone()),
            holds,
        ));
    }
    hits
}

fn payload_of(hit: ContradictionHit) -> HotListPayload {
    HotListPayload::Contradiction {
        by: hit.by,
        writer: hit.writer,
        at: hit.at,
    }
}

#[cfg(test)]
mod tests;
