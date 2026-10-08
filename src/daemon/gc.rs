//! GC — spec §9, periodic only (T4.5; canonization's food).
//!
//! Runs every `gc_interval` mutations, or once `gc_max_interval` has elapsed
//! since the last sweep and at least `gc_idle_floor` mutations happened since
//! (issue #29; [`sweep_due`] is that rule, the caller — T4.6's loop — applies
//! it; this module decides *what*). [`run`] is a pure, fixture-testable
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
//!    nothing else uses), scores each concept over the live dimensions plus
//!    its own frequency term
//!    ([`crate::daemon::score::score_live_plus_frequency`], issue #29 — an
//!    access can only raise a score, never another concept's), and compares against a per-type bar
//!    scaled by [`crate::types::ConceptType::eviction_resistance`] (ALGO-11).
//!    See [`MIN_CONCEPT_SCORE`] for the calibration and its evidence.
//!
//!    Four protections (issue #29) keep the score cut from deleting reasoning
//!    by session age once sweeps run daily:
//!
//!    * **Logic, Constraint and Observation are exempt from the score cut**
//!      ([`ConceptType::exempt_from_gc_score_cut`]; Observation joined the
//!      list by operator decision, 2026-10-07). They are still collected as
//!      orphans or disconnected components — those clauses are structural, not
//!      age-based. The score cut therefore only removes Entities and
//!      Resources, and a Resource with dependents is spared it too.
//!    * **A Resource with dependents is spared the score cut**
//!      ([`resources_with_dependents`], operator decision): another concept has
//!      a `Dependency`/`Causal`/`Hierarchical` edge into it, or its blast radius
//!      is non-zero. An isolated, untouched Resource still ages out. Counted in
//!      [`GcOutcome::resources_spared_by_dependents`].
//!    * **Eviction recency is time since last touch** over a fixed
//!      [`GC_RECENCY_WINDOW`] ([`eviction_recency`]), not position in the
//!      session's interaction span, so a session growing older does not by
//!      itself push old concepts under the bar. This is GC's cut only: the
//!      daemon's ranking ([`crate::daemon::score::rescore`]), recall and
//!      canonization keep the span-relative recency.
//!    * **Collections per sweep are capped** ([`collection_cap`]), over steps
//!      2 and 3 together. Under the cap structural garbage goes first —
//!      orphans, then disconnected components — and the score cut's
//!      candidates last, furthest under their bar first; concepts cut off by
//!      the score cut's collections (step 3's cascade) take what budget is
//!      left. Candidates past the cap are listed in [`GcOutcome::deferred`]
//!      (counted in [`GcOutcome::collections_deferred`]) and re-evaluated on
//!      the next sweep.
//! 3. **Disconnected-component cleanup** — a cycle-safe BFS (visited set, per
//!    the G6 binding note — never assume Hierarchical acyclicity) from the
//!    temporal chain over the full undirected graph; every concept not reached
//!    is collected. Protected classes are exempt ("protected classes survive"
//!    contract); interactions are append-only and never collected. Measured
//!    twice: on the post-step-1 graph (ranked before the score cut under the
//!    cap), and again after the collections, so a concept reachable only
//!    through a score-cut concept still goes in the same sweep.
//! 4. **Index maintenance** — the inverted index (T2.6) is owner-side (P3
//!    contract, `src/graph/mod.rs`), so `run(&mut Graph, …)` cannot reach it.
//!    [`GcOutcome::concepts_collected`] + [`sync_index`] are the hook: the
//!    owner MUST call `sync_index(&outcome, &mut index)` after `run` (each
//!    collected id → `InvertedIndex::remove`). Survivor bumps never change
//!    content, so no re-`add` is ever needed.
//! 5. **`gc_survived += 1` on all survivors** — except candidates the cap held
//!    back, which this sweep judged collectable (issue #29) — via
//!    [`Graph::bump_gc_survived`] (saturating `i32`), which emits `UpsertNode`
//!    mutations so the durable store mirrors the counter. Stage 1's input —
//!    the reason GC cannot be cut. **Chunked** (CONC-6/XP-10): one call applies
//!    at most [`GC_SURVIVOR_BUMP_CHUNK`] bumps and returns the rest in
//!    [`GcOutcome::survivors_pending`], which the owner drains with
//!    [`drain_survivor_bumps`] over later cycles — see that function for why
//!    the store still converges exactly. The bump order is rotated per sweep
//!    ([`survivor_drain_order`]) so the bumps a restart loses are not always
//!    the same ids'.
//! 6. **Canonical budget** — GC *records* the Canonical count and the
//!    over-budget flag in [`GcOutcome`]; demotion (lowest-blast-radius, spec
//!    §10) is T6.4's job — GC never demotes.
//! 7. **MutationEpoch** — bumped by GC's own mutations (edge removals, node
//!    removals, survivor upserts); `src/graph/graph.rs`'s epoch doc calls this
//!    "redundant but harmless (any mutation already bumps the epoch)".
//!    [`GcOutcome::epoch_before`]/[`GcOutcome::epoch_after`] prove the bump.
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
///
/// ### The CoOccurrence margin is zero, deliberately (issue #29)
///
/// `derive` writes every `CoOccurrence` edge at exactly
/// [`crate::graph::derive::COOCCURRENCE_WEIGHT`] (0.5) and the cut is a strict
/// `<`, so a never-reinforced co-occurrence edge is **kept**; reinforcement only
/// raises weights and nothing in the codebase decays them. Step 1 therefore
/// removes only `Semantic` edges written below 0.5. That is intended: a
/// co-occurrence edge is the only edge `lambo_derive` writes between the
/// concepts of one call, so cutting it after the TTL would strip most derived
/// reasoning of its only concept-to-concept link on the first sweep, collapse
/// its density, and hand the step-2 cut a cascade (the #29 dry run: 321 of 537
/// first-sweep collections touched a CoOccurrence edge). Lowering this bar, or
/// the write weight, is a behaviour change, not a tidy-up —
/// `cooccurrence_edges_sit_exactly_on_the_step_one_bar_and_survive` pins both.
pub const MIN_EDGE_WEIGHT: f64 = 0.5;
/// An edge untouched for this long is "past `gc_edge_ttl`" (step 1).
pub const GC_EDGE_TTL: ChronoDuration = ChronoDuration::seconds(3600);
/// Below this daemon composite score, a concept is sub-threshold (step 2).
/// v0.6.0's value is not in-repo — v0.1 decision, **recalibrated** (ALGO-1).
///
/// The threshold is not applied flat: the concept's eviction score is
/// [`crate::daemon::score::score_over_live_dimensions`] plus the concept's own
/// frequency term ([`crate::daemon::score::score_live_plus_frequency`], issue
/// #29; an unread concept scores exactly the live-dimension score this bar was
/// calibrated against, whatever else has been read) and the comparison is against
/// `MIN_CONCEPT_SCORE / ConceptType::eviction_resistance()` — the spec §5
/// resistances (Constraint 1.5 … Observation 0.7) scale the bar per type
/// instead of every type facing the identical cut (ALGO-11); the types
/// [`ConceptType::exempt_from_gc_score_cut`] names (Logic, Constraint,
/// Observation) never reach the bar at all. Dividing the
/// threshold by the resistance is algebraically the same as multiplying the
/// score by it; the threshold form is used so the *bar* is what varies and the
/// score stays comparable to recall's.
///
/// ### Why 0.12
///
/// The original 0.3 was calibrated against nothing: with `access_count`
/// identically 0 (no write path fed it until issue #30) and `density`
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
/// scores at most its type modifier (Entity +0.05, Resource 0.0), which is
/// below both bars that still apply (Entity 0.10, Resource 0.12). Orphans and
/// disconnected components are collected by their own clauses regardless of score, so the score cut is
/// deliberately the conservative one. Reading a concept only adds headroom:
/// its frequency term is added on top of this scale, never traded for it
/// (issue #29).
pub const MIN_CONCEPT_SCORE: f64 = 0.12;
/// Advisory concept-count ceiling: warn above, never evict (spec §9).
pub const MAX_CONCEPT_NODES: usize = 10_000;

