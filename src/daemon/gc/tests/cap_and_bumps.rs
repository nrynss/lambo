//! The collection cap and survivor bumps: deferral, ordering and chunked
//! convergence.

use super::*;

// ------------------------------------------------------------------
// CONC-6 / XP-10 — bounded survivor-bump burst
// ------------------------------------------------------------------

/// A hub interaction plus `n` Canonical (protected) concepts, all
/// surviving every GC step, so step 5's survivor set is exactly `n`.
fn n_survivor_graph(n: u64) -> (Graph, Vec<NodeId>) {
    let mut g = Graph::new(sid());
    g.insert_interaction(interaction(1, None)).unwrap();
    let mut ids = Vec::new();
    for i in 0..n {
        let c = Concept {
            canonization_status: CanonizationStatus::Canonical,
            ..concept(100 + i, 1, &format!("concept {i}"), ConceptType::Entity)
        };
        ids.push(c.id);
        g.insert_concept(c, nid(1)).unwrap();
    }
    ids.sort_by_key(|id| id.0);
    (g, ids)
}

/// CONC-6/XP-10: one run bumps at most `max_survivor_bumps` survivors and
/// hands the rest back, so a sweep at the advisory ceiling cannot enqueue
/// twenty flush batches of full-`Concept` clones from inside the write
/// guard.
#[test]
fn survivor_bumps_are_chunked_and_the_remainder_is_reported() {
    let (mut g, ids) = n_survivor_graph(10);
    g.drain_log();
    let params = GcParams {
        max_survivor_bumps: 4,
        ..default_params()
    };
    let outcome = run(&mut g, params);

    assert_eq!(outcome.survivors, ids, "every concept survived");
    // Issue #29: the bumps go in the sweep's drain order (a rotation of
    // the id order), and the tail past the chunk is deferred in that order.
    let order = survivor_drain_order(&ids, outcome.epoch_before);
    assert_eq!(
        outcome.survivors_pending,
        order[4..],
        "the tail past the chunk is deferred, in drain order"
    );
    let upserts = g
        .drain_log()
        .mutations
        .iter()
        .filter(|m| matches!(m, Mutation::UpsertNode { .. }))
        .count();
    assert_eq!(upserts, 4, "one flush chunk of upserts, not ten");
    for (i, id) in order.iter().enumerate() {
        let c = match g.node(*id).unwrap() {
            Node::Concept(c) => c,
            _ => unreachable!(),
        };
        let expected = i32::from(i < 4);
        assert_eq!(c.gc_survived, expected, "only the first chunk bumped yet");
    }
}

/// CONC-6/XP-10 convergence: draining the deferred bumps leaves the graph
/// and the emitted mutation multiset identical to an unchunked sweep —
/// chunking changes *when*, never *which*.
#[test]
fn chunked_bumps_converge_to_the_unchunked_result() {
    let (mut chunked, ids) = n_survivor_graph(10);
    let (mut whole, _) = n_survivor_graph(10);
    chunked.drain_log();
    whole.drain_log();

    let mut pending = run(
        &mut chunked,
        GcParams {
            max_survivor_bumps: 3,
            ..default_params()
        },
    )
    .survivors_pending;
    let mut drains = 0;
    while !pending.is_empty() {
        drain_survivor_bumps(&mut chunked, &mut pending, 3);
        drains += 1;
        assert!(drains < 10, "the drain must terminate");
    }

    let unchunked = run(
        &mut whole,
        GcParams {
            max_survivor_bumps: usize::MAX,
            ..default_params()
        },
    );
    assert!(unchunked.survivors_pending.is_empty());

    for id in &ids {
        let a = match chunked.node(*id).unwrap() {
            Node::Concept(c) => c.gc_survived,
            _ => unreachable!(),
        };
        let b = match whole.node(*id).unwrap() {
            Node::Concept(c) => c.gc_survived,
            _ => unreachable!(),
        };
        assert_eq!(a, 1, "exactly one bump per survivor per run");
        assert_eq!(a, b, "chunked and unchunked agree for {id}");
    }
    // Same mutation multiset: one UpsertNode per survivor either way.
    let count_upserts = |g: &mut Graph| {
        g.drain_log()
            .mutations
            .iter()
            .filter(|m| matches!(m, Mutation::UpsertNode { .. }))
            .count()
    };
    assert_eq!(count_upserts(&mut chunked), count_upserts(&mut whole));
}

