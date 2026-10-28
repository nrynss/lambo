//! Suspected supersession (#198): a newer fact written **without** `against`
//! that looks like a new value of an older one.
//!
//! Only declared contradictions reach the contradiction detector
//! (`crate::daemon::contradiction`). An agent that records "the dogfood
//! embedder is candle bge-m3" and never cites the llama.cpp fact it replaces
//! leaves the stale fact ranking with no warning, and since #195 the
//! identifier merge guard refuses the vector merge that used to tie the two.
//! This module turns that refusal, when the texts say so, into a
//! **warning-only** signal: an [`EdgeType::SuspectedSupersedes`] edge,
//! `suspected replacement -> older concept`.
//!
//! # The trigger (owner decisions on #198, 2026-10-27)
//!
//! At hybrid write time (`crate::graph::hybrid`), for a concept this write
//! creates, every vector hit that
//!
//! * scores at or above the session's effective merge threshold
//!   (`semantic_match_threshold`, the #87 calibrated one when it applies),
//! * and that the **identifier guard** refused
//!   ([`crate::graph::canonical::same_identifiers`] false; a hit the
//!   content is declared `against` is the other guard, and is not one),
//!
//! is a suspected pair when every condition of [`candidate`] holds:
//!
//! * both are concepts of the session, of the **same type**, and that type
//!   is `Logic`, `Constraint` or `Observation` ([`eligible_type`]): two
//!   `Resource` or `Entity` names with different identifiers are two things
//!   by definition;
//! * the same **repository scope** (see [`repo_scope`]);
//! * no declared relation already joins them (an `against` or a
//!   `depends_on` either way settles it: [`settled`]);
//! * **one span and value slot** ([`slot`]): the aligned texts differ in
//!   exactly one token span, and in both it starts after the first
//!   predicate cue. No shared anchor is required;
//! * their **dates differ**: each concept is dated by its origin
//!   interaction's about-time clamped to its write time ([`dated_at`], the
//!   clock the contradiction detector uses), the later one is the suspected
//!   replacement, and a tie records nothing. A backfilled write with an
//!   older `event_time` therefore points the edge at itself, not away.
//!
//! The edge is written inside the write's transaction, under the same
//! write lock, after the write's own citations, so it is part of the same
//! flush batch. Only new writes are flagged: nothing rescans the store.
//! `lambo canon suspected` reports, and with `--apply` writes, the pairs an
//! existing store holds.
//!
//! Deterministic: the condition reads only the two texts, their types,
//! keys and dates, and the vector score the store returned, so the same
//! writes produce the same edges on every backend.
//!
//! # What the edge does and does not do
//!
//! Informational like `Contradicts`, and never read as one: no score
//! density or edge bonus, no blast radius or dependents, no recall
//! expansion, no decay, no GC connectivity, no conflict agent. #207's
//! stale-by-dependency check never follows it (it reads only `Dependency`
//! citations), and #181's redundancy-aware selection must treat a suspected
//! pair as never redundant ([`is_suspected_pair`]). Recall shows it on the
//! older concept only when both are in the result (`crate::recall::assemble`).
//!
//! # Lapse
//!
//! The edge is removed when either concept is collected
//! ([`Graph::remove_node`] removes every incident edge), and when an
//! explicit `against` or `depends_on` between the two is written
//! ([`settle`], called by `crate::graph::cite::write_edges`).

pub mod replacement;
pub mod slot;

use chrono::{DateTime, Utc};

use crate::graph::Graph;
use crate::types::{Concept, ConceptType, Edge, EdgeType, LamboError, Node, NodeId, SessionId};

pub use slot::{one_span_value_slot, PREDICATE_CUES};

/// The weight of a `SuspectedSupersedes` edge. It never decays and feeds no
/// score, so the weight carries nothing beyond "recorded".
pub const SUSPECTED_SUPERSEDES_WEIGHT: f64 = 1.0;