/// Default `gc_max_interval` (issue #29): a session still taking writes sweeps
/// at least this often. One day — the Metal rig's working rhythm.
pub const GC_MAX_INTERVAL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Default `gc_idle_floor` (issue #29): the time bound fires only after this
/// many session mutations since the last sweep. Measured on the Metal rig
/// (lower bound, concepts + edges + interactions per day over 49 days): a
/// floor of 100 skips the 12 trivial active days (1–8 interactions) and sweeps
/// on the 24 working days.
pub const GC_IDLE_FLOOR: u64 = 100;

/// Step 2's eviction recency window (issue #29): a concept touched `now` has
/// eviction recency 1.0, falling linearly to 0.0 at this age and staying
/// there. See [`eviction_recency`]. A scoring constant, not a cadence, so it is
/// not settable from `lambo.toml` (see [`crate::config::DaemonConfig`]).
///
/// ### Why 365 days
///
/// Lambo is long-term memory, and what the score cut can still reach is
/// mostly long-tail pointers: file paths, PRs, commits, verdicts, the Entities
/// and Resources a session mentioned once. Those are exactly what someone
/// returns to a year later, and they are a small slice of the store (about 4%
/// on the Metal rig once Logic, Constraint, Observation and Resources with
/// dependents are spared), so the space a short window would reclaim is
/// negligible next to what losing one of them costs. Whatever the window, an
/// untouched store converges on the same set once every concept is older than
/// it; the window sets how long an unused, sparsely connected concept is kept,
/// not whether. A year keeps a project's pointers through a long pause, and
/// once recall records accesses (issue #30) a concept that is still being used
/// keeps resetting its own clock.
///
/// The window started at 90 days (measured on the rig snapshot, see the
/// dev-diary note `gc-time-bound-29.md`, whose sensitivity table is the
/// 90-day-era measurement) and was widened to a year by operator decision once
/// Observations left the cut.
pub const GC_RECENCY_WINDOW: ChronoDuration = ChronoDuration::days(365);

/// Step 2+3 collection cap as a fraction of the sweep's unprotected concepts
/// (issue #29); see [`collection_cap`].
pub const GC_MAX_COLLECT_FRACTION: f64 = 0.05;

/// The cap never drops below this many collections, so a small session can
/// still clear its orphans in one sweep (issue #29); see [`collection_cap`].
pub const GC_MIN_COLLECT_CAP: usize = 32;

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
    /// Step 2: the eviction recency window ([`GC_RECENCY_WINDOW`]).
    pub recency_window: ChronoDuration,
    /// Steps 2+3: collection cap fraction ([`GC_MAX_COLLECT_FRACTION`]).
    pub max_collect_fraction: f64,
    /// Steps 2+3: collection cap floor ([`GC_MIN_COLLECT_CAP`]).
    pub min_collect_cap: usize,
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
            recency_window: GC_RECENCY_WINDOW,
            max_collect_fraction: GC_MAX_COLLECT_FRACTION,
            min_collect_cap: GC_MIN_COLLECT_CAP,
        }
    }
}

