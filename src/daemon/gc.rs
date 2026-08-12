//! GC — spec §9, periodic only (T4.5; canonization's food).
//!
//! Runs every `gc_interval` mutations (the caller — T4.6's loop — decides
//! *when*; this module decides *what*). [`run`] is a pure, fixture-testable
//! function over `&mut Graph`: it performs the seven spec steps and returns a
//! [`GcOutcome`] the owner records for T5.4 (cache epoch) and T6.4 (canonical
//! budget).
//!
//! ## Spec → code mapping
//!
//! 1. **Edge cleanup** — every edge with `weight < min_edge_weight` whose last
//!    reinforcement is older than `gc_edge_ttl` is removed. The TTL anchor is
//!    `last_reinforced` (the edge's last activity; a never-reinforced edge is
//!    dead from its write). All edge types qualify — the spec names none.
//! 2. **Concept cleanup** — orphans (no incident edges after step 1) and
//!    sub-threshold concepts (daemon composite score below
//!    [`MIN_CONCEPT_SCORE`], computed with the spec §9 formula via
//!    `score::rescore`) are collected, **excluding** Venerable, Canonical, and
//!    root-goal concepts.
//! 3. **Disconnected-component cleanup** — a cycle-safe BFS (visited set, per
//!    the G6 binding note — never assume Hierarchical acyclicity) from the
//!    temporal chain over the full undirected graph; every concept not reached
//!    is collected. Protected classes are exempt ("protected classes survive"
//!    contract); interactions are append-only and never collected.
//! 4. **Index maintenance** — the inverted index (T2.6) is owner-side (P3
//!    contract, `src/graph/mod.rs`), so `run(&mut Graph, …)` cannot reach it.
//!    [`GcOutcome::concepts_collected`] + [`sync_index`] are the hook: the
//!    owner MUST call `sync_index(&outcome, &mut index)` after `run` (each
//!    collected id → `InvertedIndex::remove`). Survivor bumps never change
//!    content, so no re-`add` is ever needed.
//! 5. **`gc_survived += 1` on all survivors** — via
//!    [`Graph::bump_gc_survived`] (saturating `i32`), which emits `UpsertNode`
//!    mutations so the durable store mirrors the counter. Stage 1's input —
//!    the reason GC cannot be cut.
//! 6. **Canonical budget** — GC *records* the Canonical count and the
//!    over-budget flag in [`GcOutcome`]; demotion (lowest-blast-radius, spec
//!    §10) is T6.4's job — GC never demotes.
//! 7. **MutationEpoch** — bumped by GC's own mutations (edge removals, node
//!    removals, survivor upserts); `src/graph/graph.rs`'s epoch doc calls this
//!    "redundant but harmless (any mutation already bumps the epoch)".
//!    [`GcOutcome::epoch_before`]/[`epoch_after`] prove the bump.
//!
//! `max_concept_nodes` is advisory-only: over-capacity produces a warning in
//! [`GcOutcome::warnings`]; nothing is evicted (spec §9: "Capacity is
//! elastic").
//!
//! ## Fixture note — session-drift "disconnected component"
//!
//! `scripts/gen-fixtures.py` names concepts 20/21 ("isolated widget" /
//! "isolated sibling") "disconnected component (GC step 3 food)", but the
//! generated JSON also carries their `Derives` provenance edges from
//! interaction 2, which DO connect them to the temporal chain in the loaded
//! graph. The fixture test materializes the planted disconnection by dropping
//! those two provenance edges before running GC — a TEST-ONLY state
//! reconstruction of what the generator planted ("disconnected component (GC
//! step 3 food)"), NOT a step-1 behavior: both edges sit at weight 0.9 (≥
//! [`MIN_EDGE_WEIGHT`]), so step 1's predicate never touches them. With the
//! drops, the pair is unreachable and collected.

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use std::collections::HashSet;

use crate::config::ScoringWeights;
use crate::graph::index::InvertedIndex;
use crate::graph::Graph;
use crate::types::{CanonizationStatus, Concept, EdgeType, NodeId};

