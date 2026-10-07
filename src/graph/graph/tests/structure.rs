//! Nodes, edges and invariants: inserts, reinforcement, adjacency,
//! removal and cycle detection.

use super::*;

#[test]
fn empty_graph_is_consistent() {
    let g = Graph::new(sid());
    assert!(g.is_empty());
    assert_eq!(g.epoch(), 0);
    assert_eq!(g.log_len(), 0);
    g.assert_invariants().unwrap();
}

#[test]
fn insert_interaction_builds_chain_and_temporal_edge() {
    let mut g = Graph::new(sid());
    let i1 = interaction(1, None, 0);
    let i2 = interaction(2, Some(i1.id), 5);
    let i1_id = i1.id;
    let i2_id = i2.id;
    g.insert_interaction(i1).unwrap();
    assert_eq!(g.temporal_chain(), &[i1_id]);
    g.insert_interaction(i2).unwrap();
    assert_eq!(g.temporal_chain(), &[i1_id, i2_id]);
    // Structural Temporal edge exists: i2 -> i1.
    let e = g
        .edge_between(i2_id, i1_id, EdgeType::Temporal)
        .expect("temporal edge");
    assert_eq!(e.weight, 1.0);
    assert_eq!(g.edge_count(), 1);
    g.assert_invariants().unwrap();
}

#[test]
fn insert_interaction_rejects_bad_chain_positions() {
    let mut g = Graph::new(sid());
    // First interaction must have previous_id None.
    let i1 = interaction(1, Some(uid(99)), 0);
    assert!(g.insert_interaction(i1).is_err());
    g.assert_invariants().unwrap();

    let i1 = interaction(1, None, 0);
    let i1_id = i1.id;
    g.insert_interaction(i1).unwrap();

    // Non-first without previous_id.
    let bad = interaction(2, None, 5);
    assert!(g.insert_interaction(bad).is_err());
    // Non-first with a previous that is not the tail.
    let bad = interaction(2, Some(uid(999)), 5);
    assert!(g.insert_interaction(bad).is_err());
    // Chain unchanged after rejections.
    assert_eq!(g.temporal_chain(), &[i1_id]);
    assert_eq!(g.edge_count(), 0);
    g.assert_invariants().unwrap();
}

#[test]
fn reupsert_interaction_is_idempotent() {
    let mut g = Graph::new(sid());
    let i1 = interaction(1, None, 0);
    let i2 = interaction(2, Some(i1.id), 5);
    g.insert_interaction(i1.clone()).unwrap();
    g.insert_interaction(i2.clone()).unwrap();

    // Same position re-upsert: ok, single chain entry.
    g.insert_interaction(i2.clone()).unwrap();
    assert_eq!(g.temporal_chain(), &[i1.id, i2.id]);
    assert_eq!(g.node_count(), 2);
    g.assert_invariants().unwrap();

    // Changing position is rejected.
    let moved = interaction(2, None, 5);
    assert!(g.insert_interaction(moved).is_err());
}

#[test]
fn insert_concept_creates_derives_edge() {
    let (g, iid, cid) = small_graph();
    let d = g
        .edge_between(iid, cid, EdgeType::Derives)
        .expect("derives");
    assert_eq!(d.weight, 0.9);
    assert_eq!(g.edge_count(), 1);
    g.assert_invariants().unwrap();
}

