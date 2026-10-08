//! The step-two score cut: thresholds, weights, exempt types, resources,
//! accesses and recency.

use super::*;

/// ALGO-11: the cut consults [`ConceptType::eviction_resistance`].
///
/// `Entity` (resistance 1.2) and `Resource` (1.0) leaves of identical
/// structure differ only by the type modifier (+0.05 vs 0), so
/// `score_r < score_e`. Choosing `min_concept_score = 1.1 · score_e` puts a
/// **flat** bar above both scores — both collected — while the Entity's own
/// bar (`1.1/1.2 · score_e`) sits below its score: the resistance factor is
/// the only thing that saves it.
///
/// (Before issue #29 this pinned Entity against Logic, which share a
/// modifier; Logic is now exempt from the score cut, see
/// `logic_constraint_and_observation_are_exempt_from_the_score_cut`.)
#[test]
fn eviction_resistance_discriminates_at_the_threshold_boundary() {
    let mut g = Graph::new(sid());
    let i1 = interaction(1, None);
    let i2 = Interaction {
        created_at: ts(100),
        ..interaction(2, Some(1))
    };
    let iid = i1.id;
    g.insert_interaction(i1).unwrap();
    g.insert_interaction(i2).unwrap();

    g.insert_concept(concept(10, 1, "anchor", ConceptType::Entity), iid)
        .unwrap();
    g.insert_concept(concept(11, 1, "entity leaf", ConceptType::Entity), iid)
        .unwrap();
    g.insert_concept(concept(12, 1, "resource leaf", ConceptType::Resource), iid)
        .unwrap();
    g.upsert_edge(edge(100, 11, 10, EdgeType::Dependency, 1.0, 0))
        .unwrap();
    g.upsert_edge(edge(101, 12, 10, EdgeType::Dependency, 1.0, 0))
        .unwrap();

    let ctx = crate::daemon::score::SessionContext::compute(&g);
    let base = aged_params();
    let score_of = |g: &Graph, id: NodeId, ctx: &crate::daemon::score::SessionContext| {
        let c = match g.node(id).unwrap() {
            Node::Concept(c) => c.clone(),
            _ => unreachable!(),
        };
        eviction_score(g, &c, ctx, base)
    };
    let entity_score = score_of(&g, nid(11), &ctx);
    let resource_score = score_of(&g, nid(12), &ctx);
    assert!(
        resource_score < entity_score,
        "test premise: same structure, Entity carries the larger modifier"
    );

    let params = GcParams {
        min_concept_score: entity_score * 1.1,
        ..base
    };
    assert!(
        params.min_concept_score > entity_score,
        "a flat bar would collect the Entity too"
    );
    assert!(
        eviction_threshold(params.min_concept_score, ConceptType::Entity) < entity_score,
        "the Entity's own bar must sit below its score"
    );
    assert!(
        eviction_threshold(params.min_concept_score, ConceptType::Resource) > resource_score,
        "the Resource bar must sit above its score"
    );

    let outcome = run(&mut g, params);
    assert_eq!(
        outcome.concepts_collected,
        vec![nid(12)],
        "only the less resistant type is collected"
    );
    assert!(g.node(nid(11)).is_some(), "Entity (1.2) resists the cut");
    assert!(g.node(nid(12)).is_none(), "Resource (1.0) does not");
}