/// Below this weight, an edge past its TTL is removed (step 1).
///
/// v0.6.0's value is not in-repo — v0.1 decision. Kept below the structural
/// write weights (`Derives` 0.9, `Temporal`/`Dependency`/`Causal` 1.0) so
/// provenance and load-bearing edges survive a default run.
pub const MIN_EDGE_WEIGHT: f64 = 0.5;
/// An edge untouched for this long is "past `gc_edge_ttl`" (step 1).
pub const GC_EDGE_TTL: ChronoDuration = ChronoDuration::seconds(3600);
/// Below this daemon composite score, a concept is sub-threshold (step 2).
/// v0.6.0's value is not in-repo — v0.1 decision.
pub const MIN_CONCEPT_SCORE: f64 = 0.3;
/// Advisory concept-count ceiling: warn above, never evict (spec §9).
pub const MAX_CONCEPT_NODES: usize = 10_000;

/// Parameters for one GC run. [`Default`] carries the named v0.1 decisions.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GcParams {
    /// The clock for TTL evaluation — tests control it (mocked time).
    pub now: DateTime<Utc>,
    /// Step 1: remove edges with `weight < min_edge_weight` **and** untouched
    /// for `gc_edge_ttl`.
    pub min_edge_weight: f64,
    /// Step 1: age anchor — `now - last_reinforced > gc_edge_ttl`.
    pub gc_edge_ttl: ChronoDuration,
    /// Step 2: daemon composite score below this is sub-threshold.
    pub min_concept_score: f64,
    /// Advisory capacity ceiling — warn above, never evict.
    pub max_concept_nodes: usize,
    /// Step 6: Canonical budget ceiling; over it → `canonical_over_budget`.
    pub max_canonical_nodes: usize,
}

impl Default for GcParams {
    fn default() -> Self {
        Self {
            now: Utc::now(),
            min_edge_weight: MIN_EDGE_WEIGHT,
            gc_edge_ttl: GC_EDGE_TTL,
            min_concept_score: MIN_CONCEPT_SCORE,
            max_concept_nodes: MAX_CONCEPT_NODES,
            max_canonical_nodes: 1000, // spec §10
        }
    }
}

/// Everything one GC run did, for the owner (T4.6), T5.4, and T6.4.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GcOutcome {
    /// Step 1: edge ids removed for being below weight past TTL.
    pub edges_removed: Vec<NodeId>,
    /// Steps 2+3: concept ids collected, together with their incident edges.
    pub concepts_collected: Vec<NodeId>,
    /// Step 5: concept ids that survived (each got `gc_survived += 1`).
    pub survivors: Vec<NodeId>,
    /// Step 6: number of Canonical concepts after cleanup.
    pub canonical_count: usize,
    /// Step 6: `canonical_count > max_canonical_nodes` — recorded for T6.4's
    /// demotion sweep; GC never demotes.
    pub canonical_over_budget: bool,
    /// Step 6: the budget ceiling this run was checked against.
    pub max_canonical_nodes: usize,
    /// Advisory `max_concept_nodes` warnings (never evictions).
    pub warnings: Vec<String>,
    /// Step 7: epoch before / after — GC's mutations bump it (see module docs).
    pub epoch_before: u64,
    pub epoch_after: u64,
}

