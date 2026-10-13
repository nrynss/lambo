//! #19(b): `against` on the canonical derive path and on `record_action`.
//! A contradiction is resolved, pinned, rechecked and refused exactly like a
//! citation (owner decision, option A); these tests pin that, and the three
//! ways it differs (no acyclicity, kept through a similarity merge, its own
//! budget). The write-queue, hybrid, replay and MCP halves are in
//! `memory::tests::against` and `mcp::server::tests::against`.

use super::*;
use crate::graph::action::{record_action_against, validate_against, Action};
use crate::graph::derive::{derive_cited, ParentOf};
use crate::recall::format;
use crate::test_util::citations::{concept, concept_with_id, graph, id_with_prefix};
use crate::types::{AgentId, ReferenceRefusal};

const BELIEF: &str = "brand x runs true to size";
const NEWER: &str = "brand x runs a size small";

fn agent() -> AgentId {
    AgentId::from("agent-a")
}

fn against(content: &str, refs: &[&str]) -> ConceptCitations {
    ConceptCitations {
        content: content.to_string(),
        depends_on: Vec::new(),
        against: refs.iter().map(|s| s.to_string()).collect(),
    }
}

/// Validate then derive, the way `Memory`'s synchronous path does.
fn derive_against(
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

fn contradictions(g: &Graph) -> Vec<(NodeId, NodeId)> {
    let mut out: Vec<(NodeId, NodeId)> = g
        .edges()
        .filter(|e| e.edge_type == EdgeType::Contradicts)
        .map(|e| (e.source, e.target))
        .collect();
    out.sort_by_key(|(s, t)| (s.0, t.0));
    out
}

/// `against` writes exactly one `newer -Contradicts-> older` edge at the
/// contradiction weight, and nothing that reads as dependence: no
/// `Dependency` edge, no dependent, no blast radius. Every reference form
/// reaches the same edge, and re-sending the call reinforces it (natural
/// key), writing nothing new.
#[test]
fn against_writes_one_contradicts_edge_newer_to_older() {
    let (mut g, i) = graph("against-basic");
    let belief = concept(&mut g, i, BELIEF, ConceptType::Logic);
    let call = [against(
        NEWER,
        &[&format::short_id(belief), &belief.0.to_string(), BELIEF],
    )];
    let out = derive_against(&mut g, i, &[(NEWER, ConceptType::Logic)], &call).unwrap();
    assert_eq!((out.contradicted, out.cited), (1, 0));
    let newer = node_of(&g, NEWER);
    assert_eq!(contradictions(&g), vec![(newer, belief)]);
    let edge = g
        .edge_between(newer, belief, EdgeType::Contradicts)
        .unwrap();
    assert_eq!(edge.weight, CONTRADICTS_WEIGHT);
    assert_eq!(edge.reinforcements, 1);
    assert!(g
        .edges()
        .all(|e| e.edge_type != EdgeType::Dependency || e.source != newer));
    assert_eq!(format::blast_radius(&g, belief), 0);

    let (nodes, edges) = (g.node_count(), g.edge_count());
    let again = derive_against(&mut g, i, &[(NEWER, ConceptType::Logic)], &call).unwrap();
    assert_eq!(again.contradicted, 0);
    assert!(again.reinforced >= 1);
    assert_eq!((g.node_count(), g.edge_count()), (nodes, edges));
    assert_eq!(
        g.edge_between(newer, belief, EdgeType::Contradicts)
            .unwrap()
            .reinforcements,
        2
    );
    g.assert_invariants().unwrap();
}

/// Each refusal the pre-pass owes, typed and naming the `against` field,
/// with nothing written. There is no cycle refusal: see
/// [`mutual_contradiction_is_allowed`].
#[test]
fn every_bad_against_is_refused_with_its_kind_and_nothing_is_written() {
    let (mut g, i) = graph("against-refuse");
    let belief = concept(&mut g, i, BELIEF, ConceptType::Logic);
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
    let before = g.snapshot();
    let nine: Vec<String> = (0..9).map(|n| format!("missing {n}")).collect();
    let nine: Vec<&str> = nine.iter().map(String::as_str).collect();
    type Case<'a> = (Vec<(&'a str, ConceptType)>, Vec<ConceptCitations>, &'a str);
    let cases: Vec<Case> = vec![
        (
            vec![(NEWER, ConceptType::Logic)],
            vec![against(NEWER, &["nothing like it"])],
            "missing",
        ),
        (
            vec![(NEWER, ConceptType::Logic)],
            vec![against(NEWER, &["8baad4e0"])],
            "ambiguous",
        ),
        (
            vec![(NEWER, ConceptType::Logic)],
            vec![against(NEWER, &[&i.0.to_string()])],
            "interaction",
        ),
        (
            vec![(BELIEF, ConceptType::Logic)],
            vec![against(BELIEF, &[&format::short_id(belief)])],
            "self",
        ),
        (
            vec![(NEWER, ConceptType::Logic)],
            vec![against(NEWER, &nine)],
            "limit",
        ),
        (
            vec![(NEWER, ConceptType::Logic)],
            vec![against("not in the call", &[BELIEF])],
            "unknown",
        ),
    ];
    for (concepts, citations, want) in cases {
        let err = derive_against(&mut g, i, &concepts, &citations).unwrap_err();
        let r = refusal(&err);
        assert_eq!(r.field, ReferenceField::Against, "{err}");
        let got = match &r.kind {
            ReferenceRefusalKind::Missing => "missing",
            ReferenceRefusalKind::Ambiguous { .. } => "ambiguous",
            ReferenceRefusalKind::NotAConcept => "interaction",
            ReferenceRefusalKind::SelfReference => "self",
            ReferenceRefusalKind::Limit { .. } => "limit",
            ReferenceRefusalKind::UnknownConcept => "unknown",
            other => panic!("{other:?}"),
        };
        assert_eq!(got, want, "{err}");
        let msg = err.to_string();
        assert!(msg.contains("against \""), "{msg}");
        assert!(msg.ends_with("nothing was written"), "{msg}");
        assert_eq!(g.snapshot(), before, "a refusal writes nothing: {err}");
    }
    let err = derive_against(
        &mut g,
        i,
        &[(BELIEF, ConceptType::Logic)],
        &[against(BELIEF, &[BELIEF])],
    )
    .unwrap_err();
    assert!(
        err.to_string()
            .contains("names the contradicting concept itself"),
        "{err}"
    );
}

