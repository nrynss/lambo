//! Contradiction detection (#19(c)): "you believe X, but something newer
//! says not-X".
//!
//! A concept is **contradicted** when both of these hold:
//!
//! 1. it has an inbound `Contradicts` edge from another concept of the
//!    session (`by -Contradicts-> node`, written by `against`, #19(b));
//! 2. that edge's **declaration date** ([`declared_at`]: the declaring
//!    write's `event_time` when one was given, else its write time, and
//!    never later than its write time) is **strictly later** than the
//!    node's latest **eligible dependent evidence**, or, with none, than the
//!    node's own origin about-time.
//!
//! Any concept can be contradicted, whatever its canonization status
//! (#179). Through 0.7.0 only `Canonical` and `Venerable` concepts were
//! ("contradicting ordinary memory is just memory"), so a stale ordinary
//! fact kept ranking first with no warning. The status still decides what
//! recall does with a contradicted hit (`crate::recall::assemble`): a
//! Canonical or Venerable one is force-included (#19), an ordinary one has
//! to earn its own slot, and either way its contradicting concept is shown
//! beside it.
//!
//! Only declared contradictions are seen: an agent that supersedes a fact
//! without `against` leaves nothing for this check to find.
//!
//! ## Dating (owner-approved, option A)
//!
//! A contradiction is dated by the write that declares it, on the
//! about-time clock, not by the contradicting concept's creation: reusing an
//! old concept for a new assertion is a new assertion. The evidence side
//! uses the same clock, so a historical ingest that backdates a
//! contradiction to 2018 and its supporting citations to 2020 reads as
//! outweighed, exactly as it happened. A **future** `event_time` is clamped
//! to the write time (review M1): it may not hide a contradiction until its
//! date and then let it outrank everything written in between. Re-sending a
//! contradiction reinforces its edge (`last_reinforced`, weight) and does
//! **not** re-date it: the edge keeps its first `event_time` and
//! `created_at`. To restate it as new, write a concept with different
//! content (the same content resolves to the same concept and the same
//! edge).
//!
//! ## Eligible dependent evidence (#36)
//!
//! The node's dependents relation ([`crate::types::Edge::dependence`]):
//! `Dependency` sources into it and its `Hierarchical` children, concepts
//! of this graph. One piece of evidence per dependence edge, dated by that
//! edge's about-time. Guards, the same as Stage 2's:
//!
//! * **age**: the edge was **written** at or before `now - min_age` (the
//!   session's `canonization_edge_min_age`); a backdated `event_time` does
//!   not skip the floor (review M3). Its date for the comparison is its
//!   [`declared_at`]. Re-sending an existing dependence reinforces the same
//!   edge and keeps its first dates, so it is not new evidence: only a new
//!   dependence edge (a concept that did not depend on the node before, or
//!   a new `parent_of` child) outweighs a contradiction.
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
//! ## Stale by dependency (#207)
//!
//! [`stale_citations`] answers "does this concept rest on a contradicted
//! one": the concepts it directly cites ([`direct_citation`]: its own
//! `Dependency` edges, from `depends_on`) for which [`contradiction_at`]
//! holds at the caller's `now`. It is the predicate itself, not a second
//! definition, and it reads the graph, not the hot list, so a write-time
//! caller (#208, reporting the concepts that cite one just contradicted)
//! gets the same answer recall shows. Direct citations only; a concept is
//! never stale on a concept it contradicts itself ([`supporting_citation`]
//! drops such a citation before anything is evaluated, so it holds whoever
//! else contradicts the cited concept).
//!
//! ## Hot list
//!
//! [`insert_contradictions`] refreshes one [`Condition::Contradiction`]
//! entry per hit; its predicate re-runs [`contradiction_at`] at the
//! caller's `now`, so recall drops the entry when newer eligible evidence
//! arrives (or ages in), or when the contradicting concept is collected
//! (a demotion no longer drops it, #179), and otherwise renders the payload
//! as of the read. Unlike conflict, it emits no `DaemonEvent`.

use std::collections::HashSet;
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};

use crate::daemon::conflict::{attribute_at, SharedTimeline, WriterTimeline};
use crate::daemon::hotlist::{Condition, HotList, HotListEntry, HotListPayload};
use crate::graph::Graph;
use crate::types::{AgentId, CanonizationStatus, Concept, Edge, EdgeType, Node, NodeId};

/// One detected contradiction: the contradicted node and the payload fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContradictionHit {
    /// The contradicted node.
    pub node: NodeId,
    /// The contradicting concept.
    pub by: NodeId,
    /// The agent that declared the contradiction.
    pub writer: AgentId,
    /// The declaring write's about-time.
    pub at: DateTime<Utc>,
}

