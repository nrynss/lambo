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
//! 1. **Edge cleanup** — every **decaying** edge (spec §5 table:
//!    `CoOccurrence`, `Semantic` — [`EdgeType::decays`]) with
//!    `weight < min_edge_weight` whose last reinforcement is older than
//!    `gc_edge_ttl` is removed (ALGO-9). The TTL anchor is `last_reinforced`
//!    (the edge's last activity; a never-reinforced edge is dead from its
//!    write). Structural types are exempt: their weight is a property of the
//!    kind, not a decayed signal, and §5.7 depends on them surviving.
//! 2. **Concept cleanup** — orphans (no incident edges after step 1) and
//!    sub-threshold concepts are collected, **excluding** Venerable,
//!    Canonical, and root-goal concepts. The cut takes the **session's**
//!    [`ScoringWeights`] (ALGO-4 — GC must not rank eviction with weights
//!    nothing else uses), scores over the live dimensions while `access_count`
//!    is dead session-wide (ALGO-1), and compares against a per-type bar
//!    scaled by [`crate::types::ConceptType::eviction_resistance`] (ALGO-11).
//!    See [`MIN_CONCEPT_SCORE`] for the calibration and its evidence.
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
//!    the reason GC cannot be cut. **Chunked** (CONC-6/XP-10): one call applies
//!    at most [`GC_SURVIVOR_BUMP_CHUNK`] bumps and returns the rest in
//!    [`GcOutcome::survivors_pending`], which the owner drains with
//!    [`drain_survivor_bumps`] over later cycles — see that function for why
//!    the store still converges exactly.
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
use crate::types::{CanonizationStatus, Concept, ConceptType, EdgeType, NodeId};

/// Below this weight, an edge past its TTL is removed (step 1).
///
/// v0.6.0's value is not in-repo — v0.1 decision. Kept below the structural
/// write weights (`Derives` 0.9, `Temporal`/`Dependency`/`Causal` 1.0) so
/// provenance and load-bearing edges survive a default run.
pub const MIN_EDGE_WEIGHT: f64 = 0.5;
/// An edge untouched for this long is "past `gc_edge_ttl`" (step 1).
pub const GC_EDGE_TTL: ChronoDuration = ChronoDuration::seconds(3600);
/// Below this daemon composite score, a concept is sub-threshold (step 2).
/// v0.6.0's value is not in-repo — v0.1 decision, **recalibrated** (ALGO-1).
///
/// The threshold is not applied flat: the concept's eviction score is
/// [`crate::daemon::score::score_over_live_dimensions`] (frequency excluded
/// while `access_count` is dead session-wide) and the comparison is against
/// `MIN_CONCEPT_SCORE / ConceptType::eviction_resistance()` — the spec §5
/// resistances (Constraint 1.5 … Observation 0.7) scale the bar per type
/// instead of every type facing the identical cut (ALGO-11). Dividing the
/// threshold by the resistance is algebraically the same as multiplying the
/// score by it; the threshold form is used so the *bar* is what varies and the
/// score stays comparable to recall's.
///
/// ### Why 0.12
///
/// The original 0.3 was calibrated against nothing: with `access_count`
/// identically 0 (no write path feeds it until P5 recall) and `density`
/// max-normalized against the session hub, an ordinary well-connected concept
/// in the shipped `session-rest-api` fixture scores 0.13–0.34 — so 0.3
/// collected **15 of its 22 concepts on the first sweep**, including `auth
/// middleware`, which spec §13 step 1 names, and left 6 non-Canonical peers
/// where canonization Stage 1 requires 20. GC starved the pipeline it exists
/// to feed.
///
/// Against the live-dimension score the same fixture's floor is 0.149
/// (`user id`: zero recency, one Derives edge, minimum density); the effective
/// Entity bar is `0.12 / 1.2 = 0.10`, ~50% below that floor, so every healthy
/// mid-session concept survives with margin while the clause still bites where
/// it should: a concept whose recency **and** density have both decayed to ~0
/// scores at most its type modifier (Entity +0.05, Observation −0.10 → 0.0),
/// which is below every type's bar. Orphans and disconnected components are
/// collected by their own clauses regardless of score, so the score cut is
/// deliberately the conservative one while a fifth of the composite is dead.
pub const MIN_CONCEPT_SCORE: f64 = 0.12;
/// Advisory concept-count ceiling: warn above, never evict (spec §9).
pub const MAX_CONCEPT_NODES: usize = 10_000;

