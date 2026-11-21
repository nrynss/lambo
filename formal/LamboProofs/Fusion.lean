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
  default (`LegScores::fused`, `src/recall/fusion.rs`);
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
  most recent interactions); its ranks feed RRF only and are left out.
* The search yields each id once (it scores through a `HashMap`), so the
  ranked list's ids are distinct; the keyword step is modelled as the Rust
  loop, last assignment wins (`lastScore`), and the theorems that read a BM25
  off the list assume distinct ids.

Left out: reciprocal rank fusion (`FusionKind::Rrf`, the leg ranks and the
`rrf` field), the trace instrumentation, and the keywordless path
(`candidates_without_keyword`, which has no name step). `KeywordLeg` and
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
* `full_local`: every other node's legs are unchanged, so an unnamed node
  beyond the cap stays out of the keyword leg.
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
node's `keyword_rank` and `rrf`, so other nodes' RRF fields can move; RRF is
not modelled, and "locality" here means the leg slots under max fusion.
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

end Lambo.Fusion
