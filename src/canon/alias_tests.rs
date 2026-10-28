//! Tests for the alias backfill plan (#36 PR 5), on the shared fair-test
//! fixture and small additions to it.

use super::*;
use crate::graph::canonical::canonical_key;
use crate::test_util::fair_test as fair;
use crate::types::{CanonizationStatus, Concept};

fn graph(snap: &GraphSnapshot) -> Graph {
    Graph::from_snapshot(snap.clone()).expect("valid snapshot")
}

fn id(key: &str) -> NodeId {
    fair::concept_id(key)
}

fn short(key: &str) -> String {
    id(key).0.simple().to_string()[..8].to_string()
}

/// A concept like `like`'s, with new id, content and type.
fn concept(snap: &GraphSnapshot, like: &str, name: &str, t: ConceptType, content: &str) -> Concept {
    let mut c = snap
        .concepts
        .iter()
        .find(|c| c.id == id(like))
        .expect("template")
        .clone();
    c.id = fair::hashed_id(&format!("test/{name}"));
    c.content = content.into();
    c.canonical_key = canonical_key(content, |_| None);
    c.concept_type = t;
    c.canonization_status = CanonizationStatus::None;
    c
}

/// A `Dependency` edge like `like`'s (same times), `source -> target`.
fn cite(snap: &GraphSnapshot, source: NodeId, target: NodeId, name: &str) -> Edge {
    let mut e = snap.edges[0].clone();
    e.id = fair::hashed_id(&format!("test-edge/{name}"));
    e.source = source;
    e.target = target;
    e.edge_type = EdgeType::Dependency;
    e
}

/// The `Derives` edge every concept carries from its origin interaction.
fn derives(snap: &GraphSnapshot, c: &Concept) -> Edge {
    let mut e = cite(
        snap,
        c.origin_interaction,
        c.id,
        &format!("derives/{}", c.id.0),
    );
    e.edge_type = EdgeType::Derives;
    e
}

#[test]
fn the_leading_phrase_rule_proposes_nothing_on_the_fixture() {
    // "writer lease" is not the leading phrase of any ruling ("a writer keeps
    // its lease ..."), so detection leaves the fixture's alias to a review.
    let (pairs, refusals) = detect(&graph(&fair::snapshot(true)));
    assert!(pairs.is_empty(), "{pairs:?}");
    assert!(refusals.is_empty(), "{refusals:?}");
}

#[test]
fn the_leading_phrase_rule_pairs_one_ruling_and_refuses_two() {
    let mut snap = fair::snapshot(true);
    // "No flush may wedge" opens exactly one ruling (`wedge`), punctuation
    // and case aside; "writer" opens `lease_stays` and the decision `d01`.
    let wedge_name = concept(
        &snap,
        "lease_alias",
        "wedge_name",
        ConceptType::Entity,
        "No flush may wedge:",
    );
    let writer_name = concept(
        &snap,
        "lease_alias",
        "writer_name",
        ConceptType::Entity,
        "writer",
    );
    let uncited = concept(
        &snap,
        "lease_alias",
        "uncited",
        ConceptType::Entity,
        "agent identity",
    );
    let mut edges = vec![
        cite(&snap, id("a01"), wedge_name.id, "w1"),
        cite(&snap, id("a02"), writer_name.id, "w2"),
    ];
    for c in [&wedge_name, &writer_name, &uncited] {
        edges.push(derives(&snap, c));
    }
    let (w, r) = (wedge_name.id, writer_name.id);
    snap.concepts.extend([wedge_name, writer_name, uncited]);
    snap.edges.extend(edges);
    let (pairs, refusals) = detect(&graph(&snap));
    assert_eq!(
        pairs,
        vec![AliasPair {
            alias: w,
            ruling: id("wedge"),
            source: PairSource::Detected,
            line: None
        }]
    );
    assert_eq!(refusals.len(), 1, "{refusals:?}");
    assert_eq!(refusals[0].kind, RefusalKind::Ambiguous);
    assert_eq!(refusals[0].alias, Some(r));
    let mut want = vec![id("lease_stays"), id("d01")];
    want.sort_by_key(|n| n.0);
    assert_eq!(refusals[0].rulings, want);
}