/// ALGO-4: the cut uses the **session's** weights, not a second default.
///
/// The concept's whole value is structural (density); weights that zero
/// density and put everything on recency (which is 0 for it) drop it under
/// the bar. Pre-fix, `run` hardcoded `ScoringWeights::default()` and the
/// session's weights could not reach the cut at all, so it survived.
#[test]
fn step_two_cut_honors_the_sessions_scoring_weights() {
    let build = || {
        let mut g = Graph::new(sid());
        let i1 = interaction(1, None);
        let i2 = Interaction {
            created_at: ts(100),
            ..interaction(2, Some(1))
        };
        let iid = i1.id;
        g.insert_interaction(i1).unwrap();
        g.insert_interaction(i2).unwrap();
        g.insert_concept(concept(10, 1, "dense anchor", ConceptType::Entity), iid)
            .unwrap();
        g.insert_concept(concept(11, 1, "dense leaf", ConceptType::Entity), iid)
            .unwrap();
        g.upsert_edge(edge(100, 11, 10, EdgeType::Dependency, 1.0, 0))
            .unwrap();
        g
    };

    // Default weights (density 0.35): the leaf's density carries it.
    let mut g = build();
    let outcome = run(&mut g, aged_params());
    assert!(
        outcome.concepts_collected.is_empty(),
        "under the session's default weights the leaf is worth keeping"
    );

    // Session weights that value only recency, which the leaf (aged past
    // the window) has none of.
    let mut g = build();
    let outcome = run(
        &mut g,
        GcParams {
            weights: ScoringWeights {
                recency: 1.0,
                frequency: 0.0,
                session_activity: 0.0,
                density: 0.0,
            },
            ..aged_params()
        },
    );
    assert!(
        outcome.concepts_collected.contains(&nid(11)),
        "recency-only weights must reach the cut, collected: {:?}",
        outcome.concepts_collected
    );
}

/// ALGO-10: non-finite weights must not disable collection.
///
/// Pre-fix a `NaN` weight produced a `NaN` composite, and `NaN < threshold`
/// is `false` — GC silently stopped collecting anything at all.
#[test]
fn non_finite_weights_do_not_disable_the_cut() {
    let mut g = Graph::new(sid());
    let i1 = interaction(1, None);
    let iid = i1.id;
    g.insert_interaction(i1).unwrap();
    // A Resource with only its Derives edge in a session with a hub —
    // dead under any sane weighting.
    g.insert_concept(concept(11, 1, "low value", ConceptType::Resource), iid)
        .unwrap();
    for id in [13u64, 14, 15, 16] {
        g.insert_concept(
            concept(id, 1, &format!("anchor {id}"), ConceptType::Entity),
            iid,
        )
        .unwrap();
    }
    for (eid, tgt) in [(101u64, 14u64), (102, 15), (103, 16)] {
        g.upsert_edge(edge(eid, 13, tgt, EdgeType::Dependency, 1.0, 0))
            .unwrap();
    }

    let outcome = run(
        &mut g,
        GcParams {
            weights: ScoringWeights {
                recency: f64::NAN,
                frequency: f64::INFINITY,
                session_activity: -1.0,
                density: 0.35,
            },
            // Every concept is far under this bar, so only a NaN composite
            // (`NaN < bar` is false) could keep the leaf.
            min_concept_score: 5.0,
            ..default_params()
        },
    );
    assert!(
        outcome.concepts_collected.contains(&nid(11)),
        "garbage weights must degrade to zeroed dimensions, not a NaN score \
             that silently disables collection"
    );
}

/// Issue #29 option (a), extended to Observation by operator decision
/// (2026-10-07): Logic, Constraint and Observation are exempt from the
/// score cut. With the same structure and zero recency, an Entity and an
/// isolated Resource leaf are collected and the exempt types are not, with
/// the bar raised far above every score. Pre-fix the Logic leaf went with
/// them (the #29 dry run collected 42 Logic concepts — early operator
/// rulings among them — on one sweep) and Observations were the first
/// victims (324 of 467).
#[test]
fn logic_constraint_and_observation_are_exempt_from_the_score_cut() {
    let mut g = hub_session(&[
        (20, ConceptType::Observation),
        (21, ConceptType::Logic),
        (22, ConceptType::Constraint),
        (23, ConceptType::Resource),
        (24, ConceptType::Entity),
    ]);
    // A bar so high every leaf is far under it, whatever its type.
    let params = GcParams {
        min_concept_score: 5.0,
        ..aged_params()
    };
    let outcome = run(&mut g, params);
    assert!(outcome.concepts_collected.contains(&nid(23)));
    assert!(outcome.concepts_collected.contains(&nid(24)));
    for (id, ty) in [(20u64, "Observation"), (21, "Logic"), (22, "Constraint")] {
        assert!(
            g.node(nid(id)).is_some(),
            "{ty} is exempt from the score cut"
        );
    }
    for ty in [
        ConceptType::Logic,
        ConceptType::Constraint,
        ConceptType::Observation,
    ] {
        assert!(ty.exempt_from_gc_score_cut(), "{ty:?} is exempt");
    }
    for ty in [ConceptType::Entity, ConceptType::Resource] {
        assert!(!ty.exempt_from_gc_score_cut(), "{ty:?} stays under the cut");
    }
}