#[test]
fn insert_concept_enforces_partial_canonical_key_uniqueness() {
    let mut g = Graph::new(sid());
    let i1 = interaction(1, None, 0);
    let iid = i1.id;
    g.insert_interaction(i1).unwrap();

    // Two non-Observation concepts with the same canonical key -> rejected
    // (schema §4 UNIQUE, spec errata 2026-08-11 / muse-spark M1).
    let mut c1 = concept(1, iid, "user schema");
    c1.canonical_key = "schema user".into();
    g.insert_concept(c1, iid).unwrap();
    let mut c2 = concept(2, iid, "schema user");
    c2.canonical_key = "schema user".into();
    let err = g.insert_concept(c2, iid).unwrap_err().to_string();
    assert!(err.contains("collides"), "{err}");
    // Rejection leaves the graph unchanged.
    assert_eq!(g.node_count(), 2);
    // Same-id re-upsert is idempotent and allowed (not a collision).
    let mut c1b = concept(1, iid, "user schema");
    c1b.canonical_key = "schema user".into();
    g.insert_concept(c1b, iid).unwrap();
    assert_eq!(g.node_count(), 2);

    // Observations are exempt (demote creates context-overflow duplicates
    // by design — muse-spark M2): two Observations sharing a key are fine.
    let mut o1 = concept(3, iid, "drift note");
    o1.concept_type = ConceptType::Observation;
    o1.canonical_key = "note".into();
    g.insert_concept(o1, iid).unwrap();
    let mut o2 = concept(4, iid, "drift note");
    o2.concept_type = ConceptType::Observation;
    o2.canonical_key = "note".into();
    g.insert_concept(o2, iid).unwrap();
    // Observation + Entity sharing a key: only one non-Observation row.
    let mut o3 = concept(5, iid, "schema user");
    o3.concept_type = ConceptType::Observation;
    o3.canonical_key = "schema user".into();
    g.insert_concept(o3, iid).unwrap();
    g.assert_invariants().unwrap();
}

#[cfg(feature = "fixtures")]
#[test]
fn assert_invariants_rejects_duplicate_canonical_keys_on_load() {
    // A loaded snapshot with two non-Observation concepts sharing a key
    // must fail assert_invariants (from_snapshot -> invariant check).
    let mut snap = crate::fixtures::load_snapshot("session-rest-api").unwrap();
    let mut clone = snap.concepts[0].clone();
    clone.id = NodeId::new();
    clone.content = "colliding clone".into();
    snap.concepts.push(clone);
    let err = Graph::from_snapshot(snap).unwrap_err().to_string();
    assert!(err.contains("canonical_key"), "{err}");
}

#[test]
fn insert_concept_rejects_missing_or_non_interaction_origin() {
    let mut g = Graph::new(sid());
    let i1 = interaction(1, None, 0);
    let iid = i1.id;
    g.insert_interaction(i1).unwrap();

    // Missing origin.
    let c = concept(1, uid(555), "orphan");
    assert!(g.insert_concept(c, uid(555)).is_err());
    // Origin that exists but is a concept, not an interaction.
    let c1 = concept(1, iid, "first");
    let c1_id = c1.id;
    g.insert_concept(c1, iid).unwrap();
    let c2 = concept(2, c1_id, "second");
    assert!(g.insert_concept(c2, c1_id).is_err());
    g.assert_invariants().unwrap();
}

#[test]
fn reupsert_concept_reinforces_derives() {
    let mut g = Graph::new(sid());
    let i1 = interaction(1, None, 0);
    let iid = i1.id;
    g.insert_interaction(i1).unwrap();
    let c = concept(1, iid, "user schema");
    let cid = c.id;
    g.insert_concept(c.clone(), iid).unwrap();
    g.insert_concept(c, iid).unwrap();

    assert_eq!(g.node_count(), 2);
    assert_eq!(g.edge_count(), 1);
    let d = g.edge_between(iid, cid, EdgeType::Derives).unwrap();
    assert_eq!(d.reinforcements, 2);
    assert_eq!(d.weight, (0.9 + REINFORCE_BUMP).min(MAX_EDGE_WEIGHT));
    g.assert_invariants().unwrap();
}