/// Whether a concept type can be in a suspected pair: `Logic`, `Constraint`
/// and `Observation` only. Exhaustive on purpose: a new type has to choose.
pub const fn eligible_type(ty: ConceptType) -> bool {
    match ty {
        ConceptType::Logic | ConceptType::Constraint | ConceptType::Observation => true,
        ConceptType::Resource | ConceptType::Entity => false,
    }
}

/// A concept's recorded repository scope: the scope of a repository-scoped
/// path key (#129), `None` for every other concept. Concepts of the
/// eligible types carry no scope of their own today (only a path-like
/// `Resource` is keyed by one), so for them the condition compares `None`
/// with `None`; it is written as the comparison so that it holds the day a
/// scope is recorded on other concepts too.
pub fn repo_scope(c: &Concept) -> Option<String> {
    crate::graph::resource::repo_scope_of_key(&c.canonical_key)
}

/// When a concept's fact is dated, for the direction: its origin
/// interaction's about-time clamped to that interaction's write time (the
/// contradiction detector's clock, `crate::daemon::contradiction`), or the
/// concept's own write time when the origin is not in the graph.
pub fn dated_at(g: &Graph, c: &Concept) -> DateTime<Utc> {
    match g.node(c.origin_interaction) {
        Some(Node::Interaction(i)) => i.about_time().min(i.created_at),
        _ => c.created_at,
    }
}

/// Whether an explicit relation already joins `a` and `b`: a `Contradicts`
/// (`against`) or `Dependency` (`depends_on`) edge, either way. Such a pair
/// is settled, so it is never suspected and an existing suspicion lapses.
pub fn settled(g: &Graph, a: NodeId, b: NodeId) -> bool {
    SETTLING
        .into_iter()
        .any(|ty| g.edge_between(a, b, ty).is_some() || g.edge_between(b, a, ty).is_some())
}

/// The declared relations that settle a suspected pair: `against`
/// (`Contradicts`) and `depends_on` (`Dependency`).
pub const SETTLING: [EdgeType; 2] = [EdgeType::Contradicts, EdgeType::Dependency];

/// [`settle`] on a snapshot's edge list, for a writer that edits edges
/// outside a [`Graph`] (the alias redirect overlay, `crate::canon::alias`):
/// every suspected-supersession edge between the ends of a `SETTLING` edge
/// in `declared`, either way, is dropped from `edges`. Returns how many.
pub fn settle_edges(edges: &mut Vec<Edge>, declared: &[Edge]) -> usize {
    let pairs: std::collections::HashSet<(NodeId, NodeId)> = declared
        .iter()
        .filter(|e| SETTLING.contains(&e.edge_type))
        .flat_map(|e| [(e.source, e.target), (e.target, e.source)])
        .collect();
    let before = edges.len();
    edges.retain(|e| {
        e.edge_type != EdgeType::SuspectedSupersedes || !pairs.contains(&(e.source, e.target))
    });
    before - edges.len()
}

/// Whether `a` and `b` are a suspected pair, either way. #181's
/// redundancy-aware selection must treat such a pair as **never
/// redundant** (owner decision 5 on #198): their cosine would otherwise
/// drop one of them, possibly the newer.
pub fn is_suspected_pair(g: &Graph, a: NodeId, b: NodeId) -> bool {
    g.edge_between(a, b, EdgeType::SuspectedSupersedes)
        .is_some()
        || g.edge_between(b, a, EdgeType::SuspectedSupersedes)
            .is_some()
}

/// One suspected pair: `newer` is suspected of replacing `older`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SuspectedPair {
    /// The suspected replacement: the edge's source.
    pub newer: NodeId,
    /// The concept it may replace: the edge's target.
    pub older: NodeId,
}

