//! Citations and names on a derive (#36 PR 4).
//!
//! A derived concept may declare what it rests on (`depends_on`: existing
//! concepts it cites) and a short `name` later writes can cite it by. A
//! citation is written as `citing -Dependency-> cited`, the edge
//! `record_action`'s `depends_on` writes, so the cited concept gains a
//! dependent (#36's dependents relation) and with it blast radius and
//! canonization evidence.
//!
//! # Resolve or refuse, never create
//!
//! Every reference resolves through `crate::surface::resolve` against the
//! session the write targets: a full node id, a unique short id, a name or
//! the canonical key, to **exactly one existing concept**. Missing,
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
//! concept removed, renamed or shadowed between the ack and the apply never
//! becomes a dangling or silently redirected edge, and a durable intent
//! replayed after a restart reaches the same answer or settles `failed`.
//! The self and cycle checks run again against the written sources
//! ([`write_edges`]), because a hybrid write can resolve a concept
//! differently at commit than the pre-pass saw.
//!
//! # Names
//!
//! A name is registered with [`Graph::declare_synonym`]: later references
//! (here, and `record_action`'s `depends_on`, whose canonicalization reads
//! the same synonyms) resolve it to the named concept. A name that already
//! resolves to another concept is refused, never taken over: redirecting an
//! existing alias is the reviewed backfill's job (#36 PR 5). Synonyms are
//! RAM-local by the pinned S5 contract (no `Mutation` kind), so **a name
//! lasts until the writer restarts**; a citation by name after that refuses
//! as missing, never mis-resolves.
//!
//! # Idempotent by natural key
//!
//! Re-sending the same citation writes no second edge: the
//! `(citing, cited, Dependency)` natural key reinforces the one edge, and a
//! name declared again for the same concept is a no-op.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};

use crate::graph::canonical::{canonical_key, canonicalize, CanonicalizeResult};
use crate::graph::derive::{DeriveOutcome, ParentOf};
use crate::graph::Graph;
use crate::surface::resolve::{self, candidate, is_id_shaped, Resolved};
use crate::types::{
    ConceptType, Edge, EdgeType, LamboError, Node, NodeId, ReferenceField, ReferenceRefusalKind,
    SessionId,
};

/// At most this many `depends_on` references on one concept.
pub const MAX_REFERENCES_PER_CONCEPT: usize = 8;
/// At most this many `depends_on` references in one derive call.
pub const MAX_REFERENCES_PER_DERIVE: usize = 32;
/// The longest `name`, in bytes.
pub const MAX_NAME_BYTES: usize = 256;

/// Initial weight of a citation's `Dependency` edge: the weight
/// `record_action` gives its own `Dependency` edges.
const CITATION_WEIGHT: f64 = 0.5;

pub use crate::types::{ConceptCitations, PinnedCitations, PinnedReference};

fn refuse(
    field: ReferenceField,
    concept: &str,
    reference: &str,
    kind: ReferenceRefusalKind,
) -> LamboError {
    resolve::refusal(field, Some(concept), reference, kind)
}

/// Merge `citations` by content, in first-seen order: the wire carries them
/// per concept, and a concept listed twice is one concept.
fn merged(citations: &[ConceptCitations]) -> Result<Vec<ConceptCitations>, LamboError> {
    let mut out: Vec<ConceptCitations> = Vec::new();
    for c in citations {
        match out.iter_mut().find(|m| m.content == c.content) {
            Some(m) => {
                m.depends_on.extend(c.depends_on.iter().cloned());
                match (&m.name, &c.name) {
                    (Some(a), Some(b)) if a != b => {
                        return Err(refuse(
                            ReferenceField::Name,
                            &c.content,
                            b,
                            ReferenceRefusalKind::NameUnusable {
                                reason: "the same concept is given two different names in this \
                                         call",
                            },
                        ))
                    }
                    (None, Some(b)) => m.name = Some(b.clone()),
                    _ => {}
                }
            }
            None => out.push(c.clone()),
        }
    }
    Ok(out)
}

