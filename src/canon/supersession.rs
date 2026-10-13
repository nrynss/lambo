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
//!    ([`DependentEvidence::interaction_span_since`]). The watermark is the
//!    later of `C`'s origin about-time and its latest eligible dependent
//!    evidence ([`DependentEvidence::latest_structural_evidence_at`]),
//!    excluding, as #19(c) does, evidence certainly written by the
//!    contradiction's own declarer and the dependence of `S` itself on `C`:
//!    a writer cannot defend a belief against its own contradiction. When
//!    who wrote the contradiction, or a piece of `C`'s evidence that might be
//!    the declarer's, cannot be told apart (several agents acting at one
//!    instant), the pair is not probed at all (review L4): an ambiguity
//!    never demotes. Evidence and dates are #19(c)'s: eligible by write time
//!    against the age floor, dated by about-time clamped to write time. An
//!    assertion-only contradiction (no newer structural support for `S`)
//!    never demotes anything.
//! 3. Neither side is inside its re-promotion cooldown: a source demoted
//!    within `canonization_repromotion_cooldown` cannot supersede, and a
//!    target demoted within it cannot be superseded again.
//!
//! The bar is **asymmetric** on purpose (review L3): `S` must clear all of
//! Stage 2 again with evidence newer than `C`'s, while one new eligible
//! piece of evidence for `C` (from anyone but the declarer) moves the
//! watermark past `S`'s support and protects `C` until `S` earns three more
//! origins after it. A belief is overturned only by a body of newer support.
//!
//! `C` goes `Canonical -> None` with `last_demotion_time = now` and
//! `reason = Superseded { by: S }`. `S` is **not** promoted: it climbs on its
//! own merits. When several sources qualify for one target, one wins: the
//! newest **counted** evidence (the newest piece the span after the
//! watermark counted, review L2), then the smallest canonical key, then the
//! smallest id.
//!
//! ## The pair rule (review H1, M1)
//!
//! Two concepts can each qualify to supersede the other (each one's support
//! is newer than the other's watermark, which self-suppression makes
//! possible). Then only one of them may act on the other: the one whose
//! counted evidence is newest, then the smaller canonical key, then the
//! smaller id. The rule is applied to the verdicts before anything commits,
//! so the result never depends on the order targets are applied in; and the
//! apply loop judges every verdict's cooldowns on the state before the
//! cycle, so a concept demoted earlier in the loop keeps what it could do
//! (chains are order-independent).
//!
//! ## The hold (owner-approved, 2026-10-13)
//!
//! With supersession on, Stage 3 also does not promote a Venerable that a
//! contradicting concept would supersede at once, under the same predicate
//! and the pair rule. That covers a superseded concept climbing back, and
//! also a Venerable that was never Canonical (review L9): promoting it would
//! only demote it on the next cycle. Without the hold a superseded concept
//! whose own evidence still passes would climb back once its cooldown is
//! over and be superseded again, once per window, forever. With it the side
//! with the newer evidence wins, and the other waits at Venerable until it
//! earns newer evidence of its own. Inspect shows the hold as a failing
//! check (`held: contradicted by <id> with newer evidence`).
//!
//! **Single writer (review H1, scenario A).** Evidence the declarer itself
//! writes never counts for the contradicted concept, so if every write comes
//! from one agent id (one writer, or several agents sharing an id), nothing
//! that writer adds for `C` can ever release `C` from a contradiction it
//! declared: only another agent's evidence can. Give agents distinct ids
//! before enabling supersession.
//!
//! ## Bounds and concurrency
//!
//! At most `batch_size` Canonical targets are probed per cycle, walking a
//! ring by identity like Stage 3's, and at most
//! [`MAX_SUPERSESSION_PROBES`] forward probes in all (supersession and hold
//! together, each with at most one reverse probe for the pair rule); a cycle
//! that hits the cap logs `CanonizationSupersessionProbeCap` and probes the
//! rest on later cycles. Each node's evidence rows are read from the store
//! once per cycle ([`EvidenceCache`]). Probes run with no graph lock, so
//! [`still_holds`] re-checks every verdict at apply time under the write
//! guard, from the graph: statuses, sessions, both cooldowns (as of the
//! start of the cycle), the edge, and the evidence itself (no newer evidence
//! for `C` than the watermark the verdict used, and `S`'s support still
//! passing after it), read through [`crate::store::evidence::rows_from_graph`].
//! The hybrid derive's `plan_still_holds` is no validator here: its
//! relevant-node ring ignores edge-only and status-only changes.
//!
//! A store without [`crate::store::GraphStore::dependent_evidence`]
//! (`StoreError::Capability`, its trait default) skips supersession and the
//! hold for the whole cycle with one warning; the rest of the cycle runs.
//! Any other store error keeps the evaluator's established semantics (Stage
//! 1 alone lands, the cycle fails).

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};

