//! Drift detection — spec §9 (§7.7), T4.4.
//!
//! Unweighted shortest path — hop count — over
//! `Causal`/`Dependency`/`Hierarchical` edges to any root goal node; a concept is
//! **drifted** when its distance from the goal region is beyond
//! `drift_threshold` hops. (Spec §9 says "weighted shortest path" but denominates
//! the threshold in *hops*; see "Distance = hop count" below.) Pure,
//! deterministic, cycle-safe:
//! takes `&Graph` + threshold, returns [`Vec<DriftHit>`]. The daemon loop
//! (T4.6) converts hits into `DaemonEvent::Drift` — this module owns no event
//! types and no broadcast channel (cross-task contract).
//!
//! ## Interpretation notes (T4.4 design decisions)
//!
//! * **Root goal nodes.** Spec §9: "Root goal nodes are automatically
//!   `Venerable`" — a goal *concept* is any node whose `content` or
//!   `canonical_key` is named by the session's `root_goal`
//!   ([`root_goal_nodes`], over [`Graph::root_goal_texts`] — the same reading
//!   [`Graph::set_root_goal`] and GC's exclusion list use, so the three cannot
//!   disagree). A goal may be a string, **an array of strings** (spec §6.1's own
//!   example, ALGO-6) or the `{content, key}` object form; a session with no root
//!   goal, or a goal that matches no concept, has no goal node → no hits
//!   (nothing to drift *from*). Multiple goals are multiple BFS sources: a
//!   concept is measured against the nearest one.
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
//! * **"… or no path" (ALGO-5).** Spec §9 warns beyond `drift_threshold` hops
//!   **or on no path**, so a concept with no traversable route to any root goal
//!   *is* a hit — the maximally drifted case, reported with
//!   [`DriftHit::hops`] = `None`. The earlier reading treated it as out of
//!   scope, which meant the one case the spec is least ambiguous about never
//!   warned.
//!
//!   The fixture's isolated `isolated widget`/`isolated sibling` pair is planted
//!   as GC food ("disconnected component (GC step 3 food)", same generator), and
//!   it now fires Drift once before GC collects it. That is correct, not a
//!   conflict: a disconnected concept is drifted *and* garbage, and the daemon
//!   detects on every cycle while GC runs every `gc_interval` mutations — so
//!   the warning legitimately precedes the collection. Emission is
//!   on-transition, so it fires once, not once per cycle.
//! * **Cycle safety (G6).** Multi-hop `Hierarchical` cycles are writable
//!   across calls, so the BFS keeps a visited set — the traversal can never
//!   loop, and no `assert_invariants` guarantee is assumed.
//! * **Determinism.** Goal seeds are sorted by id, per-node neighbor lists
//!   are id-ascending (Graph's typed-neighbor methods sort), and the output is
//!   sorted by node id. `DriftHit::goal` is the goal that *first* reaches the
//!   node in BFS order (shortest distance; ties resolved by seed order →
//!   smallest-id goal in practice).

use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet, VecDeque};

use crate::daemon::hotlist::{Condition, HotList, HotListEntry, HotListPayload};
use crate::graph::Graph;
use crate::types::{EdgeType, NodeId};

/// Spec §9 `drift_threshold` — warn strictly beyond this many hops.
pub const DRIFT_THRESHOLD: usize = 5;

/// "No path to any root goal" for `DaemonEvent::Drift`, whose §6.1 shape is
/// frozen at `hops: u32` and has no unreachable encoding — the maximally drifted
/// case (ALGO-5).
///
/// `DaemonEvent` derives `Serialize`, so **a wire consumer sees `4294967295`**
/// and must treat it as "no path", not as a hop count. `detail` says so in
/// words; see the note on `DaemonEvent::Drift` itself.
pub const DRIFT_HOPS_NO_PATH_EVENT: u32 = u32::MAX;

/// The same sentinel where the shape is `u64` ([`HotListPayload::Drift`]).
///
/// **One number, two widths (NEW-5):** this is
/// `DRIFT_HOPS_NO_PATH_EVENT as u64` — `4_294_967_295` — so the event and the
/// hot-list payload carry the identical value and a consumer comparing against
/// either is right about both. It was `u64::MAX`, which disagreed with what
/// `events::drift_event` actually put on the wire while the cross-reference
/// there claimed the hardcoded `u32::MAX` *was* this documented sentinel.
///
/// `DriftHit` itself carries `Option`, so the sentinel only ever appears at
/// these two frozen boundaries. No real hop count can reach it (hops are bounded
/// by the session's concept count).
pub const DRIFT_HOPS_NO_PATH: u64 = DRIFT_HOPS_NO_PATH_EVENT as u64;

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
    /// order — shortest distance, ties → smallest-id goal in practice), or
    /// `None` when there is **no path** to any goal (ALGO-5).
    pub goal: Option<NodeId>,
    /// Shortest-path hop count over `Causal`/`Dependency`/`Hierarchical`, or
    /// `None` for the no-path case — the maximally drifted concept, which has no
    /// finite distance to report (ALGO-5).
    pub hops: Option<usize>,
    /// Renderable summary ("concept X is N hops from root goal Y", or "… has no
    /// path to any root goal").
    pub detail: String,
}

