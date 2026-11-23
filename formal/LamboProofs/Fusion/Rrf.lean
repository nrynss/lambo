import LamboProofs.Fusion.Max

/-
# Reciprocal rank fusion (#173) and the exact-name step under it

Models `FusionKind::Rrf` and `RecallFusion` (`src/recall/fusion.rs`: `k`, the
three weights, `RecallFusion::term`, and `validate`'s finite non-negative
`k`), and in `src/recall/candidates.rs` the RRF half of `rank` (`leg_ranks`
for the keyword and vector legs, `best` over the legs that ran,
`l.rrf = sum / best` or `0`), `recent_concepts`' rank and `LegScores::fused`
under `Rrf`, over `Max.lean`'s leg tables. A leg rank (`legRank`) is one more
than the number of the leg's members that sort ahead by `Beats`; with a strict
total tie-break (`StrictTotal`, which `Order.lean` shows `tie_break_by_key`
is) that is the 1-based position in `leg_ranks`' sorted list. The recent rank
(`recentRank`) comes from the owning interaction's index (`RrfRecall.recentIdx`,
which a caller must keep consistent with `Input.recent`). `RrfRecall.rrf` is
the exact value, `RrfRecall.key` the value `rank` sorts on and
`RrfRecall.rrfAhead` a node's position in `rank`'s output. Numbers are exact
rationals: `k = kNum / kDen`, weights naturals at one common scale
(`1.0 = 10`), every term scaled by `prodDen` so each is an exact quotient.

Results: every `rrf` is in `[0, 1]` and rank 1 in every leg that ran is
exactly 1 (`rrf_range`, `num_eq_best_of_rank_one`); RRF is monotone in each
leg's rank (`rrfSum_mono`); a leg a node is not in contributes 0 and still
counts in `best` when it ran (`term_none`, `rrf_recent_example`); ranks follow
the leg's own scores and the tie-break, run from 1 to the leg's size and do not
depend on candidate order (`legRank_lt_iff`, `legRank_injective`,
`legRank_le_members`, `legRank_perm`, `rrf_perm`); with one positive-weight
leg RRF sorts its members as the leg does (`rrf_one_leg_agrees`) and a
zero-weight leg contributes nothing (`rrf_one_leg_needs_weight`); the Rust
tests' arithmetic (`rrf_two_leg_example`, `rrf_tie_example`). The exact-name
step under RRF: the named concept's keyword rank never worsens, its `rrf`
never falls and no more nodes sort ahead of it (`rrf_full_rank_named`,
`rrf_full_named_up`, `rrf_full_ahead_le`); every other node's keyword rank
moves down by at most one place, exactly when the named concept passes it,
and its vector and recent ranks never move (`rrf_full_rank_other`,
`rrf_full_other`, `rrf_full_rank_vector_recent`, `rrf_full_best`); a pass
happens only from the `1.0` floor (`full_passes_only_below_floor`,
`rrf_name_step_above_floor`); the review fix changes no keyword rank and no
`rrf` against `raiseName` (`full_rank_eq_raise`, `full_rrf_eq_raise`), though
against the table before the step it can shift a neighbour
(`rrf_beyond_cap_shifts_neighbour`); and the step can lower a neighbour's
`rrf` and swap two nodes it does not name, where max fusion moves neither
(`rrf_name_step_shifts_neighbour`, `rrf_name_step_reorders_unnamed`).

Boundary: `f64` rounding is not captured: the range and "rank 1 everywhere is
exactly 1" survive it, but two nodes whose exact values tie through different
rank vectors may differ by a rounding in Rust, which then decides their order
instead of the tie-break. Non-finite leg scores, which earn no rank in Rust,
are outside the model. The search's tie-break and `rank`'s are assumed to be
the same `tb`; an index id that is not a concept of the graph (a stale id)
gets no key in `rank`, and at an exact BM25 tie involving one the ranked list
need not be `SortedBy` `rank`'s tie-break, so `full_passes_only_below_floor`
and `full_rank_eq_raise` do not apply to that input.
-/

namespace Lambo.Fusion

open Lambo.Lib (any_of_mem length_filter_mono length_filter_lt length_filter_shift)
variable {ι : Type}
variable [DecidableEq ι]

/-! ## Reciprocal rank fusion (#173)

`FusionKind::Rrf`: each leg ranks its own members, and a node's relevance is
`Σ_leg w_leg / (k + rank_leg)` over the legs it is in, divided by
`Σ_active w_leg / (k + 1)` (`rank` in `src/recall/candidates.rs`,
`RecallFusion::term` in `src/recall/fusion.rs`). The number model is in the
header: `k = kNum / kDen`, weights at one common integer scale, and every term
an exact natural over the common multiple `prodDen`. -/

/-- `RecallFusion` under `FusionKind::Rrf`, exactly: `k = kNum / kDen`, and the
three leg weights at one common integer scale (`10` for `1.0`, `3` for `0.3`).
The value is a ratio in which a common weight scale cancels, as Rust's own
division by the largest weight relies on. -/
structure RrfConfig where
  kNum : Nat
  kDen : Nat
  wKeyword : Nat
  wVector : Nat
  wRecent : Nat
deriving Repr

/-- `k + r` times `kDen`: `w / (k + r) = w · kDen / den r`, and the common
`kDen` cancels in the ratio. -/
def RrfConfig.den (c : RrfConfig) (r : Nat) : Nat := c.kNum + c.kDen * r

/-- `k` is a finite number at least 0 (`RecallFusion::validate`): a fraction
with a positive denominator. -/
def RrfConfig.Valid (c : RrfConfig) : Prop := 1 ≤ c.kDen

/-- The defaults: `k = 3` (`DEFAULT_RRF_K`), keyword and vector weight `1.0`,
recent weight `0.3`.
Pinned by `formal_constants_tests::fusion`. -/
def rrfDefault : RrfConfig := ⟨3, 1, 10, 10, 3⟩

/-- The Rust tests' `rrf()`: `k = 60`, default weights. -/
def rrfK60 : RrfConfig := ⟨60, 1, 10, 10, 3⟩

/-- The common multiple: `den 1 · den 2 · … · den N`. Every rank in a recall of
`N` candidates is in `[1, N]`, so every term `M / den r` is exact. -/
def prodDen (c : RrfConfig) : Nat → Nat
  | 0 => 1
  | n + 1 => c.den (n + 1) * prodDen c n

theorem den_dvd_prodDen (c : RrfConfig) : ∀ {N r : Nat}, 1 ≤ r → r ≤ N → c.den r ∣ prodDen c N
  | 0, _, h1, h2 => absurd h2 (by omega)
  | N + 1, r, h1, h2 => by
    by_cases hr : r = N + 1
    · subst hr; exact Nat.dvd_mul_right _ _
    · exact Nat.dvd_trans (den_dvd_prodDen c h1 (by omega)) (Nat.dvd_mul_left _ _)

theorem den_pos {c : RrfConfig} (hv : c.Valid) {r : Nat} (hr : 1 ≤ r) : 0 < c.den r := by
  unfold RrfConfig.den RrfConfig.Valid at *
  have := Nat.mul_le_mul hv hr
  omega

theorem prodDen_pos {c : RrfConfig} (hv : c.Valid) : ∀ N, 0 < prodDen c N
  | 0 => Nat.one_pos
  | N + 1 => Nat.mul_pos (den_pos hv (by omega)) (prodDen_pos hv N)

/-- A worse rank never gives a larger reciprocal. -/
theorem div_den_anti (c : RrfConfig) (M : Nat) {a b : Nat} (ha : 1 ≤ a) (hab : a ≤ b) :
    M / c.den b ≤ M / c.den a := by
  unfold RrfConfig.den
  by_cases h0 : c.kNum + c.kDen * a = 0
  · have hk : c.kDen = 0 := by
      have h' : c.kDen * a = 0 := by omega
      rcases Nat.mul_eq_zero.mp h' with h | h
      · exact h
      · omega
    have hn : c.kNum = 0 := by omega
    simp [hk, hn]
  · apply Nat.div_le_div_left
    · have := Nat.mul_le_mul_left c.kDen hab; omega
    · omega

