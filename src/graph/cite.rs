//! Citations (#36 PR 4) and contradictions (#19(b)) on a write.
//!
//! A derived concept may declare what it rests on (`depends_on`: existing
//! concepts it cites). A citation is written as `citing -Dependency->
//! cited`, the edge `record_action`'s `depends_on` writes, so the cited
//! concept gains a dependent (#36's dependents relation) and with it blast
//! radius and canonization evidence.
//!
//! # Resolve or refuse, never create
//!
//! Every reference resolves through `crate::surface::resolve` against the
//! session the write targets: a full node id, a unique short id or the
//! canonical key, to **exactly one existing concept**. No synonym is
//! consulted, so nothing a write declared can redirect a citation. Missing,
//! ambiguous, interaction ids, foreign-session, self and cyclic references
//! refuse the whole call with a typed [`LamboError::Reference`], before the
//! ack where the pre-pass can see it ([`validate`]). A citation never
//! creates its target, so a call cannot cite a concept it creates itself.
//!
//! # Pinned at the ack, re-resolved at the commit
//!
//! [`validate`] returns the resolution it accepted ([`PinnedCitations`]),
//! and the job and its durable intent carry it. The commit re-resolves every
//! reference under the graph write lock, inside the write's transaction and
//! before the call's own writes ([`recheck`]), and refuses with
//! [`ReferenceRefusalKind::Changed`] when the answer differs: a cited
//! concept removed or shadowed between the ack and the apply never becomes
//! a dangling or silently redirected edge, and a durable intent replayed
//! after a restart reaches the same answer or settles `failed`. The self
//! and cycle checks run again against the written sources ([`write_edges`]),
//! because a hybrid write can resolve a concept differently at commit than
//! the pre-pass saw. One case is not a refusal: a hybrid similarity merge
//! that makes the citing concept the same as its cited target drops that
//! citation and counts it as a reinforcement (the merge's `Semantic` edge
//! already ties the two).
//!
//! # One path for every reference field
//!
//! The pin, the recheck and the edge write take a [`ReferenceSpec`]: the
//! field, the edge it writes, its weight, whether that edge family must
//! stay acyclic, its limits, and whether a similarity merge into the target
//! makes the reference a reinforcement. [`DEPENDS_ON`] is a derive's
//! citations; [`AGAINST`] and [`ACTION_AGAINST`] are #19(b)'s
//! contradictions, `newer -Contradicts-> older`, on a derive concept and on
//! a `record_action` action. A contradiction is resolved, pinned, rechecked
//! and refused exactly like a citation (owner decision, #19(b) option A),
//! and differs in four ways only:
//!
//! * **No acyclicity.** Two concepts may contradict each other (`Q -> X`
//!   and `X -> Q`, the #19(a) fixture's pair): each asserts the other is
//!   wrong. `Contradicts` is outside §5.7's `Causal`/`Dependency` family,
//!   and no dependence or recall walk follows it, so a cycle of
//!   contradictions cannot loop anything. GC connectivity does not follow
//!   it either (#19(c) owner decision 1, option C). Self-contradiction is
//!   still refused.
//! * **Not the same target as a citation.** One concept (or action) may not
//!   both cite and contradict the same concept in one call
//!   ([`ReferenceRefusalKind::DependsOnAndAgainst`], review M2); separate
//!   writes may.
//!
//! The edge's direction comes from the write, not from node age: the
//! concept that declares the contradiction is `newer`, whatever the two
//! concepts' creation times, and the contradiction is dated by that
//! write's about-time: its `event_time` if one was given, otherwise its
//! write time. Re-sending it reinforces the edge without re-dating it.
//! * **A similarity merge keeps it.** A hybrid similarity merge writes the
//!   new concept as its own node tied to its neighbour by a `Semantic`
//!   edge. For a citation of that neighbour the tie already says it, so the
//!   citation is a reinforcement; for a contradiction it does not (similar
//!   text often states the opposite), so the `Contradicts` edge is written.
//! * **Its own budget.** At most [`MAX_AGAINST_PER_CONCEPT`] per concept and
//!   [`MAX_AGAINST_PER_DERIVE`] per derive call, counted apart from
//!   `depends_on`; an action takes at most [`MAX_AGAINST_PER_CONCEPT`].
//!
//! # Idempotent by natural key
//!
//! Re-sending the same citation writes no second edge: the
//! `(citing, cited, Dependency)` natural key reinforces the one edge, and
//! `(newer, older, Contradicts)` likewise.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};