/// The derive's call-time pre-pass for citations: limits, resolution of
/// every reference against `g` (the target session's graph), self and cycle
/// checks against the concepts the call would match, and the name checks.
/// Returns what it accepted, to pin on the job. Read-only.
pub(crate) fn validate(
    g: &Graph,
    concepts: &[(&str, ConceptType)],
    parent_of: &ParentOf<'_>,
    citations: &[ConceptCitations],
) -> Result<PinnedCitations, LamboError> {
    if citations.is_empty() {
        return Ok(PinnedCitations::default());
    }
    let entries = merged(citations)?;
    let mut pinned = PinnedCitations::default();

    // Canonical key and current match of each of the call's contents (first
    // occurrence wins, as derive's dedup does), and the keys a name must not
    // collide with: the call's other concepts and its parent_of ends.
    let mut items: HashMap<&str, (ConceptType, String, Option<NodeId>)> = HashMap::new();
    for &(content, concept_type) in concepts {
        if items.contains_key(content) {
            continue;
        }
        let (key, matched) = match canonicalize(content, g)? {
            CanonicalizeResult::Matched { key, node } => (key, Some(node)),
            CanonicalizeResult::Unmatched { key } => (key, None),
        };
        items.insert(content, (concept_type, key, matched));
    }
    let end_keys: Vec<String> = parent_of
        .pairs()
        .iter()
        .flat_map(|(a, b)| [*a, *b])
        .map(|end| match canonicalize(end, g) {
            Ok(CanonicalizeResult::Matched { key, .. } | CanonicalizeResult::Unmatched { key }) => {
                key
            }
            Err(_) => String::new(),
        })
        .collect();

    let total: usize = entries.iter().map(|e| e.depends_on.len()).sum();
    let mut names_seen: HashSet<&str> = HashSet::new();
    for entry in &entries {
        let content = entry.content.as_str();
        let Some((concept_type, item_key, matched)) = items.get(content) else {
            let reference = entry
                .depends_on
                .first()
                .or(entry.name.as_ref())
                .map_or("", String::as_str);
            return Err(refuse(
                ReferenceField::DependsOn,
                content,
                reference,
                ReferenceRefusalKind::UnknownConcept,
            ));
        };
        if entry.depends_on.len() > MAX_REFERENCES_PER_CONCEPT {
            return Err(refuse(
                ReferenceField::DependsOn,
                content,
                &entry.depends_on[MAX_REFERENCES_PER_CONCEPT],
                ReferenceRefusalKind::Limit {
                    what: "too many depends_on references on one concept",
                    limit: MAX_REFERENCES_PER_CONCEPT,
                },
            ));
        }
        if total > MAX_REFERENCES_PER_DERIVE {
            return Err(refuse(
                ReferenceField::DependsOn,
                content,
                entry.depends_on.first().map_or("", String::as_str),
                ReferenceRefusalKind::Limit {
                    what: "too many depends_on references in one call",
                    limit: MAX_REFERENCES_PER_DERIVE,
                },
            ));
        }
        for reference in &entry.depends_on {
            let (target, _) =
                resolve::resolve_existing(g, ReferenceField::DependsOn, Some(content), reference)?;
            if *matched == Some(target) {
                return Err(refuse(
                    ReferenceField::DependsOn,
                    content,
                    reference,
                    ReferenceRefusalKind::SelfReference,
                ));
            }
            if !pinned
                .references
                .iter()
                .any(|p| p.content == content && p.target == target)
            {
                pinned.references.push(PinnedReference {
                    content: content.to_string(),
                    reference: reference.clone(),
                    target,
                });
            }
        }
        if let Some(name) = &entry.name {
            if !names_seen.insert(name.as_str()) {
                return Err(refuse(
                    ReferenceField::Name,
                    content,
                    name,
                    ReferenceRefusalKind::NameUnusable {
                        reason: "two concepts in this call are given it",
                    },
                ));
            }
            check_name_shape(content, *concept_type, name)?;
            let name_key = canonical_key(name, |_| None);
            let collides = items
                .iter()
                .filter(|(other, _)| **other != content)
                .any(|(_, (_, key, _))| *key == name_key)
                || (name_key != *item_key && end_keys.contains(&name_key));
            if collides {
                return Err(refuse(
                    ReferenceField::Name,
                    content,
                    name,
                    ReferenceRefusalKind::NameUnusable {
                        reason: "another concept in this call has the same words",
                    },
                ));
            }
            check_name_holder(g, content, name, *matched)?;
            pinned.names.push((content.to_string(), name.clone()));
        }
    }

    // A cycle the graph can already show: a cited concept that depends,
    // through `Causal`/`Dependency` edges, on a citing concept the call
    // matches. A concept the call creates has no edges yet.
    let planned: Vec<(NodeId, NodeId, EdgeType)> = pinned
        .references
        .iter()
        .filter_map(|p| {
            items
                .get(p.content.as_str())
                .and_then(|(_, _, m)| *m)
                .map(|source| (source, p.target, EdgeType::Dependency))
        })
        .collect();
    if let Some((source, target, _)) = crate::graph::action::closing_edge(g, &planned) {
        let p = pinned
            .references
            .iter()
            .find(|p| {
                p.target == target
                    && items.get(p.content.as_str()).and_then(|i| i.2) == Some(source)
            })
            .expect("the closing edge is one of the planned citations");
        return Err(refuse(
            ReferenceField::DependsOn,
            &p.content,
            &p.reference,
            ReferenceRefusalKind::Cycle,
        ));
    }
    Ok(pinned)
}