/// The call-wide budget is `against`'s own, counted apart from
/// `depends_on`: eight of each on one concept is fine, 33 contradictions
/// across a call is not.
#[test]
fn against_has_its_own_budget() {
    let (mut g, i) = graph("against-limit");
    let targets: Vec<NodeId> = (0..8)
        .map(|n| concept(&mut g, i, &format!("older belief {n}"), ConceptType::Logic))
        .collect();
    let rulings: Vec<NodeId> = (0..8)
        .map(|n| concept(&mut g, i, &format!("a ruling {n}"), ConceptType::Logic))
        .collect();
    let refs: Vec<String> = targets.iter().map(|t| format::short_id(*t)).collect();
    let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
    let both = ConceptCitations {
        content: NEWER.into(),
        depends_on: rulings.iter().map(|t| format::short_id(*t)).collect(),
        against: refs.iter().map(|s| s.to_string()).collect(),
    };
    let out = derive_against(&mut g, i, &[(NEWER, ConceptType::Logic)], &[both]).unwrap();
    assert_eq!((out.cited, out.contradicted), (8, 8));

    let contents: Vec<String> = (0..5).map(|n| format!("newer claim {n}")).collect();
    let concepts: Vec<(&str, ConceptType)> = contents
        .iter()
        .map(|c| (c.as_str(), ConceptType::Logic))
        .collect();
    // 5 concepts x 7 references = 35 > 32.
    let seven = &refs[..7];
    let calls: Vec<ConceptCitations> = contents.iter().map(|c| against(c, seven)).collect();
    let err = derive_against(&mut g, i, &concepts, &calls).unwrap_err();
    assert_eq!(refusal(&err).field, ReferenceField::Against);
    assert!(matches!(
        refusal(&err).kind,
        ReferenceRefusalKind::Limit {
            limit: MAX_AGAINST_PER_DERIVE,
            ..
        }
    ));
}

