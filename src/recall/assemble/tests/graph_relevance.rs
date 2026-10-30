//! #175: the graph-relevance map must affect real assembly without changing provenance.
use super::*;
use crate::recall::experiment175::propagate;

fn run_graph(
    values: &HashMap<NodeId, f64>,
    warm: bool,
    missing_graph_score: bool,
) -> DetailedRecall {
    let g = graph_with(5);
    let expanded = ExpandedSet {
        required: vec![
            Scored::new(uid(1), 0.5),
            Scored::new(uid(2), 0.0),
            Scored::new(uid(3), 0.0),
        ],
        siblings: vec![Scored::new(uid(4), 0.0)],
    };
    let mut ranked = vec![Scored::new(uid(2), 0.8), Scored::new(uid(4), 0.1)];
    if warm {
        ranked.push(Scored::new(uid(1), 0.5));
    }
    if !missing_graph_score {
        ranked.push(Scored::new(uid(3), 0.2));
    }
    assemble_with_graph_relevance(
        &g,
        &expanded,
        &[Scored::new(uid(1), 0.5)],
        None,
        Some(values),
        &ScoreTable { epoch: 0, ranked },
        &HashMap::new(),
        &query(4, 10_000),
        RecallWeights::default(),
        crate::Config::default().canonization_edge_min_age,
        true,
        None,
        ts(60),
        default_token_count,
    )
}

#[test]
fn graph_relevance_changes_final_order_on_warm_and_cold_paths() {
    for warm in [false, true] {
        let zero = run_graph(&HashMap::new(), warm, false);
        let changed = run_graph(&HashMap::from([(uid(3), 0.9)]), warm, false);
        assert_ne!(ids_of(&zero), ids_of(&changed));
        assert_eq!(changed.hits[0].node_id, uid(3));
        assert_eq!(changed.cold_start, !warm);
        assert_eq!(changed.warnings, zero.warnings);
        assert_eq!(changed.detailed.len(), zero.detailed.len());
        let seed = changed.hits.iter().find(|h| h.node_id == uid(1)).unwrap();
        assert!(approx(seed.score, if warm { 0.5 } else { 0.25 }));
    }
}

#[test]
fn graph_only_unscored_member_cannot_trigger_cold_mode() {
    let changed = run_graph(&HashMap::from([(uid(3), 0.9)]), true, true);
    assert!(!changed.cold_start);
    assert!(approx(
        changed
            .hits
            .iter()
            .find(|h| h.node_id == uid(2))
            .unwrap()
            .score,
        0.4
    ));
}

#[test]
fn graph_map_cannot_override_direct_hits_siblings_or_add_members() {
    let base = run_graph(&HashMap::new(), true, false);
    let injected = run_graph(
        &HashMap::from([(uid(1), 100.0), (uid(4), 100.0), (uid(5), 100.0)]),
        true,
        false,
    );
    assert_eq!(base, injected);
}

#[test]
fn invalid_graph_values_do_not_poison_the_score_order() {
    let base = run_graph(&HashMap::new(), true, false);
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
        assert_eq!(
            base,
            run_graph(&HashMap::from([(uid(3), value)]), true, false)
        );
    }
}

#[test]
fn propagation_divides_by_sender_degree_and_deduplicates_typed_neighbors() {
    let mut g = graph_with(6);
    for (u, v) in [(1, 3), (1, 4), (2, 5)] {
        g.upsert_edge(dep_edge(u, v)).unwrap();
    }
    let mut parallel = dep_edge(1, 3);
    parallel.edge_type = crate::types::EdgeType::Semantic;
    parallel.id = uid(99);
    g.upsert_edge(parallel).unwrap();
    let seeds = vec![Scored::new(uid(1), 1.0), Scored::new(uid(2), 1.0)];
    let expanded = crate::recall::expand::expand(&g, seeds.clone(), 2);
    let damped = propagate(&g, &seeds, &expanded, 0.5, 1.0, false);
    let degree = propagate(&g, &seeds, &expanded, 0.5, 1.0, true);
    for n in [3, 4, 5] {
        assert!(approx(damped[&uid(n)], 1.0));
    }
    assert!(approx(degree[&uid(3)], 0.5));
    assert!(approx(degree[&uid(4)], 0.5));
    assert!(approx(degree[&uid(5)], 1.0));
    assert!(!degree.contains_key(&uid(1)) && !degree.contains_key(&uid(2)));
    let mut reversed = seeds.clone();
    reversed.reverse();
    assert_eq!(degree, propagate(&g, &reversed, &expanded, 0.5, 1.0, true));
}

#[test]
fn two_hop_propagation_is_directed_bounded_and_never_expands_siblings() {
    let mut g = graph_with(6);
    for (u, v) in [(1, 2), (2, 3), (3, 4), (5, 1)] {
        g.upsert_edge(dep_edge(u, v)).unwrap();
    }
    let seeds = vec![Scored::new(uid(1), 1.0)];
    let mut expanded = crate::recall::expand::expand(&g, seeds.clone(), 2);
    expanded.siblings.push(Scored::new(uid(6), 0.0));
    let relevance = propagate(&g, &seeds, &expanded, 0.5, 1.0, true);
    assert_eq!(relevance.len(), 2);
    assert!(approx(relevance[&uid(2)], 1.0));
    assert!(approx(relevance[&uid(3)], 0.5));
    for n in [1, 4, 5, 6] {
        assert!(!relevance.contains_key(&uid(n)));
    }
    assert!(propagate(&g, &[Scored::new(uid(6), 1.0)], &expanded, 0.5, 1.0, true).is_empty());
}