/// The checks on a name that need no graph.
fn check_name_shape(
    content: &str,
    concept_type: ConceptType,
    name: &str,
) -> Result<(), LamboError> {
    let unusable = |reason: &'static str| {
        Err(refuse(
            ReferenceField::Name,
            content,
            name,
            ReferenceRefusalKind::NameUnusable { reason },
        ))
    };
    if name.trim().is_empty() {
        return unusable("a name must not be empty");
    }
    if name.len() > MAX_NAME_BYTES {
        return Err(refuse(
            ReferenceField::Name,
            content,
            name,
            ReferenceRefusalKind::Limit {
                what: "the name is too long",
                limit: MAX_NAME_BYTES,
            },
        ));
    }
    if is_id_shaped(name) {
        return unusable("it reads as a node id or the short id recall shows");
    }
    if canonical_key(name, |_| None).is_empty() {
        return unusable("it has no words once stopwords are removed");
    }
    if concept_type == ConceptType::Observation {
        return unusable(
            "an Observation cannot be named: names resolve through canonical matching, which \
             never matches an Observation",
        );
    }
    Ok(())
}

/// A name must not already resolve to a concept other than `source` (the
/// concept it names, when that exists yet).
fn check_name_holder(
    g: &Graph,
    content: &str,
    name: &str,
    source: Option<NodeId>,
) -> Result<(), LamboError> {
    let holder = match resolve::resolve_reference(g, name) {
        Resolved::Concept { node, .. } if Some(node) == source => return Ok(()),
        Resolved::Missing => return Ok(()),
        Resolved::Concept { node, .. } => node,
        Resolved::Ambiguous(ids) => ids[0],
        Resolved::NotAConcept(id) => id,
    };
    Err(refuse(
        ReferenceField::Name,
        content,
        name,
        ReferenceRefusalKind::NameTaken {
            holder: candidate(g, holder),
        },
    ))
}

