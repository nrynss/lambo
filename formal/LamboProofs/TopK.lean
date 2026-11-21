/-
# Top-K emission: forced members never take a slot and are never cut

Model of `emitted` in `src/recall/assemble.rs`, without the optional
redundancy skip (#181, off by default: `skip_redundant = false`):

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
      rw [emit_missing hc, ih slots hle] <;> simp [List.filter_cons, hc]
    | true =>
      cases hf : forced s with
      | true =>
        have e : ordinary isConcept forced (s :: rest) = ordinary isConcept forced rest := by
          simp [ordinary, hc, hf]
        rw [e] at hle
        rw [emit_forced hc hf, ih slots hle] <;> simp [List.filter_cons, hc]
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
        rw [emit_take hc hf hs, ih (slots + 1) (by omega)] <;> simp [List.filter_cons, hc]

end Lambo.TopK