/// CONC-6/XP-10: a concept collected before its deferred bump lands is
/// skipped, never resurrected — the store already has its `DeleteNode`.
#[test]
fn a_collected_concept_absorbs_its_pending_bump_silently() {
    let (mut g, ids) = n_survivor_graph(4);
    let mut pending = run(
        &mut g,
        GcParams {
            max_survivor_bumps: 1,
            ..default_params()
        },
    )
    .survivors_pending;
    assert_eq!(pending.len(), 3);

    // Drop one pending concept (a demotion + a later sweep would do this).
    let gone = pending[0];
    g.remove_node(gone).unwrap();
    g.drain_log();

    let applied = drain_survivor_bumps(&mut g, &mut pending, 10);
    assert_eq!(
        applied, 2,
        "the removed concept is skipped, not resurrected"
    );
    assert!(pending.is_empty());
    assert!(g.node(gone).is_none());
    assert!(!g
        .drain_log()
        .mutations
        .iter()
        .any(|m| matches!(m, Mutation::UpsertNode { node: Node::Concept(c) } if c.id == gone)));
    for id in ids.iter().filter(|id| **id != gone) {
        let c = match g.node(*id).unwrap() {
            Node::Concept(c) => c,
            _ => unreachable!(),
        };
        assert_eq!(c.gc_survived, 1);
    }
}

/// Issue #29: collections per sweep are capped; the cap is reported, the
/// held-back candidates survive and are counted once (an orphan is also a
/// disconnected component), and the next sweep takes the next slice.
#[test]
fn the_collection_cap_binds_reports_and_defers() {
    let mut g = Graph::new(sid());
    let i1 = interaction(1, None);
    let iid = i1.id;
    g.insert_interaction(i1).unwrap();
    g.insert_concept(concept(10, 1, "anchored", ConceptType::Entity), iid)
        .unwrap();
    let orphans: Vec<NodeId> = (100..112u64)
        .map(|n| {
            insert_isolated(
                &mut g,
                concept(n, 1, &format!("o{n}"), ConceptType::Entity),
                1,
            )
        })
        .collect();
    let params = GcParams {
        max_collect_fraction: 0.0,
        min_collect_cap: 5,
        ..default_params()
    };
    let first = run(&mut g, params);
    assert_eq!(first.collection_cap, 5);
    assert_eq!(first.concepts_collected, orphans[..5].to_vec());
    assert_eq!(first.collections_deferred, 7, "counted once, not twice");
    assert!(first.cap_bound());
    assert!(
        first
            .warnings
            .iter()
            .any(|w| w.contains("collection cap bound")),
        "{:?}",
        first.warnings
    );
    for id in &orphans[5..] {
        assert!(g.node(*id).is_some(), "held back, not collected");
    }
    let second = run(&mut g, params);
    assert_eq!(second.concepts_collected, orphans[5..10].to_vec());
    let third = run(&mut g, params);
    assert_eq!(third.concepts_collected, orphans[10..].to_vec());
    assert!(!third.cap_bound());
    assert!(!third.warnings.iter().any(|w| w.contains("collection cap")));
}