use crate::graph::canonical::{canonicalize, CanonicalizeResult};
use crate::graph::derive::DeriveOutcome;
use crate::graph::Graph;
use crate::surface::resolve::{self, Resolved};
use crate::types::{
    ConceptType, Edge, EdgeType, LamboError, NodeId, ReferenceField, ReferenceRefusalKind,
    SessionId,
};

pub use crate::types::{ConceptCitations, PinnedCitations, PinnedReference};

/// One reference field of a write: what it writes and its limits.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ReferenceSpec {
    /// The request field, for refusals.
    pub(crate) field: ReferenceField,
    /// The edge a resolved reference writes, `citing -> cited`.
    pub(crate) edge_type: EdgeType,
    /// Its initial weight.
    pub(crate) weight: f64,
    /// Whether the edge must keep `Causal`/`Dependency` acyclic (§5.7).
    pub(crate) acyclic: bool,
    /// At most this many references on one concept.
    pub(crate) per_concept: usize,
    /// At most this many references in one call.
    pub(crate) per_call: usize,
    /// Whether a hybrid similarity merge of the citing concept into the
    /// target drops the reference as a reinforcement (the merge's `Semantic`
    /// edge already ties the two). True for a citation, false for a
    /// contradiction, which the tie does not express.
    pub(crate) merge_is_reinforcement: bool,
}

/// At most this many `depends_on` references on one concept.
pub const MAX_REFERENCES_PER_CONCEPT: usize = 8;
/// At most this many `depends_on` references in one derive call.
pub const MAX_REFERENCES_PER_DERIVE: usize = 32;

/// A derive concept's `depends_on`: a `Dependency` edge with the weight
/// `record_action` gives its own, acyclic.
pub(crate) const DEPENDS_ON: ReferenceSpec = ReferenceSpec {
    field: ReferenceField::DependsOn,
    edge_type: EdgeType::Dependency,
    weight: 0.5,
    acyclic: true,
    per_concept: MAX_REFERENCES_PER_CONCEPT,
    per_call: MAX_REFERENCES_PER_DERIVE,
    merge_is_reinforcement: true,
};

/// At most this many `against` references on one concept (or one action).
pub const MAX_AGAINST_PER_CONCEPT: usize = 8;
/// At most this many `against` references in one derive call.
pub const MAX_AGAINST_PER_DERIVE: usize = 32;

/// The weight of a `Contradicts` edge. The edge never decays and feeds no
/// score (#19(a)), so the weight only says the opposition was declared
/// outright.
pub(crate) const CONTRADICTS_WEIGHT: f64 = 1.0;

/// A derive concept's `against` (#19(b)): `concept -Contradicts-> target`.
pub(crate) const AGAINST: ReferenceSpec = ReferenceSpec {
    field: ReferenceField::Against,
    edge_type: EdgeType::Contradicts,
    weight: CONTRADICTS_WEIGHT,
    acyclic: false,
    per_concept: MAX_AGAINST_PER_CONCEPT,
    per_call: MAX_AGAINST_PER_DERIVE,
    merge_is_reinforcement: false,
};

