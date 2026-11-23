import LamboProofs.Lib.List

/-
# A recall's top legs and its provisional confidence (#256, #288)

Model of `src/recall/confidence.rs`:

* `KEYWORD_HALF_SCORE` (`5.0`) and the keyword term `keyword_term`,
  `S / (S + 5)`, with a negative BM25 counted as none and a non-finite one
  giving `None`;
* `noisy_or`: the cosine rescaled at the null median,
  `b = clamp((c - m) / (1 - m), 0, 1)`, combined as `1 - (1 - a)(1 - b)`;
  `None` for a non-finite cosine or median, a median `>= 1`, or a non-finite
  BM25;
* `provisional`: the two bases, `BASIS = "noisy-or-v0"` when both legs
  reported (absent without a median) and `KEYWORD_ONLY_BASIS =
  "keyword-only-v0"` when only the keyword leg did (whatever the median);
* `top_legs`: the best finite vector cosine over the leg map's concepts of the
  graph, the keyword leg's own best BM25 (`KeywordLeg`, `src/recall/candidates.rs`)
  and the exact-name flag, both masked when the leg did not run;
* the part of phase 1 (`candidates_with_keyword_leg_as`,
  `src/recall/candidates.rs`) that `top_legs` reads or must not read: the
  `KeywordLeg` walk over the ranked search (`best_bm25`, `matched`,
  `exact_name`), the keyword loop cut to `limit * KEYWORD_OVERFETCH`
  (`KEYWORD_OVERFETCH = 4`) and the exact-name step that raises the named
  concept's keyword slot to `max(BM25, 1.0)` (#288; `Fusion/Max.lean` models
  that step's effect on ranking);
* the caller in `src/daemon/mod.rs` (`Route::Text` gets `provisional`, the
  other routes `None`) and the median it passes,
  `null_median_cosine` (`src/embed/calibration.rs`).

## Number model

No transcendental function is involved: every formula is rational, so the
model is exact. Every finite `f64` is a dyadic rational, so any finite set of
inputs is a set of integers over one common scale `u > 0`: `F64.fin x` is
`x / u`, and every theorem holds for every scale `u > 0`. `1.0` is `fin u`,
`KEYWORD_HALF_SCORE` is `5 · u`. The non-finite values (`NaN`, `±∞`) are
separate constructors, so "every input, finite or not" is a quantifier over
`F64`. `F64.max` is `f64::max` (a `NaN` gives way to the other operand).

A value in `[0, 1]` is an exact fraction `Frac` (`num / den`, `den > 0`),
compared by cross-multiplication (`Frac.le`, `Frac.eqv`). The fractions are
the exact values of the Rust expressions:

* `keyword_term`: `s' / (s' + 5u)` with `s' = max(s, 0)`;
* the rescaled cosine: since `1 - m > 0`, `clamp((c - m) / (1 - m), 0, 1)` is
  `clamp(c - m, 0, u - m) / (u - m)` (`vectorTerm_spec` gives the three
  regimes);
* `1 - (1 - a)(1 - b)` with `a = an / ad`, `b = bn / bd` is
  `(ad·bd - (ad - an)(bd - bn)) / (ad·bd)` (`noisyOrCombine`).

Not captured: `f64` rounding. Correct rounding makes `fl` monotone in the
exact result of each operation, and that is all the arguments below use (`-`
and `/` are not monotone in their second operand; where that operand varies,
the argument says so).

* **The range `[0, 1]` survives rounding.** Each step's exact result lies in
  `[0, 1]`, and `0` and `1` are representable, so the rounded result does too
  (`s ≤ fl(s + 5)` since `s` is representable, so `fl(s / fl(s + 5)) ≤ 1`).
* **Monotonicity in the cosine survives rounding.** With the BM25 and the
  median fixed, every step is a rounded operation applied to the varying
  quantity with the other operand fixed, and each is monotone or antitone in
  it: `c - m`, division by the fixed `1 - m > 0`, the clamp, `1 - b`, the
  product with the fixed `1 - a ≥ 0`, and the final `1 - x`.
* **Monotonicity in BM25 is exact-arithmetic only.** `S / (S + 5)` divides two
  rounded quantities that both grow, so two BM25s an ulp apart can in
  principle come out an ulp out of order (`keywordTerm_mono`,
  `noisyOr_mono_bm25`, `provisional_mono_bm25`); the Rust test checks
  integer steps only.
* **"Never below either term" and the keyword-only comparison are
  exact-arithmetic only** (`noisyOr_ge_keyword`, `noisyOr_ge_vector`,
  `keywordOnly_le_noisyOr`, `keywordOnly_eq_noisyOr_at_null`): in `f64` they
  can be off by an ulp. For example `noisy_or(1.0, 0.10, 0.36)` is
  `0.16666666666666663`, below `keyword_term(1.0) = 0.16666666666666666`,
  although the cosine is under the median (`b = 0`).