/// How far in the future a stored `last_gc_at` may be before the daemon treats
/// it as a wall-clock jump and re-anchors the time bound at `now` (issue #29);
/// see [`gc_clock_ahead`].
///
/// ### Why 5 minutes
///
/// Large enough that ordinary clock discipline never trips it: NTP slews and
/// steps on a healthy host are milliseconds to seconds, and the daemon's own
/// cycle and the flush interval are seconds, so a sweep time written a moment
/// ago is never mistaken for a jump. Small against `gc_max_interval` (a day by
/// default, and the reason this matters at all): within the tolerance a future
/// stamp delays the time trigger by at most five minutes, while any jump big
/// enough to matter — an hour, a day, a wrong year — is caught on the next
/// cycle. Not configurable: it is a sanity bound on the host clock, not a
/// cadence.
pub const GC_CLOCK_SKEW_TOLERANCE: ChronoDuration = ChronoDuration::minutes(5);

/// Is the mark's `last_gc_at` later than `now` by more than
/// [`GC_CLOCK_SKEW_TOLERANCE`]? (Issue #29.)
///
/// A forward wall-clock jump stamps a future sweep (or anchor) time, and the
/// store's monotonic merge keeps it once the clock is corrected, so
/// [`sweep_due`]'s time trigger would read "not elapsed" until real time
/// caught up — possibly years. When this returns `true` the daemon re-anchors
/// the clock at `now` ([`Graph::reanchor_gc_clock`]), which persists through
/// the one regression path the store merge allows
/// ([`crate::types::GcMark::last_gc_at_reset`]). The mutation trigger and
/// `last_gc_epoch` are unaffected.
pub fn gc_clock_ahead(mark: crate::types::GcMark, now: DateTime<Utc>) -> bool {
    match mark.last_gc_at {
        Some(at) => at.signed_duration_since(now) > GC_CLOCK_SKEW_TOLERANCE,
        None => false,
    }
}

