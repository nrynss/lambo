/-
# Daemon composite score

Model of `src/daemon/score.rs` (spec §9) and of the weight sanitizer in
`src/config.rs` (`ScoringWeights::sanitized`).

The Rust computes in `f64`; the model is its exact-arithmetic reading in
fixed point: a dimension, weight, bonus or modifier `x` is `1000 · x` (every
constant in the module is a multiple of `0.005`), a composite is carried at
scale `10⁶`, and a non-finite `f64` is `none`. Also modelled: GC's eviction
composite (`score_live_plus_frequency`), recency (`concept_recency`, #95) and
the max-normalized dimensions (density, session activity).

Results: for every input, finite or not, and every weight set, valid or not,
the composite lies in `[0, 1 + MAX_BONUS]` (`score_range`); raising any clamped
dimension, or the bonus-plus-modifier term, never lowers it (`score_mono`);
`ScoringWeights::default`'s weights sum to exactly 1 (`defaultWeights_total`);
`sanitized` is the identity on a valid weight and idempotent
(`saneWeight_eq_self`, `saneWeight_idem`), and a zeroed weight drops its
dimension (`weighted_ignores_frequency_of_bad_weight`). GC's eviction composite
scores a concept with `frequency == 0` exactly at its live-dimension score,
reading a concept can only raise it, and it stays in the same range
(`livePlusFrequency_of_frequency_zero`, `live_le_livePlusFrequency`,
`livePlusFrequency_mono`, `livePlusFrequency_range`). The two expressions for
recency agree, the historical one is in `[0, 1]` without the clamp, and a
session younger than ten minutes scores every concept near 1
(`historical_eq_floor`, `historical_in_unit`, `floor_recency_near_one`); the
last touch is never before creation (`createdAt_le_lastTouch`). A
max-normalized count is at most the session maximum, which is attained
(`le_listMax`, `listMax_mem_or_zero`), so the clamp never fires for those
dimensions.

Boundary: `f64` rounding and overflow of an intermediate sum to `±∞` (weights
near `f64::MAX`) are not captured; `score_range` still describes the Rust
then, because `finite_or_zero` returns `0.0` for a non-finite composite. The
dimensions are inputs: how each is measured from the graph is not modelled.
-/

namespace Lambo.Score

/-! ## Clamp -/

/-- `f64::clamp(lo, hi)` on a finite value. -/
def clamp (lo hi v : Int) : Int :=
  if v < lo then lo else if hi < v then hi else v

theorem clamp_ge {lo hi : Int} (h : lo ≤ hi) (v : Int) : lo ≤ clamp lo hi v := by
  unfold clamp
  repeat' split
  all_goals omega

theorem clamp_le {lo hi : Int} (h : lo ≤ hi) (v : Int) : clamp lo hi v ≤ hi := by
  unfold clamp
  repeat' split
  all_goals omega

/-- Inside the range the clamp is the identity. -/
theorem clamp_eq_self {lo hi v : Int} (h1 : lo ≤ v) (h2 : v ≤ hi) : clamp lo hi v = v := by
  unfold clamp
  repeat' split
  all_goals omega

theorem clamp_mono {lo hi : Int} (h : lo ≤ hi) {a b : Int} (hab : a ≤ b) :
    clamp lo hi a ≤ clamp lo hi b := by
  unfold clamp
  repeat' split
  all_goals omega

/-! ## The sanitizers (`clamp_dim`, `clamp_additive`, `sane_weight`) -/

/-- `clamp_dim` (score.rs): a weighted dimension is clamped to `[0, 1]`, and a
non-finite input counts as `0`. -/
def clampDim : Option Int → Int
  | none => 0
  | some v => clamp 0 1000 v

/-- `clamp_additive` (score.rs): an additive term is clamped to `[lo, hi]`, and
a non-finite input counts as `0`. -/
def clampAdditive (lo hi : Int) : Option Int → Int
  | none => 0
  | some v => clamp lo hi v

/-- `sane_weight` (config.rs): a weight is used as given when it is finite and
non-negative, and is `0` otherwise. -/
def saneWeight : Option Int → Int
  | none => 0
  | some w => if w < 0 then 0 else w

theorem clampDim_nonneg (x : Option Int) : 0 ≤ clampDim x := by
  cases x with
  | none => show (0 : Int) ≤ 0; omega
  | some v => show 0 ≤ clamp 0 1000 v; exact clamp_ge (by omega) v

theorem clampDim_le (x : Option Int) : clampDim x ≤ 1000 := by
  cases x with
  | none => show (0 : Int) ≤ 1000; omega
  | some v => show clamp 0 1000 v ≤ 1000; exact clamp_le (by omega) v

/-- On finite inputs `clamp_dim` is monotone. -/
theorem clampDim_mono {a b : Int} (h : a ≤ b) : clampDim (some a) ≤ clampDim (some b) := by
  show clamp 0 1000 a ≤ clamp 0 1000 b
  exact clamp_mono (by omega) h

/-- A dimension already in `[0, 1]` passes through `clamp_dim` unchanged. This
is the "the clamp is a no-op for it" remark on `ScoreDims::recency`. -/
theorem clampDim_eq_self {v : Int} (h1 : 0 ≤ v) (h2 : v ≤ 1000) : clampDim (some v) = v := by
  show clamp 0 1000 v = v
  exact clamp_eq_self h1 h2

theorem clampAdditive_ge {lo hi : Int} (hlo : lo ≤ 0) (hhi : 0 ≤ hi) (x : Option Int) :
    lo ≤ clampAdditive lo hi x := by
  cases x with
  | none => show lo ≤ 0; exact hlo
  | some v => show lo ≤ clamp lo hi v; exact clamp_ge (by omega) v

theorem clampAdditive_le {lo hi : Int} (hlo : lo ≤ 0) (hhi : 0 ≤ hi) (x : Option Int) :
    clampAdditive lo hi x ≤ hi := by
  cases x with
  | none => show (0 : Int) ≤ hi; exact hhi
  | some v => show clamp lo hi v ≤ hi; exact clamp_le (by omega) v

theorem saneWeight_nonneg (w : Option Int) : 0 ≤ saneWeight w := by
  cases w with
  | none => show (0 : Int) ≤ 0; omega
  | some v =>
    show 0 ≤ (if v < 0 then 0 else v)
    repeat' split
    all_goals omega

/-- `sanitized` is the identity on a valid weight ("the identity on any valid
set", config.rs). -/
theorem saneWeight_eq_self {w : Int} (h : 0 ≤ w) : saneWeight (some w) = w := by
  show (if w < 0 then 0 else w) = w
  repeat' split
  all_goals omega

/-- `sanitized` is idempotent (config.rs: "Idempotent"). -/
theorem saneWeight_idem (w : Option Int) : saneWeight (some (saneWeight w)) = saneWeight w :=
  saneWeight_eq_self (saneWeight_nonneg w)

/-! ## The composite -/

/-- `ScoreDims`: the six per-concept inputs. `none` is a non-finite `f64`. -/
structure Dims where
  recency : Option Int
  frequency : Option Int
  sessionActivity : Option Int
  density : Option Int
  edgeTypeBonus : Option Int
  conceptTypeModifier : Option Int

/-- `ScoringWeights`, as read from configuration (so possibly non-finite or
negative). -/
structure Weights where
  recency : Option Int
  frequency : Option Int
  sessionActivity : Option Int
  density : Option Int

/-- `ScoringWeights::default()`: `0.25 / 0.20 / 0.20 / 0.35`.
Pinned by `formal_constants_tests::score`. -/
def defaultWeights : Weights :=
  { recency := some 250, frequency := some 200, sessionActivity := some 200, density := some 350 }

/-- Sum of the sanitized weights (scale `10³`). -/
def weightTotal (w : Weights) : Int :=
  saneWeight w.recency + saneWeight w.frequency + saneWeight w.sessionActivity
    + saneWeight w.density

/-- The weighted part of `score` (scale `10⁶`). -/
def weighted (d : Dims) (w : Weights) : Int :=
  clampDim d.recency * saneWeight w.recency
    + clampDim d.frequency * saneWeight w.frequency
    + clampDim d.sessionActivity * saneWeight w.sessionActivity
    + clampDim d.density * saneWeight w.density

/-- `bonus_and_modifier` (scale `10³`): the edge bonus clamped to
`[0, MAX_EDGE_BONUS] = [0, 0.25]` plus the concept-type modifier clamped to
`[MIN_CONCEPT_MODIFIER, MAX_CONCEPT_MODIFIER] = [-0.10, 0.15]`.
Pinned by `formal_constants_tests::score`. -/
def bonusAndModifier (d : Dims) : Int :=
  clampAdditive 0 250 d.edgeTypeBonus + clampAdditive (-100) 150 d.conceptTypeModifier

/-- `finite_or_zero` on a finite composite: clamp to `[0, 1 + MAX_BONUS]`.
Pinned by `formal_constants_tests::score`. -/
def finiteOrZero (x : Int) : Int := clamp 0 1400000 x

/-- The composite before its final clamp. -/
def rawScore (d : Dims) (w : Weights) : Int :=
  weighted d w + 1000 * bonusAndModifier d

/-- `score` (score.rs), the spec §9 composite. -/
def score (d : Dims) (w : Weights) : Int := finiteOrZero (rawScore d w)

/-- One weighted term is between `0` and its weight. -/
theorem term_bounds (x w : Option Int) :
    0 ≤ clampDim x * saneWeight w ∧ clampDim x * saneWeight w ≤ 1000 * saneWeight w :=
  ⟨Int.mul_nonneg (clampDim_nonneg x) (saneWeight_nonneg w),
   Int.mul_le_mul_of_nonneg_right (clampDim_le x) (saneWeight_nonneg w)⟩

/-- A weighted term is monotone in its clamped dimension. -/
theorem term_mono {x y : Option Int} (h : clampDim x ≤ clampDim y) (w : Option Int) :
    clampDim x * saneWeight w ≤ clampDim y * saneWeight w :=
  Int.mul_le_mul_of_nonneg_right h (saneWeight_nonneg w)

theorem weighted_nonneg (d : Dims) (w : Weights) : 0 ≤ weighted d w := by
  have h1 := term_bounds d.recency w.recency
  have h2 := term_bounds d.frequency w.frequency
  have h3 := term_bounds d.sessionActivity w.sessionActivity
  have h4 := term_bounds d.density w.density
  unfold weighted
  omega

/-- The weighted part never exceeds the weight total: each dimension is at
most `1`. -/
theorem weighted_le (d : Dims) (w : Weights) : weighted d w ≤ 1000 * weightTotal w := by
  have h1 := term_bounds d.recency w.recency
  have h2 := term_bounds d.frequency w.frequency
  have h3 := term_bounds d.sessionActivity w.sessionActivity
  have h4 := term_bounds d.density w.density
  unfold weighted weightTotal
  omega

theorem bonusAndModifier_ge (d : Dims) : -100 ≤ bonusAndModifier d := by
  have h1 : (0 : Int) ≤ clampAdditive 0 250 d.edgeTypeBonus :=
    clampAdditive_ge (by omega) (by omega) d.edgeTypeBonus
  have h2 : (-100 : Int) ≤ clampAdditive (-100) 150 d.conceptTypeModifier :=
    clampAdditive_ge (by omega) (by omega) d.conceptTypeModifier
  unfold bonusAndModifier
  omega

/-- `edge_type_bonus + concept_type_modifier ≤ MAX_BONUS = 0.40`. -/
theorem bonusAndModifier_le (d : Dims) : bonusAndModifier d ≤ 400 := by
  have h1 : clampAdditive 0 250 d.edgeTypeBonus ≤ 250 :=
    clampAdditive_le (by omega) (by omega) d.edgeTypeBonus
  have h2 : clampAdditive (-100) 150 d.conceptTypeModifier ≤ 150 :=
    clampAdditive_le (by omega) (by omega) d.conceptTypeModifier
  unfold bonusAndModifier
  omega

/-- **Range.** For every input, finite or not, and every weight set, valid or
not, the composite lies in `[0, 1 + MAX_BONUS]`. This is the module's "bounded
and finite for any inputs" contract (spec §5.7). -/
theorem score_range (d : Dims) (w : Weights) : 0 ≤ score d w ∧ score d w ≤ 1400000 := by
  show 0 ≤ clamp 0 1400000 (rawScore d w) ∧ clamp 0 1400000 (rawScore d w) ≤ 1400000
  exact ⟨clamp_ge (by omega) _, clamp_le (by omega) _⟩

/-- With weights that sum to at most `1`, the composite cannot reach the upper
clamp: the bound `1 + MAX_BONUS` is the true supremum of the formula, not an
artifact of the final clamp. -/
theorem rawScore_le (d : Dims) (w : Weights) (hw : weightTotal w ≤ 1000) :
    rawScore d w ≤ 1400000 := by
  have h1 := weighted_le d w
  have h2 := bonusAndModifier_le d
  unfold rawScore
  omega

/-- `score_concept_with`'s composite falls below zero only through the
concept-type modifier, and by at most `|MIN_CONCEPT_MODIFIER| = 0.10`. -/
theorem rawScore_ge (d : Dims) (w : Weights) : -100000 ≤ rawScore d w := by
  have h1 := weighted_nonneg d w
  have h2 := bonusAndModifier_ge d
  unfold rawScore
  omega

/-- With sanitized weights summing to at most 1 and a non-negative composite,
`score_concept_with`'s final clamp only floors at zero: the score is the raw
composite. -/
theorem score_eq_rawScore (d : Dims) (w : Weights) (hw : weightTotal w ≤ 1000)
    (hpos : 0 ≤ rawScore d w) : score d w = rawScore d w := by
  show clamp 0 1400000 (rawScore d w) = rawScore d w
  exact clamp_eq_self hpos (rawScore_le d w hw)

/-- `ScoringWeights::default`'s weights sum to exactly 1. -/
theorem defaultWeights_total : weightTotal defaultWeights = 1000 := by
  first
    | decide
    | rfl
    | simp [weightTotal, defaultWeights, saneWeight]

/-- **Monotonicity.** Raising any clamped dimension, or the bonus-plus-modifier
term, never lowers the composite. In particular an access (which raises only
`frequency`, and the concept's own recency) can only raise the accessed
concept's score. -/
theorem score_mono (d d' : Dims) (w : Weights)
    (hr : clampDim d.recency ≤ clampDim d'.recency)
    (hf : clampDim d.frequency ≤ clampDim d'.frequency)
    (ha : clampDim d.sessionActivity ≤ clampDim d'.sessionActivity)
    (hd : clampDim d.density ≤ clampDim d'.density)
    (hb : bonusAndModifier d ≤ bonusAndModifier d') :
    score d w ≤ score d' w := by
  have h1 := term_mono hr w.recency
  have h2 := term_mono hf w.frequency
  have h3 := term_mono ha w.sessionActivity
  have h4 := term_mono hd w.density
  have hraw : rawScore d w ≤ rawScore d' w := by
    unfold rawScore weighted
    omega
  show clamp 0 1400000 (rawScore d w) ≤ clamp 0 1400000 (rawScore d' w)
  exact clamp_mono (by omega) hraw

/-- A zeroed weight drops its dimension: with `frequency` weight non-finite or
negative, the frequency dimension has no effect on the weighted part
(config.rs: "A zeroed weight drops its dimension out of the composite; it can
never poison the whole score"). -/
theorem weighted_ignores_frequency_of_bad_weight (d d' : Dims) (w : Weights)
    (hw : saneWeight w.frequency = 0)
    (hr : d.recency = d'.recency) (ha : d.sessionActivity = d'.sessionActivity)
    (hd : d.density = d'.density) : weighted d w = weighted d' w := by
  unfold weighted
  rw [hw, hr, ha, hd] <;> simp

/-! ## GC's eviction composite (`score_live_plus_frequency`) -/

/-- Sum of the sanitized *live* weights: recency, session activity, density. -/
def liveTotal (w : Weights) : Int :=
  saneWeight w.recency + saneWeight w.sessionActivity + saneWeight w.density

/-- Numerator of the live weighted part of `score_over_live_dimensions`; the
Rust code divides it by `live_total` when that is positive. -/
def liveNumerator (d : Dims) (w : Weights) : Int :=
  clampDim d.recency * saneWeight w.recency
    + clampDim d.sessionActivity * saneWeight w.sessionActivity
    + clampDim d.density * saneWeight w.density

/-- The renormalized live part is in `[0, 1]`: `0 ≤ numerator ≤ 1 · live_total`.
This is the "occupies the same `[0,1]`-plus-bonus range as `score`" claim,
stated without the division. -/
theorem liveNumerator_range (d : Dims) (w : Weights) :
    0 ≤ liveNumerator d w ∧ liveNumerator d w ≤ 1000 * liveTotal w := by
  have h1 := term_bounds d.recency w.recency
  have h3 := term_bounds d.sessionActivity w.sessionActivity
  have h4 := term_bounds d.density w.density
  unfold liveNumerator liveTotal
  constructor <;> omega

/-- In exact arithmetic: at `frequency == 0` the full composite's
weighted part *is* the live numerator, that is, `live_total ×` the renormalized
live part. Switching from the live score to the full one therefore scales the
weighted part by `live_total` (`0.8` at the default weights). -/
theorem weighted_eq_liveNumerator_of_frequency_zero (d : Dims) (w : Weights)
    (hf : clampDim d.frequency = 0) : weighted d w = liveNumerator d w := by
  unfold weighted liveNumerator
  rw [hf] <;> simp

/-- `score_live_plus_frequency`, given the already-computed live-dimension
score `live` (scale `10⁶`): add the concept's own frequency term at its spec
weight and re-apply the composite clamp. -/
def livePlusFrequency (live : Int) (frequency weight : Option Int) : Int :=
  finiteOrZero (live + clampDim frequency * saneWeight weight)

/-- "A concept with `frequency == 0` scores exactly its live-dimension score". -/
theorem livePlusFrequency_of_frequency_zero (live : Int) (frequency weight : Option Int)
    (hlive : 0 ≤ live ∧ live ≤ 1400000) (hf : clampDim frequency = 0) :
    livePlusFrequency live frequency weight = live := by
  have e : live + clampDim frequency * saneWeight weight = live := by
    rw [hf] <;> simp
  show clamp 0 1400000 (live + clampDim frequency * saneWeight weight) = live
  rw [e]
  exact clamp_eq_self hlive.1 hlive.2

/-- "Reading a concept can only raise its own score": the eviction composite is
never below the live-dimension score. -/
theorem live_le_livePlusFrequency (live : Int) (frequency weight : Option Int)
    (hlive : 0 ≤ live ∧ live ≤ 1400000) :
    live ≤ livePlusFrequency live frequency weight := by
  have ht := (term_bounds frequency weight).1
  have hmono : clamp 0 1400000 live
      ≤ clamp 0 1400000 (live + clampDim frequency * saneWeight weight) :=
    clamp_mono (by omega) (by omega)
  rw [clamp_eq_self hlive.1 hlive.2] at hmono
  show live ≤ clamp 0 1400000 (live + clampDim frequency * saneWeight weight)
  exact hmono

/-- GC's eviction composite (the live score plus the frequency term,
`score.rs`) is monotone in the concept's own frequency. -/
theorem livePlusFrequency_mono (live : Int) {f f' : Option Int} (weight : Option Int)
    (h : clampDim f ≤ clampDim f') :
    livePlusFrequency live f weight ≤ livePlusFrequency live f' weight := by
  have ht := term_mono h weight
  show clamp 0 1400000 (live + clampDim f * saneWeight weight)
    ≤ clamp 0 1400000 (live + clampDim f' * saneWeight weight)
  exact clamp_mono (by omega) (by omega)

/-- GC's eviction composite stays in the range of every score here, `[0, 1 +
MAX_BONUS]`. -/
theorem livePlusFrequency_range (live : Int) (frequency weight : Option Int) :
    0 ≤ livePlusFrequency live frequency weight
      ∧ livePlusFrequency live frequency weight ≤ 1400000 := by
  show 0 ≤ clamp 0 1400000 (live + clampDim frequency * saneWeight weight)
    ∧ clamp 0 1400000 (live + clampDim frequency * saneWeight weight) ≤ 1400000
  exact ⟨clamp_ge (by omega) _, clamp_le (by omega) _⟩

/-! ## Recency (`concept_recency`, issue #95)

Timestamps are integer milliseconds. `MIN_RECENCY_SPAN` is ten minutes. A
recency value is the fraction `num / den`; the statements below are about the
numerator and denominator, so no division appears. -/

/-- `MIN_RECENCY_SPAN = 10 · 60 · 1000` ms.
Pinned by `formal_constants_tests::score`. -/
def minRecencySpan : Int := 600000

/-- Numerator of the historical form `(last_touch − start) / span`. -/
def historicalNum (start lastTouch : Int) : Int := lastTouch - start

/-- Numerator of the floor form `1 − (end − last_touch) / denom`, over the
common denominator `denom`. -/
def floorNum (denom stop lastTouch : Int) : Int := denom - (stop - lastTouch)

/-- The two expressions for recency "agree in real arithmetic" (module docs):
over the session's own span they have the same numerator. -/
theorem historical_eq_floor (start stop lastTouch : Int) :
    historicalNum start lastTouch = floorNum (stop - start) stop lastTouch := by
  unfold historicalNum floorNum
  omega

/-- `score.rs`'s historical recency of a touch inside the session's extent is
in `[0, 1]` without help from the clamp. -/
theorem historical_in_unit (start stop lastTouch : Int)
    (h1 : start ≤ lastTouch) (h2 : lastTouch ≤ stop) :
    0 ≤ historicalNum start lastTouch ∧ historicalNum start lastTouch ≤ stop - start := by
  unfold historicalNum
  omega

/-- "A session younger than \[ten minutes\] scores every concept near 1": when
the floor widens the denominator, a touch inside the extent has recency at
least `1 − span / MIN_RECENCY_SPAN`, which is strictly positive. -/
theorem floor_recency_near_one (start stop lastTouch : Int)
    (hspan : stop - start < minRecencySpan)
    (h1 : start ≤ lastTouch) (h2 : lastTouch ≤ stop) :
    minRecencySpan - (stop - start) ≤ floorNum minRecencySpan stop lastTouch
      ∧ 0 < floorNum minRecencySpan stop lastTouch
      ∧ floorNum minRecencySpan stop lastTouch ≤ minRecencySpan := by
  unfold minRecencySpan at hspan
  unfold floorNum minRecencySpan
  omega

/-- `last_touch` is the later of `last_accessed` and `created_at`: "a concept
is never touched before it existed". -/
def lastTouch (createdAt : Int) : Option Int → Int
  | none => createdAt
  | some t => if t < createdAt then createdAt else t

/-- The last touch `score_concept_with` dates recency by (`last_accessed`,
never before `created_at`) is never earlier than the concept's creation. -/
theorem createdAt_le_lastTouch (createdAt : Int) (lastAccessed : Option Int) :
    createdAt ≤ lastTouch createdAt lastAccessed := by
  cases lastAccessed with
  | none => show createdAt ≤ createdAt; omega
  | some t =>
    show createdAt ≤ (if t < createdAt then createdAt else t)
    repeat' split
    all_goals omega

/-- Identical to `unwrap_or` whenever `last_accessed ≥ created_at` (the comment
in `score_concept_with`). -/
theorem lastTouch_eq_lastAccessed {createdAt t : Int} (h : createdAt ≤ t) :
    lastTouch createdAt (some t) = t := by
  show (if t < createdAt then createdAt else t) = t
  repeat' split
  all_goals omega

/-! ## Max-normalized dimensions (density, session activity)

Both dimensions divide a per-concept count by the session maximum of that
count. The maximum is taken over the same collection the concept belongs to,
so the ratio is at most `1` and the clamp in `score` never fires for them. -/

/-- `Iterator::max().unwrap_or(0)` over counts. -/
def listMax : List Nat → Nat
  | [] => 0
  | x :: xs => max x (listMax xs)

/-- Every count is at most the session maximum: `density` and
`session_activity` are at most `1` before clamping. -/
theorem le_listMax {l : List Nat} {x : Nat} (h : x ∈ l) : x ≤ listMax l := by
  induction l with
  | nil => simp at h
  | cons y ys ih =>
    simp only [List.mem_cons] at h
    simp only [listMax]
    cases h with
    | inl h =>
      first
        | omega
        | (rw [h]; exact Nat.le_max_left _ _)
    | inr h =>
      have hle := ih h
      first
        | omega
        | exact Nat.le_trans hle (Nat.le_max_right _ _)

/-- The maximum is attained: in a non-empty session some concept scores the
dimension at exactly `1.0` ("densest concept = `1.0`") or the maximum is `0`
and the dimension is `0.0` for everyone. -/
theorem listMax_mem_or_zero (l : List Nat) : listMax l ∈ l ∨ listMax l = 0 := by
  induction l with
  | nil => exact Or.inr rfl
  | cons y ys ih =>
    simp only [listMax, List.mem_cons]
    cases ih with
    | inl h =>
      by_cases hy : listMax ys ≤ y
      · have e : max y (listMax ys) = y := by
          first
            | omega
            | exact Nat.max_eq_left hy
        exact Or.inl (Or.inl e)
      · have e : max y (listMax ys) = listMax ys := by
          first
            | omega
            | exact Nat.max_eq_right (by omega)
        rw [e]
        exact Or.inl (Or.inr h)
    | inr h =>
      have e : max y (listMax ys) = y := by
        first
          | omega
          | exact Nat.max_eq_left (by omega)
      exact Or.inl (Or.inl e)

/-- A zero maximum means every count is zero: the `max == 0 → 0.0` guard in
`score_concept_with` loses nothing. -/
theorem all_zero_of_listMax_zero {l : List Nat} (h : listMax l = 0) {x : Nat} (hx : x ∈ l) :
    x = 0 := by
  have := le_listMax hx
  omega

end Lambo.Score
