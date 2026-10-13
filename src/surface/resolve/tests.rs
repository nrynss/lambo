//! The strict resolver (#36 PR 4): every leg, every refusal, and that
//! `inspect`'s fuzzy leg can never authorize a write.

use super::*;
use crate::surface::focus::{resolve_focus, Focus};
use crate::test_util::citations::{concept, concept_with_id, graph, id_with_prefix};
use crate::types::ConceptType;

const RULING: &str = "the writer lease stays with its holder until the holder releases it";

fn short(id: NodeId) -> String {
    crate::recall::format::short_id(id)
}

/// The design's first PR 4 test: an id, a short id and the content
/// (in another case, word order and Unicode composition) all reach the same
/// node.
#[test]
fn every_leg_reaches_the_same_concept() {
    let (mut g, i) = graph("resolve-legs");
    let ruling = concept(&mut g, i, RULING, ConceptType::Logic);
    let cafe = concept(&mut g, i, "caf\u{e9} opening rule", ConceptType::Constraint);
    g.declare_synonym("lease rule", RULING);

    let full = ruling.0.to_string();
    let simple = ruling.0.simple().to_string();
    let upper_short = short(ruling).to_uppercase();
    for reference in [
        full.as_str(),
        simple.as_str(),
        &short(ruling),
        &upper_short,
        &simple[..12],
        RULING,
        "  THE Writer Lease stays with its holder until the holder releases it ",
        "lease writer the stays with its holder until the holder releases it",
    ] {
        assert_eq!(
            resolve_reference(&g, reference),
            Resolved::Concept {
                node: ruling,
                leg: match reference {
                    r if r == full || r == simple => Leg::NodeId,
                    r if is_short_id_shaped(r) => Leg::ShortId,
                    _ => Leg::Key,
                },
            },
            "{reference:?}"
        );
    }
    // A synonym the session holds is not a reference (review H1): it is
    // in-memory state, so it could mean something else after a restart.
    assert_eq!(resolve_reference(&g, "lease rule"), Resolved::Missing);
    // Composed and decomposed spellings are one key (graph::canonical's NFC).
    assert_eq!(
        resolve_reference(&g, "cafe\u{301} opening rule"),
        Resolved::Concept {
            node: cafe,
            leg: Leg::Key
        }
    );
}

#[test]
fn nothing_resolves_to_missing_and_an_interaction_id_is_not_a_concept() {
    let (mut g, i) = graph("resolve-missing");
    concept(&mut g, i, RULING, ConceptType::Logic);
    for reference in [
        "no such concept",
        "deadbeefdeadbeef",
        &NodeId::new().0.to_string(),
        // Stopwords only: no key, so nothing to match.
        "the of and",
        "",
        "   ",
    ] {
        assert_eq!(
            resolve_reference(&g, reference),
            Resolved::Missing,
            "{reference:?}"
        );
    }
    assert_eq!(
        resolve_reference(&g, &i.0.to_string()),
        Resolved::NotAConcept(i)
    );
    // Shorter than recall's 8 characters is not an id: it is text.
    let ruling = resolve_reference(&g, RULING);
    let Resolved::Concept { node, .. } = ruling else {
        panic!("{ruling:?}")
    };
    assert_eq!(resolve_reference(&g, &short(node)[..7]), Resolved::Missing);
}