/// Issue #29 operator decision: a Resource that other concepts depend on
/// survives the score cut; an isolated, untouched one ages out. Both
/// senses of "dependents" are covered — the incoming edge `record_action`
/// writes into what an action depends on (23) and produced (24), and the
/// action node itself (22), whose blast radius is non-zero because it is
/// the only structural source of 23. 25 has only its Derives edge and is
/// collected. A Resource whose only structural edge is a self-loop has no
/// dependent (26).
#[test]
fn resources_with_dependents_survive_the_score_cut_isolated_ones_age_out() {
    let mut g = hub_session(&[
        (22, ConceptType::Resource),
        (23, ConceptType::Resource),
        (24, ConceptType::Resource),
        (25, ConceptType::Resource),
        (26, ConceptType::Resource),
        (27, ConceptType::Entity),
    ]);
    // record_action shape: action 22 -> depends_on 23, action 22 -> produces 24.
    g.upsert_edge(edge(200, 22, 23, EdgeType::Dependency, 1.0, 0))
        .unwrap();
    g.upsert_edge(edge(201, 22, 24, EdgeType::Causal, 1.0, 0))
        .unwrap();
    // 24 has a second producer, so 22 is not its sole source; 24 is still
    // depended on (incoming), and 27 (an Entity) is not protected by this.
    g.upsert_edge(edge(202, 27, 24, EdgeType::Causal, 1.0, 0))
        .unwrap();
    g.upsert_edge(edge(203, 26, 26, EdgeType::Dependency, 1.0, 0))
        .unwrap();
    let protected = resources_with_dependents(&g);
    assert!(protected.contains(&nid(22)), "blast radius > 0");
    assert!(protected.contains(&nid(23)), "incoming Dependency");
    assert!(protected.contains(&nid(24)), "incoming Causal");
    assert!(!protected.contains(&nid(25)), "isolated");
    assert!(
        !protected.contains(&nid(26)),
        "a self-loop is not a dependent"
    );
    assert!(!protected.contains(&nid(27)), "only Resources are spared");

    let params = GcParams {
        min_concept_score: 5.0,
        ..aged_params()
    };
    let outcome = run(&mut g, params);
    for kept in [22u64, 23, 24] {
        assert!(
            g.node(nid(kept)).is_some(),
            "Resource {kept} has dependents and must survive the score cut"
        );
    }
    assert!(outcome.concepts_collected.contains(&nid(25)));
    assert!(outcome.concepts_collected.contains(&nid(26)));
    assert!(outcome.concepts_collected.contains(&nid(27)));
    assert_eq!(outcome.resources_spared_by_dependents, 3);
}

/// A Resource under its bar with dependents is counted as spared only if
/// it survives: one that is also a disconnected component (here an island
/// of two Resources joined only to each other, each the other's dependent)
/// is collected by step 3 and is not counted.
#[test]
fn a_spared_resource_collected_as_disconnected_is_not_counted_as_spared() {
    let mut g = hub_session(&[(22, ConceptType::Resource), (23, ConceptType::Resource)]);
    // 22 depends on 23, and they are linked to the temporal chain only
    // through their Derives edges: reachable, so both are spared.
    g.upsert_edge(edge(200, 22, 23, EdgeType::Dependency, 1.0, 0))
        .unwrap();
    // An island: 40 -> 41, neither reachable from the chain.
    let a = insert_isolated(&mut g, concept(40, 1, "island a", ConceptType::Resource), 1);
    let b = insert_isolated(&mut g, concept(41, 1, "island b", ConceptType::Resource), 1);
    g.upsert_edge(edge(400, 40, 41, EdgeType::Dependency, 1.0, 0))
        .unwrap();
    assert!(resources_with_dependents(&g).contains(&a), "premise");
    assert!(resources_with_dependents(&g).contains(&b), "premise");
    let params = GcParams {
        min_concept_score: 5.0,
        ..aged_params()
    };
    let outcome = run(&mut g, params);
    assert!(outcome.concepts_collected.contains(&a));
    assert!(outcome.concepts_collected.contains(&b));
    assert_eq!(
        outcome.resources_spared_by_dependents, 2,
        "only 22 and 23 were spared; the island went as disconnected"
    );
    assert!(g.node(nid(22)).is_some() && g.node(nid(23)).is_some());
}

