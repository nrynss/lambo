//! Drift detection — spec §9 (§7.7), T4.4.
//!
//! Weighted shortest path over `Causal`/`Dependency`/`Hierarchical` edges to any
//! root goal node; a concept is **drifted** when its distance from the goal
//! region is beyond `drift_threshold` hops. Pure, deterministic, cycle-safe:
//! takes `&Graph` + threshold, returns [`Vec<DriftHit>`]. The daemon loop
//! (T4.6) converts hits into `DaemonEvent::Drift` — this module owns no event
//! types and no broadcast channel (cross-task contract).
//!
//! ## Interpretation notes (T4.4 design decisions)
//!
//! * **Root goal nodes.** Spec §9: "Root goal nodes are automatically
//!   `Venerable`" — the goal *concept* is the node whose `content` (or
//!   `canonical_key`) equals the session's `root_goal` string
//!   ([`root_goal_nodes`]; the same matcher [`Graph::set_root_goal`] uses to
//!   auto-promote the concept). A session with no root goal, or a goal that
//!   matches no concept, has no goal node → no hits (nothing to drift *from*).
//!   A structured (non-string) goal is stored but cannot name a concept.
//! * **Traversal edge set and direction.** Paths use only
//!   [`DRIFT_EDGE_TYPES`] (Causal/Dependency/Hierarchical, spec §9), treated as
//!   **undirected**. The committed fixture's chain is directed *away* from the
//!   goal (`scripts/gen-fixtures.py`: "drift chain (directed Dependency):
//!   goal -> ... -> far (far at 6 hops)"), so an orientation-sensitive walk
//!   would report "no path" for exactly the node the fixture plants as
//!   drifted. `Causal`/`Dependency`/`Hierarchical` edges are Concept↔Concept
//!   by construction (`record_edge` enforces endpoint kinds), so the hop
//!   count is well-defined over concept nodes only.
//! * **Distance = hop count.** Spec §9: "weighted shortest path …
//!   warn beyond `drift_threshold=5` hops". The threshold is denominated in
//!   **hops**, and the fixture's chain is unit-weight, so the operative metric
//!   is the unweighted shortest-path hop count (multi-source BFS). Edge
//!   weights are GC's concern (`min_edge_weight` decay), not drift's. "Warn
//!   beyond" is strict: `dist > threshold` fires; `dist == threshold` does not.
//! * **"… or no path".** A concept with **no** traversable route to any root
//!   goal is *out of scope* and emits no hit: it is not drifted *from* the
//!   goal — it shares no structural connection with it. The fixture's
//!   isolated `isolated widget`/`isolated sibling` component is planted as GC
//!   food, not drift ("disconnected component (GC step 3 food)", same
//!   generator), and the acceptance "exactly one Drift event, for the planted
//!   node" pins this reading. Reachability and distance come from the **same**
//!   BFS, so every in-scope concept has a finite distance.
//! * **Cycle safety (G6).** Multi-hop `Hierarchical` cycles are writable
//!   across calls, so the BFS keeps a visited set — the traversal can never
//!   loop, and no `assert_invariants` guarantee is assumed.
//! * **Determinism.** Goal seeds are sorted by id, per-node neighbor lists
//!   are id-ascending (Graph's typed-neighbor methods sort), and the output is
//!   sorted by node id. `DriftHit::goal` is the goal that *first* reaches the
//!   node in BFS order (shortest distance; ties resolved by seed order →
//!   smallest-id goal in practice).

use std::collections::{HashMap, HashSet, VecDeque};

use crate::daemon::hotlist::{Condition, HotList, HotListEntry, HotListPayload};
use crate::graph::Graph;
use crate::types::{EdgeType, NodeId};

/// Spec §9 `drift_threshold` — warn strictly beyond this many hops.
pub const DRIFT_THRESHOLD: usize = 5;

/// Edge types participating in drift paths (spec §9).
pub const DRIFT_EDGE_TYPES: [EdgeType; 3] = [
    EdgeType::Causal,
    EdgeType::Dependency,
    EdgeType::Hierarchical,
];