/// Step 5 bumps at most this many survivors per call; the rest come back as
/// [`GcOutcome::survivors_pending`] for the owner to drain over later cycles
/// with [`drain_survivor_bumps`] (CONC-6/XP-10).
///
/// Matches [`crate::config::Config::backend_flush_max_batch`] (500), so one
/// GC cycle enqueues at most one flush batch of survivor upserts instead of
/// twenty. An unchunked sweep at the advisory ceiling emitted 10,000
/// full-`Concept` clones — ~40MB of clone traffic once P7 embeddings land —
/// from inside the write guard, all in one burst.
pub const GC_SURVIVOR_BUMP_CHUNK: usize = 500;

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
    /// Step 2: daemon composite score below this is sub-threshold. Scaled per
    /// concept type by [`crate::types::ConceptType::eviction_resistance`] — see
    /// [`MIN_CONCEPT_SCORE`].
    pub min_concept_score: f64,
    /// Step 2: the **session's** scoring weights (ALGO-4). GC must rank
    /// eviction with the same function recall ranks retrieval with, or a
    /// concept can be evicted for being worthless under weights nothing else
    /// uses. P8 threads `Config::scoring` here via the daemon.
    pub weights: ScoringWeights,
    /// Advisory capacity ceiling — warn above, never evict.
    pub max_concept_nodes: usize,
    /// Step 6: Canonical budget ceiling; over it → `canonical_over_budget`.
    pub max_canonical_nodes: usize,
    /// Step 5: how many survivor bumps this call may apply
    /// ([`GC_SURVIVOR_BUMP_CHUNK`]). The remainder is returned in
    /// [`GcOutcome::survivors_pending`].
    pub max_survivor_bumps: usize,
}