/// A `record_action`'s `against` (#19(b)): `action -Contradicts-> target`.
/// One action is one concept, so the call limit is the concept limit.
pub(crate) const ACTION_AGAINST: ReferenceSpec = ReferenceSpec {
    field: ReferenceField::ActionAgainst,
    edge_type: EdgeType::Contradicts,
    weight: CONTRADICTS_WEIGHT,
    acyclic: false,
    per_concept: MAX_AGAINST_PER_CONCEPT,
    per_call: MAX_AGAINST_PER_CONCEPT,
    merge_is_reinforcement: false,
};

fn refuse(
    spec: &ReferenceSpec,
    concept: &str,
    reference: &str,
    kind: ReferenceRefusalKind,
) -> LamboError {
    resolve::refusal(spec.field, Some(concept), reference, kind)
}

/// The derive's call-time pre-pass for `depends_on` and `against` (see
/// [`pin`]). `depends_on` is resolved first, so a call with a bad entry in
/// each is refused for the citation.
pub(crate) fn validate(
    g: &Graph,
    concepts: &[(&str, ConceptType)],
    citations: &[ConceptCitations],
) -> Result<PinnedCitations, LamboError> {
    let depends_on: Vec<(&str, &[String])> = citations
        .iter()
        .map(|c| (c.content.as_str(), c.depends_on.as_slice()))
        .collect();
    let against: Vec<(&str, &[String])> = citations
        .iter()
        .map(|c| (c.content.as_str(), c.against.as_slice()))
        .collect();
    let pinned = PinnedCitations {
        references: pin(g, concepts, &DEPENDS_ON, &depends_on)?,
        against: pin(g, concepts, &AGAINST, &against)?,
    };
    refuse_cited_and_contradicted(&pinned)?;
    Ok(pinned)
}

/// #19(b) review M2: an `against` reference whose `(content, target)` the
/// same call also cites in `depends_on` is refused
/// ([`ReferenceRefusalKind::DependsOnAndAgainst`]). Both pinned lists are
/// fixed at the ack and rechecked to the same targets at the commit, so
/// this one check at the pre-pass (and again at the commit, for a library
/// caller handing the commit its own pins) covers both.
pub(crate) fn refuse_cited_and_contradicted(pinned: &PinnedCitations) -> Result<(), LamboError> {
    for p in &pinned.against {
        if pinned
            .references
            .iter()
            .any(|r| r.content == p.content && r.target == p.target)
        {
            return Err(refuse(
                &AGAINST,
                &p.content,
                &p.reference,
                ReferenceRefusalKind::DependsOnAndAgainst,
            ));
        }
    }
    Ok(())
}

/// The commit's re-resolution of a derive's citations and contradictions
/// ([`recheck`] for each list).
pub(crate) fn recheck_all(g: &Graph, pinned: &PinnedCitations) -> Result<(), LamboError> {
    refuse_cited_and_contradicted(pinned)?;
    recheck(g, &DEPENDS_ON, &pinned.references)?;
    recheck(g, &AGAINST, &pinned.against)
}

/// Write a derive's citation and contradiction edges ([`write_edges`] for
/// each list), counting them on `outcome` (`cited`, `contradicted`,
/// `reinforced`).
pub(crate) fn write_all_edges(
    g: &mut Graph,
    pinned: &PinnedCitations,
    written: &Written,
    stamp: Stamp,
    session_id: &SessionId,
    outcome: &mut DeriveOutcome,
) -> Result<(), LamboError> {
    let cited = write_edges(
        g,
        &DEPENDS_ON,
        &pinned.references,
        written,
        stamp,
        session_id,
    )?;
    let contradicted = write_edges(g, &AGAINST, &pinned.against, written, stamp, session_id)?;
    outcome.cited += cited.added;
    outcome.contradicted += contradicted.added;
    outcome.reinforced += cited.reinforced + contradicted.reinforced;
    Ok(())
}

