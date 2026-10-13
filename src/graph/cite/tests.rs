//! Citations on the canonical derive path and on
//! `record_action` (#36 PR 4). The write-queue, hybrid, replay and MCP
//! halves are in `memory::tests::citations` and `mcp::server::tests`.

use super::*;
use crate::graph::action::{record_action, record_action_pinned, validate_pinned, Action};
use crate::graph::derive::{derive_cited, ParentOf};
use crate::recall::format;
use crate::test_util::citations::{concept, concept_with_id, graph, id_with_prefix};
use crate::types::{AgentId, ReferenceRefusal};

const RULING: &str = "the writer lease stays with its holder until the holder releases it";
const DECISION: &str = "flush retries keep the lease instead of re-acquiring it";

fn agent() -> AgentId {
    AgentId::from("agent-a")
}

fn cite(content: &str, depends_on: &[&str]) -> ConceptCitations {
    ConceptCitations {
        content: content.to_string(),
        depends_on: depends_on.iter().map(|s| s.to_string()).collect(),
        against: Vec::new(),
    }
}

/// Validate then derive, the way `Memory`'s synchronous path does.
fn derive_with_citations(
    g: &mut Graph,
    i: NodeId,
    concepts: &[(&str, ConceptType)],
    citations: &[ConceptCitations],
) -> Result<DeriveOutcome, LamboError> {
    let pinned = validate(g, concepts, citations)?;
    derive_cited(g, i, &agent(), concepts, &ParentOf::none(), 10, &pinned)
}

fn refusal(err: &LamboError) -> &ReferenceRefusal {
    match err {
        LamboError::Reference(r) => r,
        other => panic!("expected a reference refusal, got {other:?}"),
    }
}

fn node_of(g: &Graph, content: &str) -> NodeId {
    g.concepts()
        .find(|c| c.content == content)
        .map(|c| c.id)
        .unwrap_or_else(|| panic!("no concept {content:?}"))
}

/// A citation writes `citing -Dependency-> cited`, and the cited concept
/// gains the citing one as a dependent (PR 1's relation): blast radius 1.
#[test]
fn a_citation_makes_the_cited_concept_load_bearing() {
    let (mut g, i) = graph("cite-basic");
    let ruling = concept(&mut g, i, RULING, ConceptType::Logic);
    assert_eq!(format::blast_radius(&g, ruling), 0);
    let out = derive_with_citations(
        &mut g,
        i,
        &[(DECISION, ConceptType::Logic)],
        &[cite(DECISION, &[&format::short_id(ruling)])],
    )
    .unwrap();
    assert_eq!(out.cited, 1);
    let decision = node_of(&g, DECISION);
    let edge = g
        .edge_between(decision, ruling, EdgeType::Dependency)
        .expect("the citation edge");
    assert_eq!(edge.reinforcements, 1);
    assert_eq!(format::blast_radius(&g, ruling), 1);
    g.assert_invariants().unwrap();
}

/// Every reference form reaches the same concept, so three references to one
/// ruling write one edge. A synonym the session holds is not a reference
/// form (review H1): it is refused as missing.
#[test]
fn every_reference_form_writes_the_same_one_edge() {
    let (mut g, i) = graph("cite-forms");
    let ruling = concept(&mut g, i, RULING, ConceptType::Logic);
    g.declare_synonym("lease rule", RULING);
    let full = ruling.0.to_string();
    let short = format::short_id(ruling);
    let err = derive_with_citations(
        &mut g,
        i,
        &[(DECISION, ConceptType::Logic)],
        &[cite(DECISION, &["lease rule"])],
    )
    .unwrap_err();
    assert_eq!(refusal(&err).kind, ReferenceRefusalKind::Missing);
    let out = derive_with_citations(
        &mut g,
        i,
        &[(DECISION, ConceptType::Logic)],
        &[cite(DECISION, &[&full, &short, RULING])],
    )
    .unwrap();
    assert_eq!(out.cited, 1);
    let decision = node_of(&g, DECISION);
    let deps: Vec<NodeId> = g
        .edges()
        .filter(|e| e.edge_type == EdgeType::Dependency && e.source == decision)
        .map(|e| e.target)
        .collect();
    assert_eq!(deps, vec![ruling]);
}