/// An isolated Resource touched recently survives on recency alone, and
/// the same Resource left untouched past the window is collected: the
/// dependents rule does not freeze Resources in general.
#[test]
fn an_isolated_resource_survives_while_fresh_and_ages_out_untouched() {
    let mut fresh = hub_session(&[(25, ConceptType::Resource)]);
    let outcome = run(&mut fresh, default_params());
    assert!(!outcome.concepts_collected.contains(&nid(25)));
    assert!(fresh.node(nid(25)).is_some());

    let mut old = hub_session(&[(25, ConceptType::Resource)]);
    let outcome = run(
        &mut old,
        GcParams {
            min_concept_score: 5.0,
            ..aged_params()
        },
    );
    assert!(outcome.concepts_collected.contains(&nid(25)));
    assert_eq!(outcome.resources_spared_by_dependents, 0);
}

/// Issue #29 item 1: an access on one concept never lowers any other
/// concept's GC score. Pre-fix, the first access anywhere switched the
/// whole session to the full composite (ALGO-1), which is `0.8 ×` the
/// live one on the weighted part for every unread concept — on the Metal
/// rig snapshot one access on one Entity took the first sweep from 159
/// candidates to 412. Here the hub (13) is read heavily; every other
/// concept's score must be bit-identical, and the collection set at a bar
/// just above the unread leaves' scores must not grow.
#[test]
fn an_access_on_another_concept_never_lowers_an_unread_concepts_gc_score() {
    let leaves = [
        (20, ConceptType::Resource),
        (21, ConceptType::Entity),
        (22, ConceptType::Entity),
    ];
    let unread = hub_session(&leaves);
    let params = aged_params();
    let read_at = params.now - ChronoDuration::days(1);
    let read = hub_session_patched(&leaves, |c| {
        if c.id == nid(13) {
            Concept {
                access_count: 40,
                last_accessed: Some(read_at),
                ..c
            }
        } else {
            c
        }
    });
    let score_in = |g: &Graph, id: u64| {
        let ctx = crate::daemon::score::SessionContext::compute(g);
        let c = match g.node(nid(id)) {
            Some(crate::types::Node::Concept(c)) => c.clone(),
            _ => panic!("concept {id}"),
        };
        eviction_score(g, &c, &ctx, params)
    };
    for id in [14u64, 15, 16, 20, 21, 22] {
        assert_eq!(
            score_in(&read, id).to_bits(),
            score_in(&unread, id).to_bits(),
            "concept {id}: another concept's access changed its GC score"
        );
    }
    assert!(score_in(&read, 13) > score_in(&unread, 13));

    // End to end: a bar between the leaves' (unread) scores and the
    // full-composite version of them. The old session-wide switch put the
    // leaves under it the moment the hub was read; now nothing changes.
    let resource_bar_scale =
        |id: u64| score_in(&unread, id) * ConceptType::Resource.eviction_resistance();
    let params_bar = GcParams {
        min_concept_score: resource_bar_scale(20) * 0.95,
        ..params
    };
    let mut a = unread.clone();
    let mut b = read.clone();
    let before = run(&mut a, params_bar);
    let after = run(&mut b, params_bar);
    assert_eq!(before.concepts_collected, after.concepts_collected);
    assert!(!after.concepts_collected.contains(&nid(20)));
}