#[test]
fn a_reviewed_map_moves_the_fixture_alias_citations_to_the_ruling() {
    let snap = fair::snapshot(true);
    let g = graph(&snap);
    // Full id one way, short id the other, with a comment.
    let map = format!(
        "# reviewed 2026-10-13\n{} {}  # writer lease -> lease stays\n",
        id("lease_alias").0,
        short("lease_stays")
    );
    let (pairs, refusals) = parse_map(&g, &map);
    assert!(refusals.is_empty(), "{refusals:?}");
    let p = plan(&g, &pairs, refusals);
    assert_eq!(p.redirects.len(), 1);
    let r = &p.redirects[0];
    assert_eq!((r.alias, r.ruling), (id("lease_alias"), id("lease_stays")));
    assert_eq!(
        (r.alias_type, r.ruling_type),
        (ConceptType::Entity, ConceptType::Logic)
    );
    assert_eq!(r.source, PairSource::Reviewed);
    assert_eq!(
        (
            r.redirected,
            r.already_cited,
            r.self_dropped,
            r.cycle_refused
        ),
        (3, 0, 0, 0)
    );
    assert_eq!((p.retire.len(), p.add.len()), (3, 3));
    // Each new edge is the old citation re-pointed, with its times.
    for added in &p.add {
        let old = snap
            .edges
            .iter()
            .find(|e| p.retire.contains(&e.id) && e.source == added.source)
            .expect("one retired per added");
        assert_eq!(added.target, id("lease_stays"));
        assert_eq!(added.edge_type, EdgeType::Dependency);
        assert_eq!(
            (added.created_at, added.event_time, added.weight),
            (old.created_at, old.event_time, old.weight)
        );
        assert_ne!(added.id, old.id);
    }
    let after = graph(&apply(&snap, &p));
    let cites = |g: &Graph, n: NodeId| {
        g.edges()
            .filter(|e| e.edge_type == EdgeType::Dependency && e.target == n)
            .count()
    };
    assert_eq!(cites(&after, id("lease_alias")), 0);
    assert_eq!(cites(&after, id("lease_stays")), 9);
}

#[test]
fn apply_leaves_its_input_alone_and_is_deterministic() {
    let snap = fair::snapshot(true);
    let g = graph(&snap);
    let map = format!("{} {}", short("lease_alias"), short("lease_stays"));
    let (pairs, refusals) = parse_map(&g, &map);
    let p1 = plan(&g, &pairs, refusals.clone());
    let p2 = plan(&g, &pairs, refusals);
    assert_eq!(p1, p2);
    let before = snap.clone();
    let _ = apply(&snap, &p1);
    assert_eq!(snap, before);
}

#[test]
fn a_second_plan_on_the_result_is_empty() {
    let snap = fair::snapshot(true);
    let g = graph(&snap);
    let map = format!("{} {}", short("lease_alias"), short("lease_stays"));
    let (pairs, refusals) = parse_map(&g, &map);
    let first = plan(&g, &pairs, refusals);
    assert!(!first.is_noop());
    let after = graph(&apply(&snap, &first));
    let (pairs, refusals) = parse_map(&after, &map);
    let second = plan(&after, &pairs, refusals);
    assert!(second.is_noop(), "{second:?}");
    assert_eq!(second.redirects.len(), 1);
    assert_eq!(second.redirects[0].redirected, 0);
}

fn kinds(p: &AliasPlan) -> Vec<(RefusalKind, Option<usize>)> {
    let mut k: Vec<_> = p.refusals.iter().map(|r| (r.kind, r.line)).collect();
    k.sort_by_key(|(k, l)| (format!("{k:?}"), *l));
    k
}