/// Natural-key idempotency: the same cited derive twice is one edge,
/// reinforced; nothing new on the second call.
#[test]
fn a_resent_citation_reinforces_one_edge() {
    let (mut g, i) = graph("cite-idem");
    let ruling = concept(&mut g, i, RULING, ConceptType::Logic);
    let call = [cite(DECISION, &[&format::short_id(ruling)])];
    let first = derive_with_citations(&mut g, i, &[(DECISION, ConceptType::Logic)], &call).unwrap();
    assert_eq!(first.cited, 1);
    let (nodes, edges) = (g.node_count(), g.edge_count());
    let second =
        derive_with_citations(&mut g, i, &[(DECISION, ConceptType::Logic)], &call).unwrap();
    assert_eq!(second.cited, 0);
    assert!(second.reinforced >= 1);
    assert_eq!((g.node_count(), g.edge_count()), (nodes, edges));
    let decision = node_of(&g, DECISION);
    assert_eq!(
        g.edge_between(decision, ruling, EdgeType::Dependency)
            .unwrap()
            .reinforcements,
        2
    );
}

/// The refusals the pre-pass owes, each typed, each before anything is
/// written.
#[test]
fn every_bad_reference_is_refused_with_its_kind_and_nothing_is_written() {
    let (mut g, i) = graph("cite-refuse");
    let ruling = concept(&mut g, i, RULING, ConceptType::Logic);
    concept_with_id(
        &mut g,
        i,
        id_with_prefix("8baad4e0aa", 1),
        "one",
        ConceptType::Entity,
    );
    concept_with_id(
        &mut g,
        i,
        id_with_prefix("8baad4e0bb", 2),
        "two",
        ConceptType::Entity,
    );
    // `DECISION` already exists and depends on `RULING`; `RULING` citing it
    // back would close a cycle.
    derive_with_citations(
        &mut g,
        i,
        &[(DECISION, ConceptType::Logic)],
        &[cite(DECISION, &[RULING])],
    )
    .unwrap();
    let before = g.snapshot();
    let nine: Vec<String> = (0..9).map(|n| format!("missing {n}")).collect();
    let nine: Vec<&str> = nine.iter().map(String::as_str).collect();
    type Case<'a> = (Vec<(&'a str, ConceptType)>, Vec<ConceptCitations>, &'a str);
    let cases: Vec<Case> = vec![
        (
            vec![("fresh", ConceptType::Logic)],
            vec![cite("fresh", &["nothing like it"])],
            "missing",
        ),
        (
            vec![("fresh", ConceptType::Logic)],
            vec![cite("fresh", &["8baad4e0"])],
            "ambiguous",
        ),
        (
            vec![("fresh", ConceptType::Logic)],
            vec![cite("fresh", &[&i.0.to_string()])],
            "interaction",
        ),
        (
            vec![(RULING, ConceptType::Logic)],
            vec![cite(RULING, &[&format::short_id(ruling)])],
            "self",
        ),
        (
            vec![(RULING, ConceptType::Logic)],
            vec![cite(RULING, &[DECISION])],
            "cycle",
        ),
        (
            vec![("fresh", ConceptType::Logic)],
            vec![cite("fresh", &nine)],
            "limit",
        ),
        (
            vec![("fresh", ConceptType::Logic)],
            vec![cite("not in the call", &[RULING])],
            "unknown",
        ),
    ];
    for (concepts, citations, want) in cases {
        let err = derive_with_citations(&mut g, i, &concepts, &citations).unwrap_err();
        let kind = &refusal(&err).kind;
        let got = match kind {
            ReferenceRefusalKind::Missing => "missing",
            ReferenceRefusalKind::Ambiguous { .. } => "ambiguous",
            ReferenceRefusalKind::NotAConcept => "interaction",
            ReferenceRefusalKind::SelfReference => "self",
            ReferenceRefusalKind::Cycle => "cycle",
            ReferenceRefusalKind::Limit { .. } => "limit",
            ReferenceRefusalKind::UnknownConcept => "unknown",
            other => panic!("{other:?}"),
        };
        assert_eq!(got, want, "{err}");
        assert!(err.to_string().ends_with("nothing was written"), "{err}");
        assert_eq!(g.snapshot(), before, "a refusal writes nothing: {err}");
    }
}

