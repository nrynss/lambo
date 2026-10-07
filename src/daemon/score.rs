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
//! * **recency** — how recently the concept was last touched
//!   (`last_accessed`, falling back to `created_at`) relative to the session's
//!   interaction temporal extent: `(last_touch − start) / (end − start)`,
//!   clamped. A single-point extent (all timestamps equal) yields `1.0`
//!   (everything is "now").
//! * **frequency** — `access_count` normalized by [`FREQUENCY_NORMALIZER`]
//!   (10 accesses = full frequency), clamped.
//! * **session_activity** — the share of the session's interactions that
//!   derived this concept (a `Derives` edge from the interaction), clamped.
//! * **density** — incident-edge count normalized by the session's most
//!   connected concept (densest concept = `1.0`), clamped.
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

/// Additive per-edge-type bonus (T4.1 interpretation; v0.6.0's table is not
/// in-repo). Structural / load-bearing edge types carry the most weight.
pub fn edge_type_bonus_value(ty: EdgeType) -> f64 {
    match ty {
        EdgeType::Causal => 0.02,
        EdgeType::Dependency => 0.02,
        EdgeType::Hierarchical => 0.015,
        EdgeType::Semantic => 0.01,
        EdgeType::CoOccurrence => 0.005,
        EdgeType::Derives | EdgeType::Temporal => 0.0,
    }
}

/// Additive concept-type modifier: the P1 typed multiplier as an offset.
pub fn concept_type_modifier(t: ConceptType) -> f64 {
    t.score_multiplier() - 1.0
}

/// Per-concept inputs to the spec §9 formula (typed carrier).
///
/// Raw, un-clamped dimension values; [`score`] applies the spec's clamp rule.
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
    /// numerator of `session_activity`. Counted once for the whole session
    /// (one pass over the edge set) so scoring a concept is `O(its degree)`
    /// instead of a scan of every interaction per concept: a GC sweep scores
    /// every concept under the write lock, and the per-concept scan made that
    /// `O(concepts × interactions)`. A concept absent from the map is derived
    /// by none.
    derived_by: HashMap<NodeId, usize>,
}

impl SessionContext {
    /// Aggregate the session's temporal extent and connectivity baselines.
    ///
    /// `max_incident` is the incident-edge count of the session's most
    /// connected concept (density is normalized against it).
    pub fn compute(graph: &Graph) -> Self {
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
        let max_incident = graph
            .concepts()
            .map(|c| graph.incident_edges(c.id).len())
            .max()
            .unwrap_or(0);
        let mut derived_by: HashMap<NodeId, usize> = HashMap::new();
        for e in graph.edges() {
            if e.edge_type == EdgeType::Derives
                && matches!(graph.node(e.source), Some(Node::Interaction(_)))
            {
                *derived_by.entry(e.target).or_default() += 1;
            }
        }
        Self {
            start,
            end,
            total_interactions,
            max_incident,
            derived_by,
        }
    }
}

/// Compute the six dimension values for one concept from graph state.
pub fn score_concept(graph: &Graph, c: &Concept, ctx: &SessionContext) -> ScoreDims {
    let last_touch = c.last_accessed.unwrap_or(c.created_at);
    let span_ms = (ctx.end - ctx.start).num_milliseconds();
    let recency = if span_ms == 0 {
        1.0
    } else {
        (last_touch - ctx.start).num_milliseconds() as f64 / span_ms as f64
    };

    let frequency = c.access_count as f64 / FREQUENCY_NORMALIZER;

    let derived_by = ctx.derived_by.get(&c.id).copied().unwrap_or(0);
    let session_activity = if ctx.total_interactions == 0 {
        0.0
    } else {
        derived_by as f64 / ctx.total_interactions as f64
    };

    let incident = graph.incident_edges(c.id);
    let density = if ctx.max_incident == 0 {
        0.0
    } else {
        incident.len() as f64 / ctx.max_incident as f64
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

/// Rescore every concept in the session, returning a score-descending ranked
/// list (the daemon's score table), ties broken by canonical key ascending
/// then `NodeId` ascending ([`tie_break_by_key`]).
///
/// Deterministic for a given graph AND across runs: the canonical key is
/// persisted, so equal-score concepts keep one order no matter which ids a run
/// minted (issue #2), and no wall-clock value enters the formula.
pub fn rescore(graph: &Graph, weights: &ScoringWeights) -> Vec<Scored<NodeId>> {
    let ctx = SessionContext::compute(graph);
    let key = |id: NodeId| match graph.node(id) {
        Some(crate::types::Node::Concept(c)) => Some(c.canonical_key.as_str()),
        _ => None,
    };
    let mut ranked: Vec<Scored<NodeId>> = graph
        .concepts()
        .map(|c| Scored::new(c.id, score(score_concept(graph, c, &ctx), weights)))
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

    /// The pre-optimization `score_concept`, kept verbatim as a test oracle:
    /// `session_activity`'s numerator is found by scanning every interaction
    /// for a `Derives` edge to the concept (`O(interactions)` per concept).
    /// [`score_concept`] now reads a once-per-sweep count from the
    /// [`SessionContext`]; the two must agree bit for bit.
    fn score_concept_reference(graph: &Graph, c: &Concept, ctx: &SessionContext) -> ScoreDims {
        let last_touch = c.last_accessed.unwrap_or(c.created_at);
        let span_ms = (ctx.end - ctx.start).num_milliseconds();
        let recency = if span_ms == 0 {
            1.0
        } else {
            (last_touch - ctx.start).num_milliseconds() as f64 / span_ms as f64
        };
        let frequency = c.access_count as f64 / FREQUENCY_NORMALIZER;
        let derived_by = graph
            .interactions()
            .filter(|i| graph.edge_between(i.id, c.id, EdgeType::Derives).is_some())
            .count();
        let session_activity = if ctx.total_interactions == 0 {
            0.0
        } else {
            derived_by as f64 / ctx.total_interactions as f64
        };
        let incident = graph.incident_edges(c.id);
        let density = if ctx.max_incident == 0 {
            0.0
        } else {
            incident.len() as f64 / ctx.max_incident as f64
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

    fn assert_scores_match_reference(g: &Graph) {
        let ctx = SessionContext::compute(g);
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
        let total = ctx.total_interactions as f64;
        assert_eq!(
            score_concept(&g, &c(c1), &ctx).session_activity,
            3.0 / total
        );
        assert_eq!(
            score_concept(&g, &c(c2), &ctx).session_activity,
            1.0 / total
        );
        assert_eq!(score_concept(&g, &c(lone_id), &ctx).session_activity, 0.0);
    }

    /// The shipped fixture session scores identically under both.
    #[cfg(feature = "fixtures")]
    #[test]
    fn score_concept_matches_the_per_concept_scan_on_the_fixture_session() {
        for name in ["session-rest-api", "session-drift"] {
            let snap = crate::fixtures::load_snapshot(name).expect("fixture");
            let g = Graph::from_snapshot(snap).unwrap();
            assert_scores_match_reference(&g);
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
        // Mirror of frequency_weights_clamped_value: session_activity is the
        // share of the session's interactions that derived the concept, an
        // already-[0,1] ratio, clamped before weighting — 0.5 * 0.20.
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
