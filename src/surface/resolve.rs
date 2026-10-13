//! The shared, typed resolve-or-refuse resolver (#36 PR 4): what an existing
//! concept a **write** names, in the session the write targets.
//!
//! A derive's `depends_on`, a `record_action` `depends_on` entry that names
//! a concept by id, and #19's `contradicts` all call
//! [`resolve_reference`] against the graph of the session they write. It is
//! deliberately not `inspect`'s resolver ([`super::focus`]): a focus may
//! resolve by **substring** and tells the caller which concept it picked,
//! which is fine for a read and never acceptable for a write. Here a
//! reference resolves only by
//!
//! 1. a **full node id** in the hyphenated form recall, inspect and receipts
//!    print ([`is_hyphenated_id`]); this leg is exclusive, since content
//!    never has that shape;
//! 2. the **short id** recall renders: exactly [`SHORT_ID_CHARS`] (8) hex
//!    characters, a unique prefix of a concept's id (the prefix match
//!    `inspect` uses, [`short_id_matches`]). No other hex string is an id to
//!    a write (orchestrator decision, #36 PR 4 review): 9 to 32 hex
//!    characters, the unhyphenated simple form, braced and `urn:uuid:`
//!    spellings are text, so the surface that changed from 0.5.0 stays as
//!    small as possible, and agents only ever see 8-character ids.
//!    `inspect`'s read-only focus keeps its wider prefix leg (8 to 32, its
//!    own check in `surface::focus`);
//! 3. its **canonical key**: the reference's key as derive computes it, so
//!    composed and decomposed spellings, case and word order reach the same
//!    concept. Normalization is `crate::graph::canonical`'s (NFC,
//!    invisible-character stripping, lowercasing, stemming); this module adds
//!    none of its own. **No synonym is consulted**: a synonym is in-memory
//!    session state (pinned contract S5), so a reference resolved through one
//!    could mean something else after a restart. Nothing a write declares can
//!    redirect a reference.
//!
//! Legs 2 and 3 are both asked, and the distinct concepts they find decide:
//! none is [`Resolved::Missing`], one is [`Resolved::Concept`], more is
//! [`Resolved::Ambiguous`]. A key two concepts hold would be ambiguous too,
//! where canonicalization picks the lowest id; the graph's own invariants
//! refuse such a pair (`Graph::from_snapshot` included), so that arm is
//! defence in depth. Nothing here creates a concept, and nothing is fuzzy.
//!
//! The resolver reads one graph: the **target** session's. A reference that
//! resolves only in another session the server reads (a layer such as
//! `general`, #33) is refused as foreign by [`attribute_foreign`], given a
//! [`ForeignScopes`] that can look there. #33 PR 4's scoped writes pass the
//! layer's graph as the target and the addressed session plus the other
//! layers as the foreign scopes; nothing else changes.

use super::limits::MAX_INSPECT_CANDIDATES;
use crate::graph::canonical::canonical_key;
use crate::graph::Graph;
use crate::types::{
    shown, tie_break_by_key, LamboError, Node, NodeId, ReferenceCandidate, ReferenceField,
    ReferenceRefusal, ReferenceRefusalKind,
};

pub(crate) use crate::recall::format::SHORT_ID_CHARS;

/// Which leg resolved a reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Leg {
    /// A full node id.
    NodeId,
    /// A unique short-id prefix.
    ShortId,
    /// The canonical key.
    Key,
}

/// What a reference names in one graph.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Resolved {
    /// Exactly one concept.
    Concept { node: NodeId, leg: Leg },
    /// Nothing.
    Missing,
    /// Several concepts, in the stable tie order (canonical key, then id).
    Ambiguous(Vec<NodeId>),
    /// A full node id naming an interaction.
    NotAConcept(NodeId),
}

/// `reference` as a full node id, when it parses as one.
pub(crate) fn parse_full_id(reference: &str) -> Option<NodeId> {
    uuid::Uuid::parse_str(reference.trim()).ok().map(NodeId)
}

