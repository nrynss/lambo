/-
# Phase-1 leg fusion and the exact-name step (#288)

Model of how phase 1 builds each candidate's leg scores and fuses them, in
`candidates_with_keyword_leg_as` (`src/recall/candidates.rs`):

* the keyword leg: the one `search_ranked` result, cut to the over-fetch cap
  (`limit * KEYWORD_OVERFETCH`), each hit's BM25 assigned to its node;
* the exact-name step (below);
* the recent leg: its members get the flat recent score;
* the vector leg: each candidate folded in with `merge_max`;
* `LegScores::merged`, the value phase 1 ranks on under `FusionKind::Max`, the
  default (`LegScores::fused`, `src/recall/candidates.rs`);
* the order `rank` sorts by: the fused value descending, then a tie-break
  (`tie_break_by_key`).

The exact-name step before #288 **assigned** the keyword leg:

```rust
if let Some(id) = graph.name_target(query)
    && matches!(graph.node(id), Some(Node::Concept(_)))
{
    legs.entry(id).or_default().keyword = Some(1.0);
    exact_name = true;
}
```

and #288's first fix made it **raise** the leg to at least `1.0`:

```rust
    merge_max(&mut legs.entry(id).or_default().keyword, 1.0);
```

That reads only the slot the capped keyword loop left, so a named concept
whose hit lies beyond the over-fetch cap got the bare `1.0`
(`beyond_cap_floor_only`). The step after the #288 review also folds in the
named concept's own BM25 from the whole ranked list, already computed, before
the floor:

```rust
    let slot = &mut legs.entry(id).or_default().keyword;
    if let Some(hit) = ranked.iter().find(|s| s.item == id) {
        merge_max(slot, hit.score);
    }
    merge_max(slot, 1.0);
```

All three are modelled (`assignName`, `raiseName`, `raiseFullName`), beside
`noName`, the table as it stands before the step, so the theorems can compare
them. `raiseFullName` is the step the Rust runs now; the `raise_*` theorems
are about the intermediate one and stay as they were.

## Number model

Exact arithmetic in fixed point, as in `Score.lean`: a score `x` is the
integer `1000 · x`, so `one = 1000` is `1.0` and `3200` is a BM25 of `3.2`.
Scores are integers, not naturals, because a cosine can be negative. A leg
slot is `Option Int`: `none` is a leg that did not produce the node. In
`merged`, `none` plays the part of the fold's `f64::NEG_INFINITY` start, so it
orders below every score (`OLe`). `f64::max` on two finite values is `imax`.

Not captured: `f64` rounding and non-finite values. BM25 from `search_ranked`
is finite and strictly positive (it keeps only scores `> 0`); a non-finite
vector cosine is possible in Rust, where `f64::max` drops a `NaN`, and is
outside this model.

## Model

* A leg table is a function `ι → Legs`. A node absent from the Rust
  `HashMap` is the all-`none` `Legs`; `rank` only sorts the map's entries, and
  every entry has at least one leg.
* `Input.nameTarget` is `graph.name_target(query)` already filtered to
  concepts. Name normalization and lookup are outside this file
  (`Names.lean` covers the registry).
* `Input.recent` is membership of the recent leg (the concepts of the three
  most recent interactions). Its RRF ranks are computed from
  `RrfRecall.recentIdx` (below), which a caller must keep consistent with
  `Input.recent`; nothing ties the two mechanically.
* The search yields each id once (it scores through a `HashMap`), so the
  ranked list's ids are distinct; the keyword step is modelled as the Rust
  loop, last assignment wins (`lastScore`), and the theorems that read a BM25
  off the list assume distinct ids.

Left out: the trace instrumentation, and the keywordless path
(`candidates_without_keyword`, which has no name step; its RRF is the same
`rank` with `keyword_ran = false`, which `RrfRecall.keywordRan` covers). `KeywordLeg` and
`top_legs` (#256) are read off the ranked list, which no step here changes,
so the name step cannot move them; the model does not restate that.

## Results

The sentences they correspond to: the module doc of `candidates.rs`, "a node
present in more than one leg keeps its HIGHEST score (max-merge)" (`merged`),
and the comment above the name step, "An exact normalized name is direct
identity evidence even when the target is old and its content has no words in
common with the query", which reads as evidence added for the named concept,
never taken away (`raise_*`, `full_*`). The Rust test
`an_exact_name_raises_the_keyword_leg_without_lowering_its_bm25`
(`candidates.rs`) pins the fixed step on #288's shape: the named concept's
keyword leg is its BM25, the neighbour's is untouched, and the named concept
ranks first (`raise_keyword_eq`, `raise_keyword_ge_bm25`, `raise_local`,
`raise_keeps_wins`, and `full_eq_raise` for the current step, the hit being
within the cap); on the unfixed code it failed with the leg at `1.0`
(`assign_lowers_named_concept`). The Rust test
`an_exact_name_keeps_its_bm25_beyond_the_overfetch_cap` pins the step after
review: a named concept whose hit lies beyond the over-fetch cap keeps its
BM25, the in-cap legs are untouched, and an unnamed neighbour beyond the cap
stays out of the leg (`full_keyword_of_mem`, `full_keyword_ge_bm25`,
`full_local`, `full_keeps_beyond_cap_bm25`); under `raiseName` it failed with
the leg at `1.0` (`beyond_cap_floor_only`).

* `merged_mono`, `merged_mono_keyword`: max fusion is monotone in each leg.
* `assign_lowers_named_concept`: **the bug.** Ranked BM25 `[X 3.2, Y 2.0]`
  with `X` named: before the name step `X` beats `Y`; the assigning step sets
  `X`'s keyword leg to `1.0`, below its own BM25, and `Y` now beats `X` (the
  example in #288).
* `raise_keyword_ge_one`, `raise_keyword_ge_leg`, `raise_keyword_ge_bm25`:
  after the raising step the named concept's keyword leg is at least `1.0`,
  at least the leg it had before the step, and at least its BM25 when its hit
  is within the over-fetch cap. `raise_keyword_eq` gives the exact value:
  `max(BM25, 1.0)`, or `1.0` with no keyword hit.
* `raise_local`: every other node's legs are unchanged by the step.
* `raise_no_demotion`, `raise_keeps_wins`, `raise_never_overtaken`,
  `raise_ahead_le`: the named concept's fused score does not fall, it keeps
  every pairwise win it had, no node that it beat comes to beat it, and the
  number of nodes ranked ahead of it cannot grow.
* `raise_eq_assign`: the raising step agrees with the assigning one, on every
  node, whenever the named concept's keyword leg was at most `1.0` or empty.
  `raise_ne_assign`: and only then.
* `beyond_cap_floor_only`: a named concept whose BM25 hit lies beyond the
  over-fetch cap is not in the keyword leg, so even the raising step scores it
  `1.0`, below its BM25. "At least its BM25" holds for hits within the cap.

The step after review, `raiseFullName`:

* `full_keyword_ge_one`, `full_keyword_ge_leg`, `full_keyword_ge_bm25`: the
  named concept's keyword leg is at least `1.0`, at least the leg it had, and
  at least its BM25 for **any** hit in the ranked list, with no over-fetch-cap
  hypothesis. `full_keyword_eq` gives the exact value, `max(BM25, 1.0)` from
  the whole list, or `1.0` without a hit (`full_keyword_of_mem`,
  `full_keyword_of_not_mem`).
* `full_local`: every other node's legs are unchanged; with
  `lastScore_of_not_mem` (under `Nodup`), an unnamed node beyond the cap stays
  out of the keyword leg, and `full_keeps_beyond_cap_bm25` shows it on a
  concrete input.
* `full_no_demotion`, `full_keeps_wins`, `full_never_overtaken`,
  `full_ahead_le`: as for `raiseName`, against the table before the step.
* `full_keyword_ge_raise`, `full_ahead_le_raise`: against `raiseName`, the
  keyword leg is never lower and nothing more sorts ahead of the named
  concept.
* `full_eq_raise`: the two steps give the same table whenever the named
  concept's hit is within the cap or it has none; they differ only beyond the
  cap. `full_keeps_beyond_cap_bm25`: on `beyondCapInput` and a three-node
  variant, the full-list step keeps the BM25 `3.2` where `raiseName` gave
  `1.0`, and an unnamed beyond-cap node stays out of the leg.

The name step writes only leg slots. In Rust, `rank` then recomputes every
node's `keyword_rank` and `rrf`, so other nodes' RRF fields can move:
"locality" above means the leg slots, and the fused value under max fusion.
The RRF section below states exactly how far the step reaches under RRF.

## Reciprocal rank fusion (#173)

Modelled: `FusionKind::Rrf` and `RecallFusion` (`src/recall/fusion.rs`:
`k`, the three weights, `RecallFusion::term`, and `validate`'s finite
non-negative `k`), and in `src/recall/candidates.rs` the RRF half of `rank`
(`leg_ranks` for the keyword and vector legs, `best` over the legs that ran,
`l.rrf = sum / best` or `0`), `recent_concepts`' rank, and `LegScores::fused`
under `Rrf` (`rrf.unwrap_or(0.0)`; every entry gets `Some`).

* A leg rank (`legRank`) is one more than the number of the leg's members that
  sort ahead by `Beats`: score descending, then the tie-break `tb`. With a
  strict total `tb` (`StrictTotal`; `Order.lean` shows `tie_break_by_key` is
  one) that is the member's 1-based position in `leg_ranks`' sorted list, which
  `legRank_lt_iff`, `legRank_injective` and `legRank_le_members` pin down.
* The recent rank (`recentRank`) is one more than the number of members of
  newer interactions, from the index of the owning interaction.
* `RrfRecall` holds the configuration, the candidates (the `LegProvenance`
  keys), `keyword_ran` and the recent indices. `RrfRecall.rrf` is the exact
  value, `RrfRecall.key` the value `rank` sorts on, and `RrfRecall.rrfAhead` a
  node's position in `rank`'s output under RRF.

### Number model for RRF

Exact rationals, as a numerator/denominator pair of naturals with no rounding
anywhere: `k = kNum / kDen` (`kDen ≥ 1`, `RrfConfig.Valid`), weights are
naturals at one common scale (`1.0 = 10`, `0.3 = 3`; the value is a ratio, so
a common scale cancels, as Rust's own division by the largest weight relies
on). A term `w / (k + r)` is `w · kDen / (kNum + kDen · r)`; every term is
scaled by `M = prodDen`, the product of `kNum + kDen · r` for `r = 1 … N` over
`N` candidates, so each scaled term is an exact quotient (`term_exact`), and
`rrf` is the exact fraction of the two scaled sums, `(num, best)`, the factor
`M / kDen` common to both cancelling (`RLe`, `REq` compare fractions by
cross-multiplication).

Not captured: `f64` rounding (`term` and the two sums round in Rust; the range
`[0, 1]` and "rank 1 everywhere is exactly 1" survive it, since the sums are
taken in the same order and rounding is monotone; but two nodes whose exact
values tie through different rank vectors over three legs may differ by a
rounding in Rust, as `f64` addition is not associative, and the rounding then
decides their order instead of the tie-break), and non-finite leg scores,
which earn no rank in Rust (`a_non_finite_leg_score_earns_no_rrf_rank`); every
score here is finite. The search's tie-break (`search_ranked`, keys from the
index) and `rank`'s (keys from the graph) are assumed to be the same `tb`,
which holds while the index mirrors the graph.