use super::eval::EvalParams;
use super::stage3::in_repromotion_cooldown;
use crate::canon::stage2_evidence_passes;
use crate::daemon::conflict::{attribute_at, EdgeWriters, SharedTimeline, WriterTimeline};
use crate::daemon::contradiction::written_by;
use crate::graph::Graph;
use crate::store::evidence::{latest_at, newest_counted, rows_from_graph, span_since};
use crate::store::{DependentEvidence, GraphStore};
use crate::types::{
    tie_break_by_key, AgentId, CanonizationEvent, CanonizationStatus, Concept, DemotionReason,
    Edge, EdgeType, Node, NodeId, SessionId, StoreError,
};

/// The most forward probes (supersession and hold together) one cycle runs
/// (review M4). Each needs the evidence rows of two nodes, read once per
/// node per cycle; with its reverse probe a cycle reads at most twice this
/// many probes' worth. A cycle that would exceed it stops adding targets
/// and logs `CanonizationSupersessionProbeCap`; the ring cursor brings the
/// rest on later cycles.
pub const MAX_SUPERSESSION_PROBES: usize = 256;

/// One `(target, source)` pair to probe, read under the gather guard.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Probe {
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
    /// A reverse probe: evaluated only for the pair rule, never a verdict.
    pub reverse_only: bool,
}

/// A bounded probe set.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Probes {
    pub list: Vec<Probe>,
    /// The forward probes hit [`MAX_SUPERSESSION_PROBES`].
    pub capped: bool,
    /// Targets with a live contradiction the cap left unprobed (focused
    /// review 1): for the hold, Stage 3 does not promote them this cycle.
    pub deferred: Vec<NodeId>,
}

impl Probes {
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// The targets of the forward probes.
    pub fn targets(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.list
            .iter()
            .filter(|p| !p.reverse_only)
            .map(|p| p.target)
    }
}

/// A passing probe: the source that wins its target.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Verdict {
    pub target: NodeId,
    pub source: NodeId,
    pub edge: NodeId,
    /// The watermark the store measured `S`'s support after.
    pub watermark: DateTime<Utc>,
}

/// The evaluator's writer timeline, kept across cycles and rebuilt only when
/// the graph's epoch moves (review L10).
#[derive(Clone)]
pub(crate) struct Timeline(std::sync::Arc<SharedTimeline>);

impl Default for Timeline {
    fn default() -> Self {
        Self(SharedTimeline::new())
    }
}

impl std::fmt::Debug for Timeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Timeline")
    }
}

impl Timeline {
    pub fn get(&self, graph: &Graph) -> std::sync::Arc<WriterTimeline> {
        self.0.get(graph)
    }
}

/// Each node's evidence rows, read from the store once per cycle (review
/// M4).
#[derive(Default)]
pub(crate) struct EvidenceCache {
    rows: HashMap<NodeId, DependentEvidence>,
}