/// Contradictions need no acyclicity: two concepts may contradict each
/// other (the #19(a) fixture's `Q <-> X`), and a contradiction against a
/// concept that depends on the contradicting one is not a `Dependency`
/// cycle either.
#[test]
fn mutual_contradiction_is_allowed() {
    let (mut g, i) = graph("against-mutual");
    let belief = concept(&mut g, i, BELIEF, ConceptType::Logic);
    derive_against(
        &mut g,
        i,
        &[(NEWER, ConceptType::Logic)],
        &[against(NEWER, &[BELIEF])],
    )
    .unwrap();
    let newer = node_of(&g, NEWER);
    let out = derive_against(
        &mut g,
        i,
        &[(BELIEF, ConceptType::Logic)],
        &[against(BELIEF, &[NEWER])],
    )
    .unwrap();
    assert_eq!(out.contradicted, 1);
    assert_eq!(contradictions(&g), {
        let mut v = vec![(newer, belief), (belief, newer)];
        v.sort_by_key(|(s, t)| (s.0, t.0));
        v
    });
    // The belief depends on the newer claim, and the newer claim still
    // contradicts it: a citation and a contradiction in opposite
    // directions are no cycle.
    let mut cites = against(BELIEF, &[]);
    cites.depends_on = vec![NEWER.to_string()];
    derive_against(&mut g, i, &[(BELIEF, ConceptType::Logic)], &[cites]).unwrap();
    assert!(g
        .edge_between(belief, newer, EdgeType::Dependency)
        .is_some());
    g.assert_invariants().unwrap();
}

/// Pinned at the ack, re-resolved at the commit: a contradicted concept
/// removed, or its short id made ambiguous, in between refuses the write
/// and leaves graph, epoch and log as they were. A citation in the same
/// call is rolled back with it.
#[test]
fn a_changed_against_target_refuses_at_the_commit() {
    for change in ["removed", "shadowed"] {
        let (mut g, i) = graph("against-changed");
        let belief = concept_with_id(
            &mut g,
            i,
            id_with_prefix("c0ffee00aa", 1),
            BELIEF,
            ConceptType::Logic,
        );
        let ruling = concept(&mut g, i, "a ruling it rests on", ConceptType::Logic);
        let concepts = [(NEWER, ConceptType::Logic)];
        let call = ConceptCitations {
            content: NEWER.into(),
            depends_on: vec![format::short_id(ruling)],
            against: vec!["c0ffee00".into()],
        };
        let pinned = validate(&g, &concepts, &[call]).unwrap();
        assert_eq!(pinned.against[0].target, belief);
        match change {
            "removed" => g.remove_node(belief).unwrap(),
            _ => {
                concept_with_id(
                    &mut g,
                    i,
                    id_with_prefix("c0ffee00bb", 2),
                    "shadow",
                    ConceptType::Entity,
                );
            }
        }
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
        let r = refusal(&err);
        assert_eq!(r.field, ReferenceField::Against, "{change}");
        assert_eq!(
            r.kind,
            ReferenceRefusalKind::Changed {
                accepted: Some(belief),
                now: None
            },
            "{change}"
        );
        assert_eq!(g.snapshot(), before, "{change}: nothing written");
        assert_eq!((g.epoch(), g.log_len()), (epoch, log), "{change}");
    }
}

/// The commit runs the self check again on the source as written: a
/// synonym declared (through the library) between the ack and the commit
/// makes the contradicting content canonical-match its own target.
#[test]
fn the_commit_refuses_a_self_contradiction_it_could_not_see_at_the_ack() {
    let (mut g, i) = graph("against-commit-self");
    concept(&mut g, i, BELIEF, ConceptType::Logic);
    let concepts = [("an alias of the belief", ConceptType::Logic)];
    let pinned = validate(
        &g,
        &concepts,
        &[against("an alias of the belief", &[BELIEF])],
    )
    .unwrap();
    g.declare_synonym("an alias of the belief", BELIEF);
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
}