/// Issue #29 item 4: when the cap binds with orphans present, the orphans
/// are taken first and the score cut gets only what is left — weakest
/// first. Item 5: the candidate held back keeps its `gc_survived` (it is
/// not credited with surviving a sweep that judged it collectable), while
/// every real survivor takes its bump.
#[test]
fn the_cap_takes_orphans_before_score_cut_candidates() {
    let mut g = hub_session(&[(20, ConceptType::Entity), (21, ConceptType::Resource)]);
    let orphans: Vec<NodeId> = (30..33u64)
        .map(|n| {
            insert_isolated(
                &mut g,
                concept(n, 1, &format!("o{n}"), ConceptType::Entity),
                1,
            )
        })
        .collect();
    // Score-cut candidates are 13..16 and 20 (Entities) and 21 (an
    // isolated Resource) under a bar of 5.0; the cap is 4: three orphans
    // + the single weakest.
    let params = GcParams {
        min_concept_score: 5.0,
        max_collect_fraction: 0.0,
        min_collect_cap: 4,
        ..aged_params()
    };
    let outcome = run(&mut g, params);
    for o in &orphans {
        assert!(outcome.concepts_collected.contains(o), "orphan {o:?} first");
    }
    assert_eq!(outcome.concepts_collected.len(), 4);
    assert!(
        outcome.concepts_collected.contains(&nid(21)),
        "then the score cut, furthest under its bar first (the Resource)"
    );
    assert!(outcome.cap_bound());
    assert_eq!(outcome.collections_deferred, outcome.deferred.len());
    assert_eq!(outcome.deferred.len(), 5, "13, 14, 15, 16, 20");
    for id in &outcome.deferred {
        let c = match g.node(*id) {
            Some(Node::Concept(c)) => c,
            _ => panic!("held back, not collected"),
        };
        assert_eq!(
            c.gc_survived, 0,
            "a held-back candidate takes no survivor bump"
        );
        assert!(!outcome.survivors.contains(id));
    }
    for id in &outcome.survivors {
        let c = match g.node(*id) {
            Some(Node::Concept(c)) => c,
            _ => unreachable!(),
        };
        assert_eq!(c.gc_survived, 1, "a real survivor is bumped");
    }
}

/// Issue #29 item 4: disconnected components are structural garbage and go
/// before the score cut when the cap binds; an island concept that is
/// also under its bar is counted once, as disconnected.
#[test]
fn the_cap_takes_disconnected_components_before_score_cut_candidates() {
    let mut g = hub_session(&[(20, ConceptType::Resource)]);
    let a = insert_isolated(&mut g, concept(40, 1, "island a", ConceptType::Entity), 1);
    let b = insert_isolated(&mut g, concept(41, 1, "island b", ConceptType::Entity), 1);
    g.upsert_edge(edge(400, 40, 41, EdgeType::Dependency, 1.0, 0))
        .unwrap();
    let params = GcParams {
        min_concept_score: 5.0,
        max_collect_fraction: 0.0,
        min_collect_cap: 2,
        ..aged_params()
    };
    let outcome = run(&mut g, params);
    assert_eq!(outcome.concepts_collected, vec![a, b]);
    assert!(
        g.node(nid(20)).is_some(),
        "the score cut waits for the next sweep"
    );
    assert!(outcome.deferred.contains(&nid(20)));
    assert!(!outcome.deferred.contains(&a) && !outcome.deferred.contains(&b));
}

/// Ordering structural garbage first must not lose the cascade: a concept
/// reachable from the chain only through a score-cut concept is still
/// collected in the same sweep, after it (uncapped), and counted once.
#[test]
fn a_component_cut_off_by_the_score_cut_goes_in_the_same_sweep() {
    // An Entity: a Resource with a dependent (50) would be spared the cut.
    let mut g = hub_session(&[(20, ConceptType::Entity)]);
    // 50 (Logic, exempt from the score cut) hangs only off leaf 20.
    let tail = insert_isolated(&mut g, concept(50, 1, "rule", ConceptType::Logic), 1);
    g.upsert_edge(edge(500, 20, 50, EdgeType::Dependency, 1.0, 0))
        .unwrap();
    let params = GcParams {
        min_concept_score: 0.5,
        ..aged_params()
    };
    let outcome = run(&mut g, params);
    assert!(outcome.concepts_collected.contains(&nid(20)));
    assert!(
        outcome.concepts_collected.contains(&tail),
        "cascade, same sweep"
    );
    assert!(!outcome.cap_bound());
    let mut dedup = outcome.concepts_collected.clone();
    dedup.dedup();
    assert_eq!(dedup, outcome.concepts_collected, "each id collected once");
}

