/-
# Oracle vectors for `Fusion.lean`: RRF

`Fusion.RrfRecall`: each candidate's exact RRF value (`rrf`, a fraction) and
its position in `rank`'s output (`rrfAhead`), against `rank` in
`src/recall/candidates.rs` under `FusionKind::Rrf`.

Instantiations:

* `tb` is `tie_break_by_key` as in the Order vectors: `Order.tieBreak` with
  `str::cmp` on keys and the member number as id; a keyless member is an id
  in no graph.
* Leg scores are at scale `1000` (finite: the model has no non-finite score);
  the Rust leg score is `n / 1000`.
* The recent leg's rank is the model's `recentRank` of the generated
  `recentIdx` (which of the three newest interactions owns the member).
  `rank` reads it from `LegScores::recent_rank`, which `recent_concepts`
  fills upstream, so the vector writes the model's rank into the input.
* `RrfConfig`: `k = kNum / kDen`, weights at scale `10`.

The model is exact and the Rust rounds (`f64`). The model's header says where
that matters: two candidates whose exact values tie through different rank
vectors may be ordered by a rounding in Rust instead of by the tie-break.
Cases with such a tie above zero are not generated (`roundingTie`);
everywhere else the order is compared exactly and each value within `1e-12`.

A tie at zero is different, and the oracle found it (#328): a candidate
ranked only in legs of weight zero had the Rust value `+0.0` (a sum of zero
terms), one ranked in no leg `-0.0` (Rust's empty `f64` sum), and
`total_cmp` ordered the first ahead whatever the tie-break said, where the
model calls both `0` and orders them by the tie-break. The Rust now sums
from `+0.0` and agrees with the model. Those cases (`zeroTie`) keep their
own function, as #328's regression cases.
-/
import LamboProofs.Fusion
import OracleExport.Gen
import OracleExport.Order

namespace Lambo.Oracle.Fusion

open Lean Lambo.Fusion Lambo.Oracle

instance : Inhabited RrfConfig := ⟨rrfDefault⟩

structure Member where
  id : Nat
  key : Option String
  keyword : Option Nat
  vector : Option Nat
  recentIdx : Option Nat
  deriving Inhabited

def find (ms : List Member) (i : Nat) : Member := (ms.find? (·.id == i)).getD default

def legsOf (ms : List Member) (i : Nat) : Legs :=
  let m := find ms i
  ⟨m.keyword.map Int.ofNat, none, m.vector.map Int.ofNat⟩

def tbOf (ms : List Member) (a b : Nat) : Bool :=
  Lambo.Order.tieBreak Order.charsCmp (fun i => (find ms i).key.map String.toList)
    Lambo.Order.natCmp a b == .lt

def recallOf (cfg : RrfConfig) (ms : List Member) (ran : Bool) : RrfRecall Nat :=
  { cfg, tb := tbOf ms, ids := ms.map (·.id), keywordRan := ran,
    recentIdx := fun i => (find ms i).recentIdx }

/-- `(kNum, kDen, wKeyword, wVector, wRecent)`: the default, the tests'
`k = 60`, `k = 0`, a fractional `k`, zero and unequal weights. -/
def configs : List RrfConfig :=
  [rrfDefault, rrfDefault, rrfDefault, rrfK60, ⟨0, 1, 10, 10, 3⟩, ⟨1, 2, 10, 10, 3⟩,
   ⟨3, 1, 10, 0, 3⟩, ⟨3, 1, 0, 10, 0⟩, ⟨3, 1, 7, 2, 10⟩]

def genMembers : Gen (List Member) := do
  let n ← range 0 8
  let ids := (← shuffle ((List.range 12).map (· + 1))).take n
  let keys ← many (← range 1 3) Order.genKey
  let scores ← many (← range 1 4) (pick [0, 1, 500, 1000, 9000, 2000, 800])
  ids.mapM fun id => do
    let key ← if ← chance 4 5 then pure (some (← pick keys)) else pure none
    let keyword ← if ← chance 1 2 then pure (some (← pick scores)) else pure none
    let vector ← if ← chance 1 2 then pure (some (← pick scores)) else pure none
    let recentIdx ← if ← chance 1 3 then pure (some (← below 3)) else pure none
    return { id, key, keyword, vector, recentIdx }

/-- Two candidates whose exact values tie, above zero, through different rank
vectors: the one place the model says the Rust's rounding, not the
tie-break, may decide the order. -/
def roundingTie (R : RrfRecall Nat) (legs : Nat → Legs) : Bool :=
  R.ids.any fun a => R.ids.any fun b =>
    a != b && R.num legs a == R.num legs b && R.num legs a != 0 &&
      R.ranks legs a != R.ranks legs b

/-- Two candidates whose exact values are both zero through different rank
vectors (one ranked only in legs of weight zero, one in no leg). The oracle
found that the Rust ordered these by the sign of a zero (an empty `f64` sum
is `-0.0`, a sum of zero terms `+0.0`, and `total_cmp` puts `+0.0` first)
rather than by the tie-break (#328, fixed); they keep their own function. -/
def zeroTie (R : RrfRecall Nat) (legs : Nat → Legs) : Bool :=
  R.best legs != 0 && R.ids.any fun a => R.ids.any fun b =>
    a != b && R.num legs a == 0 && R.num legs b == 0 && R.ranks legs a != R.ranks legs b

def memberJson (R : RrfRecall Nat) (legs : Nat → Legs) (m : Member) : Json :=
  Json.mkObj [("id", toJson m.id), ("key", toJson m.key), ("keyword", toJson m.keyword),
    ("vector", toJson m.vector), ("recent_rank", toJson (R.ranks legs m.id).recent)]

def render (cfg : RrfConfig) (ms : List Member) (ran : Bool) : Json :=
  let R := recallOf cfg ms ran
  let legs := legsOf ms
  let order := (R.ids.map fun i => (R.rrfAhead legs i, i)).mergeSort (fun a b => a.1 ≤ b.1)
  let ordered := order.map (·.2)
  case
    (Json.mkObj [("members", toJson (ms.map (memberJson R legs))), ("keyword_ran", toJson ran),
      ("config", toJson #[cfg.kNum, cfg.kDen, cfg.wKeyword, cfg.wVector, cfg.wRecent])])
    (Json.mkObj [("order", toJson ordered),
      ("rrf", toJson (ordered.map fun i => let r := R.rrf legs i; #[r.1, r.2]))])

/-- A case without a rounding tie whose zero ties are as `zeros` asks
(redrawn otherwise; after 200 draws, the empty case). -/
def genCase (zeros : Bool) : Gen Json := do
  for _ in [0:200] do
    let ms ← genMembers
    let cfg ← pick configs
    let ran ← chance 1 2
    let R := recallOf cfg ms ran
    if !roundingTie R (legsOf ms) && zeroTie R (legsOf ms) == zeros then
      return render cfg ms ran
  return render rrfDefault [] false

def seed : UInt64 := 0x465553494F4E -- "FUSION"

def vectors : Vectors := run seed do
  let cases ← many 250 (genCase false)
  let zeros ← many 30 (genCase true)
  return {
    model := "Fusion", seed
    notes := [
      "RrfRecall.rrf (an exact fraction [num, den]) and RrfRecall.rrfAhead (the output order) against rank under FusionKind::Rrf.",
      "members: id (Uuid pair (2,id)), key (a concept with that canonical_key; null: an id in no graph), keyword and vector leg scores at scale 1000, recent_rank (the model's recentRank of a generated recentIdx, written into the input because rank reads LegScores::recent_rank).",
      "tb: Order.tieBreak with str::cmp keys and member numbers, as in Order.json. config: [kNum, kDen, wKeyword, wVector, wRecent], k = kNum / kDen, weights at scale 10.",
      "No case has two candidates whose exact values tie above zero through different rank vectors (the f64 rounding the model leaves out decides those). The order is compared exactly, each value within 1e-12.",
      "Cases where two candidates' exact values are both zero through different rank vectors are in the second function only: the regression cases of #328, where the Rust ordered by the sign of the zero (an empty f64 sum is -0.0) rather than by the tie-break until it summed from +0.0."
    ]
    fns := [
      { lean := "Lambo.Fusion.RrfRecall.rrf", rust := "recall::candidates::rank (FusionKind::Rrf)", cases },
      { lean := "Lambo.Fusion.RrfRecall.rrf zero ties", rust := "recall::candidates::rank (FusionKind::Rrf)",
        cases := zeros }
    ]
  }

end Lambo.Oracle.Fusion