/// Resolve every reference of `entries` (`(citing content, references)`,
/// a content listed twice being one concept) against `g`, the target
/// session's graph, under `spec`'s limits; refuse self references and, for
/// an acyclic spec, an edge that would close a cycle with what the graph
/// already shows. Returns what it accepted, to pin on the job. Read-only.
pub(crate) fn pin(
    g: &Graph,
    concepts: &[(&str, ConceptType)],
    spec: &ReferenceSpec,
    entries: &[(&str, &[String])],
) -> Result<Vec<PinnedReference>, LamboError> {
    let mut pinned: Vec<PinnedReference> = Vec::new();
    if entries.iter().all(|(_, refs)| refs.is_empty()) {
        return Ok(pinned);
    }
    // Each of the call's contents and its current canonical match.
    let mut matched: HashMap<&str, Option<NodeId>> = HashMap::new();
    for &(content, _) in concepts {
        if matched.contains_key(content) {
            continue;
        }
        let m = match canonicalize(content, g)? {
            CanonicalizeResult::Matched { node, .. } => Some(node),
            CanonicalizeResult::Unmatched { .. } => None,
        };
        matched.insert(content, m);
    }
    // Limits first, so an over-long call is refused as such whatever its
    // references would resolve to.
    let mut per_concept: HashMap<&str, usize> = HashMap::new();
    let mut total = 0usize;
    for &(content, refs) in entries {
        for reference in refs {
            let n = per_concept.entry(content).or_insert(0);
            *n += 1;
            total += 1;
            if *n > spec.per_concept {
                return Err(refuse(
                    spec,
                    content,
                    reference,
                    ReferenceRefusalKind::Limit {
                        what: "too many references on one concept",
                        limit: spec.per_concept,
                    },
                ));
            }
            if total > spec.per_call {
                return Err(refuse(
                    spec,
                    content,
                    reference,
                    ReferenceRefusalKind::Limit {
                        what: "too many references in one call",
                        limit: spec.per_call,
                    },
                ));
            }
        }
    }
    for &(content, refs) in entries {
        let Some(source) = matched.get(content) else {
            return Err(refuse(
                spec,
                content,
                refs.first().map_or("", String::as_str),
                ReferenceRefusalKind::UnknownConcept,
            ));
        };
        for reference in refs {
            let (target, _) = resolve::resolve_existing(g, spec.field, Some(content), reference)?;
            if *source == Some(target) {
                return Err(refuse(
                    spec,
                    content,
                    reference,
                    ReferenceRefusalKind::SelfReference,
                ));
            }
            if !pinned
                .iter()
                .any(|p| p.content == content && p.target == target)
            {
                pinned.push(PinnedReference {
                    content: content.to_string(),
                    reference: reference.clone(),
                    target,
                });
            }
        }
    }
    if spec.acyclic {
        // A cycle the graph can already show: a cited concept that depends,
        // through `Causal`/`Dependency` edges, on a citing concept the call
        // matches. A concept the call creates has no edges yet.
        let source_of = |p: &PinnedReference| matched.get(p.content.as_str()).copied().flatten();
        let planned: Vec<(NodeId, NodeId, EdgeType)> = pinned
            .iter()
            .filter_map(|p| source_of(p).map(|s| (s, p.target, spec.edge_type)))
            .collect();
        if let Some((source, target, _)) = crate::graph::action::closing_edge(g, &planned) {
            let p = pinned
                .iter()
                .find(|p| p.target == target && source_of(p) == Some(source))
                .expect("the closing edge is one of the planned references");
            return Err(refuse(
                spec,
                &p.content,
                &p.reference,
                ReferenceRefusalKind::Cycle,
            ));
        }
    }
    Ok(pinned)
}

/// The commit's re-resolution, under the write lock and before the call's
/// own writes: every pinned reference must still name its pinned concept.
pub(crate) fn recheck(
    g: &Graph,
    spec: &ReferenceSpec,
    pinned: &[PinnedReference],
) -> Result<(), LamboError> {
    for p in pinned {
        let now = match resolve::resolve_reference(g, &p.reference) {
            Resolved::Concept { node, .. } if node == p.target => continue,
            Resolved::Concept { node, .. } => Some(node),
            Resolved::Missing | Resolved::Ambiguous(_) | Resolved::NotAConcept(_) => None,
        };
        return Err(refuse(
            spec,
            &p.content,
            &p.reference,
            ReferenceRefusalKind::Changed {
                accepted: Some(p.target),
                now,
            },
        ));
    }
    Ok(())
}

