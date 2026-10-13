//! Evidence-driven supersession (#20): newer dependent evidence for a
//! contradicting concept demotes a Canonical one.
//!
//! Off by default (`Config::canonization_supersede`). When on, a Canonical
//! concept `C` is **superseded** by `S` when all of these hold at the
//! cycle's `now`:
//!
//! 1. `S -Contradicts-> C` exists (#19), written by `now`, with `S` another
//!    concept of the session.
//! 2. `S`'s eligible dependent evidence **after** `C`'s watermark clears
//!    Stage 2's bars: at least three origin interactions other than `S`'s
//!    own, covering at least 0.3 of min(session extent, the coverage cap)
//!    ([`crate::store::GraphStore::interaction_span_since`]). The watermark
//!    is the later of `C`'s origin about-time and its latest eligible
//!    dependent evidence ([`crate::store::GraphStore::latest_structural_evidence_at`]),
//!    excluding, as #19(c) does, evidence written by the contradiction's own
//!    declarer and the dependence of `S` itself on `C`: a writer cannot
//!    defend a belief against its own contradiction. Evidence and dates are
//!    #19(c)'s: eligible by write time against the age floor, dated by
//!    about-time clamped to write time. An assertion-only contradiction (no
//!    newer structural support for `S`) never demotes anything.
//! 3. Neither side is inside its re-promotion cooldown: a source demoted
//!    within `canonization_repromotion_cooldown` cannot supersede, and a
//!    target demoted within it cannot be superseded again. With the
//!    cooldown, two concepts contradicting each other cannot trade places
//!    more than once per window.
//!
//! `C` goes `Canonical -> None` with `last_demotion_time = now` and
//! `reason = Superseded { by: S }`. `S` is **not** promoted: it climbs on its
//! own merits. When several sources qualify for one target, one wins: the
//! newest eligible evidence, then the smallest canonical key, then the
//! smallest id.
//!
//! ## The hold
//!
//! With supersession on, Stage 3 also does not promote a Venerable that a
//! contradicting concept would supersede at once ([`held`]), probed for
//! Stage 3's window with the same predicate. A superseded concept whose own
//! evidence still passes every stage would otherwise climb back once its
//! cooldown is over and be superseded again, once per window, forever. The
//! steady state is instead the one the issue names: the side with the newer
//! evidence wins, and the other waits at Venerable until it earns newer
//! evidence of its own.
//!
//! ## Bounds and concurrency
//!
//! At most `batch_size` targets are probed per cycle, walking a ring of
//! targets by identity like Stage 3's. Probes run with no graph lock, so
//! [`still_holds`] re-checks everything at apply time under the write
//! guard, from the graph: statuses, sessions, both cooldowns, the edge, and
//! the evidence itself (no newer evidence for `C` than the watermark the
//! verdict used, and `S`'s support still passing after it), read through
//! [`crate::store::evidence::rows_from_graph`]. The hybrid derive's
//! `plan_still_holds` is no validator here: its relevant-node ring ignores
//! edge-only and status-only changes.
//!
//! A store without the two accessors (`StoreError::Capability`, their trait
//! default) skips supersession for the whole cycle with one warning; the
//! rest of the cycle runs. Any other store error keeps the evaluator's
//! established semantics (Stage 1 alone lands, the cycle fails).

use std::collections::HashSet;

use chrono::{DateTime, Utc};

use super::eval::EvalParams;
use super::stage3::in_repromotion_cooldown;
use crate::canon::stage2_evidence_passes;
use crate::daemon::conflict::{attribute_at, WriterTimeline};
use crate::daemon::contradiction::written_by;
use crate::graph::Graph;
use crate::store::evidence::{latest_at, rows_from_graph, span_since};
use crate::store::GraphStore;
use crate::types::{
    tie_break_by_key, AgentId, CanonizationEvent, CanonizationStatus, Concept, DemotionReason,
    EdgeType, Node, NodeId, SessionId, StoreError,
};

/// One `(target, source)` pair to probe, read under the gather guard.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Probe {
    pub target: NodeId,
    pub source: NodeId,
    pub source_key: String,
    /// The `Contradicts` edge.
    pub edge: NodeId,
    /// The target's origin about-time (clamped): the watermark when it has
    /// no eligible evidence.
    pub baseline: DateTime<Utc>,
    /// Dependence edges into the target that do not count against this
    /// contradiction (#19(c) self-suppression).
    pub exclude: Vec<NodeId>,
}

/// A passing probe: the source that wins its target.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Verdict {
    pub target: NodeId,
    pub source: NodeId,
    pub edge: NodeId,
    /// The watermark the store measured `S`'s support after.
    pub watermark: DateTime<Utc>,
}

fn concept(graph: &Graph, id: NodeId) -> Option<&Concept> {
    match graph.node(id) {
        Some(Node::Concept(c)) => Some(c),
        _ => None,
    }
}

/// The target's origin about-time, clamped to its write time; the concept's
/// own creation when the origin is missing (#19(c)'s baseline).
fn baseline(graph: &Graph, target: &Concept) -> DateTime<Utc> {
    match graph.node(target.origin_interaction) {
        Some(Node::Interaction(i)) => i.about_time().min(i.created_at),
        _ => target.created_at,
    }
}