/// Why the daemon started a sweep (issue #29).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GcTrigger {
    /// `gc_interval` session mutations since the last sweep (spec §9).
    Mutations,
    /// `gc_max_interval` elapsed since the last sweep, with at least
    /// `gc_idle_floor` session mutations since it.
    Elapsed,
}

impl GcTrigger {
    /// The wire name: `"mutations"` or `"elapsed"` (`lambo_stats`' GC block).
    pub fn as_str(self) -> &'static str {
        match self {
            GcTrigger::Mutations => "mutations",
            GcTrigger::Elapsed => "elapsed",
        }
    }
}

/// The sweep trigger (issue #29): is a sweep due, and why?
///
/// `since` is the session mutations since the last sweep — `epoch -
/// mark.last_gc_epoch`, which excludes GC's own deferred writes (NEW-2). The
/// mutation trigger is unchanged from spec §9 and is not gated by the floor.
/// The time trigger needs both the elapsed bound and the floor, so an idle
/// session never sweeps on time alone, and it measures from one stored
/// instant, so a writer that was down for N intervals sweeps **once** (no
/// backlog). A never-anchored mark (`last_gc_at == None`) cannot time-trigger;
/// the daemon anchors it on first observation ([`Graph::anchor_gc_clock`]).
/// A never-*swept* session has `last_gc_epoch == 0`, so `since` is its whole
/// lifetime mutation count: one with at least `gc_idle_floor` of them sweeps
/// once, `gc_max_interval` after the anchor, **even if idle since** — a
/// deliberate catch-up for a store that grew before sweeps ran on time, not
/// an accident of the floor
/// (`never_swept_session_over_the_floor_catches_up_once_after_the_anchor`). A
/// clock that went backwards past the mark reads as "not elapsed" — the time
/// trigger waits, the mutation trigger is unaffected. A mark more than
/// [`GC_CLOCK_SKEW_TOLERANCE`] in the future is the daemon's to re-anchor
/// before calling this ([`gc_clock_ahead`]); this function stays pure.
pub fn sweep_due(
    epoch: u64,
    mark: crate::types::GcMark,
    now: DateTime<Utc>,
    gc_interval: u64,
    gc_max_interval: std::time::Duration,
    gc_idle_floor: u64,
) -> Option<GcTrigger> {
    let since = epoch.saturating_sub(mark.last_gc_epoch);
    if since >= gc_interval {
        return Some(GcTrigger::Mutations);
    }
    if since < gc_idle_floor {
        return None;
    }
    let bound = ChronoDuration::from_std(gc_max_interval).unwrap_or(ChronoDuration::MAX);
    match mark.last_gc_at {
        Some(at) if now.signed_duration_since(at) >= bound => Some(GcTrigger::Elapsed),
        _ => None,
    }
}

