//! Daemon composite scoring — spec §9.
//!
//! ```text
//! score = recency·0.25 + frequency·0.20 + session_activity·0.20 + density·0.35
//!         + edge_type_bonus + concept_type_modifier
//! ```
//!
//! Every weighted dimension is clamped to `[0,1]` **before** weighting; a
//! NaN/±Inf input counts as `0.0` for that dimension (spec §9).
//! `centrality_bonus` is cut. The composite is clamped to
//! `[0, 1 + MAX_BONUS]` so the score is bounded and finite for any inputs.
//!
//! ## Dimension semantics (T4.1 interpretation)
//!
//! v0.6.0 §7.3's tables are not in-repo (the v0.1 spec freezes only the
//! formula, §9), so each dimension is defined from data the graph actually
//! carries. All four weighted dimensions are session-relative, so the same
//! graph scores identically regardless of wall clock (fixture-friendly):
//!
//! * **recency** — how recently the concept was last touched (the later of
//!   `last_accessed` and `created_at`), measured back from the session's
//!   latest interaction. In integer milliseconds,
//!   `1 − (end − last_touch) / max(end − start, MIN_RECENCY_SPAN)`, clamped
//!   to `[0, 1]`. [`MIN_RECENCY_SPAN`] is 10 minutes, so a session younger
//!   than that scores every concept near 1 and a millisecond gap between
//!   derives cannot sweep the dimension. A session at least that long uses
//!   its own span, and those values are the historical
//!   `(last_touch − start) / span` bits: the two expressions agree in real
//!   arithmetic and not in `f64`, and the bits are what the fixtures pin.
//!   A single-instant session returns `1.0` when `last_touch` is that
//!   instant, and a session with no interactions returns `1.0`. No wall
//!   clock enters.
//! * **frequency** — `access_count` normalized by [`FREQUENCY_NORMALIZER`]
//!   (10 accesses = full frequency), clamped.
//! * **session_activity** — how many interactions derived this concept,
//!   divided by the most-derived concept in the session
//!   (`derived_by(c) / max derived_by`, issue #180). Same shape as density:
//!   the busiest concept is `1.0`, a concept nothing derived is `0.0`, and a
//!   session with no derivations is `0.0` rather than `NaN`. [`score`] clamps
//!   it with every other weighted dimension, and a non-finite value counts as
//!   `0.0`. Dividing by the interaction count sent the term to about `1/|I|`
//!   in any real session, so a fifth of the composite carried nothing.
//! * **density** — incident-edge count normalized by the session's most
//!   connected concept (densest concept = `1.0`), clamped. `Contradicts`
//!   edges are not counted ([`EdgeType::counts_toward_density`], #19).
//! * **edge_type_bonus** — additive per incident edge by type; structural /
//!   load-bearing types carry the most, provenance (`Derives`, `Temporal`)
//!   carries none; capped at [`MAX_EDGE_BONUS`]. The v0.6.0 table is not
//!   in-repo — [`edge_type_bonus_value`] is the T4.1 interpretation.
//! * **concept_type_modifier** — the additive form of the P1 typed
//!   multipliers: [`ConceptType::score_multiplier`] − 1.0 (Constraint +0.15,
//!   Entity +0.05, Logic +0.05, Resource ±0.0, Observation −0.10). The spec
//!   formula is additive (`+ concept_type_modifier`), so the existing
//!   multiplier consts are converted to offsets rather than applied
//!   multiplicatively.
//!
//! ## Which activity each caller reads (#180)
//!
//! [`score_concept`] returns the max-normalized value. Recall, [`rescore`]
//! and swarm Stage 1's queue all read [`score`] of that. GC's step-2 cut
//! does not. [`SessionContext::per_interaction_activity`] is still
//! `derived_by / |interactions|`, and `eviction_score` substitutes it before
//! the comparison with [`crate::daemon::gc::MIN_CONCEPT_SCORE`].
//! Max-normalization would score a concept derived once at `1/max`. At a
//! session maximum of 1 or 2 that floor alone clears the Resource bar (0.12)
//! and the Entity bar (0.10), so every aged, unread, ordinary concept would
//! survive the cut. The bar stays on the ratio it was calibrated against.
//!
//! Swarm Stage 1 does not threshold the composite. Membership is
//! `gc_survived >= 3` and two aged dependent interactions
//! ([`crate::canon::stage1_candidates`]); the composite only orders who a
//! bounded cycle takes first. Solo promotion ([`crate::canon::SoloScorer`])
//! scores a separate evidence sum and does not read this dimension. Neither
//! admission bar moves with the scale, so neither is recalibrated. Ordering
//! does: a concept derived more often ranks earlier in the swarm queue,
//! which is the signal #180 puts back.
//!
//! The log form `ln(1+k) / ln(1+k_max)` was not taken. On the dogfood
//! snapshot measured for this change the busiest concept is derived 5 times
//! and almost every other concept once, so `1/5` is already on a usable
//! scale and damping the top would shrink the only variation the dimension
//! has. Method and the Kendall τ: `evidence/issue-180/`.

use chrono::{DateTime, Utc};
use std::collections::HashMap;

use crate::config::ScoringWeights;
use crate::graph::Graph;
use crate::types::{tie_break_by_key, Concept, ConceptType, EdgeType, Node, NodeId, Scored};

/// Frequency saturates at this many accesses (documented interpretation).
pub const FREQUENCY_NORMALIZER: f64 = 10.0;
/// Cap on the additive edge-type bonus (keeps the composite bounded).
pub const MAX_EDGE_BONUS: f64 = 0.25;
/// Largest additive concept-type modifier (Constraint: 1.15 − 1.0).
pub const MAX_CONCEPT_MODIFIER: f64 = 0.15;
/// Smallest additive concept-type modifier (Observation: 0.9 − 1.0).
pub const MIN_CONCEPT_MODIFIER: f64 = -0.10;
/// Upper bound of `edge_type_bonus + concept_type_modifier`.
pub const MAX_BONUS: f64 = MAX_EDGE_BONUS + MAX_CONCEPT_MODIFIER;

/// Floor on recency's denominator, in integer milliseconds (issue #95).
///
/// Ten minutes. A session shorter than this still measures recency back from
/// its latest interaction across this window, so a stall of tens of
/// milliseconds cannot move a final recall score by a real query gap. A
/// session at least this long uses its own span. The denominator is
/// `max(span, MIN_RECENCY_SPAN)`, and the historical division is taken when
/// `denom == span_ms`, which is `span_ms >= MIN_RECENCY_SPAN`: a session of
/// exactly this length stays on the historical bits. A `span_ms >
/// MIN_RECENCY_SPAN` test would send it down the floor formula instead. Do
/// not convert the denominator to seconds: truncating it would move those
/// bits.
pub const MIN_RECENCY_SPAN: i64 = 10 * 60 * 1_000;

/// Additive per-edge-type bonus (T4.1 interpretation; v0.6.0's table is not
/// in-repo). Structural / load-bearing edge types carry the most weight.
pub fn edge_type_bonus_value(ty: EdgeType) -> f64 {
    match ty {
        EdgeType::Causal => 0.02,
        EdgeType::Dependency => 0.02,
        EdgeType::Hierarchical => 0.015,
        EdgeType::Semantic => 0.01,
        EdgeType::CoOccurrence => 0.005,
        EdgeType::Derives | EdgeType::Temporal | EdgeType::Contradicts => 0.0,
    }
}

/// Additive concept-type modifier: the P1 typed multiplier as an offset.
pub fn concept_type_modifier(t: ConceptType) -> f64 {
    t.score_multiplier() - 1.0
}

/// Per-concept inputs to the spec §9 formula (typed carrier).
///
/// [`score`] applies the spec's clamp rule. Every dimension except `recency`
/// is the raw, un-clamped value; `recency` arrives already clamped to
/// `[0, 1]` (issue #95), so the clamp is a no-op for it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScoreDims {
    pub recency: f64,
    pub frequency: f64,
    pub session_activity: f64,
    pub density: f64,
    pub edge_type_bonus: f64,
    pub concept_type_modifier: f64,
}

impl ScoreDims {
    /// The spec §9 composite with default weights.
    pub fn composite(self) -> f64 {
        score(self, &ScoringWeights::default())
    }
}