#[test]
fn ambiguous_chained_cyclic_and_self_pairs_are_refused_and_nothing_moves() {
    let snap = fair::snapshot(true);
    let g = graph(&snap);
    let s = short;
    for (map, want) in [
        // One alias, two rulings.
        (
            format!(
                "{} {}\n{} {}",
                s("lease_alias"),
                s("lease_stays"),
                s("lease_alias"),
                s("wedge")
            ),
            vec![
                (RefusalKind::Ambiguous, Some(1)),
                (RefusalKind::Ambiguous, Some(2)),
            ],
        ),
        // a -> b, b -> c.
        (
            format!(
                "{} {}\n{} {}",
                s("lease_alias"),
                s("lease_stays"),
                s("lease_stays"),
                s("wedge")
            ),
            vec![(RefusalKind::Chain, Some(1)), (RefusalKind::Chain, Some(2))],
        ),
        // a -> b, b -> a.
        (
            format!(
                "{} {}\n{} {}",
                s("lease_alias"),
                s("lease_stays"),
                s("lease_stays"),
                s("lease_alias")
            ),
            vec![(RefusalKind::Cycle, Some(1)), (RefusalKind::Cycle, Some(2))],
        ),
        // a -> a.
        (
            format!("{} {}", s("lease_alias"), s("lease_alias")),
            vec![(RefusalKind::SelfRedirect, Some(1))],
        ),
    ] {
        let (pairs, refusals) = parse_map(&g, &map);
        let p = plan(&g, &pairs, refusals);
        assert!(p.redirects.is_empty(), "{map}: {:?}", p.redirects);
        assert!(p.is_noop(), "{map}");
        assert_eq!(kinds(&p), want, "{map}");
    }
}

#[test]
fn a_map_names_concepts_by_id_only() {
    let snap = fair::snapshot(true);
    let g = graph(&snap);
    let interaction = snap.interactions[0].id;
    let map = format!(
        "writer lease {ruling}\n\
         {alias}\n\
         {alias} 00000000\n\
         {alias} {interaction}\n\
         {alias} {ruling} extra\n",
        alias = short("lease_alias"),
        ruling = short("lease_stays"),
        interaction = interaction.0,
    );
    let (pairs, refusals) = parse_map(&g, &map);
    assert!(pairs.is_empty(), "{pairs:?}");
    let got: Vec<_> = refusals.iter().map(|r| (r.kind, r.line)).collect();
    assert_eq!(
        got,
        vec![
            // Three words: not a pair.
            (RefusalKind::Malformed, Some(1)),
            (RefusalKind::Malformed, Some(2)),
            (RefusalKind::Unresolved, Some(3)),
            (RefusalKind::NotAConcept, Some(4)),
            (RefusalKind::Malformed, Some(5)),
        ]
    );
    // A two-word line whose first word is content, not an id.
    let (_, refusals) = parse_map(&g, &format!("CHANGELOG.md {}", short("lease_stays")));
    assert_eq!(refusals[0].kind, RefusalKind::ByContent);
}

#[test]
fn each_citation_is_moved_kept_or_dropped_on_its_own_merits() {
    let mut snap = fair::snapshot(true);
    // a30 already cites the ruling: its alias edge is retired, nothing added.
    // The ruling cites its own alias: retired, never a self-loop.
    // The ruling depends on a31: moving a31's citation would close a cycle,
    // so it stays on the alias.
    let extra = [
        cite(&snap, id("a30"), id("lease_stays"), "already"),
        cite(&snap, id("lease_stays"), id("lease_alias"), "self"),
        cite(&snap, id("lease_stays"), id("a31"), "cycle"),
    ];
    snap.edges.extend(extra);
    let g = graph(&snap);
    let (pairs, refusals) = parse_map(
        &g,
        &format!("{} {}", short("lease_alias"), short("lease_stays")),
    );
    let p = plan(&g, &pairs, refusals);
    let r = &p.redirects[0];
    assert_eq!(
        (
            r.redirected,
            r.already_cited,
            r.self_dropped,
            r.cycle_refused
        ),
        (1, 1, 1, 1)
    );
    assert_eq!((p.retire.len(), p.add.len()), (3, 1));
    assert_eq!(p.add[0].source, id("a32"));
    // The overlay is a valid graph and the kept citation is still there.
    let after = graph(&apply(&snap, &p));
    assert!(after.edges().any(|e| e.source == id("a31")
        && e.target == id("lease_alias")
        && e.edge_type == EdgeType::Dependency));
    assert!(!after.edges().any(|e| e.source == e.target));
}

