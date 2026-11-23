/-
# Ranking comparators are strict total orders

Every score-ordered list in Lambo is produced by `sort_by` with an ad hoc
comparator built from `Ordering::then_with`:

* `tie_break_by_key` (`src/types/mod.rs`): canonical key ascending, a keyless
  node after every keyed one, then node id ascending;
* the score table (`daemon::score::rescore`), phase-1 `rank`
  (`src/recall/candidates.rs`) and BM25 `search_ranked`: score descending, then
  `tie_break_by_key`;
* `rank_members` (`src/recall/assemble.rs`): Canonical first, then score
  descending, then `tie_break_by_key`.

The doc comments claim these are total orders, so the sorted output does not
depend on the order the input arrived in ("made the comparator total: the same
pair now always orders the same way", `tie_break_by_key`; "Sorting is a total
order ... so the output is deterministic regardless of leg iteration or store
result order", `candidates.rs`). This file proves that claim from the shape of
the comparators alone:

* `lawful_lex`: `a.then_with(b)` of two lawful comparators is lawful;
* `rankMembers_lawful`, `rankMembers_separating`: the assembled comparators
  are lawful and only call two members equal when they are the same node;
* `sorted_unique`: two lists sorted by such a comparator with the same members
  are the same list, which is the determinism statement;
* `rankMembers_canonical_first`: a Canonical member orders before every
  non-Canonical one, whatever their scores.

The last section is a **design model** of issue #280 (open, no Rust yet): a
relevance-gated Canonical-first order, `gatedRankMembers`, next to today's,
which it leaves unchanged. Its readings of the issue (the gate compares query
relevance, clears at or above, `none` is off, banners not modelled) are listed
there. Results: off is today's order (`gatedRankMembers_off`); a Canonical member
at or above the gate still precedes every non-promoted member
(`gatedRankMembers_promoted_first`); below the gate it is ordered by score
like any other member (`gatedRankMembers_below_gate_score`); and the order is
still lawful, strict and deterministic (`gatedRankMembers_lawful`, `gatedRankMembers_separating`,
`gatedRankMembers_sorted_unique`). `gatedRankMembers_below_gate_score` is also
the statement that the gate breaks spec §10's "always promoted first".

Members are node ids, with score, key and Canonical status as functions of
the id, as in the Rust comparators, which look all three up by id.

Boundary: the score order (`f64::total_cmp`), the key order (`str::cmp` on the
tie form) and the id order (`Uuid::cmp`) are parameters assumed lawful, not
proved so here, and the id order is assumed to separate distinct ids. One
entry per id is presumed, which `expand` guarantees (`Expand.expand_nodup`).
`sort_by`'s algorithm is not modelled: `sorted_unique` holds for any correct
sort.
-/

universe u v

namespace Lambo.Order

/-! ## `Ordering` algebra -/

theorem then_eq_eq (x y : Ordering) : x.then y = .eq → x = .eq ∧ y = .eq := by
  cases x <;> cases y <;> decide

theorem then_eq_lt (x y : Ordering) :
    x.then y = .lt ↔ x = .lt ∨ (x = .eq ∧ y = .lt) := by
  cases x <;> cases y <;> decide

theorem swap_then (x y : Ordering) : (x.then y).swap = x.swap.then y.swap := by
  cases x <;> cases y <;> rfl

/-! ## Lawful comparators -/

/-- A comparator that behaves like `Ord::cmp` on a total preorder: comparing
the other way round swaps the answer, `Less` is transitive, and two elements
that compare `Equal` compare identically against everything else. -/
structure Lawful {α : Type u} (cmp : α → α → Ordering) : Prop where
  swap : ∀ a b, cmp b a = (cmp a b).swap
  lt_trans : ∀ a b c, cmp a b = .lt → cmp b c = .lt → cmp a c = .lt
  eq_left : ∀ a b c, cmp a b = .eq → cmp a c = cmp b c

/-- A comparator that calls two elements `Equal` only when they are the same
element. -/
def Separating {α : Type u} (cmp : α → α → Ordering) : Prop :=
  ∀ a b, cmp a b = .eq → a = b

section Laws

variable {α : Type u} {cmp : α → α → Ordering}

theorem Lawful.refl (h : Lawful cmp) (a : α) : cmp a a = .eq := by
  have hs := h.swap a a
  generalize cmp a a = o at hs ⊢
  cases o
  · exact absurd hs (by decide)
  · rfl
  · exact absurd hs (by decide)

theorem Lawful.eq_symm (h : Lawful cmp) {a b : α} (hab : cmp a b = .eq) : cmp b a = .eq := by
  have hs := h.swap a b
  rw [hab] at hs
  exact hs

theorem Lawful.eq_right (h : Lawful cmp) (a b c : α) (hbc : cmp b c = .eq) :
    cmp a c = cmp a b := by
  have e := h.eq_left c b a (h.eq_symm hbc)
  rw [h.swap c a, h.swap b a, e]

/-- `Less` one way is `Greater` the other way. -/
theorem Lawful.gt_of_lt (h : Lawful cmp) {a b : α} (hab : cmp a b = .lt) : cmp b a = .gt := by
  have hs := h.swap a b
  rw [hab] at hs
  exact hs

end Laws

/-! ## Combinators -/

/-- `a.then_with(|| b)`. -/
abbrev lex {α : Type u} (c₁ c₂ : α → α → Ordering) : α → α → Ordering :=
  fun a b => (c₁ a b).then (c₂ a b)

/-- Compare through a projection, as in `key(a).cmp(key(b))`. -/
abbrev byKey {α : Type u} {β : Type v} (f : α → β) (cmp : β → β → Ordering) :
    α → α → Ordering :=
  fun a b => cmp (f a) (f b)

/-- Descending order, as in `b.score.total_cmp(&a.score)`. -/
abbrev reversed {α : Type u} (cmp : α → α → Ordering) : α → α → Ordering :=
  fun a b => cmp b a

section Combinators

variable {α : Type u} {β : Type v}

theorem lawful_lex {c₁ c₂ : α → α → Ordering} (h₁ : Lawful c₁) (h₂ : Lawful c₂) :
    Lawful (lex c₁ c₂) where
  swap := by
    intro a b
    show (c₁ b a).then (c₂ b a) = ((c₁ a b).then (c₂ a b)).swap
    rw [swap_then, h₁.swap a b, h₂.swap a b]
  lt_trans := by
    intro a b c hab hbc
    have hab' := (then_eq_lt (c₁ a b) (c₂ a b)).mp hab
    have hbc' := (then_eq_lt (c₁ b c) (c₂ b c)).mp hbc
    apply (then_eq_lt (c₁ a c) (c₂ a c)).mpr
    cases hab' with
    | inl h =>
      cases hbc' with
      | inl k => exact Or.inl (h₁.lt_trans a b c h k)
      | inr k =>
        have e := h₁.eq_right a b c k.1
        rw [h] at e
        exact Or.inl e
    | inr h =>
      cases hbc' with
      | inl k =>
        have e := h₁.eq_left a b c h.1
        rw [k] at e
        exact Or.inl e
      | inr k =>
        have e := h₁.eq_left a b c h.1
        rw [k.1] at e
        exact Or.inr ⟨e, h₂.lt_trans a b c h.2 k.2⟩
  eq_left := by
    intro a b c hab
    have hboth := then_eq_eq (c₁ a b) (c₂ a b) hab
    show (c₁ a c).then (c₂ a c) = (c₁ b c).then (c₂ b c)
    rw [h₁.eq_left a b c hboth.1, h₂.eq_left a b c hboth.2]

theorem separating_lex (c₁ : α → α → Ordering) {c₂ : α → α → Ordering} (h : Separating c₂) :
    Separating (lex c₁ c₂) :=
  fun a b hab => h a b (then_eq_eq (c₁ a b) (c₂ a b) hab).2

theorem lawful_byKey {cmp : β → β → Ordering} (h : Lawful cmp) (f : α → β) :
    Lawful (byKey f cmp) where
  swap := fun a b => h.swap (f a) (f b)
  lt_trans := fun a b c => h.lt_trans (f a) (f b) (f c)
  eq_left := fun a b c => h.eq_left (f a) (f b) (f c)

theorem lawful_reversed {cmp : α → α → Ordering} (h : Lawful cmp) : Lawful (reversed cmp) where
  swap := fun a b => h.swap b a
  lt_trans := fun a b c hab hbc => h.lt_trans c b a hbc hab
  eq_left := fun a b c hab => h.eq_right c b a hab

end Combinators

/-! ## The primitive comparators -/

/-- `bool::cmp`: `false < true`. -/
def boolCmp : Bool → Bool → Ordering
  | false, true => .lt
  | true, false => .gt
  | _, _ => .eq

theorem boolCmp_lawful : Lawful boolCmp where
  swap := by
    intro a b
    cases a <;> cases b <;> rfl
  lt_trans := by
    intro a b c
    cases a <;> cases b <;> cases c <;> decide
  eq_left := by
    intro a b c
    cases a <;> cases b <;> cases c <;> decide

/-- The key half of `tie_break_by_key`: two keys compare by `cmp`, a keyed node
sorts before a keyless one (`NULLS LAST`), two keyless nodes are equal. -/
def optCmp {κ : Type u} (cmp : κ → κ → Ordering) : Option κ → Option κ → Ordering
  | some x, some y => cmp x y
  | some _, none => .lt
  | none, some _ => .gt
  | none, none => .eq

/-- Key *presence* is itself compared, which is what makes the comparator
transitive on mixed input (remediation round 1; pinned by the Rust test
`tie_break_is_transitive_on_mixed_key_presence`). -/
theorem optCmp_lawful {κ : Type u} {cmp : κ → κ → Ordering} (h : Lawful cmp) :
    Lawful (optCmp cmp) where
  swap := by
    intro a b
    cases a <;> cases b <;> first | exact h.swap _ _ | rfl
  lt_trans := by
    intro a b c
    cases a <;> cases b <;> cases c <;> first | exact h.lt_trans _ _ _ | simp [optCmp]
  eq_left := by
    intro a b c
    cases a <;> cases b <;> cases c <;> first | exact h.eq_left _ _ _ | simp [optCmp]

/-- A comparator on naturals, used to instantiate the abstract orders. -/
def natCmp (a b : Nat) : Ordering :=
  if a < b then .lt else if b < a then .gt else .eq

theorem natCmp_lawful : Lawful natCmp where
  swap := by
    intro a b
    unfold natCmp
    repeat' split
    all_goals first | rfl | omega
  lt_trans := by
    intro a b c
    unfold natCmp
    repeat' split
    all_goals (intros; first | rfl | contradiction | omega)
  eq_left := by
    intro a b c
    unfold natCmp
    repeat' split
    all_goals (intros; first | rfl | contradiction | omega)

theorem natCmp_separating : Separating natCmp := by
  intro a b
  unfold natCmp
  repeat' split
  all_goals (intros; first | omega | contradiction)

/-! ## Lambo's comparators -/

section Rank

variable {ι : Type u} {κ : Type v} {σ : Type v}

/-- `tie_break_by_key`: canonical key (in tie form) ascending with keyless
nodes last, then node id ascending. -/
def tieBreak (keyCmp : κ → κ → Ordering) (key : ι → Option κ) (idCmp : ι → ι → Ordering) :
    ι → ι → Ordering :=
  lex (byKey key (optCmp keyCmp)) idCmp

/-- Score descending, then `tie_break_by_key`: the comparator of `rescore`, of
phase-1 `rank` and of `search_ranked`. -/
def scoreThenTie (scoreCmp : σ → σ → Ordering) (score : ι → σ)
    (keyCmp : κ → κ → Ordering) (key : ι → Option κ) (idCmp : ι → ι → Ordering) :
    ι → ι → Ordering :=
  lex (reversed (byKey score scoreCmp)) (tieBreak keyCmp key idCmp)

/-- `rank_members`: Canonical first, then score descending, then
`tie_break_by_key`. -/
def rankMembers (isCanonical : ι → Bool) (scoreCmp : σ → σ → Ordering) (score : ι → σ)
    (keyCmp : κ → κ → Ordering) (key : ι → Option κ) (idCmp : ι → ι → Ordering) :
    ι → ι → Ordering :=
  lex (reversed (byKey isCanonical boolCmp)) (scoreThenTie scoreCmp score keyCmp key idCmp)

theorem tieBreak_lawful {keyCmp : κ → κ → Ordering} (hkey : Lawful keyCmp)
    (key : ι → Option κ) {idCmp : ι → ι → Ordering} (hid : Lawful idCmp) :
    Lawful (tieBreak keyCmp key idCmp) :=
  lawful_lex (lawful_byKey (optCmp_lawful hkey) key) hid

/-- The tie-break separates distinct nodes: "the id fallback stays" because
"the key is not a total separator". -/
theorem tieBreak_separating (keyCmp : κ → κ → Ordering) (key : ι → Option κ)
    {idCmp : ι → ι → Ordering} (hsep : Separating idCmp) :
    Separating (tieBreak keyCmp key idCmp) :=
  separating_lex (byKey key (optCmp keyCmp)) hsep

theorem scoreThenTie_lawful {scoreCmp : σ → σ → Ordering} (hscore : Lawful scoreCmp)
    (score : ι → σ) {keyCmp : κ → κ → Ordering} (hkey : Lawful keyCmp) (key : ι → Option κ)
    {idCmp : ι → ι → Ordering} (hid : Lawful idCmp) :
    Lawful (scoreThenTie scoreCmp score keyCmp key idCmp) :=
  lawful_lex (lawful_reversed (lawful_byKey hscore score)) (tieBreak_lawful hkey key hid)

theorem scoreThenTie_separating (scoreCmp : σ → σ → Ordering) (score : ι → σ)
    (keyCmp : κ → κ → Ordering) (key : ι → Option κ)
    {idCmp : ι → ι → Ordering} (hsep : Separating idCmp) :
    Separating (scoreThenTie scoreCmp score keyCmp key idCmp) :=
  separating_lex (reversed (byKey score scoreCmp)) (tieBreak_separating keyCmp key hsep)

theorem rankMembers_lawful (isCanonical : ι → Bool)
    {scoreCmp : σ → σ → Ordering} (hscore : Lawful scoreCmp)
    (score : ι → σ) {keyCmp : κ → κ → Ordering} (hkey : Lawful keyCmp) (key : ι → Option κ)
    {idCmp : ι → ι → Ordering} (hid : Lawful idCmp) :
    Lawful (rankMembers isCanonical scoreCmp score keyCmp key idCmp) :=
  lawful_lex (lawful_reversed (lawful_byKey boolCmp_lawful isCanonical))
    (scoreThenTie_lawful hscore score hkey key hid)

theorem rankMembers_separating (isCanonical : ι → Bool) (scoreCmp : σ → σ → Ordering)
    (score : ι → σ) (keyCmp : κ → κ → Ordering) (key : ι → Option κ)
    {idCmp : ι → ι → Ordering} (hsep : Separating idCmp) :
    Separating (rankMembers isCanonical scoreCmp score keyCmp key idCmp) :=
  separating_lex (reversed (byKey isCanonical boolCmp))
    (scoreThenTie_separating scoreCmp score keyCmp key hsep)

/-- **Canonical first.** "Every Canonical member of the expanded set is ranked
ahead of every non-Canonical one" (`assemble.rs`), whatever the two scores
are: the partition is the first sort key, not a score boost. -/
theorem rankMembers_canonical_first (isCanonical : ι → Bool) (scoreCmp : σ → σ → Ordering)
    (score : ι → σ) (keyCmp : κ → κ → Ordering) (key : ι → Option κ)
    (idCmp : ι → ι → Ordering) {a b : ι}
    (ha : isCanonical a = true) (hb : isCanonical b = false) :
    rankMembers isCanonical scoreCmp score keyCmp key idCmp a b = .lt := by
  show (boolCmp (isCanonical b) (isCanonical a)).then
      (scoreThenTie scoreCmp score keyCmp key idCmp a b) = .lt
  rw [ha, hb] <;> rfl

/-- Inside one Canonical class the higher score orders first. -/
theorem rankMembers_score_desc (isCanonical : ι → Bool) (scoreCmp : σ → σ → Ordering)
    (score : ι → σ) (keyCmp : κ → κ → Ordering) (key : ι → Option κ)
    (idCmp : ι → ι → Ordering) {a b : ι}
    (hc : isCanonical a = isCanonical b) (hs : scoreCmp (score b) (score a) = .lt) :
    rankMembers isCanonical scoreCmp score keyCmp key idCmp a b = .lt := by
  show (boolCmp (isCanonical b) (isCanonical a)).then
      ((scoreCmp (score b) (score a)).then (tieBreak keyCmp key idCmp a b)) = .lt
  rw [hc, hs]
  cases isCanonical b <;> rfl

end Rank

/-! ## Determinism of the sorted output -/

section Sorted

variable {α : Type u} {cmp : α → α → Ordering}

/-- **Determinism.** Two lists that are both strictly sorted by a lawful
comparator and have the same members are equal. A correct sort returns a sorted
permutation of its input, so its output depends only on the *set* of members,
not on the order they were supplied in. -/
theorem sorted_unique (h : Lawful cmp) :
    ∀ (l₁ l₂ : List α),
      l₁.Pairwise (fun a b => cmp a b = .lt) →
      l₂.Pairwise (fun a b => cmp a b = .lt) →
      (∀ x, x ∈ l₁ ↔ x ∈ l₂) → l₁ = l₂ := by
  intro l₁
  induction l₁ with
  | nil =>
    intro l₂ _ _ hm
    cases l₂ with
    | nil => rfl
    | cons b t =>
      have hb : b ∈ ([] : List α) := (hm b).mpr (List.mem_cons.mpr (Or.inl rfl))
      simp at hb
  | cons a t₁ ih =>
    intro l₂ h₁ h₂ hm
    cases l₂ with
    | nil =>
      have ha : a ∈ ([] : List α) := (hm a).mp (List.mem_cons.mpr (Or.inl rfl))
      simp at ha
    | cons b t₂ =>
      have h₁' := List.pairwise_cons.mp h₁
      have h₂' := List.pairwise_cons.mp h₂
      have hab : a = b := by
        by_cases hab : a = b
        · exact hab
        · have ha2 : a ∈ b :: t₂ := (hm a).mp (List.mem_cons.mpr (Or.inl rfl))
          have hb1 : b ∈ a :: t₁ := (hm b).mpr (List.mem_cons.mpr (Or.inl rfl))
          have ha2' : a ∈ t₂ := by
            cases List.mem_cons.mp ha2 with
            | inl e => exact absurd e hab
            | inr m => exact m
          have hb1' : b ∈ t₁ := by
            cases List.mem_cons.mp hb1 with
            | inl e => exact absurd e.symm hab
            | inr m => exact m
          have lt1 : cmp a b = .lt := h₁'.1 b hb1'
          have lt2 : cmp b a = .lt := h₂'.1 a ha2'
          have gt2 := h.gt_of_lt lt1
          rw [lt2] at gt2
          exact absurd gt2 (by decide)
      subst hab
      have htail : ∀ x, x ∈ t₁ ↔ x ∈ t₂ := by
        intro x
        constructor
        · intro hx
          have hx2 : x ∈ a :: t₂ := (hm x).mp (List.mem_cons.mpr (Or.inr hx))
          cases List.mem_cons.mp hx2 with
          | inl e =>
            have lt : cmp a x = .lt := h₁'.1 x hx
            rw [e, h.refl a] at lt
            exact absurd lt (by decide)
          | inr m => exact m
        · intro hx
          have hx1 : x ∈ a :: t₁ := (hm x).mpr (List.mem_cons.mpr (Or.inr hx))
          cases List.mem_cons.mp hx1 with
          | inl e =>
            have lt : cmp a x = .lt := h₂'.1 x hx
            rw [e, h.refl a] at lt
            exact absurd lt (by decide)
          | inr m => exact m
      rw [ih t₂ h₁'.2 h₂'.2 htail]

end Sorted

/-! ## Gated Canonical-first (#280, design model)

Issue #280 (open, not implemented) proposes gating `rank_members`'s
Canonical-first partition on query relevance, so that a Canonical concept
reached only by graph expansion or a weak tail match no longer jumps ahead of
the direct matches. This section models that proposal next to today's order,
before any Rust exists. Readings chosen where #280 leaves a choice (each is a
question for the owner; see the file header):