/// The session's root goal concept nodes: concepts whose `content` or
/// `canonical_key` equals the `root_goal` string. Empty when the goal is
/// unset, non-string, or matches no concept. Deterministic (id-ascending).
pub fn root_goal_nodes(graph: &Graph) -> Vec<NodeId> {
    let texts = graph.root_goal_texts();
    if texts.is_empty() {
        return Vec::new();
    }
    let mut goals: Vec<NodeId> = graph
        .concepts()
        .filter(|c| {
            texts
                .iter()
                .any(|t| c.content == *t || c.canonical_key == *t)
        })
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
        // Every `dist` entry has a `src` entry (goals seed both; each discovery
        // copies the parent's source).
        .filter_map(|(node, d)| drift_hit(*node, Some(src[node]), Some(*d), threshold))
        .collect();
    // ALGO-5: spec §9's "or no path". Every concept the goal-seeded BFS never
    // reached is unreachable from every goal — the maximally drifted case.
    hits.extend(
        graph
            .concepts()
            .filter(|c| !dist.contains_key(&c.id))
            .filter_map(|c| drift_hit(c.id, None, None, threshold)),
    );
    hits.sort_by_key(|h| h.node.0);
    hits
}

/// Is this one concept drifted? The per-node primitive (CONC-5): a BFS
/// **outward from `node`** that stops at the first root goal it reaches, so a
/// well-anchored node costs only the ball of radius `threshold + 1` around it
/// instead of a whole-graph multi-source pass.
///
/// Distance is symmetric (the traversable edge set is treated as undirected),
/// so the hop count this finds is the same one [`detect`] finds from the goal
/// side, and the tie-break matches: among goals at the shortest distance the
/// smallest id wins.
pub fn drift_at(graph: &Graph, node: NodeId, threshold: usize) -> Option<DriftHit> {
    let goals: HashSet<NodeId> = root_goal_nodes(graph).into_iter().collect();
    if goals.is_empty() {
        return None;
    }
    let mut seen: HashSet<NodeId> = HashSet::from([node]);
    let mut frontier: Vec<NodeId> = vec![node];
    let mut hops = 0usize;
    while !frontier.is_empty() {
        // Any goal on this level is at the shortest distance; smallest id wins.
        if let Some(goal) = frontier
            .iter()
            .filter(|n| goals.contains(n))
            .min_by_key(|n| n.0)
        {
            return drift_hit(node, Some(*goal), Some(hops), threshold);
        }
        let mut next: Vec<NodeId> = Vec::new();
        for cur in &frontier {
            for nxt in traversable_neighbors(graph, *cur) {
                if seen.insert(nxt) {
                    next.push(nxt);
                }
            }
        }
        frontier = next;
        hops += 1;
    }
    // ALGO-5: the ball around `node` closed without reaching a goal — no path,
    // which spec §9 warns on. Only nodes that are actually in the graph qualify;
    // a caller asking about an absent id gets `None`.
    graph.node(node)?;
    drift_hit(node, None, None, threshold)
}

/// A hit for a node at a known distance, or `None` when it is within threshold.
/// `hops`/`goal` are `None` for the no-path case (ALGO-5), which always warns.
fn drift_hit(
    node: NodeId,
    goal: Option<NodeId>,
    hops: Option<usize>,
    threshold: usize,
) -> Option<DriftHit> {
    if hops.is_some_and(|h| h <= threshold) {
        return None;
    }
    let detail = match (hops, goal) {
        (Some(h), Some(g)) => {
            format!("concept {node} is {h} hops from root goal {g} (threshold {threshold})")
        }
        // ALGO-5: no path to any goal — maximally drifted, always a hit.
        _ => format!("concept {node} has no path to any root goal (threshold {threshold} hops)"),
    };
    Some(DriftHit {
        node,
        goal,
        hops,
        detail,
    })
}