/// One drifted concept: `node` is `hops` structural hops from root goal `goal`.
///
/// T5.3 renders `DaemonEvent::Drift { node_id, hops, detail }` from this
/// (T4.6's job to convert); `detail` carries a renderable sentence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriftHit {
    /// The drifted concept.
    pub node: NodeId,
    /// The root goal it was measured against (the one reached first in BFS
    /// order — shortest distance, ties → smallest-id goal in practice).
    pub goal: NodeId,
    /// Shortest-path hop count over `Causal`/`Dependency`/`Hierarchical`.
    pub hops: usize,
    /// Renderable summary ("concept X is N hops from root goal Y").
    pub detail: String,
}

/// The session's root goal concept nodes: concepts whose `content` or
/// `canonical_key` equals the `root_goal` string. Empty when the goal is
/// unset, non-string, or matches no concept. Deterministic (id-ascending).
pub fn root_goal_nodes(graph: &Graph) -> Vec<NodeId> {
    let Some(text) = graph.root_goal().and_then(serde_json::Value::as_str) else {
        return Vec::new();
    };
    let mut goals: Vec<NodeId> = graph
        .concepts()
        .filter(|c| c.content == text || c.canonical_key == text)
        .map(|c| c.id)
        .collect();
    goals.sort_by_key(|id| id.0);
    goals
}

/// Detect drifted concepts: every concept within the goal's traversable
/// component whose shortest-path hop count to the nearest root goal is
/// strictly greater than `threshold` (spec §9; see module docs for the "no
/// path" scope rule). Deterministic; cycle-safe.
pub fn detect(graph: &Graph, threshold: usize) -> Vec<DriftHit> {
    let goals = root_goal_nodes(graph);
    if goals.is_empty() {
        return Vec::new();
    }

    // Multi-source BFS over the undirected traversable edge set. First
    // discovery wins (all hops weigh 1), giving the shortest distance plus a
    // deterministic nearest-goal attribution.
    let mut dist: HashMap<NodeId, usize> = HashMap::new();
    let mut src: HashMap<NodeId, NodeId> = HashMap::new();
    let mut queue: VecDeque<NodeId> = VecDeque::new();
    for g in &goals {
        if dist.insert(*g, 0).is_none() {
            src.insert(*g, *g);
            queue.push_back(*g);
        }
    }
    while let Some(cur) = queue.pop_front() {
        let d = dist[&cur];
        let s = src[&cur];
        for nxt in traversable_neighbors(graph, cur) {
            if dist.contains_key(&nxt) {
                continue;
            }
            dist.insert(nxt, d + 1);
            src.insert(nxt, s);
            queue.push_back(nxt);
        }
    }

    let mut hits: Vec<DriftHit> = dist
        .iter()
        .filter(|(_, d)| **d > threshold)
        .map(|(node, d)| {
            // Every `dist` entry has a `src` entry (goals seed both; each
            // discovery copies the parent's source).
            let goal = src[node];
            DriftHit {
                node: *node,
                goal,
                hops: *d,
                detail: format!(
                    "concept {node} is {d} hops from root goal {goal} (threshold {threshold})"
                ),
            }
        })
        .collect();
    hits.sort_by_key(|h| h.node.0);
    hits
}

/// Run [`detect`] and refresh the daemon hot list (T4.2): one
/// `Condition::Drift` entry per hit, carrying T4.2's drift payload (hops +
/// root goal — what T5.3 renders) and a re-validation predicate that
/// re-runs [`detect`] so a recall-time re-validation (T5.3) drops the entry
/// once the node's distance falls back to the threshold (or the goal is
/// cleared). The hot list is a side effect; the hits are returned so the
/// daemon loop (T4.6) can emit `DaemonEvent::Drift` on transition and sync
/// the hot list against them ([`HotList::retain_conditions`] — entries no
/// longer detected are dropped each cycle).
pub fn record(hotlist: &mut HotList, graph: &Graph, threshold: usize) -> Vec<DriftHit> {
    let hits = detect(graph, threshold);
    for hit in &hits {
        let node = hit.node;
        let t = threshold;
        let holds = move |g: &Graph| detect(g, t).iter().any(|h| h.node == node);
        let entry = HotListEntry::new(
            node,
            Condition::Drift,
            HotListPayload::Drift {
                hops: hit.hops as u64,
                root: hit.goal,
            },
            holds,
        );
        let _ = hotlist.insert(entry);
    }
    hits
}