/// Run one full GC cycle (spec §9 steps 1–7, in order).
///
/// Pure RAM work: no I/O, no locks — the caller owns the graph lock. All
/// outcome vectors are id-ascending so results are deterministic.
pub fn run(graph: &mut Graph, params: GcParams) -> GcOutcome {
    let epoch_before = graph.epoch();
    let mut outcome = GcOutcome {
        epoch_before,
        max_canonical_nodes: params.max_canonical_nodes,
        ..Default::default()
    };

    // Protected classes: Venerable / Canonical / root-goal. Computed once —
    // canonization statuses do not change during a GC run.
    let protected: HashSet<NodeId> = graph
        .concepts()
        .filter(|c| is_protected(graph, c))
        .map(|c| c.id)
        .collect();

    // Step 1 — edge cleanup: below min weight AND past TTL.
    for eid in dead_edge_ids(graph, params) {
        if graph.remove_edge(eid).is_ok() {
            outcome.edges_removed.push(eid);
        }
    }

    // Step 2 — concept cleanup: orphans + sub-threshold, excluding protected.
    let scores: std::collections::HashMap<NodeId, f64> =
        crate::daemon::score::rescore(graph, &ScoringWeights::default())
            .into_iter()
            .map(|s| (s.item, s.score))
            .collect();
    let mut candidates: Vec<NodeId> = Vec::new();
    for c in graph.concepts() {
        if protected.contains(&c.id) {
            continue;
        }
        let orphan = graph.incident_edges(c.id).is_empty();
        let below = scores.get(&c.id).copied().unwrap_or(0.0) < params.min_concept_score;
        if orphan || below {
            candidates.push(c.id);
        }
    }
    candidates.sort_by_key(|id| id.0);
    for id in &candidates {
        if graph.remove_node(*id).is_ok() {
            outcome.concepts_collected.push(*id);
        }
    }

    // Step 3 — disconnected components: cycle-safe BFS from the temporal chain.
    let mut reachable: HashSet<NodeId> = HashSet::new();
    let mut stack: Vec<NodeId> = graph.temporal_chain().to_vec();
    for &seed in &stack {
        reachable.insert(seed);
    }
    while let Some(n) = stack.pop() {
        for nb in graph.out_neighbors(n) {
            if reachable.insert(nb) {
                stack.push(nb);
            }
        }
        for nb in graph.in_neighbors(n) {
            if reachable.insert(nb) {
                stack.push(nb);
            }
        }
    }
    let mut disconnected: Vec<NodeId> = graph
        .concepts()
        .map(|c| c.id)
        .filter(|id| !reachable.contains(id) && !protected.contains(id))
        .collect();
    disconnected.sort_by_key(|id| id.0);
    for id in &disconnected {
        if graph.remove_node(*id).is_ok() {
            outcome.concepts_collected.push(*id);
        }
    }
    outcome.concepts_collected.sort_by_key(|id| id.0);

    // Step 5 — survivors: every remaining concept gets gc_survived += 1.
    let mut survivors: Vec<NodeId> = graph.concepts().map(|c| c.id).collect();
    survivors.sort_by_key(|id| id.0);
    graph.bump_gc_survived(&survivors);
    outcome.survivors = survivors;

    // Step 6 — canonical budget: record only; T6.4 demotes (never here).
    outcome.canonical_count = graph
        .concepts()
        .filter(|c| c.canonization_status == CanonizationStatus::Canonical)
        .count();
    outcome.canonical_over_budget = outcome.canonical_count > params.max_canonical_nodes;
    if outcome.canonical_over_budget {
        outcome.warnings.push(format!(
            "canonical concept count {} exceeds max_canonical_nodes {} — \
             T6.4 demotion sweep must act (GC does not demote)",
            outcome.canonical_count, params.max_canonical_nodes
        ));
    }

    // Advisory capacity warning (spec §9: elastic, never evict).
    let concept_count = graph.concepts().count();
    if concept_count > params.max_concept_nodes {
        outcome.warnings.push(format!(
            "concept count {concept_count} exceeds advisory max_concept_nodes {} — \
             capacity is elastic; nothing evicted",
            params.max_concept_nodes
        ));
    }

    // Step 7 — the epoch is bumped by the mutations above (see module docs).
    outcome.epoch_after = graph.epoch();
    outcome
}

/// T2.6 hook — mirror a GC run into the owner's inverted index.
///
/// The index is owner-side (P3 contract, `src/graph/mod.rs`), so `run` cannot
/// maintain it directly. The owner MUST call this after `run`: every collected
/// concept is dropped from the index ([`InvertedIndex::remove`]); survivors'
/// content never changes, so no re-`add` is required.
pub fn sync_index(outcome: &GcOutcome, index: &mut InvertedIndex) {
    for id in &outcome.concepts_collected {
        index.remove(*id);
    }
}

/// A concept is protected when it is Venerable or Canonical, or it is the
/// session's root-goal node (spec §9 step 2's exclusion list).
fn is_protected(graph: &Graph, c: &Concept) -> bool {
    matches!(
        c.canonization_status,
        CanonizationStatus::Venerable | CanonizationStatus::Canonical
    ) || is_root_goal_concept(graph, c)
}