/// A similarity merge does not drop a contradiction (it drops a citation,
/// #36 PR 4 review M2): the merged concept is its own node, and the
/// `Semantic` tie says nothing about opposition.
#[test]
fn a_similarity_merge_keeps_the_contradiction() {
    let (mut g, i) = graph("against-merge");
    let belief = concept(&mut g, i, BELIEF, ConceptType::Logic);
    let newer = concept(&mut g, i, NEWER, ConceptType::Logic);
    let pinned = vec![PinnedReference {
        content: NEWER.into(),
        reference: BELIEF.into(),
        target: belief,
    }];
    let mut written = Written::default();
    written.by_content.insert(NEWER.into(), newer);
    written.merged_into.insert(newer, belief);
    let stamp = Stamp {
        created_at: chrono::Utc::now(),
        event_time: None,
    };
    let sid = g.session_id().clone();
    let counts = write_edges(&mut g, &AGAINST, &pinned, &written, stamp, &sid).unwrap();
    assert_eq!(
        counts,
        EdgeCounts {
            added: 1,
            reinforced: 0
        }
    );
    assert!(g
        .edge_between(newer, belief, EdgeType::Contradicts)
        .is_some());
    // The same merge drops a citation as a reinforcement.
    let counts = write_edges(&mut g, &DEPENDS_ON, &pinned, &written, stamp, &sid).unwrap();
    assert_eq!(
        counts,
        EdgeCounts {
            added: 0,
            reinforced: 1
        }
    );
    assert!(g
        .edge_between(newer, belief, EdgeType::Dependency)
        .is_none());
}

fn action<'a>(text: &'a str) -> Action<'a> {
    Action {
        repo_scope: None,
        event_time: None,
        action: text,
        produces: &[],
        modifies: &[],
        depends_on: &[],
    }
}

/// `record_action`'s `against`: the action's own `Resource` concept
/// contradicts each target, resolved by the shared resolver (never created),
/// refused when missing, ambiguous, an interaction or the action itself.
#[test]
fn record_action_against_writes_from_the_action_node_and_refuses_bad_targets() {
    let (mut g, i) = graph("against-action");
    let belief = concept(&mut g, i, "brand y chargers are fine", ConceptType::Logic);
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
    let act = action("refunded: the charger overheated");
    let refs = vec![format::short_id(belief)];
    let pinned = validate_against(&g, &act, &refs).unwrap();
    assert_eq!(pinned.len(), 1);
    assert_eq!(pinned[0].content, act.action);
    let out = record_action_against(
        &mut g,
        i,
        &agent(),
        &act,
        &Default::default(),
        &[],
        &pinned,
        None,
    )
    .unwrap();
    assert!(g
        .edge_between(out.action_node, belief, EdgeType::Contradicts)
        .is_some());
    // No produces, modifies or depends_on: the one new edge is the
    // contradiction.
    assert_eq!(out.edges, 1);
    assert_eq!(contradictions(&g), vec![(out.action_node, belief)]);

    let before = g.snapshot();
    for (reference, want) in [
        ("nothing like it".to_string(), "missing"),
        ("8baad4e0".to_string(), "ambiguous"),
        (i.0.to_string(), "interaction"),
        (format::short_id(out.action_node), "self"),
    ] {
        let err = validate_against(&g, &act, &[reference]).unwrap_err();
        let r = refusal(&err);
        assert_eq!(r.field, ReferenceField::ActionAgainst);
        let got = match r.kind {
            ReferenceRefusalKind::Missing => "missing",
            ReferenceRefusalKind::Ambiguous { .. } => "ambiguous",
            ReferenceRefusalKind::NotAConcept => "interaction",
            ReferenceRefusalKind::SelfReference => "self",
            ref other => panic!("{other:?}"),
        };
        assert_eq!(got, want, "{err}");
    }
    let nine: Vec<String> = (0..9).map(|_| format::short_id(belief)).collect();
    let err = validate_against(&g, &act, &nine).unwrap_err();
    assert!(matches!(
        refusal(&err).kind,
        ReferenceRefusalKind::Limit {
            limit: MAX_AGAINST_PER_CONCEPT,
            ..
        }
    ));
    assert_eq!(g.snapshot(), before);
}