impl EvidenceCache {
    async fn get(
        &mut self,
        store: &dyn GraphStore,
        session: &SessionId,
        node: NodeId,
    ) -> Result<&DependentEvidence, StoreError> {
        match self.rows.entry(node) {
            std::collections::hash_map::Entry::Occupied(e) => Ok(e.into_mut()),
            std::collections::hash_map::Entry::Vacant(e) => {
                Ok(e.insert(store.dependent_evidence(session, node).await?))
            }
        }
    }
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

/// The one agent that certainly wrote an edge, when there is one: an exact
/// attribution, or a single interaction acting at its instant.
fn certain(writers: &EdgeWriters<'_>) -> Option<AgentId> {
    match writers {
        EdgeWriters::Certain(agent) => Some(agent.clone()),
        EdgeWriters::Acting([(_, agent)]) => Some(agent.clone()),
        EdgeWriters::Acting(_) => None,
    }
}

/// The dependence edges into `target` that do not count against the
/// contradiction `edge` declares: the ones its declarer certainly wrote, and
/// the dependence of the contradicting concept itself. `None` (do not probe)
/// when the declarer is not certain, or when a piece of evidence may or may
/// not be the declarer's (review L4: an ambiguity never demotes).
fn self_suppressed(
    graph: &Graph,
    target: NodeId,
    edge: &Edge,
    timeline: &WriterTimeline,
) -> Option<Vec<NodeId>> {
    let declarer = certain(&attribute_at(graph, edge, edge.created_at, timeline)?)?;
    let mut out = Vec::new();
    for e in graph.incident_edges(target) {
        let Some((pillar, dependent)) = e.dependence() else {
            continue;
        };
        if pillar != target {
            continue;
        }
        if dependent == edge.source {
            out.push(e.id);
            continue;
        }
        let Some(writers) = attribute_at(graph, e, e.created_at, timeline) else {
            continue;
        };
        match certain(&writers) {
            Some(agent) if agent == declarer => out.push(e.id),
            Some(_) => {}
            None if writers.all().any(|a| *a == declarer) => return None,
            None => {}
        }
    }
    out.sort_by_key(|id| id.0);
    Some(out)
}

/// `id`'s `last_demotion_time` as of the start of the cycle (the apply
/// loop's snapshot), else as the graph has it now.
fn last_demotion(
    c: &Concept,
    before: Option<&HashMap<NodeId, Option<DateTime<Utc>>>>,
) -> Option<DateTime<Utc>> {
    before
        .and_then(|b| b.get(&c.id).copied())
        .unwrap_or(c.last_demotion_time)
}

/// Whether `id` is a concept of `session` outside its cooldown.
fn usable<'g>(
    graph: &'g Graph,
    id: NodeId,
    session: &SessionId,
    params: &EvalParams,
    now: DateTime<Utc>,
    before: Option<&HashMap<NodeId, Option<DateTime<Utc>>>>,
) -> Option<&'g Concept> {
    concept(graph, id).filter(|c| {
        &c.session_id == session
            && !in_repromotion_cooldown(last_demotion(c, before), params.cooldown, now)
    })
}

/// The live contradictions of `target` that may act on it: `(edge, source)`
/// with the source a usable concept of the session.
fn contradictions<'g>(
    graph: &'g Graph,
    target: &Concept,
    params: &EvalParams,
    now: DateTime<Utc>,
) -> Vec<(&'g Edge, &'g Concept)> {
    let mut out: Vec<_> = graph
        .incident_edges(target.id)
        .into_iter()
        .filter(|e| {
            e.edge_type == EdgeType::Contradicts
                && e.target == target.id
                && e.source != target.id
                && written_by(e, now)
        })
        .filter_map(|e| {
            usable(graph, e.source, &target.session_id, params, now, None).map(|s| (e, s))
        })
        .collect();
    out.sort_by_key(|(e, _)| e.id.0);
    out
}

fn probe(
    graph: &Graph,
    target: &Concept,
    edge: &Edge,
    source: &Concept,
    timeline: &WriterTimeline,
    reverse_only: bool,
) -> Option<Probe> {
    Some(Probe {
        target: target.id,
        source: source.id,
        source_key: source.canonical_key.clone(),
        edge: edge.id,
        baseline: baseline(graph, target),
        exclude: self_suppressed(graph, target.id, edge, timeline)?,
        reverse_only,
    })
}

/// The forward probes for `targets` in order, at most `budget` of them
/// (whole targets only), plus one reverse probe per forward pair whose
/// target also contradicts its source (the pair rule's other direction).
fn probes_for(
    graph: &Graph,
    targets: &[NodeId],
    params: &EvalParams,
    now: DateTime<Utc>,
    timeline: &WriterTimeline,
    budget: &mut usize,
) -> Probes {
    let mut out = Probes::default();
    // Pairs whose attribution is ambiguous (review L4): never probed, and
    // their other direction is not probed either (focused review 3), so an
    // ambiguity on one side cannot let the other side act alone.
    let mut ambiguous: HashSet<(NodeId, NodeId)> = HashSet::new();
    let mut targets = targets.iter();
    for &target_id in targets.by_ref() {
        let Some(target) = concept(graph, target_id) else {
            continue;
        };
        let mut forward = Vec::new();
        for (edge, source) in contradictions(graph, target, params, now) {
            match probe(graph, target, edge, source, timeline, false) {
                Some(p) => forward.push(p),
                None => {
                    ambiguous.insert((target.id, source.id));
                }
            }
        }
        if forward.len() > *budget {
            out.capped = true;
            if out.list.is_empty() && *budget == MAX_SUPERSESSION_PROBES {
                // A target with more contradictions than the whole cap is
                // still probed when it comes first, so it cannot starve the
                // ring; its reads count against the cap, which is spent, and
                // the cycle warns (focused review 1).
                *budget = 0;
                out.list.extend(forward);
                continue;
            }
            out.deferred.push(target_id);
            break;
        }
        *budget -= forward.len();
        out.list.extend(forward);
    }
    // Every target after the cap with a live contradiction is deferred: not
    // probed, so (for the hold) not promoted this cycle either.
    for &target_id in targets {
        if let Some(target) = concept(graph, target_id)
            && !contradictions(graph, target, params, now).is_empty()
        {
            out.deferred.push(target_id);
        }
    }
    let mut reverse = Vec::new();
    for p in out.list.iter() {
        if out
            .list
            .iter()
            .any(|q| q.target == p.source && q.source == p.target)
        {
            continue;
        }
        let (Some(t), Some(s)) = (concept(graph, p.source), concept(graph, p.target)) else {
            continue;
        };
        if let Some((edge, source)) = contradictions(graph, t, params, now)
            .into_iter()
            .find(|(_, s2)| s2.id == s.id)
        {
            match probe(graph, t, edge, source, timeline, true) {
                Some(r) => reverse.push(r),
                None => {
                    ambiguous.insert((t.id, source.id));
                }
            }
        }
    }
    out.list.extend(reverse);
    out.list
        .retain(|p| !ambiguous.contains(&(p.source, p.target)));
    out
}