/// Issue #29 item 1: reading a concept can only raise its own GC score —
/// for any access count and any access time (none, before creation,
/// mid-window, now), its score is at least its never-read score.
#[test]
fn an_accessed_concepts_gc_score_is_at_least_its_no_access_score() {
    let params = aged_params();
    let created = ts(0);
    for ty in [
        ConceptType::Observation,
        ConceptType::Resource,
        ConceptType::Entity,
    ] {
        let leaves = [(20u64, ty)];
        let base_graph = hub_session(&leaves);
        let ctx = crate::daemon::score::SessionContext::compute(&base_graph);
        let base_c = match base_graph.node(nid(20)) {
            Some(crate::types::Node::Concept(c)) => c.clone(),
            _ => unreachable!(),
        };
        let base = eviction_score(&base_graph, &base_c, &ctx, params);
        for count in [1, 3, 25, 1_000, i32::MAX] {
            for at in [
                None,
                Some(created - ChronoDuration::days(5)),
                Some(created + ChronoDuration::days(45)),
                Some(params.now),
            ] {
                let g = hub_session_patched(&leaves, |c| {
                    if c.id == nid(20) {
                        Concept {
                            access_count: count,
                            last_accessed: at,
                            ..c
                        }
                    } else {
                        c
                    }
                });
                let ctx = crate::daemon::score::SessionContext::compute(&g);
                let c = match g.node(nid(20)) {
                    Some(crate::types::Node::Concept(c)) => c.clone(),
                    _ => unreachable!(),
                };
                let s = eviction_score(&g, &c, &ctx, params);
                assert!(
                    s >= base,
                    "{ty:?} count {count} at {at:?}: {s} < no-access {base}"
                );
                assert!(s.is_finite() && s <= 1.0 + crate::daemon::score::MAX_BONUS);
            }
        }
    }
}

/// Issue #30 x #29: `last_accessed` is GC's recency anchor, so an old
/// Entity and an isolated Resource that agents keep recalling are not
/// collected by the 365-day cut, while the same concepts untouched are.
/// The access goes through the real write path (`Graph::record_accesses`),
/// not a patched field. A second comparison holds the access count fixed
/// (one read, long ago versus recently) so the frequency term cannot be
/// what saves the concept: only the recency anchor separates them.
#[test]
fn a_recalled_old_entity_and_isolated_resource_survive_the_365_day_cut() {
    let params = aged_params();
    let at = params.now - ChronoDuration::days(1);
    for ty in [ConceptType::Resource, ConceptType::Entity] {
        // 20 is recalled, 22 is not; both are old, isolated leaves.
        let leaves = [(20u64, ty), (22, ty)];
        let untouched = hub_session(&leaves);
        let mut recalled = untouched.clone();
        assert_eq!(recalled.record_accesses(&[(nid(20), 1, at)]), 1);

        let score_of = |g: &Graph, id: u64| {
            let ctx = crate::daemon::score::SessionContext::compute(g);
            let c = match g.node(nid(id)) {
                Some(crate::types::Node::Concept(c)) => c.clone(),
                _ => panic!("concept {id}"),
            };
            eviction_score(g, &c, &ctx, params)
        };
        let old = score_of(&untouched, 20);
        let read = score_of(&recalled, 20);
        assert!(read > old, "{ty:?}: a recent read lifts the score");
        assert_eq!(
            score_of(&recalled, 22).to_bits(),
            score_of(&untouched, 22).to_bits(),
            "{ty:?}: the unread twin is unchanged"
        );

        // A bar between the two scores (the bar is `min / resistance`).
        let bar_params = GcParams {
            min_concept_score: (old + read) / 2.0 * ty.eviction_resistance(),
            ..params
        };
        let mut baseline = untouched.clone();
        let base = run(&mut baseline, bar_params);
        assert!(
            base.concepts_collected.contains(&nid(20))
                && base.concepts_collected.contains(&nid(22)),
            "{ty:?} setup: untouched old leaves age out, got {:?}",
            base.concepts_collected
        );
        let out = run(&mut recalled, bar_params);
        assert!(
            !out.concepts_collected.contains(&nid(20)),
            "{ty:?}: the recalled leaf must survive the recency cut"
        );
        assert!(recalled.node(nid(20)).is_some());
        assert!(
            out.concepts_collected.contains(&nid(22)),
            "{ty:?}: the unread twin still ages out"
        );

        // Recency alone: the same single read, once long ago (at creation)
        // and once recently. The frequency term is identical, so only the
        // `last_accessed` recency anchor can separate them.
        let created = match untouched.node(nid(20)) {
            Some(crate::types::Node::Concept(c)) => c.created_at,
            _ => panic!("concept 20"),
        };
        let mut stale_read = untouched.clone();
        assert_eq!(stale_read.record_accesses(&[(nid(20), 1, created)]), 1);
        let stale = score_of(&stale_read, 20);
        assert!(
            read > stale,
            "{ty:?}: a recent read must outscore an old read of the same count"
        );
        let recency_bar = GcParams {
            min_concept_score: (stale + read) / 2.0 * ty.eviction_resistance(),
            ..params
        };
        let mut recent = untouched.clone();
        assert_eq!(recent.record_accesses(&[(nid(20), 1, at)]), 1);
        assert!(
            run(&mut stale_read, recency_bar)
                .concepts_collected
                .contains(&nid(20)),
            "{ty:?}: a leaf last read long ago ages out"
        );
        assert!(
            !run(&mut recent, recency_bar)
                .concepts_collected
                .contains(&nid(20)),
            "{ty:?}: the same leaf read recently survives on recency alone"
        );
    }
}