#[test]
fn the_call_wide_reference_limit_is_enforced() {
    let (mut g, i) = graph("cite-limit");
    let ruling = concept(&mut g, i, RULING, ConceptType::Logic);
    let short = format::short_id(ruling);
    let contents: Vec<String> = (0..5).map(|n| format!("decision number {n}")).collect();
    let concepts: Vec<(&str, ConceptType)> = contents
        .iter()
        .map(|c| (c.as_str(), ConceptType::Logic))
        .collect();
    // 5 concepts x 7 references = 35 > 32.
    let refs: Vec<&str> = std::iter::repeat_n(short.as_str(), 7).collect();
    let citations: Vec<ConceptCitations> = contents.iter().map(|c| cite(c, &refs)).collect();
    let err = derive_with_citations(&mut g, i, &concepts, &citations).unwrap_err();
    assert!(matches!(
        refusal(&err).kind,
        ReferenceRefusalKind::Limit {
            limit: MAX_REFERENCES_PER_DERIVE,
            ..
        }
    ));
}

/// Pinned at the ack, re-resolved at the commit: a cited concept removed,
/// or a short id made ambiguous, in between refuses the write and leaves
/// the graph exactly as it was.
#[test]
fn a_reference_that_changes_after_the_ack_refuses_at_the_commit() {
    for change in ["removed", "shadowed"] {
        let (mut g, i) = graph("cite-changed");
        let ruling = concept_with_id(
            &mut g,
            i,
            id_with_prefix("c0ffee00aa", 1),
            RULING,
            ConceptType::Logic,
        );
        let reference = "c0ffee00".to_string();
        let concepts = [(DECISION, ConceptType::Logic)];
        let pinned = validate(&g, &concepts, &[cite(DECISION, &[&reference])]).unwrap();
        assert_eq!(pinned.references[0].target, ruling);
        let now = match change {
            "removed" => {
                g.remove_node(ruling).unwrap();
                None
            }
            _ => {
                concept_with_id(
                    &mut g,
                    i,
                    id_with_prefix("c0ffee00bb", 2),
                    "shadow",
                    ConceptType::Entity,
                );
                None
            }
        };
        let before = g.snapshot();
        let (epoch, log) = (g.epoch(), g.log_len());
        let err = derive_cited(
            &mut g,
            i,
            &agent(),
            &concepts,
            &ParentOf::none(),
            10,
            &pinned,
        )
        .unwrap_err();
        assert_eq!(
            refusal(&err).kind,
            ReferenceRefusalKind::Changed {
                accepted: Some(ruling),
                now
            },
            "{change}"
        );
        assert_eq!(g.snapshot(), before, "{change}: nothing written");
        assert_eq!((g.epoch(), g.log_len()), (epoch, log), "{change}");
    }
}