/// Gather: the bounded supersession probes, under the read guard. Advances
/// `cursor` over the ring of eligible Canonical targets.
pub(crate) fn gather(
    graph: &Graph,
    params: &EvalParams,
    now: DateTime<Utc>,
    cursor: &mut Option<NodeId>,
    timeline: &Timeline,
    budget: &mut usize,
) -> Probes {
    if !params.supersede {
        return Probes::default();
    }
    let session = graph.session_id();
    let mut ring: Vec<NodeId> = graph
        .concepts()
        .filter(|c| c.canonization_status == CanonizationStatus::Canonical)
        .filter(|c| usable(graph, c.id, session, params, now, None).is_some())
        .filter(|c| !contradictions(graph, c, params, now).is_empty())
        .map(|c| c.id)
        .collect();
    if ring.is_empty() {
        return Probes::default();
    }
    ring.sort_by_key(|id| id.0);
    let window = super::eval::ring_window(&ring, *cursor, params.batch_size);
    let probes = probes_for(graph, &window, params, now, &timeline.get(graph), budget);
    // The cursor stops at the last target actually probed, so a capped
    // cycle resumes where it stopped.
    let last = if probes.capped {
        probes.targets().last()
    } else {
        window.last().copied()
    };
    if let Some(last) = last {
        *cursor = Some(last);
    }
    probes
}

/// The hold probes for Stage 3's window: the same probes, with Venerable
/// targets. Empty unless supersession is on.
pub(crate) fn gather_holds(
    graph: &Graph,
    window: &[NodeId],
    params: &EvalParams,
    now: DateTime<Utc>,
    timeline: &Timeline,
    budget: &mut usize,
) -> Probes {
    if !params.supersede {
        return Probes::default();
    }
    let session = graph.session_id();
    let targets: Vec<NodeId> = window
        .iter()
        .copied()
        .filter(|&id| usable(graph, id, session, params, now, None).is_some())
        .collect();
    probes_for(graph, &targets, params, now, &timeline.get(graph), budget)
}

/// The hold probes for one Venerable concept, for inspect's explanation
/// (review M2). Empty unless supersession is on and `node` is Venerable.
pub(crate) fn gather_hold_for(
    graph: &Graph,
    node: NodeId,
    params: &EvalParams,
    now: DateTime<Utc>,
) -> Probes {
    let venerable = concept(graph, node)
        .is_some_and(|c| c.canonization_status == CanonizationStatus::Venerable);
    if !params.supersede || !venerable {
        return Probes::default();
    }
    let timeline = WriterTimeline::of(graph);
    let mut budget = MAX_SUPERSESSION_PROBES;
    probes_for(graph, &[node], params, now, &timeline, &mut budget)
}

/// One passing probe.
struct Pass<'p> {
    probe: &'p Probe,
    watermark: DateTime<Utc>,
    /// The newest evidence the span counted (review L2).
    newest: DateTime<Utc>,
}

/// Whether `a` outranks `b`: newer counted evidence, then the smaller
/// canonical key of the source, then the smaller id.
fn outranks(a: &Pass<'_>, b: &Pass<'_>) -> bool {
    a.newest
        .cmp(&b.newest)
        .then_with(|| {
            tie_break_by_key(
                Some(&b.probe.source_key),
                &b.probe.source,
                Some(&a.probe.source_key),
                &a.probe.source,
            )
        })
        .is_gt()
}