/// An action's contradiction is pinned at the ack and re-resolved in the
/// apply's transaction: a target removed in between refuses the call and
/// rolls back the action itself; a self-contradiction only the commit can
/// see (a synonym makes the action match its target) rolls back too.
#[test]
fn record_action_against_rolls_back_on_a_changed_target() {
    let (mut g, i) = graph("against-action-changed");
    let belief = concept(&mut g, i, "brand y chargers are fine", ConceptType::Logic);
    let act = action("refunded: the charger overheated");
    let pinned = validate_against(&g, &act, &[format::short_id(belief)]).unwrap();

    let mut gone = g.clone();
    gone.remove_node(belief).unwrap();
    let before = gone.snapshot();
    let (epoch, log) = (gone.epoch(), gone.log_len());
    let err = record_action_against(
        &mut gone,
        i,
        &agent(),
        &act,
        &Default::default(),
        &[],
        &pinned,
        None,
    )
    .unwrap_err();
    assert_eq!(
        refusal(&err).kind,
        ReferenceRefusalKind::Changed {
            accepted: Some(belief),
            now: None
        }
    );
    assert_eq!(gone.snapshot(), before);
    assert_eq!((gone.epoch(), gone.log_len()), (epoch, log));

    let mut aliased = g.clone();
    aliased.declare_synonym(act.action, "brand y chargers are fine");
    let before = aliased.snapshot();
    let err = record_action_against(
        &mut aliased,
        i,
        &agent(),
        &act,
        &Default::default(),
        &[],
        &pinned,
        None,
    )
    .unwrap_err();
    assert_eq!(refusal(&err).kind, ReferenceRefusalKind::SelfReference);
    assert_eq!(aliased.snapshot(), before, "the action was rolled back");
}

/// #19(b) review M2: one call cannot both cite and contradict the same
/// concept for the same citing concept, by any reference form; refused
/// typed, nothing written. Another concept of the call may cite what this
/// one contradicts, and separate writes may record both edges. The commit
/// refuses forged pins that skipped the pre-pass.
#[test]
fn citing_and_contradicting_one_concept_in_one_call_is_refused() {
    let (mut g, i) = graph("against-both");
    let belief = concept(&mut g, i, BELIEF, ConceptType::Logic);
    let before = g.snapshot();
    for (dep, against_ref) in [
        (format::short_id(belief), format::short_id(belief)),
        (belief.0.to_string(), BELIEF.to_string()),
    ] {
        let call = ConceptCitations {
            content: NEWER.into(),
            depends_on: vec![dep],
            against: vec![against_ref],
        };
        let err = derive_against(&mut g, i, &[(NEWER, ConceptType::Logic)], &[call]).unwrap_err();
        let r = refusal(&err);
        assert_eq!(r.field, ReferenceField::Against);
        assert_eq!(r.kind, ReferenceRefusalKind::DependsOnAndAgainst, "{err}");
        assert!(
            err.to_string().contains("also cites in depends_on"),
            "{err}"
        );
        assert_eq!(g.snapshot(), before);
    }
    // Different citing concepts in one call: fine.
    let other = "a second claim";
    let out = derive_against(
        &mut g,
        i,
        &[(NEWER, ConceptType::Logic), (other, ConceptType::Logic)],
        &[
            against(NEWER, &[BELIEF]),
            ConceptCitations {
                content: other.into(),
                depends_on: vec![BELIEF.into()],
                against: vec![],
            },
        ],
    )
    .unwrap();
    assert_eq!((out.cited, out.contradicted), (1, 1));
    // Separate writes: the same concept may come to both cite and
    // contradict it.
    let mut cites = against(NEWER, &[]);
    cites.depends_on = vec![BELIEF.into()];
    derive_against(&mut g, i, &[(NEWER, ConceptType::Logic)], &[cites]).unwrap();
    let newer = node_of(&g, NEWER);
    assert!(g
        .edge_between(newer, belief, EdgeType::Contradicts)
        .is_some());
    assert!(g
        .edge_between(newer, belief, EdgeType::Dependency)
        .is_some());

    // Forged pins handed straight to the commit are refused there.
    let forged = PinnedCitations {
        references: vec![PinnedReference {
            content: "a third claim".into(),
            reference: BELIEF.into(),
            target: belief,
        }],
        against: vec![PinnedReference {
            content: "a third claim".into(),
            reference: BELIEF.into(),
            target: belief,
        }],
    };
    let before = g.snapshot();
    let err = derive_cited(
        &mut g,
        i,
        &agent(),
        &[("a third claim", ConceptType::Logic)],
        &ParentOf::none(),
        10,
        &forged,
    )
    .unwrap_err();
    assert_eq!(
        refusal(&err).kind,
        ReferenceRefusalKind::DependsOnAndAgainst
    );
    assert_eq!(g.snapshot(), before);
}