impl Default for GcParams {
    fn default() -> Self {
        Self {
            now: Utc::now(),
            min_edge_weight: MIN_EDGE_WEIGHT,
            gc_edge_ttl: GC_EDGE_TTL,
            min_concept_score: MIN_CONCEPT_SCORE,
            weights: ScoringWeights::default(),
            max_concept_nodes: MAX_CONCEPT_NODES,
            max_canonical_nodes: 1000, // spec §10
            max_survivor_bumps: GC_SURVIVOR_BUMP_CHUNK,
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
    /// Step 5: every concept id that survived this run.
    pub survivors: Vec<NodeId>,
    /// Step 5: the tail of [`survivors`](GcOutcome::survivors) whose
    /// `gc_survived += 1` this run **deferred** — the owner must drain it with
    /// [`drain_survivor_bumps`] on later cycles (CONC-6/XP-10). Empty when the
    /// survivor set fit in one chunk.
    pub survivors_pending: Vec<NodeId>,
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
    // canonization statuses and the root goal do not change during a GC run.
    let goal_texts = graph.root_goal_texts();
    let protected: HashSet<NodeId> = graph
        .concepts()
        .filter(|c| is_protected(c, &goal_texts))
        .map(|c| c.id)
        .collect();

    // Step 1 — edge cleanup: below min weight AND past TTL.
    for eid in dead_edge_ids(graph, params) {
        if graph.remove_edge(eid).is_ok() {
            outcome.edges_removed.push(eid);
        }
    }

    // Step 2 — concept cleanup: orphans + sub-threshold, excluding protected.
    // Scored against post-step-1 state with the session's own weights (ALGO-4),
    // over the live dimensions only while `access_count` is dead session-wide
    // (ALGO-1), and cut per concept type (ALGO-11).
    let ctx = crate::daemon::score::SessionContext::compute(graph);
    let frequency_is_live = graph.concepts().any(|c| c.access_count > 0);
    let mut candidates: Vec<NodeId> = Vec::new();
    for c in graph.concepts() {
        if protected.contains(&c.id) {
            continue;
        }
        let orphan = graph.incident_edges(c.id).is_empty();
        let below = eviction_score(graph, c, &ctx, params, frequency_is_live)
            < eviction_threshold(params.min_concept_score, c.concept_type);
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

    // Step 5 — survivors: every remaining concept gets gc_survived += 1, but at
    // most `max_survivor_bumps` of them here; the tail is deferred to later
    // cycles (CONC-6/XP-10 — see `drain_survivor_bumps` for the convergence
    // argument).
    let mut survivors: Vec<NodeId> = graph.concepts().map(|c| c.id).collect();
    survivors.sort_by_key(|id| id.0);
    let split = survivors.len().min(params.max_survivor_bumps);
    graph.bump_gc_survived(&survivors[..split]);
    outcome.survivors_pending = survivors[split..].to_vec();
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

/// Apply up to `max` deferred survivor bumps, removing them from `pending`
/// (CONC-6/XP-10). The owner calls this each cycle until `pending` is empty,
/// and must not start another [`run`] before then.
///
/// ## Why the store still converges exactly
///
/// Chunking changes *when* each `UpsertNode` is emitted, never *which*. Every
/// survivor of a run receives exactly one `gc_survived += 1` and exactly one
/// `UpsertNode` carrying the post-increment concept, whether it lands in the GC
/// cycle or a later one — the emitted multiset is byte-identical to the
/// unchunked sweep, so the store's converged state is identical.
///
/// Two edge cases, both benign:
///
/// * **A pending concept is collected before its bump lands.** It is skipped
///   (`bump_gc_survived` ignores absent ids), and the store already has the
///   `DeleteNode`. Under the unchunked sweep the row would have been upserted
///   and then deleted — same final state, one fewer write.
/// * **Interleaving with another run** cannot happen: the owner drains
///   `pending` to empty before the next [`run`], so a concept never carries two
///   outstanding bumps and can never be double-counted or skipped.
///
/// Returns the number of bumps applied.
pub fn drain_survivor_bumps(graph: &mut Graph, pending: &mut Vec<NodeId>, max: usize) -> usize {
    let take = pending.len().min(max.max(1));
    let chunk: Vec<NodeId> = pending.drain(..take).collect();
    graph.bump_gc_survived(&chunk)
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

/// The step-2 bar for one concept type: [`MIN_CONCEPT_SCORE`] divided by the
/// spec §5 [`ConceptType::eviction_resistance`] (ALGO-11).
///
/// A Constraint (1.5) faces a bar a third lower than a Resource (1.0); an
/// Observation (0.7) faces one ~43% higher. A non-positive or non-finite
/// resistance would invert or poison the comparison, so it falls back to the
/// unscaled threshold (the `const fn` cannot produce one today — this is a
/// guard against a future table edit, not a live branch).
fn eviction_threshold(min_concept_score: f64, ty: ConceptType) -> f64 {
    let resistance = ty.eviction_resistance();
    if resistance.is_finite() && resistance > 0.0 {
        min_concept_score / resistance
    } else {
        min_concept_score
    }
}

/// One concept's step-2 eviction score.
///
/// `frequency_is_live` is computed once per run over the whole session: while
/// **no** concept has been accessed, the frequency dimension is structurally
/// dead and is renormalized out of the cut (ALGO-1,
/// [`crate::daemon::score::score_over_live_dimensions`]). The moment any
/// access lands the full spec §9 composite is used again — so the session
/// switches scoring functions exactly once, and only in the direction that
/// raises scores.
fn eviction_score(
    graph: &Graph,
    c: &Concept,
    ctx: &crate::daemon::score::SessionContext,
    params: GcParams,
    frequency_is_live: bool,
) -> f64 {
    let dims = crate::daemon::score::score_concept(graph, c, ctx);
    if frequency_is_live {
        crate::daemon::score::score(dims, &params.weights)
    } else {
        crate::daemon::score::score_over_live_dimensions(dims, &params.weights)
    }
}

/// A concept is protected when it is Venerable or Canonical, or it is one of
/// the session's root-goal nodes (spec §9 step 2's exclusion list).
///
/// `goal_texts` is [`Graph::root_goal_texts`], resolved **once per run** by the
/// caller: the goal cannot change mid-run, and re-parsing it per concept would
/// allocate once per concept.
fn is_protected(c: &Concept, goal_texts: &[String]) -> bool {
    matches!(
        c.canonization_status,
        CanonizationStatus::Venerable | CanonizationStatus::Canonical
    ) || is_root_goal_concept(c, goal_texts)
}

/// A root-goal concept is one whose content (or canonical key) is named by the
/// session's `root_goal`. The fixture carries `root_goal: "launch the
/// product"` matching concept content; T4.4 additionally marks it Venerable
/// via `set_root_goal` — this is belt-and-suspenders per spec §9 step 2's
/// "excluding Venerable/Canonical/root-goal".
///
/// The goal shape is read by [`Graph::root_goal_texts`] (ALGO-6), the one
/// parser GC, drift and `set_root_goal` share — so an **array** goal (spec
/// §6.1's own example) protects all of its concepts here instead of none.
fn is_root_goal_concept(c: &Concept, goal_texts: &[String]) -> bool {
    goal_texts
        .iter()
        .any(|t| c.content == *t || c.canonical_key == *t)
}

/// Edge ids qualifying for step-1 removal, id-ascending and deduplicated.
///
/// **Only decaying edge types** (ALGO-9): the spec §5 table marks `CoOccurrence`
/// and `Semantic` as decaying and every other type as not, so a weight-and-TTL
/// cut is only meaningful for those two — a structural edge's weight is a fixed
/// property of its kind, not a decayed signal, and collecting one on a protected
/// concept would break §5.7's structural guarantees. The margin today is
/// **zero**: `record_action` writes `Causal`/`Dependency` at exactly
/// [`MIN_EDGE_WEIGHT`] and the predicate is a strict `<`, so any weight tweak or
/// a lower configured `min_edge_weight` would start deleting the demo's
/// dependency graph out from under it. [`EdgeType::decays`] is the single source
/// of truth for the table.
fn dead_edge_ids(graph: &Graph, params: GcParams) -> Vec<NodeId> {
    const DECAYING_TYPES: [EdgeType; 2] = [EdgeType::CoOccurrence, EdgeType::Semantic];
    debug_assert!(
        DECAYING_TYPES.iter().all(|t| t.decays()),
        "DECAYING_TYPES must mirror EdgeType::decays (spec §5 table)"
    );
    let mut dead: Vec<NodeId> = Vec::new();
    for node in graph
        .interactions()
        .map(|i| i.id)
        .chain(graph.concepts().map(|c| c.id))
    {
        for ty in DECAYING_TYPES {
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
        AgentId, CanonizationEvent, ConceptType, Edge, Interaction, Mutation, Node, SessionId,
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

    /// ALGO-9: step 1 only cuts **decaying** edge types (spec §5 table). A
    /// structural edge that is stale and under the weight bar must survive —
    /// its weight is a property of its kind, not a decayed signal, and §5.7
    /// depends on it.
    ///
    /// The margin is zero today: `record_action` writes `Causal`/`Dependency` at
    /// exactly `MIN_EDGE_WEIGHT` against a strict `<`, so this test uses a
    /// sub-threshold structural weight — which the pre-fix pass collected — plus
    /// a decaying edge at the same weight to prove the cut still bites.
    #[test]
    fn edge_cleanup_spares_non_decaying_edge_types() {
        let mut g = Graph::new(sid());
        let i1 = interaction(1, None);
        let iid = i1.id;
        g.insert_interaction(i1).unwrap();
        // Protected (Canonical) endpoints, so nothing is collected for score.
        for n in [10u64, 11] {
            let c = Concept {
                canonization_status: CanonizationStatus::Canonical,
                ..concept(n, 1, &format!("structural {n}"), ConceptType::Entity)
            };
            g.insert_concept(c, iid).unwrap();
        }
        // Stale + under weight, but structural: must survive (ALGO-9).
        for (id, ty) in [
            (200u64, EdgeType::Dependency),
            (201, EdgeType::Causal),
            (202, EdgeType::Hierarchical),
        ] {
            g.upsert_edge(edge(id, 10, 11, ty, 0.4, 0)).unwrap();
        }
        // Same weight and age, decaying type: must go.
        g.upsert_edge(edge(203, 10, 11, EdgeType::CoOccurrence, 0.4, 0))
            .unwrap();

        let outcome = run(&mut g, default_params());
        assert_eq!(
            outcome.edges_removed,
            vec![nid(203)],
            "only the decaying edge is collected"
        );
        for id in [200u64, 201, 202] {
            assert!(
                g.edge(nid(id)).is_some(),
                "structural edge {id} must survive step 1 (spec §5 table)"
            );
        }
        // And the Derives provenance edges — also non-decaying — are intact.
        assert!(g.edge_between(iid, nid(10), EdgeType::Derives).is_some());
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
        // Sub-threshold (and not an orphan): an Observation whose only edge is
        // its Derives provenance — zero recency, minimum density (1 of the
        // hub's 4), and the Observation modifier (−0.10) against the highest
        // per-type bar in the table (resistance 0.7 ⇒ 0.12/0.7 = 0.171).
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
        // A hub (13) plus three spokes: max incident = 4, so the Observation's
        // density is 1/4. All four must survive.
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

        // Sanity: the Observation really is sub-threshold under the cut GC
        // applies — live-dimension score vs. its own type's bar (ALGO-1/11).
        let params = default_params();
        let ctx = crate::daemon::score::SessionContext::compute(&g);
        let low = match g.node(low_id).unwrap() {
            Node::Concept(c) => c,
            _ => unreachable!(),
        };
        let low_score = eviction_score(&g, low, &ctx, params, false);
        let low_bar = eviction_threshold(params.min_concept_score, ConceptType::Observation);
        assert!(
            low_score < low_bar,
            "test premise: score {low_score} must be under the Observation bar {low_bar}"
        );

        let outcome = run(&mut g, params);
        assert_eq!(outcome.concepts_collected, vec![nid(10), nid(11)]);
        assert!(g.node(orphan).is_none());
        assert!(g.node(low_id).is_none());
        assert!(g.node(protected).is_some(), "protected class must survive");
        for id in [13u64, 14, 15, 16] {
            assert!(g.node(nid(id)).is_some(), "anchor {id} must survive");
        }
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
    // Step 2 — calibration (ALGO-1 / ALGO-4 / ALGO-11)
    // ------------------------------------------------------------------

    /// ALGO-1: the shipped demo session must survive a sweep.
    ///
    /// Pre-fix (flat `MIN_CONCEPT_SCORE = 0.3` against the full spec §9
    /// composite) this collected **15 of the 22** concepts on the first run —
    /// `auth middleware` (spec §13 step 1) among them — leaving 6 non-Canonical
    /// peers where canonization Stage 1 needs 20, i.e. GC starved the pipeline
    /// it exists to feed.
    #[cfg(feature = "fixtures")]
    #[test]
    fn rest_api_demo_session_survives_a_gc_sweep() {
        let snap = crate::fixtures::load_snapshot("session-rest-api").unwrap();
        let mut g = Graph::from_snapshot(snap).unwrap();
        assert_eq!(g.concepts().count(), 22, "fixture premise");

        // Every fixture edge is >= 0.6 (> MIN_EDGE_WEIGHT), so step 1 cannot
        // fire and `now` only has to be past the TTL to prove that.
        let outcome = run(
            &mut g,
            GcParams {
                now: Utc
                    .with_ymd_and_hms(2026, 8, 12, 0, 0, 0)
                    .single()
                    .expect("valid timestamp"),
                ..Default::default()
            },
        );
        assert!(
            outcome.edges_removed.is_empty(),
            "no fixture edge is below min_edge_weight"
        );
        assert!(
            outcome.concepts_collected.is_empty(),
            "a healthy session must survive a sweep intact, collected: {:?}",
            outcome
                .concepts_collected
                .iter()
                .map(|id| match g.node(*id) {
                    Some(Node::Concept(c)) => c.content.clone(),
                    _ => id.to_string(),
                })
                .collect::<Vec<_>>()
        );

        // The concepts spec §13 names by content (`session store` has no
        // counterpart in this fixture) plus the planted conflict node.
        let surviving: Vec<&str> = g.concepts().map(|c| c.content.as_str()).collect();
        for named in ["user schema", "auth middleware", "caching layer"] {
            assert!(
                surviving.contains(&named),
                "spec §13 names {named}; it must survive GC"
            );
        }

        // Canonization Stage 1 needs >= 20 non-Canonical peers in the session.
        let peers = g
            .concepts()
            .filter(|c| c.canonization_status != CanonizationStatus::Canonical)
            .count();
        assert!(
            peers >= 20,
            "Stage 1 needs >= 20 non-Canonical peers, found {peers}"
        );
    }

    /// ALGO-11: the cut consults [`ConceptType::eviction_resistance`].
    ///
    /// `Entity` and `Logic` share a `score_multiplier` (1.05), so two
    /// structurally identical concepts of those types score **identically**;
    /// their resistances differ (1.2 vs 1.1). Choosing
    /// `min_concept_score = 1.15 · score` puts the Entity bar (score/1.2·1.15 =
    /// 0.958·score) below the score and the Logic bar (1.045·score) above it,
    /// so the resistance factor is the *only* thing deciding their fate. Under
    /// the pre-fix flat cut both shared one bar and one outcome.
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
        g.insert_concept(concept(12, 1, "logic leaf", ConceptType::Logic), iid)
            .unwrap();
        g.upsert_edge(edge(100, 11, 10, EdgeType::Dependency, 1.0, 0))
            .unwrap();
        g.upsert_edge(edge(101, 12, 10, EdgeType::Dependency, 1.0, 0))
            .unwrap();

        // The two leaves score identically — same structure, same modifier.
        let ctx = crate::daemon::score::SessionContext::compute(&g);
        let base = default_params();
        let score_of = |g: &Graph, id: NodeId, ctx: &crate::daemon::score::SessionContext| {
            let c = match g.node(id).unwrap() {
                Node::Concept(c) => c.clone(),
                _ => unreachable!(),
            };
            eviction_score(g, &c, ctx, base, false)
        };
        let entity_score = score_of(&g, nid(11), &ctx);
        let logic_score = score_of(&g, nid(12), &ctx);
        assert_eq!(
            entity_score, logic_score,
            "test premise: Entity and Logic share score_multiplier"
        );

        let params = GcParams {
            min_concept_score: entity_score * 1.15,
            ..base
        };
        assert!(
            eviction_threshold(params.min_concept_score, ConceptType::Entity) < entity_score,
            "Entity bar must sit below the shared score"
        );
        assert!(
            eviction_threshold(params.min_concept_score, ConceptType::Logic) > logic_score,
            "Logic bar must sit above the shared score"
        );

        let outcome = run(&mut g, params);
        assert_eq!(
            outcome.concepts_collected,
            vec![nid(12)],
            "only the less resistant type is collected"
        );
        assert!(g.node(nid(11)).is_some(), "Entity (1.2) resists the cut");
        assert!(g.node(nid(12)).is_none(), "Logic (1.1) does not");
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
        let outcome = run(&mut g, default_params());
        assert!(
            outcome.concepts_collected.is_empty(),
            "under the session's default weights the leaf is worth keeping"
        );

        // Session weights that value only recency, which the leaf has none of.
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
                ..default_params()
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
        // An Observation with only its Derives edge in a session with a hub —
        // dead under any sane weighting.
        g.insert_concept(concept(11, 1, "low value", ConceptType::Observation), iid)
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
                ..default_params()
            },
        );
        assert!(
            outcome.concepts_collected.contains(&nid(11)),
            "garbage weights must degrade to zeroed dimensions, not a NaN score \
             that silently disables collection"
        );
    }

    // ------------------------------------------------------------------
    // Step 3 — disconnected-component cleanup
    // ------------------------------------------------------------------

    /// ALGO-6: an **array** root goal must protect every concept it names.
    ///
    /// GC's exclusion list read only the string and object shapes, so an array
    /// goal — spec §6.1's own example — protected nothing: the session's goal
    /// concepts became ordinary GC candidates. Isolated (step-3) goal concepts
    /// make the exclusion the only thing keeping them alive.
    #[test]
    fn array_root_goal_protects_every_named_concept() {
        let mut g = Graph::new(sid());
        let i1 = interaction(1, None);
        let iid = i1.id;
        g.insert_interaction(i1).unwrap();
        g.insert_concept(concept(10, 1, "anchored", ConceptType::Entity), iid)
            .unwrap();
        // Both goal concepts are isolated: only the root-goal exclusion can
        // save them from step 3.
        let goal_a = insert_isolated(
            &mut g,
            concept(20, 1, "launch the product", ConceptType::Entity),
            1,
        );
        let goal_b = insert_isolated(
            &mut g,
            concept(21, 1, "ship the API", ConceptType::Entity),
            1,
        );
        // A third isolated concept the goal does NOT name — the control.
        let bystander = insert_isolated(
            &mut g,
            concept(22, 1, "unnamed island", ConceptType::Entity),
            1,
        );

        g.set_root_goal(Some(serde_json::json!([
            "launch the product",
            "ship the API"
        ])));
        assert_eq!(g.root_goal_texts().len(), 2);
        // `set_root_goal` also auto-promotes both to Venerable (spec §9), which
        // would protect them by status alone. Demote them so the *exclusion
        // list* is the only thing under test.
        for (ev, node) in [(60u64, 20u64), (61, 21)] {
            g.apply_canonization_transition(transition(
                ev,
                node,
                CanonizationStatus::Venerable,
                CanonizationStatus::Canonical,
            ))
            .unwrap();
            g.apply_canonization_transition(transition(
                ev + 100,
                node,
                CanonizationStatus::Canonical,
                CanonizationStatus::None,
            ))
            .unwrap();
        }

        let outcome = run(&mut g, default_params());
        for id in [goal_a, goal_b] {
            assert!(
                g.node(id).is_some(),
                "an array-named goal concept must be protected: {outcome:?}"
            );
        }
        assert!(
            g.node(bystander).is_none(),
            "an unnamed isolated concept is still collected"
        );
    }

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
        assert_eq!(
            outcome.survivors_pending,
            ids[4..],
            "the tail past the chunk is deferred, id-ascending"
        );
        let upserts = g
            .drain_log()
            .mutations
            .iter()
            .filter(|m| matches!(m, Mutation::UpsertNode { .. }))
            .count();
        assert_eq!(upserts, 4, "one flush chunk of upserts, not ten");
        for (i, id) in ids.iter().enumerate() {
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
}