#[test]
fn duplicate_edge_reinforces_in_place() {
    let mut g = Graph::new(sid());
    let i1 = interaction(1, None, 0);
    let iid = i1.id;
    g.insert_interaction(i1).unwrap();
    let c1 = concept(1, iid, "user schema");
    let cid = c1.id;
    g.insert_concept(c1, iid).unwrap();
    let c2 = concept(2, iid, "auth middleware");
    let c2id = c2.id;
    g.insert_concept(c2, iid).unwrap();

    let e1 = edge(1, cid, c2id, EdgeType::CoOccurrence, 0.5);
    let e1_created = e1.created_at;
    g.upsert_edge(e1.clone()).unwrap();
    let mut e2 = edge(2, cid, c2id, EdgeType::CoOccurrence, 0.5);
    e2.last_reinforced = ts(99);
    g.upsert_edge(e2).unwrap();

    // Single edge, reinforced in place.
    assert_eq!(g.edge_count(), 3); // derives x2 + cooccurrence
    let e = g.edge_between(cid, c2id, EdgeType::CoOccurrence).unwrap();
    assert_eq!(e.reinforcements, 2);
    assert_eq!(e.weight, 0.5 + REINFORCE_BUMP);
    assert_eq!(e.created_at, e1_created);
    assert_eq!(e.last_reinforced, ts(99));
    // Original id wins.
    assert_eq!(e.id, e1.id);
    g.assert_invariants().unwrap();
}

/// D-R1-4: the reinforcement arm mutates only weight/reinforcements/
/// last_reinforced, so the ORIGINAL edge's event_time survives — but that
/// is three assignments away from copying incoming fields. Pin it: an
/// edge written under a historical (event-timed) interaction keeps its
/// stamp when reinforced from a differently-timed one; only the fields
/// reinforcement owns move.
#[test]
fn reinforcement_preserves_the_original_edge_event_time() {
    let mut g = Graph::new(sid());
    let i1 = interaction(1, None, 0);
    let iid = i1.id;
    g.insert_interaction(i1).unwrap();
    let c1 = concept(1, iid, "user schema");
    let cid = c1.id;
    g.insert_concept(c1, iid).unwrap();
    let c2 = concept(2, iid, "auth middleware");
    let c2id = c2.id;
    g.insert_concept(c2, iid).unwrap();

    // Written under a historical turn: about-time long before flush time.
    let original_about = ts(-100_000);
    let mut e1 = edge(1, cid, c2id, EdgeType::CoOccurrence, 0.5);
    e1.event_time = Some(original_about);
    g.upsert_edge(e1.clone()).unwrap();

    // Reinforced from a DIFFERENTLY-timed turn: incoming edge carries the
    // new interaction's event_time, which must be ignored.
    let reinforcer_about = ts(500);
    assert_ne!(reinforcer_about, original_about);
    let mut e2 = edge(2, cid, c2id, EdgeType::CoOccurrence, 0.9);
    e2.event_time = Some(reinforcer_about);
    e2.last_reinforced = ts(99);
    g.upsert_edge(e2).unwrap();

    let e = g.edge_between(cid, c2id, EdgeType::CoOccurrence).unwrap();
    assert_eq!(e.reinforcements, 2, "the duplicate reinforced in place");
    assert_eq!(
        e.event_time,
        Some(original_about),
        "reinforcement must not re-age the edge onto the incoming turn's clock"
    );
    assert_eq!(e.created_at, e1.created_at, "original created_at preserved");
    assert_eq!(e.last_reinforced, ts(99));
    assert_eq!(e.weight, 0.5 + REINFORCE_BUMP, "incoming weight ignored");
    g.assert_invariants().unwrap();
}

#[test]
fn edge_weight_normalization() {
    let (mut g, iid, cid) = small_graph();
    let c2 = concept(2, iid, "auth middleware");
    let c2id = c2.id;
    g.insert_concept(c2, iid).unwrap();

    // NaN and +Inf clamp to 0.0.
    let nan = edge(1, cid, c2id, EdgeType::Semantic, f64::NAN);
    g.upsert_edge(nan).unwrap();
    let e = g.edge_between(cid, c2id, EdgeType::Semantic).unwrap();
    assert_eq!(e.weight, 0.0);

    let inf = edge(2, cid, c2id, EdgeType::Hierarchical, f64::INFINITY);
    g.upsert_edge(inf).unwrap();
    let e = g.edge_between(cid, c2id, EdgeType::Hierarchical).unwrap();
    assert_eq!(e.weight, 0.0);

    // Negative (and -Inf) rejected.
    let neg = edge(3, cid, c2id, EdgeType::Causal, -1.0);
    assert!(g.upsert_edge(neg).is_err());
    let neg_inf = edge(4, cid, c2id, EdgeType::Dependency, f64::NEG_INFINITY);
    assert!(g.upsert_edge(neg_inf).is_err());
    g.assert_invariants().unwrap();
}