/// Issue #29 item 6: the drain order is a rotation of the id order (a
/// permutation: every survivor exactly once) whose start depends on the
/// sweep epoch, and over many sweeps every concept lands in a fixed-size
/// tail about equally often — a restart that loses the tail no longer
/// always costs the same high ids.
#[test]
fn survivor_drain_order_is_a_rotation_that_spreads_the_tail() {
    let ids: Vec<NodeId> = (0..10u64).map(nid).collect();
    let mut tail_hits = vec![0u32; ids.len()];
    let mut starts = HashSet::new();
    for epoch in 0..2_000u64 {
        let order = survivor_drain_order(&ids, epoch);
        let mut sorted = order.clone();
        sorted.sort_by_key(|id| id.0);
        assert_eq!(sorted, ids, "a permutation");
        let start = ids.iter().position(|id| *id == order[0]).unwrap();
        let rotated: Vec<NodeId> = ids[start..].iter().chain(&ids[..start]).copied().collect();
        assert_eq!(order, rotated, "a rotation of the id order");
        starts.insert(start);
        for id in &order[7..] {
            tail_hits[ids.iter().position(|x| x == id).unwrap()] += 1;
        }
    }
    assert_eq!(starts.len(), ids.len(), "every start position occurs");
    // 2,000 sweeps × 3 tail slots / 10 ids = 600 expected per id.
    for (i, hits) in tail_hits.iter().enumerate() {
        assert!(
            (450..=750).contains(hits),
            "id {i} in the tail {hits} times of ~600"
        );
    }
    assert_eq!(survivor_drain_order(&ids[..1], 7), ids[..1].to_vec());
    assert!(survivor_drain_order(&[], 7).is_empty());
    assert_eq!(
        survivor_drain_order(&ids, 42),
        survivor_drain_order(&ids, 42)
    );
}

/// Under the cap the score cut's candidates go furthest-under-the-bar
/// first.
#[test]
fn the_cap_takes_the_weakest_score_candidates_first() {
    // The Entity has the lower id, so a cap that took candidates in id
    // order would take it; the ratio order takes the Resource.
    let mut g = hub_session(&[(20, ConceptType::Entity), (21, ConceptType::Resource)]);
    let params = GcParams {
        min_concept_score: 5.0,
        max_collect_fraction: 0.0,
        min_collect_cap: 1,
        ..aged_params()
    };
    // Resource: lower score (no +0.05 Entity modifier) against a higher
    // bar (resistance 1.0 against 1.2) — the smaller score/bar ratio.
    let outcome = run(&mut g, params);
    assert_eq!(outcome.concepts_collected, vec![nid(21)]);
    assert!(outcome.cap_bound());
}

#[test]
fn collection_cap_is_a_fraction_with_a_floor() {
    let p = GcParams::default();
    assert_eq!(collection_cap(0, p), GC_MIN_COLLECT_CAP);
    assert_eq!(collection_cap(100, p), GC_MIN_COLLECT_CAP);
    assert_eq!(
        collection_cap(3370, p),
        169,
        "5% of the Metal rig, rounded up"
    );
    for bad in [f64::NAN, f64::INFINITY, -1.0] {
        let q = GcParams {
            max_collect_fraction: bad,
            ..p
        };
        assert_eq!(collection_cap(10_000, q), GC_MIN_COLLECT_CAP, "{bad}");
    }
    let all = GcParams {
        max_collect_fraction: 7.0,
        ..p
    };
    assert_eq!(collection_cap(1000, all), 1000, "a fraction is capped at 1");
}