/// When the reference edges are stamped: the interaction's about-time and
/// creation time, as every other edge the derive writes.
#[derive(Clone, Copy)]
pub(crate) struct Stamp {
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) event_time: Option<DateTime<Utc>>,
}

/// What a derive's commit knows about the concepts it wrote, for
/// [`write_edges`]: the node each content was written as, and which new
/// nodes a hybrid similarity merge tied to an existing one (`new -> target`).
#[derive(Debug, Default)]
pub(crate) struct Written {
    pub(crate) by_content: HashMap<String, NodeId>,
    pub(crate) merged_into: HashMap<NodeId, NodeId>,
}

/// What [`write_edges`] wrote: edges it added, and references that
/// reinforced an existing edge or were dropped as a reinforcement.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct EdgeCounts {
    pub(crate) added: usize,
    pub(crate) reinforced: usize,
}

/// Write the reference edges, inside the write's transaction and after its
/// concepts. For a spec whose [`ReferenceSpec::merge_is_reinforcement`]
/// holds, a reference whose citing concept a similarity merge tied to its
/// target is dropped and counted as reinforced. Refuses (and so rolls the
/// whole write back) a reference whose source turned out to be its target,
/// or, for an acyclic spec, whose edge would close a cycle with the graph as
/// written so far.
pub(crate) fn write_edges(
    g: &mut Graph,
    spec: &ReferenceSpec,
    pinned: &[PinnedReference],
    written: &Written,
    stamp: Stamp,
    session_id: &SessionId,
) -> Result<EdgeCounts, LamboError> {
    let mut counts = EdgeCounts::default();
    let mut planned: Vec<(NodeId, NodeId, EdgeType)> = Vec::new();
    let mut seen: HashSet<(NodeId, NodeId)> = HashSet::new();
    for p in pinned {
        let source = written.by_content.get(&p.content).copied().ok_or_else(|| {
            refuse(
                spec,
                &p.content,
                &p.reference,
                ReferenceRefusalKind::UnknownConcept,
            )
        })?;
        if spec.merge_is_reinforcement && written.merged_into.get(&source) == Some(&p.target) {
            counts.reinforced += 1;
            continue;
        }
        if source == p.target {
            return Err(refuse(
                spec,
                &p.content,
                &p.reference,
                ReferenceRefusalKind::SelfReference,
            ));
        }
        if seen.insert((source, p.target)) {
            planned.push((source, p.target, spec.edge_type));
        }
    }
    if spec.acyclic
        && let Some((source, target, _)) = crate::graph::action::closing_edge(g, &planned)
    {
        let p = pinned
            .iter()
            .find(|p| p.target == target && written.by_content.get(&p.content) == Some(&source))
            .expect("the closing edge is one of the planned references");
        return Err(refuse(
            spec,
            &p.content,
            &p.reference,
            ReferenceRefusalKind::Cycle,
        ));
    }
    for &(source, target, edge_type) in &planned {
        if g.edge_between(source, target, edge_type).is_some() {
            counts.reinforced += 1;
        } else {
            counts.added += 1;
        }
        g.upsert_edge(Edge {
            event_time: stamp.event_time,
            id: NodeId::new(),
            session_id: session_id.clone(),
            source,
            target,
            edge_type,
            weight: spec.weight,
            reinforcements: 1,
            created_at: stamp.created_at,
            last_reinforced: stamp.created_at,
        })?;
    }
    Ok(counts)
}

#[cfg(test)]
mod against_tests;
#[cfg(test)]
mod tests;