* **The identities** (`1/2` at `S = 5`, the `0.75` of #174's test) hold up to
  rounding; the Rust tests compare to within `1e-12`.
* **"Strictly below `1`" is exact-arithmetic only.** The keyword term alone
  (`keywordTerm_lt_one`) rounds to `1.0` once `S` is near `7.2e16 ≈ 2^56`, a
  BM25 no index produces. For the two-leg value, the "if" direction of
  `noisyOr_eq_one_iff` (cosine at least `1` gives `1`) survives rounding, but
  the "only if" direction does not: a cosine within an ulp or two of `1`
  can round the rescaled term to exactly `1.0`. At the BGE-M3 median
  `m = 0.3586` and `c = 1 - 2^-53`, `(c - m) / (1 - m)` rounds to `1.0`, so
  `noisy_or(0.0, c, 0.3586) = 1.0` with `c < 1`; likewise
  `noisy_or(20.0, c, 0.36) = 1.0`.

Overflow of `c - m` to `+∞` (inputs near `f64::MAX`) gives `+∞ / (1 - m)`,
which clamps to `1` in Rust as in the exact model; `1 - m` and `S + 5` cannot
overflow. The BM25 itself (its `idf` uses `ln`) and the cosine are inputs, so
no transcendental function needs an abstract model here.

## Results, and what they correspond to

The module doc of `confidence.rs` and the field docs of `TopLegs` and
`ProvisionalConfidence` (`src/types/mod.rs`) are the contract; the Rust tests
named are in `src/recall/confidence.rs` (`the_arithmetic_is_the_noisy_or_recorded_on_174`,
`the_basis_follows_the_legs_and_the_value_is_never_guessed`,
`top_legs_are_the_maxima_and_ignore_the_recent_leg`,
`a_vector_candidate_outside_the_graph_is_not_a_top_leg`) and
`src/memory/tests/top_legs.rs`.

* `provisional_inUnit`: **"The combined value, in `[0, 1]`"**
  (`ProvisionalConfidence::value`), for every `TopLegs` and every median,
  finite or not. `keywordTerm_lt_one`: the keyword term alone never reaches 1
  (exact arithmetic only; see the number model).
* `keywordTerm_half`, `keywordTerm_of_nonpos`, `keywordTerm_tendsto_one`:
  `S / (S + 5)` is `1/2` at `KEYWORD_HALF_SCORE`, `0` at and below `0` ("A
  negative score counts as none"), and within `1/n` of `1` from `S = 5n` on.
* `keywordTerm_mono`, `noisyOr_mono_bm25`, `noisyOr_mono_cosine`,
  `provisional_mono_bm25`, `provisional_mono_cosine`: **monotone in each leg**
  (the test's "monotone in each leg"), with the basis unchanged. In `f64` the
  cosine statements survive rounding; the BM25 ones hold up to an ulp.
* `noisyOr_ge_keyword`, `noisyOr_ge_vector`, `noisyOrCombine_num`,
  `noisyOrCombine_comm`, `noisyOr_eq_one_iff`: the noisy-OR is never below
  either term, equals `a + b - ab`, is symmetric, and is `1` exactly when the
  cosine is at least `1` ("one at 1 is 1"); keyword evidence alone never
  makes it `1`. `vectorTerm_spec`: "A cosine at or under the null median adds
  nothing". The two "never below" statements and the "only if" direction of
  `noisyOr_eq_one_iff` are exact-arithmetic only (the number model gives
  `f64` counterexamples).
* `keywordOnly_eq_noisyOr_at_null`, `keywordOnly_le_noisyOr`: **"It is the
  two-leg value with `b = 0`, a lower quantity than the two-leg value would
  have been"**, in exact arithmetic; in `f64` "lower" holds only up to an ulp
  (`noisy_or(1.0, 0.10, 0.36)` is an ulp below `keyword_term(1.0)`).
* `provisional_basis_iff`, `provisional_keywordOnly`: **the keyword-only basis
  applies exactly when `vector_cosine` is `None`** (no vector search, a failed
  embed or vector read, nothing embedded, every cosine non-finite, every
  vector candidate outside the graph: `topLegs_vector_none_iff`), whatever
  the median.
* `provisional_twoLeg_without_median`, `recall_twoLeg_absent_off_nonReference`:
  **the two-leg value is absent, never guessed, without a measured null
  median** (#256); `recall_keywordOnly_without_median`: a recall with no
  vector leg still reports the keyword-only value without one (#256's
  acceptance, the degraded-vector test in `top_legs.rs`).
* `provisional_exactName`, `exactName_zero_overlap`: **`confidence.value`
  ignores `exact_name_match` for both bases** (#256): a name query with no
  word overlap reports `bm25` 0, value 0, beside `exact_name_match: true`.
* `topLegs_bm25`, `keywordLeg_bm25_raw`, `keywordLeg_bm25_best`,
  `topLegs_keywordPhase`, `bm25_not_name_floor`: **`top_legs.bm25` is the raw
  best BM25 read before the exact-name step** (#288): it is a concept's score
  as the ranked search gave it, the best one when the search is best-first,
  the keyword slots (the cap loop and the name step) never reach `top_legs`,
  and on a store whose named concept scores `0.4` the slot holds `1.0` while
  `bm25` stays `0.4`. Since the cap loop is the only place `limit` enters,
  `bm25` is the same at every `top_k` (`topLegs_keywordPhase`).
* `topLegs_vector_max`, `topLegs_perm`, `topLegs_congr`: the cosine is the
  maximum of the finite cosines of in-graph concepts (out-of-graph and
  non-finite ones skipped, the recent leg ignored), independent of the
  `HashMap`'s order.
* `keywordLeg_bm25_finite`, `topLegs_vector_finite`, `provisional_isSome_of_built`:
  "Both leg maxima it reads are finite by construction ... so the non-finite
  guards in `noisy_or` and the keyword term are defensive": on what
  `top_legs` builds, with a finite median below `1`, a confidence is reported
  whenever the keyword leg reported a score.
* `provisional_default`, `recall_nonText`: no keyword leg (a refused recall or
  a traversal answer, both of which leave `TopLegs::default()`), and a recall
  by image or vector or a layer's side, report no confidence.

## Left out

The search itself (`search_ranked`, `src/graph/index.rs`, modelled as its
ranked list; it is score-descending with only positive scores, assumed as
`BestFirst` where a theorem needs it), name normalization and lookup (the
model takes the named concept, already filtered to concepts, as `nameTarget`),
the recent leg's construction (its slot is carried and ignored), the vector
search and its approximate indexes (modelled as the vector slots of the leg
map; the merge of vector hits into the map after the keyword phase is not
modelled, `topLegs_congr` and `topLegs_perm` covering what `top_legs` reads),
`calibration::lookup` and `is_reference` (inputs of `nullMedian`), the
daemon's `embedding.and_then(|(_, contract)| null_median_cosine(..))`
(`src/daemon/mod.rs`, the `Route::Text` arm), which passes no median at all
when the query embed failed, under any contract (harmless: such a recall has
no vector leg and takes the keyword-only path, which needs none), the
layered recall's `layer_top_legs`, and the serialized wire shape. The leg
map is a list of entries with distinct keys standing for the `HashMap`;
`upsertKeyword` is `entry(id).or_default()` followed by a write to the
keyword slot. `noisyOr_none` records the `None` cases of `noisy_or`.
-/

namespace Lambo.Confidence

open Lambo.Lib (foldl_max_ge_init foldl_max_ge foldl_max_mem)

/-! ## `f64` values -/

/-- An `f64` at scale `u`: `fin x` is `x / u`. -/
inductive F64 where
  | fin (x : Int)
  | nan
  | posInf
  | negInf
deriving DecidableEq, Repr

/-- `f64::is_finite`. -/
def F64.isFinite : F64 → Bool
  | .fin _ => true
  | _ => false

/-- `f64::max`: a `NaN` gives way to the other operand. -/
def F64.max : F64 → F64 → F64
  | .nan, b => b
  | a, .nan => a
  | .posInf, _ => .posInf
  | _, .posInf => .posInf
  | .negInf, b => b
  | a, .negInf => a
  | .fin a, .fin b => .fin (Max.max a b)

/-- `merge_max(slot, score)` (`candidates.rs`). -/
def mergeMax (slot : Option F64) (score : F64) : F64 :=
  match slot with
  | some previous => previous.max score
  | none => score

/-! ## Exact fractions -/

/-- `num / den`; every fraction the model builds has `den > 0`. -/
structure Frac where
  num : Int
  den : Int
deriving DecidableEq, Repr

/-- `p ≤ q` for positive denominators. -/
def Frac.le (p q : Frac) : Prop := p.num * q.den ≤ q.num * p.den

instance (p q : Frac) : Decidable (p.le q) := inferInstanceAs (Decidable (_ ≤ _))

/-- `p = q` as numbers. -/
def Frac.eqv (p q : Frac) : Prop := p.num * q.den = q.num * p.den

instance (p q : Frac) : Decidable (p.eqv q) := inferInstanceAs (Decidable (_ = _))

/-- `p ∈ [0, 1]`, with a positive denominator. -/
def Frac.InUnit (p : Frac) : Prop := 0 < p.den ∧ 0 ≤ p.num ∧ p.num ≤ p.den

instance (p : Frac) : Decidable p.InUnit := by unfold Frac.InUnit; infer_instance

/-! ## The keyword term -/

/-- `KEYWORD_HALF_SCORE`, in units of `1.0`.
Pinned by `formal_constants_tests::confidence`. -/
def keywordHalfScore : Int := 5

/-- `S / (S + KEYWORD_HALF_SCORE)` on a finite BM25 `s` (at scale `u`), with
`s.max(0.0)`. -/
def keywordTermFin (u s : Int) : Frac :=
  ⟨max s 0, max s 0 + keywordHalfScore * u⟩

/-- `keyword_term`: `None` for a non-finite score. -/
def keywordTerm (u : Int) : F64 → Option Frac
  | .fin s => some (keywordTermFin u s)
  | _ => none

theorem keywordTermFin_inUnit {u : Int} (hu : 0 < u) (s : Int) :
    (keywordTermFin u s).InUnit := by
  simp only [Frac.InUnit, keywordTermFin, keywordHalfScore]
  omega

/-- The keyword term is strictly below `1`: keyword evidence alone never
makes it `1`. -/
theorem keywordTerm_lt_one {u : Int} (hu : 0 < u) (s : Int) :
    (keywordTermFin u s).num < (keywordTermFin u s).den := by
  simp only [keywordTermFin, keywordHalfScore]
  omega

/-- At and below `0` the term is `0` ("A negative score counts as none"). -/
theorem keywordTerm_of_nonpos {u s : Int} (hs : s ≤ 0) : (keywordTermFin u s).num = 0 := by
  simp only [keywordTermFin]
  omega

/-- **`1/2` at `KEYWORD_HALF_SCORE`.** -/
theorem keywordTerm_half {u : Int} (hu : 0 < u) :
    keywordTerm u (.fin (keywordHalfScore * u)) = some ⟨5 * u, 10 * u⟩ ∧
      (Frac.eqv ⟨5 * u, 10 * u⟩ ⟨1, 2⟩) := by
  refine ⟨?_, ?_⟩
  · simp only [keywordTerm, keywordTermFin, keywordHalfScore, Option.some.injEq, Frac.mk.injEq]
    omega
  · simp only [Frac.eqv]
    omega

/-- **Towards `1` at large BM25**: from `S = 5n` on, `1 - a ≤ 1/n`. -/
theorem keywordTerm_tendsto_one {u : Int} (hu : 0 < u) (n s : Int) (hn : 0 < n)
    (hs : n * (keywordHalfScore * u) ≤ s) :
    ((keywordTermFin u s).den - (keywordTermFin u s).num) * n ≤ (keywordTermFin u s).den := by
  simp only [keywordTermFin]
  generalize hK : keywordHalfScore * u = K at hs ⊢
  have hK0 : 0 < K := by subst hK; simp [keywordHalfScore]; omega
  have h0 : 0 ≤ n * K := Int.mul_nonneg (by omega) (by omega)
  have hm : max s 0 = s := by omega
  rw [hm, show s + K - s = K by omega, Int.mul_comm]
  omega

/-- **The keyword term is monotone in BM25.** -/
theorem keywordTerm_mono {u : Int} (hu : 0 < u) {s t : Int} (h : s ≤ t) :
    (keywordTermFin u s).le (keywordTermFin u t) := by
  simp only [Frac.le, keywordTermFin]
  generalize hs' : max s 0 = s'
  generalize ht' : max t 0 = t'
  have hle : s' ≤ t' := by omega
  have hK : 0 ≤ keywordHalfScore * u := by simp [keywordHalfScore]; omega
  have := Int.mul_le_mul_of_nonneg_left hle hK
  grind

/-! ## The vector term -/

/-- `f64::clamp(lo, hi)` on a finite value. -/
def clamp (lo hi v : Int) : Int :=
  if v < lo then lo else if hi < v then hi else v

/-- `((c - m) / (1 - m)).clamp(0.0, 1.0)` at scale `u`, for `m < 1`. -/
def vectorTerm (u c m : Int) : Frac :=
  ⟨clamp 0 (u - m) (c - m), u - m⟩

theorem vectorTerm_inUnit {u c m : Int} (hm : m < u) : (vectorTerm u c m).InUnit := by
  simp only [Frac.InUnit, vectorTerm, clamp]
  split <;> (try split) <;> omega

/-- The three regimes of the rescaled cosine: `0` at or under the null median,
`1` at or over `1`, and `(c - m) / (1 - m)` between. -/
theorem vectorTerm_spec {u c m : Int} (hm : m < u) :
    (c ≤ m → (vectorTerm u c m).num = 0) ∧
    (u ≤ c → (vectorTerm u c m).num = (vectorTerm u c m).den) ∧
    (m ≤ c → c ≤ u → (vectorTerm u c m).num = c - m) := by
  simp only [vectorTerm, clamp]
  refine ⟨?_, ?_, ?_⟩ <;> intros <;> split <;> (try split) <;> omega

/-- The rescaled cosine is monotone in the cosine (same denominator). -/
theorem vectorTerm_mono {u m c c' : Int} (hm : m < u) (h : c ≤ c') :
    (vectorTerm u c m).le (vectorTerm u c' m) := by
  simp only [Frac.le, vectorTerm]
  have : clamp 0 (u - m) (c - m) ≤ clamp 0 (u - m) (c' - m) := by
    simp only [clamp]; split <;> split <;> (try split) <;> (try split) <;> omega
  exact Int.mul_le_mul_of_nonneg_right this (by omega)

/-! ## The noisy-OR -/

/-- `1 - (1 - a)(1 - b)`, exactly. -/
def noisyOrCombine (a b : Frac) : Frac :=
  ⟨a.den * b.den - (a.den - a.num) * (b.den - b.num), a.den * b.den⟩

/-- `noisy_or(bm25, vector_cosine, null_median)`. -/
def noisyOr (u : Int) (bm25 cosine median : F64) : Option Frac :=
  match cosine, median with
  | .fin c, .fin m =>
    if u ≤ m then none
    else (keywordTerm u bm25).map fun a => noisyOrCombine a (vectorTerm u c m)
  | _, _ => none

/-- `confidence.rs` combines the two legs as the noisy-OR `a + b − ab`. -/
theorem noisyOrCombine_num (a b : Frac) :
    (noisyOrCombine a b).num = a.num * b.den + b.num * a.den - a.num * b.num := by
  simp only [noisyOrCombine]
  grind

/-- The noisy-OR is symmetric. -/
theorem noisyOrCombine_comm (a b : Frac) : noisyOrCombine a b = noisyOrCombine b a := by
  simp only [noisyOrCombine, Frac.mk.injEq]
  constructor
  · rw [Int.mul_comm a.den, Int.mul_comm (a.den - a.num)]
  · exact Int.mul_comm _ _

theorem noisyOrCombine_inUnit {a b : Frac} (ha : a.InUnit) (hb : b.InUnit) :
    (noisyOrCombine a b).InUnit := by
  obtain ⟨had, han, hanl⟩ := ha
  obtain ⟨hbd, hbn, hbnl⟩ := hb
  simp only [Frac.InUnit, noisyOrCombine]
  have h1 : (a.den - a.num) * (b.den - b.num) ≤ a.den * b.den :=
    Int.mul_le_mul (by omega) (by omega) (by omega) (by omega)
  have h2 : 0 ≤ (a.den - a.num) * (b.den - b.num) := Int.mul_nonneg (by omega) (by omega)
  have h3 : 0 < a.den * b.den := Int.mul_pos had hbd
  omega

/-- **Never below the first term.** -/
theorem le_noisyOrCombine_left {a b : Frac} (ha : a.InUnit) (hb : b.InUnit) :
    a.le (noisyOrCombine a b) := by
  obtain ⟨had, han, hanl⟩ := ha
  obtain ⟨hbd, hbn, hbnl⟩ := hb
  simp only [Frac.le, noisyOrCombine]
  have : 0 ≤ a.den * (b.num * (a.den - a.num)) :=
    Int.mul_nonneg (by omega) (Int.mul_nonneg hbn (by omega))
  grind

/-- **Never below the second term.** -/
theorem le_noisyOrCombine_right {a b : Frac} (ha : a.InUnit) (hb : b.InUnit) :
    b.le (noisyOrCombine a b) := by
  rw [noisyOrCombine_comm]
  exact le_noisyOrCombine_left hb ha

/-- The noisy-OR is monotone in its first term. -/
theorem noisyOrCombine_mono_left {a a' b : Frac} (ha : a.InUnit) (ha' : a'.InUnit)
    (hb : b.InUnit) (h : a.le a') : (noisyOrCombine a b).le (noisyOrCombine a' b) := by
  obtain ⟨had, han, hanl⟩ := ha
  obtain ⟨had', han', hanl'⟩ := ha'
  obtain ⟨hbd, hbn, hbnl⟩ := hb
  simp only [Frac.le, noisyOrCombine] at h ⊢
  have : 0 ≤ b.den * ((b.den - b.num) * (a'.num * a.den - a.num * a'.den)) :=
    Int.mul_nonneg (by omega) (Int.mul_nonneg (by omega) (by omega))
  grind

/-- The noisy-OR is monotone in its second term. -/
theorem noisyOrCombine_mono_right {a b b' : Frac} (ha : a.InUnit) (hb : b.InUnit)
    (hb' : b'.InUnit) (h : b.le b') : (noisyOrCombine a b).le (noisyOrCombine a b') := by
  rw [noisyOrCombine_comm a b, noisyOrCombine_comm a b']
  exact noisyOrCombine_mono_left hb hb' ha h

/-- Unfolding `noisyOr` on finite inputs with a usable median. -/
theorem noisyOr_fin {u s c m : Int} (hm : m < u) :
    noisyOr u (.fin s) (.fin c) (.fin m) =
      some (noisyOrCombine (keywordTermFin u s) (vectorTerm u c m)) := by
  have : ¬ u ≤ m := by omega
  simp [noisyOr, keywordTerm, this]

/-- What `noisyOr` needs to report: a finite BM25, a finite cosine and a finite
median below `1`. -/
theorem noisyOr_eq_some {u : Int} {bm25 cosine median : F64} {v : Frac}
    (h : noisyOr u bm25 cosine median = some v) :
    ∃ s c m, bm25 = .fin s ∧ cosine = .fin c ∧ median = .fin m ∧ m < u ∧
      v = noisyOrCombine (keywordTermFin u s) (vectorTerm u c m) := by
  unfold noisyOr at h
  split at h
  · rename_i c m
    split at h
    · cases h
    · cases bm25 with
      | fin s =>
        simp only [keywordTerm, Option.map_some, Option.some.injEq] at h
        exact ⟨s, c, m, rfl, rfl, rfl, by omega, h.symm⟩
      | _ => simp [keywordTerm] at h
  · cases h

/-- `provisional_confidence`'s two-leg value is `None` for any non-finite
input and for a null median at or above `1`. -/
theorem noisyOr_none {u : Int} {bm25 cosine median : F64}
    (h : bm25.isFinite = false ∨ cosine.isFinite = false ∨ median.isFinite = false ∨
      ∃ m, median = .fin m ∧ u ≤ m) :
    noisyOr u bm25 cosine median = none := by
  cases hv : noisyOr u bm25 cosine median with
  | none => rfl
  | some v =>
    obtain ⟨s, c, m, rfl, rfl, rfl, hm, -⟩ := noisyOr_eq_some hv
    simp only [F64.isFinite] at h
    rcases h with h | h | h | ⟨m', hm', hle⟩
    · cases h
    · cases h
    · cases h
    · cases hm'; omega

/-- **The value is in `[0, 1]`** whenever it is reported. -/
theorem noisyOr_inUnit {u : Int} (hu : 0 < u) {bm25 cosine median : F64} {v : Frac}
    (h : noisyOr u bm25 cosine median = some v) : v.InUnit := by
  obtain ⟨s, c, m, -, -, -, hm, rfl⟩ := noisyOr_eq_some h
  exact noisyOrCombine_inUnit (keywordTermFin_inUnit hu s) (vectorTerm_inUnit hm)

/-- **Never below the keyword term.** -/
theorem noisyOr_ge_keyword {u : Int} (hu : 0 < u) {s c m : Int} (hm : m < u) :
    (keywordTermFin u s).le (noisyOrCombine (keywordTermFin u s) (vectorTerm u c m)) :=
  le_noisyOrCombine_left (keywordTermFin_inUnit hu s) (vectorTerm_inUnit hm)

/-- **Never below the rescaled cosine.** -/
theorem noisyOr_ge_vector {u : Int} (hu : 0 < u) {s c m : Int} (hm : m < u) :
    (vectorTerm u c m).le (noisyOrCombine (keywordTermFin u s) (vectorTerm u c m)) :=
  le_noisyOrCombine_right (keywordTermFin_inUnit hu s) (vectorTerm_inUnit hm)

/-- **Monotone in BM25.** -/
theorem noisyOr_mono_bm25 {u : Int} (hu : 0 < u) {s t c m : Int} (hm : m < u) (h : s ≤ t) :
    (noisyOrCombine (keywordTermFin u s) (vectorTerm u c m)).le
      (noisyOrCombine (keywordTermFin u t) (vectorTerm u c m)) :=
  noisyOrCombine_mono_left (keywordTermFin_inUnit hu s) (keywordTermFin_inUnit hu t)
    (vectorTerm_inUnit hm) (keywordTerm_mono hu h)

/-- **Monotone in the cosine.** -/
theorem noisyOr_mono_cosine {u : Int} (hu : 0 < u) {s c c' m : Int} (hm : m < u)
    (h : c ≤ c') :
    (noisyOrCombine (keywordTermFin u s) (vectorTerm u c m)).le
      (noisyOrCombine (keywordTermFin u s) (vectorTerm u c' m)) :=
  noisyOrCombine_mono_right (keywordTermFin_inUnit hu s) (vectorTerm_inUnit hm)
    (vectorTerm_inUnit hm) (vectorTerm_mono hm h)

/-- **`1` exactly when the cosine is at least `1`.** Keyword evidence alone
never makes the two-leg value `1`. -/
theorem noisyOr_eq_one_iff {u : Int} (hu : 0 < u) {s c m : Int} (hm : m < u) :
    (noisyOrCombine (keywordTermFin u s) (vectorTerm u c m)).num =
      (noisyOrCombine (keywordTermFin u s) (vectorTerm u c m)).den ↔ u ≤ c := by
  have ha := keywordTerm_lt_one hu s
  simp only [noisyOrCombine]
  constructor
  · intro h
    have h0 : ((keywordTermFin u s).den - (keywordTermFin u s).num) *
        ((vectorTerm u c m).den - (vectorTerm u c m).num) = 0 := by omega
    rcases Int.mul_eq_zero.mp h0 with h1 | h1
    · omega
    · simp only [vectorTerm, clamp] at h1
      split at h1 <;> (try split at h1) <;> omega
  · intro h
    have h1 : (vectorTerm u c m).den - (vectorTerm u c m).num = 0 := by
      have := (vectorTerm_spec (c := c) hm).2.1 h
      omega
    rw [h1, Int.mul_zero, Int.sub_zero]

/-- **The keyword-only value is the two-leg value with `b = 0`**: at a cosine
at or under the null median the two agree. -/
theorem keywordOnly_eq_noisyOr_at_null {u : Int} {s c m : Int} (hm : m < u) (hc : c ≤ m) :
    (keywordTermFin u s).eqv (noisyOrCombine (keywordTermFin u s) (vectorTerm u c m)) := by
  have h0 := (vectorTerm_spec (c := c) hm).1 hc
  simp only [Frac.eqv, noisyOrCombine, h0, Int.sub_zero]
  grind

/-- **A lower quantity than the two-leg value** for the same store. -/
theorem keywordOnly_le_noisyOr {u : Int} (hu : 0 < u) {s c m : Int} (hm : m < u) :
    (keywordTermFin u s).le (noisyOrCombine (keywordTermFin u s) (vectorTerm u c m)) :=
  noisyOr_ge_keyword hu hm

/-! ## `provisional` and the two bases -/

/-- `TopLegs` (`src/types/mod.rs`). -/
structure TopLegs where
  bm25 : Option F64
  vectorCosine : Option F64
  exactNameMatch : Bool
deriving DecidableEq, Repr

/-- `TopLegs::default()`: no leg ran (a refused recall, a traversal answer). -/
def TopLegs.default : TopLegs := ⟨none, none, false⟩

/-- The two bases. -/
inductive Basis where
  /-- `BASIS`. -/
  | noisyOrV0
  /-- `KEYWORD_ONLY_BASIS`. -/
  | keywordOnlyV0
deriving DecidableEq, Repr

/-- The wire string of a basis.
Pinned by `formal_constants_tests::confidence`. -/
def Basis.name : Basis → String
  | .noisyOrV0 => "noisy-or-v0"
  | .keywordOnlyV0 => "keyword-only-v0"

/-- `ProvisionalConfidence`. -/
structure Provisional where
  value : Frac
  basis : Basis
  provisional : Bool
deriving DecidableEq, Repr

/-- `provisional(top, null_median)`:

```rust
let bm25 = top.bm25?;
let (value, basis) = match top.vector_cosine {
    Some(cosine) => (noisy_or(bm25, cosine, null_median?)?, BASIS),
    None => (keyword_term(bm25)?, KEYWORD_ONLY_BASIS),
};
``` -/
def provisional (u : Int) (top : TopLegs) (median : Option F64) : Option Provisional :=
  match top.bm25 with
  | none => none
  | some bm25 =>
    match top.vectorCosine with
    | some c =>
      match median with
      | none => none
      | some m => (noisyOr u bm25 c m).map fun v => ⟨v, .noisyOrV0, true⟩
    | none => (keywordTerm u bm25).map fun v => ⟨v, .keywordOnlyV0, true⟩

/-- What `provisional` reports, case by case: a finite BM25, and either no
vector leg and the keyword-only value, or a finite cosine, a finite median
below `1` and the noisy-OR. -/
theorem provisional_eq_some {u : Int} {top : TopLegs} {median : Option F64} {p : Provisional}
    (h : provisional u top median = some p) :
    ∃ s, top.bm25 = some (.fin s) ∧
      ((top.vectorCosine = none ∧ p = ⟨keywordTermFin u s, .keywordOnlyV0, true⟩) ∨
       (∃ c m, top.vectorCosine = some (.fin c) ∧ median = some (.fin m) ∧ m < u ∧
         p = ⟨noisyOrCombine (keywordTermFin u s) (vectorTerm u c m), .noisyOrV0, true⟩)) := by
  unfold provisional at h
  cases hb : top.bm25 with
  | none => simp [hb] at h
  | some b =>
    cases hv : top.vectorCosine with
    | none =>
      simp only [hb, hv] at h
      cases b with
      | fin s =>
        simp only [keywordTerm, Option.map_some, Option.some.injEq] at h
        exact ⟨s, rfl, Or.inl ⟨rfl, h.symm⟩⟩
      | _ => simp [keywordTerm] at h
    | some c =>
      simp only [hb, hv] at h
      cases median with
      | none => simp at h
      | some m =>
        cases hn : noisyOr u b c m with
        | none => simp [hn] at h
        | some v =>
          simp only [hn, Option.map_some, Option.some.injEq] at h
          obtain ⟨s, c', m', rfl, rfl, rfl, hm, rfl⟩ := noisyOr_eq_some hn
          exact ⟨s, rfl, Or.inr ⟨c', m', rfl, rfl, hm, h.symm⟩⟩

/-- **The value is in `[0, 1]`** for every `TopLegs` and every median, finite
or not, and the object says `provisional: true`. -/
theorem provisional_inUnit {u : Int} (hu : 0 < u) {top : TopLegs} {median : Option F64}
    {p : Provisional} (h : provisional u top median = some p) :
    p.value.InUnit ∧ p.provisional = true := by
  rcases provisional_eq_some h with ⟨s, -, ⟨-, rfl⟩ | ⟨c, m, -, -, hm, rfl⟩⟩
  · exact ⟨keywordTermFin_inUnit hu s, rfl⟩
  · exact ⟨noisyOrCombine_inUnit (keywordTermFin_inUnit hu s) (vectorTerm_inUnit hm), rfl⟩

/-- **The basis is keyword-only exactly when `vector_cosine` is `None`.** -/
theorem provisional_basis_iff {u : Int} {top : TopLegs} {median : Option F64}
    {p : Provisional} (h : provisional u top median = some p) :
    p.basis = .keywordOnlyV0 ↔ top.vectorCosine = none := by
  rcases provisional_eq_some h with ⟨s, -, ⟨hv, rfl⟩ | ⟨c, m, hv, -, -, rfl⟩⟩
  · exact ⟨fun _ => hv, fun _ => rfl⟩
  · simp [hv]

/-- **With the keyword leg alone, the keyword-only value, whatever the median**
(present, absent, finite or not). -/
theorem provisional_keywordOnly (u : Int) {top : TopLegs} {s : Int}
    (hb : top.bm25 = some (.fin s)) (hv : top.vectorCosine = none) (median : Option F64) :
    provisional u top median = some ⟨keywordTermFin u s, .keywordOnlyV0, true⟩ := by
  simp [provisional, hb, hv, keywordTerm]

/-- **Both legs, no median: absent, never guessed** (nor replaced by the
keyword-only value). -/
theorem provisional_twoLeg_without_median (u : Int) {top : TopLegs} {c : F64}
    (hv : top.vectorCosine = some c) : provisional u top none = none := by
  unfold provisional
  split
  · rfl
  · simp [hv]

/-- Both legs and a usable median: the noisy-OR. -/
theorem provisional_twoLeg {u : Int} {top : TopLegs} {s c m : Int}
    (hb : top.bm25 = some (.fin s)) (hv : top.vectorCosine = some (.fin c)) (hm : m < u) :
    provisional u top (some (.fin m)) =
      some ⟨noisyOrCombine (keywordTermFin u s) (vectorTerm u c m), .noisyOrV0, true⟩ := by
  simp [provisional, hb, hv, noisyOr_fin hm]

/-- No keyword leg: no confidence of either basis. -/
theorem provisional_no_keyword (u : Int) {top : TopLegs} (hb : top.bm25 = none)
    (median : Option F64) : provisional u top median = none := by
  simp [provisional, hb]

/-- A recall that ran no leg (refused, or answered by traversal) reports no
confidence. -/
theorem provisional_default (u : Int) (median : Option F64) :
    provisional u TopLegs.default median = none :=
  provisional_no_keyword u rfl median

/-- **`confidence.value` ignores `exact_name_match`** (#256), for both bases:
the whole result is the same whatever the flag. -/
theorem provisional_exactName (u : Int) (top : TopLegs) (e : Bool) (median : Option F64) :
    provisional u { top with exactNameMatch := e } median = provisional u top median := rfl

/-- **Monotone in BM25**, the basis unchanged. -/
theorem provisional_mono_bm25 {u : Int} (hu : 0 < u) {top : TopLegs} {s t : Int}
    (hb : top.bm25 = some (.fin s)) (hst : s ≤ t) {median : Option F64} {p : Provisional}
    (h : provisional u top median = some p) :
    ∃ p', provisional u { top with bm25 := some (.fin t) } median = some p' ∧
      p'.basis = p.basis ∧ p.value.le p'.value := by
  rcases provisional_eq_some h with ⟨s', hb', ⟨hv, rfl⟩ | ⟨c, m, hv, rfl, hm, rfl⟩⟩
  · rw [hb] at hb'; cases hb'
    exact ⟨_, provisional_keywordOnly u (top := { top with bm25 := some (.fin t) }) rfl hv median,
      rfl, keywordTerm_mono hu hst⟩
  · rw [hb] at hb'; cases hb'
    exact ⟨_, provisional_twoLeg (top := { top with bm25 := some (.fin t) }) rfl hv hm, rfl,
      noisyOr_mono_bm25 hu hm hst⟩

/-- **Monotone in the cosine**, the basis unchanged. -/
theorem provisional_mono_cosine {u : Int} (hu : 0 < u) {top : TopLegs} {c c' : Int}
    (hv : top.vectorCosine = some (.fin c)) (hcc : c ≤ c') {median : Option F64}
    {p : Provisional} (h : provisional u top median = some p) :
    ∃ p', provisional u { top with vectorCosine := some (.fin c') } median = some p' ∧
      p'.basis = p.basis ∧ p.value.le p'.value := by
  rcases provisional_eq_some h with ⟨s, hb, ⟨hv', -⟩ | ⟨c₀, m, hv', rfl, hm, rfl⟩⟩
  · rw [hv] at hv'; cases hv'
  · rw [hv] at hv'; cases hv'
    exact ⟨_, provisional_twoLeg (top := { top with vectorCosine := some (.fin c') }) hb rfl hm,
      rfl, noisyOr_mono_cosine hu hm hcc⟩

/-! ### The Rust tests, at scale `u = 100` -/

/-- `p` is reported, with value `n / d` and basis `b`. -/
def Reports (p : Option Provisional) (n d : Int) (b : Basis) : Prop :=
  match p with
  | some p => p.value.eqv ⟨n, d⟩ ∧ p.basis = b ∧ p.provisional = true
  | none => False

instance (p : Option Provisional) (n d : Int) (b : Basis) : Decidable (Reports p n d b) := by
  unfold Reports; split <;> infer_instance

/-- `top(bm25, vector_cosine)` of the Rust tests. -/
def top (bm25 vectorCosine : Option F64) : TopLegs := ⟨bm25, vectorCosine, false⟩

/-- `the_arithmetic_is_the_noisy_or_recorded_on_174`: `a = 0.5`, `b = 0.5`,
value `0.75`; each term alone; a cosine at or under the median adds nothing,
one at `1` is `1`; a negative keyword score counts as none. -/
example :
    (noisyOr 100 (.fin 500) (.fin 68) (.fin 36)).map (fun v => decide (v.eqv ⟨3, 4⟩)) = some true ∧
    (noisyOr 100 (.fin 1500) (.fin 36) (.fin 36)).map (fun v => decide (v.eqv ⟨3, 4⟩)) = some true ∧
    (noisyOr 100 (.fin 0) (.fin 68) (.fin 36)).map (fun v => decide (v.eqv ⟨1, 2⟩)) = some true ∧
    (noisyOr 100 (.fin 0) (.fin 10) (.fin 36)).map (fun v => decide (v.eqv ⟨0, 1⟩)) = some true ∧
    (noisyOr 100 (.fin 0) (.fin (-50)) (.fin 36)).map (fun v => decide (v.eqv ⟨0, 1⟩)) = some true ∧
    (noisyOr 100 (.fin 0) (.fin 100) (.fin 36)).map (fun v => decide (v.eqv ⟨1, 1⟩)) = some true ∧
    (noisyOr 100 (.fin (-300)) (.fin 36) (.fin 36)).map (fun v => decide (v.eqv ⟨0, 1⟩)) = some true := by
  decide

/-- `the_basis_follows_the_legs_and_the_value_is_never_guessed`. -/
example :
    Reports (provisional 100 (top (some (.fin 400)) (some (.fin 70))) (some (.fin 36)))
      (Frac.num (noisyOrCombine (keywordTermFin 100 400) (vectorTerm 100 70 36)))
      (Frac.den (noisyOrCombine (keywordTermFin 100 400) (vectorTerm 100 70 36))) .noisyOrV0 ∧
    provisional 100 (top (some (.fin 400)) (some (.fin 70))) none = none ∧
    Reports (provisional 100 (top (some (.fin 400)) none) (some (.fin 36))) 4 9 .keywordOnlyV0 ∧
    Reports (provisional 100 (top (some (.fin 400)) none) none) 4 9 .keywordOnlyV0 ∧
    Reports (provisional 100 (top (some (.fin 0)) none) none) 0 1 .keywordOnlyV0 ∧
    provisional 100 (top none (some (.fin 70))) (some (.fin 36)) = none ∧
    provisional 100 TopLegs.default (some (.fin 36)) = none ∧
    noisyOr 100 .nan (.fin 70) (.fin 36) = none ∧
    noisyOr 100 (.fin 400) .posInf (.fin 36) = none ∧
    noisyOr 100 (.fin 400) (.fin 70) (.fin 100) = none ∧
    provisional 100 (top (some .posInf) none) none = none := by
  decide

/-- `1/2` at `KEYWORD_HALF_SCORE`, and the `1/n` bound, on numbers. -/
example : (keywordTermFin 100 500).eqv ⟨1, 2⟩ ∧ (keywordTermFin 100 0).num = 0 ∧
    (keywordTermFin 100 (-300)).num = 0 ∧
    ((keywordTermFin 100 5000).den - (keywordTermFin 100 5000).num) * 10 ≤
      (keywordTermFin 100 5000).den := by
  decide

/-- A two-leg value of `1` needs a cosine of `1`: a huge BM25 alone does not. -/
example :
    (noisyOrCombine (keywordTermFin 100 1000000) (vectorTerm 100 99 36)).num <
      (noisyOrCombine (keywordTermFin 100 1000000) (vectorTerm 100 99 36)).den := by
  decide

/-! ## `top_legs` -/

/-- A leg map entry's slots (`LegScores`, without the RRF fields). -/
structure LegSlot where
  keyword : Option F64
  recent : Option F64
  vector : Option F64
deriving DecidableEq, Repr

/-- `KeywordLeg` (`candidates.rs`). -/
structure KeywordLeg where
  ran : Bool
  bestBm25 : Option F64
  exactName : Bool
deriving DecidableEq, Repr

/-- The finite value of an `f64`. -/
def finiteOf : F64 → Option Int
  | .fin x => some x
  | _ => none

/-- `Iterator::reduce(f64::max)` on finite values. -/
def reduceMax : List Int → Option Int
  | [] => none
  | x :: xs => some (xs.foldl max x)

/-- `.filter(|s| s.is_finite()).reduce(f64::max)`. -/
def finiteMax (l : List F64) : Option Int := reduceMax (l.filterMap finiteOf)

variable {ι : Type}

/-- The vector slots `top_legs` folds: those of the entries whose node is a
concept of the graph (`inGraph`). -/
def vectors (inGraph : ι → Bool) (legs : List (ι × LegSlot)) : List F64 :=
  (legs.filter fun p => inGraph p.1).filterMap fun p => p.2.vector

/-- `top_legs(graph, legs, keyword)`. -/
def topLegs (inGraph : ι → Bool) (legs : List (ι × LegSlot)) (kw : KeywordLeg) : TopLegs :=
  { bm25 := if kw.ran then kw.bestBm25 else none
    vectorCosine := (finiteMax (vectors inGraph legs)).map .fin
    exactNameMatch := kw.ran && kw.exactName }

theorem mem_filterMap_finiteOf {l : List F64} {x : Int} :
    x ∈ l.filterMap finiteOf ↔ F64.fin x ∈ l := by
  rw [List.mem_filterMap]
  constructor
  · rintro ⟨a, ha, hx⟩
    cases a <;> simp [finiteOf] at hx
    subst hx; exact ha
  · intro h; exact ⟨_, h, rfl⟩

/-- **The fold is the maximum**: `some x` exactly when `x` is a finite member
at least every finite member. -/
theorem finiteMax_eq_some {l : List F64} {x : Int} :
    finiteMax l = some x ↔ F64.fin x ∈ l ∧ ∀ y, F64.fin y ∈ l → y ≤ x := by
  unfold finiteMax
  constructor
  · intro h
    cases hl : l.filterMap finiteOf with
    | nil => rw [hl] at h; cases h
    | cons z zs =>
      rw [hl] at h
      simp only [reduceMax, Option.some.injEq] at h
      subst h
      refine ⟨?_, ?_⟩
      · rw [← mem_filterMap_finiteOf, hl]; exact foldl_max_mem zs z
      · intro y hy
        rw [← mem_filterMap_finiteOf, hl] at hy
        rcases List.mem_cons.mp hy with rfl | hy
        · exact foldl_max_ge_init zs y
        · exact foldl_max_ge zs z y hy
  · rintro ⟨hx, hmax⟩
    cases hl : l.filterMap finiteOf with
    | nil =>
      have := mem_filterMap_finiteOf.mpr hx
      rw [hl] at this; cases this
    | cons z zs =>
      simp only [reduceMax, Option.some.injEq]
      have hmem : zs.foldl max z ∈ l.filterMap finiteOf := hl ▸ foldl_max_mem zs z
      have hx' : x ∈ z :: zs := hl ▸ mem_filterMap_finiteOf.mpr hx
      have h1 : x ≤ zs.foldl max z := by
        rcases List.mem_cons.mp hx' with rfl | h
        · exact foldl_max_ge_init zs x
        · exact foldl_max_ge zs z x h
      have h2 := hmax _ (mem_filterMap_finiteOf.mp hmem)
      omega

/-- `None` exactly when no member is finite. -/
theorem finiteMax_eq_none {l : List F64} :
    finiteMax l = none ↔ ∀ x, F64.fin x ∉ l := by
  unfold finiteMax
  constructor
  · intro h x hx
    have := mem_filterMap_finiteOf.mpr hx
    cases hl : l.filterMap finiteOf with
    | nil => rw [hl] at this; cases this
    | cons z zs => rw [hl] at h; cases h
  · intro h
    cases hl : l.filterMap finiteOf with
    | nil => rfl
    | cons z zs =>
      have : z ∈ l.filterMap finiteOf := by rw [hl]; simp
      exact absurd (mem_filterMap_finiteOf.mp this) (h z)

/-- The fold depends only on which finite values occur. -/
theorem finiteMax_congr {l l' : List F64} (h : ∀ x, F64.fin x ∈ l ↔ F64.fin x ∈ l') :
    finiteMax l = finiteMax l' := by
  cases hl : finiteMax l with
  | none =>
    exact (finiteMax_eq_none.mpr fun x hx => finiteMax_eq_none.mp hl x ((h x).mpr hx)).symm
  | some x =>
    obtain ⟨hx, hmax⟩ := finiteMax_eq_some.mp hl
    exact (finiteMax_eq_some.mpr ⟨(h x).mp hx, fun y hy => hmax y ((h y).mpr hy)⟩).symm

/-- **The cosine is the best finite cosine of an in-graph concept**: the value
is one of them and at least every other; out-of-graph concepts, non-finite
cosines and the recent and keyword slots do not count. -/
theorem topLegs_vector_max {inGraph : ι → Bool} {legs : List (ι × LegSlot)} {kw : KeywordLeg}
    {c : Int} (h : (topLegs inGraph legs kw).vectorCosine = some (.fin c)) :
    (∃ i l, (i, l) ∈ legs ∧ inGraph i = true ∧ l.vector = some (.fin c)) ∧
    ∀ i l y, (i, l) ∈ legs → inGraph i = true → l.vector = some (.fin y) → y ≤ c := by
  simp only [topLegs, Option.map_eq_some_iff, F64.fin.injEq] at h
  obtain ⟨c', hc', rfl⟩ := h
  obtain ⟨hmem, hmax⟩ := finiteMax_eq_some.mp hc'
  refine ⟨?_, ?_⟩
  · simp only [vectors, List.mem_filterMap, List.mem_filter] at hmem
    obtain ⟨⟨i, l⟩, ⟨hl, hg⟩, hv⟩ := hmem
    exact ⟨i, l, hl, hg, hv⟩
  · intro i l y hl hg hv
    apply hmax
    simp only [vectors, List.mem_filterMap, List.mem_filter]
    exact ⟨(i, l), ⟨hl, hg⟩, hv⟩

/-- **The vector leg counts as absent** exactly when no in-graph concept has a
finite cosine: a vector leg that did not run (no slots), returned nothing,
returned only non-finite cosines or only concepts outside the graph. -/
theorem topLegs_vector_none_iff {inGraph : ι → Bool} {legs : List (ι × LegSlot)}
    {kw : KeywordLeg} :
    (topLegs inGraph legs kw).vectorCosine = none ↔
      ∀ i l x, (i, l) ∈ legs → inGraph i = true → l.vector ≠ some (.fin x) := by
  simp only [topLegs, Option.map_eq_none_iff, finiteMax_eq_none, vectors, List.mem_filterMap,
    List.mem_filter]
  constructor
  · intro h i l x hl hg hv
    exact h x ⟨(i, l), ⟨hl, hg⟩, hv⟩
  · rintro h x ⟨⟨i, l⟩, ⟨hl, hg⟩, hv⟩
    exact h i l x hl hg hv

/-- The cosine `top_legs` reports is finite. -/
theorem topLegs_vector_finite (inGraph : ι → Bool) (legs : List (ι × LegSlot))
    (kw : KeywordLeg) {c : F64} (h : (topLegs inGraph legs kw).vectorCosine = some c) :
    c.isFinite = true := by
  simp only [topLegs, Option.map_eq_some_iff] at h
  obtain ⟨_, -, rfl⟩ := h
  rfl

/-- `top_legs` reads only the in-graph vector slots, and only which finite
values occur among them. -/
theorem topLegs_congr {inGraph : ι → Bool} {legs legs' : List (ι × LegSlot)} (kw : KeywordLeg)
    (h : ∀ x, F64.fin x ∈ vectors inGraph legs ↔ F64.fin x ∈ vectors inGraph legs') :
    topLegs inGraph legs kw = topLegs inGraph legs' kw := by
  simp only [topLegs, finiteMax_congr h]

/-- `TopLegs` (`src/recall/confidence.rs`) does not depend on the candidate
`HashMap`'s iteration order. -/
theorem topLegs_perm {inGraph : ι → Bool} {legs legs' : List (ι × LegSlot)} (kw : KeywordLeg)
    (h : legs.Perm legs') : topLegs inGraph legs kw = topLegs inGraph legs' kw :=
  topLegs_congr kw fun _ => ((h.filter _).filterMap _).mem_iff

/-- The best BM25 and the name flag are the keyword leg's own, masked when it
did not run. -/
theorem topLegs_bm25 (inGraph : ι → Bool) (legs : List (ι × LegSlot)) (kw : KeywordLeg) :
    (topLegs inGraph legs kw).bm25 = (if kw.ran then kw.bestBm25 else none) ∧
    (topLegs inGraph legs kw).exactNameMatch = (kw.ran && kw.exactName) :=
  ⟨rfl, rfl⟩

/-! ## What phase 1 hands `top_legs` -/

/-- The walk over the ranked search: the score of the first concept with a
finite score (`best_bm25` before the `matched` match). -/
def firstFiniteConcept (isConcept : ι → Bool) : List (ι × F64) → Option Int
  | [] => none
  | (i, s) :: rest =>
    if isConcept i then
      match s with
      | .fin x => some x
      | _ => firstFiniteConcept isConcept rest
    else firstFiniteConcept isConcept rest

/-- `KeywordLeg` as `candidates_with_keyword_leg_as` builds it from the ranked
search (`ranked`), whether the query had text (`ran`) and the named concept
(`nameTarget`, `graph.name_target(query)` filtered to concepts). -/
def keywordLeg (ran : Bool) (isConcept : ι → Bool) (ranked : List (ι × F64))
    (nameTarget : Option ι) : KeywordLeg :=
  { ran
    bestBm25 :=
      if ran then
        if ranked.any (fun p => isConcept p.1) then (firstFiniteConcept isConcept ranked).map .fin
        else some (.fin 0)
      else none
    exactName := nameTarget.isSome }

/-- `entry(id).or_default()` then a write to its keyword slot. -/
def upsertKeyword [DecidableEq ι] (id : ι) (f : Option F64 → Option F64) :
    List (ι × LegSlot) → List (ι × LegSlot)
  | [] => [(id, ⟨f none, none, none⟩)]
  | (j, l) :: rest =>
    if j = id then (j, { l with keyword := f l.keyword }) :: rest
    else (j, l) :: upsertKeyword id f rest

/-- The keyword loop: the first `limit * KEYWORD_OVERFETCH` hits' BM25 into
their keyword slots.
Pinned by `formal_constants_tests::confidence`. -/
def keywordLoop [DecidableEq ι] (limit : Nat) (ranked : List (ι × F64))
    (legs : List (ι × LegSlot)) : List (ι × LegSlot) :=
  (ranked.take (limit * 4)).foldl (fun acc p => upsertKeyword p.1 (fun _ => some p.2) acc) legs

/-- The exact-name step (#288): the named concept's keyword slot raised to its
BM25 from the whole ranked list, then to `1.0` (`fin u`). -/
def nameStep [DecidableEq ι] (u : Int) (nameTarget : Option ι) (ranked : List (ι × F64))
    (legs : List (ι × LegSlot)) : List (ι × LegSlot) :=
  match nameTarget with
  | none => legs
  | some id =>
    upsertKeyword id
      (fun slot =>
        let slot := match ranked.find? (fun p => p.1 = id) with
          | some hit => some (mergeMax slot hit.2)
          | none => slot
        some (mergeMax slot (.fin u)))
      legs

/-- The keyword part of phase 1, on a leg map `legs`. -/
def keywordPhase [DecidableEq ι] (u : Int) (limit : Nat) (ranked : List (ι × F64))
    (nameTarget : Option ι) (legs : List (ι × LegSlot)) : List (ι × LegSlot) :=
  nameStep u nameTarget ranked (keywordLoop limit ranked legs)

theorem vectors_upsertKeyword [DecidableEq ι] (inGraph : ι → Bool) (id : ι)
    (f : Option F64 → Option F64) (legs : List (ι × LegSlot)) :
    vectors inGraph (upsertKeyword id f legs) = vectors inGraph legs := by
  induction legs with
  | nil =>
    simp only [upsertKeyword, vectors, List.filter_cons, List.filter_nil]
    split <;> rfl
  | cons p rest ih =>
    obtain ⟨j, l⟩ := p
    unfold vectors at ih ⊢
    simp only [upsertKeyword]
    split
    · simp only [List.filter_cons]; split <;> rfl
    · simp only [List.filter_cons]
      split
      · simp only [List.filterMap_cons, ih]
      · exact ih

/-- **The keyword slots never reach `top_legs`**: the cap loop (the only place
`limit`, so `top_k`, enters) and the exact-name step leave it unchanged. -/
theorem topLegs_keywordPhase [DecidableEq ι] (u : Int) (inGraph : ι → Bool) (limit : Nat)
    (ranked : List (ι × F64)) (nameTarget : Option ι) (legs : List (ι × LegSlot))
    (kw : KeywordLeg) :
    topLegs inGraph (keywordPhase u limit ranked nameTarget legs) kw = topLegs inGraph legs kw := by
  have hloop : ∀ (r : List (ι × F64)) (acc : List (ι × LegSlot)),
      vectors inGraph (r.foldl (fun acc p => upsertKeyword p.1 (fun _ => some p.2) acc) acc) =
        vectors inGraph acc := by
    intro r
    induction r with
    | nil => intro acc; rfl
    | cons p rest ih =>
      intro acc
      simp only [List.foldl_cons]
      rw [ih, vectors_upsertKeyword]
  have : vectors inGraph (keywordPhase u limit ranked nameTarget legs) = vectors inGraph legs := by
    simp only [keywordPhase, nameStep, keywordLoop]
    split
    · exact hloop _ _
    · rw [vectors_upsertKeyword, hloop]
  simp only [topLegs, this]

/-- The leg's best is finite: "a non-finite score is never a leg's best". -/
theorem keywordLeg_bm25_finite (ran : Bool) (isConcept : ι → Bool) (ranked : List (ι × F64))
    (t : Option ι) {b : F64} (h : (keywordLeg ran isConcept ranked t).bestBm25 = some b) :
    b.isFinite = true := by
  simp only [keywordLeg] at h
  split at h
  · split at h
    · simp only [Option.map_eq_some_iff] at h
      obtain ⟨_, -, rfl⟩ := h
      rfl
    · cases h; rfl
  · cases h

theorem firstFiniteConcept_mem {isConcept : ι → Bool} {ranked : List (ι × F64)} {x : Int}
    (h : firstFiniteConcept isConcept ranked = some x) :
    ∃ i, (i, F64.fin x) ∈ ranked ∧ isConcept i = true := by
  induction ranked with
  | nil => cases h
  | cons p rest ih =>
    obtain ⟨i, s⟩ := p
    simp only [firstFiniteConcept] at h
    split at h
    · rename_i hc
      cases s with
      | fin y => cases h; exact ⟨i, by simp, hc⟩
      | _ =>
        obtain ⟨j, hj, hcj⟩ := ih h
        exact ⟨j, List.mem_cons_of_mem _ hj, hcj⟩
    · obtain ⟨j, hj, hcj⟩ := ih h
      exact ⟨j, List.mem_cons_of_mem _ hj, hcj⟩

/-- The ranked search is best-first among finite scores. -/
def BestFirst (ranked : List (ι × F64)) : Prop :=
  ranked.Pairwise fun p q => ∀ x y, p.2 = .fin x → q.2 = .fin y → y ≤ x

theorem firstFiniteConcept_max {isConcept : ι → Bool} {ranked : List (ι × F64)} {x : Int}
    (hbf : BestFirst ranked) (h : firstFiniteConcept isConcept ranked = some x) :
    ∀ i y, (i, F64.fin y) ∈ ranked → isConcept i = true → y ≤ x := by
  induction ranked with
  | nil => cases h
  | cons p rest ih =>
    obtain ⟨i, s⟩ := p
    have hbf' := List.pairwise_cons.mp hbf
    intro j y hj hcj
    simp only [firstFiniteConcept] at h
    by_cases hci : isConcept i = true
    · simp only [hci, ↓reduceIte] at h
      cases s with
      | fin z =>
        cases h
        rcases List.mem_cons.mp hj with heq | hj'
        · cases heq; exact Int.le_refl _
        · exact hbf'.1 _ hj' _ y rfl rfl
      | nan | posInf | negInf =>
        rcases List.mem_cons.mp hj with heq | hj'
        · cases heq
        · exact ih hbf'.2 h j y hj' hcj
    · simp only [hci, Bool.false_eq_true, ↓reduceIte] at h
      rcases List.mem_cons.mp hj with heq | hj'
      · cases heq; exact absurd hcj hci
      · exact ih hbf'.2 h j y hj' hcj

/-- **`top_legs.bm25` is a raw BM25** (#288): a concept's score as the ranked
search gave it, whatever the leg map holds (after the cap loop and the name
step included), and, when the search is best-first, the best one. -/
theorem keywordLeg_bm25_raw (inGraph isConcept : ι → Bool) (ranked : List (ι × F64))
    (t : Option ι) (legs : List (ι × LegSlot)) {x : Int}
    (h : (topLegs inGraph legs (keywordLeg true isConcept ranked t)).bm25 = some (.fin x)) :
    x = 0 ∨ ∃ i, (i, F64.fin x) ∈ ranked ∧ isConcept i = true := by
  simp only [topLegs, keywordLeg, ↓reduceIte] at h
  split at h
  · simp only [Option.map_eq_some_iff, F64.fin.injEq] at h
    obtain ⟨y, hy, rfl⟩ := h
    exact Or.inr (firstFiniteConcept_mem hy)
  · cases h; exact Or.inl rfl

/-- **The best one**, on a best-first search. -/
theorem keywordLeg_bm25_best (inGraph isConcept : ι → Bool) (ranked : List (ι × F64))
    (hbf : BestFirst ranked) (t : Option ι) (legs : List (ι × LegSlot)) {x : Int}
    (h : (topLegs inGraph legs (keywordLeg true isConcept ranked t)).bm25 = some (.fin x)) :
    ∀ i y, (i, F64.fin y) ∈ ranked → isConcept i = true → y ≤ x := by
  simp only [topLegs, keywordLeg, ↓reduceIte] at h
  split at h
  · simp only [Option.map_eq_some_iff, F64.fin.injEq] at h
    obtain ⟨y, hy, rfl⟩ := h
    exact firstFiniteConcept_max hbf hy
  · rename_i hany
    intro i y hi hc
    exfalso; apply hany
    exact List.any_eq_true.mpr ⟨_, hi, hc⟩

/-- **On what `top_legs` builds the non-finite guards are defensive**: with a
finite median below `1`, a recall whose keyword leg reported a score reports a
confidence, on either basis. -/
theorem provisional_isSome_of_built {u : Int} (inGraph isConcept : ι → Bool)
    (ranked : List (ι × F64)) (t : Option ι) (ran : Bool) (legs : List (ι × LegSlot))
    {m : Int} (hm : m < u)
    (hb : (topLegs inGraph legs (keywordLeg ran isConcept ranked t)).bm25.isSome) :
    (provisional u (topLegs inGraph legs (keywordLeg ran isConcept ranked t))
      (some (.fin m))).isSome := by
  generalize htop : topLegs inGraph legs (keywordLeg ran isConcept ranked t) = top at hb
  obtain ⟨b, hb'⟩ := Option.isSome_iff_exists.mp hb
  have hbf : b.isFinite = true := by
    have h := hb'
    rw [← htop] at h
    simp only [topLegs] at h
    split at h
    · exact keywordLeg_bm25_finite ran isConcept ranked t h
    · cases h
  cases b with
  | fin s =>
    cases hv : top.vectorCosine with
    | none => simp [provisional_keywordOnly u hb' hv]
    | some c =>
      have hcf := topLegs_vector_finite inGraph legs (keywordLeg ran isConcept ranked t)
        (htop ▸ hv)
      cases c with
      | fin c => simp [provisional_twoLeg hb' hv hm]
      | _ => cases hcf
  | _ => cases hbf

/-! ### The Rust and `Memory` tests, at scale `u = 100` (`ι = Nat`) -/

/-- The graph of `top_legs_are_the_maxima_and_ignore_the_recent_leg`: concepts
`0..4`; `9` is outside it. -/
def inGraphDemo (i : Nat) : Bool := decide (i < 5)

/-- `top_legs_are_the_maxima_and_ignore_the_recent_leg`: the cosine is the best
finite in-graph one (`0.66`; a `NaN` skipped), the keyword best is the leg's
own report (`7.25`), not a fold over the slots; recent-only candidates give
neither leg a score; a leg that did not run reports nothing, its name match
included. `a_vector_candidate_outside_the_graph_is_not_a_top_leg`: `0.93`
outside the graph is not counted. -/
example :
    topLegs inGraphDemo
        [(0, ⟨some (.fin 250), none, some (.fin 41)⟩), (1, ⟨some (.fin 725), some (.fin 35), none⟩),
         (2, ⟨none, some (.fin 35), some (.fin 66)⟩), (3, ⟨none, some (.fin 35), none⟩),
         (4, ⟨none, none, some .nan⟩)]
        ⟨true, some (.fin 725), false⟩ = top (some (.fin 725)) (some (.fin 66)) ∧
    topLegs inGraphDemo [(0, ⟨none, some (.fin 35), none⟩)] ⟨true, some (.fin 0), false⟩ =
      top (some (.fin 0)) none ∧
    topLegs inGraphDemo [(0, ⟨none, some (.fin 35), none⟩)] ⟨false, none, false⟩ = TopLegs.default ∧
    topLegs inGraphDemo [(0, ⟨none, some (.fin 35), none⟩)] ⟨true, none, false⟩ = TopLegs.default ∧
    (topLegs inGraphDemo [] ⟨true, some (.fin 150), true⟩).exactNameMatch = true ∧
    topLegs inGraphDemo [] ⟨false, some (.fin 150), true⟩ = TopLegs.default ∧
    (topLegs inGraphDemo [(0, ⟨none, none, some (.fin 41)⟩), (9, ⟨none, none, some (.fin 93)⟩)]
      ⟨false, none, false⟩).vectorCosine = some (.fin 41) := by
  decide

/-- `firstFiniteConcept`'s branches (`isConcept` is `inGraphDemo`, so `9` is
not a concept): a non-concept hit first is skipped; a `+∞` concept hit before
a finite one is skipped (`matched` without a best), and the leg's best is the
finite one; a concept list of only non-finite scores has no best, so the leg
reports none although it matched; on a multi-hit best-first list the first
concept is the best (`firstFiniteConcept_max`). -/
example :
    firstFiniteConcept inGraphDemo [(9, .fin 900), (1, .fin 300)] = some 300 ∧
    firstFiniteConcept inGraphDemo [(2, .posInf), (1, .fin 300)] = some 300 ∧
    (keywordLeg true inGraphDemo [(2, .posInf), (1, .fin 300)] none).bestBm25 = some (.fin 300) ∧
    (keywordLeg true inGraphDemo [(2, .nan)] none).bestBm25 = none ∧
    (keywordLeg true inGraphDemo [(9, .fin 900)] none).bestBm25 = some (.fin 0) ∧
    firstFiniteConcept inGraphDemo [(9, .fin 900), (3, .fin 720), (0, .fin 410), (4, .fin 100)] =
      some 720 ∧
    BestFirst ([(9, .fin 900), (3, .fin 720), (0, .fin 410), (4, .fin 100)] : List (Nat × F64)) := by
  refine ⟨by decide, by decide, by decide, by decide, by decide, by decide, ?_⟩
  simp [BestFirst]

/-- **`bm25` is not the name floor** (#288, `an_exact_name_query_reports_the_raw_bm25_and_the_match`):
the named concept `0` scores `0.4`; after the keyword phase its keyword slot
holds `1.0`, and `top_legs.bm25` is still `0.4`, at `top_k` 0 as at 1. -/
theorem bm25_not_name_floor :
    keywordPhase 100 1 [(0, .fin 40)] (some 0) [] = [(0, ⟨some (.fin 100), none, none⟩)] ∧
    (topLegs inGraphDemo (keywordPhase 100 1 [(0, .fin 40)] (some 0) [])
      (keywordLeg true inGraphDemo [(0, .fin 40)] (some 0))) =
      ⟨some (.fin 40), none, true⟩ ∧
    (topLegs inGraphDemo (keywordPhase 100 0 [(0, .fin 40)] (some 0) [])
      (keywordLeg true inGraphDemo [(0, .fin 40)] (some 0))) =
      ⟨some (.fin 40), none, true⟩ := by
  decide

/-- **A name query with no word overlap** (#256,
`an_exact_name_with_no_word_overlap_reports_zero_bm25_beside_the_match`): the
search returns nothing, so `bm25` is `0`, `exact_name_match` is `true`, and
the keyword-only value is `0`. -/
theorem exactName_zero_overlap :
    topLegs inGraphDemo (keywordPhase 100 1 [] (some 3) []) (keywordLeg true inGraphDemo [] (some 3)) =
      ⟨some (.fin 0), none, true⟩ ∧
    provisional 100 ⟨some (.fin 0), none, true⟩ none =
      some ⟨⟨0, 500⟩, .keywordOnlyV0, true⟩ := by
  decide

/-- `a_zero_top_k_reports_the_same_bm25_as_top_k_one`, and the module doc's
`top_k = 0` remark: with no neighbours asked for, the vector slots are empty
and a text recall reports `keyword-only-v0` where `top_k = 1` (one neighbour
at `0.70`) reports `noisy-or-v0`. The degraded-vector test is the same
keyword-only case without a median. -/
example :
    (provisional 100 (topLegs inGraphDemo (keywordPhase 100 0 [(0, .fin 400)] none [])
      (keywordLeg true inGraphDemo [(0, .fin 400)] none)) (some (.fin 36))).map (·.basis) =
      some .keywordOnlyV0 ∧
    (provisional 100 (topLegs inGraphDemo
      ((keywordPhase 100 1 [(0, .fin 400)] none []) ++ [(1, ⟨none, none, some (.fin 70)⟩)])
      (keywordLeg true inGraphDemo [(0, .fin 400)] none)) (some (.fin 36))).map (·.basis) =
      some .noisyOrV0 ∧
    Reports (provisional 100 (topLegs inGraphDemo [] (keywordLeg true inGraphDemo [(0, .fin 400)] none))
      none) 4 9 .keywordOnlyV0 := by
  decide

/-! ## The caller and the null median -/

/-- `Route` (`src/daemon/mod.rs`). -/
inductive Route where
  | text
  | byVector
  | layer
deriving DecidableEq, Repr

/-- `result.confidence`: `provisional` on a text recall, `None` otherwise. -/
def recallConfidence (u : Int) (route : Route) (top : TopLegs) (median : Option F64) :
    Option Provisional :=
  match route with
  | .text => provisional u top median
  | _ => none

/-- `CalibrationMode`. -/
inductive CalibrationMode where
  | builtin
  | off
deriving DecidableEq, Repr

/-- `null_median_cosine(mode, contract)`: `row` is the contract's `BUILTIN`
row's `null_median_cosine` (`none` with no row), `isReference` is
`is_reference(contract)`, `reference` is `REFERENCE_NULL_MEDIAN_COSINE`. -/
def nullMedian (mode : CalibrationMode) (row : Option (Option F64)) (isReference : Bool)
    (reference : F64) : Option F64 :=
  match mode, row with
  | .builtin, some r => r
  | _, _ => if isReference then some reference else none

/-- A recall by image or vector, and a layer's side, report no confidence. -/
theorem recall_nonText (u : Int) {route : Route} (h : route ≠ .text) (top : TopLegs)
    (median : Option F64) : recallConfidence u route top median = none := by
  cases route <;> simp_all [recallConfidence]

/-- Under `calibration = "off"` a non-reference contract has no median. -/
theorem nullMedian_off_nonReference (row : Option (Option F64)) (reference : F64) :
    nullMedian .off row false reference = none := by
  cases row <;> rfl

/-- The BGE-M3 reference keeps its median under `"off"`. -/
theorem nullMedian_off_reference (row : Option (Option F64)) (reference : F64) :
    nullMedian .off row true reference = some reference := by
  cases row <;> rfl

/-- **Both legs, no measured median: no confidence** (#256), e.g. a
non-reference contract under `"off"`. -/
theorem recall_twoLeg_absent_off_nonReference (u : Int) {top : TopLegs} {c : F64}
    (hv : top.vectorCosine = some c) (row : Option (Option F64)) (reference : F64) :
    recallConfidence u .text top (nullMedian .off row false reference) = none := by
  rw [nullMedian_off_nonReference]
  exact provisional_twoLeg_without_median u hv

/-- **No vector leg: the keyword-only value is reported without a median**
(#256's acceptance; the degraded-vector test runs on a contract with none). -/
theorem recall_keywordOnly_without_median (u : Int) {top : TopLegs} {s : Int}
    (hb : top.bm25 = some (.fin s)) (hv : top.vectorCosine = none) :
    recallConfidence u .text top none = some ⟨keywordTermFin u s, .keywordOnlyV0, true⟩ :=
  provisional_keywordOnly u hb hv none

example : nullMedian .builtin (some (some (.fin 4182))) false (.fin 3586) = some (.fin 4182) ∧
    nullMedian .builtin (some none) false (.fin 3586) = none ∧
    nullMedian .builtin none false (.fin 3586) = none ∧
    nullMedian .off (some (some (.fin 4182))) false (.fin 3586) = none ∧
    nullMedian .off none true (.fin 3586) = some (.fin 3586) ∧
    recallConfidence 100 .byVector (top (some (.fin 400)) none) none = none ∧
    recallConfidence 100 .layer (top (some (.fin 400)) none) none = none := by
  decide

end Lambo.Confidence
