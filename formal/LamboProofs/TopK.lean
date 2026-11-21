import LamboProofs.Order

/-
# Top-K emission: forced members never take a slot and are never cut

Model of `emitted` in `src/recall/assemble.rs`. The first part (`emit`)
leaves out the optional redundancy skip (#181, off by default:
`skip_redundant = false`); the second part (`emitR`, from "The redundancy skip"
on) models it, with the selection rule of `src/recall/redundancy.rs`. Without
the skip the loop is:

```rust
let mut slots = 0usize;
for s in ranked {
    let Some(Node::Concept(c)) = graph.node(s.item) else { continue };
    let force = forced(s.item);
    if !force && slots >= top_k { continue; }
    if !force { slots += 1; }
    out.push(s.clone());
}
```

`ranked` is the member list in `rank_members` order. `forced` marks members
that must be shown whatever their rank (a live conflict, a contradiction on a
Canonical or Venerable concept and its companion, and in cold mode a kept
reservation holder).

## Model

`emit isConcept forced topK slots ranked` is the loop from a state with `slots`
slots already used. `isConcept` is "the id resolves to a concept in the graph"
(a stale durable-vector id does not).

## Results

* `emit_sublist`: the output is a subsequence of `ranked`: order is kept and
  nothing is invented. Hence `emit_pairwise`: any ordering property of the
  ranked list, such as Canonical-first, holds of the output.
* `forced_mem_emit`: every forced concept is emitted, at any `top_k`,
  including `0` ("a live conflict warning is never truncated away by rank").