/// The root-goal concept is the one whose content (or canonical key) equals
/// the session's `root_goal`. The fixture carries `root_goal: "launch the
/// product"` matching concept content; T4.4 additionally marks it Venerable
/// via `set_root_goal` — this is belt-and-suspenders per spec §9 step 2's
/// "excluding Venerable/Canonical/root-goal".
fn is_root_goal_concept(graph: &Graph, c: &Concept) -> bool {
    let Some(goal) = graph.root_goal() else {
        return false;
    };
    match goal {
        serde_json::Value::String(s) => c.content == *s || c.canonical_key == *s,
        serde_json::Value::Object(map) => {
            let content = map.get("content").and_then(|v| v.as_str());
            let key = map.get("key").and_then(|v| v.as_str());
            content.is_some_and(|s| c.content == s)
                || key.is_some_and(|s| c.content == s || c.canonical_key == s)
        }
        _ => false,
    }
}

/// Edge ids qualifying for step-1 removal, id-ascending and deduplicated.
fn dead_edge_ids(graph: &Graph, params: GcParams) -> Vec<NodeId> {
    const ALL_TYPES: [EdgeType; 7] = [
        EdgeType::Temporal,
        EdgeType::Derives,
        EdgeType::CoOccurrence,
        EdgeType::Causal,
        EdgeType::Dependency,
        EdgeType::Hierarchical,
        EdgeType::Semantic,
    ];
    let mut dead: Vec<NodeId> = Vec::new();
    for node in graph
        .interactions()
        .map(|i| i.id)
        .chain(graph.concepts().map(|c| c.id))
    {
        for ty in ALL_TYPES {
            for tgt in graph.out_neighbors_typed(node, ty) {
                if let Some(e) = graph.edge_between(node, tgt, ty) {
                    if e.weight < params.min_edge_weight
                        && params.now.signed_duration_since(e.last_reinforced) > params.gc_edge_ttl
                    {
                        dead.push(e.id);
                    }
                }
            }
        }
    }
    dead.sort_by_key(|id| id.0);
    dead.dedup();
    dead
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Graph;
    use crate::types::{
        AgentId, CanonizationEvent, ConceptType, Edge, Interaction, Node, SessionId,
    };
    use chrono::TimeZone;
    use uuid::Uuid;

    fn ts(m: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000 + m * 60, 0).unwrap()
    }

    fn sid() -> SessionId {
        SessionId::from("t4.5-gc")
    }

    fn nid(n: u64) -> NodeId {
        NodeId(Uuid::from_u64_pair(0, n))
    }

    /// Deterministic clock for GC runs: every `ts(0)` write is 100 min old —
    /// past the 1h TTL but heavy edges (>= 0.5) always survive step 1.
    fn default_params() -> GcParams {
        GcParams {
            now: ts(100),
            ..Default::default()
        }
    }

    fn interaction(id: u64, prev: Option<u64>) -> Interaction {
        Interaction {
            id: nid(id),
            session_id: sid(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: Some("p".into()),
            previous_id: prev.map(nid),
            created_at: ts(0),
        }
    }

    fn concept(id: u64, origin: u64, content: &str, ty: ConceptType) -> Concept {
        Concept {
            id: nid(id),
            session_id: sid(),
            content: content.into(),
            canonical_key: content.to_string(),
            concept_type: ty,
            origin_interaction: nid(origin),
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

    fn edge(id: u64, src: u64, tgt: u64, ty: EdgeType, weight: f64, at_min: i64) -> Edge {
        Edge {
            id: nid(id),
            session_id: sid(),
            source: nid(src),
            target: nid(tgt),
            edge_type: ty,
            weight,
            reinforcements: 1,
            created_at: ts(at_min),
            last_reinforced: ts(at_min),
        }
    }

    fn transition(
        ev_id: u64,
        node: u64,
        from: CanonizationStatus,
        to: CanonizationStatus,
    ) -> CanonizationEvent {
        CanonizationEvent {
            id: nid(ev_id),
            session_id: sid(),
            node_id: nid(node),
            from_status: from,
            to_status: to,
            blast_radius: None,
            last_demotion_time: None,
            occurred_at: ts(1),
        }
    }

    /// Insert `c` then strip its `Derives` edge, leaving an isolated concept
    /// (orphan / disconnected-component material).
    fn insert_isolated(g: &mut Graph, c: Concept, origin: u64) -> NodeId {
        let id = c.id;
        let oid = nid(origin);
        g.insert_concept(c, oid).unwrap();
        let derives_id = g
            .edge_between(oid, id, EdgeType::Derives)
            .expect("insert_concept creates a Derives edge")
            .id;
        g.remove_edge(derives_id).unwrap();
        id
    }

    // ------------------------------------------------------------------
    // Step 1 — edge cleanup
    // ------------------------------------------------------------------

    #[test]
    fn edge_cleanup_removes_stale_low_weight_edges_only() {
        let mut g = Graph::new(sid());
        let i1 = interaction(1, None);
        let iid = i1.id;
        g.insert_interaction(i1).unwrap();
        g.insert_concept(concept(10, 1, "kept concept", ConceptType::Entity), iid)
            .unwrap();
        g.insert_concept(concept(11, 1, "decay concept", ConceptType::Entity), iid)
            .unwrap();

        // Stale (ts 0, past the 1h TTL) AND below min weight -> removed.
        g.upsert_edge(edge(100, 10, 11, EdgeType::CoOccurrence, 0.1, 0))
            .unwrap();
        // Fresh (ts 99, inside the TTL) though below weight -> kept.
        g.upsert_edge(edge(101, 11, 10, EdgeType::CoOccurrence, 0.1, 99))
            .unwrap();
        // Stale but heavy -> kept.
        g.upsert_edge(edge(102, 10, 11, EdgeType::Semantic, 0.9, 0))
            .unwrap();

        let outcome = run(&mut g, default_params());
        assert_eq!(outcome.edges_removed, vec![nid(100)]);
        assert!(g.edge(nid(100)).is_none());
        assert!(g.edge(nid(101)).is_some(), "fresh edge must survive");
        assert!(g.edge(nid(102)).is_some(), "heavy edge must survive");
        // Concepts stay connected through the surviving edges.
        assert!(outcome.concepts_collected.is_empty());
        assert_eq!(outcome.survivors, vec![nid(10), nid(11)]);
        assert!(outcome.epoch_after > outcome.epoch_before);
    }

    // ------------------------------------------------------------------
    // Step 2 — orphan / sub-threshold concept cleanup
    // ------------------------------------------------------------------

    #[test]
    fn orphan_and_subthreshold_collected_protected_survive() {
        let mut g = Graph::new(sid());
        let i1 = interaction(1, None);
        // Second interaction 100 min later gives the session a temporal span,
        // so concepts written at ts(0) have zero recency.
        let i2 = Interaction {
            created_at: ts(100),
            ..interaction(2, Some(1))
        };
        let iid = i1.id;
        g.insert_interaction(i1).unwrap();
        g.insert_interaction(i2).unwrap();

        // Orphan: no edges at all -> step 2, not protected.
        let orphan = insert_isolated(&mut g, concept(10, 1, "orphan", ConceptType::Entity), 1);
        // Sub-threshold (and not an orphan): an Observation with only its
        // Derives edge — zero recency/frequency, half session_activity, 2/3
        // density, Observation modifier -> composite below 0.3.
        let low_id = nid(11);
        g.insert_concept(concept(11, 1, "low value", ConceptType::Observation), iid)
            .unwrap();
        // Protected: Venerable, isolated -> must survive both step 2 and 3.
        let protected = insert_isolated(
            &mut g,
            concept(12, 1, "venerable island", ConceptType::Entity),
            1,
        );
        g.apply_canonization_transition(transition(
            50,
            12,
            CanonizationStatus::None,
            CanonizationStatus::Venerable,
        ))
        .unwrap();
        // Anchors that raise the density baseline (max incident = 3) so the
        // Observation's density is 2/3, not 1.0. They must survive.
        g.insert_concept(concept(13, 1, "anchor a", ConceptType::Entity), iid)
            .unwrap();
        g.insert_concept(concept(14, 1, "anchor b", ConceptType::Entity), iid)
            .unwrap();
        g.upsert_edge(edge(100, 13, 11, EdgeType::Dependency, 1.0, 0))
            .unwrap();
        g.upsert_edge(edge(101, 13, 14, EdgeType::Dependency, 1.0, 0))
            .unwrap();

        // Sanity: the Observation really is sub-threshold.
        let scores = crate::daemon::score::rescore(&g, &ScoringWeights::default());
        let low_score = scores.iter().find(|s| s.item == low_id).unwrap().score;
        assert!(low_score < 0.3, "test premise: got {low_score}");

        let outcome = run(&mut g, default_params());
        assert_eq!(outcome.concepts_collected, vec![nid(10), nid(11)]);
        assert!(g.node(orphan).is_none());
        assert!(g.node(low_id).is_none());
        assert!(g.node(protected).is_some(), "protected class must survive");
        assert!(g.node(nid(13)).is_some() && g.node(nid(14)).is_some());
        // The Venerable island is a survivor and its counter increments.
        assert!(outcome.survivors.contains(&protected));
        let p = match g.node(protected).unwrap() {
            Node::Concept(c) => c,
            _ => unreachable!(),
        };
        assert_eq!(p.gc_survived, 1);
        assert_eq!(p.canonization_status, CanonizationStatus::Venerable);
    }

    // ------------------------------------------------------------------
    // Step 3 — disconnected-component cleanup
    // ------------------------------------------------------------------

    #[test]
    fn protected_classes_survive_disconnected_component() {
        let mut g = Graph::new(sid());
        let i1 = interaction(1, None);
        let i2 = interaction(2, Some(1));
        let iid = i1.id;
        g.insert_interaction(i1).unwrap();
        g.insert_interaction(i2).unwrap();
        // Anchored concept keeps the main component reachable.
        g.insert_concept(concept(10, 1, "anchored", ConceptType::Entity), iid)
            .unwrap();

        // Two isolated islands, protected by status, with no path to the chain.
        let venerable = insert_isolated(
            &mut g,
            concept(20, 1, "venerable island", ConceptType::Entity),
            1,
        );
        let canonical = insert_isolated(
            &mut g,
            concept(21, 1, "canonical island", ConceptType::Entity),
            1,
        );
        g.apply_canonization_transition(transition(
            50,
            20,
            CanonizationStatus::None,
            CanonizationStatus::Venerable,
        ))
        .unwrap();
        g.apply_canonization_transition(transition(
            51,
            21,
            CanonizationStatus::None,
            CanonizationStatus::Venerable,
        ))
        .unwrap();
        g.apply_canonization_transition(transition(
            52,
            21,
            CanonizationStatus::Venerable,
            CanonizationStatus::Canonical,
        ))
        .unwrap();

        let outcome = run(&mut g, default_params());
        assert!(
            outcome.concepts_collected.is_empty(),
            "only unprotected concepts may be collected"
        );
        assert!(g.node(venerable).is_some());
        assert!(g.node(canonical).is_some());
        assert_eq!(outcome.survivors.len(), 3, "anchored + both islands");
        // The islands' counters increment even though they are protected.
        for id in [venerable, canonical] {
            let c = match g.node(id).unwrap() {
                Node::Concept(c) => c,
                _ => unreachable!(),
            };
            assert_eq!(c.gc_survived, 1);
        }
    }

    #[test]
    fn disconnected_bfs_is_cycle_safe() {
        // A Hierarchical cycle in a component with no path to the temporal
        // chain (G6: multi-hop Hierarchical cycles are writable; the BFS must
        // terminate and collect the whole component).
        let mut g = Graph::new(sid());
        let i1 = interaction(1, None);
        let iid = i1.id;
        g.insert_interaction(i1).unwrap();
        g.insert_concept(concept(10, 1, "main", ConceptType::Entity), iid)
            .unwrap();
        insert_isolated(&mut g, concept(20, 1, "cycle x", ConceptType::Entity), 1);
        insert_isolated(&mut g, concept(21, 1, "cycle y", ConceptType::Entity), 1);
        insert_isolated(&mut g, concept(22, 1, "cycle z", ConceptType::Entity), 1);
        g.upsert_edge(edge(100, 20, 21, EdgeType::Hierarchical, 1.0, 0))
            .unwrap();
        g.upsert_edge(edge(101, 21, 22, EdgeType::Hierarchical, 1.0, 0))
            .unwrap();
        g.upsert_edge(edge(102, 22, 20, EdgeType::Hierarchical, 1.0, 0))
            .unwrap();

        let outcome = run(&mut g, default_params());
        assert_eq!(outcome.concepts_collected, vec![nid(20), nid(21), nid(22)]);
        assert!(g.node(nid(10)).is_some(), "anchored concept survives");
        // The cycle component is gone, so the graph is well-formed again.
        g.assert_invariants().unwrap();
    }

    // ------------------------------------------------------------------
    // Steps 5–7 — survivors, canonical budget, epoch
    // ------------------------------------------------------------------

    #[test]
    fn empty_graph_run_is_safe() {
        let mut g = Graph::new(sid());
        let outcome = run(&mut g, default_params());
        assert!(outcome.concepts_collected.is_empty());
        assert!(outcome.edges_removed.is_empty());
        assert!(outcome.survivors.is_empty());
        assert!(!outcome.canonical_over_budget);
        // Nothing to bump: a concept-free session has no survivors (see docs).
        assert_eq!(outcome.epoch_after, outcome.epoch_before);
    }

    #[test]
    fn canonical_budget_records_over_budget_without_demotion() {
        let mut g = Graph::new(sid());
        let i1 = interaction(1, None);
        let iid = i1.id;
        g.insert_interaction(i1).unwrap();
        for id in [10u64, 11] {
            g.insert_concept(
                concept(id, 1, &format!("canon {id}"), ConceptType::Entity),
                iid,
            )
            .unwrap();
            g.apply_canonization_transition(transition(
                id,
                id,
                CanonizationStatus::None,
                CanonizationStatus::Venerable,
            ))
            .unwrap();
            g.apply_canonization_transition(transition(
                id + 100,
                id,
                CanonizationStatus::Venerable,
                CanonizationStatus::Canonical,
            ))
            .unwrap();
        }

        let outcome = run(
            &mut g,
            GcParams {
                max_canonical_nodes: 1,
                ..default_params()
            },
        );
        assert_eq!(outcome.canonical_count, 2);
        assert!(outcome.canonical_over_budget);
        assert_eq!(outcome.max_canonical_nodes, 1);
        assert!(
            outcome.warnings.iter().any(|w| w.contains("T6.4")),
            "over-budget must be recorded for T6.4"
        );
        // No demotion happened here (T6.4's job): both remain Canonical.
        for id in [10u64, 11] {
            let c = match g.node(nid(id)).unwrap() {
                Node::Concept(c) => c,
                _ => unreachable!(),
            };
            assert_eq!(c.canonization_status, CanonizationStatus::Canonical);
        }
    }

    #[test]
    fn max_concept_nodes_warns_without_evicting() {
        let mut g = Graph::new(sid());
        let i1 = interaction(1, None);
        let iid = i1.id;
        g.insert_interaction(i1).unwrap();
        g.insert_concept(concept(10, 1, "concept a", ConceptType::Entity), iid)
            .unwrap();
        g.insert_concept(concept(11, 1, "concept b", ConceptType::Entity), iid)
            .unwrap();

        let outcome = run(
            &mut g,
            GcParams {
                max_concept_nodes: 1,
                ..default_params()
            },
        );
        assert!(outcome
            .warnings
            .iter()
            .any(|w| w.contains("max_concept_nodes")));
        assert!(
            outcome.concepts_collected.is_empty(),
            "advisory capacity must never evict"
        );
        assert!(g.node(nid(10)).is_some() && g.node(nid(11)).is_some());
    }

    // ------------------------------------------------------------------
    // Step 4 — T2.6 index maintenance hook
    // ------------------------------------------------------------------

    #[cfg(feature = "fixtures")]
    #[test]
    fn index_sync_removes_collected_concepts() {
        let snap = crate::fixtures::load_snapshot("session-drift").unwrap();
        let mut index = InvertedIndex::from_snapshot(&snap);
        let isolated = NodeId(Uuid::parse_str("f0000000-0000-4000-8000-000000005020").unwrap());
        assert!(
            index
                .search("widget", 10)
                .iter()
                .any(|s| s.item == isolated),
            "test premise: isolated widget is indexed"
        );

        let mut g = Graph::from_snapshot(snap).unwrap();
        for eid in [
            "f0000000-0000-4000-8000-000000014009",
            "f0000000-0000-4000-8000-000000014010",
        ] {
            g.remove_edge(NodeId(Uuid::parse_str(eid).unwrap()))
                .unwrap();
        }
        let outcome = run(&mut g, default_params());
        sync_index(&outcome, &mut index);
        assert!(
            !index
                .search("widget", 10)
                .iter()
                .any(|s| s.item == isolated),
            "collected concept must leave the inverted index"
        );
    }

    // ------------------------------------------------------------------
    // Fixture — session-drift planted disconnected component (done-when)
    // ------------------------------------------------------------------

    #[cfg(feature = "fixtures")]
    #[test]
    fn session_drift_disconnected_component_collected() {
        let snap = crate::fixtures::load_snapshot("session-drift").unwrap();
        let mut g = Graph::from_snapshot(snap).unwrap();

        // Materialize the planted disconnection (see module docs): the
        // "isolated" pair's only link to the temporal chain is its two
        // Derives provenance edges — drop them (TEST-ONLY state
        // reconstruction; step 1's predicate never touches 0.9-weight
        // edges), so step 3 sees the component the generator planted.
        for eid in [
            "f0000000-0000-4000-8000-000000014009",
            "f0000000-0000-4000-8000-000000014010",
        ] {
            g.remove_edge(NodeId(Uuid::parse_str(eid).unwrap()))
                .unwrap();
        }
        let isolated_widget =
            NodeId(Uuid::parse_str("f0000000-0000-4000-8000-000000005020").unwrap());
        let isolated_sibling =
            NodeId(Uuid::parse_str("f0000000-0000-4000-8000-000000005021").unwrap());
        let goal = NodeId(Uuid::parse_str("f0000000-0000-4000-8000-000000005010").unwrap());
        let step_one = NodeId(Uuid::parse_str("f0000000-0000-4000-8000-000000005011").unwrap());

        let now = Utc
            .with_ymd_and_hms(2026, 8, 12, 0, 0, 0)
            .single()
            .expect("valid timestamp");
        let epoch_before = g.epoch();
        let outcome = run(
            &mut g,
            GcParams {
                now,
                ..Default::default()
            },
        );

        // Step 3 collected exactly the planted disconnected component.
        assert_eq!(
            outcome.concepts_collected,
            vec![isolated_widget, isolated_sibling]
        );
        assert!(
            outcome.edges_removed.is_empty(),
            "fixture edges are all above min_edge_weight"
        );

        // Protected classes survive: the Venerable root goal is still present
        // and its counter incremented 5 -> 6 (step 5).
        let goal_c = match g.node(goal).unwrap() {
            Node::Concept(c) => c,
            _ => unreachable!(),
        };
        assert_eq!(goal_c.canonization_status, CanonizationStatus::Venerable);
        assert_eq!(goal_c.gc_survived, 6);
        // A path concept also survives and increments 2 -> 3.
        let step_c = match g.node(step_one).unwrap() {
            Node::Concept(c) => c,
            _ => unreachable!(),
        };
        assert_eq!(step_c.gc_survived, 3);

        // The planted component is gone; every remaining concept is a survivor.
        assert!(g.node(isolated_widget).is_none());
        assert!(g.node(isolated_sibling).is_none());
        assert_eq!(outcome.survivors.len(), 7);
        assert!(outcome.survivors.contains(&goal));

        // Step 7: the epoch bumped (removals + survivor upserts all append
        // mutations).
        assert!(outcome.epoch_after > outcome.epoch_before);
        assert_eq!(outcome.epoch_after, g.epoch());
        assert_eq!(outcome.epoch_before, epoch_before);

        // The graph is well-formed again after cleanup.
        g.assert_invariants().unwrap();
    }
}