#[test]
fn upsert_edge_validates_endpoints_and_session() {
    let (mut g, iid, cid) = small_graph();
    let c2 = concept(2, iid, "auth middleware");
    let c2id = c2.id;
    g.insert_concept(c2, iid).unwrap();

    // Missing source / target.
    assert!(g
        .upsert_edge(edge(1, uid(900), c2id, EdgeType::Causal, 0.5))
        .is_err());
    assert!(g
        .upsert_edge(edge(2, cid, uid(900), EdgeType::Causal, 0.5))
        .is_err());

    // Session mismatch.
    let mut bad = edge(3, cid, c2id, EdgeType::Causal, 0.5);
    bad.session_id = SessionId::from("other");
    assert!(g.upsert_edge(bad).is_err());
    g.assert_invariants().unwrap();
}

#[test]
fn record_edge_rejects_type_invalid_endpoints() {
    // GRAPH-2: spec §5 pins what each edge type connects; the write gate
    // must reject type-invalid endpoint pairs instead of storing them (they
    // would pollute recall BFS permanently).
    let (mut g, iid, cid) = small_graph();
    let c2 = concept(2, iid, "auth middleware");
    let c2id = c2.id;
    g.insert_concept(c2, iid).unwrap();

    // Concept-only edge types from/to an interaction.
    let err = g
        .upsert_edge(edge(1, iid, c2id, EdgeType::Semantic, 0.5))
        .unwrap_err()
        .to_string();
    assert!(err.contains("Concept -> Concept"), "{err}");
    let err = g
        .upsert_edge(edge(2, c2id, iid, EdgeType::Causal, 0.5))
        .unwrap_err()
        .to_string();
    assert!(err.contains("Concept -> Concept"), "{err}");
    // Temporal must connect interactions.
    let err = g
        .upsert_edge(edge(3, iid, cid, EdgeType::Temporal, 0.5))
        .unwrap_err()
        .to_string();
    assert!(err.contains("Interaction -> Interaction"), "{err}");
    // Derives must connect interaction -> concept.
    let err = g
        .upsert_edge(edge(4, cid, c2id, EdgeType::Derives, 0.5))
        .unwrap_err()
        .to_string();
    assert!(err.contains("Interaction -> Concept"), "{err}");
    // A legal Concept -> Concept edge still writes.
    g.upsert_edge(edge(5, cid, c2id, EdgeType::Semantic, 0.5))
        .unwrap();
    // Nothing was written by the rejected attempts (derives x2 + semantic).
    assert_eq!(g.edge_count(), 3);
    g.assert_invariants().unwrap();
}

#[test]
fn assert_invariants_flags_type_invalid_edges() {
    // GRAPH-2: assert_invariants must catch the class even if an edge got
    // into the graph another way (record_edge rejects it, so inject into the
    // private indexes directly — defense for future load-path bugs).
    let (mut g, iid, _) = small_graph();
    let c2 = concept(2, iid, "auth middleware");
    let c2id = c2.id;
    g.insert_concept(c2, iid).unwrap();
    let bad = edge(9, iid, c2id, EdgeType::Semantic, 0.5);
    g.edges.insert(bad.id, bad.clone());
    g.edge_keys
        .insert((bad.source, bad.target, bad.edge_type), bad.id);
    g.add_adjacency(bad.source, bad.target, bad.edge_type);
    let err = g.assert_invariants().unwrap_err().to_string();
    assert!(err.contains("Semantic edge must connect"), "{err}");
}