/// The store's answers for `probes`, with no lock held: every passing
/// probe, the pair rule, then one winning source per forward target.
/// `Err(Capability)` when the store lacks `dependent_evidence`; the caller
/// warns once.
pub(crate) async fn decide(
    store: &dyn GraphStore,
    session: &SessionId,
    probes: &Probes,
    params: &EvalParams,
    now: DateTime<Utc>,
    cache: &mut EvidenceCache,
) -> Result<Vec<Verdict>, StoreError> {
    let mut passing: Vec<Pass<'_>> = Vec::new();
    for p in &probes.list {
        let target = cache.get(store, session, p.target).await?;
        let watermark = latest_at(target, params.min_age, now, &p.exclude)
            .map_or(p.baseline, |at| at.max(p.baseline));
        let source = cache.get(store, session, p.source).await?;
        crate::store::validate_coverage_window(params.coverage_window)?;
        let span = span_since(
            source,
            params.min_age,
            now,
            params.coverage_window,
            watermark,
        );
        if !stage2_evidence_passes(span) {
            continue;
        }
        let newest = newest_counted(source, params.min_age, now, watermark).unwrap_or(watermark);
        passing.push(Pass {
            probe: p,
            watermark,
            newest,
        });
    }
    // The pair rule: of two concepts that each qualify against the other,
    // only the higher-ranked acts.
    let beaten: HashSet<(NodeId, NodeId)> = passing
        .iter()
        .filter(|a| {
            passing.iter().any(|b| {
                b.probe.target == a.probe.source
                    && b.probe.source == a.probe.target
                    && outranks(b, a)
            })
        })
        .map(|a| (a.probe.target, a.probe.source))
        .collect();
    let mut winners: Vec<&Pass<'_>> = passing
        .iter()
        .filter(|p| !p.probe.reverse_only && !beaten.contains(&(p.probe.target, p.probe.source)))
        .collect();
    winners.sort_by(|a, b| {
        a.probe.target.0.cmp(&b.probe.target.0).then_with(|| {
            match (outranks(a, b), outranks(b, a)) {
                (true, _) => std::cmp::Ordering::Less,
                (_, true) => std::cmp::Ordering::Greater,
                _ => std::cmp::Ordering::Equal,
            }
        })
    });
    let mut out: Vec<Verdict> = Vec::new();
    for w in winners {
        if out.last().is_some_and(|v| v.target == w.probe.target) {
            continue;
        }
        out.push(Verdict {
            target: w.probe.target,
            source: w.probe.source,
            edge: w.probe.edge,
            watermark: w.watermark,
        });
    }
    Ok(out)
}

/// The cooldown state before the cycle commits anything, for every concept
/// a verdict names (the apply loop's snapshot, review M1).
pub(crate) fn cooldowns_before(
    graph: &Graph,
    verdicts: &[Verdict],
) -> HashMap<NodeId, Option<DateTime<Utc>>> {
    verdicts
        .iter()
        .flat_map(|v| [v.target, v.source])
        .filter_map(|id| concept(graph, id).map(|c| (id, c.last_demotion_time)))
        .collect()
}

/// Apply-time revalidation under the write guard (see the module docs).
/// Cooldowns are judged as of `before` (the start of the cycle), so the
/// order verdicts are applied in changes nothing.
pub(crate) fn still_holds(
    graph: &Graph,
    verdict: &Verdict,
    params: &EvalParams,
    now: DateTime<Utc>,
    timeline: &WriterTimeline,
    before: &HashMap<NodeId, Option<DateTime<Utc>>>,
) -> bool {
    let session = graph.session_id();
    let Some(target) = usable(graph, verdict.target, session, params, now, Some(before)) else {
        return false;
    };
    if target.canonization_status != CanonizationStatus::Canonical
        || usable(graph, verdict.source, session, params, now, Some(before)).is_none()
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
pub(crate) fn event(
    graph: &Graph,
    verdict: &Verdict,
    now: DateTime<Utc>,
    id: NodeId,
) -> CanonizationEvent {
    CanonizationEvent {
        id,
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

/// The warning for a store without `dependent_evidence`.
pub(crate) fn warn_unsupported(capability: &str) {
    tracing::warn!(
        target: "lambo::canon",
        capability = %capability,
        "CanonizationSupersessionUnsupported: canonization_supersede is on but the \
         store does not implement dependent_evidence, so no concept is superseded or \
         held this cycle; the rest of the cycle ran"
    );
}

/// The warning for a cycle that hit [`MAX_SUPERSESSION_PROBES`].
pub(crate) fn warn_capped() {
    tracing::warn!(
        target: "lambo::canon",
        cap = MAX_SUPERSESSION_PROBES,
        "CanonizationSupersessionProbeCap: this cycle reached the supersession probe \
         cap; the remaining targets are probed on later cycles"
    );
}