/// The exemption is from the score cut only: a Logic concept with no edge
/// at all is still an orphan, and an unreachable one is still a
/// disconnected component.
#[test]
fn exempt_types_are_still_collected_as_orphans_and_islands() {
    let mut g = Graph::new(sid());
    let i1 = interaction(1, None);
    let iid = i1.id;
    g.insert_interaction(i1).unwrap();
    g.insert_concept(concept(10, 1, "anchored", ConceptType::Entity), iid)
        .unwrap();
    let orphan = insert_isolated(&mut g, concept(20, 1, "lone rule", ConceptType::Logic), 1);
    let a = insert_isolated(
        &mut g,
        concept(21, 1, "island a", ConceptType::Constraint),
        1,
    );
    let b = insert_isolated(&mut g, concept(22, 1, "island b", ConceptType::Logic), 1);
    g.upsert_edge(edge(100, 21, 22, EdgeType::Dependency, 1.0, 0))
        .unwrap();
    let note = insert_isolated(
        &mut g,
        concept(23, 1, "lone note", ConceptType::Observation),
        1,
    );
    let c = insert_isolated(
        &mut g,
        concept(24, 1, "island c", ConceptType::Observation),
        1,
    );
    g.upsert_edge(edge(101, 24, 21, EdgeType::Dependency, 1.0, 0))
        .unwrap();
    let outcome = run(&mut g, default_params());
    assert_eq!(outcome.concepts_collected, vec![orphan, a, b, note, c]);
}

/// Issue #29: GC's eviction recency is time since last touch, not the
/// concept's position in the session span. The same concept, the same
/// clock, two sessions whose spans differ only by a later interaction:
/// span-relative recency moves from 1.0 to ~0 (that is what made the dry
/// run's collections grow with session age), GC's eviction score does not
/// move at all.
#[test]
fn eviction_recency_ignores_the_session_span() {
    let leaf = |g: &Graph| match g.node(nid(20)).unwrap() {
        Node::Concept(c) => c.clone(),
        _ => unreachable!(),
    };
    let build = |extra_span: bool| {
        let mut g = hub_session(&[]);
        // Created at the end of the short span.
        let c = Concept {
            created_at: ts(100),
            ..concept(20, 2, "late observation", ConceptType::Observation)
        };
        g.insert_concept(c, nid(2)).unwrap();
        if extra_span {
            let i3 = Interaction {
                created_at: ts(100) + ChronoDuration::days(9),
                ..interaction(3, Some(2))
            };
            g.insert_interaction(i3).unwrap();
        }
        g
    };
    let params = GcParams {
        now: ts(100) + ChronoDuration::days(10),
        ..Default::default()
    };
    let short = build(false);
    let long = build(true);
    let span_recency = |g: &Graph| {
        let ctx = crate::daemon::score::SessionContext::compute(g);
        crate::daemon::score::score_concept(g, &leaf(g), &ctx).recency
    };
    assert_eq!(span_recency(&short), 1.0, "premise: end of the short span");
    assert!(span_recency(&long) < 0.01, "premise: start of the long one");

    let gc_score = |g: &Graph| {
        let ctx = crate::daemon::score::SessionContext::compute(g);
        eviction_score(g, &leaf(g), &ctx, params)
    };
    let (a, b) = (gc_score(&short), gc_score(&long));
    // The extra interaction does not change the leaf's degree or the hub,
    // only `session_activity`'s denominator (1/2 → 1/3); recency itself
    // is identical.
    let expected = eviction_recency(&leaf(&short), params.now, params.recency_window);
    let window_days = params.recency_window.num_days() as f64;
    assert!((expected - (1.0 - 10.0 / window_days)).abs() < 1e-3);
    assert_eq!(
        eviction_recency(&leaf(&long), params.now, params.recency_window),
        expected
    );
    assert!(
        (a - b).abs() < 0.05,
        "span growth alone must not move GC's score materially: {a} vs {b}"
    );
}