/// CONC-1: `incident_edges` must read the **adjacency index**, not scan the
/// edge set. Structural, not timing-based: an edge present in `edges` +
/// `edge_keys` but *absent* from the adjacency maps is invisible to an
/// index-backed lookup and visible to a `edges.values()` filter — so this
/// fails on the pre-fix implementation and passes on the index-backed one.
///
/// The daemon calls this per concept in every detector and in `rescore`, so
/// the scan made each pass `O(nodes × edges)` and held the graph lock for
/// hundreds of milliseconds per cycle at 4k concepts (§6.4's second clause).
#[test]
fn incident_edges_reads_the_adjacency_index_not_the_edge_map() {
    let (mut g, iid, cid) = small_graph();
    let c2 = concept(2, iid, "api layer");
    let c2id = c2.id;
    g.insert_concept(c2, iid).unwrap();
    g.upsert_edge(edge(1, cid, c2id, EdgeType::Dependency, 0.7))
        .unwrap();

    let via_index: Vec<NodeId> = g.incident_edges(cid).iter().map(|e| e.id).collect();
    // Ground truth for this graph: the Derives provenance edge + the
    // Dependency edge, id-ascending.
    let mut expected: Vec<NodeId> = g
        .edges
        .values()
        .filter(|e| e.source == cid || e.target == cid)
        .map(|e| e.id)
        .collect();
    expected.sort_by_key(|id| id.0);
    assert_eq!(via_index, expected, "index-backed lookup must be complete");

    // Now desynchronize: an edge in `edges`/`edge_keys` but not in the
    // adjacency maps. An index-backed reader cannot see it; a full scan can.
    let ghost = edge(99, c2id, cid, EdgeType::Causal, 0.6);
    let ghost_id = ghost.id;
    g.edges.insert(ghost_id, ghost.clone());
    g.edge_keys
        .insert((ghost.source, ghost.target, ghost.edge_type), ghost_id);

    assert!(
        !g.incident_edges(cid).iter().any(|e| e.id == ghost_id),
        "incident_edges must be sourced from the adjacency index — a scan of \
             the edge map would surface the un-indexed edge"
    );
    assert_eq!(
        g.incident_edges(cid).len(),
        expected.len(),
        "and the indexed set is unchanged"
    );
}

/// CONC-1: routing through the index must not change the id-ascending
/// contract existing callers rely on, including with a self-loop (which
/// appears in both the out and in maps and must appear once).
#[test]
fn incident_edges_stay_id_ascending_and_deduplicated() {
    let mut g = Graph::new(sid());
    let i1 = interaction(1, None, 0);
    let iid = i1.id;
    g.insert_interaction(i1).unwrap();
    let hub = concept(1, iid, "hub");
    let hub_id = hub.id;
    g.insert_concept(hub, iid).unwrap();
    // Spokes attached in descending id order so insertion order and id
    // order disagree.
    for n in (2..=6u64).rev() {
        let c = concept(n, iid, &format!("spoke {n}"));
        let cid = c.id;
        g.insert_concept(c, iid).unwrap();
        g.upsert_edge(edge(100 + n, hub_id, cid, EdgeType::Dependency, 0.7))
            .unwrap();
    }
    // A self-loop: present in both adjacency directions.
    g.edges.insert(
        uid(777),
        Edge {
            id: uid(777),
            ..edge(777, hub_id, hub_id, EdgeType::CoOccurrence, 0.6)
        },
    );
    g.edge_keys
        .insert((hub_id, hub_id, EdgeType::CoOccurrence), uid(777));
    g.add_adjacency(hub_id, hub_id, EdgeType::CoOccurrence);

    let ids: Vec<NodeId> = g.incident_edges(hub_id).iter().map(|e| e.id).collect();
    let mut sorted = ids.clone();
    sorted.sort_by_key(|id| id.0);
    assert_eq!(ids, sorted, "id-ascending order is part of the contract");
    assert_eq!(
        ids.iter().filter(|id| **id == uid(777)).count(),
        1,
        "a self-loop must appear exactly once"
    );
}