/// The commit runs the self and cycle checks again on the sources as
/// written: here a synonym declared (through the library) between the ack
/// and the commit makes the citing content canonical-match the cited
/// concept itself, and an edge added in
/// between closes a cycle.
#[test]
fn the_commit_refuses_self_and_cycle_it_could_not_see_at_the_ack() {
    let (mut g, i) = graph("cite-commit-self");
    concept(&mut g, i, RULING, ConceptType::Logic);
    let concepts = [("an alias of the ruling", ConceptType::Logic)];
    let pinned = validate(&g, &concepts, &[cite("an alias of the ruling", &[RULING])]).unwrap();
    g.declare_synonym("an alias of the ruling", RULING);
    let before = g.snapshot();
    let err = derive_cited(
        &mut g,
        i,
        &agent(),
        &concepts,
        &ParentOf::none(),
        10,
        &pinned,
    )
    .unwrap_err();
    assert_eq!(refusal(&err).kind, ReferenceRefusalKind::SelfReference);
    assert_eq!(g.snapshot(), before);

    let (mut g, i) = graph("cite-commit-cycle");
    let ruling = concept(&mut g, i, RULING, ConceptType::Logic);
    let decision = concept(&mut g, i, DECISION, ConceptType::Logic);
    let concepts = [(DECISION, ConceptType::Logic)];
    let pinned = validate(&g, &concepts, &[cite(DECISION, &[RULING])]).unwrap();
    // In between, the ruling comes to depend on the decision.
    derive_with_citations(
        &mut g,
        i,
        &[(RULING, ConceptType::Logic)],
        &[cite(RULING, &[DECISION])],
    )
    .unwrap();
    assert!(g
        .edge_between(ruling, decision, EdgeType::Dependency)
        .is_some());
    let before = g.snapshot();
    let err = derive_cited(
        &mut g,
        i,
        &agent(),
        &concepts,
        &ParentOf::none(),
        10,
        &pinned,
    )
    .unwrap_err();
    assert_eq!(refusal(&err).kind, ReferenceRefusalKind::Cycle);
    assert_eq!(g.snapshot(), before);
}

/// The background refuses exactly what the pre-pass refuses: handed pins
/// that skipped the pre-pass's self and cycle checks, the commit refuses
/// them with the same kinds.
#[test]
fn the_commit_and_the_pre_pass_refuse_the_same_kinds() {
    let (mut g, i) = graph("cite-agree");
    let ruling = concept(&mut g, i, RULING, ConceptType::Logic);
    let decision = concept(&mut g, i, DECISION, ConceptType::Logic);
    derive_with_citations(
        &mut g,
        i,
        &[(DECISION, ConceptType::Logic)],
        &[cite(DECISION, &[RULING])],
    )
    .unwrap();
    for (content, target, reference) in [(RULING, ruling, RULING), (RULING, decision, DECISION)] {
        let concepts = [(content, ConceptType::Logic)];
        let pre = validate(&g, &concepts, &[cite(content, &[reference])]).unwrap_err();
        let forged = PinnedCitations {
            against: Vec::new(),
            references: vec![PinnedReference {
                content: content.to_string(),
                reference: reference.to_string(),
                target,
            }],
        };
        let commit = derive_cited(
            &mut g,
            i,
            &agent(),
            &concepts,
            &ParentOf::none(),
            10,
            &forged,
        )
        .unwrap_err();
        assert_eq!(
            refusal(&pre).kind,
            refusal(&commit).kind,
            "{pre} / {commit}"
        );
    }
}