/// Everything one GC run did, for the owner (T4.6), T5.4, and T6.4.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GcOutcome {
    /// Step 1: edge ids removed for being below weight past TTL.
    pub edges_removed: Vec<NodeId>,
    /// Steps 2+3: concept ids collected, together with their incident edges.
    pub concepts_collected: Vec<NodeId>,
    /// Step 5: every concept id that survived this run **and takes its
    /// `gc_survived += 1`** — every remaining concept except
    /// [`deferred`](GcOutcome::deferred). Id-ascending.
    pub survivors: Vec<NodeId>,
    /// Step 5: the part of [`survivors`](GcOutcome::survivors) whose
    /// `gc_survived += 1` this run **deferred** — the owner must drain it with
    /// [`drain_survivor_bumps`] on later cycles (CONC-6/XP-10). Empty when the
    /// survivor set fit in one chunk. In **drain order**
    /// ([`survivor_drain_order`]), not id order.
    pub survivors_pending: Vec<NodeId>,
    /// Steps 2+3: the candidates the cap held back (issue #29), id-ascending.
    /// They stay in the graph, are re-evaluated next sweep, and do **not** take
    /// this sweep's survivor bump. `deferred.len() == collections_deferred`.
    pub deferred: Vec<NodeId>,
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
    /// Steps 2+3: the most concepts this sweep could collect
    /// ([`collection_cap`], issue #29).
    pub collection_cap: usize,
    /// Steps 2+3: candidates the cap held back this sweep (they survive it and
    /// are re-evaluated on the next one). Non-zero exactly when the cap bound;
    /// a warning in [`GcOutcome::warnings`] says so too.
    pub collections_deferred: usize,
    /// Why the daemon ran this sweep. `None` when `run` was called directly
    /// (tests, tooling); the daemon fills it in.
    pub trigger: Option<GcTrigger>,
    /// Step 2: Resources that scored under their bar this sweep and were kept
    /// only because they have dependents ([`resources_with_dependents`], issue
    /// #29 operator decision). A spared Resource that step 3 collects anyway,
    /// as a disconnected component, is not counted: it was not spared.
    pub resources_spared_by_dependents: usize,
}

impl GcOutcome {
    /// Did the per-sweep collection cap hold anything back?
    pub fn cap_bound(&self) -> bool {
        self.collections_deferred > 0
    }
}