### RRF results

* `rrf_range`: every candidate's `rrf` is in `[0, 1]` (the doc of
  `LegScores::rrf`). `num_eq_best_of_rank_one`: rank 1 in every leg that ran
  is exactly 1 (`rrf_is_one_for_rank_one_in_every_active_leg_and_ignores_empty_legs`).
* `rrfSum_mono`: monotone in each leg's rank; a better rank never lowers it.
* `term_none`, `rrf_recent_example`: a leg a node is not in contributes 0, and
  still counts in `best` when it ran (`rrf_counts_a_keyword_leg_that_ran_empty_but_not_one_that_did_not_run`).
* `legRank_lt_iff`, `legRank_injective`, `legRank_le_members`,
  `legRank_congr`, `legRank_perm`, `recentRank_perm`, `rrf_perm`: ranks follow
  the leg's own scores and the tie-break, run from 1 to the leg's size, and do
  not depend on the order the candidates arrive in (`legRank_example`).
* `rrf_one_leg_agrees`: with one leg the only one with members and its weight
  above 0, RRF sorts its members exactly as the leg does.
  `rrf_one_leg_needs_weight`: with that leg's weight 0, which `validate`
  admits beside another positive weight, it does not.
* `rrf_two_leg_example`, `rrf_tie_example`: the arithmetic of
  `rrf_ranks_two_leg_agreement_above_one_leg_with_a_bigger_raw_score` and
  `rrf_ties_break_by_canonical_key_and_a_zero_weight_leg_scores_zero`.

The exact-name step under RRF (`raiseFullName` against the table before it):

* `rrf_full_rank_named`, `rrf_full_named_up`, `rrf_full_ahead_le`: the named
  concept's keyword rank never gets worse (it may join the leg), its `rrf`
  never falls, and no more nodes sort ahead of it under RRF.
* `rrf_full_rank_other`, `rrf_full_other`: every other node's keyword rank
  moves by at most one place, down, exactly when the named concept passes it
  (`Passes`), and then its keyword term is that of the next rank; its vector
  and recent ranks never move (`rrf_full_rank_vector_recent`), so its `rrf`
  falls by exactly that keyword term's drop or not at all. `best` is unchanged
  (`rrf_full_best`, given `keyword_ran`, which a name match implies).
* `full_passes_only_below_floor`: with the ranked list sorted as
  `search_ranked` sorts it, a pass happens only from the `1.0` floor: the
  named concept's slot before the step was below `1.0` or empty, and the node
  passed scores at most `1.0`. `rrf_name_step_above_floor`: on #288's example
  nothing moves.
* `full_rank_eq_raise`, `full_rrf_eq_raise`: the review fix (reading the
  BM25 beyond the cap) changes no node's keyword rank and no `rrf` against
  `raiseName`. Against the table before the step it can:
  `rrf_beyond_cap_shifts_neighbour`. `full_rank_eq_raise` needs the ranked
  list in search order (`full_rank_eq_raise_needs_sorted`).
* `rrf_name_step_shifts_neighbour`, `rrf_name_step_reorders_unnamed`: under
  RRF the step lowers a neighbour's `rrf` and can swap two nodes it does not
  name, where max fusion moves neither.
-/

namespace Lambo.Fusion

/-! ## Scores -/

/-- `1.0` at scale 1000. -/
def one : Int := 1000

/-- `f64::max` on finite values. -/
def imax (a b : Int) : Int := if a ≤ b then b else a

/-- `a ≤ b` on optional scores, `none` below everything (the fold's
`f64::NEG_INFINITY`, or a leg that produced nothing). -/
def OLe : Option Int → Option Int → Prop
  | none, _ => True
  | some _, none => False
  | some a, some b => a ≤ b

instance instDecidableOLe : (a b : Option Int) → Decidable (OLe a b)
  | none, _ => isTrue trivial
  | some _, none => isFalse (fun h => h)
  | some a, some b => inferInstanceAs (Decidable (a ≤ b))

/-- Strictly below. -/
def OLt (a b : Option Int) : Prop := ¬ OLe b a

instance (a b : Option Int) : Decidable (OLt a b) :=
  inferInstanceAs (Decidable (¬ OLe b a))

