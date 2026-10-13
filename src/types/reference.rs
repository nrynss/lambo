//! The typed refusal of a reference to an existing concept (#36 PR 4).
//!
//! A derive's `depends_on`, a `record_action` `depends_on` entry that
//! names a concept by id, and an `against` entry on either (#19(b)) must
//! resolve to exactly one existing concept in the session the write
//! targets. When one does not, the write is
//! refused whole with a [`ReferenceRefusal`]: what was sent, where, and why,
//! worded so the caller can fix it. The resolver that produces these lives in
//! `crate::surface::resolve`.
//!
//! The message is the caller's own reference (shortened and escaped) plus
//! session-local ids and content snippets the caller can already read through
//! recall and inspect, so every surface shows it intact
//! (`crate::surface::error`), and a replayed intent that meets one settles
//! `failed`: it is a fact about this input against this graph.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::NodeId;

/// Which request field a [`ReferenceRefusal`] is about.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReferenceField {
    /// A derive concept's `depends_on` entry.
    DependsOn,
    /// A `record_action` `depends_on` entry.
    ActionDependsOn,
    /// A derive concept's `against` entry (#19(b)): a concept it contradicts.
    Against,
    /// A `record_action` `against` entry (#19(b)).
    ActionAgainst,
}

impl ReferenceField {
    /// The field as the wire spells it.
    pub fn wire_name(self) -> &'static str {
        match self {
            ReferenceField::DependsOn => "depends_on",
            ReferenceField::ActionDependsOn => "depends_on",
            ReferenceField::Against | ReferenceField::ActionAgainst => "against",
        }
    }

    /// Whether this field records a contradiction rather than a citation.
    fn is_against(self) -> bool {
        matches!(
            self,
            ReferenceField::Against | ReferenceField::ActionAgainst
        )
    }

    /// What one reference of this field records, in the plural.
    fn records(self) -> &'static str {
        if self.is_against() {
            "contradictions"
        } else {
            "citations"
        }
    }
}

/// One concept a reference could have meant: its id and a content snippet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReferenceCandidate {
    /// The concept.
    pub node: NodeId,
    /// Its content, shortened for display.
    pub content: String,
}

/// Why a reference was refused.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReferenceRefusalKind {
    /// No concept in this session answers to it.
    Missing,
    /// It answers to several concepts.
    Ambiguous {
        /// The concepts it matched, in a stable order.
        candidates: Vec<ReferenceCandidate>,
    },
    /// A full node id that names an interaction, not a concept.
    NotAConcept,
    /// It names nothing in this session but resolves in another session this
    /// server reads (a layer such as `general`). Dependence and contradiction
    /// are per session, so the reference is not recorded.
    ForeignScope {
        /// The session that holds it.
        scope: String,
    },
    /// The citing concept would cite (or contradict) itself.
    SelfReference,
    /// The `Dependency` edge would close a cycle: the cited concept already
    /// depends, directly or through others, on the citing one.
    Cycle,
    /// It resolved to `accepted` when the write was acknowledged and resolves
    /// to `now` (or to nothing) when the write is applied: the cited concept
    /// was removed or shadowed in between.
    Changed {
        /// What it named at the ack (`None`: it named nothing and was to be
        /// created, for a `record_action` entry).
        accepted: Option<NodeId>,
        /// What it names now.
        now: Option<NodeId>,
    },
    /// Too many references, or one too long.
    Limit {
        /// What was over, in words.
        what: &'static str,
        /// The limit.
        limit: usize,
    },
    /// The citation names a concept the call does not derive (a library
    /// caller's mistake; the MCP and CLI surfaces build them per concept).
    UnknownConcept,
}

/// A refused reference: the field, the citing concept, the reference as sent
/// and why. Built by `crate::surface::resolve` and the citation write path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReferenceRefusal {
    /// The field the reference came from.
    pub field: ReferenceField,
    /// The citing concept's content (derive) or the action (record_action),
    /// shortened for display; `None` when it does not apply.
    pub concept: Option<String>,
    /// The reference as sent, shortened for display.
    pub reference: String,
    /// Why it was refused.
    pub kind: ReferenceRefusalKind,
    /// The reference exactly as sent, for the foreign-scope lookup
    /// (`crate::surface::resolve::attribute_foreign`). Never displayed.
    pub(crate) raw: Option<String>,
}