/// Run one full GC cycle (spec §9 steps 1–7, in order).
///
/// Pure RAM work: no I/O, no locks — the caller owns the graph lock. Every
/// outcome vector is id-ascending except
/// [`GcOutcome::survivors_pending`], which is in drain order; all of them are
/// deterministic for a given graph and epoch.
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

    // Issue #29: one collection budget for steps 2 and 3 together.
    let unprotected = graph
        .concepts()
        .filter(|c| !protected.contains(&c.id))
        .count();
    outcome.collection_cap = collection_cap(unprotected, params);
    let mut budget = outcome.collection_cap;

    // Step 2 — concept cleanup: orphans + sub-threshold, excluding protected.
    // Scored against post-step-1 state with the session's own weights (ALGO-4),
    // each concept's own frequency on top of the live dimensions (issue #29),
    // GC's time-anchored recency (issue #29), and cut per concept type
    // (ALGO-11). Logic, Constraint and Observation are exempt from the score cut, not from
    // the orphan clause, and a Resource with dependents is spared it (issue
    // #29).
    let ctx = crate::daemon::score::SessionContext::compute(graph);
    // Issue #29 operator decision: a Resource with dependents is spared the
    // score cut (see `resources_with_dependents`). Computed once, post-step-1.
    let depended_on = resources_with_dependents(graph);
    let mut orphans: Vec<NodeId> = Vec::new();
    let mut below: Vec<(f64, NodeId)> = Vec::new();
    // Under-bar Resources the dependents rule kept; counted below, once the
    // disconnected components are known (a spared Resource that is also
    // disconnected is collected as that, so it was not spared).
    let mut spared: Vec<NodeId> = Vec::new();
    for c in graph.concepts() {
        if protected.contains(&c.id) {
            continue;
        }
        if graph.incident_edges(c.id).is_empty() {
            orphans.push(c.id);
            continue;
        }
        if c.concept_type.exempt_from_gc_score_cut() {
            continue;
        }
        let score = eviction_score(graph, c, &ctx, params);
        let bar = eviction_threshold(params.min_concept_score, c.concept_type);
        if score < bar {
            if depended_on.contains(&c.id) {
                spared.push(c.id);
                continue;
            }
            below.push((score / bar, c.id));
        }
    }

    // Step 3 — disconnected components, measured on the same post-step-1
    // graph: a cycle-safe BFS from the temporal chain. An orphan is also
    // unreachable; it is listed once, as an orphan.
    let reachable = reachable_from_temporal_chain(graph);
    let orphan_set: HashSet<NodeId> = orphans.iter().copied().collect();
    let mut disconnected: Vec<NodeId> = graph
        .concepts()
        .map(|c| c.id)
        .filter(|id| !reachable.contains(id) && !protected.contains(id) && !orphan_set.contains(id))
        .collect();
    let disconnected_set: HashSet<NodeId> = disconnected.iter().copied().collect();
    outcome.resources_spared_by_dependents = spared
        .iter()
        .filter(|id| !disconnected_set.contains(id))
        .count();

    // Under the cap, structural garbage goes first — orphans, then the other
    // disconnected components — and the score cut's candidates last, furthest
    // under their bar first. A concept both disconnected and under its bar is
    // taken as disconnected. Ids break ties so the choice is deterministic.
    orphans.sort_by_key(|id| id.0);
    disconnected.sort_by_key(|id| id.0);
    below.retain(|(_, id)| !disconnected_set.contains(id));
    below.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1 .0.cmp(&b.1 .0)));
    let candidates: Vec<NodeId> = orphans
        .into_iter()
        .chain(disconnected)
        .chain(below.into_iter().map(|(_, id)| id))
        .collect();
    let take = candidates.len().min(budget);
    let mut held_back: Vec<NodeId> = candidates[take..].to_vec();
    for id in &candidates[..take] {
        if graph.remove_node(*id).is_ok() {
            outcome.concepts_collected.push(*id);
            budget -= 1;
        }
    }

    // Step 3, cascade — concepts the collections above cut off from the
    // temporal chain (reachable only through a score-cut concept). Collected
    // in the same sweep, as before the cap existed, with whatever budget is
    // left; a candidate already held back above is not counted twice.
    let reachable = reachable_from_temporal_chain(graph);
    let already: HashSet<NodeId> = held_back.iter().copied().collect();
    let mut cascade: Vec<NodeId> = graph
        .concepts()
        .map(|c| c.id)
        .filter(|id| !reachable.contains(id) && !protected.contains(id) && !already.contains(id))
        .collect();
    cascade.sort_by_key(|id| id.0);
    let take = cascade.len().min(budget);
    held_back.extend_from_slice(&cascade[take..]);
    for id in &cascade[..take] {
        if graph.remove_node(*id).is_ok() {
            outcome.concepts_collected.push(*id);
        }
    }
    outcome.concepts_collected.sort_by_key(|id| id.0);
    held_back.sort_by_key(|id| id.0);
    outcome.collections_deferred = held_back.len();
    if outcome.cap_bound() {
        outcome.warnings.push(format!(
            "GC collection cap bound: collected {} of {} candidates (cap {} = max({}, {:.0}% of {} \
             unprotected concepts)); {} deferred to the next sweep",
            outcome.concepts_collected.len(),
            outcome.concepts_collected.len() + outcome.collections_deferred,
            outcome.collection_cap,
            params.min_collect_cap,
            params.max_collect_fraction * 100.0,
            unprotected,
            outcome.collections_deferred
        ));
    }

    // Step 5 — survivors: every remaining concept gets gc_survived += 1,
    // except the candidates the cap held back (issue #29: this sweep judged
    // them collectable, so it must not credit them with surviving it — Stage 1
    // reads the counter). At most `max_survivor_bumps` land here; the tail is
    // deferred to later cycles (CONC-6/XP-10 — see `drain_survivor_bumps` for
    // the convergence argument), in a drain order rotated per sweep so no id
    // range is always last (see `survivor_drain_order`).
    let held: HashSet<NodeId> = held_back.iter().copied().collect();
    let mut survivors: Vec<NodeId> = graph
        .concepts()
        .map(|c| c.id)
        .filter(|id| !held.contains(id))
        .collect();
    survivors.sort_by_key(|id| id.0);
    let order = survivor_drain_order(&survivors, epoch_before);
    let split = order.len().min(params.max_survivor_bumps);
    graph.bump_gc_survived(&order[..split]);
    outcome.survivors_pending = order[split..].to_vec();
    outcome.survivors = survivors;
    outcome.deferred = held_back;

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

/// Every node reachable from the temporal chain over the undirected graph —
/// step 3's cycle-safe BFS (visited set, per the G6 binding note: never assume
/// Hierarchical acyclicity).
fn reachable_from_temporal_chain(graph: &Graph) -> HashSet<NodeId> {
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
    reachable
}