/-- One step of `merged`'s `fold(f64::NEG_INFINITY, f64::max)` over present
legs. -/
def omax : Option Int → Option Int → Option Int
  | none, b => b
  | some a, none => some a
  | some a, some b => some (imax a b)

/-- `merge_max(slot, score)` (`candidates.rs`). -/
def mergeMax (slot : Option Int) (score : Int) : Option Int :=
  match slot with
  | some previous => some (imax previous score)
  | none => some score

theorem oLe_refl (a : Option Int) : OLe a a := by
  cases a <;> simp [OLe]

theorem oLe_trans {a b c : Option Int} (h1 : OLe a b) (h2 : OLe b c) : OLe a c := by
  cases a <;> cases b <;> cases c <;> simp_all [OLe] <;> omega

theorem oLt_of_le_of_lt {a b c : Option Int} (h1 : OLe a b) (h2 : OLt b c) : OLt a c := by
  cases a <;> cases b <;> cases c <;> simp_all [OLt, OLe] <;> omega

theorem oLt_of_lt_of_le {a b c : Option Int} (h1 : OLt a b) (h2 : OLe b c) : OLt a c := by
  cases a <;> cases b <;> cases c <;> simp_all [OLt, OLe] <;> omega

theorem oLt_irrefl (a : Option Int) : ¬ OLt a a := by
  cases a <;> simp [OLt, OLe]

/-- A score that does not fall and moves at all rises strictly. -/
theorem oLt_of_le_of_ne {a b : Option Int} (h : OLe a b) (hne : a ≠ b) : OLt a b := by
  cases a <;> cases b <;> simp_all [OLt, OLe] <;> omega

theorem omax_mono {a a' b b' : Option Int} (ha : OLe a a') (hb : OLe b b') :
    OLe (omax a b) (omax a' b') := by
  cases a <;> cases a' <;> cases b <;> cases b' <;> simp_all [OLe, omax, imax] <;>
    (try split) <;> (try split) <;> omega

theorem mergeMax_eq (slot : Option Int) (score : Int) :
    mergeMax slot score = omax slot (some score) := by
  cases slot <;> rfl

/-- `merge_max` never lowers the slot. -/
theorem le_mergeMax (slot : Option Int) (score : Int) : OLe slot (mergeMax slot score) := by
  cases slot <;> simp [OLe, mergeMax, imax] <;> split <;> omega

/-- `merge_max` leaves the slot at least the incoming score. -/
theorem score_le_mergeMax (slot : Option Int) (score : Int) :
    OLe (some score) (mergeMax slot score) := by
  cases slot <;> simp [OLe, mergeMax, imax] <;> split <;> omega

/-! ## Legs and max fusion -/

/-- `LegScores`, without the RRF ranks and value. -/
@[ext] structure Legs where
  keyword : Option Int
  recent : Option Int
  vector : Option Int
deriving DecidableEq, Repr

/-- `LegScores::merged`: the highest present leg, `none` (negative infinity)
when no leg produced the node. -/
def Legs.merged (l : Legs) : Option Int :=
  omax (omax l.keyword l.recent) l.vector

/-- **Max fusion is monotone** in every leg at once. -/
theorem merged_mono {l l' : Legs} (hk : OLe l.keyword l'.keyword)
    (hr : OLe l.recent l'.recent) (hv : OLe l.vector l'.vector) :
    OLe l.merged l'.merged :=
  omax_mono (omax_mono hk hr) hv

/-- Max fusion is monotone in the keyword leg alone. -/
theorem merged_mono_keyword (l : Legs) {k : Option Int} (hk : OLe l.keyword k) :
    OLe l.merged { l with keyword := k }.merged :=
  merged_mono hk (oLe_refl _) (oLe_refl _)

/-! ## Ranking -/

variable {ι : Type}

/-- `a` sorts ahead of `b` in `rank`: a higher fused value, or an equal one and
`a` first by the tie-break (`tb`, standing for `tie_break_by_key`, which
`Order.lean` shows is a strict total order). -/
def Beats (sc : ι → Option Int) (tb : ι → ι → Bool) (a b : ι) : Prop :=
  OLt (sc b) (sc a) ∨ (sc a = sc b ∧ tb a b = true)

instance (sc : ι → Option Int) (tb : ι → ι → Bool) (a b : ι) :
    Decidable (Beats sc tb a b) := by
  unfold Beats; infer_instance

/-- How many of `ids` sort ahead of `t`: its 0-based position in `rank`'s
output when `ids` are the candidates (the comparator being a strict total
order). -/
def ahead (sc : ι → Option Int) (tb : ι → ι → Bool) (ids : List ι) (t : ι) : Nat :=
  (ids.filter fun j => decide (Beats sc tb j t)).length

/-- Raising one node's score and leaving the rest keeps its wins. -/
theorem beats_of_raise {sc sc' : ι → Option Int} {tb : ι → ι → Bool} {t j : ι}
    (hloc : ∀ n, n ≠ t → sc' n = sc n) (hup : OLe (sc t) (sc' t)) (hj : j ≠ t)
    (h : Beats sc tb t j) : Beats sc' tb t j := by
  unfold Beats at *
  rw [hloc j hj]
  rcases h with h | ⟨heq, htb⟩
  · exact Or.inl (oLt_of_lt_of_le h hup)
  · by_cases hs : sc t = sc' t
    · exact Or.inr ⟨hs ▸ heq, htb⟩
    · exact Or.inl (heq ▸ oLt_of_le_of_ne hup hs)

