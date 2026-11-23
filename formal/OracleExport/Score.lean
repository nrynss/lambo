/-
# Oracle vectors for `Score.lean`

`Score.bonusAndModifier` and `Score.score` (with `Score.defaultWeights` and
with generated weights) against `bonus_and_modifier`, `score` and
`ScoreDims::composite` in `src/daemon/score.rs`.

The model is the exact-arithmetic reading of `f64` code in fixed point: a
dimension or weight `x` is the integer `1000 · x`, a composite is at scale
`10⁶`, and `none` is a non-finite `f64`. The inputs are therefore multiples
of `0.001` (written as those integers; the Rust test divides by `1000`,
which is the correctly rounded `f64` of the decimal) or one of `NaN`,
`inf`, `-inf` for `none`. The Rust computes in `f64` and the model does
not capture its rounding, so the test compares within `1e-9`, far below the
model's resolution (`10⁻⁶`); any difference in a constant, a clamp bound or
a weight shows up at `10⁻⁶` or more.
-/
import LamboProofs.Score
import OracleExport.Gen

namespace Lambo.Oracle.Score

open Lean Lambo.Score Lambo.Oracle

/-- A generated `f64`: `some n` is `n / 1000`, `none` one of the three
non-finite values (its spelling, for the Rust side). -/
structure F where
  val : Option Int
  spelling : String
  deriving Inhabited

def F.json (f : F) : Json :=
  match f.val with
  | some n => toJson n
  | none => toJson f.spelling

/-- Values around every clamp bound: `[0, 1]` for a dimension, `[0, 0.25]`
for the edge bonus, `[-0.10, 0.15]` for the modifier, and beyond. -/
def genF (special : List Int) : Gen F := do
  match ← below 12 with
  | 0 => return { val := none, spelling := ← pick ["NaN", "inf", "-inf"] }
  | 1 | 2 | 3 | 4 => return { val := some (← pick special), spelling := "" }
  | 5 => return { val := some (Int.ofNat (← range 0 4000) - 2000), spelling := "" }
  | _ => return { val := some (← range 0 1000), spelling := "" }

def dimSpecial : List Int := [0, 1, -1, 999, 1000, 1001, 500, 2000, -1000]
def bonusSpecial : List Int := [0, -1, 1, 249, 250, 251, 20, 15, 5, 1000]
def modSpecial : List Int := [-101, -100, -99, 0, 149, 150, 151, 50, -50, -1000]
def weightSpecial : List Int := [0, -1, 250, 200, 350, 1000, 1, -250]

structure DimsIn where
  recency : F
  frequency : F
  sessionActivity : F
  density : F
  edgeTypeBonus : F
  conceptTypeModifier : F

def genDims : Gen DimsIn := do
  return { recency := ← genF dimSpecial, frequency := ← genF dimSpecial,
           sessionActivity := ← genF dimSpecial, density := ← genF dimSpecial,
           edgeTypeBonus := ← genF bonusSpecial, conceptTypeModifier := ← genF modSpecial }

def DimsIn.model (d : DimsIn) : Dims :=
  ⟨d.recency.val, d.frequency.val, d.sessionActivity.val, d.density.val, d.edgeTypeBonus.val,
    d.conceptTypeModifier.val⟩

def DimsIn.json (d : DimsIn) : Json :=
  Json.mkObj [("recency", d.recency.json), ("frequency", d.frequency.json),
    ("session_activity", d.sessionActivity.json), ("density", d.density.json),
    ("edge_type_bonus", d.edgeTypeBonus.json),
    ("concept_type_modifier", d.conceptTypeModifier.json)]

structure WeightsIn where
  recency : F
  frequency : F
  sessionActivity : F
  density : F

def genWeights : Gen WeightsIn := do
  let g : Gen F := do
    if ← chance 1 12 then return { val := none, spelling := ← pick ["NaN", "inf", "-inf"] }
    if ← chance 1 2 then return { val := some (← pick weightSpecial), spelling := "" }
    return { val := some (← range 0 1500), spelling := "" }
  return { recency := ← g, frequency := ← g, sessionActivity := ← g, density := ← g }

def WeightsIn.model (w : WeightsIn) : Weights :=
  ⟨w.recency.val, w.frequency.val, w.sessionActivity.val, w.density.val⟩

def WeightsIn.json (w : WeightsIn) : Json :=
  Json.mkObj [("recency", w.recency.json), ("frequency", w.frequency.json),
    ("session_activity", w.sessionActivity.json), ("density", w.density.json)]

def seed : UInt64 := 0x53434F5245 -- "SCORE"

def vectors : Vectors := run seed do
  let bonus ← many 150 genDims
  let dflt ← many 150 genDims
  let weighted ← many 150 (do return (← genDims, ← genWeights))
  return {
    model := "Score", seed
    notes := [
      "Fixed point as the model: an input number n is the f64 n / 1000; NaN, inf, -inf are the model's none. Outputs: bonusAndModifier at scale 1000, score at scale 10^6.",
      "The model is exact arithmetic and the Rust is f64, so the Rust test compares within 1e-9 (the model's resolution is 1e-6).",
      "score with defaultWeights is ScoreDims::composite; with generated weights, score(dims, &weights)."
    ]
    fns := [
      { lean := "Lambo.Score.bonusAndModifier", rust := "daemon::score::bonus_and_modifier",
        cases := bonus.map fun d => case d.json (toJson (bonusAndModifier d.model)) },
      { lean := "Lambo.Score.score defaultWeights", rust := "daemon::score::ScoreDims::composite",
        cases := dflt.map fun d => case d.json (toJson (score d.model defaultWeights)) },
      { lean := "Lambo.Score.score", rust := "daemon::score::score",
        cases := weighted.map fun (d, w) =>
          case (Json.mkObj [("dims", d.json), ("weights", w.json)]) (toJson (score d.model w.model)) }
    ]
  }

end Lambo.Oracle.Score