* `emit_free_le`: at most `top_k` non-forced members are emitted ("a forced
  member never consumes \[a slot\]").
* `emit_all_concepts`: a graph-missing id is never emitted.
* `emit_eq_filter`: when the ordinary concepts fit in `top_k`, every concept
  is emitted.
* `emit_free_eq_take`: the non-forced members emitted are exactly the first
  `top_k` non-forced concepts in rank order, so forced members do not change
  which ordinary members make the cut ("adding one never moves the cut").

## The redundancy skip (#181)

`emitR` is `emitted` with a `Selection`, and `skipsR` the skips it records
(`Emission::skipped`, as `(kept, skipped)` pairs in scan order). The pair
verdict `red` and the warning test `warns` are parameters; the signals are
not modelled (see the section's own comment for why the pair budget is
covered).

* `emitR_sublist`, `emitR_pairwise`: still a subsequence of the ranked list
  ("Canonical-first order is untouched: the rule only skips").
* `forced_mem_emitR`: forced members are still never cut, nor skipped
  ("A forced member is never skipped").
* `skipsR_spec`: a skipped member is a concept, not forced, carries no warning
  line of its own, and is redundant with the hit it is named on ("Never
  skipped").
* `emitR_free_count`: the ordinary hits number `min top_k (ordinary −
  skipped)`: a skipped member takes no slot ("the next distinct member fills
  its slot"); `emitR_free_le_topK`: still at most `top_k`.
* `skipsR_length_le`: at most `window top_k` members are skipped ("past the
  window nothing is checked").
* `emitR_eq_emit_of_skipsR_nil`, `emitR_eq_emit_of_no_redundant_pair`,
  `emitR_eq_emit_of_no_redundant_distinct_pair`, `emitR_window_zero`: when nothing is skipped the result is the no-skip
  model's ("When nothing is skipped the result is exactly the one without
  it"; test `off_or_with_nothing_redundant_the_result_is_byte_identical`).
* `skipsR_kept_emitted`, `skipsR_kept_before`, `skipsR_pairwise`: the kept
  hit was emitted, ahead of the skipped one; of a redundant pair the one
  ranked first is kept (test
  `a_semantic_edge_pair_keeps_the_higher_and_the_next_distinct_fills_the_slot`,
  `equal_scores_keep_the_copy_the_tie_break_ranks_first`).
* `skipsR_not_emitted`: a skipped member is not emitted (the companion step
  after emission, not modelled, can still bring it back as a contradiction
  companion; `assemble.rs` drops it from the skip list then).
* `skipped_canonical_kept_canonical`: in `rank_members` order a skipped
  Canonical member was skipped for a Canonical hit, so a ⚑ banner, which
  `warns` does not count, is still present: the kept copy's. The skipped
  member's own ⚑ line, which states its own blast radius
  (`radii.get(&c.id)`), is not shown; only the kept copy's count and an
  `also <id>` label remain (test `canonical_first_order_is_kept`).

The last section applies #280's gated order (`Order.gatedRankMembers`, a
design model) to emission: a forced pillar is shown whatever the order, and
behind the gate `skipped_canonical_kept_canonical` fails (a `decide`
counterexample, which needs a redundant pair whose relevance falls on both
sides of the gate).
-/

namespace Lambo.TopK

variable {ι : Type}

/-- The emission loop. -/
def emit (isConcept forced : ι → Bool) (topK : Nat) : Nat → List ι → List ι
  | _, [] => []
  | slots, s :: rest =>
    if isConcept s = false then emit isConcept forced topK slots rest
    else if forced s = true then s :: emit isConcept forced topK slots rest
    else if slots < topK then s :: emit isConcept forced topK (slots + 1) rest
    else emit isConcept forced topK slots rest

theorem emit_nil (isConcept forced : ι → Bool) (topK slots : Nat) :
    emit isConcept forced topK slots [] = [] := by
  simp [emit]

section Unfold

variable {isConcept forced : ι → Bool} {topK : Nat} {s : ι}

/-- A graph-missing member is skipped and takes no slot. -/
theorem emit_missing (hc : isConcept s = false) (slots : Nat) (rest : List ι) :
    emit isConcept forced topK slots (s :: rest) = emit isConcept forced topK slots rest := by
  simp [emit, hc]

/-- A forced member is emitted and takes no slot. -/
theorem emit_forced (hc : isConcept s = true) (hf : forced s = true) (slots : Nat)
    (rest : List ι) :
    emit isConcept forced topK slots (s :: rest)
      = s :: emit isConcept forced topK slots rest := by
  simp [emit, hc, hf]

/-- An ordinary member with a free slot is emitted and takes it. -/
theorem emit_take (hc : isConcept s = true) (hf : forced s = false) {slots : Nat}
    (hs : slots < topK) (rest : List ι) :
    emit isConcept forced topK slots (s :: rest)
      = s :: emit isConcept forced topK (slots + 1) rest := by
  simp [emit, hc, hf, hs]

/-- An ordinary member with no free slot is cut; the scan continues. -/
theorem emit_cut (hc : isConcept s = true) (hf : forced s = false) {slots : Nat}
    (hs : ¬ slots < topK) (rest : List ι) :
    emit isConcept forced topK slots (s :: rest) = emit isConcept forced topK slots rest := by
  simp [emit, hc, hf, hs]

end Unfold

/-- **Order kept.** The emitted list is a subsequence of the ranked list. -/
theorem emit_sublist (isConcept forced : ι → Bool) (topK : Nat) :
    ∀ (ranked : List ι) (slots : Nat),
      List.Sublist (emit isConcept forced topK slots ranked) ranked := by
  intro ranked
  induction ranked with
  | nil =>
    intro slots
    rw [emit_nil] <;> exact List.Sublist.slnil
  | cons s rest ih =>
    intro slots
    cases hc : isConcept s with
    | false =>
      rw [emit_missing hc]
      exact List.Sublist.cons s (ih slots)
    | true =>
      cases hf : forced s with
      | true =>
        rw [emit_forced hc hf]
        exact List.Sublist.cons_cons s (ih slots)
      | false =>
        by_cases hs : slots < topK
        · rw [emit_take hc hf hs]
          exact List.Sublist.cons_cons s (ih (slots + 1))
        · rw [emit_cut hc hf hs]
          exact List.Sublist.cons s (ih slots)

/-- Any pairwise ordering of the ranked list survives emission. With
`Order.rankMembers_canonical_first` this gives Canonical-first in the hits a
recall returns. -/
theorem emit_pairwise (isConcept forced : ι → Bool) (topK slots : Nat) {R : ι → ι → Prop}
    {ranked : List ι} (h : ranked.Pairwise R) :
    (emit isConcept forced topK slots ranked).Pairwise R :=
  List.Pairwise.sublist (emit_sublist isConcept forced topK ranked slots) h

/-- **Forced members are never cut.** -/
theorem forced_mem_emit (isConcept forced : ι → Bool) (topK : Nat) (x : ι)
    (hcx : isConcept x = true) (hfx : forced x = true) :
    ∀ (ranked : List ι) (slots : Nat), x ∈ ranked →
      x ∈ emit isConcept forced topK slots ranked := by
  intro ranked
  induction ranked with
  | nil =>
    intro slots hx
    simp at hx
  | cons s rest ih =>
    intro slots hx
    cases hc : isConcept s with
    | false =>
      rw [emit_missing hc]
      cases List.mem_cons.mp hx with
      | inl heq =>
        rw [heq, hc] at hcx
        exact absurd hcx (by decide)
      | inr hmem => exact ih slots hmem
    | true =>
      cases hf : forced s with
      | true =>
        rw [emit_forced hc hf]
        cases List.mem_cons.mp hx with
        | inl heq => exact List.mem_cons.mpr (Or.inl heq)
        | inr hmem => exact List.mem_cons.mpr (Or.inr (ih slots hmem))
      | false =>
        by_cases hs : slots < topK
        · rw [emit_take hc hf hs]
          cases List.mem_cons.mp hx with
          | inl heq => exact List.mem_cons.mpr (Or.inl heq)
          | inr hmem => exact List.mem_cons.mpr (Or.inr (ih (slots + 1) hmem))
        · rw [emit_cut hc hf hs]
          cases List.mem_cons.mp hx with
          | inl heq =>
            rw [heq, hf] at hfx
            exact absurd hfx (by decide)
          | inr hmem => exact ih slots hmem

/-- Everything emitted is a concept of the graph and was ranked. -/
theorem emit_all_concepts (isConcept forced : ι → Bool) (topK : Nat) (x : ι) :
    ∀ (ranked : List ι) (slots : Nat), x ∈ emit isConcept forced topK slots ranked →
      isConcept x = true := by
  intro ranked
  induction ranked with
  | nil =>
    intro slots hx
    rw [emit_nil] at hx
    simp at hx
  | cons s rest ih =>
    intro slots hx
    cases hc : isConcept s with
    | false =>
      rw [emit_missing hc] at hx
      exact ih slots hx
    | true =>
      cases hf : forced s with
      | true =>
        rw [emit_forced hc hf] at hx
        cases List.mem_cons.mp hx with
        | inl heq =>
          rw [heq]
          exact hc
        | inr hmem => exact ih slots hmem
      | false =>
        by_cases hs : slots < topK
        · rw [emit_take hc hf hs] at hx
          cases List.mem_cons.mp hx with
          | inl heq =>
            rw [heq]
            exact hc
          | inr hmem => exact ih (slots + 1) hmem
        · rw [emit_cut hc hf hs] at hx
          exact ih slots hx

/-! ## Counting slots -/

/-- Number of non-forced members of a list. -/
def freeCount (forced : ι → Bool) : List ι → Nat
  | [] => 0
  | x :: xs => freeCount forced xs + (if forced x = true then 0 else 1)

theorem freeCount_cons_forced {forced : ι → Bool} {x : ι} (h : forced x = true)
    (xs : List ι) : freeCount forced (x :: xs) = freeCount forced xs := by
  simp [freeCount, h]

theorem freeCount_cons_free {forced : ι → Bool} {x : ι} (h : forced x = false)
    (xs : List ι) : freeCount forced (x :: xs) = freeCount forced xs + 1 := by
  simp [freeCount, h]

/-- **At most `top_k` ordinary members.** Forced members are not counted: they
never consume a slot. -/
theorem emit_free_le (isConcept forced : ι → Bool) (topK : Nat) :
    ∀ (ranked : List ι) (slots : Nat), slots ≤ topK →
      freeCount forced (emit isConcept forced topK slots ranked) + slots ≤ topK := by
  intro ranked
  induction ranked with
  | nil =>
    intro slots hle
    have e : freeCount forced ([] : List ι) = 0 := by simp [freeCount]
    rw [emit_nil, e]
    omega
  | cons s rest ih =>
    intro slots hle
    cases hc : isConcept s with
    | false =>
      rw [emit_missing hc]
      exact ih slots hle
    | true =>
      cases hf : forced s with
      | true =>
        rw [emit_forced hc hf, freeCount_cons_forced hf]
        exact ih slots hle
      | false =>
        by_cases hs : slots < topK
        · rw [emit_take hc hf hs, freeCount_cons_free hf]
          have := ih (slots + 1) (by omega)
          omega
        · rw [emit_cut hc hf hs]
          exact ih slots hle

/-- The headline form: a recall emits at most `top_k` non-forced hits. -/
theorem emit_free_le_topK (isConcept forced : ι → Bool) (topK : Nat) (ranked : List ι) :
    freeCount forced (emit isConcept forced topK 0 ranked) ≤ topK := by
  have := emit_free_le isConcept forced topK ranked 0 (Nat.zero_le topK)
  omega

/-! ## Exactly which members are emitted -/

/-- The ordinary concepts of a list, in order: in the graph and not forced. -/
def ordinary (isConcept forced : ι → Bool) : List ι → List ι
  | [] => []
  | x :: xs =>
    if isConcept x = true ∧ forced x = false then x :: ordinary isConcept forced xs
    else ordinary isConcept forced xs

/-- The non-forced members of a list, in order. -/
def freeOnly (forced : ι → Bool) : List ι → List ι
  | [] => []
  | x :: xs => if forced x = false then x :: freeOnly forced xs else freeOnly forced xs

/-- **The cut is by rank among ordinary members only.** The non-forced hits are
exactly the first `top_k − slots` ordinary concepts of the ranked list. Forced
members appear nowhere in this statement, so adding or removing them cannot
change which ordinary members are shown. -/
theorem emit_free_eq_take (isConcept forced : ι → Bool) (topK : Nat) :
    ∀ (ranked : List ι) (slots : Nat),
      freeOnly forced (emit isConcept forced topK slots ranked)
        = (ordinary isConcept forced ranked).take (topK - slots) := by
  intro ranked
  induction ranked with
  | nil =>
    intro slots
    rw [emit_nil]
    simp [freeOnly, ordinary]
  | cons s rest ih =>
    intro slots
    cases hc : isConcept s with
    | false =>
      rw [emit_missing hc]
      have e : ordinary isConcept forced (s :: rest) = ordinary isConcept forced rest := by
        simp [ordinary, hc]
      rw [e]
      exact ih slots
    | true =>
      cases hf : forced s with
      | true =>
        rw [emit_forced hc hf]
        have e : ordinary isConcept forced (s :: rest) = ordinary isConcept forced rest := by
          simp [ordinary, hc, hf]
        have e' : freeOnly forced (s :: emit isConcept forced topK slots rest)
            = freeOnly forced (emit isConcept forced topK slots rest) := by
          simp [freeOnly, hf]
        rw [e, e']
        exact ih slots
      | false =>
        have e : ordinary isConcept forced (s :: rest)
            = s :: ordinary isConcept forced rest := by
          simp [ordinary, hc, hf]
        by_cases hs : slots < topK
        · rw [emit_take hc hf hs]
          have e' : freeOnly forced (s :: emit isConcept forced topK (slots + 1) rest)
              = s :: freeOnly forced (emit isConcept forced topK (slots + 1) rest) := by
            simp [freeOnly, hf]
          have en : topK - slots = (topK - (slots + 1)) + 1 := by omega
          rw [e, e', en, List.take_succ_cons, ih (slots + 1)]
        · rw [emit_cut hc hf hs]
          have en : topK - slots = 0 := by omega
          rw [ih slots, e, en] <;> simp

/-- The forced concepts of a list, in order: in the graph and forced. -/
def forcedConcepts (isConcept forced : ι → Bool) : List ι → List ι
  | [] => []
  | x :: xs =>
    if isConcept x = true ∧ forced x = true then x :: forcedConcepts isConcept forced xs
    else forcedConcepts isConcept forced xs

/-- The forced members of a list, in order. -/
def forcedOnly (forced : ι → Bool) : List ι → List ι
  | [] => []
  | x :: xs => if forced x = true then x :: forcedOnly forced xs else forcedOnly forced xs

/-- The forced hits are exactly the forced concepts of the ranked list, in
order: none is cut and none is added. -/
theorem emit_forced_eq (isConcept forced : ι → Bool) (topK : Nat) :
    ∀ (ranked : List ι) (slots : Nat),
      forcedOnly forced (emit isConcept forced topK slots ranked)
        = forcedConcepts isConcept forced ranked := by
  intro ranked
  induction ranked with
  | nil =>
    intro slots
    rw [emit_nil]
    simp [forcedOnly, forcedConcepts]
  | cons s rest ih =>
    intro slots
    cases hc : isConcept s with
    | false =>
      rw [emit_missing hc]
      have e : forcedConcepts isConcept forced (s :: rest)
          = forcedConcepts isConcept forced rest := by
        simp [forcedConcepts, hc]
      rw [e]
      exact ih slots
    | true =>
      cases hf : forced s with
      | true =>
        rw [emit_forced hc hf]
        have e : forcedConcepts isConcept forced (s :: rest)
            = s :: forcedConcepts isConcept forced rest := by
          simp [forcedConcepts, hc, hf]
        have e' : forcedOnly forced (s :: emit isConcept forced topK slots rest)
            = s :: forcedOnly forced (emit isConcept forced topK slots rest) := by
          simp [forcedOnly, hf]
        rw [e, e', ih slots]
      | false =>
        have e : forcedConcepts isConcept forced (s :: rest)
            = forcedConcepts isConcept forced rest := by
          simp [forcedConcepts, hc, hf]
        by_cases hs : slots < topK
        · rw [emit_take hc hf hs]
          have e' : forcedOnly forced (s :: emit isConcept forced topK (slots + 1) rest)
              = forcedOnly forced (emit isConcept forced topK (slots + 1) rest) := by
            simp [forcedOnly, hf]
          rw [e, e']
          exact ih (slots + 1)
        · rw [emit_cut hc hf hs, e]
          exact ih slots

/-- When the ordinary concepts all fit, nothing is cut: the recall emits every
ranked concept. -/
theorem emit_eq_filter (isConcept forced : ι → Bool) (topK : Nat) :
    ∀ (ranked : List ι) (slots : Nat),
      slots + (ordinary isConcept forced ranked).length ≤ topK →
      emit isConcept forced topK slots ranked = ranked.filter isConcept := by
  intro ranked
  induction ranked with
  | nil =>
    intro slots _
    rw [emit_nil] <;> simp
  | cons s rest ih =>
    intro slots hle
    cases hc : isConcept s with
    | false =>
      have e : ordinary isConcept forced (s :: rest) = ordinary isConcept forced rest := by
        simp [ordinary, hc]
      rw [e] at hle
      rw [emit_missing hc, ih slots hle] <;> simp [hc]
    | true =>
      cases hf : forced s with
      | true =>
        have e : ordinary isConcept forced (s :: rest) = ordinary isConcept forced rest := by
          simp [ordinary, hc, hf]
        rw [e] at hle
        rw [emit_forced hc hf, ih slots hle] <;> simp [hc]
      | false =>
        have e : ordinary isConcept forced (s :: rest)
            = s :: ordinary isConcept forced rest := by
          simp [ordinary, hc, hf]
        rw [e] at hle
        have hlen : (s :: ordinary isConcept forced rest).length
            = (ordinary isConcept forced rest).length + 1 := by
          simp
        rw [hlen] at hle
        have hs : slots < topK := by omega
        rw [emit_take hc hf hs, ih (slots + 1) (by omega)] <;> simp [hc]

/-! ## The redundancy skip (#181)

`emitted` with a `Selection` (`[selection] skip_redundant = true`): each of the
first `window top_k` ordinary members that reach the slot check is compared
with the first `WINDOW_MAX` members already emitted (`taken`); if some signal
finds it redundant with one of them it is skipped, takes no slot, and the scan
goes on, unless it would carry a warning line of its own (`warns`). A forced
member is never checked, but is `taken`, so a later ordinary copy of it can be
skipped. The loop, past the slot test:

```rust
let check = !force && checked < window;
if check || taken.len() < WINDOW_MAX {
    let text = normalized_content(&c.content);
    if check {
        checked += 1;
        let found = taken.iter().find_map(|(t, t_norm)| sel.verdict(graph, (t, t_norm), (c, &text)).map(..));
        if let Some((kept, signal)) = found && !(sel.warns)(c.id) {
            skipped.entry(kept).or_default().push(..);
            continue;
        }
    }
    norm = Some(text);
}
if !force { slots += 1; }
if let Some(text) = norm && taken.len() < WINDOW_MAX { taken.push((c, text)); }
out.push(s.clone());
```

`taken` is therefore the first `WINDOW_MAX` emitted members: every emitted
member is pushed while `taken` has room. The model passes the emitted prefix
`prev` and reads `prev.take wmax`.

`red t s` is the pair verdict, `Selection::verdict`. It is a parameter, so
every theorem below holds for every verdict relation. That covers the pair
budget: a pair is judged once per assembly and memoized, and once
`PAIR_BUDGET` verdicts exist an unjudged pair is not redundant from then on,
so within one assembly each pair gets one fixed answer, and the passes behave
as `emitR` with that (run-dependent) sub-relation of `Redundancy::signal`.
`warns` is the `warns` closure (hot-list payload, active reservation,
suspected pair, non-Canonical supersession line, #207 stale citation). The
signals themselves (`normalized_content`, `distinct_claims`,
`never_redundant`, cosine) are not modelled. `selection = None` is
`emitR` with `win = 0` or with `red` never true (`emitR_window_zero`,
`emitR_eq_emit_of_no_redundant_pair`).
-/

/-- `redundancy::WINDOW_MAX`: the most members `taken` holds, and the cap on
the window. -/
def windowMax : Nat := 20

/-- `redundancy::window`: `min(20, 4 · top_k)` ordinary members are checked. -/
def window (topK : Nat) : Nat := min windowMax (4 * topK)

/-- The hit `s` is redundant with, if any: the first of the first `wmax`
emitted members that the verdict finds `s` redundant with (`find_map` over
`taken`). -/
def keptBy (red : ι → ι → Bool) (wmax : Nat) (prev : List ι) (s : ι) : Option ι :=
  (prev.take wmax).find? (fun t => red t s)

theorem keptBy_spec {red : ι → ι → Bool} {wmax : Nat} {prev : List ι} {s k : ι}
    (h : keptBy red wmax prev s = some k) : k ∈ prev ∧ red k s = true := by
  unfold keptBy at h
  have hp := List.find?_some h
  exact ⟨List.mem_of_mem_take (List.mem_of_find?_eq_some h), hp⟩

/-- `emitted` with the redundancy skip, from a state with `slots` slots used,
`checked` members checked and `prev` emitted. -/
def emitR (isConcept forced warns : ι → Bool) (red : ι → ι → Bool) (topK win wmax : Nat) :
    Nat → Nat → List ι → List ι → List ι
  | _, _, _, [] => []
  | slots, checked, prev, s :: rest =>
    if isConcept s = false then emitR isConcept forced warns red topK win wmax slots checked prev rest
    else if forced s = true then
      s :: emitR isConcept forced warns red topK win wmax slots checked (prev ++ [s]) rest
    else if slots < topK then
      if checked < win then
        match keptBy red wmax prev s with
        | some _ =>
          if warns s = false then
            emitR isConcept forced warns red topK win wmax slots (checked + 1) prev rest
          else
            s :: emitR isConcept forced warns red topK win wmax (slots + 1) (checked + 1)
              (prev ++ [s]) rest
        | none =>
          s :: emitR isConcept forced warns red topK win wmax (slots + 1) (checked + 1)
            (prev ++ [s]) rest
      else s :: emitR isConcept forced warns red topK win wmax (slots + 1) checked (prev ++ [s]) rest
    else emitR isConcept forced warns red topK win wmax slots checked prev rest

/-- The skips `emitted` records, in scan order: `(kept, skipped)`, the hit a
member was skipped as redundant with and the member (`Emission::skipped`,
flattened). -/
def skipsR (isConcept forced warns : ι → Bool) (red : ι → ι → Bool) (topK win wmax : Nat) :
    Nat → Nat → List ι → List ι → List (ι × ι)
  | _, _, _, [] => []
  | slots, checked, prev, s :: rest =>
    if isConcept s = false then skipsR isConcept forced warns red topK win wmax slots checked prev rest
    else if forced s = true then
      skipsR isConcept forced warns red topK win wmax slots checked (prev ++ [s]) rest
    else if slots < topK then
      if checked < win then
        match keptBy red wmax prev s with
        | some k =>
          if warns s = false then
            (k, s) :: skipsR isConcept forced warns red topK win wmax slots (checked + 1) prev rest
          else
            skipsR isConcept forced warns red topK win wmax (slots + 1) (checked + 1)
              (prev ++ [s]) rest
        | none =>
          skipsR isConcept forced warns red topK win wmax (slots + 1) (checked + 1)
            (prev ++ [s]) rest
      else skipsR isConcept forced warns red topK win wmax (slots + 1) checked (prev ++ [s]) rest
    else skipsR isConcept forced warns red topK win wmax slots checked prev rest

section RedundancyUnfold

variable {isConcept forced warns : ι → Bool} {red : ι → ι → Bool} {topK win wmax : Nat}
  {s : ι} {slots checked : Nat} {prev : List ι}

/-- Not skipped: no taken hit finds `s` redundant, or `s` warns. -/
theorem keep_of_not_skip
    (h : ¬ ∃ k, keptBy red wmax prev s = some k ∧ warns s = false) :
    ∀ k, keptBy red wmax prev s = some k → warns s = true := by
  intro k hk
  cases hw : warns s
  · exact absurd ⟨k, hk, hw⟩ h
  · rfl

theorem emitR_nil' :
    emitR isConcept forced warns red topK win wmax slots checked prev [] = [] := by
  simp [emitR]

theorem skipsR_nil' :
    skipsR isConcept forced warns red topK win wmax slots checked prev [] = [] := by
  simp [skipsR]

theorem emitR_missing (hc : isConcept s = false) (rest : List ι) :
    emitR isConcept forced warns red topK win wmax slots checked prev (s :: rest)
      = emitR isConcept forced warns red topK win wmax slots checked prev rest := by
  simp [emitR, hc]

theorem skipsR_missing (hc : isConcept s = false) (rest : List ι) :
    skipsR isConcept forced warns red topK win wmax slots checked prev (s :: rest)
      = skipsR isConcept forced warns red topK win wmax slots checked prev rest := by
  simp [skipsR, hc]

theorem emitR_forced (hc : isConcept s = true) (hf : forced s = true) (rest : List ι) :
    emitR isConcept forced warns red topK win wmax slots checked prev (s :: rest)
      = s :: emitR isConcept forced warns red topK win wmax slots checked (prev ++ [s]) rest := by
  simp [emitR, hc, hf]

theorem skipsR_forced (hc : isConcept s = true) (hf : forced s = true) (rest : List ι) :
    skipsR isConcept forced warns red topK win wmax slots checked prev (s :: rest)
      = skipsR isConcept forced warns red topK win wmax slots checked (prev ++ [s]) rest := by
  simp [skipsR, hc, hf]

theorem emitR_cut (hc : isConcept s = true) (hf : forced s = false) (hs : ¬ slots < topK)
    (rest : List ι) :
    emitR isConcept forced warns red topK win wmax slots checked prev (s :: rest)
      = emitR isConcept forced warns red topK win wmax slots checked prev rest := by
  simp [emitR, hc, hf, hs]

theorem skipsR_cut (hc : isConcept s = true) (hf : forced s = false) (hs : ¬ slots < topK)
    (rest : List ι) :
    skipsR isConcept forced warns red topK win wmax slots checked prev (s :: rest)
      = skipsR isConcept forced warns red topK win wmax slots checked prev rest := by
  simp [skipsR, hc, hf, hs]

theorem emitR_skip (hc : isConcept s = true) (hf : forced s = false) (hs : slots < topK)
    (hw : checked < win) {k : ι} (hk : keptBy red wmax prev s = some k) (hn : warns s = false)
    (rest : List ι) :
    emitR isConcept forced warns red topK win wmax slots checked prev (s :: rest)
      = emitR isConcept forced warns red topK win wmax slots (checked + 1) prev rest := by
  simp [emitR, hc, hf, hs, hw, hk, hn]

theorem skipsR_skip (hc : isConcept s = true) (hf : forced s = false) (hs : slots < topK)
    (hw : checked < win) {k : ι} (hk : keptBy red wmax prev s = some k) (hn : warns s = false)
    (rest : List ι) :
    skipsR isConcept forced warns red topK win wmax slots checked prev (s :: rest)
      = (k, s) :: skipsR isConcept forced warns red topK win wmax slots (checked + 1) prev rest := by
  simp [skipsR, hc, hf, hs, hw, hk, hn]

theorem emitR_keep (hc : isConcept s = true) (hf : forced s = false) (hs : slots < topK)
    (hw : checked < win) (hkeep : ∀ k, keptBy red wmax prev s = some k → warns s = true)
    (rest : List ι) :
    emitR isConcept forced warns red topK win wmax slots checked prev (s :: rest)
      = s :: emitR isConcept forced warns red topK win wmax (slots + 1) (checked + 1)
          (prev ++ [s]) rest := by
  cases hk : keptBy red wmax prev s with
  | none => simp [emitR, hc, hf, hs, hw, hk]
  | some k => simp [emitR, hc, hf, hs, hw, hk, hkeep k hk]

theorem skipsR_keep (hc : isConcept s = true) (hf : forced s = false) (hs : slots < topK)
    (hw : checked < win) (hkeep : ∀ k, keptBy red wmax prev s = some k → warns s = true)
    (rest : List ι) :
    skipsR isConcept forced warns red topK win wmax slots checked prev (s :: rest)
      = skipsR isConcept forced warns red topK win wmax (slots + 1) (checked + 1)
          (prev ++ [s]) rest := by
  cases hk : keptBy red wmax prev s with
  | none => simp [skipsR, hc, hf, hs, hw, hk]
  | some k => simp [skipsR, hc, hf, hs, hw, hk, hkeep k hk]

theorem emitR_late (hc : isConcept s = true) (hf : forced s = false) (hs : slots < topK)
    (hw : ¬ checked < win) (rest : List ι) :
    emitR isConcept forced warns red topK win wmax slots checked prev (s :: rest)
      = s :: emitR isConcept forced warns red topK win wmax (slots + 1) checked (prev ++ [s]) rest := by
  simp [emitR, hc, hf, hs, hw]

theorem skipsR_late (hc : isConcept s = true) (hf : forced s = false) (hs : slots < topK)
    (hw : ¬ checked < win) (rest : List ι) :
    skipsR isConcept forced warns red topK win wmax slots checked prev (s :: rest)
      = skipsR isConcept forced warns red topK win wmax (slots + 1) checked (prev ++ [s]) rest := by
  simp [skipsR, hc, hf, hs, hw]

end RedundancyUnfold

theorem ordinary_cons_missing {isConcept forced : ι → Bool} {s : ι} (hc : isConcept s = false)
    (rest : List ι) : ordinary isConcept forced (s :: rest) = ordinary isConcept forced rest := by
  simp [ordinary, hc]

theorem ordinary_cons_forced {isConcept forced : ι → Bool} {s : ι} (hf : forced s = true)
    (rest : List ι) : ordinary isConcept forced (s :: rest) = ordinary isConcept forced rest := by
  simp [ordinary, hf]

theorem ordinary_cons_free {isConcept forced : ι → Bool} {s : ι} (hc : isConcept s = true)
    (hf : forced s = false) (rest : List ι) :
    ordinary isConcept forced (s :: rest) = s :: ordinary isConcept forced rest := by
  simp [ordinary, hc, hf]

section Redundancy

variable (isConcept forced warns : ι → Bool) (red : ι → ι → Bool) (topK win wmax : Nat)

/-- **Order kept.** With the skip, the emitted list is still a subsequence of
the ranked list: the rule only skips ("Canonical-first order is untouched"). -/
theorem emitR_sublist :
    ∀ (ranked : List ι) (slots checked : Nat) (prev : List ι),
      List.Sublist (emitR isConcept forced warns red topK win wmax slots checked prev ranked)
        ranked := by
  intro ranked
  induction ranked with
  | nil =>
    intro slots checked prev
    rw [emitR_nil']
    exact List.Sublist.slnil
  | cons s rest ih =>
    intro slots checked prev
    cases hc : isConcept s with
    | false =>
      rw [emitR_missing hc]
      exact (ih _ _ _).cons s
    | true =>
      cases hf : forced s with
      | true =>
        rw [emitR_forced hc hf]
        exact (ih _ _ _).cons_cons s
      | false =>
        by_cases hs : slots < topK
        · by_cases hw : checked < win
          · by_cases hskip : ∃ k, keptBy red wmax prev s = some k ∧ warns s = false
            · obtain ⟨k, hk, hn⟩ := hskip
              rw [emitR_skip hc hf hs hw hk hn]
              exact (ih _ _ _).cons s
            · rw [emitR_keep hc hf hs hw (keep_of_not_skip hskip)]
              exact (ih _ _ _).cons_cons s
          · rw [emitR_late hc hf hs hw]
            exact (ih _ _ _).cons_cons s
        · rw [emitR_cut hc hf hs]
          exact (ih _ _ _).cons s

theorem emitR_pairwise {R : ι → ι → Prop} {ranked : List ι} (h : ranked.Pairwise R)
    (slots checked : Nat) (prev : List ι) :
    (emitR isConcept forced warns red topK win wmax slots checked prev ranked).Pairwise R :=
  List.Pairwise.sublist (emitR_sublist isConcept forced warns red topK win wmax ranked slots
    checked prev) h

/-- **Forced members are still never cut**, and never skipped. -/
theorem forced_mem_emitR (x : ι) (hcx : isConcept x = true) (hfx : forced x = true) :
    ∀ (ranked : List ι) (slots checked : Nat) (prev : List ι), x ∈ ranked →
      x ∈ emitR isConcept forced warns red topK win wmax slots checked prev ranked := by
  intro ranked
  induction ranked with
  | nil =>
    intro slots checked prev hx
    simp at hx
  | cons s rest ih =>
    intro slots checked prev hx
    -- `s` is emitted, or `x` is in `rest`.
    have tail : ∀ {sl ch : Nat} {pv : List ι} {out : List ι},
        (x ≠ s) → out = emitR isConcept forced warns red topK win wmax sl ch pv rest →
        x ∈ out := by
      intro sl ch pv out hne e
      cases List.mem_cons.mp hx with
      | inl heq => exact absurd heq hne
      | inr hmem => rw [e]; exact ih sl ch pv hmem
    have head : ∀ {sl ch : Nat} {pv : List ι},
        x ∈ s :: emitR isConcept forced warns red topK win wmax sl ch pv rest := by
      intro sl ch pv
      cases List.mem_cons.mp hx with
      | inl heq => exact List.mem_cons.mpr (Or.inl heq)
      | inr hmem => exact List.mem_cons.mpr (Or.inr (ih sl ch pv hmem))
    cases hc : isConcept s with
    | false =>
      rw [emitR_missing hc]
      exact tail (fun heq => by rw [heq, hc] at hcx; exact absurd hcx (by decide)) rfl
    | true =>
      cases hf : forced s with
      | true =>
        rw [emitR_forced hc hf]
        exact head
      | false =>
        have hne : x ≠ s := fun heq => by rw [heq, hf] at hfx; exact absurd hfx (by decide)
        by_cases hs : slots < topK
        · by_cases hw : checked < win
          · by_cases hskip : ∃ k, keptBy red wmax prev s = some k ∧ warns s = false
            · obtain ⟨k, hk, hn⟩ := hskip
              rw [emitR_skip hc hf hs hw hk hn]
              exact tail hne rfl
            · rw [emitR_keep hc hf hs hw (keep_of_not_skip hskip)]
              exact head
          · rw [emitR_late hc hf hs hw]
            exact head
        · rw [emitR_cut hc hf hs]
          exact tail hne rfl

/-- Everything emitted is a concept of the graph. -/
theorem emitR_all_concepts (x : ι) :
    ∀ (ranked : List ι) (slots checked : Nat) (prev : List ι),
      x ∈ emitR isConcept forced warns red topK win wmax slots checked prev ranked →
        isConcept x = true := by
  intro ranked
  induction ranked with
  | nil =>
    intro slots checked prev hx
    rw [emitR_nil'] at hx
    simp at hx
  | cons s rest ih =>
    intro slots checked prev hx
    have head : ∀ {sl ch : Nat} {pv : List ι}, isConcept s = true →
        x ∈ s :: emitR isConcept forced warns red topK win wmax sl ch pv rest →
        isConcept x = true := by
      intro sl ch pv hc h
      cases List.mem_cons.mp h with
      | inl heq => rw [heq]; exact hc
      | inr hmem => exact ih sl ch pv hmem
    cases hc : isConcept s with
    | false =>
      rw [emitR_missing hc] at hx
      exact ih _ _ _ hx
    | true =>
      cases hf : forced s with
      | true =>
        rw [emitR_forced hc hf] at hx
        exact head hc hx
      | false =>
        by_cases hs : slots < topK
        · by_cases hw : checked < win
          · by_cases hskip : ∃ k, keptBy red wmax prev s = some k ∧ warns s = false
            · obtain ⟨k, hk, hn⟩ := hskip
              rw [emitR_skip hc hf hs hw hk hn] at hx
              exact ih _ _ _ hx
            · rw [emitR_keep hc hf hs hw (keep_of_not_skip hskip)] at hx
              exact head hc hx
          · rw [emitR_late hc hf hs hw] at hx
            exact head hc hx
        · rw [emitR_cut hc hf hs] at hx
          exact ih _ _ _ hx

/-- The slot accounting, as one invariant: ordinary members emitted plus
members skipped never exceed the ordinary concepts; emitted ones fit in the
slots left; and either the slots fill up or every ordinary concept was
emitted or skipped. -/
theorem emitR_count :
    ∀ (ranked : List ι) (slots checked : Nat) (prev : List ι), slots ≤ topK →
      freeCount forced (emitR isConcept forced warns red topK win wmax slots checked prev ranked)
          + (skipsR isConcept forced warns red topK win wmax slots checked prev ranked).length
          ≤ (ordinary isConcept forced ranked).length
      ∧ freeCount forced (emitR isConcept forced warns red topK win wmax slots checked prev ranked)
          + slots ≤ topK
      ∧ (freeCount forced (emitR isConcept forced warns red topK win wmax slots checked prev ranked)
            + slots = topK
          ∨ freeCount forced
              (emitR isConcept forced warns red topK win wmax slots checked prev ranked)
            + (skipsR isConcept forced warns red topK win wmax slots checked prev ranked).length
            = (ordinary isConcept forced ranked).length) := by
  intro ranked
  induction ranked with
  | nil =>
    intro slots checked prev hle
    rw [emitR_nil', skipsR_nil']
    simp [freeCount, ordinary]
    omega
  | cons s rest ih =>
    intro slots checked prev hle
    cases hc : isConcept s with
    | false =>
      rw [emitR_missing hc, skipsR_missing hc, ordinary_cons_missing hc]
      exact ih slots checked prev hle
    | true =>
      cases hf : forced s with
      | true =>
        rw [emitR_forced hc hf, skipsR_forced hc hf, ordinary_cons_forced hf,
          freeCount_cons_forced hf]
        exact ih slots checked _ hle
      | false =>
        rw [ordinary_cons_free hc hf, List.length_cons]
        by_cases hs : slots < topK
        · by_cases hw : checked < win
          · by_cases hskip : ∃ k, keptBy red wmax prev s = some k ∧ warns s = false
            · obtain ⟨k, hk, hn⟩ := hskip
              rw [emitR_skip hc hf hs hw hk hn, skipsR_skip hc hf hs hw hk hn, List.length_cons]
              have := ih slots (checked + 1) prev hle
              omega
            · rw [emitR_keep hc hf hs hw (keep_of_not_skip hskip),
                skipsR_keep hc hf hs hw (keep_of_not_skip hskip), freeCount_cons_free hf]
              have := ih (slots + 1) (checked + 1) (prev ++ [s]) (by omega)
              omega
          · rw [emitR_late hc hf hs hw, skipsR_late hc hf hs hw, freeCount_cons_free hf]
            have := ih (slots + 1) checked (prev ++ [s]) (by omega)
            omega
        · rw [emitR_cut hc hf hs, skipsR_cut hc hf hs]
          have := ih slots checked prev hle
          omega

/-- **At most `top_k` ordinary hits**, with the skip on. -/
theorem emitR_free_le_topK (ranked : List ι) :
    freeCount forced (emitR isConcept forced warns red topK win wmax 0 0 [] ranked) ≤ topK := by
  have := (emitR_count isConcept forced warns red topK win wmax ranked 0 0 [] (Nat.zero_le _)).2.1
  omega

/-- **A skipped member takes no slot: the next distinct member fills it.** The
ordinary hits number `min top_k (ordinary − skipped)`: every ordinary concept
not skipped competes for the `top_k` slots as if the skipped ones were not in
the list. -/
theorem emitR_free_count (ranked : List ι) :
    freeCount forced (emitR isConcept forced warns red topK win wmax 0 0 [] ranked)
      = min topK ((ordinary isConcept forced ranked).length
          - (skipsR isConcept forced warns red topK win wmax 0 0 [] ranked).length) := by
  have := emitR_count isConcept forced warns red topK win wmax ranked 0 0 [] (Nat.zero_le _)
  omega

/-- **Agreement.** When nothing is skipped, the walk takes exactly the members
it takes without the rule ("When nothing is skipped the result is exactly the
one without it"; "byte-identical", module doc of `redundancy.rs`). -/
theorem emitR_eq_emit_of_skipsR_nil :
    ∀ (ranked : List ι) (slots checked : Nat) (prev : List ι),
      skipsR isConcept forced warns red topK win wmax slots checked prev ranked = [] →
      emitR isConcept forced warns red topK win wmax slots checked prev ranked
        = emit isConcept forced topK slots ranked := by
  intro ranked
  induction ranked with
  | nil =>
    intro slots checked prev _
    rw [emitR_nil', emit_nil]
  | cons s rest ih =>
    intro slots checked prev h
    cases hc : isConcept s with
    | false =>
      rw [skipsR_missing hc] at h
      rw [emitR_missing hc, emit_missing hc, ih _ _ _ h]
    | true =>
      cases hf : forced s with
      | true =>
        rw [skipsR_forced hc hf] at h
        rw [emitR_forced hc hf, emit_forced hc hf, ih _ _ _ h]
      | false =>
        by_cases hs : slots < topK
        · by_cases hw : checked < win
          · by_cases hskip : ∃ k, keptBy red wmax prev s = some k ∧ warns s = false
            · obtain ⟨k, hk, hn⟩ := hskip
              rw [skipsR_skip hc hf hs hw hk hn] at h
              exact absurd h (List.cons_ne_nil _ _)
            · rw [skipsR_keep hc hf hs hw (keep_of_not_skip hskip)] at h
              rw [emitR_keep hc hf hs hw (keep_of_not_skip hskip), emit_take hc hf hs, ih _ _ _ h]
          · rw [skipsR_late hc hf hs hw] at h
            rw [emitR_late hc hf hs hw, emit_take hc hf hs, ih _ _ _ h]
        · rw [skipsR_cut hc hf hs] at h
          rw [emitR_cut hc hf hs, emit_cut hc hf hs, ih _ _ _ h]

/-- Nothing is skipped when no verdict between an emitted-or-ranked member and
a later ranked one is redundant. The hypothesis covers a member against
itself too, which the Rust verdict satisfies (`Redundancy::signal` returns
`None` when `a.id == b.id`); `skipsR_nil_of_no_red_distinct` drops that
case. -/
theorem skipsR_nil_of_no_red :
    ∀ (ranked : List ι) (slots checked : Nat) (prev : List ι),
      (∀ a b, a ∈ prev ++ ranked → b ∈ ranked → red a b = false) →
      skipsR isConcept forced warns red topK win wmax slots checked prev ranked = [] := by
  intro ranked
  induction ranked with
  | nil =>
    intro slots checked prev _
    rw [skipsR_nil']
  | cons s rest ih =>
    intro slots checked prev h
    have same : ∀ a b, a ∈ prev ++ rest → b ∈ rest → red a b = false := by
      intro a b ha hb
      apply h a b _ (List.mem_cons_of_mem s hb)
      cases List.mem_append.mp ha with
      | inl h' => exact List.mem_append_left _ h'
      | inr h' => exact List.mem_append_right _ (List.mem_cons_of_mem s h')
    have grown : ∀ a b, a ∈ (prev ++ [s]) ++ rest → b ∈ rest → red a b = false := by
      intro a b ha hb
      apply h a b _ (List.mem_cons_of_mem s hb)
      rw [List.append_assoc, List.singleton_append] at ha
      exact ha
    cases hc : isConcept s with
    | false =>
      rw [skipsR_missing hc]
      exact ih _ _ _ same
    | true =>
      cases hf : forced s with
      | true =>
        rw [skipsR_forced hc hf]
        exact ih _ _ _ grown
      | false =>
        by_cases hs : slots < topK
        · by_cases hw : checked < win
          · by_cases hskip : ∃ k, keptBy red wmax prev s = some k ∧ warns s = false
            · obtain ⟨k, hk, _⟩ := hskip
              have ⟨hkp, hred⟩ := keptBy_spec hk
              have := h k s (List.mem_append_left _ hkp) (List.mem_cons_self)
              rw [hred] at this
              exact absurd this (by decide)
            · rw [skipsR_keep hc hf hs hw (keep_of_not_skip hskip)]
              exact ih _ _ _ grown
          · rw [skipsR_late hc hf hs hw]
            exact ih _ _ _ grown
        · rw [skipsR_cut hc hf hs]
          exact ih _ _ _ same

/-- **Agreement with the no-skip model.** With no redundant pair among the
ranked members, the skip rule changes nothing. -/
theorem emitR_eq_emit_of_no_redundant_pair (ranked : List ι)
    (h : ∀ a ∈ ranked, ∀ b ∈ ranked, red a b = false) :
    emitR isConcept forced warns red topK win wmax 0 0 [] ranked
      = emit isConcept forced topK 0 ranked :=
  emitR_eq_emit_of_skipsR_nil isConcept forced warns red topK win wmax ranked 0 0 []
    (skipsR_nil_of_no_red isConcept forced warns red topK win wmax ranked 0 0 []
      (fun a b ha hb => h a (by simpa using ha) b hb))

/-- Nothing is skipped when no verdict between two *distinct* members, an
emitted-or-ranked one and a later ranked one, is redundant, given unique ids.
The verdict of a member against itself is not constrained (the Rust never
asks it: `Redundancy::signal` returns `None` when `a.id == b.id`, and the
emitted prefix and the rest are disjoint). -/
theorem skipsR_nil_of_no_red_distinct :
    ∀ (ranked : List ι) (slots checked : Nat) (prev : List ι), (prev ++ ranked).Nodup →
      (∀ a b, a ∈ prev ++ ranked → b ∈ ranked → a ≠ b → red a b = false) →
      skipsR isConcept forced warns red topK win wmax slots checked prev ranked = [] := by
  intro ranked
  induction ranked with
  | nil =>
    intro slots checked prev _ _
    rw [skipsR_nil']
  | cons s rest ih =>
    intro slots checked prev hnd h
    have hndSame : (prev ++ rest).Nodup :=
      List.Nodup.sublist (List.Sublist.append_left (List.sublist_cons_self s rest) prev) hnd
    have hndGrown : (prev ++ [s] ++ rest).Nodup := by
      rw [List.append_assoc, List.singleton_append]
      exact hnd
    have same : ∀ a b, a ∈ prev ++ rest → b ∈ rest → a ≠ b → red a b = false := by
      intro a b ha hb hab
      apply h a b _ (List.mem_cons_of_mem s hb) hab
      cases List.mem_append.mp ha with
      | inl h' => exact List.mem_append_left _ h'
      | inr h' => exact List.mem_append_right _ (List.mem_cons_of_mem s h')
    have grown : ∀ a b, a ∈ (prev ++ [s]) ++ rest → b ∈ rest → a ≠ b → red a b = false := by
      intro a b ha hb hab
      apply h a b _ (List.mem_cons_of_mem s hb) hab
      rw [List.append_assoc, List.singleton_append] at ha
      exact ha
    cases hc : isConcept s with
    | false =>
      rw [skipsR_missing hc]
      exact ih _ _ _ hndSame same
    | true =>
      cases hf : forced s with
      | true =>
        rw [skipsR_forced hc hf]
        exact ih _ _ _ hndGrown grown
      | false =>
        by_cases hs : slots < topK
        · by_cases hw : checked < win
          · by_cases hskip : ∃ k, keptBy red wmax prev s = some k ∧ warns s = false
            · obtain ⟨k, hk, _⟩ := hskip
              have ⟨hkp, hred⟩ := keptBy_spec hk
              have hks : k ≠ s := by
                intro e
                rw [e] at hkp
                have := (List.nodup_append.mp hnd).2.2 s hkp s List.mem_cons_self
                exact this rfl
              have := h k s (List.mem_append_left _ hkp) List.mem_cons_self hks
              rw [hred] at this
              exact absurd this (by decide)
            · rw [skipsR_keep hc hf hs hw (keep_of_not_skip hskip)]
              exact ih _ _ _ hndGrown grown
          · rw [skipsR_late hc hf hs hw]
            exact ih _ _ _ hndGrown grown
        · rw [skipsR_cut hc hf hs]
          exact ih _ _ _ hndSame same

/-- **Agreement with the no-skip model, distinct pairs only.** With unique ids
(`Expand.expand_nodup`) and no redundant pair of distinct ranked members, the
skip rule changes nothing. -/
theorem emitR_eq_emit_of_no_redundant_distinct_pair (ranked : List ι) (hnd : ranked.Nodup)
    (h : ∀ a ∈ ranked, ∀ b ∈ ranked, a ≠ b → red a b = false) :
    emitR isConcept forced warns red topK win wmax 0 0 [] ranked
      = emit isConcept forced topK 0 ranked :=
  emitR_eq_emit_of_skipsR_nil isConcept forced warns red topK win wmax ranked 0 0 []
    (skipsR_nil_of_no_red_distinct isConcept forced warns red topK win wmax ranked 0 0 []
      (by simpa using hnd) (fun a b ha hb => h a (by simpa using ha) b hb))

/-- **Past the window nothing is checked.** At most `window` members are
skipped. -/
theorem skipsR_length_le :
    ∀ (ranked : List ι) (slots checked : Nat) (prev : List ι), checked ≤ win →
      (skipsR isConcept forced warns red topK win wmax slots checked prev ranked).length
        + checked ≤ win := by
  intro ranked
  induction ranked with
  | nil =>
    intro slots checked prev hle
    rw [skipsR_nil']
    simpa using hle
  | cons s rest ih =>
    intro slots checked prev hle
    cases hc : isConcept s with
    | false =>
      rw [skipsR_missing hc]
      exact ih _ _ _ hle
    | true =>
      cases hf : forced s with
      | true =>
        rw [skipsR_forced hc hf]
        exact ih _ _ _ hle
      | false =>
        by_cases hs : slots < topK
        · by_cases hw : checked < win
          · by_cases hskip : ∃ k, keptBy red wmax prev s = some k ∧ warns s = false
            · obtain ⟨k, hk, hn⟩ := hskip
              rw [skipsR_skip hc hf hs hw hk hn, List.length_cons]
              have := ih slots (checked + 1) prev (by omega)
              omega
            · rw [skipsR_keep hc hf hs hw (keep_of_not_skip hskip)]
              have := ih (slots + 1) (checked + 1) (prev ++ [s]) (by omega)
              omega
          · rw [skipsR_late hc hf hs hw]
            exact ih _ _ _ hle
        · rw [skipsR_cut hc hf hs]
          exact ih _ _ _ hle

/-- With no window (`selection = None`, or `top_k = 0`) nothing is skipped and
the result is the no-skip model's. -/
theorem emitR_window_zero (ranked : List ι) :
    emitR isConcept forced warns red topK 0 wmax 0 0 [] ranked
      = emit isConcept forced topK 0 ranked := by
  apply emitR_eq_emit_of_skipsR_nil
  have := skipsR_length_le isConcept forced warns red topK 0 wmax ranked 0 0 [] (Nat.le_refl 0)
  exact List.eq_nil_of_length_eq_zero (by omega)

/-- **What a skip is.** A skipped member is a graph concept, not forced, would
carry no warning line of its own, and the verdict finds it redundant with the
hit named as kept. -/
theorem skipsR_spec (k x : ι) :
    ∀ (ranked : List ι) (slots checked : Nat) (prev : List ι),
      (k, x) ∈ skipsR isConcept forced warns red topK win wmax slots checked prev ranked →
      isConcept x = true ∧ forced x = false ∧ warns x = false ∧ red k x = true := by
  intro ranked
  induction ranked with
  | nil =>
    intro slots checked prev h
    rw [skipsR_nil'] at h
    simp at h
  | cons s rest ih =>
    intro slots checked prev h
    cases hc : isConcept s with
    | false =>
      rw [skipsR_missing hc] at h
      exact ih _ _ _ h
    | true =>
      cases hf : forced s with
      | true =>
        rw [skipsR_forced hc hf] at h
        exact ih _ _ _ h
      | false =>
        by_cases hs : slots < topK
        · by_cases hw : checked < win
          · by_cases hskip : ∃ k, keptBy red wmax prev s = some k ∧ warns s = false
            · obtain ⟨k', hk, hn⟩ := hskip
              rw [skipsR_skip hc hf hs hw hk hn] at h
              cases List.mem_cons.mp h with
              | inl heq =>
                have e1 : k = k' := (Prod.mk.inj heq).1
                have e2 : x = s := (Prod.mk.inj heq).2
                subst e1 e2
                exact ⟨hc, hf, hn, (keptBy_spec hk).2⟩
              | inr hmem => exact ih _ _ _ hmem
            · rw [skipsR_keep hc hf hs hw (keep_of_not_skip hskip)] at h
              exact ih _ _ _ h
          · rw [skipsR_late hc hf hs hw] at h
            exact ih _ _ _ h
        · rw [skipsR_cut hc hf hs] at h
          exact ih _ _ _ h

/-- The hit a member is skipped for was emitted before it. -/
theorem skipsR_kept_emitted (k x : ι) :
    ∀ (ranked : List ι) (slots checked : Nat) (prev : List ι),
      (k, x) ∈ skipsR isConcept forced warns red topK win wmax slots checked prev ranked →
      k ∈ prev ∨ k ∈ emitR isConcept forced warns red topK win wmax slots checked prev ranked := by
  intro ranked
  induction ranked with
  | nil =>
    intro slots checked prev h
    rw [skipsR_nil'] at h
    simp at h
  | cons s rest ih =>
    intro slots checked prev h
    have grow : ∀ {sl ch : Nat},
        (k ∈ prev ++ [s] ∨ k ∈ emitR isConcept forced warns red topK win wmax sl ch
          (prev ++ [s]) rest) →
        k ∈ prev ∨ k ∈ s :: emitR isConcept forced warns red topK win wmax sl ch
          (prev ++ [s]) rest := by
      intro sl ch hk
      cases hk with
      | inl h' =>
        cases List.mem_append.mp h' with
        | inl h'' => exact Or.inl h''
        | inr h'' => exact Or.inr (List.mem_cons.mpr (Or.inl (List.mem_singleton.mp h'')))
      | inr h' => exact Or.inr (List.mem_cons_of_mem s h')
    cases hc : isConcept s with
    | false =>
      rw [skipsR_missing hc] at h
      rw [emitR_missing hc]
      exact ih _ _ _ h
    | true =>
      cases hf : forced s with
      | true =>
        rw [skipsR_forced hc hf] at h
        rw [emitR_forced hc hf]
        exact grow (ih _ _ _ h)
      | false =>
        by_cases hs : slots < topK
        · by_cases hw : checked < win
          · by_cases hskip : ∃ k, keptBy red wmax prev s = some k ∧ warns s = false
            · obtain ⟨k', hk, hn⟩ := hskip
              rw [skipsR_skip hc hf hs hw hk hn] at h
              rw [emitR_skip hc hf hs hw hk hn]
              cases List.mem_cons.mp h with
              | inl heq =>
                have e1 : k = k' := (Prod.mk.inj heq).1
                subst e1
                exact Or.inl (keptBy_spec hk).1
              | inr hmem => exact ih _ _ _ hmem
            · rw [skipsR_keep hc hf hs hw (keep_of_not_skip hskip)] at h
              rw [emitR_keep hc hf hs hw (keep_of_not_skip hskip)]
              exact grow (ih _ _ _ h)
          · rw [skipsR_late hc hf hs hw] at h
            rw [emitR_late hc hf hs hw]
            exact grow (ih _ _ _ h)
        · rw [skipsR_cut hc hf hs] at h
          rw [emitR_cut hc hf hs]
          exact ih _ _ _ h

/-- **The kept copy is ranked ahead of the skipped one.** A skipped member sits
at some position of the ranked list, and the hit it was skipped for was
already emitted before the scan started or sits before that position. -/
theorem skipsR_kept_before (k x : ι) :
    ∀ (ranked : List ι) (slots checked : Nat) (prev : List ι),
      (k, x) ∈ skipsR isConcept forced warns red topK win wmax slots checked prev ranked →
      ∃ pre post, ranked = pre ++ x :: post ∧ (k ∈ prev ∨ k ∈ pre) := by
  intro ranked
  induction ranked with
  | nil =>
    intro slots checked prev h
    rw [skipsR_nil'] at h
    simp at h
  | cons s rest ih =>
    intro slots checked prev h
    have shift : ∀ {pv : List ι}, (∀ a, a ∈ pv → a ∈ prev ∨ a = s) →
        (∃ pre post, rest = pre ++ x :: post ∧ (k ∈ pv ∨ k ∈ pre)) →
        ∃ pre post, s :: rest = pre ++ x :: post ∧ (k ∈ prev ∨ k ∈ pre) := by
      intro pv hpv ⟨pre, post, e, hk⟩
      refine ⟨s :: pre, post, by rw [e]; rfl, ?_⟩
      cases hk with
      | inl h' =>
        cases hpv k h' with
        | inl h'' => exact Or.inl h''
        | inr h'' => exact Or.inr (List.mem_cons.mpr (Or.inl h''))
      | inr h' => exact Or.inr (List.mem_cons_of_mem s h')
    have same : ∀ a, a ∈ prev → a ∈ prev ∨ a = s := fun a h => Or.inl h
    have grown : ∀ a, a ∈ prev ++ [s] → a ∈ prev ∨ a = s := by
      intro a h
      cases List.mem_append.mp h with
      | inl h' => exact Or.inl h'
      | inr h' => exact Or.inr (List.mem_singleton.mp h')
    cases hc : isConcept s with
    | false =>
      rw [skipsR_missing hc] at h
      exact shift same (ih _ _ _ h)
    | true =>
      cases hf : forced s with
      | true =>
        rw [skipsR_forced hc hf] at h
        exact shift grown (ih _ _ _ h)
      | false =>
        by_cases hs : slots < topK
        · by_cases hw : checked < win
          · by_cases hskip : ∃ k, keptBy red wmax prev s = some k ∧ warns s = false
            · obtain ⟨k', hk, hn⟩ := hskip
              rw [skipsR_skip hc hf hs hw hk hn] at h
              cases List.mem_cons.mp h with
              | inl heq =>
                have e1 : k = k' := (Prod.mk.inj heq).1
                have e2 : x = s := (Prod.mk.inj heq).2
                subst e1 e2
                exact ⟨[], rest, rfl, Or.inl (keptBy_spec hk).1⟩
              | inr hmem => exact shift same (ih _ _ _ hmem)
            · rw [skipsR_keep hc hf hs hw (keep_of_not_skip hskip)] at h
              exact shift grown (ih _ _ _ h)
          · rw [skipsR_late hc hf hs hw] at h
            exact shift grown (ih _ _ _ h)
        · rw [skipsR_cut hc hf hs] at h
          exact shift same (ih _ _ _ h)

/-- **Which copy is kept.** Any ordering property of the ranked list holds from
the kept hit to the member skipped for it: of a redundant pair, the one
ranked first is kept (under `rank_members`, the higher score, then the
tie-break: the Rust test
`equal_scores_keep_the_copy_the_tie_break_ranks_first`). -/
theorem skipsR_pairwise {R : ι → ι → Prop} {ranked : List ι} (h : ranked.Pairwise R)
    {k x : ι}
    (hkx : (k, x) ∈ skipsR isConcept forced warns red topK win wmax 0 0 [] ranked) : R k x := by
  obtain ⟨pre, post, e, hk⟩ :=
    skipsR_kept_before isConcept forced warns red topK win wmax k x ranked 0 0 [] hkx
  have hk' : k ∈ pre := by
    cases hk with
    | inl h' => simp at h'
    | inr h' => exact h'
  rw [e] at h
  exact (List.pairwise_append.mp h).2.2 k hk' x List.mem_cons_self

/-- **A skipped member is not emitted** (ids are unique in the ranked list, as
`expand` guarantees). The companion step after `emitted` can still bring it
back as a contradiction companion (`assemble.rs`, the `retain` on the skip
lists); that step is not modelled. -/
theorem skipsR_not_emitted (k x : ι) :
    ∀ (ranked : List ι) (slots checked : Nat) (prev : List ι), ranked.Nodup →
      (k, x) ∈ skipsR isConcept forced warns red topK win wmax slots checked prev ranked →
      x ∉ emitR isConcept forced warns red topK win wmax slots checked prev ranked := by
  intro ranked
  induction ranked with
  | nil =>
    intro slots checked prev _ h
    rw [skipsR_nil'] at h
    simp at h
  | cons s rest ih =>
    intro slots checked prev hnd h
    have hnd' := List.nodup_cons.mp hnd
    -- A skip recorded further on is of a member of `rest`, so not `s`.
    have later : ∀ {sl ch : Nat} {pv : List ι},
        (k, x) ∈ skipsR isConcept forced warns red topK win wmax sl ch pv rest →
        x ∉ s :: emitR isConcept forced warns red topK win wmax sl ch pv rest := by
      intro sl ch pv hm hx
      obtain ⟨pre, post, e, _⟩ :=
        skipsR_kept_before isConcept forced warns red topK win wmax k x rest sl ch pv hm
      have hxr : x ∈ rest := by rw [e]; simp
      cases List.mem_cons.mp hx with
      | inl heq => rw [heq] at hxr; exact hnd'.1 hxr
      | inr hmem => exact ih sl ch pv hnd'.2 hm hmem
    cases hc : isConcept s with
    | false =>
      rw [skipsR_missing hc] at h
      rw [emitR_missing hc]
      exact ih _ _ _ hnd'.2 h
    | true =>
      cases hf : forced s with
      | true =>
        rw [skipsR_forced hc hf] at h
        rw [emitR_forced hc hf]
        exact later h
      | false =>
        by_cases hs : slots < topK
        · by_cases hw : checked < win
          · by_cases hskip : ∃ k, keptBy red wmax prev s = some k ∧ warns s = false
            · obtain ⟨k', hk, hn⟩ := hskip
              rw [skipsR_skip hc hf hs hw hk hn] at h
              rw [emitR_skip hc hf hs hw hk hn]
              cases List.mem_cons.mp h with
              | inl heq =>
                have e2 : x = s := (Prod.mk.inj heq).2
                subst e2
                intro hx
                exact hnd'.1 ((emitR_sublist isConcept forced warns red topK win wmax rest _ _ _
                  ).subset hx)
              | inr hmem => exact ih _ _ _ hnd'.2 hmem
            · rw [skipsR_keep hc hf hs hw (keep_of_not_skip hskip)] at h
              rw [emitR_keep hc hf hs hw (keep_of_not_skip hskip)]
              exact later h
          · rw [skipsR_late hc hf hs hw] at h
            rw [emitR_late hc hf hs hw]
            exact later h
        · rw [skipsR_cut hc hf hs] at h
          rw [emitR_cut hc hf hs]
          exact ih _ _ _ hnd'.2 h

end Redundancy

/-! ### Examples (#181) -/

/-- Members `1..9` share one text; `10` is distinct. -/
def exDup (a b : Nat) : Bool := a ≠ b && a ≤ 9 && b ≤ 9

/-- `only_the_window_is_checked`: at `top_k = 2` the window is 8, so members
2..8 are skipped and the ninth copy, past the window, takes the second
slot; at `top_k = 3` the window (12) reaches the distinct member. -/
example : window 2 = 8 ∧ window 3 = 12 ∧ window 6 = 20 := by decide

example : emitR (fun _ => true) (fun _ => false) (fun _ => false) exDup 2 (window 2) windowMax
    0 0 [] [1, 2, 3, 4, 5, 6, 7, 8, 9, 10] = [1, 9] := by decide

example : (skipsR (fun _ => true) (fun _ => false) (fun _ => false) exDup 2 (window 2) windowMax
    0 0 [] [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]).length = 7 := by decide

example : emitR (fun _ => true) (fun _ => false) (fun _ => false) exDup 3 (window 3) windowMax
    0 0 [] [1, 2, 3, 4, 5, 6, 7, 8, 9, 10] = [1, 10] := by decide

/-- `a_semantic_edge_pair_keeps_the_higher_and_the_next_distinct_fills_the_slot`:
`2` is redundant with `1`; `1` is kept and `3` takes the freed slot. -/
def exPair (a b : Nat) : Bool := (a == 1 && b == 2) || (a == 2 && b == 1)

example : emitR (fun _ => true) (fun _ => false) (fun _ => false) exPair 2 (window 2) windowMax
    0 0 [] [1, 2, 3, 4] = [1, 3] ∧
    skipsR (fun _ => true) (fun _ => false) (fun _ => false) exPair 2 (window 2) windowMax
    0 0 [] [1, 2, 3, 4] = [(1, 2)] := by decide

/-- `a_forced_member_is_never_skipped`: a forced `2` is emitted beside `1` and
takes no slot. -/
example : emitR (fun _ => true) (fun i => i == 2) (fun _ => false) exPair 1 (window 1) windowMax
    0 0 [] [1, 2, 3] = [1, 2] := by decide

/-- `a_forced_member_is_never_skipped`, as the Rust test runs it (`RANKED4`,
`top_k = 2`): the hot duplicate `2` is forced and stays past `top_k`, and
nothing is skipped; with the original `1` hot instead, the ordinary duplicate
`2` is skipped for it and `3`, `4` take the slots. -/
example : emitR (fun _ => true) (fun i => i == 2) (fun _ => false) exPair 2 (window 2) windowMax
    0 0 [] [1, 2, 3, 4] = [1, 2, 3] ∧
    skipsR (fun _ => true) (fun i => i == 2) (fun _ => false) exPair 2 (window 2) windowMax
    0 0 [] [1, 2, 3, 4] = [] ∧
    emitR (fun _ => true) (fun i => i == 1) (fun _ => false) exPair 2 (window 2) windowMax
    0 0 [] [1, 2, 3, 4] = [1, 3, 4] ∧
    skipsR (fun _ => true) (fun i => i == 1) (fun _ => false) exPair 2 (window 2) windowMax
    0 0 [] [1, 2, 3, 4] = [(1, 2)] := by decide

/-- `a_duplicate_with_a_warning_of_its_own_is_not_skipped`: `2` warns, so it is
emitted and takes the second slot. -/
example : emitR (fun _ => true) (fun _ => false) (fun i => i == 2) exPair 2 (window 2) windowMax
    0 0 [] [1, 2, 3] = [1, 2] := by decide

/-- A later ordinary copy of a forced member is skipped for it. -/
example : skipsR (fun _ => true) (fun i => i == 1) (fun _ => false) exPair 1 (window 1) windowMax
    0 0 [] [1, 2, 3] = [(1, 2)] := by decide

/-! ## Redundancy and Canonical-first

`warns` does not include the ⚑ load-bearing line every Canonical hit carries,
so a Canonical member can be skipped. Under today's `rank_members` a ⚑ banner
is still present: a skipped Canonical member was skipped for a Canonical hit,
which carries one. It is the kept copy's, not the skipped member's own line:
each ⚑ line states its own concept's blast radius (`radii.get(&c.id)` in
`assemble`), so of two Canonical copies with 10 and 3 dependents where the
second ranks first, only "3 nodes depend on this" is shown, with an
`also <id>` label naming the skipped copy. -/

/-- **A skipped Canonical member is skipped only for a Canonical hit** when the
ranked list is in `rank_members` order. -/
theorem skipped_canonical_kept_canonical {σ κ : Type} (isConcept forced warns : ι → Bool)
    (red : ι → ι → Bool) (topK win wmax : Nat) (isCanonical : ι → Bool)
    (scoreCmp : σ → σ → Ordering) (score : ι → σ) (keyCmp : κ → κ → Ordering)
    (key : ι → Option κ) (idCmp : ι → ι → Ordering) {ranked : List ι}
    (hsorted : ranked.Pairwise (fun a b =>
      Order.rankMembers isCanonical scoreCmp score keyCmp key idCmp a b = .lt))
    {k x : ι} (hkx : (k, x) ∈ skipsR isConcept forced warns red topK win wmax 0 0 [] ranked)
    (hx : isCanonical x = true) : isCanonical k = true := by
  have hR := skipsR_pairwise isConcept forced warns red topK win wmax hsorted hkx
  cases hk : isCanonical k
  · have : Order.rankMembers isCanonical scoreCmp score keyCmp key idCmp k x = .gt := by
      show (Order.boolCmp (isCanonical x) (isCanonical k)).then
        (Order.scoreThenTie scoreCmp score keyCmp key idCmp k x) = .gt
      rw [hx, hk]
      rfl
    rw [this] at hR
    exact absurd hR (by decide)
  · rfl

/-- Canonical `1`, `2` (one text) and a plain `3`, in `rank_members` order. -/
def exCanon (i : Nat) : Bool := i ≤ 2

def exCanonScore : Nat → Nat
  | 1 => 90
  | 2 => 80
  | _ => 70

def exCanonDup (a b : Nat) : Bool := (a == 1 && b == 2) || (a == 2 && b == 1)

/-- `skipped_canonical_kept_canonical` at work: the Canonical `2` is skipped
for the Canonical `1`, whose ⚑ line (not `2`'s) is the one shown. -/
example : [1, 2, 3].Pairwise (fun a b =>
      Order.rankMembers exCanon Order.natCmp exCanonScore Order.natCmp (fun _ => none)
        Order.natCmp a b = .lt) ∧
    skipsR (fun _ => true) (fun _ => false) (fun _ => false) exCanonDup 2 (window 2) windowMax
      0 0 [] [1, 2, 3] = [(1, 2)] ∧
    emitR (fun _ => true) (fun _ => false) (fun _ => false) exCanonDup 2 (window 2) windowMax
      0 0 [] [1, 2, 3] = [1, 3] ∧
    exCanon 1 = true ∧ exCanon 2 = true := by decide

/-! ## #280's gate against emission (design model)

Two consequences of gating only the order (`Order.gatedRankMembers`), using
the dogfood shape of `Order.lean`'s examples: direct matches `1`, `2` and a
Canonical pillar `3` reached by expansion.

1. **A forced pillar is shown whatever the order.** #280's example pillar
   carried a conflict banner, a hot-list `Conflict` payload, which `forces`
   force-includes past `top_k`. `forced_mem_emit` and `forced_mem_emitR` hold
   for every ranked order, so a gate on the order moves such a member down but
   cannot remove it, or its banners, from the hits. Only an unforced pillar is
   cut by the gate.
2. **The gate breaks `skipped_canonical_kept_canonical`.** Behind the gate a
   below-gate Canonical member can be ranked after a non-Canonical copy that
   is redundant with it and be skipped for it, and the ⚑ line, which `warns`
   does not count, goes with it: no ⚑ banner is left. This needs a redundant
   pair whose query relevance falls on opposite sides of the gate. The
   default same-content signal rarely produces one (identical texts score
   alike in phase 1); it becomes realistic under `similar_signals = true`,
   where a `Semantic` edge or cosine joins differently worded texts, or when
   one copy misses the phase-1 cap or has no vector.
-/

/-- `top_k = 2`, pillar not forced: today it takes the first slot; behind the
gate it is cut. -/
example : emit (fun _ => true) (fun _ => false) 2 0 [3, 1, 2] = [3, 1] ∧
    emit (fun _ => true) (fun _ => false) 2 0 [1, 2, 3] = [1, 2] := by decide

/-- `top_k = 2`, pillar forced (a conflict payload): shown in either order. -/
example : emit (fun _ => true) (fun i => i == 3) 2 0 [3, 1, 2] = [3, 1, 2] ∧
    emit (fun _ => true) (fun i => i == 3) 2 0 [1, 2, 3] = [1, 2, 3] := by decide

/-- `1` and the pillar `3` hold the same text. -/
def exSame (a b : Nat) : Bool := (a == 1 && b == 3) || (a == 3 && b == 1)

/-- `top_k = 3`: today the Canonical copy ranks first and the plain copy is
skipped for it; behind the gate the Canonical copy is skipped for the plain
one. The example's relevances (pillar 0, plain copy 90) put the pair on
opposite sides of the gate, which identical texts rarely are; see item 2
above. -/
example : skipsR (fun _ => true) (fun _ => false) (fun _ => false) exSame 3 (window 3) windowMax
    0 0 [] [3, 1, 2] = [(3, 1)] ∧
    skipsR (fun _ => true) (fun _ => false) (fun _ => false) exSame 3 (window 3) windowMax
    0 0 [] [1, 2, 3] = [(1, 3)] := by decide

/-- That second list is in gated order (gate 10), and its skipped member is
Canonical while its kept one is not. -/
example : [1, 2, 3].Pairwise (fun a b => Order.ex280Order Order.ex280Rel (some 10) a b = .lt) ∧
    Order.ex280Canonical 3 = true ∧ Order.ex280Canonical 1 = false := by decide

end Lambo.TopK