#[test]
fn other_dependents_of_an_alias_are_counted_and_kept() {
    let mut snap = fair::snapshot(true);
    let mut child = cite(&snap, id("lease_alias"), id("c01"), "child");
    child.edge_type = EdgeType::Hierarchical;
    snap.edges.push(child);
    let g = graph(&snap);
    let (pairs, refusals) = parse_map(
        &g,
        &format!("{} {}", short("lease_alias"), short("lease_stays")),
    );
    let p = plan(&g, &pairs, refusals);
    assert_eq!(p.redirects[0].other_dependents_kept, 1);
    assert_eq!(p.retire.len(), 3);
}

/// Review H1: the ruling is the `parent_of` parent of a concept that cites
/// its alias. Moving that citation would make `child -> ruling -> child`,
/// a cycle the graph's invariant refuses through `Hierarchical`, so the
/// citation stays on the alias and is counted; the rest still move, and
/// the overlay is a valid graph.
#[test]
fn a_rulings_child_citing_its_alias_is_kept_as_a_cycle() {
    let mut snap = fair::snapshot(true);
    let mut child = cite(&snap, id("lease_stays"), id("a30"), "parent-of-a30");
    child.edge_type = EdgeType::Hierarchical;
    snap.edges.push(child);
    let g = graph(&snap);
    let (pairs, refusals) = parse_map(
        &g,
        &format!("{} {}", short("lease_alias"), short("lease_stays")),
    );
    let p = plan(&g, &pairs, refusals);
    let r = &p.redirects[0];
    assert_eq!((r.redirected, r.cycle_refused), (2, 1));
    assert_eq!(r.citations_left(), 1);
    assert!(p.add.iter().all(|e| e.source != id("a30")));
    graph(&apply(&snap, &p));
}

#[test]
fn reviewed_pairs_carry_status_fan_in_and_type_warnings() {
    let mut snap = fair::snapshot(true);
    for c in snap
        .concepts
        .iter_mut()
        .filter(|c| c.id == id("lease_alias"))
    {
        c.canonization_status = CanonizationStatus::Venerable;
    }
    let g = graph(&snap);
    // Line 1 as intended; line 2 swapped (a ruling as the alias, a file as
    // the ruling); line 3 a second alias for lease_stays.
    let map = format!(
        "{} {}\n{} {}\n{} {}\n",
        short("lease_alias"),
        short("lease_stays"),
        short("wedge"),
        short("file_flush"),
        short("write_path_hub"),
        short("lease_stays"),
    );
    let (pairs, refusals) = parse_map(&g, &map);
    let p = plan(&g, &pairs, refusals);
    let lease = p
        .redirects
        .iter()
        .find(|r| r.alias == id("lease_alias"))
        .unwrap();
    assert_eq!(lease.alias_status, CanonizationStatus::Venerable);
    assert_eq!(lease.ruling_fan_in, 2);
    let mut w: Vec<_> = p.warnings.iter().map(|w| (w.kind, w.line)).collect();
    w.sort_by_key(|(k, l)| (format!("{k:?}"), *l));
    assert_eq!(
        w,
        vec![
            (WarningKind::AliasNotAnEntity, Some(2)),
            (WarningKind::RulingNotARuling, Some(2)),
        ]
    );
}

/// #198: a redirected citation is a declared `depends_on` between citer and
/// ruling, so applying the plan drops a suspected-supersession edge
/// between them (either way), as a citation by id does. One between other
/// concepts stays.
#[test]
fn a_redirect_settles_a_suspected_pair_between_citer_and_ruling() {
    let mut snap = fair::snapshot(true);
    let g = graph(&snap);
    let map = format!("{} {}\n", id("lease_alias").0, id("lease_stays").0);
    let (pairs, refusals) = parse_map(&g, &map);
    let p = plan(&g, &pairs, refusals);
    assert!(!p.add.is_empty());
    let citer = p.add[0].source;
    let suspected = |name: &str, source: NodeId, target: NodeId| {
        let mut e = cite(&snap, source, target, name);
        e.edge_type = EdgeType::SuspectedSupersedes;
        e
    };
    let settled = suspected("s1", id("lease_stays"), citer);
    let kept = suspected("s2", id("wedge"), id("d01"));
    let (settled_id, kept_id) = (settled.id, kept.id);
    snap.edges.extend([settled, kept]);
    let out = apply(&snap, &p);
    assert!(!out.edges.iter().any(|e| e.id == settled_id));
    assert!(out.edges.iter().any(|e| e.id == kept_id));
}