/// The dependence edges into `target` that do not count against the
/// contradiction `edge` declares: the ones written by its declarer, and the
/// dependence of the contradicting concept itself. `None` when the
/// contradiction has no attributable writer (it is skipped, as #19(c) skips
/// it).
fn self_suppressed(
    graph: &Graph,
    target: NodeId,
    edge: &crate::types::Edge,
    timeline: &WriterTimeline,
) -> Option<Vec<NodeId>> {
    let declarers: HashSet<AgentId> = attribute_at(graph, edge, edge.created_at, timeline)?
        .all()
        .cloned()
        .collect();
    let mut out = Vec::new();
    for e in graph.incident_edges(target) {
        let Some((pillar, dependent)) = e.dependence() else {
            continue;
        };
        if pillar != target {
            continue;
        }
        let by_declarer = attribute_at(graph, e, e.created_at, timeline)
            .is_some_and(|w| w.all().any(|a| declarers.contains(a)));
        if dependent == edge.source || by_declarer {
            out.push(e.id);
        }
    }
    out.sort_by_key(|id| id.0);
    Some(out)
}

/// Whether `id` is a concept of `session` outside its cooldown.
fn usable<'g>(
    graph: &'g Graph,
    id: NodeId,
    session: &SessionId,
    params: &EvalParams,
    now: DateTime<Utc>,
) -> Option<&'g Concept> {
    concept(graph, id).filter(|c| {
        &c.session_id == session
            && !in_repromotion_cooldown(c.last_demotion_time, params.cooldown, now)
    })
}

/// The live contradictions of `target` that may supersede it: `(edge, source)`
/// with the source a usable concept of the session.
fn contradictions<'g>(
    graph: &'g Graph,
    target: &Concept,
    params: &EvalParams,
    now: DateTime<Utc>,
) -> Vec<(&'g crate::types::Edge, &'g Concept)> {
    let mut out: Vec<_> = graph
        .incident_edges(target.id)
        .into_iter()
        .filter(|e| {
            e.edge_type == EdgeType::Contradicts
                && e.target == target.id
                && e.source != target.id
                && written_by(e, now)
        })
        .filter_map(|e| usable(graph, e.source, &target.session_id, params, now).map(|s| (e, s)))
        .collect();
    out.sort_by_key(|(e, _)| e.id.0);
    out
}

/// Gather: the bounded probe set, under the read guard. Advances `cursor`
/// over the ring of eligible targets.
pub(super) fn gather(
    graph: &Graph,
    params: &EvalParams,
    now: DateTime<Utc>,
    cursor: &mut Option<NodeId>,
) -> Vec<Probe> {
    if !params.supersede {
        return Vec::new();
    }
    let session = graph.session_id();
    let mut ring: Vec<NodeId> = graph
        .concepts()
        .filter(|c| c.canonization_status == CanonizationStatus::Canonical)
        .filter(|c| usable(graph, c.id, session, params, now).is_some())
        .filter(|c| !contradictions(graph, c, params, now).is_empty())
        .map(|c| c.id)
        .collect();
    if ring.is_empty() {
        return Vec::new();
    }
    ring.sort_by_key(|id| id.0);
    let window = super::eval::ring_window(&ring, *cursor, params.batch_size);
    if let Some(&last) = window.last() {
        *cursor = Some(last);
    }
    probes_for(graph, &window, params, now)
}

/// The hold probes for Stage 3's window (see [`hold`]): the same probes,
/// with Venerable targets. Empty unless supersession is on.
pub(super) fn gather_holds(
    graph: &Graph,
    window: &[NodeId],
    params: &EvalParams,
    now: DateTime<Utc>,
) -> Vec<Probe> {
    if !params.supersede {
        return Vec::new();
    }
    let session = graph.session_id();
    let targets: Vec<NodeId> = window
        .iter()
        .copied()
        .filter(|&id| usable(graph, id, session, params, now).is_some())
        .collect();
    probes_for(graph, &targets, params, now)
}

fn probes_for(
    graph: &Graph,
    targets: &[NodeId],
    params: &EvalParams,
    now: DateTime<Utc>,
) -> Vec<Probe> {
    if targets.is_empty() {
        return Vec::new();
    }
    let timeline = WriterTimeline::of(graph);
    let mut probes = Vec::new();
    for &target_id in targets {
        let Some(target) = concept(graph, target_id) else {
            continue;
        };
        for (edge, source) in contradictions(graph, target, params, now) {
            let Some(exclude) = self_suppressed(graph, target_id, edge, &timeline) else {
                continue;
            };
            probes.push(Probe {
                target: target_id,
                source: source.id,
                source_key: source.canonical_key.clone(),
                edge: edge.id,
                baseline: baseline(graph, target),
                exclude,
            });
        }
    }
    probes
}