/// Whether `reference` is a short id to a **write**: exactly
/// [`SHORT_ID_CHARS`] ASCII hex characters, what recall prints. Inspect's
/// read-only focus accepts longer prefixes with its own check.
pub(crate) fn is_short_id_shaped(reference: &str) -> bool {
    let b = reference.trim().as_bytes();
    b.len() == SHORT_ID_CHARS && b.iter().all(u8::is_ascii_hexdigit)
}

/// Whether `reference` is a node id in its canonical hyphenated form
/// (`8-4-4-4-12` hex digits, either case), the form recall, inspect and
/// receipts print. `record_action` treats only this form as a full id
/// (#36 PR 4 review M1): `urn:uuid:`, braced and 32-hex spellings are text
/// there, as in 0.5.0.
pub(crate) fn is_hyphenated_id(reference: &str) -> bool {
    let b = reference.trim().as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Whether `reference` reads as an id to `record_action`: the hyphenated
/// form, or a short id's shape. Used to decide whether a request carries
/// pins at all.
pub(crate) fn is_id_shaped(reference: &str) -> bool {
    is_hyphenated_id(reference) || is_short_id_shaped(reference)
}

/// Whether `id`'s simple hex form starts with `prefix` (ASCII hex, either
/// case, at most 32 characters). Allocation-free, so a scan over every
/// concept stays O(concepts) nibble compares.
pub(crate) fn id_has_hex_prefix(id: uuid::Uuid, prefix: &str) -> bool {
    let bytes = id.as_bytes();
    for (i, ch) in prefix.bytes().enumerate() {
        if i >= 32 {
            return false;
        }
        let nibble = (ch as char).to_digit(16).unwrap_or(0) as u8;
        let byte = bytes[i / 2];
        let target = if i % 2 == 0 { byte >> 4 } else { byte & 0x0f };
        if nibble != target {
            return false;
        }
    }
    true
}

/// The concepts whose id starts with `prefix` (already short-id shaped), in
/// the stable tie order. Shared with `inspect`'s short-id leg, so a rendered
/// id resolves the same way for a read and for a write.
pub(crate) fn short_id_matches(g: &Graph, prefix: &str) -> Vec<NodeId> {
    let prefix = prefix.trim();
    let mut found: Vec<&crate::types::Concept> = g
        .concepts()
        .filter(|c| id_has_hex_prefix(c.id.0, prefix))
        .collect();
    found.sort_by(|a, b| {
        tie_break_by_key(Some(&a.canonical_key), &a.id, Some(&b.canonical_key), &b.id)
    });
    found.into_iter().map(|c| c.id).collect()
}

/// Resolve `reference` against `g`, the target session's graph. See the
/// module docs for the legs. Read-only; the caller holds the graph's lock
/// (read for a pre-pass, write at a commit) and no other.
pub(crate) fn resolve_reference(g: &Graph, reference: &str) -> Resolved {
    let reference = reference.trim();
    if let Some(id) = is_hyphenated_id(reference)
        .then(|| parse_full_id(reference))
        .flatten()
    {
        return match g.node(id) {
            Some(Node::Concept(_)) => Resolved::Concept {
                node: id,
                leg: Leg::NodeId,
            },
            Some(Node::Interaction(_)) => Resolved::NotAConcept(id),
            None => Resolved::Missing,
        };
    }
    // (node, leg) per distinct concept, in leg order.
    let mut found: Vec<(NodeId, Leg)> = Vec::new();
    let mut push = |node: NodeId, leg: Leg| {
        if !found.iter().any(|(n, _)| *n == node) {
            found.push((node, leg));
        }
    };
    if is_short_id_shaped(reference) {
        for node in short_id_matches(g, reference) {
            push(node, Leg::ShortId);
        }
    }
    // The key without the synonym lookup (see the module docs).
    let key = canonical_key(reference, |_| None);
    if !key.is_empty() {
        for &node in g.concepts_by_key(&key) {
            push(node, Leg::Key);
        }
    }
    match found.len() {
        0 => Resolved::Missing,
        1 => Resolved::Concept {
            node: found[0].0,
            leg: found[0].1,
        },
        _ => {
            let mut ids: Vec<NodeId> = found.into_iter().map(|(n, _)| n).collect();
            let key_of = |id: NodeId| match g.node(id) {
                Some(Node::Concept(c)) => Some(c.canonical_key.clone()),
                _ => None,
            };
            ids.sort_by(|a, b| {
                tie_break_by_key(key_of(*a).as_deref(), a, key_of(*b).as_deref(), b)
            });
            Resolved::Ambiguous(ids)
        }
    }
}

/// `ids` as refusal candidates (id and content snippet), at most
/// [`MAX_INSPECT_CANDIDATES`] of them, in the order given.
pub(crate) fn candidates(g: &Graph, ids: &[NodeId]) -> Vec<ReferenceCandidate> {
    ids.iter()
        .take(MAX_INSPECT_CANDIDATES)
        .map(|&node| candidate(g, node))
        .collect()
}

/// One concept as a refusal candidate.
pub(crate) fn candidate(g: &Graph, node: NodeId) -> ReferenceCandidate {
    let content = match g.node(node) {
        Some(Node::Concept(c)) => shown(&c.content),
        _ => String::new(),
    };
    ReferenceCandidate { node, content }
}

/// Resolve `reference` to exactly one existing concept of `g`, or refuse it
/// as `field` of `concept` with the typed reason.
pub(crate) fn resolve_existing(
    g: &Graph,
    field: ReferenceField,
    concept: Option<&str>,
    reference: &str,
) -> Result<(NodeId, Leg), LamboError> {
    let kind = match resolve_reference(g, reference) {
        Resolved::Concept { node, leg } => return Ok((node, leg)),
        Resolved::Missing => ReferenceRefusalKind::Missing,
        Resolved::Ambiguous(ids) => ReferenceRefusalKind::Ambiguous {
            candidates: candidates(g, &ids),
        },
        Resolved::NotAConcept(_) => ReferenceRefusalKind::NotAConcept,
    };
    Err(refusal(field, concept, reference, kind))
}

/// A [`LamboError::Reference`] for `reference`.
pub(crate) fn refusal(
    field: ReferenceField,
    concept: Option<&str>,
    reference: &str,
    kind: ReferenceRefusalKind,
) -> LamboError {
    LamboError::Reference(Box::new(ReferenceRefusal::new(
        field, concept, reference, kind,
    )))
}

/// The other sessions a server reads beside the target, so a reference that
/// names nothing in the target but something there can be refused by name
/// ("is in scope 'general'") instead of as missing.
///
/// An implementation reads each scope's graph under that graph's own read
/// lock, one at a time, holding no other session's lock and never across an
/// `.await`. It answers only "which scope holds it"; nothing it finds is
/// ever cited.
pub(crate) trait ForeignScopes: Sync {
    /// The first scope, in tier order, in which `reference` resolves to
    /// exactly one concept.
    fn holder_of(&self, reference: &str) -> Option<String>;
}

/// No other scope: the unlayered default.
pub(crate) struct NoForeignScopes;

impl ForeignScopes for NoForeignScopes {
    fn holder_of(&self, _reference: &str) -> Option<String> {
        None
    }
}

/// `err` with a missing reference that `foreign` finds in another scope
/// re-labelled as foreign; any other error unchanged.
pub(crate) fn attribute_foreign(err: LamboError, foreign: &dyn ForeignScopes) -> LamboError {
    match err {
        LamboError::Reference(mut r) if r.kind == ReferenceRefusalKind::Missing => {
            if let Some(raw) = r.raw.as_deref()
                && let Some(scope) = foreign.holder_of(raw)
            {
                r.kind = ReferenceRefusalKind::ForeignScope { scope };
            }
            LamboError::Reference(r)
        }
        other => other,
    }
}

#[cfg(test)]
mod tests;
