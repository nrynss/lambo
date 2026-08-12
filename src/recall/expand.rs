//! Phase-2 BFS expansion (T5.2, spec §8): the neighborhood of phase 1.
//!
//! [`expand`] runs a BFS from the phase-1 candidate set out to
//! [`DEFAULT_TRAVERSAL_DEPTH`] levels, following only the five recall
//! traversable edge types in the spec §8 priority order:
//!
//! `Dependency`/`Causal` → `Hierarchical` → `CoOccurrence` → `Semantic`
//!
//! [`EdgeType::Derives`] and [`EdgeType::Temporal`] are structural edges of
//! the bipartite graph (Interaction → Concept, Interaction → Interaction) and
//! are NOT part of phase 2: the spec's priority list is exactly the five
//! Concept↔Concept types, and the golden `phase2_expanded` lists in
//! `fixtures/recall-goldens.json` pin that reading (the direct neighbors of
//! the golden candidates are exactly their `Dependency` targets).
//!
//! Semantics:
//! * **Depth** counts BFS levels from the candidate set: candidates are level
//!   0, their neighbors level 1, and so on, so `traversal_depth=1` returns
//!   candidates plus direct neighbors and `traversal_depth=2` adds
//!   neighbors-of-neighbors (the default, matching the recall cache key).
//! * **Edge priority** is per frontier node: at each node the five edge types
//!   are traversed in the documented order, and within one type the
//!   [`Graph::out_neighbors_typed`] id-ascending order is kept. The result is
//!   therefore deterministic given the same graph and candidate list.
//! * **Visited-set cycle guard**: a node is recorded and enqueued at most
//!   once; the first (highest-priority, shortest-level) discovery wins, and a
//!   node reached again through a different path is never re-expanded.
//!   Cycles terminate because every neighbor of a visited node is skipped.
//! * **`chunk_group_id` siblings** (T2.5's field): every concept sharing a
//!   chunk group with an expanded member is force-included even when the BFS
//!   never reached it (spec §8 "force-included, scored independently"). The
//!   group key is guaranteed non-empty by `demote` (grok G3), so the grouping
//!   boundary is meaningful. Sibling inclusion is the transitive closure over
//!   the group key: all concepts carrying any group id present among the
//!   required members join [`ExpandedSet::siblings`]. Siblings are NOT
//!   re-expanded: they were not reached by the BFS, and a `chunk_group_id`
//!   tag is not a graph path, so expanding from them would conflate the two
//!   membership sources (documented deviation from a naive closure-BFS).
//!
//! Scores: T5.3 assigns real scores. [`expand`] carries phase-1 candidate
//! scores through unchanged (they are inputs, not re-scored here) and stamps
//! every other member, BFS-reached or sibling, with the [`UNSCORED`]
//! placeholder. [`ExpandedSet::required`] is ordered structurally (candidate
//! order at level 0, then BFS discovery order), NOT by score. Ordering for
//! scoring is T5.3's job.
//!
//! Pure and lock-safe like [`crate::recall::candidates::candidates`]: reads
//! only `graph` under the caller's lock, performs no I/O.

use std::collections::{HashMap, HashSet};

use crate::graph::Graph;
use crate::types::{EdgeType, NodeId, Scored};

/// The five recall-traversable edge types in spec §8 priority order.
/// `Dependency` and `Causal` share the top tier ("Dependency/Causal first");
/// the array order resolves the intra-tier tie for determinism.
pub const TRAVERSAL_ORDER: [EdgeType; 5] = [
    EdgeType::Dependency,
    EdgeType::Causal,
    EdgeType::Hierarchical,
    EdgeType::CoOccurrence,
    EdgeType::Semantic,
];

/// Default BFS depth (spec §8): two levels beyond the candidate set.
pub const DEFAULT_TRAVERSAL_DEPTH: usize = 2;

/// Placeholder score for members that were never a phase-1 candidate.
///
/// T5.3 assigns the real `final_score`; until then these entries are carried
/// so their identity is visible to assembly without fabricating a score.
pub const UNSCORED: f64 = 0.0;

/// The phase-2 expansion: every concept the recall pipeline should score.
///
/// Distinguished by provenance so T5.3 can weight the two sources differently
/// and never double-count a node: a concept appears in at most one field.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExpandedSet {
    /// BFS members: the phase-1 candidates (level 0, input order) followed by
    /// every concept the traversal reached, in deterministic discovery order
    /// (level by level, edge-priority order within a level). Candidates keep
    /// their phase-1 score; every other member carries [`UNSCORED`].
    pub required: Vec<Scored<NodeId>>,
    /// Force-included `chunk_group_id` siblings: concepts sharing a chunk
    /// group with a required member but not reached by the BFS. Each keeps
    /// its own identity and an [`UNSCORED`] placeholder (T5.3 assigns);
    /// ordered id-ascending for determinism.
    pub siblings: Vec<Scored<NodeId>>,
}