/// Every condition of the trigger but the vector one (module docs): the
/// pair `a`/`b` as `(newer, older)`, or `None`. The caller supplies the
/// vector condition: a hit at or above the merge threshold that the
/// identifier guard refused. That guard is checked here too, so the
/// function cannot flag a pair the guard would have merged.
pub fn candidate(g: &Graph, a: &Concept, b: &Concept) -> Option<SuspectedPair> {
    if a.id == b.id
        || a.session_id != b.session_id
        || a.concept_type != b.concept_type
        || !eligible_type(a.concept_type)
        || repo_scope(a) != repo_scope(b)
        || crate::graph::canonical::same_identifiers(&a.content, &b.content)
        || settled(g, a.id, b.id)
    {
        return None;
    }
    let (ta, tb) = (dated_at(g, a), dated_at(g, b));
    let (newer, older) = match ta.cmp(&tb) {
        std::cmp::Ordering::Greater => (a, b),
        std::cmp::Ordering::Less => (b, a),
        std::cmp::Ordering::Equal => return None,
    };
    slot::one_span_value_slot(&older.content, &newer.content).then_some(SuspectedPair {
        newer: newer.id,
        older: older.id,
    })
}

/// When the edge was written: the write's own timestamps.
#[derive(Clone, Copy, Debug)]
pub struct Stamp {
    /// The write time.
    pub created_at: DateTime<Utc>,
    /// The write's about-time, when it has one.
    pub event_time: Option<DateTime<Utc>>,
}

/// The stamp a pair's edge carries when it is recorded after the fact
/// (`lambo canon suspected --apply`): the write that made the pair, which
/// is the later of the two concepts' writes (the newer concept's, unless the
/// older was backfilled after it), with that write's about-time, exactly as
/// the hybrid trigger would have stamped it then. `None` if either end is
/// not a concept of `g`.
pub fn pair_stamp(g: &Graph, pair: SuspectedPair) -> Option<Stamp> {
    let concept = |id| match g.node(id) {
        Some(Node::Concept(c)) => Some(c),
        _ => None,
    };
    let (newer, older) = (concept(pair.newer)?, concept(pair.older)?);
    let later = if older.created_at > newer.created_at {
        older
    } else {
        newer
    };
    let event_time = match g.node(later.origin_interaction) {
        Some(Node::Interaction(i)) => i.event_time,
        _ => None,
    };
    Some(Stamp {
        created_at: later.created_at,
        event_time,
    })
}

/// Write `newer -SuspectedSupersedes-> older` unless it is already there.
/// Returns whether an edge was added. Both ends must be concepts of `g`
/// (the graph's endpoint matrix refuses anything else).
pub fn record(
    g: &mut Graph,
    pair: SuspectedPair,
    stamp: Stamp,
    session_id: &SessionId,
) -> Result<bool, LamboError> {
    if g.edge_between(pair.newer, pair.older, EdgeType::SuspectedSupersedes)
        .is_some()
    {
        return Ok(false);
    }
    g.upsert_edge(Edge {
        event_time: stamp.event_time,
        id: NodeId::new(),
        session_id: session_id.clone(),
        source: pair.newer,
        target: pair.older,
        edge_type: EdgeType::SuspectedSupersedes,
        weight: SUSPECTED_SUPERSEDES_WEIGHT,
        reinforcements: 1,
        created_at: stamp.created_at,
        last_reinforced: stamp.created_at,
    })?;
    Ok(true)
}

/// The trigger at commit (`crate::graph::hybrid`): for the concept `new`
/// this write created, and the vector hits the identifier guard refused at
/// or above the merge threshold, record every [`candidate`] pair. Hits that
/// are no longer concepts of `g` are skipped. Returns how many edges were
/// added. Hits are judged in id order, so the edges' write order is
/// deterministic.
pub fn record_refused_hits(
    g: &mut Graph,
    new: NodeId,
    refused: &[NodeId],
    stamp: Stamp,
) -> Result<usize, LamboError> {
    let Some(Node::Concept(new_c)) = g.node(new) else {
        return Ok(0);
    };
    let new_c = new_c.clone();
    let mut hits: Vec<NodeId> = refused.to_vec();
    hits.sort_by_key(|id| id.0);
    hits.dedup();
    let session_id = g.session_id().clone();
    let mut added = 0;
    for hit in hits {
        let pair = match g.node(hit) {
            Some(Node::Concept(old)) => candidate(g, &new_c, old),
            _ => None,
        };
        if let Some(pair) = pair
            && record(g, pair, stamp, &session_id)?
        {
            added += 1;
        }
    }
    Ok(added)
}