/// The window is a year (operator decision, 2026-10-07): a concept
/// untouched for six months keeps about half its eviction recency, and one
/// untouched for 365 days has none.
#[test]
fn the_recency_window_is_a_year() {
    assert_eq!(GC_RECENCY_WINDOW, ChronoDuration::days(365));
    assert_eq!(GcParams::default().recency_window, GC_RECENCY_WINDOW);
    let c = concept(1, 1, "c", ConceptType::Entity);
    let half = eviction_recency(&c, ts(0) + ChronoDuration::days(183), GC_RECENCY_WINDOW);
    assert!((half - 0.5).abs() < 0.01, "{half}");
    assert_eq!(
        eviction_recency(&c, ts(0) + ChronoDuration::days(365), GC_RECENCY_WINDOW),
        0.0
    );
}

/// `eviction_recency`'s edges: linear in age inside the window, 0 past
/// it, a fresh access restores it, future touches clamp to 1, and a
/// non-positive window cannot divide by zero.
#[test]
fn eviction_recency_is_linear_clamped_and_access_aware() {
    let w = ChronoDuration::days(30);
    let c = concept(1, 1, "c", ConceptType::Entity); // created ts(0)
    assert_eq!(eviction_recency(&c, ts(0), w), 1.0);
    let half = eviction_recency(&c, ts(0) + ChronoDuration::days(15), w);
    assert!((half - 0.5).abs() < 1e-9);
    assert_eq!(
        eviction_recency(&c, ts(0) + ChronoDuration::days(31), w),
        0.0
    );
    assert_eq!(
        eviction_recency(&c, ts(0) - ChronoDuration::days(1), w),
        1.0
    );
    let touched = Concept {
        access_count: 1,
        last_accessed: Some(ts(0) + ChronoDuration::days(40)),
        ..c.clone()
    };
    assert_eq!(
        eviction_recency(&touched, ts(0) + ChronoDuration::days(40), w),
        1.0,
        "a recall (issue #30) re-anchors recency"
    );
    // An access stamp older than creation cannot age the concept.
    let odd = Concept {
        last_accessed: Some(ts(0) - ChronoDuration::days(100)),
        ..c.clone()
    };
    assert_eq!(eviction_recency(&odd, ts(0), w), 1.0);
    assert_eq!(eviction_recency(&c, ts(0), ChronoDuration::zero()), 0.0);
}

/// The span-anchored ranking is untouched: `rescore` (recall, Stage 1)
/// still uses span-relative recency; only GC's cut changed.
#[test]
fn daemon_ranking_keeps_span_relative_recency() {
    let g = hub_session(&[(20, ConceptType::Observation)]);
    let ctx = crate::daemon::score::SessionContext::compute(&g);
    let c = match g.node(nid(20)).unwrap() {
        Node::Concept(c) => c.clone(),
        _ => unreachable!(),
    };
    assert_eq!(
        crate::daemon::score::score_concept(&g, &c, &ctx).recency,
        0.0,
        "ts(0) is the start of the span"
    );
    assert_eq!(
        eviction_recency(&c, ts(100), GC_RECENCY_WINDOW),
        1.0 - 100.0 / GC_RECENCY_WINDOW.num_minutes() as f64,
    );
}