/// When an edge's declaration counts as made: its about-time
/// ([`Edge::about_time`]), clamped to its write time (#19(c) review M1).
///
/// The clamp only moves a **future** `event_time` back to when the edge was
/// written. A contradiction (or a citation) dated 30 days ahead would
/// otherwise stay invisible for 30 days and then outrank everything written
/// in between; a backdated one keeps its earlier date, which is the point of
/// about-time (owner-approved dating, option A). The same date drives the
/// detector, recall and GC's protection (`daemon::gc`).
pub(crate) fn declared_at(e: &Edge) -> DateTime<Utc> {
    e.about_time().min(e.created_at)
}

/// Whether `e` had been written at `now`: its write time, not its
/// about-time, is what says a declaration exists yet. `declared_at <= now`
/// alone let an edge written after `now` with an earlier `event_time` count
/// (PR #154 review). Shared with GC's protection.
pub(crate) fn written_by(e: &Edge, now: DateTime<Utc>) -> bool {
    e.created_at <= now
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
    concept(graph, node)?;
    if !has_inbound_contradiction(graph, node) {
        return None;
    }
    contradiction_at_with(graph, node, min_age, now, &WriterTimeline::of(graph))
}

/// Whether recall force-includes `c` when it is contradicted (#19): a
/// Canonical or Venerable concept is a belief the reader should be warned
/// about whatever its rank. An ordinary contradicted concept is shown only
/// when it earns its slot (#179).
pub fn forces_inclusion(c: &Concept) -> bool {
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

/// Whether `a` and `b` are joined by a `Contradicts` edge in either
/// direction: a declared disagreement, whatever its date or the
/// detector's verdict. The pair test a redundancy or merge check needs
/// (#178's suggestion guard, #181): two claims that contradict each other
/// are never the same memory. `O(degree of a)`.
pub fn contradicts_either_way(graph: &Graph, a: NodeId, b: NodeId) -> bool {
    a != b
        && (graph.edge_between(a, b, EdgeType::Contradicts).is_some()
            || graph.edge_between(b, a, EdgeType::Contradicts).is_some())
}

fn contradiction_at_with(
    graph: &Graph,
    node: NodeId,
    min_age: Duration,
    now: DateTime<Utc>,
    timeline: &WriterTimeline,
) -> Option<ContradictionHit> {
    let target = concept(graph, node)?;
    // Evidence written at or before this instant is old enough to count.
    // `None` when `now - min_age` is out of chrono's range (review M5, as
    // Stage 1's `age_cutoff`): then no evidence is eligible.
    let cutoff = ChronoDuration::from_std(min_age)
        .ok()
        .and_then(|age| now.checked_sub_signed(age));
    // With no eligible evidence, the node's own origin about-time (clamped
    // like every date here) is the baseline a contradiction must be newer
    // than.
    let baseline = match graph.node(target.origin_interaction) {
        Some(Node::Interaction(i)) => i.about_time().min(i.created_at),
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
        // Eligibility is by write time (review M3): a citation backdated by
        // its event_time does not skip the age floor. Its date for the
        // comparison is still its (clamped) about-time.
        if cutoff.is_none_or(|cutoff| e.created_at > cutoff) {
            continue;
        }
        let about = declared_at(e);
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
        if !written_by(e, now) {
            continue; // not written yet at the instant examined (a mocked clock)
        }
        let at = declared_at(e);
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
        .filter(|&t| concept(graph, t).is_some())
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

/// A **direct citation** (#207): `e` is a `Dependency` edge from one concept
/// of the graph to another concept of the same session, written by
/// `depends_on` (the citing concept is the edge's source). Returns
/// `(citer, cited)`.
///
/// Only `Dependency` counts: a `Hierarchical` child, a `Contradicts` edge
/// and any other type (#198's `SuspectedSupersedes` among them) is not a
/// citation. The one edge test shared by [`stale_citations`] and its
/// inverse (#208's "who cites the concept just contradicted"), so the two
/// always agree on what a citation is.
pub fn direct_citation(graph: &Graph, e: &Edge) -> Option<(NodeId, NodeId)> {
    if e.edge_type != EdgeType::Dependency || e.source == e.target {
        return None;
    }
    let citer = concept(graph, e.source)?;
    let cited = concept(graph, e.target)?;
    (citer.session_id == cited.session_id).then_some((citer.id, cited.id))
}

/// A **supporting citation** (#207): `e` is a [`direct_citation`] written
/// by `now` whose citing concept does not itself contradict the cited one
/// (no `Contradicts` edge citer -> cited written by `now`). Returns
/// `(citer, cited)`.
///
/// A concept that cites what it contradicts does not rest on it, so such a
/// citation is never a stale one, whoever else contradicts the cited
/// concept and whichever contradictor [`contradiction_at`] names. This is
/// the edge test [`stale_citations`] applies, and the one its inverse
/// (#208's "who cites the concept just contradicted") must apply, so both
/// directions exclude a self-contradicting citer the same way.
pub fn supporting_citation(
    graph: &Graph,
    e: &Edge,
    now: DateTime<Utc>,
) -> Option<(NodeId, NodeId)> {
    if !written_by(e, now) {
        return None;
    }
    let (citer, cited) = direct_citation(graph, e)?;
    let contradicts_it = graph
        .edge_between(citer, cited, EdgeType::Contradicts)
        .is_some_and(|c| written_by(c, now));
    (!contradicts_it).then_some((citer, cited))
}

/// **Stale by dependency** (#207): the concepts `node` directly cites
/// ([`supporting_citation`]: written by `now`, and not contradicted by
/// `node` itself) that are contradicted at `now`,
/// each as the [`ContradictionHit`] [`contradiction_at`] gives it (its
/// `node` is the cited concept, `by` its contradicting concept), in
/// node-id order of the cited concept.
///
/// The contradicted test is exactly [`contradiction_at`]'s, at the same
/// `min_age` and `now`: no second definition. Direct citations only, so a
/// chain (`node` cites A, A cites a contradicted B) flags nothing. A
/// concept is never warned that it rests on what it contradicts: a cited
/// concept `node` has a `Contradicts` edge to is dropped before it is
/// evaluated, even when a newer contradictor than `node` is the one
/// [`contradiction_at`] would name.
///
/// `O(degree + interactions)`, the timeline built only when a cited
/// concept has an inbound `Contradicts` edge. Empty when `node` is not a
/// concept of the graph.
pub fn stale_citations(
    graph: &Graph,
    node: NodeId,
    min_age: Duration,
    now: DateTime<Utc>,
) -> Vec<ContradictionHit> {
    stale_citations_shared(graph, node, min_age, now, &SharedTimeline::new())
}

/// [`stale_citations`] against a shared, epoch-keyed timeline, so a recall
/// checking many hits builds it at most once.
pub(crate) fn stale_citations_shared(
    graph: &Graph,
    node: NodeId,
    min_age: Duration,
    now: DateTime<Utc>,
    shared: &SharedTimeline,
) -> Vec<ContradictionHit> {
    if concept(graph, node).is_none() {
        return Vec::new();
    }
    let mut cited: Vec<NodeId> = graph
        .incident_edges(node)
        .into_iter()
        .filter_map(|e| supporting_citation(graph, e, now))
        .filter(|(citer, _)| *citer == node)
        .map(|(_, cited)| cited)
        .filter(|&cited| has_inbound_contradiction(graph, cited))
        .collect();
    if cited.is_empty() {
        return Vec::new();
    }
    cited.sort_by_key(|id| id.0);
    cited.dedup();
    cited
        .into_iter()
        .filter_map(|cited| contradiction_at_with(graph, cited, min_age, now, &shared.get(graph)))
        .collect()
}

/// [`contradiction_at`] against a shared, epoch-keyed timeline: the hot-list
/// predicate's path (review M2).
fn contradiction_at_shared(
    graph: &Graph,
    node: NodeId,
    min_age: Duration,
    now: DateTime<Utc>,
    shared: &SharedTimeline,
) -> Option<ContradictionHit> {
    concept(graph, node)?;
    if !has_inbound_contradiction(graph, node) {
        return None;
    }
    contradiction_at_with(graph, node, min_age, now, &shared.get(graph))
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
    // One timeline for every entry this pass inserts, rebuilt only when the
    // graph's epoch moves, so a recall re-validating many contradicted
    // members under the hot-list write guard builds it at most once.
    let shared = SharedTimeline::new();
    // One sort for the whole pass (#179: every contradiction is an entry).
    // Oldest-declared first (#196): within the tier the later insert ranks
    // higher, so a full contradiction budget keeps the most recently
    // declared contradictions, node id breaking ties.
    let mut order: Vec<&ContradictionHit> = hits.iter().collect();
    order.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.node.0.cmp(&b.node.0)));
    let _ = hot.insert_many(order.into_iter().map(|hit| {
        let node = hit.node;
        let shared = shared.clone();
        let holds = move |g: &Graph, at: DateTime<Utc>| {
            contradiction_at_shared(g, node, min_age, at, &shared).map(payload_of)
        };
        HotListEntry::new(
            node,
            Condition::Contradiction,
            payload_of(hit.clone()),
            holds,
        )
    }));
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
