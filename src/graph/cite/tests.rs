//! Citations and names on the canonical derive path and on
//! `record_action` (#36 PR 4). The write-queue, hybrid, replay and MCP
//! halves are in `memory::tests::citations` and `mcp::server::tests`.

use super::*;
use crate::graph::action::{record_action, record_action_pinned, validate_pinned, Action};
use crate::graph::derive::derive_cited;
use crate::recall::format;
use crate::test_util::citations::{concept, concept_with_id, graph, id_with_prefix};
use crate::types::{AgentId, ReferenceRefusal};

const RULING: &str = "the writer lease stays with its holder until the holder releases it";
const DECISION: &str = "flush retries keep the lease instead of re-acquiring it";

fn agent() -> AgentId {
    AgentId::from("agent-a")
}

fn cite(content: &str, depends_on: &[&str], name: Option<&str>) -> ConceptCitations {
    ConceptCitations {
        content: content.to_string(),
        depends_on: depends_on.iter().map(|s| s.to_string()).collect(),
        name: name.map(str::to_string),
    }
}

/// Validate then derive, the way `Memory`'s synchronous path does.
fn derive_with_citations(
    g: &mut Graph,
    i: NodeId,
    concepts: &[(&str, ConceptType)],
    citations: &[ConceptCitations],
) -> Result<DeriveOutcome, LamboError> {
    let pinned = validate(g, concepts, &ParentOf::none(), citations)?;
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
        &[cite(DECISION, &[&format::short_id(ruling)], None)],
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

/// Every reference form reaches the same concept, so four references to one
/// ruling write one edge.
#[test]
fn every_reference_form_writes_the_same_one_edge() {
    let (mut g, i) = graph("cite-forms");
    let ruling = concept(&mut g, i, RULING, ConceptType::Logic);
    g.declare_synonym("lease rule", RULING);
    let full = ruling.0.to_string();
    let short = format::short_id(ruling);
    let out = derive_with_citations(
        &mut g,
        i,
        &[(DECISION, ConceptType::Logic)],
        &[cite(DECISION, &[&full, &short, "lease rule", RULING], None)],
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
/// reinforced, and one name; nothing new on the second call.
#[test]
fn a_resent_citation_reinforces_one_edge_and_one_name() {
    let (mut g, i) = graph("cite-idem");
    let ruling = concept(&mut g, i, RULING, ConceptType::Logic);
    let call = [cite(
        DECISION,
        &[&format::short_id(ruling)],
        Some("flush keeps lease"),
    )];
    let first = derive_with_citations(&mut g, i, &[(DECISION, ConceptType::Logic)], &call).unwrap();
    assert_eq!((first.cited, first.named), (1, 1));
    let (nodes, edges) = (g.node_count(), g.edge_count());
    let second =
        derive_with_citations(&mut g, i, &[(DECISION, ConceptType::Logic)], &call).unwrap();
    assert_eq!((second.cited, second.named), (0, 0));
    assert!(second.reinforced >= 1);
    assert_eq!((g.node_count(), g.edge_count()), (nodes, edges));
    let decision = node_of(&g, DECISION);
    assert_eq!(
        g.edge_between(decision, ruling, EdgeType::Dependency)
            .unwrap()
            .reinforcements,
        2
    );
    assert_eq!(g.synonym("flush keeps lease"), Some(DECISION));
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
        &[cite(DECISION, &[RULING], None)],
    )
    .unwrap();
    let before = g.snapshot();
    let nine: Vec<String> = (0..9).map(|n| format!("missing {n}")).collect();
    let nine: Vec<&str> = nine.iter().map(String::as_str).collect();
    type Case<'a> = (Vec<(&'a str, ConceptType)>, Vec<ConceptCitations>, &'a str);
    let cases: Vec<Case> = vec![
        (
            vec![("fresh", ConceptType::Logic)],
            vec![cite("fresh", &["nothing like it"], None)],
            "missing",
        ),
        (
            vec![("fresh", ConceptType::Logic)],
            vec![cite("fresh", &["8baad4e0"], None)],
            "ambiguous",
        ),
        (
            vec![("fresh", ConceptType::Logic)],
            vec![cite("fresh", &[&i.0.to_string()], None)],
            "interaction",
        ),
        (
            vec![(RULING, ConceptType::Logic)],
            vec![cite(RULING, &[&format::short_id(ruling)], None)],
            "self",
        ),
        (
            vec![(RULING, ConceptType::Logic)],
            vec![cite(RULING, &[DECISION], None)],
            "cycle",
        ),
        (
            vec![("fresh", ConceptType::Logic)],
            vec![cite("fresh", &nine, None)],
            "limit",
        ),
        (
            vec![("fresh", ConceptType::Logic)],
            vec![cite("not in the call", &[RULING], None)],
            "unknown",
        ),
        (
            vec![("fresh", ConceptType::Logic)],
            vec![cite("fresh", &[], Some(RULING))],
            "taken",
        ),
        (
            vec![("fresh", ConceptType::Logic)],
            vec![cite("fresh", &[], Some("deadbeef"))],
            "unusable",
        ),
        (
            vec![("fresh", ConceptType::Logic)],
            vec![cite("fresh", &[], Some("the of"))],
            "unusable",
        ),
        (
            vec![("fresh", ConceptType::Observation)],
            vec![cite("fresh", &[], Some("a fresh name"))],
            "unusable",
        ),
        (
            vec![
                ("fresh", ConceptType::Logic),
                ("other one", ConceptType::Logic),
            ],
            vec![cite("fresh", &[], Some("other one"))],
            "unusable",
        ),
        (
            vec![
                ("fresh", ConceptType::Logic),
                ("other one", ConceptType::Logic),
            ],
            vec![
                cite("fresh", &[], Some("shared")),
                cite("other one", &[], Some("shared")),
            ],
            "unusable",
        ),
        (
            vec![("fresh", ConceptType::Logic)],
            vec![
                cite("fresh", &[], Some("one name")),
                cite("fresh", &[], Some("another")),
            ],
            "unusable",
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
            ReferenceRefusalKind::NameTaken { .. } => "taken",
            ReferenceRefusalKind::NameUnusable { .. } => "unusable",
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
    let citations: Vec<ConceptCitations> = contents.iter().map(|c| cite(c, &refs, None)).collect();
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
/// a name re-pointed, or a short id made ambiguous in between refuses the
/// write and leaves the graph exactly as it was.
#[test]
fn a_reference_that_changes_after_the_ack_refuses_at_the_commit() {
    for change in ["removed", "renamed", "shadowed"] {
        let (mut g, i) = graph("cite-changed");
        let ruling = concept_with_id(
            &mut g,
            i,
            id_with_prefix("c0ffee00aa", 1),
            RULING,
            ConceptType::Logic,
        );
        let other = concept(&mut g, i, "another ruling entirely", ConceptType::Logic);
        g.declare_synonym("lease rule", RULING);
        let reference = match change {
            "renamed" => "lease rule".to_string(),
            _ => "c0ffee00".to_string(),
        };
        let concepts = [(DECISION, ConceptType::Logic)];
        let pinned = validate(
            &g,
            &concepts,
            &ParentOf::none(),
            &[cite(DECISION, &[&reference], None)],
        )
        .unwrap();
        assert_eq!(pinned.references[0].target, ruling);
        let now = match change {
            "removed" => {
                g.remove_node(ruling).unwrap();
                None
            }
            "renamed" => {
                g.declare_synonym("lease rule", "another ruling entirely");
                Some(other)
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
/// written: here a name declared between the ack and the commit makes the
/// citing content resolve to the cited concept itself, and an edge added in
/// between closes a cycle.
#[test]
fn the_commit_refuses_self_and_cycle_it_could_not_see_at_the_ack() {
    let (mut g, i) = graph("cite-commit-self");
    concept(&mut g, i, RULING, ConceptType::Logic);
    let concepts = [("an alias of the ruling", ConceptType::Logic)];
    let pinned = validate(
        &g,
        &concepts,
        &ParentOf::none(),
        &[cite("an alias of the ruling", &[RULING], None)],
    )
    .unwrap();
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
    let pinned = validate(
        &g,
        &concepts,
        &ParentOf::none(),
        &[cite(DECISION, &[RULING], None)],
    )
    .unwrap();
    // In between, the ruling comes to depend on the decision.
    derive_with_citations(
        &mut g,
        i,
        &[(RULING, ConceptType::Logic)],
        &[cite(RULING, &[DECISION], None)],
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
        &[cite(DECISION, &[RULING], None)],
    )
    .unwrap();
    for (content, target, reference) in [(RULING, ruling, RULING), (RULING, decision, DECISION)] {
        let concepts = [(content, ConceptType::Logic)];
        let pre = validate(
            &g,
            &concepts,
            &ParentOf::none(),
            &[cite(content, &[reference], None)],
        )
        .unwrap_err();
        let forged = PinnedCitations {
            references: vec![PinnedReference {
                content: content.to_string(),
                reference: reference.to_string(),
                target,
            }],
            names: vec![],
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

/// The alias defect, going forward: once a ruling carries a name, a later
/// `record_action` citing that name lands on the ruling and mints no alias
/// Entity. A name an existing alias Entity already holds is refused, not
/// taken over (the reviewed backfill redirects existing aliases).
#[test]
fn a_named_ruling_takes_later_citations_and_an_existing_alias_is_not_taken_over() {
    let (mut g, i) = graph("cite-alias");
    let concepts = [(RULING, ConceptType::Logic)];
    let out = derive_with_citations(
        &mut g,
        i,
        &concepts,
        &[cite(RULING, &[], Some("writer lease"))],
    )
    .unwrap();
    assert_eq!(out.named, 1);
    let ruling = node_of(&g, RULING);
    let entities = g.concepts().count();
    let act = record_action(
        &mut g,
        i,
        &agent(),
        &Action {
            event_time: None,
            action: "kept the lease across a flush retry",
            produces: &[],
            modifies: &[],
            depends_on: &["writer lease"],
        },
    )
    .unwrap();
    assert_eq!(act.created, vec![act.action_node], "no alias Entity minted");
    assert_eq!(g.concepts().count(), entities + 1);
    assert!(g
        .edge_between(act.action_node, ruling, EdgeType::Dependency)
        .is_some());

    let (mut g, i) = graph("cite-alias-taken");
    let alias = concept(&mut g, i, "writer lease", ConceptType::Entity);
    let err = derive_with_citations(
        &mut g,
        i,
        &concepts,
        &[cite(RULING, &[], Some("writer lease"))],
    )
    .unwrap_err();
    match &refusal(&err).kind {
        ReferenceRefusalKind::NameTaken { holder } => assert_eq!(holder.node, alias),
        other => panic!("{other:?}"),
    }
}

fn action<'a>(depends_on: &'a [&'a str]) -> Action<'a> {
    Action {
        event_time: None,
        action: "ran the migration",
        produces: &[],
        modifies: &[],
        depends_on,
    }
}

/// `record_action`'s `depends_on`: an id resolves to the existing concept
/// (no Entity named by the hex), a full id that names nothing or an
/// interaction is refused, an ambiguous short id is refused, and a
/// short-id-shaped string that names nothing is an artifact name as
/// before (a commit hash). Text entries are unchanged.
#[test]
fn record_action_resolves_ids_first_and_keeps_its_legacy_text_rule() {
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
    for reference in [format::short_id(ruling), ruling.0.to_string()] {
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
        (NodeId::new().0.to_string(), "missing"),
        (i.0.to_string(), "interaction"),
        ("8baad4e0".to_string(), "ambiguous"),
    ] {
        let deps = [reference.as_str()];
        let before = g.snapshot();
        let err = record_action(&mut g, i, &agent(), &action(&deps)).unwrap_err();
        let r = refusal(&err);
        assert_eq!(r.field, ReferenceField::ActionDependsOn);
        let got = match r.kind {
            ReferenceRefusalKind::Missing => "missing",
            ReferenceRefusalKind::NotAConcept => "interaction",
            ReferenceRefusalKind::Ambiguous { .. } => "ambiguous",
            ref other => panic!("{other:?}"),
        };
        assert_eq!(got, want);
        assert_eq!(g.snapshot(), before);
    }
    // A commit hash that names no concept is still an artifact.
    let out = record_action(&mut g, i, &agent(), &action(&["9bef6ec8"])).unwrap();
    let hash = node_of(&g, "9bef6ec8");
    assert!(out.created.contains(&hash));
    // Plain text is the legacy rule: canonical match, else create.
    let out = record_action(&mut g, i, &agent(), &action(&[RULING, "schema v1"])).unwrap();
    assert!(g
        .edge_between(out.action_node, ruling, EdgeType::Dependency)
        .is_some());
    node_of(&g, "schema v1");
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