#[test]
fn a_shared_prefix_is_ambiguous_until_it_is_long_enough() {
    let (mut g, i) = graph("resolve-ambiguous");
    let a = concept_with_id(
        &mut g,
        i,
        id_with_prefix("8baad4e0aa", 1),
        "first",
        ConceptType::Logic,
    );
    let b = concept_with_id(
        &mut g,
        i,
        id_with_prefix("8baad4e0bb", 2),
        "second",
        ConceptType::Logic,
    );
    match resolve_reference(&g, "8baad4e0") {
        Resolved::Ambiguous(ids) => {
            assert_eq!(ids.len(), 2);
            assert!(ids.contains(&a) && ids.contains(&b));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        resolve_reference(&g, "8baad4e0aa"),
        Resolved::Concept {
            node: a,
            leg: Leg::ShortId
        }
    );
    let err = resolve_existing(&g, ReferenceField::DependsOn, Some("x"), "8baad4e0").unwrap_err();
    let LamboError::Reference(r) = &err else {
        panic!("{err:?}")
    };
    let ReferenceRefusalKind::Ambiguous { candidates } = &r.kind else {
        panic!("{r:?}")
    };
    assert_eq!(candidates.len(), 2);
    let msg = err.to_string();
    assert!(
        msg.contains("matches 2 concepts") && msg.contains("\"first\""),
        "{msg}"
    );
}

/// A hex string that is one concept's short id and another's content is
/// ambiguous: the legs disagree, so neither wins.
#[test]
fn legs_that_disagree_are_ambiguous() {
    let (mut g, i) = graph("resolve-disagree");
    let by_id = concept_with_id(
        &mut g,
        i,
        id_with_prefix("cafebabe", 1),
        "an id holder",
        ConceptType::Entity,
    );
    let by_text = concept(&mut g, i, "cafebabe", ConceptType::Entity);
    match resolve_reference(&g, "cafebabe") {
        Resolved::Ambiguous(ids) => {
            assert!(ids.contains(&by_id) && ids.contains(&by_text), "{ids:?}");
        }
        other => panic!("{other:?}"),
    }
}

/// An Observation is reachable by id but never by its text: canonical
/// matching never matches one (GRAPH-1), and neither does this.
#[test]
fn an_observation_is_reachable_by_id_only() {
    let (mut g, i) = graph("resolve-obs");
    let obs = concept(&mut g, i, "a noticed thing", ConceptType::Observation);
    assert_eq!(resolve_reference(&g, "a noticed thing"), Resolved::Missing);
    assert_eq!(
        resolve_reference(&g, &short(obs)),
        Resolved::Concept {
            node: obs,
            leg: Leg::ShortId
        }
    );
}

/// The acceptance case for "inspect's fuzzy leg must not authorize a
/// write": a focus `inspect` resolves by substring resolves to nothing
/// here, and a citation by it is refused.
#[test]
fn a_substring_inspect_resolves_never_authorizes_a_write() {
    let (mut g, i) = graph("resolve-fuzzy");
    let ruling = concept(&mut g, i, RULING, ConceptType::Logic);
    for focus in [
        "writer lease stays",
        "lease stays with its holder",
        "until the holder",
    ] {
        match resolve_focus(&g, focus) {
            Focus::Fuzzy { id, .. } => assert_eq!(id, ruling),
            other => panic!("inspect should resolve {focus:?} by substring: {other:?}"),
        }
        assert_eq!(resolve_reference(&g, focus), Resolved::Missing, "{focus:?}");
        let err = resolve_existing(&g, ReferenceField::DependsOn, Some("d"), focus).unwrap_err();
        assert!(
            matches!(&err, LamboError::Reference(r) if r.kind == ReferenceRefusalKind::Missing),
            "{err:?}"
        );
    }
}

/// `inspect` and a write resolve a rendered short id the same way: one
/// prefix matcher.
#[test]
fn inspect_and_writes_share_the_short_id_leg() {
    let (mut g, i) = graph("resolve-shared");
    let a = concept_with_id(
        &mut g,
        i,
        id_with_prefix("abcdef01", 1),
        "alpha",
        ConceptType::Entity,
    );
    concept_with_id(
        &mut g,
        i,
        id_with_prefix("abcdef02", 2),
        "beta",
        ConceptType::Entity,
    );
    match resolve_focus(&g, "abcdef01") {
        Focus::Exact(id) => assert_eq!(id, a),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        resolve_focus(&g, "abcdef0"),
        Focus::Fuzzy { .. } | Focus::Missing { .. } | Focus::Ambiguous { .. }
    ));
    assert_eq!(short_id_matches(&g, "ABCDEF01"), vec![a]);
}

struct HoldsIn(&'static str, &'static str);

impl ForeignScopes for HoldsIn {
    fn holder_of(&self, reference: &str) -> Option<String> {
        (reference == self.0).then(|| self.1.to_string())
    }
}

/// A reference missing here but held by another scope is refused by name;
/// any other refusal, and any reference the other scope lacks, is left as
/// it was.
#[test]
fn a_reference_held_elsewhere_is_refused_as_foreign() {
    let (g, _) = graph("resolve-foreign");
    let probe = HoldsIn("8baad4e0", "general");
    let missing = resolve_existing(&g, ReferenceField::DependsOn, None, "8baad4e0").unwrap_err();
    let foreign = attribute_foreign(missing, &probe);
    let msg = foreign.to_string();
    assert!(
        msg.contains("is in scope \"general\"; cross-session citations are not recorded"),
        "{msg}"
    );
    let other = resolve_existing(&g, ReferenceField::DependsOn, None, "elsewhere").unwrap_err();
    assert!(matches!(
        attribute_foreign(other, &probe),
        LamboError::Reference(r) if r.kind == ReferenceRefusalKind::Missing
    ));
    assert!(matches!(
        attribute_foreign(LamboError::Config("x".into()), &probe),
        LamboError::Config(_)
    ));
}

#[test]
fn id_shapes() {
    assert!(is_id_shaped("8baad4e0"));
    assert!(is_id_shaped(&NodeId::new().0.to_string()));
    assert!(!is_id_shaped("8baad4e"));
    assert!(!is_id_shaped("8baad4eg"));
    assert!(!is_id_shaped("writer lease"));
    assert!(id_has_hex_prefix(
        uuid::Uuid::from_u128(0xABCD_0000_0000_0000_0000_0000_0000_0000),
        "abcd"
    ));
}