/// The order a sweep's survivor bumps are applied in (issue #29): the
/// id-ascending survivor list rotated by an offset derived from the sweep's
/// starting epoch.
///
/// Pending bumps live in the daemon, not the store, so a writer restart in the
/// middle of a drain loses the rest of that sweep's bumps. With a fixed
/// id-ascending order the lost tail was always the same high-id concepts, so a
/// restart-prone writer systematically starved them of `gc_survived` (Stage 1's
/// input). Rotating the start per sweep spreads that loss across the id space:
/// each concept lands in the tail about as often as any other. The offset is a
/// SplitMix64 mix of `epoch_before`, so two sweeps a few mutations apart do not
/// start at neighbouring positions, and the order is still deterministic for a
/// given graph and epoch. Persisting the pending set instead was not done: it
/// is a new store column and schema change for a counter whose only loss is a
/// bounded, now unbiased delay.
pub fn survivor_drain_order(survivors: &[NodeId], epoch_before: u64) -> Vec<NodeId> {
    let mut order = survivors.to_vec();
    if order.len() > 1 {
        let mut z = epoch_before.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        let offset = (z % order.len() as u64) as usize;
        order.rotate_left(offset);
    }
    order
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

/// The per-sweep collection cap (issue #29): `max(min_collect_cap,
/// ceil(max_collect_fraction × unprotected))`, counted over steps 2 and 3
/// together.
///
/// The cap bounds how much a single sweep can delete if a scoring change, a
/// calibration miss or a bug misjudges a whole class of concepts: with daily
/// sweeps the worst case is a 5%-per-day erosion that an operator sees in the
/// `GC collection cap bound` warning, instead of one sweep taking a sixth of
/// the store (the #29 dry run's first sweep). A non-finite or negative
/// fraction counts as 0, leaving the floor.
pub fn collection_cap(unprotected: usize, params: GcParams) -> usize {
    let fraction = if params.max_collect_fraction.is_finite() && params.max_collect_fraction > 0.0 {
        params.max_collect_fraction.min(1.0)
    } else {
        0.0
    };
    let by_fraction = (unprotected as f64 * fraction).ceil() as usize;
    by_fraction.max(params.min_collect_cap)
}

/// Resources the step-2 score cut must not collect because other concepts
/// depend on them (issue #29 operator decision, 2026-10-07).
///
/// "Dependents" uses the two senses the codebase already gives the word, over
/// the structural edge kinds blast radius counts
/// ([`crate::recall::format::STRUCTURAL_EDGE_TYPES`]: `Dependency`, `Causal`,
/// `Hierarchical`; never `Derives`, `Temporal`, `CoOccurrence` or `Semantic`),
/// and only concept-to-concept edges with a source other than the Resource
/// itself (a self-loop is not a dependent):
///
/// * **Incoming** — another concept has a structural edge **into** the
///   Resource. This is `record_action`'s direction (`src/graph/action.rs`):
///   the action node is the source of `Dependency` edges to what it depends
///   on and of `Causal` edges to what it produces or modifies, so a Resource
///   that some action depends on, produced or modified has one. This is the
///   operator's rule as stated.
/// * **Blast radius** — the Resource has a non-zero blast radius
///   ([`crate::recall::format::blast_radius`]): some concept's *only*
///   structural source is this Resource, which is exactly what the load-bearing
///   warning reports as "N nodes depend on this". This is the outgoing side:
///   a `record_action` node is the sole source of the Resources it alone
///   produced or depends on.
///
/// The second sense is included because the first alone erodes from the
/// source end: an action node usually has no incoming structural edge, so it
/// would be collected, its targets would lose their only incoming edge with it,
/// and the next sweep would collect them too — the dependency graph blast
/// radius reads would disappear one layer per day.
///
/// Only the score cut honours this. A Resource with no structural edge at all
/// (an isolated, untouched one) ages out under [`eviction_recency`] like any
/// other concept, and orphan and disconnected-component cleanup are unchanged.
pub fn resources_with_dependents(graph: &Graph) -> HashSet<NodeId> {
    let is_resource = |id: NodeId| {
        matches!(
            graph.node(id),
            Some(crate::types::Node::Concept(c)) if c.concept_type == ConceptType::Resource
        )
    };
    let mut out = HashSet::new();
    for (dst, srcs) in crate::recall::format::inbound_sources(graph) {
        if is_resource(dst) && srcs.iter().any(|s| *s != dst) {
            out.insert(dst);
        }
        if let [only] = srcs.as_slice()
            && *only != dst
            && is_resource(*only)
        {
            out.insert(*only);
        }
    }
    out
}

/// GC's eviction recency for one concept (issue #29): `1 − age / window`,
/// clamped to `[0, 1]`, where `age = now − last touch` and the last touch is
/// the later of `created_at` and `last_accessed`.
///
/// This replaces the span-relative recency of
/// [`crate::daemon::score::score_concept`] **in GC's cut only**. Span-relative
/// recency puts the session's oldest concept at 0 however recently the session
/// started, so a session that simply keeps going pushes ever more untouched
/// concepts under the bar (the #29 projection: 537 → 1,049 collections as the
/// span grew by 60 days with no new concepts). Anchored to a fixed window, a
/// concept's eviction recency depends only on how long since anything touched
/// it, and once every untouched concept is past the window, more session age
/// changes nothing. A concept recalled again (`last_accessed`, issue #30)
/// regains recency. A future-dated touch (clock skew) counts as `now`; a
/// non-positive window degrades to 0 for every concept rather than dividing by
/// zero.
pub fn eviction_recency(c: &Concept, now: DateTime<Utc>, window: ChronoDuration) -> f64 {
    let last_touch = match c.last_accessed {
        Some(at) => at.max(c.created_at),
        None => c.created_at,
    };
    let window_ms = window.num_milliseconds();
    if window_ms <= 0 {
        return 0.0;
    }
    let age_ms = now
        .signed_duration_since(last_touch)
        .num_milliseconds()
        .max(0);
    (1.0 - age_ms as f64 / window_ms as f64).clamp(0.0, 1.0)
}

/// The step-2 bar for one concept type: [`MIN_CONCEPT_SCORE`] divided by the
/// spec §5 [`ConceptType::eviction_resistance`] (ALGO-11).
///
/// An Entity (1.2) faces a bar a sixth lower than a Resource (1.0). The
/// exempt types (Logic, Constraint, Observation) never get here; their
/// resistances still scale Solo promotion ([`crate::canon::SoloScorer`]). A
/// non-positive or non-finite
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
/// The live-dimension score plus this concept's own frequency term
/// ([`crate::daemon::score::score_live_plus_frequency`]), with GC's
/// time-anchored recency in place of the span-relative one.
///
/// Issue #29 replaced ALGO-1's **session-wide** switch here. That switch moved
/// every concept from the live-dimension score to the full composite as soon
/// as *any* concept had an access, which lowers every unread concept's score
/// (the full composite is `0.8 ×` the live one on the weighted part at
/// frequency 0, NEW-6). Under a time-anchored recency that was a cliff: on the
/// Metal rig snapshot one access on one Entity took the first sweep from 159
/// to 412 candidates. Per concept and additive, an access can only raise the
/// accessed concept's score and never touches anyone else's, and an unread
/// concept keeps exactly the scale [`MIN_CONCEPT_SCORE`] and
/// [`GC_RECENCY_WINDOW`] were calibrated on. Recall ranking, the daemon's score
/// table and canonization are unchanged.
fn eviction_score(
    graph: &Graph,
    c: &Concept,
    ctx: &crate::daemon::score::SessionContext,
    params: GcParams,
) -> f64 {
    let mut dims = crate::daemon::score::score_concept(graph, c, ctx);
    // Issue #29: GC's cut measures recency from the last touch, not from the
    // concept's position in the session span (see `eviction_recency`).
    dims.recency = eviction_recency(c, params.now, params.recency_window);
    crate::daemon::score::score_live_plus_frequency(dims, &params.weights)
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
                if let Some(e) = graph.edge_between(node, tgt, ty)
                    && e.weight < params.min_edge_weight
                    && params.now.signed_duration_since(e.last_reinforced) > params.gc_edge_ttl
                {
                    dead.push(e.id);
                }
            }
        }
    }
    dead.sort_by_key(|id| id.0);
    dead.dedup();
    dead
}

#[cfg(test)]
mod tests;