/// The commit's re-resolution, under the write lock and before the call's
/// own writes: every pinned reference must still name its pinned concept.
pub(crate) fn recheck(g: &Graph, pinned: &PinnedCitations) -> Result<(), LamboError> {
    for p in &pinned.references {
        let now = match resolve::resolve_reference(g, &p.reference) {
            Resolved::Concept { node, .. } if node == p.target => continue,
            Resolved::Concept { node, .. } => Some(node),
            Resolved::Missing | Resolved::Ambiguous(_) | Resolved::NotAConcept(_) => None,
        };
        return Err(refuse(
            ReferenceField::DependsOn,
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

/// When the citation edges are stamped: the interaction's about-time and
/// creation time, as every other edge the derive writes.
#[derive(Clone, Copy)]
pub(crate) struct Stamp {
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) event_time: Option<DateTime<Utc>>,
}

/// Write the citation edges, inside the derive's transaction and after its
/// concepts: `by_content` maps each of the call's contents to the node it
/// was written as. Refuses (and so rolls the whole write back) a citation
/// whose source turned out to be its target, or whose edge would close a
/// `Causal`/`Dependency` cycle with the graph as written so far. Then
/// checks every name against the written graph. Returns the
/// `(name, content)` synonyms to declare once the transaction has
/// committed ([`declare_names`]).
pub(crate) fn write_edges(
    g: &mut Graph,
    pinned: &PinnedCitations,
    by_content: &HashMap<String, NodeId>,
    stamp: Stamp,
    session_id: &SessionId,
    outcome: &mut DeriveOutcome,
) -> Result<Vec<(String, String)>, LamboError> {
    let source_of = |content: &str, reference: &str| {
        by_content.get(content).copied().ok_or_else(|| {
            refuse(
                ReferenceField::DependsOn,
                content,
                reference,
                ReferenceRefusalKind::UnknownConcept,
            )
        })
    };
    let mut planned: Vec<(NodeId, NodeId, EdgeType)> = Vec::new();
    let mut seen: HashSet<(NodeId, NodeId)> = HashSet::new();
    for p in &pinned.references {
        let source = source_of(&p.content, &p.reference)?;
        if source == p.target {
            return Err(refuse(
                ReferenceField::DependsOn,
                &p.content,
                &p.reference,
                ReferenceRefusalKind::SelfReference,
            ));
        }
        if seen.insert((source, p.target)) {
            planned.push((source, p.target, EdgeType::Dependency));
        }
    }
    if let Some((source, target, _)) = crate::graph::action::closing_edge(g, &planned) {
        let p = pinned
            .references
            .iter()
            .find(|p| p.target == target && by_content.get(p.content.as_str()) == Some(&source))
            .expect("the closing edge is one of the planned citations");
        return Err(refuse(
            ReferenceField::DependsOn,
            &p.content,
            &p.reference,
            ReferenceRefusalKind::Cycle,
        ));
    }
    for &(source, target, edge_type) in &planned {
        if g.edge_between(source, target, edge_type).is_some() {
            outcome.reinforced += 1;
        } else {
            outcome.cited += 1;
        }
        g.upsert_edge(Edge {
            event_time: stamp.event_time,
            id: NodeId::new(),
            session_id: session_id.clone(),
            source,
            target,
            edge_type,
            weight: CITATION_WEIGHT,
            reinforcements: 1,
            created_at: stamp.created_at,
            last_reinforced: stamp.created_at,
        })?;
    }

    let mut declare: Vec<(String, String)> = Vec::with_capacity(pinned.names.len());
    for (content, name) in &pinned.names {
        let source = source_of(content, name)?;
        let (concept_type, source_content) = match g.node(source) {
            Some(Node::Concept(c)) => (c.concept_type, c.content.clone()),
            _ => {
                return Err(refuse(
                    ReferenceField::Name,
                    content,
                    name,
                    ReferenceRefusalKind::UnknownConcept,
                ))
            }
        };
        // The written concept can be an existing one of another type (a
        // canonical match), so the shape check runs on what was written.
        check_name_shape(content, concept_type, name)?;
        check_name_holder(g, content, name, Some(source))?;
        declare.push((name.clone(), source_content));
    }
    Ok(declare)
}

/// Register the names [`write_edges`] accepted, after the transaction has
/// committed (synonyms are outside its undo log). Counts each new or changed
/// mapping in `outcome.named`.
pub(crate) fn declare_names(
    g: &mut Graph,
    names: &[(String, String)],
    outcome: &mut DeriveOutcome,
) {
    for (name, content) in names {
        if g.synonym(name.trim()) != Some(content.as_str()) {
            outcome.named += 1;
        }
        g.declare_synonym(name.trim(), content);
    }
}

#[cfg(test)]
mod tests;
