//! `formal/oracle/Score.json`: Lean `Score.bonusAndModifier` and
//! `Score.score` against `bonus_and_modifier`, `ScoreDims::composite` and
//! `score` (`src/daemon/score.rs`).
//!
//! The model is fixed point (a number `x` is `1000 · x`, a composite is at
//! scale `10⁶`, `none` is non-finite) and exact; the Rust is `f64`. An input
//! integer `n` is the `f64` `n / 1000`, and an output is compared within
//! [`TOLERANCE`] of the model's exact value: far below the model's
//! resolution, so a wrong constant, clamp bound or weight is caught, and
//! above `f64` rounding on values of this size.

use serde::Deserialize;

use super::{vectors, Vectors};
use crate::config::ScoringWeights;
use crate::daemon::score::formal_oracle::bonus_and_modifier;
use crate::daemon::score::{score, ScoreDims};

/// Allowed distance between the Rust `f64` and the model's exact value.
const TOLERANCE: f64 = 1e-9;

/// An `f64` input: `n / 1000`, or a non-finite spelling.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum F {
    Milli(i64),
    Special(String),
}

impl F {
    fn f64(&self) -> f64 {
        match self {
            F::Milli(n) => *n as f64 / 1000.0,
            F::Special(s) => match s.as_str() {
                "NaN" => f64::NAN,
                "inf" => f64::INFINITY,
                "-inf" => f64::NEG_INFINITY,
                other => panic!("Score.json: unknown spelling {other:?}"),
            },
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Dims {
    recency: F,
    frequency: F,
    session_activity: F,
    density: F,
    edge_type_bonus: F,
    concept_type_modifier: F,
}

impl Dims {
    fn rust(&self) -> ScoreDims {
        ScoreDims {
            recency: self.recency.f64(),
            frequency: self.frequency.f64(),
            session_activity: self.session_activity.f64(),
            density: self.density.f64(),
            edge_type_bonus: self.edge_type_bonus.f64(),
            concept_type_modifier: self.concept_type_modifier.f64(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Weights {
    recency: F,
    frequency: F,
    session_activity: F,
    density: F,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Weighted {
    dims: Dims,
    weights: Weights,
}

/// A value the model gives at scale `S`, compared within [`TOLERANCE`].
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(from = "i64")]
struct Scaled<const S: i64>(f64);

impl<const S: i64> From<i64> for Scaled<S> {
    fn from(n: i64) -> Self {
        Scaled(n as f64 / S as f64)
    }
}

impl<const S: i64> PartialEq for Scaled<S> {
    fn eq(&self, other: &Self) -> bool {
        (self.0 - other.0).abs() <= TOLERANCE
    }
}

#[test]
fn score_() {
    let v = Vectors::load(
        "Score",
        vectors!("Score"),
        &[
            "Lambo.Score.bonusAndModifier",
            "Lambo.Score.score defaultWeights",
            "Lambo.Score.score",
        ],
    );
    v.replay("Lambo.Score.bonusAndModifier", |d: &Dims| {
        Scaled::<1_000>(bonus_and_modifier(d.rust()))
    });
    v.replay("Lambo.Score.score defaultWeights", |d: &Dims| {
        Scaled::<1_000_000>(d.rust().composite())
    });
    v.replay("Lambo.Score.score", |input: &Weighted| {
        let w = ScoringWeights {
            recency: input.weights.recency.f64(),
            frequency: input.weights.frequency.f64(),
            session_activity: input.weights.session_activity.f64(),
            density: input.weights.density.f64(),
        };
        Scaled::<1_000_000>(score(input.dims.rust(), &w))
    });
}