/// The longest reference or content snippet a refusal repeats, in chars.
const SHOWN_CHARS: usize = 80;

/// `text` shortened to [`SHOWN_CHARS`] chars, as a debug-escaped quoted string
/// so a control character can never reach the reader raw.
pub(crate) fn shown(text: &str) -> String {
    let mut cut: String = text.chars().take(SHOWN_CHARS).collect();
    if cut.len() < text.len() {
        cut.push('…');
    }
    format!("{cut:?}")
}

/// The 8-hex short form recall renders (`recall::format::short_id`).
fn short(node: NodeId) -> String {
    node.0.simple().to_string()[..8].to_string()
}

fn candidate(c: &ReferenceCandidate) -> String {
    format!("{} {}", short(c.node), c.content)
}

impl ReferenceRefusal {
    /// A refusal of `reference` in `field`.
    pub(crate) fn new(
        field: ReferenceField,
        concept: Option<&str>,
        reference: &str,
        kind: ReferenceRefusalKind,
    ) -> Self {
        Self {
            field,
            concept: concept.map(shown),
            reference: shown(reference),
            kind,
            raw: Some(reference.to_string()),
        }
    }
}

impl fmt::Display for ReferenceRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let field = self.field.wire_name();
        let on = match &self.concept {
            Some(c) => format!(" on {c}"),
            None => String::new(),
        };
        let r = &self.reference;
        match &self.kind {
            ReferenceRefusalKind::Missing => write!(
                f,
                "{field} {r}{on} names no concept in this session; {} a concept that already \
                 exists, by the id recall shows (8 hex characters), its full node id or its exact \
                 content",
                if self.field.is_against() {
                    "name"
                } else {
                    "cite"
                }
            ),
            ReferenceRefusalKind::Ambiguous { candidates } => write!(
                f,
                "{field} {r}{on} matches {} concepts: {}; cite one by its full node id",
                candidates.len(),
                candidates
                    .iter()
                    .map(candidate)
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            ReferenceRefusalKind::NotAConcept => write!(
                f,
                "{field} {r}{on} is the id of an interaction, not a concept; cite a concept"
            ),
            ReferenceRefusalKind::ForeignScope { scope } if self.field.is_against() => write!(
                f,
                "{field} {r}{on} is in scope {scope:?}; cross-session {} are not recorded. \
                 Contradict a concept in this session; to challenge that one, write the \
                 assertion in scope {scope:?} itself",
                self.field.records()
            ),
            ReferenceRefusalKind::ForeignScope { scope } => write!(
                f,
                "{field} {r}{on} is in scope {scope:?}; cross-session {} are not recorded. \
                 Cite a concept in this session",
                self.field.records()
            ),
            ReferenceRefusalKind::SelfReference if self.field.is_against() => {
                write!(f, "{field} {r}{on} names the contradicting concept itself")
            }
            ReferenceRefusalKind::SelfReference => {
                write!(f, "{field} {r}{on} names the citing concept itself")
            }
            ReferenceRefusalKind::Cycle => write!(
                f,
                "{field} {r}{on} would close a Dependency cycle: the cited concept already \
                 depends on the citing one"
            ),
            ReferenceRefusalKind::Changed { accepted, now } => {
                let then = accepted.map_or("nothing".to_string(), short);
                let now = now.map_or("nothing".to_string(), short);
                write!(
                    f,
                    "{field} {r}{on} named {then} when the write was accepted and names {now} \
                     now (the concept was removed or shadowed in between); check the \
                     reference and resend"
                )
            }
            ReferenceRefusalKind::Limit { what, limit } => {
                write!(f, "{field} {r}{on}: {what} (the limit is {limit})")
            }
            ReferenceRefusalKind::UnknownConcept => write!(
                f,
                "{field} {r}{on}: the citing concept is not one of this call's concepts"
            ),
        }?;
        f.write_str("; nothing was written")
    }
}

impl std::error::Error for ReferenceRefusal {}

// ---------------------------------------------------------------------------
// Citations on a derive (#36 PR 4): the request and what the ack accepted.
// The write path is `crate::graph::cite`.
// ---------------------------------------------------------------------------

/// One derived concept's references, as the caller sent them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConceptCitations {
    /// The citing concept's content: one of the call's `concepts`.
    pub content: String,
    /// Existing concepts it rests on, each by full node id, recall's short
    /// id or its content.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    /// Existing concepts it contradicts (#19(b)), by the same forms: each is
    /// written as `this -Contradicts-> that`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub against: Vec<String>,
}

