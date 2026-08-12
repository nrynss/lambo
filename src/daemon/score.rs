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

use crate::config::ScoringWeights;
use crate::graph::Graph;
use crate::types::{Concept, ConceptType, EdgeType, NodeId, Scored};

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
/// GC's step-2 cut uses this while `access_count` is dead session-wide
/// (ALGO-1). The spec formula reserves 20% of the composite for a dimension no
/// write path feeds until P5 recall lands, so an **absolute** threshold against
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
/// Measured on the shipped `session-rest-api` fixture (all 22 concepts, at the
/// moment the first access lands and GC's cut flips): **every** concept's
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
pub struct SessionContext {
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    total_interactions: usize,
    max_incident: usize,
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
        Self {
            start,
            end,
            total_interactions,
            max_incident,
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

/// Rescore every concept in the session, returning a score-descending,
/// id-ascending ranked list (the daemon's score table).
///
/// Deterministic for a given graph: ties break by `NodeId` ascending, and no
/// wall-clock value enters the formula.
pub fn rescore(graph: &Graph, weights: &ScoringWeights) -> Vec<Scored<NodeId>> {
    let ctx = SessionContext::compute(graph);
    let mut ranked: Vec<Scored<NodeId>> = graph
        .concepts()
        .map(|c| Scored::new(c.id, score(score_concept(graph, c, &ctx), weights)))
        .collect();
    ranked.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.item.0.cmp(&b.item.0))
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
}