/-- And a strictly worse rank gives a strictly smaller one, ranks in `[1, N]`
against `prodDen N`. -/
theorem div_den_strict {c : RrfConfig} (hv : c.Valid) {N a b : Nat} (ha : 1 ≤ a) (hab : a < b)
    (hb : b ≤ N) : prodDen c N / c.den b < prodDen c N / c.den a := by
  have hda := den_pos hv ha
  have hdb := den_pos hv (Nat.le_trans ha (Nat.le_of_lt hab))
  have hlt : c.den a < c.den b := by
    unfold RrfConfig.den RrfConfig.Valid at *
    have := Nat.mul_lt_mul_of_pos_left hab hv
    omega
  obtain ⟨q, hq⟩ := den_dvd_prodDen c ha (Nat.le_trans (Nat.le_of_lt hab) hb)
  obtain ⟨q', hq'⟩ := den_dvd_prodDen c (Nat.le_trans ha (Nat.le_of_lt hab)) hb
  have hM := prodDen_pos hv N
  have e1 : prodDen c N / c.den a = q := by rw [hq]; exact Nat.mul_div_cancel_left q hda
  have e2 : prodDen c N / c.den b = q' := by rw [hq']; exact Nat.mul_div_cancel_left q' hdb
  rw [e1, e2]
  have hq'pos : 0 < q' := by
    rcases Nat.eq_zero_or_pos q' with h | h
    · rw [h, Nat.mul_zero] at hq'; omega
    · exact h
  apply Nat.lt_of_not_le
  intro hle
  have h1 : c.den a * q ≤ c.den a * q' := Nat.mul_le_mul_left _ hle
  have h2 : c.den a * q' < c.den b * q' := (Nat.mul_lt_mul_right hq'pos).mpr hlt
  omega

/-- One leg's scaled term for a member at 1-based rank `r`, `0` for a node
the leg did not rank (`RecallFusion::term`, times `M / kDen`). -/
def term (c : RrfConfig) (M w : Nat) : Option Nat → Nat
  | none => 0
  | some r => w * (M / c.den r)

/-- A node's three leg ranks (`keyword_rank`, `vector_rank`, `recent_rank`). -/
structure Ranks where
  keyword : Option Nat
  vector : Option Nat
  recent : Option Nat
deriving DecidableEq, Repr

/-- `Σ_leg w_leg / (k + rank_leg)` over the legs the node is in, scaled. -/
def rrfSum (c : RrfConfig) (M : Nat) (rk : Ranks) : Nat :=
  term c M c.wKeyword rk.keyword + term c M c.wVector rk.vector + term c M c.wRecent rk.recent

/-- `best`: rank 1 in every leg that ran, scaled. -/
def bestSum (c : RrfConfig) (M : Nat) (kAct vAct rAct : Bool) : Nat :=
  (if kAct then c.wKeyword * (M / c.den 1) else 0) +
  (if vAct then c.wVector * (M / c.den 1) else 0) +
  (if rAct then c.wRecent * (M / c.den 1) else 0)

/-- `a` is a rank at least as good as `b`: a smaller number, or `b` absent. -/
def RankLe : Option Nat → Option Nat → Prop
  | _, none => True
  | none, some _ => False
  | some a, some b => a ≤ b

instance instDecidableRankLe : (a b : Option Nat) → Decidable (RankLe a b)
  | _, none => isTrue trivial
  | none, some _ => isFalse (fun h => h)
  | some a, some b => inferInstanceAs (Decidable (a ≤ b))

/-- A rank, when present, is at least 1. -/
def RankPos : Option Nat → Prop
  | none => True
  | some r => 1 ≤ r

/-- In `RecallFusion`, a leg that did not rank a node adds 0 to its `rrf`. -/
theorem term_none (c : RrfConfig) (M w : Nat) : term c M w none = 0 := rfl

theorem term_anti (c : RrfConfig) (M w : Nat) {a b : Option Nat} (ha : RankPos a)
    (h : RankLe a b) : term c M w b ≤ term c M w a := by
  cases a <;> cases b <;> simp_all [RankLe, RankPos, term]
  exact Nat.mul_le_mul_left _ (div_den_anti c M ha h)

theorem term_le_best (c : RrfConfig) (M w : Nat) {a : Option Nat} (ha : RankPos a) :
    term c M w a ≤ w * (M / c.den 1) :=
  term_anti c M w (a := some 1) (Nat.le_refl 1) (by cases a <;> simp_all [RankLe, RankPos])

/-- **RRF is monotone in each leg's rank**: no leg's rank getting worse
(a larger number, or the node leaving the leg) can raise the sum, so a better
rank never lowers it. -/
theorem rrfSum_mono (c : RrfConfig) (M : Nat) {rk rk' : Ranks}
    (hk : RankPos rk'.keyword) (hv : RankPos rk'.vector) (hr : RankPos rk'.recent)
    (hk' : RankLe rk'.keyword rk.keyword) (hv' : RankLe rk'.vector rk.vector)
    (hr' : RankLe rk'.recent rk.recent) : rrfSum c M rk ≤ rrfSum c M rk' := by
  unfold rrfSum
  have := term_anti c M c.wKeyword hk hk'
  have := term_anti c M c.wVector hv hv'
  have := term_anti c M c.wRecent hr hr'
  omega

/-! ### Leg ranks

`rank`'s `leg_ranks`: the members of a leg (the nodes with a score in it) sorted
by `order`, score descending then `tie_break_by_key`, and numbered from 1. With
a strict total order that number is one more than the count of members that
sort ahead, which is what `legRank` computes. -/

/-- A node's 1-based rank within the leg whose slots are `sc`, `none` when it
is not in the leg. A node with no score never sorts ahead of one with a score,
so `ahead` counts members only (`ahead_le_members`). -/
def legRank (sc : ι → Option Int) (tb : ι → ι → Bool) (ids : List ι) (n : ι) : Option Nat :=
  match sc n with
  | none => none
  | some _ => some (ahead sc tb ids n + 1)

/-- `recent_concepts`' rank: `idx n` is which of the three most recent
interactions owns `n` (`0` the newest), and the rank is one more than the
number of members of newer interactions, so members of one interaction share
a rank. -/
def recentRank (idx : ι → Option Nat) (ids : List ι) (n : ι) : Option Nat :=
  match idx n with
  | none => none
  | some i =>
    some ((ids.filter fun m => match idx m with
      | some j => decide (j < i)
      | none => false).length + 1)

omit [DecidableEq ι] in
theorem legRank_pos (sc : ι → Option Int) (tb : ι → ι → Bool) (ids : List ι) (n : ι) :
    RankPos (legRank sc tb ids n) := by
  unfold legRank; cases sc n <;> simp [RankPos]

omit [DecidableEq ι] in
theorem recentRank_pos (idx : ι → Option Nat) (ids : List ι) (n : ι) :
    RankPos (recentRank idx ids n) := by
  unfold recentRank; cases idx n <;> simp [RankPos]

omit [DecidableEq ι] in
/-- **Absent from the leg, no rank**, so its term is 0. -/
theorem legRank_eq_none (sc : ι → Option Int) (tb : ι → ι → Bool) (ids : List ι) (n : ι) :
    legRank sc tb ids n = none ↔ sc n = none := by
  unfold legRank; cases sc n <;> simp

/-- `tie_break_by_key` as a strict total order (`Order.lean` proves the Rust
comparator is one). -/
structure StrictTotal (tb : ι → ι → Bool) : Prop where
  irrefl : ∀ a, tb a a = false
  total : ∀ a b, a ≠ b → tb a b = true ∨ tb b a = true
  trans : ∀ a b c, tb a b = true → tb b c = true → tb a c = true

omit [DecidableEq ι] in
theorem StrictTotal.asymm {tb : ι → ι → Bool} (h : StrictTotal tb) {a b : ι}
    (hab : tb a b = true) : tb b a = false := by
  cases hba : tb b a
  · rfl
  · have := h.trans a b a hab hba; rw [h.irrefl] at this; cases this

theorem oLe_total (a b : Option Int) : OLe a b ∨ OLe b a := by
  cases a <;> cases b <;> simp [OLe] <;> omega

theorem oLe_of_oLt {a b : Option Int} (h : OLt a b) : OLe a b := by
  rcases oLe_total a b with h' | h'
  · exact h'
  · exact absurd h' h

omit [DecidableEq ι] in
theorem beats_self_iff (sc : ι → Option Int) (tb : ι → ι → Bool) (a : ι) :
    Beats sc tb a a ↔ tb a a = true := by
  unfold Beats
  constructor
  · rintro (h | ⟨_, h⟩)
    · exact absurd h (oLt_irrefl _)
    · exact h
  · intro h; exact Or.inr ⟨rfl, h⟩

omit [DecidableEq ι] in
theorem beats_irrefl {tb : ι → ι → Bool} (htb : StrictTotal tb) (sc : ι → Option Int) (a : ι) :
    ¬ Beats sc tb a a := by
  rw [beats_self_iff, htb.irrefl]; simp

omit [DecidableEq ι] in
theorem beats_trans {tb : ι → ι → Bool} (htb : StrictTotal tb) (sc : ι → Option Int) {a b c : ι}
    (h1 : Beats sc tb a b) (h2 : Beats sc tb b c) : Beats sc tb a c := by
  unfold Beats at *
  rcases h1 with h1 | ⟨e1, t1⟩ <;> rcases h2 with h2 | ⟨e2, t2⟩
  · exact Or.inl (oLt_of_le_of_lt (oLe_of_oLt h2) h1)
  · exact Or.inl (e2 ▸ h1)
  · exact Or.inl (e1 ▸ h2)
  · exact Or.inr ⟨e1.trans e2, htb.trans a b c t1 t2⟩

omit [DecidableEq ι] in
theorem beats_total {tb : ι → ι → Bool} (htb : StrictTotal tb) (sc : ι → Option Int) {a b : ι}
    (hab : a ≠ b) : Beats sc tb a b ∨ Beats sc tb b a := by
  unfold Beats
  by_cases he : sc a = sc b
  · rcases htb.total a b hab with h | h
    · exact Or.inl (Or.inr ⟨he, h⟩)
    · exact Or.inr (Or.inr ⟨he.symm, h⟩)
  · rcases oLe_total (sc a) (sc b) with h | h
    · exact Or.inr (Or.inl (oLt_of_le_of_ne h he))
    · exact Or.inl (Or.inl (oLt_of_le_of_ne h (Ne.symm he)))

omit [DecidableEq ι] in
/-- The recall order (score, then `tie_break_by_key`) never puts each of two
nodes ahead of the other. -/
theorem beats_asymm {tb : ι → ι → Bool} (htb : StrictTotal tb) (sc : ι → Option Int) {a b : ι}
    (h : Beats sc tb a b) : ¬ Beats sc tb b a :=
  fun h' => beats_irrefl htb sc a (beats_trans htb sc h h')

omit [DecidableEq ι] in
omit [DecidableEq ι] in
/-- A member that sorts ahead of another has a strictly smaller `ahead`. -/
theorem ahead_lt_of_beats {tb : ι → ι → Bool} (htb : StrictTotal tb) (sc : ι → Option Int)
    {ids : List ι} {a b : ι} (ha : a ∈ ids) (h : Beats sc tb a b) :
    ahead sc tb ids a < ahead sc tb ids b := by
  unfold ahead
  apply length_filter_lt (a := a) _ ha
  · simpa using h
  · simpa using beats_irrefl htb sc a
  · intro x hx
    simp only [decide_eq_true_eq] at hx ⊢
    exact beats_trans htb sc hx h

omit [DecidableEq ι] in
/-- **Leg rank order is the leg's score order**, score descending, ties by
`tb`: of two distinct nodes, the one with the smaller rank is the one that
sorts ahead. -/
theorem legRank_lt_iff {tb : ι → ι → Bool} (htb : StrictTotal tb) (sc : ι → Option Int)
    {ids : List ι} {a b : ι} (ha : a ∈ ids) (hb : b ∈ ids) (hab : a ≠ b) {ra rb : Nat}
    (hra : legRank sc tb ids a = some ra) (hrb : legRank sc tb ids b = some rb) :
    ra < rb ↔ Beats sc tb a b := by
  unfold legRank at hra hrb
  cases hsa : sc a <;> cases hsb : sc b <;> simp [hsa, hsb] at hra hrb
  subst hra hrb
  constructor
  · intro hlt
    rcases beats_total htb sc hab with h | h
    · exact h
    · have := ahead_lt_of_beats htb sc hb h; omega
  · intro h; have := ahead_lt_of_beats htb sc ha h; omega

omit [DecidableEq ι] in
/-- **Distinct members, distinct ranks** (the leg's ties are broken). -/
theorem legRank_injective {tb : ι → ι → Bool} (htb : StrictTotal tb) (sc : ι → Option Int)
    {ids : List ι} {a b : ι} (ha : a ∈ ids) (hb : b ∈ ids) (hab : a ≠ b) {r : Nat}
    (hra : legRank sc tb ids a = some r) (hrb : legRank sc tb ids b = some r) : False := by
  rcases beats_total htb sc hab with h | h
  · exact Nat.lt_irrefl r ((legRank_lt_iff htb sc ha hb hab hra hrb).mpr h)
  · exact Nat.lt_irrefl r ((legRank_lt_iff htb sc hb ha (Ne.symm hab) hrb hra).mpr h)

omit [DecidableEq ι] in
/-- **Ranks run from 1 to the leg's size.** -/
theorem legRank_le_members {tb : ι → ι → Bool} (htb : StrictTotal tb) (sc : ι → Option Int)
    {ids : List ι} {n : ι} (hn : n ∈ ids) {r : Nat} (hr : legRank sc tb ids n = some r) :
    1 ≤ r ∧ r ≤ (ids.filter fun m => (sc m).isSome).length := by
  unfold legRank at hr
  cases hs : sc n with
  | none => simp [hs] at hr
  | some v =>
    simp [hs] at hr
    subst hr
    refine ⟨by omega, ?_⟩
    have : ahead sc tb ids n < (ids.filter fun m => (sc m).isSome).length := by
      unfold ahead
      apply length_filter_lt (a := n) _ hn
      · simp [hs]
      · simpa using beats_irrefl htb sc n
      · intro x hx
        simp only [decide_eq_true_eq] at hx
        unfold Beats at hx
        rw [hs] at hx
        cases hsx : sc x <;> simp_all [OLt, OLe]
    omega

omit [DecidableEq ι] in
theorem legRank_le_length {tb : ι → ι → Bool} (htb : StrictTotal tb) (sc : ι → Option Int)
    {ids : List ι} {n : ι} (hn : n ∈ ids) {r : Nat} (hr : legRank sc tb ids n = some r) :
    r ≤ ids.length :=
  Nat.le_trans (legRank_le_members htb sc hn hr).2 (List.length_filter_le _ _)

omit [DecidableEq ι] in
/-- **Input order does not matter**: the rank is a function of the set of
candidates, whatever order the leg's `HashMap` iterates them in. -/
theorem legRank_perm (sc : ι → Option Int) (tb : ι → ι → Bool) {ids ids' : List ι}
    (h : ids.Perm ids') (n : ι) : legRank sc tb ids n = legRank sc tb ids' n := by
  unfold legRank ahead
  rw [(h.filter _).length_eq]

omit [DecidableEq ι] in
theorem recentRank_perm (idx : ι → Option Nat) {ids ids' : List ι} (h : ids.Perm ids') (n : ι) :
    recentRank idx ids n = recentRank idx ids' n := by
  unfold recentRank
  cases idx n with
  | none => rfl
  | some i => simp only [(h.filter _).length_eq]

omit [DecidableEq ι] in
/-- **Computed from the leg's own scores**: two slot functions that agree on
the candidates give the same ranks. -/
theorem legRank_congr {sc sc' : ι → Option Int} (tb : ι → ι → Bool) {ids : List ι} {n : ι}
    (h : ∀ m ∈ ids, sc m = sc' m) (hn : sc n = sc' n) :
    legRank sc tb ids n = legRank sc' tb ids n := by
  unfold legRank ahead
  rw [hn]
  cases sc' n with
  | none => rfl
  | some _ =>
    simp only
    congr 2
    exact congrArg List.length (List.filter_congr fun x hx => by simp only [Beats, h x hx, hn])

/-! ### One recall -/

/-- What `rank` reads besides the leg table. -/
structure RrfRecall (ι : Type) where
  cfg : RrfConfig
  /-- `tie_break_by_key` on the candidates' canonical keys and ids. -/
  tb : ι → ι → Bool
  /-- The candidates: the keys of the `LegProvenance` map, distinct. -/
  ids : List ι
  /-- `keyword_ran`: a text query searched the index. -/
  keywordRan : Bool
  /-- For each recent-leg member, which of the three most recent interactions
  owns it (`0` the newest). -/
  recentIdx : ι → Option Nat

namespace RrfRecall

variable (R : RrfRecall ι) (legs : ι → Legs)

/-- The common multiple every term is exact over. -/
def scale : Nat := prodDen R.cfg R.ids.length

/-- A node's three leg ranks. -/
def ranks (n : ι) : Ranks :=
  { keyword := legRank (fun m => (legs m).keyword) R.tb R.ids n
    vector := legRank (fun m => (legs m).vector) R.tb R.ids n
    recent := recentRank R.recentIdx R.ids n }

/-- The keyword leg counts toward `best` when it ran or has a member. -/
def kActive : Bool := R.keywordRan || R.ids.any fun m => (legs m).keyword.isSome
def vActive : Bool := R.ids.any fun m => (legs m).vector.isSome
def rActive : Bool := R.ids.any fun m => (R.recentIdx m).isSome

/-- `best`, scaled. -/
def best : Nat := bestSum R.cfg R.scale (R.kActive legs) (R.vActive legs) R.rActive

/-- `sum`, scaled. -/
def num (n : ι) : Nat := rrfSum R.cfg R.scale (R.ranks legs n)

/-- `l.rrf`, exactly, as numerator and denominator: `sum / best`, or `0` when
`best` is not above 0. -/
def rrf (n : ι) : Nat × Nat := if R.best legs = 0 then (0, 1) else (R.num legs n, R.best legs)

/-- The value phase 1 sorts on under `FusionKind::Rrf`, over the common
denominator `best` (`rrf_le_iff_key`). -/
def key (n : ι) : Option Int := some (if R.best legs = 0 then 0 else (R.num legs n : Int))

/-- A node's 0-based position in `rank`'s output under RRF. -/
def rrfAhead (n : ι) : Nat := ahead (R.key legs) R.tb R.ids n

/-- The RRF values of a list of nodes, for running the model against the
Rust on the same input. -/
def rrfs (ns : List ι) : List (ι × (Nat × Nat)) := ns.map fun n => (n, R.rrf legs n)

end RrfRecall

/-- `a ≤ b` on non-negative fractions `(num, den)`. -/
def RLe (a b : Nat × Nat) : Prop := a.1 * b.2 ≤ b.1 * a.2

/-- `a = b` on fractions. -/
def REq (a b : Nat × Nat) : Prop := a.1 * b.2 = b.1 * a.2

instance (a b : Nat × Nat) : Decidable (RLe a b) := inferInstanceAs (Decidable (_ ≤ _))
instance (a b : Nat × Nat) : Decidable (REq a b) := inferInstanceAs (Decidable (_ = _))

section RrfRecallThms

variable (R : RrfRecall ι) (legs : ι → Legs)

omit [DecidableEq ι] in
theorem ranks_pos (n : ι) : RankPos (R.ranks legs n).keyword ∧ RankPos (R.ranks legs n).vector ∧
    RankPos (R.ranks legs n).recent := by
  simp only [RrfRecall.ranks]
  exact ⟨legRank_pos _ _ _ _, legRank_pos _ _ _ _, recentRank_pos _ _ _⟩

omit [DecidableEq ι] in
omit [DecidableEq ι] in
/-- `fusion.rs` orders two nodes by `rrf` exactly as it orders them by their
sort key. -/
theorem rrf_le_iff_key (a b : ι) : RLe (R.rrf legs a) (R.rrf legs b) ↔ OLe (R.key legs a) (R.key legs b) := by
  unfold RrfRecall.rrf RrfRecall.key RLe
  by_cases h : R.best legs = 0
  · simp [h, OLe]
  · simp only [h, ite_false, OLe]
    rw [Nat.mul_le_mul_right_iff (by omega)]
    omega

omit [DecidableEq ι] in
omit [DecidableEq ι] in
/-- The sum never exceeds `best`. -/
theorem num_le_best {n : ι} (hn : n ∈ R.ids) : R.num legs n ≤ R.best legs := by
  unfold RrfRecall.num RrfRecall.best rrfSum bestSum
  have hk : term R.cfg R.scale R.cfg.wKeyword (R.ranks legs n).keyword ≤
      if R.kActive legs then R.cfg.wKeyword * (R.scale / R.cfg.den 1) else 0 := by
    cases hr : (R.ranks legs n).keyword with
    | none => simp [term]
    | some r =>
      have hs : ((legs n).keyword).isSome = true := by
        simp only [RrfRecall.ranks] at hr
        cases h : (legs n).keyword <;> simp_all [legRank]
      have : R.kActive legs = true := by
        simp only [RrfRecall.kActive, Bool.or_eq_true]
        exact Or.inr (any_of_mem (f := fun m => (legs m).keyword.isSome) hn hs)
      rw [this, ite_eq_left rfl, ← hr]
      exact term_le_best _ _ _ (legRank_pos (fun m => (legs m).keyword) R.tb R.ids n)
  have hv : term R.cfg R.scale R.cfg.wVector (R.ranks legs n).vector ≤
      if R.vActive legs then R.cfg.wVector * (R.scale / R.cfg.den 1) else 0 := by
    cases hr : (R.ranks legs n).vector with
    | none => simp [term]
    | some r =>
      have hs : ((legs n).vector).isSome = true := by
        simp only [RrfRecall.ranks] at hr
        cases h : (legs n).vector <;> simp_all [legRank]
      have : R.vActive legs = true := any_of_mem (f := fun m => (legs m).vector.isSome) hn hs
      rw [this, ite_eq_left rfl, ← hr]
      exact term_le_best _ _ _ (legRank_pos (fun m => (legs m).vector) R.tb R.ids n)
  have hr : term R.cfg R.scale R.cfg.wRecent (R.ranks legs n).recent ≤
      if R.rActive then R.cfg.wRecent * (R.scale / R.cfg.den 1) else 0 := by
    cases hr : (R.ranks legs n).recent with
    | none => simp [term]
    | some r =>
      have hs : (R.recentIdx n).isSome = true := by
        simp only [RrfRecall.ranks] at hr
        cases h : R.recentIdx n <;> simp_all [recentRank]
      have : R.rActive = true := any_of_mem (f := fun m => (R.recentIdx m).isSome) hn hs
      rw [this, ite_eq_left rfl, ← hr]
      exact term_le_best _ _ _ (recentRank_pos R.recentIdx R.ids n)
  omega

omit [DecidableEq ι] in
/-- **Range.** A candidate's `rrf` is in `[0, 1]` (`LegScores::rrf`'s doc). -/
theorem rrf_range {n : ι} (hn : n ∈ R.ids) :
    RLe (0, 1) (R.rrf legs n) ∧ RLe (R.rrf legs n) (1, 1) := by
  unfold RrfRecall.rrf RLe
  have := num_le_best R legs hn
  split <;> simp <;> omega

omit [DecidableEq ι] in
/-- **Rank 1 in every leg that ran is exactly 1**: the sum is `best`
(the test `rrf_is_one_for_rank_one_in_every_active_leg_and_ignores_empty_legs`). -/
theorem num_eq_best_of_rank_one {n : ι} (hn : n ∈ R.ids)
    (hk : R.kActive legs = true → (R.ranks legs n).keyword = some 1)
    (hv : R.vActive legs = true → (R.ranks legs n).vector = some 1)
    (hr : R.rActive = true → (R.ranks legs n).recent = some 1) :
    R.num legs n = R.best legs := by
  unfold RrfRecall.num RrfRecall.best rrfSum bestSum
  have e1 : term R.cfg R.scale R.cfg.wKeyword (R.ranks legs n).keyword =
      if R.kActive legs then R.cfg.wKeyword * (R.scale / R.cfg.den 1) else 0 := by
    cases ha : R.kActive legs
    · cases hr' : (R.ranks legs n).keyword with
      | none => rfl
      | some r =>
        have hs : ((legs n).keyword).isSome = true := by
          simp only [RrfRecall.ranks] at hr'
          cases h : (legs n).keyword <;> simp_all [legRank]
        have := any_of_mem (f := fun m => (legs m).keyword.isSome) hn hs
        simp [RrfRecall.kActive, this] at ha
    · rw [hk ha]; rfl
  have e2 : term R.cfg R.scale R.cfg.wVector (R.ranks legs n).vector =
      if R.vActive legs then R.cfg.wVector * (R.scale / R.cfg.den 1) else 0 := by
    cases ha : R.vActive legs
    · cases hr' : (R.ranks legs n).vector with
      | none => rfl
      | some r =>
        have hs : ((legs n).vector).isSome = true := by
          simp only [RrfRecall.ranks] at hr'
          cases h : (legs n).vector <;> simp_all [legRank]
        have := any_of_mem (f := fun m => (legs m).vector.isSome) hn hs
        simp_all [RrfRecall.vActive]
    · rw [hv ha]; rfl
  have e3 : term R.cfg R.scale R.cfg.wRecent (R.ranks legs n).recent =
      if R.rActive then R.cfg.wRecent * (R.scale / R.cfg.den 1) else 0 := by
    cases ha : R.rActive
    · cases hr' : (R.ranks legs n).recent with
      | none => rfl
      | some r =>
        have hs : (R.recentIdx n).isSome = true := by
          simp only [RrfRecall.ranks] at hr'
          cases h : R.recentIdx n <;> simp_all [recentRank]
        have := any_of_mem (f := fun m => (R.recentIdx m).isSome) hn hs
        simp_all [RrfRecall.rActive]
    · rw [hr ha]; rfl
  rw [e1, e2, e3]

omit [DecidableEq ι] in
/-- **Input order does not matter** for the whole recall: permuting the
candidates changes no `rrf` value and no position. -/
theorem rrf_perm {ids' : List ι} (h : R.ids.Perm ids') (n : ι) :
    ({ R with ids := ids' }).rrf legs n = R.rrf legs n ∧
    ({ R with ids := ids' }).rrfAhead legs n = R.rrfAhead legs n := by
  have hs : ({ R with ids := ids' } : RrfRecall ι).scale = R.scale := by
    simp [RrfRecall.scale, h.length_eq]
  have hr : ∀ m, ({ R with ids := ids' } : RrfRecall ι).ranks legs m = R.ranks legs m := by
    intro m
    simp only [RrfRecall.ranks]
    rw [legRank_perm _ _ h.symm, legRank_perm _ _ h.symm, recentRank_perm _ h.symm]
  have hb : ({ R with ids := ids' } : RrfRecall ι).best legs = R.best legs := by
    simp only [RrfRecall.best, RrfRecall.kActive, RrfRecall.vActive, RrfRecall.rActive, hs]
    rw [h.any_eq, h.any_eq, h.any_eq]
  have hn : ∀ m, ({ R with ids := ids' } : RrfRecall ι).num legs m = R.num legs m := by
    intro m; simp only [RrfRecall.num, hr, hs]
  have hk : ({ R with ids := ids' } : RrfRecall ι).key legs = R.key legs := by
    funext m; simp only [RrfRecall.key, hb, hn]
  refine ⟨?_, ?_⟩
  · simp only [RrfRecall.rrf, hb, hn]
  · simp only [RrfRecall.rrfAhead, hk, ahead]
    exact (h.symm.filter _).length_eq

omit [DecidableEq ι] in
/-- **One leg: RRF orders as the leg does.** When the keyword leg is the only
leg with members and its weight is above 0, two of its members sort under RRF
exactly as they sort in the leg, by BM25 descending then the tie-break. With a
weight of 0 the leg contributes nothing, as designed: every `rrf` is 0
(`rrf_one_leg_needs_weight`). -/
theorem rrf_one_leg_agrees (hv : R.cfg.Valid) (hw : 0 < R.cfg.wKeyword)
    (htb : StrictTotal R.tb)
    (honly : ∀ m ∈ R.ids, (legs m).vector = none ∧ R.recentIdx m = none)
    {a b : ι} (ha : a ∈ R.ids) (hb : b ∈ R.ids)
    (hka : ((legs a).keyword).isSome = true) (hkb : ((legs b).keyword).isSome = true) :
    Beats (R.key legs) R.tb a b ↔ Beats (fun m => (legs m).keyword) R.tb a b := by
  by_cases hab : a = b
  · subst hab; rw [beats_self_iff, beats_self_iff]
  let sc := fun m => (legs m).keyword
  have hrank : ∀ m ∈ R.ids, (sc m).isSome = true →
      ∃ r, legRank sc R.tb R.ids m = some r ∧ 1 ≤ r ∧ r ≤ R.ids.length ∧
        R.num legs m = R.cfg.wKeyword * (R.scale / R.cfg.den r) := by
    intro m hm hs
    cases hsm : sc m with
    | none => simp [hsm] at hs
    | some v =>
      refine ⟨ahead sc R.tb R.ids m + 1, by simp [legRank, hsm], by omega, ?_, ?_⟩
      · exact legRank_le_length htb sc hm (by simp [legRank, hsm])
      · obtain ⟨h1, h2⟩ := honly m hm
        have hvr : legRank (fun m => (legs m).vector) R.tb R.ids m = none :=
          (legRank_eq_none _ _ _ _).mpr h1
        have hrr : recentRank R.recentIdx R.ids m = none := by simp [recentRank, h2]
        simp only [RrfRecall.num, rrfSum, RrfRecall.ranks, hvr, hrr]
        have : legRank (fun m => (legs m).keyword) R.tb R.ids m = some (ahead sc R.tb R.ids m + 1) := by
          show legRank sc R.tb R.ids m = _
          simp [legRank, hsm]
        rw [this]; simp [term]
  obtain ⟨ra, hra, ha1, haN, hna⟩ := hrank a ha hka
  obtain ⟨rb, hrb, hb1, hbN, hnb⟩ := hrank b hb hkb
  have hbest : R.best legs ≠ 0 := by
    have hpos : 0 < R.cfg.wKeyword * (R.scale / R.cfg.den 1) := by
      apply Nat.mul_pos hw
      have hd := den_dvd_prodDen R.cfg (N := R.ids.length) (Nat.le_refl 1)
        (Nat.le_trans ha1 haN)
      apply Nat.div_pos (Nat.le_of_dvd (prodDen_pos hv _) hd) (den_pos hv (Nat.le_refl 1))
    have hk : R.kActive legs = true := by
      simp [RrfRecall.kActive, any_of_mem (f := fun m => (legs m).keyword.isSome) ha hka]
    simp only [RrfRecall.best, bestSum, hk, ite_true]
    omega
  have key_eq : ∀ m r, R.num legs m = R.cfg.wKeyword * (R.scale / R.cfg.den r) →
      R.key legs m = some ((R.cfg.wKeyword * (R.scale / R.cfg.den r) : Nat) : Int) := by
    intro m r h; simp [RrfRecall.key, hbest, h]
  have hiff := legRank_lt_iff htb sc ha hb hab hra hrb
  have hstrict : ∀ {x y : Nat}, 1 ≤ x → x < y → y ≤ R.ids.length →
      R.cfg.wKeyword * (R.scale / R.cfg.den y) < R.cfg.wKeyword * (R.scale / R.cfg.den x) :=
    fun h1 h2 h3 => (Nat.mul_lt_mul_left hw).mpr (div_den_strict hv h1 h2 h3)
  show Beats (R.key legs) R.tb a b ↔ Beats sc R.tb a b
  rw [← hiff]
  unfold Beats
  rw [key_eq a ra hna, key_eq b rb hnb]
  simp only [OLt, OLe, Option.some.injEq]
  constructor
  · rintro (h | ⟨h, _⟩)
    · rcases Nat.lt_trichotomy ra rb with hlt | heq | hgt
      · exact hlt
      · subst heq; exact absurd (legRank_injective htb sc ha hb hab hra hrb) id
      · have := hstrict hb1 hgt haN; omega
    · rcases Nat.lt_trichotomy ra rb with hlt | heq | hgt
      · exact hlt
      · exact absurd (legRank_injective htb sc ha hb hab hra (heq ▸ hrb)) id
      · have := hstrict hb1 hgt haN; omega
  · intro hlt
    left
    have := hstrict ha1 hlt hbN
    omega

end RrfRecallThms

/-! ### Examples: the Rust tests' arithmetic -/

/-- `rrf_ranks_two_leg_agreement_above_one_leg_with_a_bigger_raw_score`, at
`k = 60`: `a` (node 1) is keyword rank 1 with BM25 `9.0`; `b` (node 2) is
keyword rank 2 and vector rank 1; node 3 is vector rank 2. -/
def twoLegLegs : Nat → Legs
  | 1 => ⟨some 9000, none, none⟩
  | 2 => ⟨some 2000, none, some 800⟩
  | 3 => ⟨none, none, some 500⟩
  | _ => ⟨none, none, none⟩

def twoLegRecall : RrfRecall Nat :=
  { cfg := rrfK60, tb := idOrder, ids := [1, 2, 3], keywordRan := true,
    recentIdx := fun _ => none }

/-- The ranks and values the Rust test asserts: `b` is
`(1/62 + 1/61) / (2/61) = 123/124`, `a` is `1/2`, node 3 is `61/124`; RRF
sorts `b, a, 3` where max fusion puts `a` first. -/
theorem rrf_two_leg_example :
    twoLegRecall.ranks twoLegLegs 2 = ⟨some 2, some 1, none⟩ ∧
    twoLegRecall.ranks twoLegLegs 3 = ⟨none, some 2, none⟩ ∧
    REq (twoLegRecall.rrf twoLegLegs 2) (123, 124) ∧
    REq (twoLegRecall.rrf twoLegLegs 1) (1, 2) ∧
    REq (twoLegRecall.rrf twoLegLegs 3) (61, 124) ∧
    twoLegRecall.rrfAhead twoLegLegs 2 = 0 ∧
    twoLegRecall.rrfAhead twoLegLegs 1 = 1 ∧
    twoLegRecall.rrfAhead twoLegLegs 3 = 2 ∧
    ahead (fun n => (twoLegLegs n).merged) idOrder [1, 2, 3] 1 = 0 := by
  decide

/-- `rrf_ranks_the_recent_leg_by_recency_with_one_rank_per_interaction` and
`rrf_counts_a_keyword_leg_that_ran_empty_but_not_one_that_did_not_run`:
nodes 2 and 5 belong to the newest interaction, 3 to the next, 1 to the third.
The ranks are `1, 1, 3, 4`; the keyword leg ran and found nothing, so the
newest member scores `0.3 / (1.0 + 0.3) = 3/13`, not 1 (**an absent leg
contributes 0 but still counts in `best`**); with no text query it is 1. -/
def recentRecall (ran : Bool) : RrfRecall Nat :=
  { cfg := rrfK60, tb := idOrder, ids := [2, 5, 3, 1], keywordRan := ran,
    recentIdx := fun n => match n with
      | 2 => some 0 | 5 => some 0 | 3 => some 1 | 1 => some 2 | _ => none }

def recentLegs : Nat → Legs := fun _ => ⟨none, some 350, none⟩

/-- `RecallFusion`'s recent leg on the example input: it ranks by recency, and
the `rrf` values follow from those ranks. -/
theorem rrf_recent_example :
    (recentRecall true).ranks recentLegs 2 = ⟨none, none, some 1⟩ ∧
    ((recentRecall true).ranks recentLegs 5).recent = some 1 ∧
    ((recentRecall true).ranks recentLegs 3).recent = some 3 ∧
    ((recentRecall true).ranks recentLegs 1).recent = some 4 ∧
    REq ((recentRecall true).rrf recentLegs 2) (3, 13) ∧
    REq ((recentRecall false).rrf recentLegs 2) (1, 1) ∧
    (recentRecall true).rrfAhead recentLegs 5 = 1 := by
  decide

/-- `rrf_ties_break_by_canonical_key_and_a_zero_weight_leg_scores_zero`:
rank 1 in one leg each is a tie, broken by `tb`; and a recall whose only leg
has weight 0 scores 0 (no division by a zero `best`). -/
def tieLegs : Nat → Legs
  | 3 => ⟨some 1000, none, none⟩
  | 1 => ⟨none, none, some 900⟩
  | _ => ⟨none, none, none⟩

/-- A recall whose only leg, the recent one, has weight 0. -/
def zeroRecall : RrfRecall Nat :=
  { cfg := { rrfK60 with wRecent := 0 }, tb := idOrder, ids := [1], keywordRan := false,
    recentIdx := fun n => if n = 1 then some 0 else none }

def tieRecall : RrfRecall Nat :=
  { cfg := rrfK60, tb := idOrder, ids := [3, 1], keywordRan := true, recentIdx := fun _ => none }

/-- RRF ties break by `tie_break_by_key`, and a zero-weight leg scores zero
(test `rrf_ties_break_by_canonical_key_and_a_zero_weight_leg_scores_zero`). -/
theorem rrf_tie_example :
    tieRecall.key tieLegs 1 = tieRecall.key tieLegs 3 ∧ tieRecall.rrfAhead tieLegs 1 = 0 ∧
    zeroRecall.best recentLegs = 0 ∧ zeroRecall.rrf recentLegs 1 = (0, 1) := by
  decide

/-- A keyword-only recall: BM25 `2.0, 2.0, 3.0` on nodes `0, 1, 2`. The leg
sorts `2, 0, 1` (the tie by `tb`), and so does RRF (`rrf_one_leg_agrees`). -/
def oneLegLegs : Nat → Legs
  | 0 => ⟨some 2000, none, none⟩
  | 1 => ⟨some 2000, none, none⟩
  | 2 => ⟨some 3000, none, none⟩
  | _ => ⟨none, none, none⟩

def oneLegRecall (c : RrfConfig) : RrfRecall Nat :=
  { cfg := c, tb := idOrder, ids := [0, 1, 2], keywordRan := true, recentIdx := fun _ => none }

/-- With one active leg, RRF (`fusion.rs`) orders the candidates as that leg
ranks them. -/
theorem rrf_one_leg_example :
    (oneLegRecall rrfDefault).rrfAhead oneLegLegs 2 = 0 ∧
    (oneLegRecall rrfDefault).rrfAhead oneLegLegs 0 = 1 ∧
    (oneLegRecall rrfDefault).rrfAhead oneLegLegs 1 = 2 ∧
    ahead (fun n => (oneLegLegs n).keyword) idOrder [0, 1, 2] 2 = 0 := by
  decide

/-- The leg ranks `RecallFusion` assigns on the one-leg example: BM25 `3.0`
first, then the `2.0` tie broken by `tie_break_by_key`, whatever order the
candidates arrive in. -/
theorem legRank_example :
    (oneLegRecall rrfDefault).ranks oneLegLegs 2 = ⟨some 1, none, none⟩ ∧
    (oneLegRecall rrfDefault).ranks oneLegLegs 0 = ⟨some 2, none, none⟩ ∧
    (oneLegRecall rrfDefault).ranks oneLegLegs 1 = ⟨some 3, none, none⟩ ∧
    legRank (fun n => (oneLegLegs n).keyword) idOrder [1, 2, 0] 1 = some 3 ∧
    legRank (fun n => (oneLegLegs n).keyword) idOrder [1, 2, 0] 2 = some 1 ∧
    REq ((oneLegRecall rrfDefault).rrf oneLegLegs 1) (2, 3) := by
  decide

/-- **The weight hypothesis is needed.** A leg of weight 0 contributes nothing
by design: `RecallFusion::validate` admits a keyword weight of 0 beside a
positive vector weight, and the test
`rrf_ties_break_by_canonical_key_and_a_zero_weight_leg_scores_zero` asserts the
zero-`best` case on purpose. A recall where only the keyword leg produced (no
embedding, no recent interaction) then scores every candidate's relevance 0,
so phase 1's own order is the tie-break (node 2, the best BM25, comes last
here). Downstream that order is not what a user sees: phase 1 returns the
union unreduced and expansion does not cut by it, so the final order falls to
the daemon's share of `final = daemon × w_daemon + relevance × w_query`. -/
theorem rrf_one_leg_needs_weight :
    let c := { rrfDefault with wKeyword := 0 }
    (oneLegRecall c).best oneLegLegs = 0 ∧
    (oneLegRecall c).rrfAhead oneLegLegs 2 = 2 ∧
    (oneLegRecall c).rrfAhead oneLegLegs 0 = 0 := by
  decide

/-! ### The exact-name step under RRF

The name step writes only the named concept's keyword slot and raises it
(`full_keyword_ge_leg`, `full_local`). Under max fusion that leaves every other
node's fused value alone. Under RRF it does not: `rank` re-ranks the keyword
leg, and a node the named concept passes drops one keyword place. -/

/-- The named concept passes `n` in the leg: it sorts ahead of `n` after the
step and did not before. -/
def Passes (sc sc' : ι → Option Int) (tb : ι → ι → Bool) (ids : List ι) (t n : ι) : Prop :=
  t ∈ ids ∧ Beats sc' tb t n ∧ ¬ Beats sc tb t n

instance (sc sc' : ι → Option Int) (tb : ι → ι → Bool) (ids : List ι) (t n : ι) :
    Decidable (Passes sc sc' tb ids t n) := by
  unfold Passes; infer_instance

/-- **Another node moves at most one place**, and exactly when the named
concept passes it. -/
theorem legRank_raise_other {sc sc' : ι → Option Int} {tb : ι → ι → Bool} {ids : List ι}
    {t n : ι} (hloc : ∀ m, m ≠ t → sc' m = sc m) (hup : OLe (sc t) (sc' t))
    (hnd : ids.Nodup) (hn : n ≠ t) :
    legRank sc' tb ids n =
      (legRank sc tb ids n).map (· + if Passes sc sc' tb ids t n then 1 else 0) := by
  unfold legRank
  rw [hloc n hn]
  cases hs : sc n with
  | none => rfl
  | some v =>
    simp only [Option.map_some]
    congr 1
    unfold ahead
    rw [length_filter_shift hnd (fun j => decide (Beats sc tb j n))
      (fun j => decide (Beats sc' tb j n)) t]
    · simp only [Passes, decide_eq_true_eq, decide_eq_false_iff_not]
      omega
    · intro x hx
      simp only [Beats, hloc x hx, hloc n hn]
    · intro h
      simp only [decide_eq_true_eq] at h ⊢
      exact beats_of_raise hloc hup hn h

omit [DecidableEq ι] in
/-- **The named concept's rank never gets worse**, and it may join the leg. -/
theorem legRank_raise_self {sc sc' : ι → Option Int} {tb : ι → ι → Bool} {ids : List ι}
    {t : ι} (hloc : ∀ m, m ≠ t → sc' m = sc m) (hup : OLe (sc t) (sc' t)) :
    RankLe (legRank sc' tb ids t) (legRank sc tb ids t) := by
  unfold legRank
  cases hs : sc t with
  | none => cases sc' t <;> simp [RankLe]
  | some v =>
    rw [hs] at hup
    cases hs' : sc' t with
    | none => rw [hs'] at hup; exact absurd hup id
    | some v' =>
      rw [hs'] at hup
      simp only [RankLe]
      have : ahead sc' tb ids t ≤ ahead sc tb ids t := by
        unfold ahead
        apply length_filter_mono
        intro j hj
        simp only [decide_eq_true_eq] at hj ⊢
        exact beats_of_raise_rev hloc (by rw [hs, hs']; exact hup) hj
      omega

omit [DecidableEq ι] in
/-- Raising one node and lowering (or keeping) every other lets nobody
overtake it. -/
theorem beats_of_raise_lower_rev {sc sc' : ι → Option Int} {tb : ι → ι → Bool} {t j : ι}
    (hlow : ∀ n, n ≠ t → OLe (sc' n) (sc n)) (hup : OLe (sc t) (sc' t))
    (h : Beats sc' tb j t) : Beats sc tb j t := by
  by_cases hj : j = t
  · subst hj
    rw [beats_self_iff] at h ⊢; exact h
  · unfold Beats at *
    have hl := hlow j hj
    rcases h with h | ⟨heq, htb⟩
    · exact Or.inl (oLt_of_le_of_lt hup (oLt_of_lt_of_le h hl))
    · have h1 : OLe (sc t) (sc j) := oLe_trans hup (heq ▸ hl)
      by_cases hs : sc j = sc t
      · exact Or.inr ⟨hs, htb⟩
      · exact Or.inl (oLt_of_le_of_ne h1 (Ne.symm hs))

/-- The keyword slots of a phase-1 table. -/
def kwSlots (legs : ι → Legs) : ι → Option Int := fun m => (legs m).keyword

section NameRrf

variable (inp : Input ι) {t : ι} (ht : inp.nameTarget = some t) (R : RrfRecall ι)
include ht

omit ht in
theorem full_vector_eq (n : ι) : ((phase1 (raiseFullName inp.ranked) inp) n).vector = ((phase1 noName inp) n).vector := by
  rw [phase1_eq (raiseFullName_keywordOnly _)]

theorem full_kw_loc : ∀ m, m ≠ t → kwSlots (phase1 (raiseFullName inp.ranked) inp) m = kwSlots (phase1 noName inp) m :=
  fun _ hm => by simp only [kwSlots]; rw [full_local inp ht hm]

theorem full_kw_up : OLe (kwSlots (phase1 noName inp) t) (kwSlots (phase1 (raiseFullName inp.ranked) inp) t) := full_keyword_ge_leg inp ht

/-- **The named concept's keyword rank never gets worse.** -/
theorem rrf_full_rank_named :
    RankLe (R.ranks (phase1 (raiseFullName inp.ranked) inp) t).keyword (R.ranks (phase1 noName inp) t).keyword :=
  legRank_raise_self (full_kw_loc inp ht) (full_kw_up inp ht)

/-- **Another node's keyword rank moves by at most one place**, down, and
only when the named concept passes it. -/
theorem rrf_full_rank_other (hnd : R.ids.Nodup) {n : ι} (hn : n ≠ t) :
    (R.ranks (phase1 (raiseFullName inp.ranked) inp) n).keyword = (R.ranks (phase1 noName inp) n).keyword.map
      (· + if Passes (kwSlots (phase1 noName inp)) (kwSlots (phase1 (raiseFullName inp.ranked) inp)) R.tb R.ids t n then 1 else 0) :=
  legRank_raise_other (full_kw_loc inp ht) (full_kw_up inp ht) hnd hn

omit ht in
/-- The vector and recent ranks do not move at all. -/
theorem rrf_full_rank_vector_recent (n : ι) :
    (R.ranks (phase1 (raiseFullName inp.ranked) inp) n).vector = (R.ranks (phase1 noName inp) n).vector ∧
    (R.ranks (phase1 (raiseFullName inp.ranked) inp) n).recent = (R.ranks (phase1 noName inp) n).recent := by
  refine ⟨?_, rfl⟩
  simp only [RrfRecall.ranks]
  apply legRank_congr <;> intros <;> exact full_vector_eq inp _

omit ht in
/-- `best` does not move when the keyword leg ran, as it did whenever there
is a name match (a blank query has no normalized name). -/
theorem rrf_full_best (hkr : R.keywordRan = true) : R.best (phase1 (raiseFullName inp.ranked) inp) = R.best (phase1 noName inp) := by
  have hv : R.vActive (phase1 (raiseFullName inp.ranked) inp) = R.vActive (phase1 noName inp) := by
    simp only [RrfRecall.vActive, full_vector_eq]
  simp only [RrfRecall.best, RrfRecall.kActive, hkr, Bool.true_or, hv]

/-- **The named concept's `rrf` never falls** under the step. -/
theorem rrf_full_named_up (hkr : R.keywordRan = true) :
    R.num (phase1 noName inp) t ≤ R.num (phase1 (raiseFullName inp.ranked) inp) t ∧ OLe (R.key (phase1 noName inp) t) (R.key (phase1 (raiseFullName inp.ranked) inp) t) := by
  have hle : R.num (phase1 noName inp) t ≤ R.num (phase1 (raiseFullName inp.ranked) inp) t := by
    unfold RrfRecall.num
    obtain ⟨hv, hr⟩ := rrf_full_rank_vector_recent inp R t
    obtain ⟨p1, p2, p3⟩ := ranks_pos R (phase1 (raiseFullName inp.ranked) inp) t
    apply rrfSum_mono _ _ p1 p2 p3
    · exact rrf_full_rank_named inp ht R
    · rw [hv]; cases (R.ranks (phase1 noName inp) t).vector <;> simp [RankLe]
    · rw [hr]; cases (R.ranks (phase1 noName inp) t).recent <;> simp [RankLe]
  refine ⟨hle, ?_⟩
  simp only [RrfRecall.key, rrf_full_best inp R hkr, OLe]
  split <;> omega

/-- **Every other node's `rrf` can only fall**, and does not move unless the
named concept passes it; then its keyword rank is exactly one worse. -/
theorem rrf_full_other (hkr : R.keywordRan = true) (hnd : R.ids.Nodup) {n : ι} (hn : n ≠ t) :
    R.num (phase1 (raiseFullName inp.ranked) inp) n ≤ R.num (phase1 noName inp) n ∧
    (¬ Passes (kwSlots (phase1 noName inp)) (kwSlots (phase1 (raiseFullName inp.ranked) inp)) R.tb R.ids t n → R.num (phase1 (raiseFullName inp.ranked) inp) n = R.num (phase1 noName inp) n) ∧
    (Passes (kwSlots (phase1 noName inp)) (kwSlots (phase1 (raiseFullName inp.ranked) inp)) R.tb R.ids t n →
      (R.ranks (phase1 (raiseFullName inp.ranked) inp) n).keyword = (R.ranks (phase1 noName inp) n).keyword.map (· + 1)) ∧
    OLe (R.key (phase1 (raiseFullName inp.ranked) inp) n) (R.key (phase1 noName inp) n) := by
  have hk := rrf_full_rank_other inp ht R hnd hn
  obtain ⟨hv, hr⟩ := rrf_full_rank_vector_recent inp R n
  have hle : R.num (phase1 (raiseFullName inp.ranked) inp) n ≤ R.num (phase1 noName inp) n := by
    unfold RrfRecall.num
    obtain ⟨p1, p2, p3⟩ := ranks_pos R (phase1 noName inp) n
    apply rrfSum_mono _ _ p1 p2 p3
    · rw [hk]; cases (R.ranks (phase1 noName inp) n).keyword <;> simp [RankLe]
    · rw [hv]; cases (R.ranks (phase1 noName inp) n).vector <;> simp [RankLe]
    · rw [hr]; cases (R.ranks (phase1 noName inp) n).recent <;> simp [RankLe]
  refine ⟨hle, ?_, ?_, ?_⟩
  · intro hp
    rw [ite_eq_right hp] at hk
    unfold RrfRecall.num rrfSum
    rw [hk, hv, hr]
    cases (R.ranks (phase1 noName inp) n).keyword <;> rfl
  · intro hp
    rw [ite_eq_left hp] at hk
    exact hk
  · simp only [RrfRecall.key, rrf_full_best inp R hkr, OLe]
    split <;> omega

/-- **Its position cannot get worse** under RRF either: no more nodes sort
ahead of the named concept after the step than before it. -/
theorem rrf_full_ahead_le (hkr : R.keywordRan = true) (hnd : R.ids.Nodup) :
    R.rrfAhead (phase1 (raiseFullName inp.ranked) inp) t ≤ R.rrfAhead (phase1 noName inp) t := by
  unfold RrfRecall.rrfAhead ahead
  apply length_filter_mono
  intro j hj
  simp only [decide_eq_true_eq] at hj ⊢
  exact beats_of_raise_lower_rev (fun n hn => (rrf_full_other inp ht R hkr hnd hn).2.2.2)
    (rrf_full_named_up inp ht R hkr).2 hj

end NameRrf

/-! ### When the shift happens: only from the `1.0` floor

The ranked list is the BM25 search, sorted by score descending then the same
tie-break (`search_ranked`). Under that hypothesis the step moves another
node's keyword rank only when the floor lifts the named concept: its old
keyword slot was below `1.0` (or empty), and the node it passes scores at most
`1.0`. A BM25 the named concept already had, in the leg or beyond the cap,
passes nobody. -/

/-- `search_ranked`'s order: score descending, ties by `tb`. -/
def SortedBy (tb : ι → ι → Bool) (ranked : List (ι × Int)) : Prop :=
  ranked.Pairwise fun a b => b.2 < a.2 ∨ (a.2 = b.2 ∧ tb a.1 b.1 = true)

theorem lastScore_isSome_of_mem {l : List (ι × Int)} {n : ι} {s : Int} (h : (n, s) ∈ l) :
    (lastScore l n).isSome = true := by
  induction l with
  | nil => simp at h
  | cons p rest ih =>
    obtain ⟨i, s'⟩ := p
    simp only [lastScore]
    rcases List.mem_cons.mp h with h | h
    · cases h' : lastScore rest n with
      | some _ => rfl
      | none => simp only [Prod.mk.injEq] at h; simp [h.1]
    · rw [Option.isSome_iff_exists] at ih
      obtain ⟨u, hu⟩ := ih h
      simp [hu]

theorem mem_of_findScore {l : List (ι × Int)} {n : ι} {s : Int} (h : findScore l n = some s) :
    (n, s) ∈ l := by
  induction l with
  | nil => simp [findScore] at h
  | cons p rest ih =>
    obtain ⟨i, s'⟩ := p
    simp only [findScore] at h
    by_cases hi : i = n
    · subst hi; simp at h; subst h; exact List.mem_cons_self
    · rw [ite_eq_right hi] at h; exact List.mem_cons_of_mem _ (ih h)

theorem not_mem_of_findScore {l : List (ι × Int)} {n : ι} (h : findScore l n = none) :
    n ∉ l.map Prod.fst := by
  induction l with
  | nil => simp
  | cons p rest ih =>
    obtain ⟨i, s'⟩ := p
    simp only [findScore] at h
    by_cases hi : i = n
    · rw [ite_eq_left hi] at h; cases h
    · rw [ite_eq_right hi] at h
      simp only [List.map_cons, List.mem_cons, not_or]
      exact ⟨Ne.symm hi, ih h⟩

/-- Beyond the cap, every member of the keyword leg sorts ahead of the named
concept's hit in the search order. -/
theorem in_cap_ahead_of_beyond {tb : ι → ι → Bool} {ranked : List (ι × Int)} {cap : Nat}
    (hsort : SortedBy tb ranked) {n t : ι} {v s : Int}
    (hn : keywordLeg ranked cap n = some v) (ht : keywordLeg ranked cap t = none)
    (hts : (t, s) ∈ ranked) : s < v ∨ (v = s ∧ tb n t = true) := by
  have hnm : (n, v) ∈ ranked.take cap := mem_of_lastScore hn
  have htm : (t, s) ∈ ranked.drop cap := by
    have hall : (t, s) ∈ ranked.take cap ++ ranked.drop cap := by
      rw [List.take_append_drop]; exact hts
    rcases List.mem_append.mp hall with h | h
    · have := lastScore_isSome_of_mem h
      simp [keywordLeg] at ht; rw [ht] at this; cases this
    · exact h
  unfold SortedBy at hsort
  rw [← List.take_append_drop cap ranked, List.pairwise_append] at hsort
  exact hsort.2.2 _ hnm _ htm

/-- **Shifts come only from the floor.** Under the full-list step, when the
named concept passes another member of the keyword leg, that member's keyword
score is at most `1.0` and the named concept's own slot before the step was
below `1.0` or empty. -/
theorem full_passes_only_below_floor (inp : Input ι) {t : ι} (ht : inp.nameTarget = some t)
    {tb : ι → ι → Bool} (htb : StrictTotal tb) (hnd : (inp.ranked.map Prod.fst).Nodup)
    (hsort : SortedBy tb inp.ranked) {ids : List ι} {n : ι} (hn : n ≠ t)
    (hmem : (kwSlots (phase1 noName inp) n).isSome = true)
    (hp : Passes (kwSlots (phase1 noName inp)) (kwSlots (phase1 (raiseFullName inp.ranked) inp))
      tb ids t n) :
    OLe (kwSlots (phase1 noName inp) n) (some one) ∧
    OLt (kwSlots (phase1 noName inp) t) (some one) := by
  obtain ⟨_, hafter, hbefore⟩ := hp
  have hkb : ∀ m, kwSlots (phase1 noName inp) m = keywordLeg inp.ranked inp.cap m :=
    fun m => phase1_noName_keyword inp m
  have hka : ∀ m, m ≠ t → kwSlots (phase1 (raiseFullName inp.ranked) inp) m =
      keywordLeg inp.ranked inp.cap m := fun m hm => by
    rw [full_kw_loc inp ht m hm, hkb]
  obtain ⟨v, hv⟩ := Option.isSome_iff_exists.mp hmem
  rw [hkb] at hv
  unfold Beats at hafter hbefore
  rw [hka n hn, hv] at hafter
  rw [hkb, hkb, hv] at hbefore
  have hkt := full_keyword inp ht t
  rw [ite_eq_left rfl, phase1_noName_keyword] at hkt
  change kwSlots (phase1 (raiseFullName inp.ranked) inp) t = _ at hkt
  rw [hkb, hkb, hv]
  cases hf : findScore inp.ranked t with
  | none =>
    have hnt : keywordLeg inp.ranked inp.cap t = none := by
      apply lastScore_of_not_mem
      intro hm; rw [List.map_take] at hm
      exact not_mem_of_findScore hf (List.mem_of_mem_take hm)
    rw [hf, hnt] at hkt
    simp only [mergeHit, mergeMax] at hkt
    rw [hkt] at hafter
    rw [hnt]
    simp [OLt, OLe] at hafter ⊢
    unfold one at *
    omega
  | some s =>
    rw [hf] at hkt
    cases hkb_t : keywordLeg inp.ranked inp.cap t with
    | some s' =>
      have hs' : s' = s := by
        have := keywordLeg_eq_findScore hnd hkb_t; rw [hf] at this; cases this; rfl
      subst hs'
      rw [hkb_t] at hkt hbefore
      simp only [mergeHit, mergeMax, imax, ite_self] at hkt
      rw [hkt] at hafter
      by_cases hs1 : s' ≤ one
      · rw [ite_eq_left hs1] at hafter
        simp only [OLt, OLe, Option.some.injEq] at hafter hbefore ⊢
        unfold one at *
        refine ⟨?_, ?_⟩
        · rcases hafter with h | ⟨h, _⟩ <;> omega
        · intro hge
          apply hbefore
          rcases hafter with h | ⟨h, htn⟩
          · left; omega
          · right; exact ⟨by omega, htn⟩
      · rw [ite_eq_right hs1] at hafter
        exact absurd hafter hbefore
    | none =>
      rw [hkb_t] at hkt hbefore
      simp only [mergeHit, mergeMax, imax] at hkt
      have hrel := in_cap_ahead_of_beyond hsort hv hkb_t (mem_of_findScore hf)
      rw [hkt] at hafter
      refine ⟨?_, fun h => h⟩
      by_cases hs1 : s ≤ one
      · rw [ite_eq_left hs1] at hafter
        simp only [OLt, OLe, Option.some.injEq] at hafter ⊢
        rcases hafter with h | ⟨h, _⟩ <;> omega
      · rw [ite_eq_right hs1] at hafter
        simp only [OLt, OLe, Option.some.injEq] at hafter
        exfalso
        rcases hrel with hrel | ⟨hrel, htn⟩
        · rcases hafter with h | ⟨h, _⟩ <;> omega
        · rcases hafter with h | ⟨_, h⟩
          · omega
          · rw [htb.asymm htn] at h; cases h

/-- Abstract core of `full_rank_eq_raise`: a beyond-cap named concept at the
floor `1.0` and at its BM25 `s > 1.0` sorts the same way against everyone, as
every member of the leg scores at least `s` and sorts ahead of it at a tie. -/
theorem beats_floor_eq {kb kf kr : ι → Option Int} {tb : ι → ι → Bool} (htb : StrictTotal tb)
    {t : ι} {s : Int} (hs : one < s) (htf : kf t = some s) (htr : kr t = some one)
    (hb : kb t = none) (hf : ∀ m, m ≠ t → kf m = kb m) (hr : ∀ m, m ≠ t → kr m = kb m)
    (hrel : ∀ m v, kb m = some v → s < v ∨ (v = s ∧ tb m t = true)) (j n : ι) :
    Beats kf tb j n ↔ Beats kr tb j n := by
  unfold one at hs htr
  by_cases hj : j = t <;> by_cases hn : n = t
  · subst hj; subst hn; rw [beats_self_iff, beats_self_iff]
  · subst hj
    unfold Beats
    rw [htf, htr, hf n hn, hr n hn]
    cases hkn : kb n with
    | none => simp [OLt, OLe]
    | some v =>
      rcases hrel n v hkn with h | ⟨h, htn⟩
      · simp [OLt, OLe]; omega
      · simp [OLt, OLe]
        constructor
        · rintro (h' | ⟨_, h'⟩)
          · omega
          · rw [htb.asymm htn] at h'; cases h'
        · omega
  · subst hn
    unfold Beats
    rw [htf, htr, hf j hj, hr j hj]
    cases hkj : kb j with
    | none => simp [OLt, OLe]
    | some v =>
      rcases hrel j v hkj with h | ⟨h, htj⟩
      · simp [OLt, OLe]; omega
      · simp [OLt, OLe, htj]; omega
  · unfold Beats
    rw [hf j hj, hr j hj, hf n hn, hr n hn]

/-- **The review fix moves no keyword rank.** The full-list step and the
capped raise give every node the same keyword rank, the named concept
included, when the ranked list is the sorted search. Within the cap or with no
hit the tables are equal (`full_eq_raise`). Beyond the cap the named concept is
in the leg under both, at `1.0` under `raiseName` and at `max(BM25, 1.0)`
under the full step, and every in-cap member sorts ahead of it at either value:
at least its BM25 by the search order, and above `1.0` whenever the two
differ. So under RRF the review fix changes no `rrf` (`full_rrf_eq_raise`). -/
theorem full_rank_eq_raise (inp : Input ι) {t : ι} (ht : inp.nameTarget = some t)
    {tb : ι → ι → Bool} (htb : StrictTotal tb) (hnd : (inp.ranked.map Prod.fst).Nodup)
    (hsort : SortedBy tb inp.ranked) (ids : List ι) (n : ι) :
    legRank (kwSlots (phase1 (raiseFullName inp.ranked) inp)) tb ids n =
      legRank (kwSlots (phase1 raiseName inp)) tb ids n := by
  have heq_tab : ∀ (h : phase1 (raiseFullName inp.ranked) inp = phase1 raiseName inp),
      legRank (kwSlots (phase1 (raiseFullName inp.ranked) inp)) tb ids n =
        legRank (kwSlots (phase1 raiseName inp)) tb ids n := fun h => by rw [h]
  cases hf : findScore inp.ranked t with
  | none =>
    exact heq_tab (full_eq_raise inp ht hnd (Or.inl (not_mem_of_findScore hf)))
  | some s =>
    cases hkb : keywordLeg inp.ranked inp.cap t with
    | some s' =>
      exact heq_tab (full_eq_raise inp ht hnd (Or.inr ⟨s', mem_of_lastScore hkb⟩))
    | none =>
      have hkf := full_keyword inp ht t
      rw [ite_eq_left rfl, phase1_noName_keyword, hkb, hf] at hkf
      simp only [mergeHit, mergeMax] at hkf
      have hkr := raise_keyword inp ht t
      rw [ite_eq_left rfl, phase1_noName_keyword, hkb] at hkr
      simp only [mergeMax] at hkr
      have hloc_f : ∀ m, m ≠ t → kwSlots (phase1 (raiseFullName inp.ranked) inp) m =
          keywordLeg inp.ranked inp.cap m := fun m hm => by
        simp only [kwSlots]; rw [full_local inp ht hm, phase1_noName_keyword]
      have hloc_r : ∀ m, m ≠ t → kwSlots (phase1 raiseName inp) m =
          keywordLeg inp.ranked inp.cap m := fun m hm => by
        simp only [kwSlots]; rw [raise_local inp ht hm, phase1_noName_keyword]
      by_cases hs : s ≤ one
      · have : kwSlots (phase1 (raiseFullName inp.ranked) inp) =
            kwSlots (phase1 raiseName inp) := by
          funext m
          by_cases hm : m = t
          · subst hm; simp only [kwSlots]; rw [hkf, hkr]; simp [imax, hs]
          · rw [hloc_f m hm, hloc_r m hm]
        rw [this]
      · have hs' : one < s := by omega
        have htf : kwSlots (phase1 (raiseFullName inp.ranked) inp) t = some s := by
          simp only [kwSlots]; rw [hkf]; simp [imax]; omega
        have htr : kwSlots (phase1 raiseName inp) t = some one := by
          simp only [kwSlots]; rw [hkr]
        have hb := beats_floor_eq (kb := keywordLeg inp.ranked inp.cap) htb hs' htf htr hkb
          hloc_f hloc_r
          (fun m v hm => in_cap_ahead_of_beyond hsort hm hkb (mem_of_findScore hf))
        unfold legRank ahead
        have hsome : (kwSlots (phase1 (raiseFullName inp.ranked) inp) n).isSome =
            (kwSlots (phase1 raiseName inp) n).isSome := by
          by_cases hm : n = t
          · subst hm; rw [htf, htr]; rfl
          · rw [hloc_f n hm, hloc_r n hm]
        have hfilt : (ids.filter fun j =>
              decide (Beats (kwSlots (phase1 (raiseFullName inp.ranked) inp)) tb j n)) =
            ids.filter fun j => decide (Beats (kwSlots (phase1 raiseName inp)) tb j n) := by
          apply List.filter_congr
          intro x _
          simp only [hb x n]
        rw [hfilt]
        cases h1 : kwSlots (phase1 (raiseFullName inp.ranked) inp) n <;>
          cases h2 : kwSlots (phase1 raiseName inp) n <;> simp_all

/-- And so the two steps give every node the same `rrf` and the same RRF
position. -/
theorem full_rrf_eq_raise (inp : Input ι) {t : ι} (ht : inp.nameTarget = some t)
    (R : RrfRecall ι) (htb : StrictTotal R.tb) (hnd : (inp.ranked.map Prod.fst).Nodup)
    (hsort : SortedBy R.tb inp.ranked) :
    R.key (phase1 (raiseFullName inp.ranked) inp) = R.key (phase1 raiseName inp) ∧
    R.rrf (phase1 (raiseFullName inp.ranked) inp) = R.rrf (phase1 raiseName inp) := by
  have hv : ∀ m, (phase1 (raiseFullName inp.ranked) inp m).vector =
      (phase1 raiseName inp m).vector := fun m => by
    rw [phase1_eq (raiseFullName_keywordOnly _), phase1_eq raiseName_keywordOnly]
  have hranks : ∀ m, R.ranks (phase1 (raiseFullName inp.ranked) inp) m =
      R.ranks (phase1 raiseName inp) m := fun m => by
    simp only [RrfRecall.ranks]
    rw [show (fun x => (phase1 (raiseFullName inp.ranked) inp x).keyword) =
        kwSlots (phase1 (raiseFullName inp.ranked) inp) from rfl,
      show (fun x => (phase1 raiseName inp x).keyword) = kwSlots (phase1 raiseName inp) from rfl,
      full_rank_eq_raise inp ht htb hnd hsort]
    simp only [hv]
  have hkS : ∀ m, ((phase1 (raiseFullName inp.ranked) inp m).keyword).isSome =
      ((phase1 raiseName inp m).keyword).isSome := fun m => by
    by_cases hm : m = t
    · subst hm
      have isSome_of : ∀ {x : Option Int}, OLe (some one) x → x.isSome = true := by
        intro x hx; cases x <;> simp_all [OLe]
      rw [isSome_of (full_keyword_ge_one inp ht), isSome_of (raise_keyword_ge_one inp ht)]
    · rw [full_local inp ht hm, raise_local inp ht hm]
  have hbest : R.best (phase1 (raiseFullName inp.ranked) inp) = R.best (phase1 raiseName inp) := by
    simp only [RrfRecall.best, RrfRecall.kActive, RrfRecall.vActive, hkS, hv]
  have hnum : ∀ m, R.num (phase1 (raiseFullName inp.ranked) inp) m =
      R.num (phase1 raiseName inp) m := fun m => by
    simp only [RrfRecall.num, hranks]
  constructor
  · funext m; simp only [RrfRecall.key, hbest, hnum]
  · funext m; simp only [RrfRecall.rrf, hbest, hnum]

/-! ### Examples of the name step under RRF -/

/-- Both nodes within the cap: `Y` (node 1) has BM25 `0.9`, the named `X`
(node 0) `0.4`. The floor lifts `X` to `1.0`, past `Y`. -/
def floorInput : Input Nat where
  ranked := [(1, 900), (0, 400)]
  cap := 8
  nameTarget := some 0
  recent := fun _ => false
  recentScore := 350
  vector := []

/-- A recall over the given candidates at the defaults, no recent leg. -/
def nameRecall (ids : List Nat) : RrfRecall Nat :=
  { cfg := rrfDefault, tb := idOrder, ids := ids, keywordRan := true, recentIdx := fun _ => none }

/-- **Other nodes' ranks do shift**: `Y` drops from keyword rank 1 to 2 and
its `rrf` from `1` to `4/5` (at `k = 3`), while `X` rises from `4/5` to `1`.
Under max fusion `Y`'s score is untouched (`full_local`). -/
theorem rrf_name_step_shifts_neighbour :
    ((nameRecall [0, 1]).ranks (phase1 noName floorInput) 1).keyword = some 1 ∧
    ((nameRecall [0, 1]).ranks (phase1 (raiseFullName floorInput.ranked) floorInput) 1).keyword =
      some 2 ∧
    ((nameRecall [0, 1]).ranks (phase1 noName floorInput) 0).keyword = some 2 ∧
    ((nameRecall [0, 1]).ranks (phase1 (raiseFullName floorInput.ranked) floorInput) 0).keyword =
      some 1 ∧
    Passes (kwSlots (phase1 noName floorInput))
      (kwSlots (phase1 (raiseFullName floorInput.ranked) floorInput)) idOrder [0, 1] 0 1 ∧
    REq ((nameRecall [0, 1]).rrf (phase1 noName floorInput) 1) (1, 1) ∧
    REq ((nameRecall [0, 1]).rrf (phase1 (raiseFullName floorInput.ranked) floorInput) 1) (4, 5) ∧
    REq ((nameRecall [0, 1]).rrf (phase1 noName floorInput) 0) (4, 5) ∧
    REq ((nameRecall [0, 1]).rrf (phase1 (raiseFullName floorInput.ranked) floorInput) 0) (1, 1) ∧
    score noName floorInput 1 = score (raiseFullName floorInput.ranked) floorInput 1 := by
  decide

/-- Above the floor, nothing moves: on #288's example (`X` named with BM25
`3.2`, `Y` `2.0`) the step keeps `X` at `3.2`, so no keyword rank and no
`rrf` changes (no pass, `full_passes_only_below_floor`). -/
theorem rrf_name_step_above_floor :
    let R := nameRecall [0, 1]
    let F := phase1 (raiseFullName bugInput.ranked) bugInput
    let B := phase1 noName bugInput
    R.ranks F 0 = R.ranks B 0 ∧ R.ranks F 1 = R.ranks B 1 ∧
    R.rrf F 0 = R.rrf B 0 ∧ R.rrf F 1 = R.rrf B 1 := by
  decide

/-- Beyond the cap, below the floor: the cap keeps only `Y` (node 1, BM25
`0.8`); the named `X` (node 0, `0.5`) lies beyond it. Before the step `X` is
not in the keyword leg; the step puts it there at `1.0`, ahead of `Y`, whose
keyword rank goes from 1 to 2. So "the named concept was already in the leg"
is false against the table before the step, and a beyond-cap name step does
move another node's rank. `raiseName` does the same (the floor is common to
both). -/
def beyondCapLow : Input Nat where
  ranked := [(1, 800), (0, 500)]
  cap := 1
  nameTarget := some 0
  recent := fun _ => false
  recentScore := 350
  vector := []

/-- #288 review: beyond the cap, raising the named concept into the keyword
leg moves its neighbour down one keyword rank in RRF. -/
theorem rrf_beyond_cap_shifts_neighbour :
    ((nameRecall [0, 1]).ranks (phase1 noName beyondCapLow) 0).keyword = none ∧
    ((nameRecall [0, 1]).ranks (phase1 (raiseFullName beyondCapLow.ranked) beyondCapLow) 0).keyword =
      some 1 ∧
    ((nameRecall [0, 1]).ranks (phase1 noName beyondCapLow) 1).keyword = some 1 ∧
    ((nameRecall [0, 1]).ranks (phase1 (raiseFullName beyondCapLow.ranked) beyondCapLow) 1).keyword =
      some 2 ∧
    ((nameRecall [0, 1]).ranks (phase1 raiseName beyondCapLow) 1).keyword = some 2 := by
  decide

/-- Beyond the cap, above the floor (`beyondCapInput3`): `X` (node 0, BM25
`3.2`) joins the leg behind `Z` (node 1, `5.0`) under both steps, at `1.0` or
at `3.2`, so the review fix changes no rank and no `rrf`; `W` (node 2) stays
out; and against the table before the step `Z` keeps rank 1, since `X` passes
nobody. -/
theorem rrf_beyond_cap_example :
    let R := nameRecall [0, 1, 2]
    let F := phase1 (raiseFullName beyondCapInput3.ranked) beyondCapInput3
    let A := phase1 raiseName beyondCapInput3
    let B := phase1 noName beyondCapInput3
    (R.ranks F 0).keyword = some 2 ∧ (R.ranks A 0).keyword = some 2 ∧
    (R.ranks B 0).keyword = none ∧
    (R.ranks F 1).keyword = some 1 ∧ (R.ranks A 1).keyword = some 1 ∧
    (R.ranks B 1).keyword = some 1 ∧
    (R.ranks F 2).keyword = none ∧
    R.rrf F 0 = R.rrf A 0 ∧ R.rrf F 1 = R.rrf A 1 ∧
    (F 0).keyword = some 3200 ∧ (A 0).keyword = some 1000 := by
  decide

/-- **The sortedness hypothesis is needed.** On a ranked list out of
`search_ranked`'s order, `[Z 2.0, X 3.2]` with a cap of 1, the full-list step
puts `X` at `3.2` past `Z`, where `raiseName`'s `1.0` left it behind: a pass
above the floor, and the two steps' ranks differ. -/
def unsortedInput : Input Nat where
  ranked := [(1, 2000), (0, 3200)]
  cap := 1
  nameTarget := some 0
  recent := fun _ => false
  recentScore := 350
  vector := []

/-- Two unnamed nodes, `Y` (node 1) and `Z` (node 2), and the named `X`
(node 0). Keyword BM25: `Z 2.0, Y 0.5, X 0.3`; vector: `Y 0.9, Z 0.8`. -/
def reorderInput : Input Nat where
  ranked := [(2, 2000), (1, 500), (0, 300)]
  cap := 8
  nameTarget := some 0
  recent := fun _ => false
  recentScore := 350
  vector := [(1, 900), (2, 800)]

/-- **The name step can reorder two nodes it does not name.** Before it, `Y`
and `Z` tie under RRF (keyword ranks 2 and 1, vector ranks 1 and 2) and `Y`
wins the tie-break. The floor lifts `X` past `Y` but not past `Z` (BM25 `2.0`),
so `Y` drops a keyword place and `Z` now sorts ahead of it. Under max fusion
neither node's score moves. -/
theorem rrf_name_step_reorders_unnamed :
    let R := nameRecall [0, 1, 2]
    let F := phase1 (raiseFullName reorderInput.ranked) reorderInput
    let B := phase1 noName reorderInput
    Beats (R.key B) idOrder 1 2 ∧ Beats (R.key F) idOrder 2 1 ∧
    score noName reorderInput 1 = score (raiseFullName reorderInput.ranked) reorderInput 1 ∧
    score noName reorderInput 2 = score (raiseFullName reorderInput.ranked) reorderInput 2 := by
  decide

end Lambo.Fusion