/// Clamp a weighted dimension to `[0,1]`; NaN/±Inf → `0.0` (spec §9).
fn clamp_dim(x: f64) -> f64 {
    if x.is_finite() {
        x.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// Clamp an additive term to `[lo, hi]`; NaN/±Inf → `0.0`.
fn clamp_additive(x: f64, lo: f64, hi: f64) -> f64 {
    if x.is_finite() {
        x.clamp(lo, hi)
    } else {
        0.0
    }
}

/// Spec §9 composite score for one concept's dimensions.
///
/// Each weighted dimension is clamped to `[0,1]` before weighting; the
/// additive bonus and modifier are clamped to their defined ranges; the
/// result is clamped to `[0, 1 + MAX_BONUS]` (bounded, always finite).
///
/// `weights` are sanitized first ([`ScoringWeights::sanitized`], ALGO-10):
/// they arrive from TOML/JSON, where `NaN` is admissible, and a `NaN` weight
/// would otherwise propagate into a `NaN` composite — which ranks as garbage
/// and silently disables GC's threshold comparison. The final clamp is
/// non-finite-guarded for the same reason: this function returns a finite
/// value for every possible input (spec §5.7).
pub fn score(dims: ScoreDims, weights: &ScoringWeights) -> f64 {
    let w = weights.sanitized();
    let weighted = clamp_dim(dims.recency) * w.recency
        + clamp_dim(dims.frequency) * w.frequency
        + clamp_dim(dims.session_activity) * w.session_activity
        + clamp_dim(dims.density) * w.density;
    finite_or_zero(weighted + bonus_and_modifier(dims))
}

/// Spec §9 composite over the dimensions that are **live** in v0.1: the
/// `frequency` term is dropped and the weighted sum is renormalized over the
/// remaining weights, so the result occupies the same `[0,1]`-plus-bonus range
/// as [`score`].
///
/// GC's step-2 cut builds on this: [`score_live_plus_frequency`] adds each
/// concept's own frequency term on top. Issue #29 replaced ALGO-1's
/// session-wide switch to [`score`] (measured in the NEW-6 note below) with
/// that per-concept addition. The spec formula reserves 20% of the composite
/// for a dimension no write path feeds until P5 recall lands, so an
/// **absolute** threshold against
/// the full composite measures every concept against a fifth of a score it
/// cannot yet earn. Recall *ranking* is unaffected by the dead term (a
/// constant-zero dimension cannot reorder anything), which is why [`score`]
/// itself stays spec-verbatim — only a threshold comparison is distorted.
///
/// ## The switch back to [`score`] LOWERS scores (NEW-6)
///
/// Renormalizing rather than re-weighting keeps the change reversible, but not
/// score-preserving, and the direction is the opposite of what this block used
/// to claim. Dividing by the live weight total is a *multiplication by
/// `1/live_total`* — 1.25 at the default weights — so switching back multiplies
/// the weighted part by `live_total` again: at `frequency == 0` the full
/// composite is `0.8 ×` the live one on the weighted part (the additive bonus and
/// type modifier are untouched). Only a concept whose frequency has actually
/// started earning comes out ahead.
///
/// GC's cut no longer flips (issue #29, [`score_live_plus_frequency`]); this
/// note records why it must not. Measured on the shipped `session-rest-api`
/// fixture (all 22 concepts, at the moment the first access lands and the old
/// cut flipped): **every** concept's
/// eviction score falls, by a factor of 0.83–0.89, and the smallest margin to its
/// type's bar goes from **1.49× to 1.33×**. Nothing crosses the bar, so the
/// switch does not by itself make GC collect anything — but the headroom
/// [`crate::daemon::gc::MIN_CONCEPT_SCORE`] was calibrated against is ~11%
/// smaller after it, which is the number to anchor on when tuning the threshold.
///
/// Issue #30 made `access_count` live, which is why GC's cut no longer
/// switches: with a session-wide switch the first access anywhere would have
/// dropped every unread concept's eviction score at once (on the Metal rig
/// snapshot that alone raised a first sweep's collections by roughly a quarter
/// on the cut as it stood before #29). [`score_live_plus_frequency`] adds the
/// frequency term per concept, so an access only raises the accessed concept's
/// score. [`score`], which recall ranking, the daemon score table and
/// canonization Stage 1 use, has no session-wide term either: there too an
/// access moves only the accessed concept's score.
/// Evidence and method: `dev-diary/notes/issue-30-access-count.md`.
pub fn score_over_live_dimensions(dims: ScoreDims, weights: &ScoringWeights) -> f64 {
    let w = weights.sanitized();
    let live_total = w.recency + w.session_activity + w.density;
    let weighted = if live_total > 0.0 {
        (clamp_dim(dims.recency) * w.recency
            + clamp_dim(dims.session_activity) * w.session_activity
            + clamp_dim(dims.density) * w.density)
            / live_total
    } else {
        0.0
    };
    finite_or_zero(weighted + bonus_and_modifier(dims))
}

/// GC's step-2 eviction composite (issue #29): the live-dimension score
/// ([`score_over_live_dimensions`]) **plus** the frequency term at its spec
/// weight, `w.frequency × clamp(frequency)`, under the same `[0, 1 +
/// MAX_BONUS]` bound every composite here has.
///
/// GC's cut used to switch the whole session from the live-dimension score to
/// the full composite the moment any concept recorded an access (ALGO-1).
/// Under #29's time-anchored recency that switch made every unread concept
/// ~20% easier to collect at once: on the Metal rig snapshot one access on one
/// Entity took the first sweep from 159 to 412 candidates, and the untouched
/// end state from 1,468 to 2,034. The additive form has no session-wide state:
///
/// * a concept with `frequency == 0` scores exactly its live-dimension score,
///   the scale `crate::daemon::gc::MIN_CONCEPT_SCORE` and the one-year window
///   were calibrated on, however many other concepts have been read;
/// * reading a concept can only raise its own score (frequency is
///   non-negative, and its `last_accessed` only raises GC's recency).
///
/// It is not the spec composite (the weighted part can exceed 1 before the
/// clamp), so it is **only** for GC's threshold comparison. Recall ranking,
/// the daemon's score table and canonization keep [`score`].
pub fn score_live_plus_frequency(dims: ScoreDims, weights: &ScoringWeights) -> f64 {
    let w = weights.sanitized();
    let live = score_over_live_dimensions(dims, &w);
    finite_or_zero(live + clamp_dim(dims.frequency) * w.frequency)
}

/// The two additive terms, each clamped to its defined range.
fn bonus_and_modifier(dims: ScoreDims) -> f64 {
    clamp_additive(dims.edge_type_bonus, 0.0, MAX_EDGE_BONUS)
        + clamp_additive(
            dims.concept_type_modifier,
            MIN_CONCEPT_MODIFIER,
            MAX_CONCEPT_MODIFIER,
        )
}

/// Clamp a composite into `[0, 1 + MAX_BONUS]`; a non-finite composite is
/// `0.0` (ALGO-10 — the composite is finite for every input, spec §5.7).
fn finite_or_zero(x: f64) -> f64 {
    if x.is_finite() {
        x.clamp(0.0, 1.0 + MAX_BONUS)
    } else {
        0.0
    }
}

/// Session-wide values shared by every concept's dimensions. Compute once per
/// rescore, not once per concept.
///
/// Compute it from the same graph state the concepts are then scored against
/// (it caches each concept's derivation count and the connectivity baseline):
/// a context that outlives a write to the graph scores against the old counts.
pub struct SessionContext {
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    total_interactions: usize,
    max_incident: usize,
    /// Per concept, how many interactions carry a `Derives` edge to it — the
    /// numerator of both activity formulas. Counted once for the whole session
    /// (one pass over the edge set) so scoring a concept is `O(its degree)`
    /// instead of a scan of every interaction per concept: a GC sweep scores
    /// every concept under the write lock, and the per-concept scan made that
    /// `O(concepts × interactions)`. A concept absent from the map is derived
    /// by none. One `Derives` edge per interaction is what the graph writes;
    /// the count is edges whose source is an interaction.
    derived_by: HashMap<NodeId, usize>,
    /// Highest [`Self::derivations`] over the session's concepts. The
    /// denominator of recall's `session_activity` (#180). Zero when the
    /// session has no concept, or none of them is derived. A `Derives` edge
    /// whose target is not a concept does not raise it.
    max_derived_by: usize,
}

/// A concept's incident-edge count for the `density` dimension: every
/// incident edge whose type [`EdgeType::counts_toward_density`] (all but
/// `Contradicts`, #19). The reference form [`incidence`] must agree with.
#[cfg(test)]
fn density_degree(graph: &Graph, node: NodeId) -> usize {
    graph
        .incident_edges(node)
        .iter()
        .filter(|e| e.edge_type.counts_toward_density())
        .count()
}

impl SessionContext {
    /// Aggregate the session's temporal extent and connectivity baselines.
    ///
    /// `max_incident` is the density degree (incident edges that
    /// [`EdgeType::counts_toward_density`]) of the session's most connected
    /// concept; density is normalized against it.
    pub fn compute(graph: &Graph) -> Self {
        let mut scratch = IncidenceScratch::default();
        let max_incident = graph
            .concepts()
            .map(|c| incidence(graph, c.id, &mut scratch).density_degree)
            .max()
            .unwrap_or(0);
        Self::with_max_incident(graph, max_incident)
    }

    /// [`SessionContext::compute`] with the connectivity baseline already
    /// known, for `rescore`, which has every concept's density degree in hand
    /// from its own pass (#171 B4).
    fn with_max_incident(graph: &Graph, max_incident: usize) -> Self {
        let interactions: Vec<_> = graph.interactions().collect();
        let total_interactions = interactions.len();
        let start = interactions
            .iter()
            .map(|i| i.created_at)
            .min()
            .unwrap_or_else(Utc::now);
        let end = interactions
            .iter()
            .map(|i| i.created_at)
            .max()
            .unwrap_or(start);
        let mut derived_by: HashMap<NodeId, usize> = HashMap::new();
        for e in graph.edges() {
            if e.edge_type == EdgeType::Derives
                && matches!(graph.node(e.source), Some(Node::Interaction(_)))
            {
                *derived_by.entry(e.target).or_default() += 1;
            }
        }
        // Over concepts only. A derivation aimed at something else must not
        // set the denominator the concepts are scored against.
        let max_derived_by = graph
            .concepts()
            .map(|c| derived_by.get(&c.id).copied().unwrap_or(0))
            .max()
            .unwrap_or(0);
        Self {
            start,
            end,
            total_interactions,
            max_incident,
            derived_by,
            max_derived_by,
        }
    }

    /// How many interactions derived `id`. Zero when none did.
    pub(crate) fn derivations(&self, id: NodeId) -> usize {
        self.derived_by.get(&id).copied().unwrap_or(0)
    }

    /// Highest [`Self::derivations`] over the session's concepts.
    pub(crate) fn max_derivations(&self) -> usize {
        self.max_derived_by
    }

    /// `derivations(id) / |interactions|`, the activity GC's step-2 cut still
    /// scores (#180).
    ///
    /// Recall and the daemon score table use the max-normalized value on
    /// [`ScoreDims::session_activity`]. This ratio is the scale
    /// [`crate::daemon::gc::MIN_CONCEPT_SCORE`] was calibrated on. A session
    /// with no interactions returns `0.0`.
    pub(crate) fn per_interaction_activity(&self, id: NodeId) -> f64 {
        if self.total_interactions == 0 {
            0.0
        } else {
            self.derivations(id) as f64 / self.total_interactions as f64
        }
    }
}

/// Recency of one touch against the session extent (issue #95).
///
/// `last_touch` is already the later of `last_accessed` and `created_at`.
/// The denominator is `max(span, MIN_RECENCY_SPAN)` in integer milliseconds.
/// When that `max` equals the span, this returns the historical
/// `(last_touch − start) / span` bits. `1 − (end − last_touch) / span` is the
/// same real number and a different `f64` for most positions, so a session
/// the floor does not widen must not take that form. When the floor does
/// widen it, the value is `1 − (end − last_touch) / MIN_RECENCY_SPAN`: a
/// short session scores near 1 instead of spreading a few milliseconds
/// across `[0, 1]`. Clamped to `[0, 1]`.
///
/// A session with no interactions has no extent (`start` falls back to the
/// wall clock), so it returns `1.0`, as the historical zero-span branch did,
/// rather than aging a concept against the wall clock.
fn concept_recency(last_touch: DateTime<Utc>, ctx: &SessionContext) -> f64 {
    if ctx.total_interactions == 0 {
        return 1.0;
    }
    let span_ms = (ctx.end - ctx.start).num_milliseconds();
    let denom = span_ms.max(MIN_RECENCY_SPAN);
    let recency = if denom == span_ms {
        (last_touch - ctx.start).num_milliseconds() as f64 / denom as f64
    } else {
        let age_ms = (ctx.end - last_touch).num_milliseconds();
        1.0 - age_ms as f64 / denom as f64
    };
    recency.clamp(0.0, 1.0)
}

/// What scoring needs from a concept's incident edges, from one walk of its
/// adjacency (#171 B4). Scoring used to walk it three times per concept, each
/// walk allocating and sorting: once for `max_incident`, once for the bonus
/// and once more for the density degree.
struct Incidence {
    /// Incident edges that [`EdgeType::counts_toward_density`].
    density_degree: usize,
    /// `Σ` [`edge_type_bonus_value`] over every incident edge, summed in edge-id
    /// order with `Iterator::sum`, exactly as [`Graph::incident_edges`] fed it
    /// before, so the bits do not move.
    edge_type_bonus: f64,
}

/// Reusable buffers for [`incidence`], one set per rescore.
#[derive(Default)]
struct IncidenceScratch {
    ids: Vec<NodeId>,
    types: Vec<EdgeType>,
}

fn incidence(graph: &Graph, node: NodeId, scratch: &mut IncidenceScratch) -> Incidence {
    graph.incident_edge_ids_into(node, &mut scratch.ids);
    scratch.types.clear();
    scratch.types.extend(
        scratch
            .ids
            .iter()
            .filter_map(|id| graph.edge(*id))
            .map(|e| e.edge_type),
    );
    Incidence {
        density_degree: scratch
            .types
            .iter()
            .filter(|t| t.counts_toward_density())
            .count(),
        edge_type_bonus: scratch
            .types
            .iter()
            .map(|t| edge_type_bonus_value(*t))
            .sum(),
    }
}

/// Compute the six dimension values for one concept from graph state.
pub fn score_concept(graph: &Graph, c: &Concept, ctx: &SessionContext) -> ScoreDims {
    let inc = incidence(graph, c.id, &mut IncidenceScratch::default());
    score_concept_with(c, ctx, &inc)
}

/// [`score_concept`] with the concept's [`Incidence`] already walked.
fn score_concept_with(c: &Concept, ctx: &SessionContext, inc: &Incidence) -> ScoreDims {
    // The later of the two: a concept is never "touched" before it existed. A
    // read stamped from a different clock than the write (issue #30 — reads
    // use the wall clock, `lambo demo` scripts its writes) must not move a
    // concept's recency backwards. Identical to `unwrap_or` whenever
    // `last_accessed >= created_at`, which every production stamp is.
    let last_touch = c
        .last_accessed
        .map_or(c.created_at, |t| t.max(c.created_at));
    let recency = concept_recency(last_touch, ctx);

    let frequency = c.access_count as f64 / FREQUENCY_NORMALIZER;

    let derived_by = ctx.derivations(c.id);
    // Max-normalized, like density (#180). `max_derived_by == 0` is an empty
    // derivation set: return 0 rather than dividing into NaN. `score` still
    // clamps a non-finite dimension to 0.
    let session_activity = if ctx.max_derived_by == 0 {
        0.0
    } else {
        derived_by as f64 / ctx.max_derived_by as f64
    };

    let density = if ctx.max_incident == 0 {
        0.0
    } else {
        inc.density_degree as f64 / ctx.max_incident as f64
    };

    ScoreDims {
        recency,
        frequency,
        session_activity,
        density,
        edge_type_bonus: inc.edge_type_bonus,
        concept_type_modifier: concept_type_modifier(c.concept_type),
    }
}

/// Rescore every concept in the session, returning a score-descending ranked
/// list (the daemon's score table), ties broken by canonical key ascending
/// then `NodeId` ascending ([`tie_break_by_key`]).
///
/// Deterministic for a given graph AND across runs: the canonical key is
/// persisted, so equal-score concepts keep one order no matter which ids a run
/// minted (issue #2), and no wall-clock value enters the formula.
///
/// Each concept's incident edges are walked once, into reused buffers, and
/// the walk serves both the session's connectivity baseline and the
/// concept's own score (#171 B4); the result is bit-identical to scoring
/// every concept with [`score_concept`].
pub fn rescore(graph: &Graph, weights: &ScoringWeights) -> Vec<Scored<NodeId>> {
    let mut scratch = IncidenceScratch::default();
    let walked: Vec<(&Concept, Incidence)> = graph
        .concepts()
        .map(|c| (c, incidence(graph, c.id, &mut scratch)))
        .collect();
    let max_incident = walked
        .iter()
        .map(|(_, inc)| inc.density_degree)
        .max()
        .unwrap_or(0);
    let ctx = SessionContext::with_max_incident(graph, max_incident);
    let key = |id: NodeId| match graph.node(id) {
        Some(crate::types::Node::Concept(c)) => Some(c.canonical_key.as_str()),
        _ => None,
    };
    let mut ranked: Vec<Scored<NodeId>> = walked
        .iter()
        .map(|(c, inc)| Scored::new(c.id, score(score_concept_with(c, &ctx, inc), weights)))
        .collect();
    ranked.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| tie_break_by_key(key(a.item), &a.item, key(b.item), &b.item))
    });
    ranked
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Graph;
    use crate::types::{AgentId, CanonizationStatus, SessionId};
    use chrono::{TimeZone, Utc};
    use uuid::Uuid;

    fn ts(m: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000 + m * 60, 0).unwrap()
    }

    fn sid() -> SessionId {
        SessionId::from("t4.1-score")
    }

    fn interaction(id: u64, prev: Option<u64>, at: i64) -> crate::types::Interaction {
        crate::types::Interaction {
            event_time: None,
            id: NodeId(Uuid::from_u64_pair(0, id)),
            session_id: sid(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: Some("p".into()),
            previous_id: prev.map(|p| NodeId(Uuid::from_u64_pair(0, p))),
            created_at: ts(at),
        }
    }

    fn concept(id: u64, origin: NodeId, content: &str, at: i64) -> Concept {
        Concept {
            id: NodeId(Uuid::from_u64_pair(1, id)),
            session_id: sid(),
            content: content.into(),
            canonical_key: content.to_string(),
            concept_type: ConceptType::Entity,
            origin_interaction: origin,
            origin_agent: AgentId::from("agent-a"),
            created_at: ts(at),
            access_count: 0,
            last_accessed: None,
            gc_survived: 0,
            canonization_status: CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: None,
            embedding: None,
            human_confirmed: 0,
            embedding_source: None,
            chunk_group_id: None,
        }
    }

    fn graph_with_two_concepts() -> (Graph, NodeId, NodeId) {
        let mut g = Graph::new(sid());
        // Two interactions (t=0, t=10) give the session a 10-minute extent.
        let i1 = interaction(1, None, 0);
        let i2 = interaction(2, Some(1), 10);
        let iid = i1.id;
        g.insert_interaction(i1).unwrap();
        g.insert_interaction(i2).unwrap();
        let c1 = concept(1, iid, "user schema", 0);
        let c1_id = c1.id;
        g.insert_concept(c1, iid).unwrap();
        let c2 = concept(2, iid, "auth middleware", 10);
        let c2_id = c2.id;
        g.insert_concept(c2, iid).unwrap();
        (g, c1_id, c2_id)
    }

    /// The pre-optimization `score_concept`, kept as a test oracle:
    /// `session_activity`'s numerator is found by scanning every interaction
    /// for a `Derives` edge to the concept (`O(interactions)` per concept).
    /// The denominator is the session maximum (#180). [`score_concept`] reads
    /// both from the [`SessionContext`]; [`assert_scores_match_reference`]
    /// checks the maximum against this same scan. The two scores must agree
    /// bit for bit. The oracle's job is that scan, not a second recency
    /// formula.
    fn score_concept_reference(graph: &Graph, c: &Concept, ctx: &SessionContext) -> ScoreDims {
        // Same touch rule and the same recency as [`score_concept`]. The
        // oracle's job is the per-concept `session_activity` scan, not a
        // second formula.
        let last_touch = c
            .last_accessed
            .map_or(c.created_at, |t| t.max(c.created_at));
        let recency = concept_recency(last_touch, ctx);
        let frequency = c.access_count as f64 / FREQUENCY_NORMALIZER;
        let derived_by = graph
            .interactions()
            .filter(|i| graph.edge_between(i.id, c.id, EdgeType::Derives).is_some())
            .count();
        let session_activity = if ctx.max_derivations() == 0 {
            0.0
        } else {
            derived_by as f64 / ctx.max_derivations() as f64
        };
        let incident = graph.incident_edges(c.id);
        let density = if ctx.max_incident == 0 {
            0.0
        } else {
            density_degree(graph, c.id) as f64 / ctx.max_incident as f64
        };
        let edge_type_bonus = incident
            .iter()
            .map(|e| edge_type_bonus_value(e.edge_type))
            .sum();
        ScoreDims {
            recency,
            frequency,
            session_activity,
            density,
            edge_type_bonus,
            concept_type_modifier: concept_type_modifier(c.concept_type),
        }
    }

    /// The pre-#171 `rescore`, kept as an oracle: the baseline from a
    /// `density_degree` walk per concept, then [`score_concept_reference`]
    /// per concept, then the same sort.
    fn rescore_reference(graph: &Graph, weights: &ScoringWeights) -> Vec<Scored<NodeId>> {
        // Three walks per concept, as before #171: this baseline pass, then
        // `incident_edges` and `density_degree` inside the scoring.
        let max_incident = graph
            .concepts()
            .map(|c| density_degree(graph, c.id))
            .max()
            .unwrap_or(0);
        let ctx = SessionContext::with_max_incident(graph, max_incident);
        let key = |id: NodeId| match graph.node(id) {
            Some(Node::Concept(c)) => Some(c.canonical_key.as_str()),
            _ => None,
        };
        let mut ranked: Vec<Scored<NodeId>> = graph
            .concepts()
            .map(|c| {
                Scored::new(
                    c.id,
                    score(score_concept_reference(graph, c, &ctx), weights),
                )
            })
            .collect();
        ranked.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| tie_break_by_key(key(a.item), &a.item, key(b.item), &b.item))
        });
        ranked
    }

    /// #171 B4: the single-walk `rescore` is the reference's table, bit for
    /// bit and in the same order.
    fn assert_rescore_matches_reference(g: &Graph) {
        let max_incident = g
            .concepts()
            .map(|c| density_degree(g, c.id))
            .max()
            .unwrap_or(0);
        assert_eq!(
            SessionContext::compute(g).max_incident,
            max_incident,
            "connectivity baseline"
        );
        let weights = ScoringWeights::default();
        let fast = rescore(g, &weights);
        let reference = rescore_reference(g, &weights);
        assert_eq!(fast.len(), reference.len());
        for (a, b) in fast.iter().zip(&reference) {
            assert_eq!(a.item, b.item);
            assert_eq!(a.score.to_bits(), b.score.to_bits(), "{}", a.item);
        }
    }

    /// A generated session: `n` concepts over a few interactions, with
    /// deterministic edges of every concept-to-concept type between them,
    /// `Contradicts` and self-loops included, and skewed degrees so the
    /// connectivity baseline is one hub's.
    fn generated_session(n: u64) -> Graph {
        let mut g = Graph::new(sid());
        let interactions = 8;
        for i in 1..=interactions {
            g.insert_interaction(interaction(i, (i > 1).then(|| i - 1), i as i64 * 3))
                .unwrap();
        }
        let types = [
            EdgeType::CoOccurrence,
            EdgeType::Causal,
            EdgeType::Dependency,
            EdgeType::Hierarchical,
            EdgeType::Semantic,
            EdgeType::Contradicts,
        ];
        let mut s = 0x5EED_u64;
        let mut next = move || {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            s >> 33
        };
        for c in 0..n {
            let origin = NodeId(Uuid::from_u64_pair(0, 1 + c % interactions));
            let mut concept = concept(c, origin, &format!("concept {c}"), (c % 20) as i64);
            concept.access_count = (next() % 7) as i32;
            concept.concept_type = match c % 3 {
                0 => ConceptType::Entity,
                1 => ConceptType::Logic,
                _ => ConceptType::Constraint,
            };
            g.insert_concept(concept, origin).unwrap();
        }
        let id = |c: u64| NodeId(Uuid::from_u64_pair(1, c));
        for e in 0..n * 3 {
            let src = next() % n;
            // Skew: a third of the edges land on the first ten concepts.
            let tgt = if e % 3 == 0 {
                next() % 10.min(n)
            } else {
                next() % n
            };
            let ty = types[(next() % types.len() as u64) as usize];
            let edge = crate::types::Edge {
                event_time: None,
                id: NodeId(Uuid::from_u64_pair(7, next())),
                session_id: sid(),
                source: id(src),
                target: id(tgt),
                edge_type: ty,
                weight: 0.5,
                reinforcements: 1,
                created_at: ts(0),
                last_reinforced: ts(0),
            };
            // A rejected edge (a type the graph refuses for this pair) is
            // simply not part of the fixture.
            let _ = g.upsert_edge(edge);
        }
        g
    }

    #[test]
    fn single_walk_rescore_matches_the_reference_bit_for_bit() {
        for n in [1, 2, 10, 97, 400] {
            let g = generated_session(n);
            assert_scores_match_reference(&g);
            assert_rescore_matches_reference(&g);
        }
        let (g, _, _) = graph_with_two_concepts();
        assert_rescore_matches_reference(&g);
        assert_rescore_matches_reference(&Graph::new(sid()));
    }

    /// `cargo test --release --features fixtures --lib \
    ///  daemon::score::tests::rescore_timing -- --ignored --nocapture`
    #[test]
    #[ignore = "measurement; run in release with --nocapture"]
    fn rescore_timing() {
        for n in [5_000, 9_100] {
            let g = generated_session(n);
            let weights = ScoringWeights::default();
            let time = |name: &str, f: &dyn Fn() -> Vec<Scored<NodeId>>| {
                let _ = f();
                let runs = 20;
                let start = std::time::Instant::now();
                for _ in 0..runs {
                    std::hint::black_box(f());
                }
                println!(
                    "rescore_timing {n} concepts, {name}: {:?} per rescore",
                    start.elapsed() / runs
                );
            };
            time("pre-#171 (three walks per concept)", &|| {
                rescore_reference(&g, &weights)
            });
            time("single walk (#171)", &|| rescore(&g, &weights));
        }
    }

    fn assert_scores_match_reference(g: &Graph) {
        let ctx = SessionContext::compute(g);
        let scanned_max = g
            .concepts()
            .map(|c| {
                g.interactions()
                    .filter(|i| g.edge_between(i.id, c.id, EdgeType::Derives).is_some())
                    .count()
            })
            .max()
            .unwrap_or(0);
        assert_eq!(
            ctx.max_derivations(),
            scanned_max,
            "the activity denominator is the scanned maximum"
        );
        let mut n = 0;
        for c in g.concepts() {
            let (a, b) = (
                score_concept(g, c, &ctx),
                score_concept_reference(g, c, &ctx),
            );
            for (name, x, y) in [
                ("recency", a.recency, b.recency),
                ("frequency", a.frequency, b.frequency),
                ("session_activity", a.session_activity, b.session_activity),
                ("density", a.density, b.density),
                ("edge_type_bonus", a.edge_type_bonus, b.edge_type_bonus),
                (
                    "concept_type_modifier",
                    a.concept_type_modifier,
                    b.concept_type_modifier,
                ),
            ] {
                assert_eq!(x.to_bits(), y.to_bits(), "{} {name}: {x} vs {y}", c.content);
            }
            n += 1;
        }
        assert!(n > 0, "the fixture must have concepts");
    }

    /// The once-per-sweep derived-by count equals the per-concept scan:
    /// concepts derived by one, several and no interaction (a hand-built
    /// concept with no `Derives` edge at all), a
    /// concept-to-concept edge into the same concept (not a derivation).
    #[test]
    fn score_concept_matches_the_per_concept_scan_on_a_hand_built_session() {
        let (mut g, c1, c2) = graph_with_two_concepts();
        let i1 = NodeId(Uuid::from_u64_pair(0, 1));
        let i2 = NodeId(Uuid::from_u64_pair(0, 2));
        let i3 = interaction(3, Some(2), 25);
        let i3_id = i3.id;
        g.insert_interaction(i3).unwrap();
        let edge = |id: u64, src: NodeId, tgt: NodeId, ty: EdgeType| crate::types::Edge {
            event_time: None,
            id: NodeId(Uuid::from_u64_pair(5, id)),
            session_id: sid(),
            source: src,
            target: tgt,
            edge_type: ty,
            weight: 0.9,
            reinforcements: 1,
            created_at: ts(0),
            last_reinforced: ts(0),
        };
        // c1 is derived by i1 (insert_concept) and again by i2 and i3.
        g.upsert_edge(edge(1, i2, c1, EdgeType::Derives)).unwrap();
        g.upsert_edge(edge(2, i3_id, c1, EdgeType::Derives))
            .unwrap();
        // A concept-to-concept edge adds density but no derivation.
        g.upsert_edge(edge(3, c1, c2, EdgeType::Causal)).unwrap();
        // A concept with no provenance edge at all.
        let lone = concept(9, i1, "no provenance", 5);
        g.insert_concept(lone, i1).unwrap();
        let lone_id = NodeId(Uuid::from_u64_pair(1, 9));
        g.remove_edge(g.edge_between(i1, lone_id, EdgeType::Derives).unwrap().id)
            .unwrap();
        assert_scores_match_reference(&g);

        let ctx = SessionContext::compute(&g);
        let c = |id: NodeId| match g.node(id) {
            Some(Node::Concept(c)) => c.clone(),
            _ => unreachable!(),
        };
        // Recall: count / session maximum (3). GC: count / interactions (3).
        assert_eq!(ctx.max_derivations(), 3);
        assert_eq!(ctx.total_interactions, 3);
        assert_eq!(score_concept(&g, &c(c1), &ctx).session_activity, 1.0);
        assert_eq!(score_concept(&g, &c(c2), &ctx).session_activity, 1.0 / 3.0);
        assert_eq!(score_concept(&g, &c(lone_id), &ctx).session_activity, 0.0);
        assert_eq!(ctx.per_interaction_activity(c1), 1.0);
        assert_eq!(ctx.per_interaction_activity(c2), 1.0 / 3.0);
        assert_eq!(ctx.per_interaction_activity(lone_id), 0.0);
    }

    /// Issue #180. An empty graph has no derivations, so the dimension is 0
    /// rather than a division by zero.
    #[test]
    fn session_activity_is_zero_on_an_empty_session() {
        let g = Graph::new(sid());
        let ctx = SessionContext::compute(&g);
        assert_eq!(ctx.total_interactions, 0);
        assert_eq!(ctx.max_derivations(), 0);
        let absent = concept(1, NodeId(Uuid::from_u64_pair(0, 1)), "absent", 0);
        let activity = score_concept(&g, &absent, &ctx).session_activity;
        assert_eq!(activity, 0.0);
        assert!(activity.is_finite());
        assert_eq!(ctx.per_interaction_activity(absent.id), 0.0);
    }

    /// Issue #180. When every concept was derived once, each scores 1.0.
    /// Activity is then a constant, so it cannot reorder the table.
    #[test]
    fn session_activity_is_one_when_every_concept_is_derived_once() {
        let (g, c1, c2) = graph_with_two_concepts();
        let ctx = SessionContext::compute(&g);
        assert_eq!(ctx.max_derivations(), 1);
        assert_eq!(ctx.total_interactions, 2);
        let concept = |id: NodeId| match g.node(id) {
            Some(Node::Concept(c)) => c.clone(),
            _ => unreachable!(),
        };
        let d1 = score_concept(&g, &concept(c1), &ctx);
        let d2 = score_concept(&g, &concept(c2), &ctx);
        assert_eq!(d1.session_activity, 1.0);
        assert_eq!(d2.session_activity, 1.0);
        // The old ratio would have been 1/2. GC still uses that.
        assert_eq!(ctx.per_interaction_activity(c1), 0.5);
        assert_eq!(ctx.per_interaction_activity(c2), 0.5);
    }

    /// Issue #180. Activity is the derivation count over the session maximum,
    /// and it rises strictly with that count.
    #[test]
    fn session_activity_is_max_normalized_and_monotonic() {
        let mut g = Graph::new(sid());
        let mut prev = None;
        let mut ids = Vec::new();
        // Six interactions, and the busiest concept is derived by only four
        // of them, so `k/4` (recall) and `k/6` (GC) are different scales.
        for i in 1..=6 {
            let interaction = interaction(i, prev, i as i64 * 10);
            prev = Some(i);
            ids.push(interaction.id);
            g.insert_interaction(interaction).unwrap();
        }
        let origin = ids[0];
        // Derived 4, 2, 1 and 0 times. The zero is an inserted concept whose
        // provenance edge was removed.
        let heavy = concept(1, origin, "heavy", 0);
        let mid = concept(2, origin, "mid", 0);
        let once = concept(3, origin, "once", 0);
        let none = concept(4, origin, "none", 0);
        let (heavy_id, mid_id, once_id, none_id) = (heavy.id, mid.id, once.id, none.id);
        for c in [heavy, mid, once, none] {
            g.insert_concept(c, origin).unwrap();
        }
        let edge = |id: u64, src: NodeId, tgt: NodeId| crate::types::Edge {
            event_time: None,
            id: NodeId(Uuid::from_u64_pair(5, id)),
            session_id: sid(),
            source: src,
            target: tgt,
            edge_type: EdgeType::Derives,
            weight: 0.9,
            reinforcements: 1,
            created_at: ts(0),
            last_reinforced: ts(0),
        };
        // insert_concept already derived each once from `origin` (ids[0]).
        g.upsert_edge(edge(1, ids[1], heavy_id)).unwrap();
        g.upsert_edge(edge(2, ids[2], heavy_id)).unwrap();
        g.upsert_edge(edge(3, ids[3], heavy_id)).unwrap();
        g.upsert_edge(edge(4, ids[1], mid_id)).unwrap();
        g.remove_edge(
            g.edge_between(origin, none_id, EdgeType::Derives)
                .unwrap()
                .id,
        )
        .unwrap();

        let ctx = SessionContext::compute(&g);
        assert_eq!(ctx.max_derivations(), 4);
        assert_eq!(ctx.total_interactions, 6);
        let activity = |id: NodeId| {
            let c = match g.node(id) {
                Some(Node::Concept(c)) => c.clone(),
                _ => unreachable!(),
            };
            score_concept(&g, &c, &ctx).session_activity
        };
        let got = [
            activity(none_id),
            activity(once_id),
            activity(mid_id),
            activity(heavy_id),
        ];
        assert_eq!(got, [0.0, 0.25, 0.5, 1.0]);
        for w in got.windows(2) {
            assert!(w[0] < w[1], "activity must rise with derived_by: {got:?}");
        }
        // GC still divides by the interaction count. That matches recall at
        // 0 derivations only; a concept derived once is 1/4 on the table and
        // 1/6 on the cut.
        assert_eq!(ctx.per_interaction_activity(none_id), 0.0);
        assert_eq!(ctx.per_interaction_activity(once_id), 1.0 / 6.0);
        assert_eq!(ctx.per_interaction_activity(mid_id), 2.0 / 6.0);
        assert_eq!(ctx.per_interaction_activity(heavy_id), 4.0 / 6.0);
        assert_ne!(ctx.per_interaction_activity(once_id), activity(once_id));
    }

    /// The shipped fixture session scores identically under both.
    #[cfg(feature = "fixtures")]
    #[test]
    fn score_concept_matches_the_per_concept_scan_on_the_fixture_session() {
        for name in ["session-rest-api", "session-drift"] {
            let snap = crate::fixtures::load_snapshot(name).expect("fixture");
            let g = Graph::from_snapshot(snap).unwrap();
            assert_scores_match_reference(&g);
            assert_rescore_matches_reference(&g);
        }
    }

    // ------------------------------------------------------------------
    // Pure score function — clamp / bound / NaN rules (spec §9)
    // ------------------------------------------------------------------

    #[test]
    fn score_is_bounded_and_finite_for_bounded_inputs() {
        let values = [0.0, 0.25, 0.5, 1.0, 2.0, -1.0, 1e300, -1e300];
        for &r in &values {
            for &f in &values {
                for &s in &values {
                    for &d in &values {
                        let dims = ScoreDims {
                            recency: r,
                            frequency: f,
                            session_activity: s,
                            density: d,
                            edge_type_bonus: 0.1,
                            concept_type_modifier: 0.05,
                        };
                        let out = dims.composite();
                        assert!(out.is_finite(), "non-finite for {dims:?}");
                        assert!(
                            (0.0..=1.0 + MAX_BONUS).contains(&out),
                            "out of bound for {dims:?}: {out}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn score_bounded_above_by_weights_plus_bonus_and_modifier() {
        let dims = ScoreDims {
            recency: 1.0,
            frequency: 1.0,
            session_activity: 1.0,
            density: 1.0,
            edge_type_bonus: 10.0, // way over the cap
            concept_type_modifier: 10.0,
        };
        let out = dims.composite();
        assert_eq!(out, 1.0 + MAX_BONUS);
    }

    #[test]
    fn monotonic_in_each_dimension() {
        let weights = ScoringWeights::default();
        // Sweep each dimension upward holding the others fixed at 0.5; with
        // all default weights positive the composite must strictly increase
        // on every step. If a weighted dimension were dropped from the
        // composite, `out == prev` exactly and the strict assertion fires.
        for (dim, field) in [
            ("recency", 0usize),
            ("frequency", 1),
            ("session_activity", 2),
            ("density", 3),
        ] {
            let mut prev = f64::NEG_INFINITY;
            for v in (0..=20).map(|i| i as f64 / 20.0) {
                let mut dims = ScoreDims {
                    recency: 0.5,
                    frequency: 0.5,
                    session_activity: 0.5,
                    density: 0.5,
                    edge_type_bonus: 0.1,
                    concept_type_modifier: 0.05,
                };
                match field {
                    0 => dims.recency = v,
                    1 => dims.frequency = v,
                    2 => dims.session_activity = v,
                    _ => dims.density = v,
                }
                let out = score(dims, &weights);
                assert!(
                    out > prev,
                    "{dim} did not strictly increase at {v}: {out} <= {prev}"
                );
                prev = out;
            }
        }
    }

    /// ALGO-10: a garbage **weight** (they arrive from TOML, where `NaN` is
    /// admissible) must degrade its own dimension to zero, never poison the
    /// composite. Pre-fix the weight multiplied straight into the sum, so the
    /// composite was `NaN` — which ranks as garbage and makes GC's
    /// `score < threshold` test silently `false`.
    #[test]
    fn non_finite_weights_zero_their_dimension_and_keep_the_composite_finite() {
        let dims = ScoreDims {
            recency: 1.0,
            frequency: 1.0,
            session_activity: 1.0,
            density: 1.0,
            edge_type_bonus: 0.1,
            concept_type_modifier: 0.05,
        };
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
            let poisoned = ScoringWeights {
                recency: bad,
                ..ScoringWeights::default()
            };
            let out = score(dims, &poisoned);
            assert!(out.is_finite(), "recency weight {bad} produced {out}");
            // The surviving dimensions still count: zeroing recency costs
            // exactly its own contribution, nothing more.
            assert_eq!(
                out,
                score(
                    dims,
                    &ScoringWeights {
                        recency: 0.0,
                        ..ScoringWeights::default()
                    }
                ),
                "weight {bad} must behave as 0.0"
            );
            // The comparison GC makes is a real comparison again.
            assert!((0.0..1.0 + MAX_BONUS).contains(&out));
        }
        assert!(!ScoringWeights {
            recency: f64::NAN,
            ..ScoringWeights::default()
        }
        .is_valid());
        assert!(ScoringWeights::default().is_valid());
    }

    /// Issue #29: GC's composite is the live score plus the concept's own
    /// frequency term — identical to the live score at frequency 0, strictly
    /// above it once the concept earns frequency, monotone in frequency,
    /// bounded like every composite, and finite for poisoned input.
    #[test]
    fn live_plus_frequency_adds_only_the_concepts_own_frequency() {
        let w = ScoringWeights::default();
        let dims = ScoreDims {
            recency: 0.3,
            frequency: 0.0,
            session_activity: 0.1,
            density: 0.2,
            edge_type_bonus: 0.02,
            concept_type_modifier: -0.1,
        };
        let live = score_over_live_dimensions(dims, &w);
        assert_eq!(
            score_live_plus_frequency(dims, &w).to_bits(),
            live.to_bits()
        );
        let mut prev = live;
        for f in [0.1, 0.5, 1.0, 7.0] {
            let s = score_live_plus_frequency(
                ScoreDims {
                    frequency: f,
                    ..dims
                },
                &w,
            );
            assert!(s > prev || (f > 1.0 && s == prev), "f={f}: {s} vs {prev}");
            prev = s;
        }
        let one = score_live_plus_frequency(
            ScoreDims {
                frequency: 1.0,
                ..dims
            },
            &w,
        );
        assert!((one - (live + w.frequency)).abs() < 1e-12);
        let maxed = ScoreDims {
            recency: 1.0,
            frequency: 1.0,
            session_activity: 1.0,
            density: 1.0,
            edge_type_bonus: MAX_EDGE_BONUS,
            concept_type_modifier: MAX_CONCEPT_MODIFIER,
        };
        assert_eq!(score_live_plus_frequency(maxed, &w), 1.0 + MAX_BONUS);
        let poisoned = ScoreDims {
            frequency: f64::NAN,
            ..dims
        };
        assert_eq!(
            score_live_plus_frequency(poisoned, &w).to_bits(),
            live.to_bits()
        );
    }

    /// ALGO-1: the live-dimension composite drops `frequency` and renormalizes
    /// over the surviving weights, so it stays on the same `[0,1]`+bonus scale
    /// and lifts every concept whose only missing dimension is the dead one.
    #[test]
    fn live_dimension_score_renormalizes_over_surviving_weights() {
        let w = ScoringWeights::default();
        let dims = ScoreDims {
            recency: 0.0,
            frequency: 0.0,
            session_activity: 0.1,
            density: 0.2,
            edge_type_bonus: 0.02,
            concept_type_modifier: 0.05,
        };
        let live = score_over_live_dimensions(dims, &w);
        let full = score(dims, &w);
        assert!(
            live > full,
            "excluding a dead 20% weight must raise the score: {live} vs {full}"
        );
        // Exactly the renormalization, not an arbitrary boost.
        let expected = (0.1 * w.session_activity + 0.2 * w.density)
            / (w.recency + w.session_activity + w.density)
            + 0.02
            + 0.05;
        assert!((live - expected).abs() < 1e-12, "{live} != {expected}");

        // The renormalization is a multiplication by 1/live_total, so at
        // frequency 0 the switch back to `score` costs exactly `live_total`
        // (0.8 by default) on the weighted part — NEW-6: it LOWERS the score,
        // it does not raise it.
        let weighted_live = live - 0.07;
        let weighted_full = full - 0.07;
        let live_total = w.recency + w.session_activity + w.density;
        assert!(
            (weighted_full - weighted_live * live_total).abs() < 1e-12,
            "full weighted part must be live × {live_total}: {weighted_full} vs \
             {weighted_live}"
        );

        // A saturated concept still tops out at the same bound; only a concept
        // whose frequency is genuinely earning comes out ahead of the live score.
        let maxed = ScoreDims {
            recency: 1.0,
            frequency: 1.0,
            session_activity: 1.0,
            density: 1.0,
            edge_type_bonus: MAX_EDGE_BONUS,
            concept_type_modifier: MAX_CONCEPT_MODIFIER,
        };
        assert_eq!(score_over_live_dimensions(maxed, &w), 1.0 + MAX_BONUS);

        // All-zero weights cannot divide by zero.
        let zeroed = ScoringWeights {
            recency: 0.0,
            frequency: 0.0,
            session_activity: 0.0,
            density: 0.0,
        };
        let out = score_over_live_dimensions(dims, &zeroed);
        assert!(out.is_finite(), "zero weight total produced {out}");
        assert!(
            (out - 0.07).abs() < 1e-12,
            "bonus + modifier only, got {out}"
        );
    }

    #[test]
    fn nan_and_inf_dimension_counts_as_zero() {
        let weights = ScoringWeights::default();
        let base = ScoreDims {
            recency: 0.5,
            frequency: 0.5,
            session_activity: 0.5,
            density: 0.5,
            edge_type_bonus: 0.1,
            concept_type_modifier: 0.05,
        };
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut dims = base;
            dims.recency = bad;
            assert_eq!(
                score(dims, &weights),
                score(
                    ScoreDims {
                        recency: 0.0,
                        ..base
                    },
                    &weights
                ),
                "recency {bad} must clamp to 0.0"
            );

            let mut dims = base;
            dims.frequency = bad;
            assert_eq!(
                score(dims, &weights),
                score(
                    ScoreDims {
                        frequency: 0.0,
                        ..base
                    },
                    &weights
                ),
                "frequency {bad} must clamp to 0.0"
            );

            let mut dims = base;
            dims.session_activity = bad;
            assert_eq!(
                score(dims, &weights),
                score(
                    ScoreDims {
                        session_activity: 0.0,
                        ..base
                    },
                    &weights
                ),
                "session_activity {bad} must clamp to 0.0"
            );

            let mut dims = base;
            dims.density = bad;
            assert_eq!(
                score(dims, &weights),
                score(
                    ScoreDims {
                        density: 0.0,
                        ..base
                    },
                    &weights
                ),
                "density {bad} must clamp to 0.0"
            );

            let mut dims = base;
            dims.edge_type_bonus = bad;
            assert_eq!(
                score(dims, &weights),
                score(
                    ScoreDims {
                        edge_type_bonus: 0.0,
                        ..base
                    },
                    &weights
                ),
                "edge_type_bonus {bad} must clamp to 0.0"
            );

            let mut dims = base;
            dims.concept_type_modifier = bad;
            assert_eq!(
                score(dims, &weights),
                score(
                    ScoreDims {
                        concept_type_modifier: 0.0,
                        ..base
                    },
                    &weights
                ),
                "concept_type_modifier {bad} must clamp to 0.0"
            );
        }
    }

    #[test]
    fn concept_type_modifier_matches_p1_multipliers() {
        let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
        assert!(close(concept_type_modifier(ConceptType::Entity), 0.05));
        assert!(close(concept_type_modifier(ConceptType::Constraint), 0.15));
        assert!(close(
            concept_type_modifier(ConceptType::Observation),
            -0.10
        ));
    }

    #[test]
    fn edge_type_bonus_table_provenance_is_zero_structural_is_positive() {
        assert_eq!(edge_type_bonus_value(EdgeType::Derives), 0.0);
        assert_eq!(edge_type_bonus_value(EdgeType::Temporal), 0.0);
        assert_eq!(edge_type_bonus_value(EdgeType::Contradicts), 0.0);
        for ty in [
            EdgeType::Causal,
            EdgeType::Dependency,
            EdgeType::Hierarchical,
            EdgeType::Semantic,
            EdgeType::CoOccurrence,
        ] {
            assert!(edge_type_bonus_value(ty) > 0.0, "{ty:?} must carry bonus");
        }
    }

    // ------------------------------------------------------------------
    // Graph-derived dimensions — direction and normalization
    // ------------------------------------------------------------------

    #[test]
    fn recency_is_monotone_in_created_at_and_clamped() {
        let (g, c1_id, c2_id) = graph_with_two_concepts();
        // c1 at t=0 (session start), c2 at t=10 (session end) → recency 0 vs 1.
        let ctx = SessionContext::compute(&g);
        let c1 = match g.node(c1_id).unwrap() {
            crate::types::Node::Concept(c) => c.clone(),
            _ => unreachable!(),
        };
        let c2 = match g.node(c2_id).unwrap() {
            crate::types::Node::Concept(c) => c.clone(),
            _ => unreachable!(),
        };
        let d1 = score_concept(&g, &c1, &ctx);
        let d2 = score_concept(&g, &c2, &ctx);
        assert_eq!(d1.recency, 0.0);
        assert_eq!(d2.recency, 1.0);
        assert!(d2.recency > d1.recency);
    }

    fn at_ms(ms: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000, 0).unwrap() + chrono::Duration::milliseconds(ms)
    }

    fn interaction_ms(id: u64, prev: Option<u64>, at: i64) -> crate::types::Interaction {
        crate::types::Interaction {
            created_at: at_ms(at),
            ..interaction(id, prev, 0)
        }
    }

    fn concept_ms(id: u64, origin: NodeId, content: &str, at: i64) -> Concept {
        Concept {
            created_at: at_ms(at),
            ..concept(id, origin, content, 0)
        }
    }

    /// Two interactions at `0` and `end_ms`, and one concept per touch time,
    /// each derived from the interaction that shares its timestamp when one
    /// does, otherwise from the first.
    fn session_with_touches(end_ms: i64, touches: &[(u64, &str, i64)]) -> (Graph, Vec<Concept>) {
        let mut g = Graph::new(sid());
        let i1 = interaction_ms(1, None, 0);
        let i2 = interaction_ms(2, Some(1), end_ms);
        let origin = i1.id;
        g.insert_interaction(i1).unwrap();
        g.insert_interaction(i2).unwrap();
        let mut concepts = Vec::new();
        for (id, content, at) in touches {
            let c = concept_ms(*id, origin, content, *at);
            g.insert_concept(c.clone(), origin).unwrap();
            concepts.push(c);
        }
        (g, concepts)
    }

    /// The pre-#95 recency expression, including the `last_touch` guard that
    /// stays. A span of zero yielded `1.0`.
    fn historical_recency(c: &Concept, ctx: &SessionContext) -> f64 {
        let last_touch = c
            .last_accessed
            .map_or(c.created_at, |t| t.max(c.created_at));
        let span_ms = (ctx.end - ctx.start).num_milliseconds();
        if span_ms == 0 {
            1.0
        } else {
            (last_touch - ctx.start).num_milliseconds() as f64 / span_ms as f64
        }
    }

    fn recall_final(daemon: f64, query: f64) -> f64 {
        let w = crate::config::RecallWeights::default();
        w.w_daemon * daemon + w.w_query * query
    }

    /// Issue #95. A 40ms stall between two otherwise identical derives, on a
    /// session younger than the floor. Query cosines 0.5 then 0.3 are a real
    /// gap (0.1 of the default 0.5/0.5 blend). On the historical formula the
    /// stall assigns recency 0 and 1, and 0.125 of final score flips that gap.
    #[test]
    fn a_tens_of_ms_stall_cannot_flip_a_real_query_gap() {
        let stall_ms = 40;
        let (g, concepts) =
            session_with_touches(stall_ms, &[(1, "established", 0), (2, "stalled", stall_ms)]);
        let ctx = SessionContext::compute(&g);
        assert!(
            (ctx.end - ctx.start).num_milliseconds() < MIN_RECENCY_SPAN,
            "the session has to be younger than the floor"
        );
        let established = score_concept(&g, &concepts[0], &ctx);
        let stalled = score_concept(&g, &concepts[1], &ctx);
        for (name, a, b) in [
            ("frequency", established.frequency, stalled.frequency),
            (
                "session_activity",
                established.session_activity,
                stalled.session_activity,
            ),
            ("density", established.density, stalled.density),
            (
                "edge_type_bonus",
                established.edge_type_bonus,
                stalled.edge_type_bonus,
            ),
            (
                "concept_type_modifier",
                established.concept_type_modifier,
                stalled.concept_type_modifier,
            ),
        ] {
            assert_eq!(a.to_bits(), b.to_bits(), "{name} must not differ");
        }
        let gap = stalled.recency - established.recency;
        assert!(
            gap > 0.0 && gap < 0.01,
            "the stall must barely move recency, got {gap} ({established:?} vs {stalled:?})"
        );
        let earlier = recall_final(established.composite(), 0.5);
        let later = recall_final(stalled.composite(), 0.3);
        assert!(
            earlier > later,
            "0.5 must stay ahead of 0.3: {earlier} vs {later}"
        );
    }

    /// A session at or above the floor matches the historical expression bit
    /// for bit, including exactly at the floor and at offsets where
    /// `1 - (end - last) / span` is a different `f64`.
    #[test]
    fn a_session_at_or_above_the_floor_matches_the_historical_bits() {
        for end_ms in [MIN_RECENCY_SPAN, 30 * 60 * 1_000, MIN_RECENCY_SPAN + 1] {
            let touches = [
                (1u64, "start", 0i64),
                (2, "one-ms", 1),
                (3, "awkward", 123_456.min(end_ms)),
                (4, "end", end_ms),
            ];
            let (mut g, concepts) = session_with_touches(end_ms, &touches);
            // A read stamped before creation must not move recency backwards.
            let mut backdated = concepts[2].clone();
            backdated.last_accessed = Some(at_ms(0));
            backdated.content = "backdated-read".into();
            backdated.canonical_key = "backdated-read".into();
            backdated.id = NodeId(Uuid::from_u64_pair(1, 99));
            g.insert_concept(backdated, concepts[2].origin_interaction)
                .unwrap();
            let ctx = SessionContext::compute(&g);
            assert!(
                (ctx.end - ctx.start).num_milliseconds() >= MIN_RECENCY_SPAN,
                "span {end_ms} must be at or above the floor"
            );
            for c in g.concepts() {
                let got = score_concept(&g, c, &ctx).recency;
                let historical = historical_recency(c, &ctx);
                assert_eq!(
                    got.to_bits(),
                    historical.to_bits(),
                    "span {end_ms} {}: {got} vs {historical}",
                    c.content
                );
            }
        }
    }

    /// The young-session formula meets the historical value at the floor.
    /// One millisecond under it, the oldest concept scores `1 - (floor-1)/floor`,
    /// which is about `1.7e-6`, not a jump.
    #[test]
    fn recency_is_continuous_at_the_floor() {
        let (at_floor, at_concepts) = session_with_touches(
            MIN_RECENCY_SPAN,
            &[(1, "oldest", 0), (2, "newest", MIN_RECENCY_SPAN)],
        );
        let ctx = SessionContext::compute(&at_floor);
        assert_eq!(score_concept(&at_floor, &at_concepts[0], &ctx).recency, 0.0);
        assert_eq!(score_concept(&at_floor, &at_concepts[1], &ctx).recency, 1.0);
        assert_eq!(
            score_concept(&at_floor, &at_concepts[0], &ctx)
                .recency
                .to_bits(),
            historical_recency(&at_concepts[0], &ctx).to_bits()
        );

        let just_under = MIN_RECENCY_SPAN - 1;
        let (below, below_concepts) =
            session_with_touches(just_under, &[(1, "oldest", 0), (2, "newest", just_under)]);
        let ctx = SessionContext::compute(&below);
        let oldest = score_concept(&below, &below_concepts[0], &ctx).recency;
        let expected = 1.0 - just_under as f64 / MIN_RECENCY_SPAN as f64;
        assert_eq!(oldest.to_bits(), expected.to_bits());
        assert!(
            oldest < 1e-5,
            "crossing the floor must not jump the oldest concept, got {oldest}"
        );
        assert_eq!(score_concept(&below, &below_concepts[1], &ctx).recency, 1.0);
    }

    /// A single instant is "now" without a zero-span branch. A concept
    /// backdated against that instant lands slightly under 1, which the old
    /// branch hid by returning 1.0 for every touch.
    #[test]
    fn a_single_instant_session_scores_one_unless_backdated() {
        let (g, concepts) = session_with_touches(0, &[(1, "now", 0)]);
        let ctx = SessionContext::compute(&g);
        assert_eq!((ctx.end - ctx.start).num_milliseconds(), 0);
        assert_eq!(score_concept(&g, &concepts[0], &ctx).recency, 1.0);

        let mut backdated = concepts[0].clone();
        backdated.created_at = at_ms(-40);
        backdated.id = NodeId(Uuid::from_u64_pair(1, 7));
        backdated.content = "earlier".into();
        backdated.canonical_key = "earlier".into();
        let mut g = g;
        g.insert_concept(backdated.clone(), concepts[0].origin_interaction)
            .unwrap();
        let ctx = SessionContext::compute(&g);
        let recency = score_concept(&g, &backdated, &ctx).recency;
        let expected = 1.0 - 40.0 / MIN_RECENCY_SPAN as f64;
        assert_eq!(recency.to_bits(), expected.to_bits());
        assert!(recency < 1.0);
    }

    /// The other tests read the floor symbolically, so a much shorter floor
    /// would still pass them. This pins 10 minutes: a 9-minute session sits
    /// under it, and its oldest concept scores exactly `1 − 540000/600000`.
    #[test]
    fn the_floor_is_ten_minutes() {
        assert_eq!(MIN_RECENCY_SPAN, 600_000);
        let nine_minutes = 9 * 60 * 1_000;
        let (g, concepts) = session_with_touches(
            nine_minutes,
            &[(1, "oldest", 0), (2, "newest", nine_minutes)],
        );
        let ctx = SessionContext::compute(&g);
        let oldest = score_concept(&g, &concepts[0], &ctx).recency;
        assert_eq!(
            oldest.to_bits(),
            (1.0 - 540_000.0_f64 / 600_000.0).to_bits()
        );
        assert_eq!(score_concept(&g, &concepts[1], &ctx).recency, 1.0);
    }

    /// A touch after the session's latest interaction is clamped to `1.0`,
    /// on the floor formula (a 40ms session) and on the historical division
    /// (exactly at the floor, and 30 minutes).
    #[test]
    fn a_touch_after_the_session_end_clamps_to_one() {
        for end_ms in [40, MIN_RECENCY_SPAN, 30 * 60 * 1_000] {
            let (g, concepts) = session_with_touches(end_ms, &[(1, "after", end_ms + 5_000)]);
            let ctx = SessionContext::compute(&g);
            assert_eq!((ctx.end - ctx.start).num_milliseconds(), end_ms);
            assert_eq!(
                score_concept(&g, &concepts[0], &ctx).recency,
                1.0,
                "span {end_ms}"
            );
        }
    }

    /// A touch before the session's start is clamped to `0.0`: on the
    /// historical division (any touch before `start`), and on the floor
    /// formula once the touch is more than the floor before `end`.
    #[test]
    fn a_touch_before_the_session_start_clamps_to_zero() {
        let long = 30 * 60 * 1_000;
        let (g, concepts) = session_with_touches(long, &[(1, "before", -5_000)]);
        let ctx = SessionContext::compute(&g);
        assert_eq!(score_concept(&g, &concepts[0], &ctx).recency, 0.0);
        assert!(historical_recency(&concepts[0], &ctx) < 0.0);

        let short = 40;
        let (g, concepts) =
            session_with_touches(short, &[(1, "before", short - MIN_RECENCY_SPAN - 5_000)]);
        let ctx = SessionContext::compute(&g);
        assert_eq!(score_concept(&g, &concepts[0], &ctx).recency, 0.0);
    }

    /// No interactions means no extent: `start` falls back to the wall
    /// clock. Recency is `1.0` there, as the historical zero-span branch
    /// returned, not an age measured against the wall clock.
    #[test]
    fn a_session_without_interactions_scores_recency_one() {
        let ctx = SessionContext::compute(&Graph::new(sid()));
        assert_eq!(ctx.total_interactions, 0);
        assert_eq!(concept_recency(at_ms(-3_600_000), &ctx), 1.0);
        assert_eq!(concept_recency(at_ms(0), &ctx), 1.0);
    }

    #[test]
    fn density_is_max_normalized() {
        let (g, c1_id, c2_id) = graph_with_two_concepts();
        // Both concepts have exactly one incident edge (Derives) → density 1.0
        // each (session max is 1).
        let ctx = SessionContext::compute(&g);
        let c1 = match g.node(c1_id).unwrap() {
            crate::types::Node::Concept(c) => c.clone(),
            _ => unreachable!(),
        };
        let c2 = match g.node(c2_id).unwrap() {
            crate::types::Node::Concept(c) => c.clone(),
            _ => unreachable!(),
        };
        assert_eq!(score_concept(&g, &c1, &ctx).density, 1.0);
        assert_eq!(score_concept(&g, &c2, &ctx).density, 1.0);
    }

    /// #19: being contradicted (or contradicting) moves no score. A
    /// `Contradicts` pair leaves every dimension, the composite and the
    /// ranking bit-identical, both for the endpoints' own density and for
    /// the session's `max_incident` baseline every other concept is
    /// normalized against.
    #[test]
    fn contradictions_leave_every_score_unchanged() {
        let (mut g, c1, c2) = graph_with_two_concepts();
        let i1 = NodeId(Uuid::from_u64_pair(0, 1));
        let c3 = concept(3, i1, "bystander", 5);
        let c3_id = c3.id;
        g.insert_concept(c3, i1).unwrap();
        let edge = |id: u64, src: NodeId, tgt: NodeId, ty: EdgeType| crate::types::Edge {
            event_time: None,
            id: NodeId(Uuid::from_u64_pair(6, id)),
            session_id: sid(),
            source: src,
            target: tgt,
            edge_type: ty,
            weight: 1.0,
            reinforcements: 1,
            created_at: ts(0),
            last_reinforced: ts(0),
        };
        // c1 -> c3 Causal: c1 is the densest (2), c2 and c3 trail it.
        g.upsert_edge(edge(1, c1, c3_id, EdgeType::Causal)).unwrap();
        let dims = |g: &Graph| {
            let ctx = SessionContext::compute(g);
            [c1, c2, c3_id].map(|id| match g.node(id) {
                Some(Node::Concept(c)) => score_concept(g, c, &ctx),
                _ => unreachable!(),
            })
        };
        let weights = ScoringWeights::default();
        let before = (dims(&g), rescore(&g, &weights));
        // Counted, these would make c2 the densest (3 > 2) and lift c1.
        g.upsert_edge(edge(2, c2, c1, EdgeType::Contradicts))
            .unwrap();
        g.upsert_edge(edge(3, c1, c2, EdgeType::Contradicts))
            .unwrap();
        let c4 = concept(4, i1, "second contrarian", 6);
        let c4_id = c4.id;
        g.insert_concept(c4, i1).unwrap();
        g.upsert_edge(edge(4, c4_id, c2, EdgeType::Contradicts))
            .unwrap();
        let after_rank: Vec<_> = rescore(&g, &weights)
            .into_iter()
            .filter(|s| s.item != c4_id)
            .collect();
        assert_eq!(dims(&g), before.0);
        assert_eq!(after_rank, before.1);
        assert_scores_match_reference(&g);
    }

    #[test]
    fn frequency_weights_clamped_value() {
        // The pure composite clamps each dim to [0,1] *before* weighting:
        // access_count 5 / FREQUENCY_NORMALIZER 10 = 0.5, then 0.5 * 0.20.
        let dims = ScoreDims {
            recency: 0.0,
            frequency: 0.5,
            session_activity: 0.0,
            density: 0.0,
            edge_type_bonus: 0.0,
            concept_type_modifier: 0.0,
        };
        assert_eq!(dims.composite(), 0.5 * 0.20);
        // A raw value over 1.0 saturates at 1.0 (clamp before weighting).
        let dims = ScoreDims {
            frequency: 5.0,
            ..dims
        };
        assert_eq!(dims.composite(), 1.0 * 0.20);
    }

    #[test]
    fn session_activity_weights_clamped_value() {
        // Mirror of frequency_weights_clamped_value: a session_activity of
        // 0.5 is already inside [0, 1] and is clamped before weighting.
        let dims = ScoreDims {
            recency: 0.0,
            frequency: 0.0,
            session_activity: 0.5,
            density: 0.0,
            edge_type_bonus: 0.0,
            concept_type_modifier: 0.0,
        };
        assert_eq!(dims.composite(), 0.5 * 0.20);
        // A raw value over 1.0 saturates at 1.0 (clamp before weighting).
        let dims = ScoreDims {
            session_activity: 5.0,
            ..dims
        };
        assert_eq!(dims.composite(), 1.0 * 0.20);
    }

    #[test]
    fn rescore_empty_graph_is_empty_and_deterministic() {
        let g = Graph::new(sid());
        let ranked = rescore(&g, &ScoringWeights::default());
        assert!(ranked.is_empty());
    }

    #[test]
    fn rescore_is_deterministic_and_ranked() {
        let (g, _, _) = graph_with_two_concepts();
        let weights = ScoringWeights::default();
        let a = rescore(&g, &weights);
        let b = rescore(&g, &weights);
        assert_eq!(a, b, "rescore must be deterministic");
        assert_eq!(a.len(), 2);
        for w in a.windows(2) {
            assert!(w[0].score >= w[1].score);
        }
    }

    /// Issue #30: [`score`] (recall ranking, the daemon score table,
    /// canonization Stage 1) has no session-wide term, so the first access in
    /// a session moves exactly one score — the accessed concept's, upwards —
    /// and every other concept scores bit-for-bit what it did. GC's cut has no
    /// session-wide switch either since #29 (`score_live_plus_frequency`).
    #[test]
    fn an_access_moves_only_the_accessed_concepts_score() {
        let (mut g, c1, c2) = graph_with_two_concepts();
        let weights = ScoringWeights::default();
        let before: std::collections::HashMap<NodeId, f64> = rescore(&g, &weights)
            .into_iter()
            .map(|s| (s.item, s.score))
            .collect();
        assert_eq!(g.record_accesses(&[(c2, 1, ts(10))]), 1);
        let after: std::collections::HashMap<NodeId, f64> = rescore(&g, &weights)
            .into_iter()
            .map(|s| (s.item, s.score))
            .collect();
        assert_eq!(
            after[&c1].to_bits(),
            before[&c1].to_bits(),
            "an unread concept's score must not move when another is read"
        );
        assert!(after[&c2] > before[&c2], "the read concept gains frequency");
    }

    // ------------------------------------------------------------------
    // Fixture — session-rest-api, "user schema" on top
    // ------------------------------------------------------------------

    #[cfg(feature = "fixtures")]
    #[test]
    fn rest_api_fixture_ranks_user_schema_on_top_stably() {
        use crate::fixtures::load_snapshot;
        let snap = load_snapshot("session-rest-api").unwrap();
        let g = Graph::from_snapshot(snap).unwrap();
        let weights = ScoringWeights::default();

        let ranked = rescore(&g, &weights);
        let again = rescore(&g, &weights);
        assert_eq!(ranked, again, "rescoring must be stable");

        // Locate the "user schema" concept (content is "user schema"; the
        // fixture's canonical_key is the token-sorted "schema user").
        let user_schema_id = g
            .concepts()
            .find(|c| c.content == "user schema" || c.canonical_key == "schema user")
            .expect("fixture must contain user schema")
            .id;

        assert_eq!(ranked.len(), g.concepts().count());
        assert!(
            ranked.iter().any(|s| s.item == user_schema_id),
            "user schema must be scored"
        );
        assert_eq!(
            ranked[0].item,
            user_schema_id,
            "user schema must rank #1; got {:?}",
            ranked.iter().take(3).collect::<Vec<_>>()
        );
        // Sanity: it must also strictly beat the runner-up.
        assert!(ranked[0].score > ranked[1].score);
    }

    // ------------------------------------------------------------------
    // Issue-2 tie-break: equal scores order by canonical key, id behind
    // ------------------------------------------------------------------

    /// Equal scores must order by canonical key ascending with the id only
    /// behind that. The ids are minted so the id order contradicts the key
    /// order ("beta" carries the smaller id), so the old id-first chain fails
    /// this test: ids are per-run random and must not decide ties.
    #[test]
    fn rescore_ties_order_by_canonical_key_ahead_of_node_id() {
        let mut g = Graph::new(sid());
        let i1 = interaction(1, None, 0);
        let i2 = interaction(2, Some(1), 10);
        let iid = i1.id;
        g.insert_interaction(i1).unwrap();
        g.insert_interaction(i2).unwrap();
        // Same created_at, type, access_count, and no edges, so the two scores
        // tie exactly; key "alpha" rides the LARGER id.
        let alpha = concept(2, iid, "alpha", 5);
        let beta = concept(1, iid, "beta", 5);
        let (alpha_id, beta_id) = (alpha.id, beta.id);
        g.insert_concept(beta, iid).unwrap();
        g.insert_concept(alpha, iid).unwrap();

        let ranked = rescore(&g, &ScoringWeights::default());
        assert_eq!(
            ranked[0].score, ranked[1].score,
            "precondition: the two concepts tie exactly"
        );
        assert_eq!(
            ranked[0].item, alpha_id,
            "canonical key order (alpha < beta) must beat id order"
        );
        assert_eq!(ranked[1].item, beta_id);
    }
}