#[test]
fn remove_node_cleans_incident_edges() {
    let mut g = Graph::new(sid());
    let i1 = interaction(1, None, 0);
    let i2 = interaction(2, Some(i1.id), 5);
    let i2id = i2.id;
    g.insert_interaction(i1).unwrap();
    g.insert_interaction(i2).unwrap();
    let c = concept(1, i2id, "user schema");
    let cid = c.id;
    g.insert_concept(c, i2id).unwrap();

    // Extra concept-to-concept edge.
    let c2 = concept(2, i2id, "api layer");
    let c2id = c2.id;
    g.insert_concept(c2, i2id).unwrap();
    g.upsert_edge(edge(1, cid, c2id, EdgeType::Dependency, 0.7))
        .unwrap();
    g.upsert_edge(edge(2, c2id, cid, EdgeType::Dependency, 0.7))
        .unwrap();

    // Remove c2: both dependency edges must go; Derives (i2 -> c2) must go too.
    let edges_before = g.edge_count();
    g.remove_node(c2id).unwrap();
    assert_eq!(g.node_count(), 3); // i1, i2, c
    assert!(g.node(c2id).is_none());
    assert_eq!(g.edge_count(), edges_before - 3);
    assert!(g.edge_between(i2id, c2id, EdgeType::Derives).is_none());
    assert!(g.edge_between(cid, c2id, EdgeType::Dependency).is_none());
    assert!(g.out_neighbors(cid).is_empty());
    assert!(g.in_neighbors(c2id).is_empty());
    g.assert_invariants().unwrap();
}

#[test]
fn remove_missing_node_or_edge_errors() {
    let (mut g, _, _) = small_graph();
    assert!(g.remove_node(uid(999)).is_err());
    assert!(g.remove_edge(uid(999)).is_err());
    g.assert_invariants().unwrap();
}

#[test]
fn remove_node_rejects_interactions() {
    let (mut g, iid, _) = small_graph();
    // Interactions are append-only in v0.1 (spec §9: compaction is cut).
    // Removing one would leave a dangling previous_id in the chain — rejected
    // at write time, not detected lazily (adve-review T2.1 S2).
    let err = g.remove_node(iid).unwrap_err().to_string();
    assert!(err.contains("append-only"), "{err}");
    // Nothing changed.
    assert_eq!(g.node_count(), 2);
    assert_eq!(g.edge_count(), 1);
    g.assert_invariants().unwrap();
}

#[test]
fn cycle_is_detected_by_assert_invariants() {
    let (mut g, iid, cid) = small_graph();
    let c2 = concept(2, iid, "auth middleware");
    let c2id = c2.id;
    g.insert_concept(c2, iid).unwrap();

    // A -> B -> A dependency cycle. upsert_edge stores it; write-time rejection
    // is record_action's (T2.4); assert_invariants must flag it.
    g.upsert_edge(edge(1, cid, c2id, EdgeType::Dependency, 0.7))
        .unwrap();
    g.upsert_edge(edge(2, c2id, cid, EdgeType::Dependency, 0.7))
        .unwrap();
    let err = g.assert_invariants().unwrap_err().to_string();
    assert!(err.contains("cycle"), "{err}");
}

#[test]
fn hierarchical_cycle_is_detected_by_assert_invariants() {
    let (mut g, iid, cid) = small_graph();
    let c2 = concept(2, iid, "auth middleware");
    let c2id = c2.id;
    g.insert_concept(c2, iid).unwrap();

    // A parent-of B parent-of A is semantically nonsensical. upsert_edge stores
    // it; assert_invariants must flag it (adve-review T2.1 M1).
    g.upsert_edge(edge(1, cid, c2id, EdgeType::Hierarchical, 0.7))
        .unwrap();
    g.upsert_edge(edge(2, c2id, cid, EdgeType::Hierarchical, 0.7))
        .unwrap();
    let err = g.assert_invariants().unwrap_err().to_string();
    assert!(err.contains("cycle"), "{err}");
}