/// Verdicts: the store's answers for every probe, with no lock held, and one
/// winning source per target. `Ok(vec![])` with one warning when the store
/// lacks either accessor.
pub(super) async fn verdicts(
    store: &dyn GraphStore,
    session: &SessionId,
    probes: &[Probe],
    params: &EvalParams,
    now: DateTime<Utc>,
) -> Result<Vec<Verdict>, StoreError> {
    match probe_all(store, session, probes, params, now).await {
        Err(StoreError::Capability(capability)) => {
            tracing::warn!(
                target: "lambo::canon",
                capability = %capability,
                "CanonizationSupersessionUnsupported: canonization_supersede is on but the \
                 store does not implement the supersession evidence accessors, so no \
                 concept is superseded this cycle; the rest of the cycle ran"
            );
            Ok(Vec::new())
        }
        other => other,
    }
}

async fn probe_all(
    store: &dyn GraphStore,
    session: &SessionId,
    probes: &[Probe],
    params: &EvalParams,
    now: DateTime<Utc>,
) -> Result<Vec<Verdict>, StoreError> {
    // (target, newest, source key, verdict) for every passing probe.
    let mut passing: Vec<(DateTime<Utc>, &Probe, Verdict)> = Vec::new();
    for probe in probes {
        let latest = store
            .latest_structural_evidence_at(
                session,
                probe.target,
                params.min_age,
                now,
                &probe.exclude,
            )
            .await?;
        let watermark = latest.map_or(probe.baseline, |at| at.max(probe.baseline));
        let span = store
            .interaction_span_since(
                session,
                probe.source,
                params.min_age,
                now,
                params.coverage_window,
                watermark,
            )
            .await?;
        if !stage2_evidence_passes(span) {
            continue;
        }
        // Rank by the source's newest eligible evidence. It passed, so it
        // has some after the watermark.
        let newest = store
            .latest_structural_evidence_at(session, probe.source, params.min_age, now, &[])
            .await?
            .unwrap_or(watermark);
        passing.push((
            newest,
            probe,
            Verdict {
                target: probe.target,
                source: probe.source,
                edge: probe.edge,
                watermark,
            },
        ));
    }
    passing.sort_by(|(an, ap, _), (bn, bp, _)| {
        ap.target
            .0
            .cmp(&bp.target.0)
            .then_with(|| bn.cmp(an))
            .then_with(|| {
                tie_break_by_key(
                    Some(&ap.source_key),
                    &ap.source,
                    Some(&bp.source_key),
                    &bp.source,
                )
            })
    });
    let mut out: Vec<Verdict> = Vec::new();
    for (_, _, verdict) in passing {
        if out.last().is_some_and(|v| v.target == verdict.target) {
            continue;
        }
        out.push(verdict);
    }
    Ok(out)
}

/// The hold (#20): with supersession on, Stage 3 does not promote a
/// Venerable that a contradicting concept would supersede at once, under the
/// same predicate. Without it a superseded concept whose own evidence still
/// passes would climb back after its cooldown and be superseded again,
/// once per window, forever; with it the side with the newer evidence wins
/// and the other waits at Venerable until it earns newer evidence. The
/// targets the verdicts name are held for this cycle.
pub(super) fn held(verdicts: &[Verdict]) -> Vec<NodeId> {
    verdicts.iter().map(|v| v.target).collect()
}

/// Apply-time revalidation under the write guard (see the module docs).
pub(super) fn still_holds(
    graph: &Graph,
    verdict: &Verdict,
    params: &EvalParams,
    now: DateTime<Utc>,
    timeline: &WriterTimeline,
) -> bool {
    let session = graph.session_id();
    let Some(target) = usable(graph, verdict.target, session, params, now) else {
        return false;
    };
    if target.canonization_status != CanonizationStatus::Canonical
        || usable(graph, verdict.source, session, params, now).is_none()
    {
        return false;
    }
    let Some(edge) = graph.edge(verdict.edge) else {
        return false;
    };
    if edge.edge_type != EdgeType::Contradicts
        || edge.source != verdict.source
        || edge.target != verdict.target
        || !written_by(edge, now)
    {
        return false;
    }
    let Some(exclude) = self_suppressed(graph, verdict.target, edge, timeline) else {
        return false;
    };
    let base = baseline(graph, target);
    let watermark = latest_at(
        &rows_from_graph(graph, verdict.target),
        params.min_age,
        now,
        &exclude,
    )
    .map_or(base, |at| at.max(base));
    if watermark > verdict.watermark {
        // Newer evidence for the target arrived while the store was probed.
        return false;
    }
    stage2_evidence_passes(span_since(
        &rows_from_graph(graph, verdict.source),
        params.min_age,
        now,
        params.coverage_window,
        verdict.watermark,
    ))
}

/// The demotion event for a verdict that still holds.
pub(super) fn event(graph: &Graph, verdict: &Verdict, now: DateTime<Utc>) -> CanonizationEvent {
    CanonizationEvent {
        id: NodeId::new(),
        session_id: graph.session_id().clone(),
        node_id: verdict.target,
        from_status: CanonizationStatus::Canonical,
        to_status: CanonizationStatus::None,
        blast_radius: None,
        last_demotion_time: Some(now),
        occurred_at: now,
        reason: Some(DemotionReason::Superseded { by: verdict.source }),
    }
}