/// #19(b) review M2 on `record_action`: an `against` target that one of
/// the action's `depends_on` entries resolves to, by id or by text, is
/// refused at the pre-pass, and at the commit when a text entry has come
/// to match it in between.
#[test]
fn an_action_that_depends_on_and_contradicts_one_concept_is_refused() {
    let (mut g, i) = graph("against-action-both");
    let belief = concept(&mut g, i, BELIEF, ConceptType::Logic);
    let refs = vec![format::short_id(belief)];
    let short = format::short_id(belief);
    for deps in [[short.as_str()], [BELIEF]] {
        let act = Action {
            depends_on: &deps,
            ..action("acted on it")
        };
        let err = validate_against(&g, &act, &refs).unwrap_err();
        let r = refusal(&err);
        assert_eq!(r.field, ReferenceField::ActionAgainst);
        assert_eq!(r.kind, ReferenceRefusalKind::DependsOnAndAgainst, "{err}");
    }
    // Commit: a text entry that names nothing at the ack is created as text;
    // if a synonym makes it match the target by the commit, it is refused.
    let deps = ["the belief, by another name"];
    let act = Action {
        depends_on: &deps,
        ..action("acted on it")
    };
    let pinned = validate_against(&g, &act, &refs).unwrap();
    g.declare_synonym("the belief, by another name", BELIEF);
    let before = g.snapshot();
    let err = record_action_against(
        &mut g,
        i,
        &agent(),
        &act,
        &Default::default(),
        &[],
        &pinned,
        None,
    )
    .unwrap_err();
    assert_eq!(
        refusal(&err).kind,
        ReferenceRefusalKind::DependsOnAndAgainst
    );
    assert_eq!(g.snapshot(), before);
}

/// #19(b) review L3: a contradiction refused at the commit leaves no
/// embedding contract stamped on the session. The stamp runs after the
/// recheck, inside the transaction; the same call with its target intact
/// stamps it.
#[test]
fn a_refused_action_contradiction_stamps_no_embedding_contract() {
    let (mut g, i) = graph("against-action-stamp");
    let belief = concept(&mut g, i, "brand y chargers are fine", ConceptType::Logic);
    assert!(g.embedding().is_none());
    let act = action("refunded: the charger overheated");
    let pinned = validate_against(&g, &act, &[format::short_id(belief)]).unwrap();
    let contract = crate::types::EmbeddingContract {
        kind: "fixture".into(),
        model: None,
        dim: 4,
    };

    let mut gone = g.clone();
    gone.remove_node(belief).unwrap();
    let before = gone.snapshot();
    let err = record_action_against(
        &mut gone,
        i,
        &agent(),
        &act,
        &Default::default(),
        &[],
        &pinned,
        Some(contract.clone()),
    )
    .unwrap_err();
    assert!(matches!(
        refusal(&err).kind,
        ReferenceRefusalKind::Changed { .. }
    ));
    assert!(gone.embedding().is_none(), "no contract was stamped");
    assert_eq!(gone.snapshot(), before);

    record_action_against(
        &mut g,
        i,
        &agent(),
        &act,
        &Default::default(),
        &[],
        &pinned,
        Some(contract.clone()),
    )
    .unwrap();
    assert_eq!(g.embedding(), Some(&contract));
}