#[test]
fn self_loop_structural_edge_is_a_cycle() {
    let (mut g, _, cid) = small_graph();
    // Non-structural self-loop is legal.
    g.upsert_edge(edge(1, cid, cid, EdgeType::CoOccurrence, 0.3))
        .unwrap();
    g.assert_invariants().unwrap();
    // Structural self-loop (A -> A) is a cycle by definition.
    g.upsert_edge(edge(2, cid, cid, EdgeType::Dependency, 0.3))
        .unwrap();
    let err = g.assert_invariants().unwrap_err().to_string();
    assert!(err.contains("cycle"), "{err}");
}

#[test]
fn out_neighbors_dedup_across_edge_types() {
    let (mut g, iid, cid) = small_graph();
    let c2 = concept(2, iid, "auth middleware");
    let c2id = c2.id;
    g.insert_concept(c2, iid).unwrap();
    g.upsert_edge(edge(1, cid, c2id, EdgeType::CoOccurrence, 0.5))
        .unwrap();
    g.upsert_edge(edge(2, cid, c2id, EdgeType::Semantic, 0.5))
        .unwrap();
    // Two edge types to the same target -> one neighbor.
    assert_eq!(g.out_neighbors(cid), vec![c2id]);
}

#[test]
fn deep_chain_cycle_check_does_not_overflow_stack() {
    // GRAPH-3 regression: dfs_cycle was recursive — a ~10k-deep
    // Causal/Dependency chain overflowed the ~2 MiB worker-thread stack that
    // load_session materializes on (SIGABRT -> session permanently
    // unloadable). The check must be iterative. Exercise the full load path
    // (from_snapshot -> assert_invariants) on a small-stack thread, exactly
    // like load.rs does.
    const N: usize = 20_000;
    let i = interaction(1, None, 0);
    let iid = i.id;
    let mut snap = GraphSnapshot {
        session_id: sid(),
        root_goal: None,
        created_at: None,
        closed_at: None,
        write_intents: Vec::new(),
        interactions: vec![i],
        concepts: (0..N)
            .map(|k| {
                let mut c = concept(k as u64 + 1, iid, "chain");
                c.content = format!("chain {k}");
                c.canonical_key = format!("chain{k}");
                c
            })
            .collect(),
        edges: Vec::with_capacity(2 * N),
        synonyms: vec![],
        reservations: vec![],
        canonization_events: vec![],
        embedding: None,
        mutation_epoch: 0,
        gc_mark: Default::default(),
    };
    // Every concept derives from the interaction (assert_invariants
    // requires it) plus a single Causal chain c0 -> c1 -> ... -> c(N-1).
    for k in 0..N {
        snap.edges.push(edge(
            k as u64 + 1,
            iid,
            NodeId(Uuid::from_u64_pair(2, k as u64 + 1)),
            EdgeType::Derives,
            0.9,
        ));
    }
    for k in 0..N - 1 {
        snap.edges.push(edge(
            N as u64 + k as u64 + 1,
            NodeId(Uuid::from_u64_pair(2, k as u64 + 1)),
            NodeId(Uuid::from_u64_pair(2, k as u64 + 2)),
            EdgeType::Causal,
            0.5,
        ));
    }
    let handle = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(move || Graph::from_snapshot(snap))
        .expect("spawn small-stack thread");
    let g = handle.join().expect("no panic on the loader thread");
    g.expect("load + invariants pass")
        .assert_invariants()
        .unwrap();
}

#[test]
fn invariant_report_lists_every_violation() {
    let mut g = Graph::new(sid());
    // No interactions yet; add one concept whose origin does not exist.
    let c = concept(1, uid(999), "orphan");
    let err = g.insert_concept(c, uid(999)).unwrap_err().to_string();
    assert!(err.contains("not an interaction"), "{err}");
}