fn action<'a>(depends_on: &'a [&'a str]) -> Action<'a> {
    Action {
        repo_scope: None,
        event_time: None,
        action: "ran the migration",
        produces: &[],
        modifies: &[],
        depends_on,
    }
}

/// `record_action`'s `depends_on` (review M1): a hyphenated node id or a
/// unique short id of an existing concept cites it; an interaction's id and
/// a short id two concepts share are refused; everything else is the 0.5.0
/// text rule, including an id that names nothing.
#[test]
fn record_action_cites_ids_and_refuses_only_interactions_and_shared_prefixes() {
    let (mut g, i) = graph("cite-action");
    let ruling = concept(&mut g, i, RULING, ConceptType::Logic);
    concept_with_id(
        &mut g,
        i,
        id_with_prefix("8baad4e0aa", 1),
        "one",
        ConceptType::Entity,
    );
    concept_with_id(
        &mut g,
        i,
        id_with_prefix("8baad4e0bb", 2),
        "two",
        ConceptType::Entity,
    );
    for reference in [
        format::short_id(ruling),
        ruling.0.to_string(),
        ruling.0.to_string().to_uppercase(),
    ] {
        let deps = [reference.as_str()];
        let before = g.concepts().count();
        let out = record_action(&mut g, i, &agent(), &action(&deps)).unwrap();
        assert!(g
            .edge_between(out.action_node, ruling, EdgeType::Dependency)
            .is_some());
        assert!(g.concepts().all(|c| c.content != reference));
        assert!(g.concepts().count() <= before + 1);
    }
    for (reference, want) in [
        (i.0.to_string(), "interaction"),
        ("8baad4e0".to_string(), "ambiguous"),
    ] {
        let deps = [reference.as_str()];
        let before = g.snapshot();
        let err = record_action(&mut g, i, &agent(), &action(&deps)).unwrap_err();
        let r = refusal(&err);
        assert_eq!(r.field, ReferenceField::ActionDependsOn);
        let got = match r.kind {
            ReferenceRefusalKind::NotAConcept => "interaction",
            ReferenceRefusalKind::Ambiguous { .. } => "ambiguous",
            ref other => panic!("{other:?}"),
        };
        assert_eq!(got, want);
        assert_eq!(g.snapshot(), before);
    }
    // Plain text is the legacy rule: canonical match, else create.
    let out = record_action(&mut g, i, &agent(), &action(&[RULING, "schema v1"])).unwrap();
    assert!(g
        .edge_between(out.action_node, ruling, EdgeType::Dependency)
        .is_some());
    node_of(&g, "schema v1");
}

/// Review M1, with what dogfood agents actually send in `depends_on`: every
/// one of these was accepted by 0.5.0 as text and still is, created on first
/// use and matched after.
#[test]
fn realistic_depends_on_entries_keep_the_0_5_0_text_rule() {
    let (mut g, i) = graph("cite-action-dogfood");
    let ruling = concept(&mut g, i, RULING, ConceptType::Logic);
    let urn = format!("urn:uuid:{}", ruling.0);
    let braced = format!("{{{}}}", ruling.0);
    let unknown = NodeId::new().0.to_string();
    let entries = [
        "3df6812",                          // 7-char commit hash
        "9bef6ec8",                         // 8-char commit hash, no concept prefix
        "d41d8cd98f00b204e9800998ecf8427e", // 32-hex (MD5)
        urn.as_str(),                       // urn:uuid: form
        braced.as_str(),                    // braced form
        unknown.as_str(),                   // a hyphenated id that names nothing
        "src/graph/cite.rs",                // a file path
        "#36",                              // an issue ref
        "nrynss/lambo#145",                 // a cross-repo issue ref
    ];
    for entry in entries {
        // The id-shaped ones never collide with these few concepts' ids.
        assert!(!format::short_id(ruling).starts_with(&entry[..entry.len().min(8)]));
        let deps = [entry];
        let out = record_action(&mut g, i, &agent(), &action(&deps)).unwrap();
        let node = node_of(&g, entry);
        assert!(
            g.edge_between(out.action_node, node, EdgeType::Dependency)
                .is_some(),
            "{entry}"
        );
        assert_ne!(node, ruling, "{entry}");
        // Re-sent: matched, nothing new.
        let before = g.concepts().count();
        record_action(&mut g, i, &agent(), &action(&deps)).unwrap();
        assert_eq!(g.concepts().count(), before, "{entry} re-sent");
    }
}

/// A 0.5.0 request re-sent: its id text was minted as an Entity back then
/// (the id names no node), so it matches that Entity, as it did.
#[test]
fn a_resent_0_5_0_request_lands_on_the_entity_its_id_text_minted() {
    let (mut g, i) = graph("cite-action-resend");
    let text = NodeId::new().0.to_string();
    let minted = concept(&mut g, i, &text, ConceptType::Entity);
    let before = g.concepts().count();
    let deps = [text.as_str()];
    let out = record_action(&mut g, i, &agent(), &action(&deps)).unwrap();
    assert!(g
        .edge_between(out.action_node, minted, EdgeType::Dependency)
        .is_some());
    assert_eq!(g.concepts().count(), before + 1, "only the action is new");
}

/// Review L4: an 8-hex commit hash that happens to prefix a concept's id
/// (chance about concepts / 2^32) cites that concept; a text match wins
/// over the prefix, as 0.5.0 matched text.
#[test]
fn a_commit_hash_that_prefixes_a_concept_id_cites_it_unless_its_text_exists() {
    let (mut g, i) = graph("cite-action-l4");
    let unlucky = concept_with_id(
        &mut g,
        i,
        id_with_prefix("9bef6ec8", 1),
        "an unlucky concept",
        ConceptType::Logic,
    );
    let out = record_action(&mut g, i, &agent(), &action(&["9bef6ec8"])).unwrap();
    assert!(g
        .edge_between(out.action_node, unlucky, EdgeType::Dependency)
        .is_some());
    let hash = concept(&mut g, i, "57eca7d6", ConceptType::Entity);
    concept_with_id(
        &mut g,
        i,
        id_with_prefix("57eca7d6", 2),
        "another unlucky concept",
        ConceptType::Logic,
    );
    let out = record_action(&mut g, i, &agent(), &action(&["57eca7d6"])).unwrap();
    assert!(g
        .edge_between(out.action_node, hash, EdgeType::Dependency)
        .is_some());
}

/// Fixed ids for the id-shape tests, so no run depends on a random id. The
/// first is the shape CI flaked on: its 12- and 11-character prefixes
/// (`2122de39e47e`, `2122de39e47`) share one canonical key, because the
/// Porter stemmer drops a trailing `e` (see
/// `hex_text_that_stems_alike_is_one_concept`).
const FIXED_IDS: [u128; 4] = [
    0x2122de39_e47e_4a51_9c3b_0d6f8e2a1b47,
    0xabcdef01_2345_4678_9abc_def012345678,
    0x9e9e9e9e_e9e9_4e9e_8e9e_9e9e9e9e9e9e,
    0x00000000_0000_4000_8000_000000000001,
];

/// Orchestrator decision on the #36 PR 4 review: only the hyphenated id and
/// exactly 8 hex characters are ids. The 32-hex simple form of a real
/// concept's id, and a 9 to 12 character prefix of it, stay text in
/// `record_action` (the 0.5.0 rule: canonical match, else create; never an
/// edge to the concept the id names) and match nothing as a derive
/// citation; exactly 8 cites.
#[test]
fn only_the_hyphenated_id_and_exactly_eight_hex_characters_are_ids() {
    for raw in FIXED_IDS {
        let (mut g, i) = graph("cite-id-shapes");
        let ruling = concept_with_id(
            &mut g,
            i,
            NodeId(uuid::Uuid::from_u128(raw)),
            RULING,
            ConceptType::Logic,
        );
        let simple = ruling.0.simple().to_string();
        let mut texts = vec![simple.clone()];
        texts.extend((9..=12).map(|n| simple[..n].to_string()));
        for text in &texts {
            let deps = [text.as_str()];
            let out = record_action(&mut g, i, &agent(), &action(&deps)).unwrap();
            // The text rule's own answer: the concept carrying the text's
            // canonical key (created now, or by an earlier text that stems
            // alike), never the concept whose id the text spells.
            let key = crate::graph::canonical::canonical_key(text, |_| None);
            let target = g.concept_by_key(&key).expect("the text rule wrote it");
            assert_ne!(target, ruling, "{text}");
            assert!(
                g.edge_between(out.action_node, target, EdgeType::Dependency)
                    .is_some(),
                "{text} is text"
            );
            assert!(
                g.edge_between(out.action_node, ruling, EdgeType::Dependency)
                    .is_none(),
                "{text} does not cite the concept"
            );
        }
        let (mut g, i) = graph("cite-id-shapes-derive");
        let ruling = concept_with_id(
            &mut g,
            i,
            NodeId(uuid::Uuid::from_u128(raw)),
            RULING,
            ConceptType::Logic,
        );
        for text in [
            simple.clone(),
            simple[..9].to_string(),
            simple[..12].to_string(),
        ] {
            let err = derive_with_citations(
                &mut g,
                i,
                &[(DECISION, ConceptType::Logic)],
                &[cite(DECISION, &[&text])],
            )
            .unwrap_err();
            assert_eq!(refusal(&err).kind, ReferenceRefusalKind::Missing, "{text}");
        }
        // Exactly 8 cites, in both places.
        let out = derive_with_citations(
            &mut g,
            i,
            &[(DECISION, ConceptType::Logic)],
            &[cite(DECISION, &[&simple[..8]])],
        )
        .unwrap();
        assert_eq!(out.cited, 1);
        let deps = [&simple[..8]];
        let act = record_action(&mut g, i, &agent(), &action(&deps)).unwrap();
        assert!(g
            .edge_between(act.action_node, ruling, EdgeType::Dependency)
            .is_some());
    }
}

/// The CI flake's root cause, pinned: canonicalization (spec section 7.1,
/// unchanged since 0.5.0) stems every token, and the Porter stemmer drops
/// a trailing `e`, so `2122de39e47e` and `2122de39e47` are one canonical key
/// and one concept, whichever came first. The same holds in 0.5.0; it is
/// the text rule, not anything #36 PR 4 resolves.
#[test]
fn hex_text_that_stems_alike_is_one_concept() {
    let key = |t: &str| crate::graph::canonical::canonical_key(t, |_| None);
    assert_eq!(key("2122de39e47e"), key("2122de39e47"));
    let (mut g, i) = graph("cite-hex-stem");
    let first = record_action(&mut g, i, &agent(), &action(&["2122de39e47"])).unwrap();
    let minted = node_of(&g, "2122de39e47");
    assert!(first.created.contains(&minted));
    let second = record_action(&mut g, i, &agent(), &action(&["2122de39e47e"])).unwrap();
    assert!(g
        .edge_between(second.action_node, minted, EdgeType::Dependency)
        .is_some());
    assert!(g.concepts().all(|c| c.content != "2122de39e47e"));
}

/// An id entry is pinned at the ack and checked at the apply; a request
/// with no id-shaped entry carries no pins at all.
#[test]
fn record_action_pins_id_entries_and_refuses_a_changed_one() {
    let (mut g, i) = graph("cite-action-pins");
    let ruling = concept_with_id(
        &mut g,
        i,
        id_with_prefix("feed0000aa", 1),
        RULING,
        ConceptType::Logic,
    );
    assert!(validate_pinned(&g, &action(&[RULING, "schema v1"]))
        .unwrap()
        .is_empty());
    let deps = ["feed0000", "schema v1", "abcabcab"];
    let pins = validate_pinned(&g, &action(&deps)).unwrap();
    assert_eq!(pins, vec![Some(ruling), None, None]);

    // Unchanged: applies.
    let mut ok = g.clone();
    record_action_pinned(
        &mut ok,
        i,
        &agent(),
        &action(&deps),
        &Default::default(),
        &pins,
    )
    .unwrap();

    // Removed in between: refused, nothing written.
    let mut gone = g.clone();
    gone.remove_node(ruling).unwrap();
    let before = gone.snapshot();
    let err = record_action_pinned(
        &mut gone,
        i,
        &agent(),
        &action(&deps),
        &Default::default(),
        &pins,
    )
    .unwrap_err();
    assert_eq!(
        refusal(&err).kind,
        ReferenceRefusalKind::Changed {
            accepted: Some(ruling),
            now: None
        }
    );
    assert_eq!(gone.snapshot(), before);

    // An id-shaped entry that named nothing at the ack and names a concept
    // now is refused too.
    let mut appeared = g.clone();
    let late = concept_with_id(
        &mut appeared,
        i,
        id_with_prefix("abcabcab", 9),
        "late",
        ConceptType::Entity,
    );
    let err = record_action_pinned(
        &mut appeared,
        i,
        &agent(),
        &action(&deps),
        &Default::default(),
        &pins,
    )
    .unwrap_err();
    assert_eq!(
        refusal(&err).kind,
        ReferenceRefusalKind::Changed {
            accepted: None,
            now: Some(late)
        }
    );
}