* the gate compares a per-member **query relevance** `rel` (assemble.rs's
  `query_relevance`: the phase-1 candidate score, `0` for a member phase 1
  never scored), not the final blended score. Its scale depends on
  `FusionKind`: a raw leg score (unbounded BM25 for keyword hits) under
  `Max`, a value in `[0, 1]` under `Rrf` (#173), so one gate constant means
  different things under the two; the model takes `rel` as given. The #175
  research seam `assemble_with_graph_relevance` can give an expansion-only
  member a nonzero relevance (`graph_relevance`, used by `score_of` when
  phase 1 has none); the model ignores it, and production passes `None`;
* a member clears the gate **at or above** it (`gate ≤ rel`);
* the gate is `Option Nat`: `none` is the gate switched off;
* a promoted member is Canonical **and** clears the gate; promotion is the
  first sort key exactly as Canonical status is today, so the gated order is
  `rank_members` with the partition predicate replaced and nothing else
  changed (`gatedRankMembers`);
* warning banners are not modelled here (`TopK.lean` shows what the order
  change does and does not do to them).

Relevance is a natural number: an exact fixed-point stand-in for the `f64`
the Rust would compare, whose sanitized value (`sane_weight`) is finite and
non-negative.
-/

section Gated

variable {ι : Type u} {κ : Type v} {σ : Type v}

/-- Whether a relevance clears the gate: everything clears a switched-off
gate (`none`); `some g` is cleared at or above `g`. -/
def clearsGate (gate : Option Nat) (rel : Nat) : Bool :=
  match gate with
  | none => true
  | some g => decide (g ≤ rel)

/-- Promoted ahead of the rest: Canonical and clearing the gate. -/
def promoted (isCanonical : ι → Bool) (rel : ι → Nat) (gate : Option Nat) (i : ι) : Bool :=
  isCanonical i && clearsGate gate (rel i)

/-- #280's gated `rank_members`: promoted first, then score descending, then
`tie_break_by_key`. -/
def gatedRankMembers (isCanonical : ι → Bool) (rel : ι → Nat) (gate : Option Nat)
    (scoreCmp : σ → σ → Ordering) (score : ι → σ)
    (keyCmp : κ → κ → Ordering) (key : ι → Option κ) (idCmp : ι → ι → Ordering) :
    ι → ι → Ordering :=
  rankMembers (promoted isCanonical rel gate) scoreCmp score keyCmp key idCmp

theorem promoted_off (isCanonical : ι → Bool) (rel : ι → Nat) :
    promoted isCanonical rel none = isCanonical := by
  funext i
  simp [promoted, clearsGate]

/-- **Off is today's order.** With the gate switched off the gated order is
`rank_members` exactly. -/
theorem gatedRankMembers_off (isCanonical : ι → Bool) (rel : ι → Nat)
    (scoreCmp : σ → σ → Ordering) (score : ι → σ) (keyCmp : κ → κ → Ordering)
    (key : ι → Option κ) (idCmp : ι → ι → Ordering) :
    gatedRankMembers isCanonical rel none scoreCmp score keyCmp key idCmp
      = rankMembers isCanonical scoreCmp score keyCmp key idCmp := by
  unfold gatedRankMembers
  rw [promoted_off]

/-- **Still a lawful order.** -/
theorem gatedRankMembers_lawful (isCanonical : ι → Bool) (rel : ι → Nat) (gate : Option Nat)
    {scoreCmp : σ → σ → Ordering} (hscore : Lawful scoreCmp)
    (score : ι → σ) {keyCmp : κ → κ → Ordering} (hkey : Lawful keyCmp) (key : ι → Option κ)
    {idCmp : ι → ι → Ordering} (hid : Lawful idCmp) :
    Lawful (gatedRankMembers isCanonical rel gate scoreCmp score keyCmp key idCmp) :=
  rankMembers_lawful _ hscore score hkey key hid

/-- **Still strict and total**: equal only on the same node. -/
theorem gatedRankMembers_separating (isCanonical : ι → Bool) (rel : ι → Nat)
    (gate : Option Nat) (scoreCmp : σ → σ → Ordering)
    (score : ι → σ) (keyCmp : κ → κ → Ordering) (key : ι → Option κ)
    {idCmp : ι → ι → Ordering} (hsep : Separating idCmp) :
    Separating (gatedRankMembers isCanonical rel gate scoreCmp score keyCmp key idCmp) :=
  rankMembers_separating _ scoreCmp score keyCmp key hsep

/-- **Still deterministic**: two lists sorted by the gated order with the same
members are equal. -/
theorem gatedRankMembers_sorted_unique (isCanonical : ι → Bool) (rel : ι → Nat)
    (gate : Option Nat) {scoreCmp : σ → σ → Ordering} (hscore : Lawful scoreCmp)
    (score : ι → σ) {keyCmp : κ → κ → Ordering} (hkey : Lawful keyCmp) (key : ι → Option κ)
    {idCmp : ι → ι → Ordering} (hid : Lawful idCmp) (l₁ l₂ : List ι)
    (h₁ : l₁.Pairwise (fun a b =>
      gatedRankMembers isCanonical rel gate scoreCmp score keyCmp key idCmp a b = .lt))
    (h₂ : l₂.Pairwise (fun a b =>
      gatedRankMembers isCanonical rel gate scoreCmp score keyCmp key idCmp a b = .lt))
    (hm : ∀ x, x ∈ l₁ ↔ x ∈ l₂) : l₁ = l₂ :=
  sorted_unique (gatedRankMembers_lawful isCanonical rel gate hscore score hkey key hid)
    l₁ l₂ h₁ h₂ hm

/-- **Promoted first.** A Canonical member at or above the gate orders before
every member that is not promoted (a non-Canonical one, or a Canonical one
below the gate), whatever the scores. -/
theorem gatedRankMembers_promoted_first (isCanonical : ι → Bool) (rel : ι → Nat)
    (gate : Option Nat) (scoreCmp : σ → σ → Ordering) (score : ι → σ)
    (keyCmp : κ → κ → Ordering) (key : ι → Option κ) (idCmp : ι → ι → Ordering) {a b : ι}
    (ha : isCanonical a = true) (hg : clearsGate gate (rel a) = true)
    (hb : promoted isCanonical rel gate b = false) :
    gatedRankMembers isCanonical rel gate scoreCmp score keyCmp key idCmp a b = .lt := by
  apply rankMembers_canonical_first
  · simp [promoted, ha, hg]
  · exact hb

/-- Two members neither of which is promoted are ordered by score, then
tie-break, exactly as `rescore` and phase-1 `rank` order them. -/
theorem gatedRankMembers_unpromoted (isCanonical : ι → Bool) (rel : ι → Nat)
    (gate : Option Nat) (scoreCmp : σ → σ → Ordering) (score : ι → σ)
    (keyCmp : κ → κ → Ordering) (key : ι → Option κ) (idCmp : ι → ι → Ordering) {a b : ι}
    (ha : promoted isCanonical rel gate a = false) (hb : promoted isCanonical rel gate b = false) :
    gatedRankMembers isCanonical rel gate scoreCmp score keyCmp key idCmp a b
      = scoreThenTie scoreCmp score keyCmp key idCmp a b := by
  show (boolCmp (promoted isCanonical rel gate b) (promoted isCanonical rel gate a)).then
      (scoreThenTie scoreCmp score keyCmp key idCmp a b) = _
  rw [ha, hb]
  rfl

/-- In particular a non-Canonical member with the higher score orders before a
Canonical member below the gate: the order today's `rank_members` never
produces (`rankMembers_canonical_first`). -/
theorem gatedRankMembers_below_gate_score (isCanonical : ι → Bool) (rel : ι → Nat)
    (gate : Option Nat) (scoreCmp : σ → σ → Ordering) (score : ι → σ)
    (keyCmp : κ → κ → Ordering) (key : ι → Option κ) (idCmp : ι → ι → Ordering) {a b : ι}
    (hg : clearsGate gate (rel a) = false) (hb : isCanonical b = false)
    (hs : scoreCmp (score a) (score b) = .lt) :
    gatedRankMembers isCanonical rel gate scoreCmp score keyCmp key idCmp b a = .lt := by
  have e := gatedRankMembers_unpromoted isCanonical rel gate scoreCmp score keyCmp key idCmp
    (a := b) (b := a) (by simp [promoted, hb]) (by simp [promoted, hg])
  rw [e]
  show (scoreCmp (score a) (score b)).then (tieBreak keyCmp key idCmp b a) = .lt
  rw [hs]
  rfl

end Gated

/-! ### Examples: #280's dogfood shape

Members `1` and `2` are direct matches (relevance and score 90 and 80); `3` is
a Canonical pillar reached by expansion (relevance 0, score 5). Today the
pillar ranks first; behind a gate of 10 it ranks last; a pillar with
relevance 50 clears the gate and still ranks first. -/

section Examples280

/-- Member ids; `3` is the Canonical pillar. -/
def ex280Canonical (i : Nat) : Bool := i == 3

/-- Query relevance of the expansion-only pillar: 0. -/
def ex280Rel : Nat → Nat
  | 1 => 90
  | 2 => 80
  | _ => 0

/-- The same pillar as a direct match of relevance 50. -/
def ex280RelHit : Nat → Nat
  | 1 => 90
  | 2 => 80
  | _ => 50

def ex280Score : Nat → Nat
  | 1 => 90
  | 2 => 80
  | _ => 5

/-- The order of the example, with no canonical keys (ties go to the id). -/
def ex280Order (rel : Nat → Nat) (gate : Option Nat) : Nat → Nat → Ordering :=
  gatedRankMembers ex280Canonical rel gate natCmp ex280Score natCmp (fun _ => none) natCmp

/-- Today: the pillar first. -/
example : [3, 1, 2].Pairwise (fun a b =>
    rankMembers ex280Canonical natCmp ex280Score natCmp (fun _ => none) natCmp a b = .lt) := by
  decide

/-- Gate off: today's order. -/
example : [3, 1, 2].Pairwise (fun a b => ex280Order ex280Rel none a b = .lt) := by decide

/-- Gate 10: the expansion-only pillar is ordered by its score, last. -/
example : [1, 2, 3].Pairwise (fun a b => ex280Order ex280Rel (some 10) a b = .lt) := by decide

/-- Gate 10: a pillar of relevance 50 clears it and still ranks first. -/
example : [3, 1, 2].Pairwise (fun a b => ex280Order ex280RelHit (some 10) a b = .lt) := by
  decide

/-- At the gate: relevance equal to the gate clears it. -/
example : clearsGate (some 50) 50 = true ∧ clearsGate (some 51) 50 = false := by decide

end Examples280

end Lambo.Order