/-- Raising one node's score and leaving the rest lets nobody overtake it. -/
theorem beats_of_raise_rev {sc sc' : ι → Option Int} {tb : ι → ι → Bool} {t j : ι}
    (hloc : ∀ n, n ≠ t → sc' n = sc n) (hup : OLe (sc t) (sc' t))
    (h : Beats sc' tb j t) : Beats sc tb j t := by
  by_cases hj : j = t
  · subst hj
    unfold Beats at *
    rcases h with h | ⟨_, htb⟩
    · exact absurd h (oLt_irrefl _)
    · exact Or.inr ⟨rfl, htb⟩
  · unfold Beats at *
    rw [hloc j hj] at h
    rcases h with h | ⟨heq, htb⟩
    · exact Or.inl (oLt_of_le_of_lt hup h)
    · by_cases hs : sc j = sc t
      · exact Or.inr ⟨hs, htb⟩
      · exact Or.inl (oLt_of_le_of_ne (heq ▸ hup) (Ne.symm hs))

theorem length_filter_mono {l : List ι} {p q : ι → Bool} (h : ∀ x, p x = true → q x = true) :
    (l.filter p).length ≤ (l.filter q).length := by
  induction l with
  | nil => simp
  | cons x rest ih =>
    simp only [List.filter_cons]
    cases hp : p x <;> cases hq : q x <;> simp_all <;> omega

/-! ## Phase 1 -/

variable [DecidableEq ι]

/-- The keyword loop's result for one node: the score of the last entry for
it, `none` when it has none (`legs.entry(s.item).or_default().keyword =
Some(s.score)` over the list). -/
def lastScore : List (ι × Int) → ι → Option Int
  | [], _ => none
  | (i, s) :: rest, n =>
    match lastScore rest n with
    | some t => some t
    | none => if i = n then some s else none

/-- The keyword leg: the ranked search cut to the over-fetch cap. -/
def keywordLeg (ranked : List (ι × Int)) (cap : Nat) : ι → Option Int :=
  lastScore (ranked.take cap)

/-- A node's score on the whole ranked list, cap or no cap: the first entry
for it (`ranked.iter().find(|s| s.item == id)`), `none` when it has none. -/
def findScore : List (ι × Int) → ι → Option Int
  | [], _ => none
  | (i, s) :: rest, n => if i = n then some s else findScore rest n

/-- Fold an optional hit into a slot with `merge_max`; no hit leaves the slot
as it is (`if let Some(hit) = … { merge_max(slot, hit.score) }`). -/
def mergeHit (slot : Option Int) : Option Int → Option Int
  | none => slot
  | some s => mergeMax slot s

/-- What phase 1 reads. -/
structure Input (ι : Type) where
  /-- `index.search_ranked(query)`: (node, BM25), best first, ids distinct. -/
  ranked : List (ι × Int)
  /-- `limit * KEYWORD_OVERFETCH`. -/
  cap : Nat
  /-- `graph.name_target(query)`, when it names a concept. -/
  nameTarget : Option ι
  /-- Membership of the recent-interactions leg (`RecentLeg::Run`). -/
  recent : ι → Bool
  /-- `Phase1Input::recent_score`. -/
  recentScore : Int
  /-- `Phase1Input::vector`: (node, similarity), as the store returned them. -/
  vector : List (ι × Int)

/-- The table after the keyword loop. -/
def keywordTable (inp : Input ι) : ι → Legs :=
  fun n => { keyword := keywordLeg inp.ranked inp.cap n, recent := none, vector := none }

/-- An exact-name step: the table and the named concept, if any. -/
abbrev NameStep (ι : Type) := (ι → Legs) → Option ι → ι → Legs

/-- No name step: the table as it stands before it. -/
def noName : NameStep ι := fun legs _ => legs

/-- The step before #288: `keyword = Some(1.0)`. -/
def assignName : NameStep ι
  | legs, none => legs
  | legs, some t => fun n => if n = t then { legs n with keyword := some one } else legs n

/-- The first #288 step: `merge_max(keyword, 1.0)` on the slot the capped
keyword loop left, so a hit beyond the over-fetch cap is not seen. -/
def raiseName : NameStep ι
  | legs, none => legs
  | legs, some t => fun n =>
    if n = t then { legs n with keyword := mergeMax (legs n).keyword one } else legs n

/-- The step after the #288 review: fold in the named concept's BM25 from the
whole ranked list, then the `1.0` floor, so a hit beyond the over-fetch cap
counts too:

```rust
let slot = &mut legs.entry(id).or_default().keyword;
if let Some(hit) = ranked.iter().find(|s| s.item == id) {
    merge_max(slot, hit.score);
}
merge_max(slot, 1.0);
```

It reads `ranked` itself, so it takes the list as a parameter. -/
def raiseFullName (ranked : List (ι × Int)) : NameStep ι
  | legs, none => legs
  | legs, some t => fun n =>
    if n = t then
      { legs n with keyword := mergeMax (mergeHit (legs n).keyword (findScore ranked t)) one }
    else legs n

/-- The recent leg: `leg.recent = Some(input.recent_score)` for its members. -/
def recentStep (inp : Input ι) (legs : ι → Legs) : ι → Legs :=
  fun n => if inp.recent n then { legs n with recent := some inp.recentScore } else legs n

/-- The vector leg: `merge_max(&mut legs.entry(s.item).or_default().vector,
s.score)` for each candidate in order. -/
def vectorStep : List (ι × Int) → (ι → Legs) → ι → Legs
  | [], legs => legs
  | (i, s) :: rest, legs =>
    vectorStep rest (fun n =>
      if n = i then { legs n with vector := mergeMax (legs n).vector s } else legs n)

/-- Phase 1's leg table under a given name step. -/
def phase1 (step : NameStep ι) (inp : Input ι) : ι → Legs :=
  vectorStep inp.vector (recentStep inp (step (keywordTable inp) inp.nameTarget))

/-- The value phase 1 ranks a node on under `FusionKind::Max`. -/
def score (step : NameStep ι) (inp : Input ι) (n : ι) : Option Int :=
  (phase1 step inp n).merged

/-- The fused scores of a list of nodes, for running the model against the
Rust on the same input. -/
def scores (step : NameStep ι) (inp : Input ι) (ids : List ι) : List (ι × Option Int) :=
  ids.map fun n => (n, score step inp n)

/-! ## Structure of the pipeline

The name step writes only the keyword slot, and the recent and vector steps
write only their own slots, so a node's final legs are its legs without the
name step with the keyword slot replaced. -/

/-- A name step that touches only keyword slots. -/
def KeywordOnly (step : NameStep ι) : Prop :=
  ∀ legs t n, (step legs t n).recent = (legs n).recent ∧ (step legs t n).vector = (legs n).vector

omit [DecidableEq ι] in
theorem noName_keywordOnly : KeywordOnly (ι := ι) noName := by
  intro legs t n; exact ⟨rfl, rfl⟩

theorem assignName_keywordOnly : KeywordOnly (ι := ι) assignName := by
  intro legs t n
  cases t with
  | none => exact ⟨rfl, rfl⟩
  | some t => simp only [assignName]; split <;> exact ⟨rfl, rfl⟩

theorem raiseName_keywordOnly : KeywordOnly (ι := ι) raiseName := by
  intro legs t n
  cases t with
  | none => exact ⟨rfl, rfl⟩
  | some t => simp only [raiseName]; split <;> exact ⟨rfl, rfl⟩

theorem raiseFullName_keywordOnly (ranked : List (ι × Int)) :
    KeywordOnly (raiseFullName ranked) := by
  intro legs t n
  cases t with
  | none => exact ⟨rfl, rfl⟩
  | some t => simp only [raiseFullName]; split <;> exact ⟨rfl, rfl⟩

/-- The recent and vector steps act slot by slot: on two tables that agree
on recent and vector slots everywhere, they produce tables that differ only in
the keyword slot, where each keeps its own input. -/
theorem vectorStep_slots (vs : List (ι × Int)) :
    ∀ (legs legs' : ι → Legs),
      (∀ m, (legs m).recent = (legs' m).recent ∧ (legs m).vector = (legs' m).vector) →
      ∀ n, vectorStep vs legs n = { vectorStep vs legs' n with keyword := (legs n).keyword } := by
  induction vs with
  | nil =>
    intro legs legs' h n
    obtain ⟨hr, hv⟩ := h n
    ext <;> simp [vectorStep, hr, hv]
  | cons p rest ih =>
    obtain ⟨i, s⟩ := p
    intro legs legs' h n
    let f := fun (L : ι → Legs) (m : ι) =>
      if m = i then { L m with vector := mergeMax (L m).vector s } else L m
    show vectorStep rest (f legs) n = { vectorStep rest (f legs') n with keyword := (legs n).keyword }
    have hf : ∀ m, (f legs m).recent = (f legs' m).recent ∧ (f legs m).vector = (f legs' m).vector := by
      intro m
      obtain ⟨hr, hv⟩ := h m
      by_cases hm : m = i
      · subst hm; simp [f, hr, hv]
      · simp [f, hm, hr, hv]
    rw [ih (f legs) (f legs') hf n]
    by_cases hn : n = i <;> simp [f, hn]

omit [DecidableEq ι] in
theorem recentStep_slots (inp : Input ι) (legs legs' : ι → Legs)
    (h : ∀ m, (legs m).recent = (legs' m).recent ∧ (legs m).vector = (legs' m).vector) :
    ∀ m, (recentStep inp legs m).recent = (recentStep inp legs' m).recent ∧
      (recentStep inp legs m).vector = (recentStep inp legs' m).vector := by
  intro m
  obtain ⟨hr, hv⟩ := h m
  unfold recentStep
  split <;> simp [hr, hv]

omit [DecidableEq ι] in
theorem recentStep_keyword (inp : Input ι) (legs : ι → Legs) (n : ι) :
    (recentStep inp legs n).keyword = (legs n).keyword := by
  unfold recentStep; split <;> rfl

/-- A node's final legs under any keyword-only name step are its legs without
the step, with the keyword slot the step wrote. -/
theorem phase1_eq {step : NameStep ι} (hs : KeywordOnly step) (inp : Input ι) (n : ι) :
    phase1 step inp n =
      { phase1 noName inp n with keyword := (step (keywordTable inp) inp.nameTarget n).keyword } := by
  unfold phase1
  rw [vectorStep_slots inp.vector _ (recentStep inp (noName (keywordTable inp) inp.nameTarget))]
  · rw [recentStep_keyword]
  · apply recentStep_slots
    intro m
    obtain ⟨hr, hv⟩ := hs (keywordTable inp) inp.nameTarget m
    exact ⟨hr, hv⟩

theorem phase1_noName_keyword (inp : Input ι) (n : ι) :
    (phase1 noName inp n).keyword = keywordLeg inp.ranked inp.cap n := by
  have := phase1_eq (noName_keywordOnly) inp n
  rw [this]; rfl

/-! ## The keyword leg reads the BM25 off the list -/

theorem lastScore_of_not_mem {l : List (ι × Int)} {n : ι} (h : n ∉ l.map Prod.fst) :
    lastScore l n = none := by
  induction l with
  | nil => rfl
  | cons p rest ih =>
    obtain ⟨i, s⟩ := p
    simp only [List.map_cons, List.mem_cons, not_or] at h
    simp only [lastScore, ih h.2]
    rw [ite_eq_right (Ne.symm h.1)]

theorem lastScore_of_mem {l : List (ι × Int)} {n : ι} {s : Int}
    (hnd : (l.map Prod.fst).Nodup) (hm : (n, s) ∈ l) : lastScore l n = some s := by
  induction l with
  | nil => simp at hm
  | cons p rest ih =>
    obtain ⟨i, s'⟩ := p
    simp only [List.map_cons, List.nodup_cons] at hnd
    simp only [List.mem_cons, Prod.mk.injEq] at hm
    rcases hm with ⟨rfl, rfl⟩ | hm
    · simp [lastScore, lastScore_of_not_mem hnd.1]
    · simp [lastScore, ih hnd.2 hm]

/-- Whatever the keyword loop wrote for a node is an entry of the list. -/
theorem mem_of_lastScore {l : List (ι × Int)} {n : ι} {s : Int} (h : lastScore l n = some s) :
    (n, s) ∈ l := by
  induction l with
  | nil => simp [lastScore] at h
  | cons p rest ih =>
    obtain ⟨i, s'⟩ := p
    simp only [lastScore] at h
    cases hr : lastScore rest n with
    | some u =>
      rw [hr] at h
      cases h
      exact List.mem_cons_of_mem _ (ih hr)
    | none =>
      rw [hr] at h
      by_cases hi : i = n
      · subst hi; simp at h; subst h; exact List.mem_cons_self
      · simp [hi] at h

theorem findScore_of_not_mem {l : List (ι × Int)} {n : ι} (h : n ∉ l.map Prod.fst) :
    findScore l n = none := by
  induction l with
  | nil => rfl
  | cons p rest ih =>
    obtain ⟨i, s⟩ := p
    simp only [List.map_cons, List.mem_cons, not_or] at h
    simp only [findScore, ih h.2]
    rw [ite_eq_right (Ne.symm h.1)]

theorem findScore_of_mem {l : List (ι × Int)} {n : ι} {s : Int}
    (hnd : (l.map Prod.fst).Nodup) (hm : (n, s) ∈ l) : findScore l n = some s := by
  induction l with
  | nil => simp at hm
  | cons p rest ih =>
    obtain ⟨i, s'⟩ := p
    simp only [List.map_cons, List.nodup_cons] at hnd
    simp only [List.mem_cons, Prod.mk.injEq] at hm
    rcases hm with ⟨rfl, rfl⟩ | hm
    · simp [findScore]
    · have hi : i ≠ n := by
        intro hi; subst hi
        exact hnd.1 (List.mem_map_of_mem (f := Prod.fst) hm)
      simp [findScore, hi, ih hnd.2 hm]

/-- The capped keyword leg, where it has a score for a node, has the same one
the whole list gives (the ids being distinct). -/
theorem keywordLeg_eq_findScore {ranked : List (ι × Int)} {cap : Nat} {n : ι} {s : Int}
    (hnd : (ranked.map Prod.fst).Nodup) (h : keywordLeg ranked cap n = some s) :
    findScore ranked n = some s :=
  findScore_of_mem hnd (List.mem_of_mem_take (mem_of_lastScore h))

theorem le_mergeHit (slot h : Option Int) : OLe slot (mergeHit slot h) := by
  cases h with
  | none => exact oLe_refl _
  | some s => exact le_mergeMax _ _

theorem mergeMax_mono {a b : Option Int} (h : OLe a b) (x : Int) :
    OLe (mergeMax a x) (mergeMax b x) := by
  rw [mergeMax_eq, mergeMax_eq]; exact omax_mono h (oLe_refl _)

/-- Folding a slot's own score back in changes nothing. -/
theorem mergeHit_self (slot : Option Int) : mergeHit slot slot = slot := by
  cases slot with
  | none => rfl
  | some s => simp [mergeHit, mergeMax, imax]

/-! ## The bug: the assigning step can demote the named concept -/

/-- #288's example: `X` (node 0) is named by the query and has BM25 `3.2`,
`Y` (node 1) has BM25 `2.0`. No recent or vector leg. -/
def bugInput : Input Nat where
  ranked := [(0, 3200), (1, 2000)]
  cap := 8
  nameTarget := some 0
  recent := fun _ => false
  recentScore := 350
  vector := []

/-- The tie-break of the example: node id ascending. -/
def idOrder (a b : Nat) : Bool := decide (a < b)

/-- **The bug.** Before the name step `X` beats `Y`. The assigning step gives
`X` a keyword leg of `1.0`, below its own BM25 of `3.2`, and `Y` then beats
`X` under max fusion. -/
theorem assign_lowers_named_concept :
    (phase1 noName bugInput 0).keyword = some 3200 ∧
    (phase1 assignName bugInput 0).keyword = some 1000 ∧
    OLt (phase1 assignName bugInput 0).keyword (phase1 noName bugInput 0).keyword ∧
    Beats (score noName bugInput) idOrder 0 1 ∧
    Beats (score assignName bugInput) idOrder 1 0 ∧
    ahead (score noName bugInput) idOrder [0, 1] 0 = 0 ∧
    ahead (score assignName bugInput) idOrder [0, 1] 0 = 1 := by
  decide

/-- The same input under the raising step: `X` keeps `3.2` and stays ahead. -/
theorem raise_fixes_example :
    (phase1 raiseName bugInput 0).keyword = some 3200 ∧
    Beats (score raiseName bugInput) idOrder 0 1 ∧
    ahead (score raiseName bugInput) idOrder [0, 1] 0 = 0 := by
  decide

/-- Under the assigning step the named concept's keyword leg is exactly
`1.0`, whatever its BM25 (the clause #288 changes). -/
theorem assign_keyword_eq (inp : Input ι) {t : ι} (ht : inp.nameTarget = some t) :
    (phase1 assignName inp t).keyword = some one := by
  rw [phase1_eq assignName_keywordOnly, ht]
  simp [assignName]

/-! ## The fix -/

section Fixed

variable (inp : Input ι) {t : ι} (ht : inp.nameTarget = some t)
include ht

/-- The named concept's keyword leg after the raising step, from its leg
before it. -/
theorem raise_keyword (n : ι) :
    (phase1 raiseName inp n).keyword =
      if n = t then mergeMax (phase1 noName inp n).keyword one
      else (phase1 noName inp n).keyword := by
  rw [phase1_eq raiseName_keywordOnly, ht]
  by_cases hn : n = t <;> simp [raiseName, hn, phase1_noName_keyword, keywordTable]

/-- The exact value: `max(BM25, 1.0)`, or `1.0` with no keyword hit. -/
theorem raise_keyword_eq :
    (phase1 raiseName inp t).keyword =
      match keywordLeg inp.ranked inp.cap t with
      | some s => some (imax s one)
      | none => some one := by
  rw [raise_keyword inp ht, ite_eq_left rfl, phase1_noName_keyword]
  cases keywordLeg inp.ranked inp.cap t <;> rfl

/-- **Floor.** The named concept's keyword leg is at least `1.0`. -/
theorem raise_keyword_ge_one : OLe (some one) (phase1 raiseName inp t).keyword := by
  rw [raise_keyword inp ht, ite_eq_left rfl]
  exact score_le_mergeMax _ _

/-- **No loss.** It is at least the keyword leg it had before the step. -/
theorem raise_keyword_ge_leg :
    OLe (phase1 noName inp t).keyword (phase1 raiseName inp t).keyword := by
  rw [raise_keyword inp ht, ite_eq_left rfl]
  exact le_mergeMax _ _

/-- **At least its BM25**, for a hit within the over-fetch cap. -/
theorem raise_keyword_ge_bm25 {s : Int} (hnd : (inp.ranked.map Prod.fst).Nodup)
    (hm : (t, s) ∈ inp.ranked.take inp.cap) :
    OLe (some s) (phase1 raiseName inp t).keyword := by
  have hnd' : ((inp.ranked.take inp.cap).map Prod.fst).Nodup := by
    rw [List.map_take]; exact hnd.sublist (List.take_sublist _ _)
  have hk : (phase1 noName inp t).keyword = some s := by
    rw [phase1_noName_keyword]; exact lastScore_of_mem hnd' hm
  rw [← hk]
  exact raise_keyword_ge_leg inp ht

/-- **Locality.** Every other node's legs are unchanged by the step. -/
theorem raise_local {n : ι} (hn : n ≠ t) : phase1 raiseName inp n = phase1 noName inp n := by
  rw [phase1_eq raiseName_keywordOnly, ht]
  ext <;> simp [raiseName, hn, phase1_noName_keyword, keywordTable]

/-- **No demotion.** The named concept's fused score does not fall. -/
theorem raise_no_demotion : OLe (score noName inp t) (score raiseName inp t) := by
  unfold score
  rw [phase1_eq raiseName_keywordOnly inp t]
  exact merged_mono_keyword _ (by
    have := raise_keyword_ge_leg inp ht
    rwa [phase1_eq raiseName_keywordOnly inp t] at this)

theorem raise_score_local {n : ι} (hn : n ≠ t) : score raiseName inp n = score noName inp n := by
  unfold score; rw [raise_local inp ht hn]

/-- **Keeps its wins.** Every node the named concept beat before the step, it
beats after. -/
theorem raise_keeps_wins (tb : ι → ι → Bool) {j : ι} (hj : j ≠ t)
    (h : Beats (score noName inp) tb t j) : Beats (score raiseName inp) tb t j :=
  beats_of_raise (fun _ hn => raise_score_local inp ht hn) (raise_no_demotion inp ht) hj h

/-- **Never overtaken.** A node that beats the named concept after the step
beat it before. -/
theorem raise_never_overtaken (tb : ι → ι → Bool) {j : ι}
    (h : Beats (score raiseName inp) tb j t) : Beats (score noName inp) tb j t :=
  beats_of_raise_rev (fun _ hn => raise_score_local inp ht hn) (raise_no_demotion inp ht) h

/-- **Rank cannot get worse.** No more nodes sort ahead of the named concept
after the step than before. -/
theorem raise_ahead_le (tb : ι → ι → Bool) (ids : List ι) :
    ahead (score raiseName inp) tb ids t ≤ ahead (score noName inp) tb ids t := by
  unfold ahead
  apply length_filter_mono
  intro j hj
  simp only [decide_eq_true_eq] at hj ⊢
  exact raise_never_overtaken inp ht tb hj

/-- **Agreement.** When the named concept's keyword leg was at most `1.0`, or
it had no keyword hit, the raising step and the assigning one give the same
table: the fix changes only the case #288 describes. -/
theorem raise_eq_assign (hle : OLe (keywordLeg inp.ranked inp.cap t) (some one)) :
    phase1 raiseName inp = phase1 assignName inp := by
  funext n
  rw [phase1_eq raiseName_keywordOnly, phase1_eq assignName_keywordOnly, ht]
  by_cases hn : n = t
  · subst hn
    simp only [raiseName, assignName, keywordTable]
    revert hle
    cases keywordLeg inp.ranked inp.cap n <;> simp [OLe, mergeMax, imax, one] <;> omega
  · simp [raiseName, assignName, hn]

/-- And only then: a keyword leg above `1.0` is where the two differ. -/
theorem raise_ne_assign {s : Int} (hs : keywordLeg inp.ranked inp.cap t = some s)
    (hgt : one < s) : (phase1 raiseName inp t).keyword ≠ (phase1 assignName inp t).keyword := by
  rw [raise_keyword_eq inp ht, assign_keyword_eq inp ht, hs]
  simp [imax]
  omega

end Fixed

/-! ## Observation: the floor is all a hit beyond the cap gets -/

/-- `X` (node 0) is named and has BM25 `3.2`, but a cap of 1 keeps only the
stronger hit `Z` (node 1, `5.0`). `X` is not in the keyword leg, so the
raising step scores it `1.0`, below its BM25. -/
def beyondCapInput : Input Nat where
  ranked := [(1, 5000), (0, 3200)]
  cap := 1
  nameTarget := some 0
  recent := fun _ => false
  recentScore := 350
  vector := []

theorem beyond_cap_floor_only :
    (phase1 noName beyondCapInput 0).keyword = none ∧
    (phase1 raiseName beyondCapInput 0).keyword = some 1000 := by
  decide

/-! ## The fix after review: the name step reads the whole ranked list

`raiseFullName` closes the gap `beyond_cap_floor_only` records: the named
concept's keyword leg is `max(BM25, 1.0)` for its hit anywhere in the ranked
list, inside the over-fetch cap or beyond it. Every other node keeps the leg
the capped loop gave it, so a node beyond the cap that is not named stays out
of the keyword leg. -/

section FullList

variable (inp : Input ι) {t : ι} (ht : inp.nameTarget = some t)
include ht

/-- The named concept's keyword leg after the full-list step, from its leg
before it. -/
theorem full_keyword (n : ι) :
    (phase1 (raiseFullName inp.ranked) inp n).keyword =
      if n = t then mergeMax (mergeHit (phase1 noName inp n).keyword (findScore inp.ranked t)) one
      else (phase1 noName inp n).keyword := by
  rw [phase1_eq (raiseFullName_keywordOnly _), ht]
  by_cases hn : n = t <;> simp [raiseFullName, hn, phase1_noName_keyword, keywordTable]

/-- **Floor.** The named concept's keyword leg is at least `1.0`. -/
theorem full_keyword_ge_one :
    OLe (some one) (phase1 (raiseFullName inp.ranked) inp t).keyword := by
  rw [full_keyword inp ht, ite_eq_left rfl]
  exact score_le_mergeMax _ _

/-- **No loss.** It is at least the keyword leg it had before the step. -/
theorem full_keyword_ge_leg :
    OLe (phase1 noName inp t).keyword (phase1 (raiseFullName inp.ranked) inp t).keyword := by
  rw [full_keyword inp ht, ite_eq_left rfl]
  exact oLe_trans (le_mergeHit _ _) (le_mergeMax _ _)

/-- **At least its BM25, for any hit.** No over-fetch-cap hypothesis: the hit
may lie anywhere in the ranked list. -/
theorem full_keyword_ge_bm25 {s : Int} (hnd : (inp.ranked.map Prod.fst).Nodup)
    (hm : (t, s) ∈ inp.ranked) :
    OLe (some s) (phase1 (raiseFullName inp.ranked) inp t).keyword := by
  rw [full_keyword inp ht, ite_eq_left rfl, findScore_of_mem hnd hm]
  exact oLe_trans (score_le_mergeMax _ _) (le_mergeMax _ _)

/-- The exact value: `max(BM25, 1.0)` for its BM25 on the whole list, or `1.0`
when the list has no hit for it. -/
theorem full_keyword_eq (hnd : (inp.ranked.map Prod.fst).Nodup) :
    (phase1 (raiseFullName inp.ranked) inp t).keyword =
      match findScore inp.ranked t with
      | some s => some (imax s one)
      | none => some one := by
  rw [full_keyword inp ht, ite_eq_left rfl, phase1_noName_keyword]
  cases hk : keywordLeg inp.ranked inp.cap t with
  | none =>
    cases findScore inp.ranked t <;> rfl
  | some k =>
    rw [keywordLeg_eq_findScore hnd hk]
    simp [mergeHit, mergeMax, imax]

/-- `full_keyword_eq` for a hit: exactly `max(BM25, 1.0)`, wherever the hit
lies. -/
theorem full_keyword_of_mem {s : Int} (hnd : (inp.ranked.map Prod.fst).Nodup)
    (hm : (t, s) ∈ inp.ranked) :
    (phase1 (raiseFullName inp.ranked) inp t).keyword = some (imax s one) := by
  rw [full_keyword_eq inp ht hnd, findScore_of_mem hnd hm]

/-- `full_keyword_eq` without a hit: exactly `1.0`. -/
theorem full_keyword_of_not_mem (hn : t ∉ inp.ranked.map Prod.fst) :
    (phase1 (raiseFullName inp.ranked) inp t).keyword = some one := by
  rw [full_keyword inp ht, ite_eq_left rfl, findScore_of_not_mem hn, phase1_noName_keyword]
  have : keywordLeg inp.ranked inp.cap t = none := by
    apply lastScore_of_not_mem
    intro hmem
    rw [List.map_take] at hmem
    exact hn (List.mem_of_mem_take hmem)
  rw [this]; rfl

/-- **Locality.** Every other node's legs are unchanged by the step, a node
beyond the over-fetch cap included: it stays out of the keyword leg. -/
theorem full_local {n : ι} (hn : n ≠ t) :
    phase1 (raiseFullName inp.ranked) inp n = phase1 noName inp n := by
  rw [phase1_eq (raiseFullName_keywordOnly _), ht]
  ext <;> simp [raiseFullName, hn, phase1_noName_keyword, keywordTable]

/-- **No demotion.** The named concept's fused score does not fall. -/
theorem full_no_demotion :
    OLe (score noName inp t) (score (raiseFullName inp.ranked) inp t) := by
  unfold score
  rw [phase1_eq (raiseFullName_keywordOnly _) inp t]
  exact merged_mono_keyword _ (by
    have := full_keyword_ge_leg inp ht
    rwa [phase1_eq (raiseFullName_keywordOnly _) inp t] at this)

theorem full_score_local {n : ι} (hn : n ≠ t) :
    score (raiseFullName inp.ranked) inp n = score noName inp n := by
  unfold score; rw [full_local inp ht hn]

/-- **Keeps its wins.** -/
theorem full_keeps_wins (tb : ι → ι → Bool) {j : ι} (hj : j ≠ t)
    (h : Beats (score noName inp) tb t j) : Beats (score (raiseFullName inp.ranked) inp) tb t j :=
  beats_of_raise (fun _ hn => full_score_local inp ht hn) (full_no_demotion inp ht) hj h

/-- **Never overtaken.** -/
theorem full_never_overtaken (tb : ι → ι → Bool) {j : ι}
    (h : Beats (score (raiseFullName inp.ranked) inp) tb j t) : Beats (score noName inp) tb j t :=
  beats_of_raise_rev (fun _ hn => full_score_local inp ht hn) (full_no_demotion inp ht) h

/-- **Rank cannot get worse.** No more nodes sort ahead of the named concept
after the step than before it. -/
theorem full_ahead_le (tb : ι → ι → Bool) (ids : List ι) :
    ahead (score (raiseFullName inp.ranked) inp) tb ids t ≤ ahead (score noName inp) tb ids t := by
  unfold ahead
  apply length_filter_mono
  intro j hj
  simp only [decide_eq_true_eq] at hj ⊢
  exact full_never_overtaken inp ht tb hj

/-- Against the first #288 step: the full-list step's keyword leg for the
named concept is never below `raiseName`'s. -/
theorem full_keyword_ge_raise :
    OLe (phase1 raiseName inp t).keyword (phase1 (raiseFullName inp.ranked) inp t).keyword := by
  rw [full_keyword inp ht, ite_eq_left rfl, raise_keyword inp ht, ite_eq_left rfl]
  exact mergeMax_mono (le_mergeHit _ _) _

/-- And so nothing sorts ahead of the named concept under the full-list step
that did not under `raiseName`. -/
theorem full_ahead_le_raise (tb : ι → ι → Bool) (ids : List ι) :
    ahead (score (raiseFullName inp.ranked) inp) tb ids t ≤ ahead (score raiseName inp) tb ids t := by
  have hloc : ∀ n, n ≠ t → score (raiseFullName inp.ranked) inp n = score raiseName inp n :=
    fun n hn => by rw [full_score_local inp ht hn, raise_score_local inp ht hn]
  have hup : OLe (score raiseName inp t) (score (raiseFullName inp.ranked) inp t) := by
    unfold score
    rw [phase1_eq raiseName_keywordOnly inp t, phase1_eq (raiseFullName_keywordOnly _) inp t]
    refine merged_mono ?_ (oLe_refl _) (oLe_refl _)
    have := full_keyword_ge_raise inp ht
    rwa [phase1_eq raiseName_keywordOnly inp t,
      phase1_eq (raiseFullName_keywordOnly _) inp t] at this
  unfold ahead
  apply length_filter_mono
  intro j hj
  simp only [decide_eq_true_eq] at hj ⊢
  exact beats_of_raise_rev hloc hup hj

/-- **Agreement.** When the named concept's hit is within the over-fetch cap,
or it has no hit at all, the full-list step and `raiseName` give the same
table: the review fix changes only the beyond-cap case. -/
theorem full_eq_raise (hnd : (inp.ranked.map Prod.fst).Nodup)
    (hcap : t ∉ inp.ranked.map Prod.fst ∨ ∃ s, (t, s) ∈ inp.ranked.take inp.cap) :
    phase1 (raiseFullName inp.ranked) inp = phase1 raiseName inp := by
  have hfind : findScore inp.ranked t = keywordLeg inp.ranked inp.cap t := by
    rcases hcap with hn | ⟨s, hm⟩
    · rw [findScore_of_not_mem hn]
      symm
      apply lastScore_of_not_mem
      intro hmem
      rw [List.map_take] at hmem
      exact hn (List.mem_of_mem_take hmem)
    · have hnd' : ((inp.ranked.take inp.cap).map Prod.fst).Nodup := by
        rw [List.map_take]; exact hnd.sublist (List.take_sublist _ _)
      rw [findScore_of_mem hnd (List.mem_of_mem_take hm)]
      exact (lastScore_of_mem hnd' hm).symm
  funext n
  rw [phase1_eq (raiseFullName_keywordOnly _), phase1_eq raiseName_keywordOnly, ht]
  by_cases hn : n = t
  · subst hn
    simp [raiseFullName, raiseName, keywordTable, hfind, mergeHit_self]
  · simp [raiseFullName, raiseName, hn]

end FullList

/-- The beyond-cap shape with a third node: `X` (node 0) is named with BM25
`3.2`; a cap of 1 keeps only `Z` (node 1, `5.0`); `W` (node 2, `4.0`) is also
beyond the cap but not named. -/
def beyondCapInput3 : Input Nat where
  ranked := [(1, 5000), (2, 4000), (0, 3200)]
  cap := 1
  nameTarget := some 0
  recent := fun _ => false
  recentScore := 350
  vector := []

/-- **The gap closed.** Where `raiseName` gave the beyond-cap named concept
only the `1.0` floor, the full-list step keeps its BM25 `3.2`; the in-cap
node keeps its `5.0`, and the unnamed node beyond the cap stays out of the
keyword leg. On `beyondCapInput`, the input of `beyond_cap_floor_only`, too. -/
theorem full_keeps_beyond_cap_bm25 :
    (phase1 raiseName beyondCapInput 0).keyword = some 1000 ∧
    (phase1 (raiseFullName beyondCapInput.ranked) beyondCapInput 0).keyword = some 3200 ∧
    (phase1 raiseName beyondCapInput3 0).keyword = some 1000 ∧
    (phase1 (raiseFullName beyondCapInput3.ranked) beyondCapInput3 0).keyword = some 3200 ∧
    (phase1 (raiseFullName beyondCapInput3.ranked) beyondCapInput3 1).keyword = some 5000 ∧
    (phase1 (raiseFullName beyondCapInput3.ranked) beyondCapInput3 2).keyword = none := by
  decide

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
recent weight `0.3`. -/
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

/-- **Exact.** Each scaled term is an exact quotient: no rounding at all. -/
theorem term_exact (c : RrfConfig) {N r : Nat} (h1 : 1 ≤ r) (h2 : r ≤ N) :
    prodDen c N / c.den r * c.den r = prodDen c N :=
  Nat.div_mul_cancel (den_dvd_prodDen c h1 h2)

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

/-- **An absent leg contributes 0.** -/
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

theorem idOrder_strictTotal : StrictTotal idOrder where
  irrefl a := by simp [idOrder]
  total a b h := by simp only [idOrder, decide_eq_true_eq]; omega
  trans a b c h1 h2 := by simp only [idOrder, decide_eq_true_eq] at *; omega

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
theorem beats_asymm {tb : ι → ι → Bool} (htb : StrictTotal tb) (sc : ι → Option Int) {a b : ι}
    (h : Beats sc tb a b) : ¬ Beats sc tb b a :=
  fun h' => beats_irrefl htb sc a (beats_trans htb sc h h')

omit [DecidableEq ι] in
theorem length_filter_lt {l : List ι} {p q : ι → Bool} (h : ∀ x, p x = true → q x = true)
    {a : ι} (ha : a ∈ l) (hq : q a = true) (hp : p a = false) :
    (l.filter p).length < (l.filter q).length := by
  induction l with
  | nil => simp at ha
  | cons x rest ih =>
    have hmono := length_filter_mono (l := rest) h
    simp only [List.filter_cons]
    rcases List.mem_cons.mp ha with hx | ha'
    · subst hx
      simp [hq, hp]
      omega
    · have := ih ha'
      cases hpx : p x <;> cases hqx : q x <;> simp <;> first | omega | (have := h x hpx; simp_all)

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
theorem rrf_den_pos (n : ι) : 0 < (R.rrf legs n).2 := by
  unfold RrfRecall.rrf; split <;> simp <;> omega

omit [DecidableEq ι] in
/-- Ordering two nodes by their exact `rrf` is ordering them by `key`. -/
theorem rrf_le_iff_key (a b : ι) : RLe (R.rrf legs a) (R.rrf legs b) ↔ OLe (R.key legs a) (R.key legs b) := by
  unfold RrfRecall.rrf RrfRecall.key RLe
  by_cases h : R.best legs = 0
  · simp [h, OLe]
  · simp only [h, ite_false, OLe]
    rw [Nat.mul_le_mul_right_iff (by omega)]
    omega

omit [DecidableEq ι] in
theorem any_of_mem {ids : List ι} {f : ι → Bool} {n : ι} (hn : n ∈ ids) (h : f n = true) :
    ids.any f = true := List.any_eq_true.mpr ⟨n, hn, h⟩

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
weight of 0 it fails: every `rrf` is 0 and the order is the tie-break alone
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

theorem rrf_one_leg_example :
    (oneLegRecall rrfDefault).rrfAhead oneLegLegs 2 = 0 ∧
    (oneLegRecall rrfDefault).rrfAhead oneLegLegs 0 = 1 ∧
    (oneLegRecall rrfDefault).rrfAhead oneLegLegs 1 = 2 ∧
    ahead (fun n => (oneLegLegs n).keyword) idOrder [0, 1, 2] 2 = 0 := by
  decide

/-- Leg ranks on `oneLegLegs`: BM25 `3.0` first, then the `2.0` tie broken by
`tb`, the same whatever order the candidates arrive in (`legRank_lt_iff`,
`legRank_perm`). -/
theorem legRank_example :
    (oneLegRecall rrfDefault).ranks oneLegLegs 2 = ⟨some 1, none, none⟩ ∧
    (oneLegRecall rrfDefault).ranks oneLegLegs 0 = ⟨some 2, none, none⟩ ∧
    (oneLegRecall rrfDefault).ranks oneLegLegs 1 = ⟨some 3, none, none⟩ ∧
    legRank (fun n => (oneLegLegs n).keyword) idOrder [1, 2, 0] 1 = some 3 ∧
    legRank (fun n => (oneLegLegs n).keyword) idOrder [1, 2, 0] 2 = some 1 ∧
    REq ((oneLegRecall rrfDefault).rrf oneLegLegs 1) (2, 3) := by
  decide

/-- **The weight hypothesis is needed.** `RecallFusion::validate` admits a
keyword weight of 0 beside a positive vector weight. A recall where only the
keyword leg produced (no embedding, no recent interaction) then scores every
candidate 0, and RRF sorts by the tie-break alone: node 2, the best BM25,
comes last. -/
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

theorem length_filter_shift {l : List ι} (hnd : l.Nodup) (p q : ι → Bool) (t : ι)
    (heq : ∀ x, x ≠ t → q x = p x) (hpt : p t = true → q t = true) :
    (l.filter q).length =
      (l.filter p).length + (if t ∈ l ∧ q t = true ∧ p t = false then 1 else 0) := by
  induction l with
  | nil => simp
  | cons x rest ih =>
    rw [List.nodup_cons] at hnd
    have ih := ih hnd.2
    simp only [List.filter_cons]
    by_cases hx : x = t
    · subst hx
      have hc : ¬ (x ∈ rest ∧ q x = true ∧ p x = false) := fun h => hnd.1 h.1
      rw [ite_eq_right hc] at ih
      cases hp : p x <;> cases hq : q x <;> simp_all
    · have hqx := heq x hx
      have hmem : (t ∈ x :: rest) ↔ t ∈ rest := by
        simp only [List.mem_cons]; constructor
        · rintro (h | h)
          · exact absurd h.symm hx
          · exact h
        · exact Or.inr
      by_cases hc : t ∈ rest ∧ q t = true ∧ p t = false
      · have hc' : t ∈ x :: rest ∧ q t = true ∧ p t = false := ⟨hmem.mpr hc.1, hc.2⟩
        rw [ite_eq_left hc] at ih; rw [ite_eq_left hc']
        cases hp : p x <;> simp_all <;> omega
      · have hc' : ¬ (t ∈ x :: rest ∧ q t = true ∧ p t = false) := fun h => hc ⟨hmem.mp h.1, h.2⟩
        rw [ite_eq_right hc] at ih; rw [ite_eq_right hc']
        cases hp : p x <;> simp_all

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

theorem full_rank_eq_raise_needs_sorted :
    ¬ SortedBy idOrder unsortedInput.ranked ∧ legRank (kwSlots (phase1 (raiseFullName unsortedInput.ranked) unsortedInput)) idOrder [0, 1] 1 =
      some 2 ∧
    legRank (kwSlots (phase1 raiseName unsortedInput)) idOrder [0, 1] 1 = some 1 := by
  refine ⟨?_, by decide⟩
  intro h
  simp only [SortedBy, unsortedInput, List.pairwise_cons] at h
  have := h.1 (0, 3200) List.mem_cons_self
  simp at this

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