/// Expand `candidates` to `depth` BFS levels, force-including
/// `chunk_group_id` siblings (see module docs for the full semantics).
///
/// `depth` counts BFS levels from the candidate set: 0 returns the candidates
/// themselves (siblings still force-included), 1 adds direct neighbors, and so
/// on. An empty candidate list yields an empty [`ExpandedSet`]. Duplicate
/// candidate ids are collapsed, keeping the first occurrence and its score.
pub fn expand(graph: &Graph, candidates: Vec<Scored<NodeId>>, depth: usize) -> ExpandedSet {
    if candidates.is_empty() {
        return ExpandedSet::default();
    }

    // Level 0: candidates in input order (phase 1 hands them score-desc, id-asc).
    let mut required: Vec<Scored<NodeId>> = Vec::new();
    let mut visited: HashSet<NodeId> = HashSet::with_capacity(candidates.len());
    let mut frontier: Vec<NodeId> = Vec::new();
    for s in candidates {
        if visited.insert(s.item) {
            frontier.push(s.item);
            required.push(s);
        }
    }

    // Levels 1..=depth. Per frontier node the edge types are traversed in
    // TRAVERSAL_ORDER; per type the neighbors come id-ascending from the graph,
    // so the recorded order is deterministic. First discovery wins: a node
    // reachable via both a high- and low-priority edge is recorded during the
    // priority pass, and one reached twice is never enqueued again.
    for _ in 0..depth {
        let mut next: Vec<NodeId> = Vec::new();
        for &src in &frontier {
            for ty in TRAVERSAL_ORDER {
                for tgt in graph.out_neighbors_typed(src, ty) {
                    if visited.insert(tgt) {
                        next.push(tgt);
                        required.push(Scored::new(tgt, UNSCORED));
                    }
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }

    let mut out = ExpandedSet {
        required,
        siblings: Vec::new(),
    };
    out.siblings = force_included_siblings(graph, &out.required);
    out
}

/// Concepts sharing a `chunk_group_id` with a required member, minus the
/// members themselves: the force-included sibling closure (spec §8).
fn force_included_siblings(graph: &Graph, required: &[Scored<NodeId>]) -> Vec<Scored<NodeId>> {
    let member: HashSet<NodeId> = required.iter().map(|s| s.item).collect();
    // group id -> member concepts, and which groups the required set touches.
    let mut by_group: HashMap<&str, Vec<NodeId>> = HashMap::new();
    let mut touched: HashSet<&str> = HashSet::new();
    for c in graph.concepts() {
        if let Some(g) = c.chunk_group_id.as_deref() {
            by_group.entry(g).or_default().push(c.id);
            if member.contains(&c.id) {
                touched.insert(g);
            }
        }
    }
    let mut ids: Vec<NodeId> = Vec::new();
    for g in touched {
        if let Some(group) = by_group.get(g) {
            for &id in group {
                if !member.contains(&id) {
                    ids.push(id);
                }
            }
        }
    }
    ids.sort_unstable_by_key(|id| id.0);
    ids.into_iter()
        .map(|id| Scored::new(id, UNSCORED))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, TimeZone, Utc};
    use uuid::Uuid;

    use crate::types::{AgentId, Concept, ConceptType, Edge, Interaction, SessionId};

    fn ts(minutes: i64) -> DateTime<Utc> {
        let base = Utc.timestamp_opt(1_752_000_000, 0).unwrap();
        base + chrono::Duration::minutes(minutes)
    }

    fn sid() -> SessionId {
        SessionId::from("test-session")
    }

    fn interaction(id: u64, prev: Option<NodeId>, at_min: i64) -> Interaction {
        Interaction {
            id: NodeId(Uuid::from_u64_pair(1, id)),
            session_id: sid(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: Some(format!("prompt {id}")),
            previous_id: prev,
            created_at: ts(at_min),
        }
    }

    fn concept(id: u64, origin: NodeId, content: &str, group: Option<&str>) -> Concept {
        Concept {
            id: NodeId(Uuid::from_u64_pair(2, id)),
            session_id: sid(),
            content: content.into(),
            canonical_key: content.into(),
            concept_type: ConceptType::Entity,
            origin_interaction: origin,
            origin_agent: AgentId::from("agent-a"),
            created_at: ts(0),
            access_count: 0,
            last_accessed: None,
            gc_survived: 0,
            canonization_status: crate::types::CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: None,
            embedding: None,
            chunk_group_id: group.map(str::to_owned),
        }
    }

    fn edge(id: u64, src: NodeId, tgt: NodeId, ty: EdgeType) -> Edge {
        Edge {
            id: NodeId(Uuid::from_u64_pair(3, id)),
            session_id: sid(),
            source: src,
            target: tgt,
            edge_type: ty,
            weight: 1.0,
            reinforcements: 1,
            created_at: ts(0),
            last_reinforced: ts(0),
        }
    }

    fn nid(u: u64) -> NodeId {
        NodeId(Uuid::from_u64_pair(2, u))
    }

    fn ids(ss: Vec<Scored<NodeId>>) -> Vec<NodeId> {
        ss.into_iter().map(|s| s.item).collect()
    }

    /// Graph with one interaction and one concept per `(id, group)` pair.
    fn planted(concepts: &[(u64, Option<&str>)]) -> (Graph, NodeId) {
        let mut g = Graph::new(sid());
        let i = interaction(1, None, 0);
        let iid = i.id;
        g.insert_interaction(i).unwrap();
        for (id, group) in concepts {
            g.insert_concept(concept(*id, iid, &format!("concept {id}"), *group), iid)
                .unwrap();
        }
        (g, iid)
    }

    // -----------------------------------------------------------------------
    // Depth semantics
    // -----------------------------------------------------------------------

    #[test]
    fn depth_counts_levels_from_candidate_set() {
        let (mut g, _iid) = planted(&[(1, None), (2, None), (3, None)]);
        let a = nid(1);
        let b = nid(2);
        let c = nid(3);
        g.upsert_edge(edge(1, a, b, EdgeType::Dependency)).unwrap();
        g.upsert_edge(edge(2, b, c, EdgeType::Dependency)).unwrap();

        // depth 0: candidates only.
        let out = expand(&g, vec![Scored::new(a, 0.8)], 0);
        assert_eq!(ids(out.required), vec![a]);

        // depth 1: direct neighbors, no level-2 nodes.
        let out = expand(&g, vec![Scored::new(a, 0.8)], 1);
        let got = ids(out.required);
        assert_eq!(got, vec![a, b]);
        assert!(!got.contains(&c), "depth=1 must not include level-2 node");

        // depth 2: neighbors-of-neighbors included.
        let out = expand(&g, vec![Scored::new(a, 0.8)], 2);
        assert_eq!(ids(out.required), vec![a, b, c]);
    }

    #[test]
    fn empty_candidates_and_isolated_candidate() {
        let (g, _iid) = planted(&[(1, None)]);
        assert_eq!(expand(&g, Vec::new(), 2), ExpandedSet::default());

        let out = expand(&g, vec![Scored::new(nid(1), 0.7)], 2);
        assert_eq!(ids(out.required), vec![nid(1)]);
        assert!(out.siblings.is_empty());
    }

    // -----------------------------------------------------------------------
    // Edge priority
    // -----------------------------------------------------------------------

    /// A target reachable via Dependency and via Semantic: the Dependency path
    /// is taken. The planted ids are chosen so that id-ascending order ALONE
    /// would put the Semantic-only neighbor first. Only the priority pass
    /// records Dependency first, and the shared target is never duplicated.
    #[test]
    fn edge_priority_dependency_beats_semantic() {
        let (mut g, _iid) = planted(&[(1, None), (2, None), (3, None), (4, None)]);
        let a = nid(1); // candidate
        let t = nid(2); // Semantic neighbor of a, ALSO Dependency-reachable via b
        let b = nid(3); // Dependency-only neighbor of a
        let c = nid(4); // out of b, proves b expanded before the Semantic pass
        g.upsert_edge(edge(1, a, b, EdgeType::Dependency)).unwrap();
        g.upsert_edge(edge(2, a, t, EdgeType::Semantic)).unwrap();
        g.upsert_edge(edge(3, b, t, EdgeType::Dependency)).unwrap();
        g.upsert_edge(edge(4, b, c, EdgeType::Dependency)).unwrap();

        let out = expand(&g, vec![Scored::new(a, 0.8)], 2);
        assert_eq!(
            ids(out.required),
            vec![a, b, t, c],
            "Dependency-pass neighbor b must be recorded before Semantic-only t \
             (id-asc alone would yield t first)"
        );
    }

    /// All five tiers traverse in the documented priority order at a single
    /// frontier node, regardless of node ids.
    #[test]
    fn edge_priority_orders_all_tiers() {
        let (mut g, _iid) = planted(&[
            (1, None),
            (2, None),
            (3, None),
            (4, None),
            (5, None),
            (6, None),
        ]);
        let a = nid(1);
        // Ids deliberately invert the expected priority order: the Semantic
        // neighbor has the smallest id, the Dependency neighbor the largest.
        g.upsert_edge(edge(1, a, nid(2), EdgeType::Semantic))
            .unwrap();
        g.upsert_edge(edge(2, a, nid(3), EdgeType::CoOccurrence))
            .unwrap();
        g.upsert_edge(edge(3, a, nid(4), EdgeType::Hierarchical))
            .unwrap();
        g.upsert_edge(edge(4, a, nid(5), EdgeType::Causal)).unwrap();
        g.upsert_edge(edge(5, a, nid(6), EdgeType::Dependency))
            .unwrap();

        let out = expand(&g, vec![Scored::new(a, 0.8)], 1);
        assert_eq!(
            ids(out.required),
            vec![a, nid(6), nid(5), nid(4), nid(3), nid(2)],
            "Dependency/Causal -> Hierarchical -> CoOccurrence -> Semantic"
        );
    }

    // -----------------------------------------------------------------------
    // Visited-set guard
    // -----------------------------------------------------------------------

    /// Diamond + cycle: A→B→A cycles back, and D is reachable twice (via B and
    /// via C). The traversal terminates and every node is recorded once.
    #[test]
    fn cycle_and_diamond_terminate_visiting_each_node_once() {
        let (mut g, _iid) = planted(&[(1, None), (2, None), (3, None), (4, None)]);
        let a = nid(1);
        let b = nid(2);
        let c = nid(3);
        let d = nid(4);
        g.upsert_edge(edge(1, a, b, EdgeType::Dependency)).unwrap();
        g.upsert_edge(edge(2, b, a, EdgeType::Dependency)).unwrap(); // cycle
        g.upsert_edge(edge(3, a, c, EdgeType::Dependency)).unwrap();
        g.upsert_edge(edge(4, b, d, EdgeType::Dependency)).unwrap();
        g.upsert_edge(edge(5, c, d, EdgeType::Dependency)).unwrap(); // diamond into d

        let out = expand(&g, vec![Scored::new(a, 0.8)], 4);
        let got = ids(out.required);
        assert_eq!(got, vec![a, b, c, d]);
        let mut once: HashSet<NodeId> = got.iter().copied().collect();
        assert_eq!(
            once.len(),
            got.len(),
            "no node may be recorded twice: {got:?}"
        );
        once.extend([a, b, c, d]);
        assert_eq!(once.len(), 4, "all four nodes visited exactly once");
    }

    /// A node reached through a lower-priority edge after the priority pass
    /// already recorded it is not re-expanded (its level-2 neighbors would
    /// otherwise surface a duplicate path).
    #[test]
    fn visited_node_is_not_re_expanded() {
        let (mut g, _iid) = planted(&[(1, None), (2, None), (3, None), (4, None)]);
        let a = nid(1);
        let b = nid(2);
        let t = nid(3);
        let e = nid(4);
        // t reachable from a via Semantic (level 1) and from b via Dependency
        // (level 2); e hangs off t so a re-expansion of t would surface it.
        g.upsert_edge(edge(1, a, b, EdgeType::Dependency)).unwrap();
        g.upsert_edge(edge(2, a, t, EdgeType::Semantic)).unwrap();
        g.upsert_edge(edge(3, b, t, EdgeType::Dependency)).unwrap();
        g.upsert_edge(edge(4, t, e, EdgeType::Dependency)).unwrap();

        let out = expand(&g, vec![Scored::new(a, 0.8)], 3);
        assert_eq!(ids(out.required), vec![a, b, t, e]);
    }

    // -----------------------------------------------------------------------
    // chunk_group_id sibling force-inclusion
    // -----------------------------------------------------------------------

    /// Two concepts share a chunk group; only one is BFS-reachable. The
    /// unreachable sibling is force-included with its own identity, and is not
    /// itself BFS-expanded.
    #[test]
    fn unreachable_chunk_sibling_is_force_included() {
        let (mut g, _iid) = planted(&[
            (1, Some("chunk-1")), // reachable member
            (2, Some("chunk-1")), // unreachable sibling
            (3, None),            // Dependency neighbor of 1
            (4, Some("chunk-1")), // second unreachable sibling (closure)
            (5, None),            // out of sibling 4; must NOT be expanded
        ]);
        let a = nid(1);
        let b = nid(2);
        let c = nid(3);
        let d = nid(4);
        let e = nid(5);
        g.upsert_edge(edge(1, a, c, EdgeType::Dependency)).unwrap();
        g.upsert_edge(edge(2, d, e, EdgeType::Dependency)).unwrap();

        let out = expand(&g, vec![Scored::new(a, 0.8)], 1);
        // Required: candidate (score carried) + Dependency neighbor (UNSCORED).
        assert_eq!(
            out.required,
            vec![Scored::new(a, 0.8), Scored::new(c, UNSCORED)]
        );
        // Siblings: the whole chunk-1 group minus members, id-ascending.
        assert_eq!(
            out.siblings,
            vec![Scored::new(b, UNSCORED), Scored::new(d, UNSCORED)]
        );
        // A sibling is force-included, not expanded: e must stay out.
        let all: HashSet<NodeId> = ids(out.required.clone())
            .into_iter()
            .chain(ids(out.siblings.clone()))
            .collect();
        assert!(
            !all.contains(&e),
            "siblings are not BFS-expanded: {e} leaked in"
        );
        // No node appears in both fields.
        for s in &out.required {
            assert!(
                !out.siblings.iter().any(|x| x.item == s.item),
                "node {} counted twice",
                s.item
            );
        }
    }

    /// Candidates themselves can carry a chunk group: their siblings join even
    /// though the candidate has no traversable edges at all.
    #[test]
    fn candidate_chunk_siblings_join_without_edges() {
        let (g, _iid) = planted(&[(1, Some("chunk-9")), (2, Some("chunk-9"))]);
        let a = nid(1);
        let out = expand(&g, vec![Scored::new(a, 0.9)], 2);
        assert_eq!(out.required, vec![Scored::new(a, 0.9)]);
        assert_eq!(out.siblings, vec![Scored::new(nid(2), UNSCORED)]);
    }

    // -----------------------------------------------------------------------
    // Golden membership (fixture)
    // -----------------------------------------------------------------------

    #[cfg(feature = "fixtures")]
    fn load_rest_api_fixture() -> crate::types::GraphSnapshot {
        use crate::fixtures;
        fixtures::load_snapshot("session-rest-api").expect("fixture loads")
    }

    /// For both golden queries the expansion CONTAINS the golden
    /// `phase2_expanded` members and the phase-1 candidates, with the
    /// candidates first (level 0, structural ordering). Membership only: the
    /// full depth-2 set is the union of all reachable levels, and scores are
    /// T5.3's contract, not this test's.
    #[cfg(feature = "fixtures")]
    #[test]
    fn golden_phase2_membership_passes() {
        use crate::recall::candidates::{candidates, Phase1Input};
        let snap = load_rest_api_fixture();
        let graph = Graph::from_snapshot(snap.clone()).expect("fixture loads");
        let index = crate::graph::index::InvertedIndex::from_snapshot(&snap);
        let goldens = crate::fixtures::load_recall_goldens().unwrap();

        let cases = goldens["cases"].as_array().expect("golden cases array");
        assert_eq!(cases.len(), 2, "fixture must keep both golden queries");
        for case in cases {
            let query = case["query"].as_str().expect("golden query");
            let top_k = case["top_k"].as_u64().expect("golden top_k") as usize;
            let depth = case["depth"].as_u64().expect("golden depth") as usize;
            let expected: Vec<NodeId> = case["phase2_expanded"]
                .as_array()
                .expect("golden phase2_expanded")
                .iter()
                .map(|v| serde_json::from_value(v.clone()).expect("parse NodeId"))
                .collect();

            let phase1 = candidates(&graph, &index, Phase1Input::default(), query, top_k);
            assert!(
                !phase1.is_empty(),
                "golden query {query:?} must produce phase-1 candidates"
            );
            let phase1_ids: Vec<NodeId> = phase1.iter().map(|s| s.item).collect();
            let out = expand(&graph, phase1, depth);

            let got: Vec<NodeId> = ids(out.required.clone());
            // Structural ordering: the candidates are the level-0 prefix of the
            // required list, in phase-1 input order (score-desc, id-asc).
            let head: Vec<NodeId> = got.iter().take(phase1_ids.len()).copied().collect();
            assert_eq!(
                head, phase1_ids,
                "candidates must open the required list for query {query:?}"
            );
            // Membership: every golden phase-2 member and every phase-1
            // candidate survives in the expansion.
            for want in expected.iter().chain(phase1_ids.iter()) {
                assert!(
                    got.contains(want),
                    "golden phase-2 member {want} missing for {query:?} (got {got:?})"
                );
            }
            // The fixture plants no chunk groups: no siblings expected.
            assert!(
                out.siblings.is_empty(),
                "fixture has no chunk_group_id, got siblings {:?} for {query:?}",
                out.siblings
            );
        }
    }
}