/// The hot-list payload for a hit. `HotListPayload::Drift` is `{hops: u64, root:
/// NodeId}`, and the no-path case has neither: it renders as
/// [`DRIFT_HOPS_NO_PATH`] hops against the nil root — the encoding
/// `HotListPayload::Drift::root` has documented since T4.2 ("nil when no path").
fn drift_payload(hit: &DriftHit) -> HotListPayload {
    HotListPayload::Drift {
        hops: hit.hops.map_or(DRIFT_HOPS_NO_PATH, |h| h as u64),
        root: hit.goal.unwrap_or_default(),
    }
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
        // Per-node re-check (CONC-5) returning the refreshed payload: a node
        // re-linked closer to the goal drops out of a recall, and one that
        // drifted further renders its new hop count. Drift is clock-free, so
        // `now` is unused — the signature is uniform across conditions.
        let holds =
            move |g: &Graph, _at: DateTime<Utc>| drift_at(g, node, t).map(|h| drift_payload(&h));
        let entry = HotListEntry::new(node, Condition::Drift, drift_payload(hit), holds);
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

        g.set_root_goal(Some(serde_json::json!(42)));
        assert!(
            root_goal_nodes(&g).is_empty(),
            "an unsupported goal shape names no concept"
        );
    }

    /// ALGO-6: spec §6.1's `root_goal` example is a **list**, and an array goal
    /// used to silently disable drift entirely — `as_str()` on an array is
    /// `None`, so there were no goal nodes and therefore no hits, ever.
    #[test]
    fn array_root_goal_anchors_drift_from_every_named_goal() {
        // Two disjoint chains, each with its own goal concept.
        let (mut g, goal_a, chain_a) = chain_graph(7);
        let iid = g.interactions().next().unwrap().id;
        let goal_b = concept(700, iid, "ship the API");
        let goal_b_id = goal_b.id;
        g.insert_concept(goal_b, iid).unwrap();
        // A short chain off goal_b, well within threshold.
        let mut prev = goal_b_id;
        let mut chain_b = Vec::new();
        for n in 0..2u64 {
            let c = concept(710 + n, iid, &format!("b step {n}"));
            let cid = c.id;
            g.insert_concept(c, iid).unwrap();
            g.upsert_edge(edge(8000 + n, prev, cid, EdgeType::Dependency))
                .unwrap();
            chain_b.push(cid);
            prev = cid;
        }

        // String-only goal: chain_b's nodes have no path to it → they now warn
        // (ALGO-5), and goal_b itself does too.
        g.set_root_goal(Some(serde_json::json!("launch the product")));
        let single = detect(&g, DRIFT_THRESHOLD);
        assert!(
            single
                .iter()
                .any(|h| h.node == goal_b_id && h.hops.is_none()),
            "with only goal A declared, goal B's island is unreachable: {single:?}"
        );

        // Both goals declared as an array: chain_b is anchored and drops out,
        // and chain_a's far nodes still report their distance from goal A.
        g.set_root_goal(Some(serde_json::json!([
            "launch the product",
            "ship the API"
        ])));
        let both = detect(&g, DRIFT_THRESHOLD);
        assert_eq!(root_goal_nodes(&g), {
            let mut v = vec![goal_a, goal_b_id];
            v.sort_by_key(|id| id.0);
            v
        });
        for id in chain_b.iter().chain(std::iter::once(&goal_b_id)) {
            assert!(
                !both.iter().any(|h| h.node == *id),
                "goal B's chain is within threshold of its own goal: {both:?}"
            );
        }
        let far = both
            .iter()
            .find(|h| h.node == chain_a[6])
            .expect("chain A's 7-hop node still drifts");
        assert_eq!(far.hops, Some(7));
        assert_eq!(far.goal, Some(goal_a));
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
        assert_eq!(hits[0].goal, Some(goal_id));
        assert_eq!(hits[0].hops, Some(6));
    }

    #[test]
    fn threshold_zero_fires_immediate_dependents_not_the_goal() {
        let (g, goal_id, chain) = chain_graph(2);
        let hits = detect(&g, 0);
        // dist 1 > 0 and dist 2 > 0 for the chain nodes; dist 0 for the goal
        // itself never fires.
        assert_eq!(hits.iter().map(|h| h.node).collect::<Vec<_>>(), chain);
        assert!(hits.iter().all(|h| h.goal == Some(goal_id)));
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

    /// ALGO-5 (behavior change): a disconnected component now **does** warn.
    ///
    /// This test previously asserted `hits.len() == 1` — "no path = out of
    /// scope". Spec §9 says warn beyond the threshold **or on no path**, so the
    /// disconnected pair is in scope and reported with no finite distance. The
    /// far chain node still reports its hop count as before.
    #[test]
    fn disconnected_component_warns_with_no_finite_distance() {
        let (mut g, goal_id, chain) = chain_graph(6);
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
        assert_eq!(hits.len(), 3, "the 6-hop node plus both isolates: {hits:?}");
        let far = hits.iter().find(|h| h.node == chain[5]).unwrap();
        assert_eq!(far.hops, Some(6));
        assert_eq!(far.goal, Some(goal_id));
        for id in [iso_a_id, iso_b_id] {
            let hit = hits.iter().find(|h| h.node == id).unwrap();
            assert_eq!(hit.hops, None, "unreachable: no finite distance");
            assert_eq!(hit.goal, None);
        }
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
        assert_eq!(hits[0].hops, Some(6));
        assert_eq!(hits[1].node, chain2[6]);
        assert_eq!(hits[1].hops, Some(7));
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
        assert_eq!(
            a.iter().map(|h| h.hops).collect::<Vec<_>>(),
            vec![Some(6), Some(7), Some(8)]
        );
    }

    // ------------------------------------------------------------------
    // Fixture — session-drift: exactly one Drift, for the planted node
    // ------------------------------------------------------------------

    #[cfg(feature = "fixtures")]
    #[test]
    fn session_drift_fixture_fires_for_the_planted_node_and_the_isolated_pair() {
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
        let by_content = |content: &str| {
            let id = g
                .concepts()
                .find(|c| c.content == content)
                .unwrap_or_else(|| panic!("fixture must contain {content:?}"))
                .id;
            hits.iter().find(|h| h.node == id).cloned()
        };

        // The planted far node: 6 hops past the 5-hop threshold.
        let planted = by_content("far budget concept").expect("planted node must drift");
        assert_eq!(planted.goal, Some(goals[0]));
        assert_eq!(planted.hops, Some(6));

        // ALGO-5: the isolated pair has no traversable path to the goal (their
        // only edges are `Derives` provenance, which is not a drift edge type),
        // so spec §9's "or no path" clause fires for them. They are also GC's
        // step-3 food; the daemon warns once, then GC collects them on its own
        // interval — a warning before the collection is correct, not a conflict.
        for content in ["isolated widget", "isolated sibling"] {
            let hit = by_content(content)
                .unwrap_or_else(|| panic!("{content} has no path to the goal — §9 warns"));
            assert_eq!(hit.hops, None);
            assert_eq!(hit.goal, None);
            assert!(hit.detail.contains("no path"), "{}", hit.detail);
        }

        assert_eq!(
            hits.len(),
            3,
            "the planted node plus the two unreachable ones: {hits:?}"
        );
        // The 5-hop neighbor ("on path step five") must still not fire.
        assert!(
            by_content("on path step five").is_none(),
            "at-threshold nodes do not fire"
        );
    }

    /// ALGO-5: spec §9 warns beyond `drift_threshold` hops **or on no path**.
    /// The earlier reading filtered over the goal-reachable set only, so the
    /// maximally drifted concept — the one with no structural connection to the
    /// goal at all — was the single case that never warned.
    #[test]
    fn no_path_to_any_goal_is_the_maximally_drifted_case() {
        let (mut g, goal_id, chain) = chain_graph(2);
        // An orphan island: two concepts linked to each other and to nothing
        // else on a drift edge type.
        let iid = g.interactions().next().unwrap().id;
        let a = concept(500, iid, "island a");
        let a_id = a.id;
        g.insert_concept(a, iid).unwrap();
        let b = concept(501, iid, "island b");
        let b_id = b.id;
        g.insert_concept(b, iid).unwrap();
        g.upsert_edge(edge(9500, a_id, b_id, EdgeType::Dependency))
            .unwrap();

        let hits = detect(&g, DRIFT_THRESHOLD);
        let nodes: Vec<NodeId> = hits.iter().map(|h| h.node).collect();
        assert!(nodes.contains(&a_id) && nodes.contains(&b_id), "{hits:?}");
        assert!(
            !nodes.contains(&goal_id) && !nodes.iter().any(|n| chain.contains(n)),
            "the anchored chain is within threshold: {hits:?}"
        );
        for hit in hits.iter().filter(|h| h.node == a_id || h.node == b_id) {
            assert_eq!(hit.hops, None, "no finite distance to report");
            assert_eq!(hit.goal, None);
            assert!(hit.detail.contains("no path"), "{}", hit.detail);
        }

        // The per-node primitive agrees, and the hot-list payload encodes the
        // no-path case as the documented sentinel against the nil root.
        let per_node = drift_at(&g, a_id, DRIFT_THRESHOLD).expect("per-node must agree");
        assert_eq!(per_node.hops, None);
        assert_eq!(
            drift_payload(&per_node),
            HotListPayload::Drift {
                hops: DRIFT_HOPS_NO_PATH,
                root: NodeId::default(),
            }
        );

        // NEW-5: the event and the payload carry the SAME no-path number, under
        // the documented widening. Pre-fix the payload said `u64::MAX` while
        // `drift_event` hardcoded `u32::MAX` — two sentinels, one of them
        // claiming to be the other.
        let event = crate::daemon::events::drift_event(&per_node);
        match event {
            crate::types::DaemonEvent::Drift { hops, detail, .. } => {
                assert_eq!(hops, DRIFT_HOPS_NO_PATH_EVENT);
                assert_eq!(u64::from(hops), DRIFT_HOPS_NO_PATH);
                // What a `Serialize`d consumer actually reads.
                assert_eq!(hops, 4_294_967_295);
                assert!(detail.contains("no path"), "{detail}");
            }
            other => panic!("expected Drift, got {other:?}"),
        }

        // With no root goal at all there is nothing to drift *from* — unchanged.
        g.set_root_goal(None);
        assert!(detect(&g, DRIFT_THRESHOLD).is_empty());
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
        assert!(
            !hot.revalidate(&g, chain[5], ts(0)),
            "condition no longer holds"
        );
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
            hot.revalidate(&g, chain[5], ts(0)),
            "still drifted -> entry survives"
        );
        assert!(hot.contains(chain[5]));
        assert_eq!(hot.len(), 1);
    }

    // ------------------------------------------------------------------
    // CONC-5 — per-node re-validation
    // ------------------------------------------------------------------

    /// CONC-5/XP-3: a surviving entry's payload is rebuilt from the per-node
    /// re-check, so a node that drifts *further* renders its new hop count.
    ///
    /// Pre-fix the predicate was `detect(g, t).iter().any(|h| h.node == node)`
    /// — a whole-graph pass returning a bool, which could neither refresh the
    /// payload nor be afforded on recall's lock-held path.
    #[test]
    fn revalidate_refreshes_drift_hops_when_the_distance_changes() {
        use crate::daemon::hotlist::HotList;
        let (mut g, goal_id, chain) = chain_graph(7);
        let mut hot = HotList::new();
        record(&mut hot, &g, DRIFT_THRESHOLD);
        let far = chain[6]; // 7 hops
        match &hot.iter().find(|e| e.node == far).unwrap().payload {
            HotListPayload::Drift { hops, .. } => assert_eq!(*hops, 7),
            other => panic!("expected Drift payload, got {other:?}"),
        }

        // Short-circuit one link of the chain (chain[0] -> chain[2]): every
        // node past it moves one hop closer, so `far` is now 6 hops out.
        g.upsert_edge(edge(9000, chain[0], chain[2], EdgeType::Dependency))
            .unwrap();
        assert!(hot.revalidate(&g, far, ts(0)), "6 hops still drifted");
        match &hot.iter().find(|e| e.node == far).unwrap().payload {
            HotListPayload::Drift { hops, .. } => assert_eq!(
                *hops, 6,
                "the payload must be rebuilt from read-time distance, not frozen at 7"
            ),
            other => panic!("expected Drift payload, got {other:?}"),
        }

        // Linking it straight to the goal puts it inside the threshold: evicted.
        g.upsert_edge(edge(9001, goal_id, far, EdgeType::Dependency))
            .unwrap();
        assert!(!hot.revalidate(&g, far, ts(0)), "1 hop is not drifted");
        assert!(!hot.contains(far), "no ghost Drift entry");
        // chain[5] was the run's other hit (6 hops); the first short-circuit
        // pulled it back to 5, so its own re-validation evicts it too.
        assert!(!hot.revalidate(&g, chain[5], ts(0)));
        assert!(hot.is_empty());
    }

    /// CONC-5: the per-node primitive is the single source of truth — it must
    /// agree with the whole-graph pass on **every** node, so replacing the
    /// predicates' `detect` call with `drift_at` cannot change what recall sees.
    #[test]
    fn drift_at_agrees_with_the_whole_graph_pass_on_every_node() {
        let (g, goal_id, chain) = chain_graph(8);
        let whole: HashMap<NodeId, DriftHit> = detect(&g, DRIFT_THRESHOLD)
            .into_iter()
            .map(|h| (h.node, h))
            .collect();
        for node in std::iter::once(goal_id).chain(chain.iter().copied()) {
            assert_eq!(
                drift_at(&g, node, DRIFT_THRESHOLD).as_ref(),
                whole.get(&node),
                "per-node and whole-graph drift must agree for {node}"
            );
        }
    }
}