/// Remove any suspected-supersession edge between `a` and `b`, either way:
/// an explicit `against` or `depends_on` between them settles the relation.
/// Returns how many edges were removed.
pub fn settle(g: &mut Graph, a: NodeId, b: NodeId) -> Result<usize, LamboError> {
    let mut removed = 0;
    for (s, t) in [(a, b), (b, a)] {
        if let Some(id) = g
            .edge_between(s, t, EdgeType::SuspectedSupersedes)
            .map(|e| e.id)
        {
            g.remove_edge(id)?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// Every suspected-supersession edge of `g`, as pairs, in `(older, newer)`
/// id order.
pub fn pairs(g: &Graph) -> Vec<SuspectedPair> {
    let mut out: Vec<SuspectedPair> = g
        .edges()
        .filter(|e| e.edge_type == EdgeType::SuspectedSupersedes)
        .map(|e| SuspectedPair {
            newer: e.source,
            older: e.target,
        })
        .collect();
    out.sort_by_key(|p| (p.older.0, p.newer.0));
    out
}

/// One pair the trigger would flag in an existing session
/// (`lambo canon suspected`, owner decision 6 on #198).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScannedPair {
    /// The pair, newer -> older.
    pub pair: SuspectedPair,
    /// The cosine of the two concepts' stored vectors.
    pub cosine: f64,
    /// Whether the session already holds its `SuspectedSupersedes` edge.
    pub recorded: bool,
}

/// Every pair of `g` the trigger would flag if its newer concept were
/// written now: two concepts whose stored **text** vectors (an image
/// vector is never a merge candidate) are at cosine `>= threshold` and
/// that pass [`candidate`] (which includes the identifier guard refusing
/// them). Strongest first, then by the older and the newer id.
///
/// A superset of what writing would have flagged: the write path judges
/// only the store's top vector candidates
/// ([`crate::graph::hybrid::VECTOR_CANDIDATE_LIMIT`]), this compares every
/// pair. Quadratic per concept type; a session of a few thousand concepts
/// takes seconds.
pub fn scan(g: &Graph, threshold: f64) -> Vec<ScannedPair> {
    let mut by_type: std::collections::BTreeMap<u8, Vec<&Concept>> = Default::default();
    for c in g.concepts() {
        if eligible_type(c.concept_type) && c.embedding_source.is_none() && c.embedding.is_some() {
            by_type.entry(c.concept_type as u8).or_default().push(c);
        }
    }
    let mut out = Vec::new();
    for concepts in by_type.values_mut() {
        concepts.sort_by_key(|c| c.id.0);
        for (i, a) in concepts.iter().enumerate() {
            let Some(va) = a.embedding.as_deref() else {
                continue;
            };
            for b in &concepts[i + 1..] {
                let Some(vb) = b.embedding.as_deref() else {
                    continue;
                };
                if va.len() != vb.len() {
                    continue;
                }
                let cosine = f64::from(crate::embed::cosine(va, vb));
                if !cosine.is_finite() || cosine < threshold {
                    continue;
                }
                if let Some(pair) = candidate(g, a, b) {
                    out.push(ScannedPair {
                        pair,
                        cosine,
                        recorded: g
                            .edge_between(pair.newer, pair.older, EdgeType::SuspectedSupersedes)
                            .is_some(),
                    });
                }
            }
        }
    }
    out.sort_by(|x, y| {
        y.cosine
            .total_cmp(&x.cosine)
            .then(x.pair.older.0.cmp(&y.pair.older.0))
            .then(x.pair.newer.0.cmp(&y.pair.newer.0))
    });
    out
}

#[cfg(test)]
mod tests;