/// Undirected traversable neighbors of `node` over [`DRIFT_EDGE_TYPES`]:
/// out- and in-neighbors per type, deduplicated, id-ascending. `Causal`/
/// `Dependency`/`Hierarchical` edges are Concept↔Concept by construction, so
/// every returned neighbor is a concept.
fn traversable_neighbors(graph: &Graph, node: NodeId) -> Vec<NodeId> {
    let mut seen: HashSet<NodeId> = HashSet::new();
    let mut out = Vec::new();
    for ty in DRIFT_EDGE_TYPES {
        for n in graph.out_neighbors_typed(node, ty) {
            if seen.insert(n) {
                out.push(n);
            }
        }
        for n in graph.in_neighbors_typed(node, ty) {
            if seen.insert(n) {
                out.push(n);
            }
        }
    }
    out.sort_by_key(|id| id.0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Graph;
    use crate::types::{
        AgentId, CanonizationStatus, Concept, ConceptType, Edge, Interaction, SessionId,
    };
    use chrono::{TimeZone, Utc};
    use uuid::Uuid;

    fn ts(m: i64) -> chrono::DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000 + m * 60, 0).unwrap()
    }

    fn sid() -> SessionId {
        SessionId::from("t4.4-drift")
    }

    fn interaction(id: u64, prev: Option<u64>, at: i64) -> Interaction {
        Interaction {
            id: NodeId(Uuid::from_u64_pair(0, id)),
            session_id: sid(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: Some("p".into()),
            previous_id: prev.map(|p| NodeId(Uuid::from_u64_pair(0, p))),
            created_at: ts(at),
        }
    }

    fn concept(id: u64, origin: NodeId, content: &str) -> Concept {
        Concept {
            id: NodeId(Uuid::from_u64_pair(1, id)),
            session_id: sid(),
            content: content.into(),
            canonical_key: content.to_string(),
            concept_type: ConceptType::Entity,
            origin_interaction: origin,
            origin_agent: AgentId::from("agent-a"),
            created_at: ts(0),
            access_count: 0,
            last_accessed: None,
            gc_survived: 0,
            canonization_status: CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: None,
            embedding: None,
            chunk_group_id: None,
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

    /// Goal concept + a Dependency chain `goal -> c1 -> ... -> cH` (H hops),
    /// every concept derived from one interaction (satisfies §5.7 structure),
    /// and the session root goal set to the goal concept's content (mirrors
    /// the fixture shape). Returns `(graph, goal_id, chain_ids)` with
    /// `chain_ids` in hop order.
    fn chain_graph(hops: usize) -> (Graph, NodeId, Vec<NodeId>) {
        let mut g = Graph::new(sid());
        let i1 = interaction(1, None, 0);
        let iid = i1.id;
        g.insert_interaction(i1).unwrap();
        let goal = concept(10, iid, "launch the product");
        let goal_id = goal.id;
        g.insert_concept(goal, iid).unwrap();
        let mut chain = Vec::new();
        let mut prev = goal_id;
        for h in 0..hops {
            let c = concept(100 + h as u64, iid, &format!("step {h}"));
            let cid = c.id;
            g.insert_concept(c, iid).unwrap();
            g.upsert_edge(edge(1000 + h as u64, prev, cid, EdgeType::Dependency))
                .unwrap();
            chain.push(cid);
            prev = cid;
        }
        g.set_root_goal(Some(serde_json::json!("launch the product")));
        (g, goal_id, chain)
    }

    // ------------------------------------------------------------------
    // Root goal identification
    // ------------------------------------------------------------------

    #[test]
    fn root_goal_nodes_match_by_content_or_canonical_key() {
        let (g, goal_id, _) = chain_graph(1);
        assert_eq!(root_goal_nodes(&g), vec![goal_id]);

        // Canonical-key match: goal text equals a concept's canonical_key
        // while differing from its content.
        let mut g2 = Graph::new(sid());
        let i1 = interaction(1, None, 0);
        let iid = i1.id;
        g2.insert_interaction(i1).unwrap();
        let mut c = concept(1, iid, "some other wording");
        c.canonical_key = "launch product".to_string();
        let cid = c.id;
        g2.insert_concept(c, iid).unwrap();
        g2.set_root_goal(Some(serde_json::json!("launch product")));
        assert_eq!(root_goal_nodes(&g2), vec![cid]);
    }

    #[test]
    fn root_goal_nodes_empty_without_goal_or_without_match() {
        let (mut g, _, _) = chain_graph(1);
        g.set_root_goal(None);
        assert!(root_goal_nodes(&g).is_empty(), "no goal -> no goal nodes");

        let (mut g, _, _) = chain_graph(1);
        g.set_root_goal(Some(serde_json::json!("no such concept")));
        assert!(root_goal_nodes(&g).is_empty());

        g.set_root_goal(Some(serde_json::json!({ "structured": true })));
        assert!(
            root_goal_nodes(&g).is_empty(),
            "non-string goal names no concept"
        );
    }

    // ------------------------------------------------------------------
    // Distance / threshold semantics
    // ------------------------------------------------------------------

    #[test]
    fn no_hits_without_root_goal() {
        let (mut g, _, chain) = chain_graph(6);
        g.set_root_goal(None);
        assert!(detect(&g, DRIFT_THRESHOLD).is_empty());
        assert_eq!(chain.len(), 6);
    }

    #[test]
    fn chain_at_threshold_does_not_fire_and_beyond_does() {
        // 5 hops == threshold: not drifted ("beyond" is strict).
        let (g, _, _) = chain_graph(5);
        assert_eq!(detect(&g, DRIFT_THRESHOLD), vec![]);

        // 6 hops: exactly the terminal node, at hops 6, vs the goal.
        let (g, goal_id, chain) = chain_graph(6);
        let hits = detect(&g, DRIFT_THRESHOLD);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].node, chain[5]);
        assert_eq!(hits[0].goal, goal_id);
        assert_eq!(hits[0].hops, 6);
    }

    #[test]
    fn threshold_zero_fires_immediate_dependents_not_the_goal() {
        let (g, goal_id, chain) = chain_graph(2);
        let hits = detect(&g, 0);
        // dist 1 > 0 and dist 2 > 0 for the chain nodes; dist 0 for the goal
        // itself never fires.
        assert_eq!(hits.iter().map(|h| h.node).collect::<Vec<_>>(), chain);
        assert!(hits.iter().all(|h| h.goal == goal_id));
    }

    #[test]
    fn shortest_path_wins_over_longer_route() {
        // goal -> A -> B -> C (3 hops) plus a direct goal -> C edge: C must be
        // measured at 1 hop, not 3 — BFS shortest path.
        let (mut g, goal_id, chain) = chain_graph(3);
        g.upsert_edge(edge(950, goal_id, chain[2], EdgeType::Dependency))
            .unwrap();
        let hits = detect(&g, DRIFT_THRESHOLD);
        assert!(hits.is_empty(), "shortcut keeps every node <= 2 hops");
        // With the shortcut removed by threshold 1, only the 2- and 3-hop
        // nodes fire (the shortcut makes C 1 hop, so it must not fire).
        let hits = detect(&g, 1);
        assert_eq!(hits.len(), 1, "only the 2-hop node fires at threshold 1");
        assert_eq!(hits[0].node, chain[1]);
    }

    // ------------------------------------------------------------------
    // Scope: disconnected components and cycles
    // ------------------------------------------------------------------

    #[test]
    fn disconnected_component_without_goal_is_out_of_scope() {
        // Goal chain of 6 hops (terminal drifts) plus an isolated
        // Dependency-linked pair with no route to the goal: the pair must
        // produce no hits ("no path" = out of scope, module docs).
        let (mut g, _, chain) = chain_graph(6);
        let iid = g.interactions().next().unwrap().id;
        let iso_a = concept(300, iid, "isolated a");
        let iso_a_id = iso_a.id;
        g.insert_concept(iso_a, iid).unwrap();
        let iso_b = concept(301, iid, "isolated b");
        let iso_b_id = iso_b.id;
        g.insert_concept(iso_b, iid).unwrap();
        g.upsert_edge(edge(901, iso_a_id, iso_b_id, EdgeType::Dependency))
            .unwrap();

        let hits = detect(&g, DRIFT_THRESHOLD);
        assert_eq!(hits.len(), 1, "only the planted 6-hop node drifts");
        assert_eq!(hits[0].node, chain[5]);
    }

    #[test]
    fn detection_is_cycle_safe_on_writable_hierarchical_cycle() {
        // G6: multi-hop Hierarchical cycles are writable across calls
        // (only self-loops are rejected at write). The BFS must terminate and
        // still report the planted drift beyond a cycle.
        let (mut g, _, chain) = chain_graph(2);
        // A->B->C->A Hierarchical cycle attached to the terminal node.
        let iid = g.interactions().next().unwrap().id;
        let a = concept(400, iid, "cycle a");
        let a_id = a.id;
        g.insert_concept(a, iid).unwrap();
        let b = concept(401, iid, "cycle b");
        let b_id = b.id;
        g.insert_concept(b, iid).unwrap();
        let c = concept(402, iid, "cycle c");
        let c_id = c.id;
        g.insert_concept(c, iid).unwrap();
        g.upsert_edge(edge(902, a_id, b_id, EdgeType::Hierarchical))
            .unwrap();
        g.upsert_edge(edge(903, b_id, c_id, EdgeType::Hierarchical))
            .unwrap();
        g.upsert_edge(edge(904, c_id, a_id, EdgeType::Hierarchical))
            .unwrap();
        // Attach the cycle to the chain so it is in scope (3-4 hops: no drift).
        g.upsert_edge(edge(905, chain[1], a_id, EdgeType::Hierarchical))
            .unwrap();

        assert!(detect(&g, DRIFT_THRESHOLD).is_empty());

        // A 7-hop chain with a 2-cycle in the middle still terminates and
        // flags the 6- and 7-hop tails (both beyond the 5-hop threshold).
        let (mut g2, _, chain2) = chain_graph(7);
        let iid2 = g2.interactions().next().unwrap().id;
        let a2 = concept(410, iid2, "cycle a2");
        let a2_id = a2.id;
        g2.insert_concept(a2, iid2).unwrap();
        g2.upsert_edge(edge(906, chain2[2], a2_id, EdgeType::Hierarchical))
            .unwrap();
        g2.upsert_edge(edge(907, a2_id, chain2[2], EdgeType::Hierarchical))
            .unwrap(); // 2-cycle at node 3
        let hits = detect(&g2, DRIFT_THRESHOLD);
        assert_eq!(hits.len(), 2, "6- and 7-hop nodes drift past the cycle");
        assert_eq!(hits[0].node, chain2[5]);
        assert_eq!(hits[0].hops, 6);
        assert_eq!(hits[1].node, chain2[6]);
        assert_eq!(hits[1].hops, 7);
    }

    #[test]
    fn hits_are_deterministic_and_id_sorted() {
        let (g, _, _) = chain_graph(8);
        // Distances 6, 7, 8 along id-ascending chain nodes → three hits.
        let a = detect(&g, DRIFT_THRESHOLD);
        let b = detect(&g, DRIFT_THRESHOLD);
        assert_eq!(a, b, "detect must be deterministic");
        assert_eq!(a.len(), 3);
        for w in a.windows(2) {
            assert!(w[0].node.0 < w[1].node.0, "hits sorted by node id");
        }
        assert_eq!(a.iter().map(|h| h.hops).collect::<Vec<_>>(), vec![6, 7, 8]);
    }

    // ------------------------------------------------------------------
    // Fixture — session-drift: exactly one Drift, for the planted node
    // ------------------------------------------------------------------

    #[cfg(feature = "fixtures")]
    #[test]
    fn session_drift_fixture_fires_exactly_one_hit_for_planted_node() {
        use crate::fixtures::load_snapshot;
        let snap = load_snapshot("session-drift").unwrap();
        let g = Graph::from_snapshot(snap).unwrap();

        // The goal concept is "launch the product" (Venerable in the fixture).
        let goals = root_goal_nodes(&g);
        assert_eq!(goals.len(), 1);
        assert_eq!(
            g.concepts().find(|c| c.id == goals[0]).unwrap().content,
            "launch the product"
        );

        let hits = detect(&g, DRIFT_THRESHOLD);
        assert_eq!(
            hits.len(),
            1,
            "exactly one drift event for the planted node"
        );
        let planted = g
            .concepts()
            .find(|c| c.content == "far budget concept")
            .expect("fixture must contain the planted drifted concept");
        assert_eq!(hits[0].node, planted.id);
        assert_eq!(hits[0].goal, goals[0]);
        assert_eq!(hits[0].hops, 6);
        // The 5-hop neighbor ("on path step five") must not fire, and the
        // isolated GC component must not fire.
        assert!(
            hits.iter().all(|h| h.hops > 5),
            "only nodes beyond the threshold fire"
        );
    }

    // ------------------------------------------------------------------
    // Hot list integration (T4.2) — Condition::Drift entries + revalidation
    // ------------------------------------------------------------------

    #[test]
    fn record_inserts_drift_entries_only_for_drifted_nodes() {
        use crate::daemon::hotlist::{Condition, HotList, HotListPayload};
        let (mut g, goal_id, chain) = chain_graph(6);
        let mut hot = HotList::new();

        let hits = record(&mut hot, &g, DRIFT_THRESHOLD);
        assert_eq!(hits.len(), 1);

        assert_eq!(hot.len(), 1, "one hot-list entry for the single hit");
        let entry = hot.iter().next().unwrap();
        assert_eq!(entry.node, chain[5]);
        assert_eq!(entry.condition, Condition::Drift);
        match &entry.payload {
            HotListPayload::Drift { hops, root } => {
                assert_eq!(*hops, 6);
                assert_eq!(*root, goal_id);
            }
            other => panic!("expected Drift payload, got {other:?}"),
        }
        record(&mut hot, &g, DRIFT_THRESHOLD);
        assert_eq!(hot.len(), 1);

        // The 5-hop neighbor and the goal itself are not on the hot list.
        assert!(!hot.contains(chain[4]));
        assert!(!hot.contains(goal_id));

        // Clearing the goal → detect finds nothing → the entry's predicate
        // stops holding and revalidate evicts it (spec §9: conditions are
        // re-validated on each recall, not on a timer).
        g.set_root_goal(None);
        assert!(!hot.revalidate(&g, chain[5]), "condition no longer holds");
        assert!(!hot.contains(chain[5]), "stale entry evicted");
        assert!(hot.is_empty());
    }

    #[test]
    fn record_keeps_entry_while_drift_persists() {
        use crate::daemon::hotlist::HotList;
        let (g, _, chain) = chain_graph(6);
        let mut hot = HotList::new();
        record(&mut hot, &g, DRIFT_THRESHOLD);

        assert!(
            hot.revalidate(&g, chain[5]),
            "still drifted -> entry survives"
        );
        assert!(hot.contains(chain[5]));
        assert_eq!(hot.len(), 1);
    }
}