/// A citation as accepted at the ack: the reference and what it resolved to.
///
/// `deny_unknown_fields`, here and on [`PinnedCitations`]: a durable intent
/// written by a later build with a field this one does not know is refused
/// at decode rather than applied with that field silently dropped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedReference {
    /// The citing concept's content.
    pub content: String,
    /// The reference as sent.
    pub reference: String,
    /// The concept it named at the ack.
    pub target: NodeId,
}

/// A derive's citations and contradictions as accepted at the ack
/// (`crate::graph::cite::validate`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedCitations {
    /// One per distinct `(content, target)`: `depends_on`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<PinnedReference>,
    /// One per distinct `(content, target)`: `against` (#19(b)). Skipped
    /// when empty, so a derive that only cites serializes as before.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub against: Vec<PinnedReference>,
}

impl PinnedCitations {
    /// Whether there is nothing to cite or contradict.
    pub fn is_empty(&self) -> bool {
        self.references.is_empty() && self.against.is_empty()
    }

    /// Retained bytes, for the write queue's byte admission.
    pub(crate) fn bytes(&self) -> usize {
        pinned_bytes(&self.references) + pinned_bytes(&self.against)
    }
}

/// Retained bytes of pinned references, for the write queue's byte
/// admission.
pub(crate) fn pinned_bytes(pinned: &[PinnedReference]) -> usize {
    pinned
        .iter()
        .map(|r| r.content.len() + r.reference.len() + 16)
        .sum::<usize>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn shown_escapes_and_shortens() {
        assert_eq!(shown("a\u{1}b"), "\"a\\u{1}b\"");
        let long = "x".repeat(200);
        let s = shown(&long);
        assert!(s.chars().count() < 90, "{s}");
        assert!(s.contains('…'));
    }

    #[test]
    fn every_kind_says_nothing_was_written_and_names_the_reference() {
        let node = NodeId(Uuid::from_u128(0x8baad4e0_0000_0000_0000_000000000001));
        let cand = ReferenceCandidate {
            node,
            content: "\"lease stays\"".into(),
        };
        let kinds = [
            ReferenceRefusalKind::Missing,
            ReferenceRefusalKind::Ambiguous {
                candidates: vec![cand.clone(), cand],
            },
            ReferenceRefusalKind::NotAConcept,
            ReferenceRefusalKind::ForeignScope {
                scope: "general".into(),
            },
            ReferenceRefusalKind::SelfReference,
            ReferenceRefusalKind::Cycle,
            ReferenceRefusalKind::Changed {
                accepted: Some(node),
                now: None,
            },
            ReferenceRefusalKind::Limit {
                what: "too many",
                limit: 8,
            },
            ReferenceRefusalKind::UnknownConcept,
        ];
        for kind in kinds {
            let r = ReferenceRefusal::new(
                ReferenceField::DependsOn,
                Some("a decision"),
                "lease stays",
                kind,
            );
            let msg = r.to_string();
            assert!(msg.ends_with("; nothing was written"), "{msg}");
            assert!(msg.contains("\"lease stays\""), "{msg}");
        }
        // #19(b): an `against` refusal names its field and its noun.
        let against = ReferenceRefusal::new(
            ReferenceField::Against,
            Some("a newer claim"),
            "an old belief",
            ReferenceRefusalKind::Missing,
        );
        assert!(against.to_string().starts_with(
            "against \"an old belief\" on \"a newer claim\" names no concept in this session; \
             name a concept"
        ));
        let against = ReferenceRefusal::new(
            ReferenceField::ActionAgainst,
            Some("ran it"),
            "8baad4e0",
            ReferenceRefusalKind::ForeignScope {
                scope: "general".into(),
            },
        );
        assert!(against.to_string().contains(
            "is in scope \"general\"; cross-session contradictions are not recorded. \
             Contradict a concept in this session; to challenge that one, write the assertion \
             in scope \"general\" itself"
        ));
        let foreign = ReferenceRefusal::new(
            ReferenceField::DependsOn,
            None,
            "8baad4e0",
            ReferenceRefusalKind::ForeignScope {
                scope: "general".into(),
            },
        );
        assert!(foreign
            .to_string()
            .contains("is in scope \"general\"; cross-session citations are not recorded"));
    }
}
