import LamboProofs.TopK
import LamboProofs.Lib.List

/-
# The daemon hot list (`src/daemon/hotlist.rs`)

Models the hot list: `Condition`, `Condition::severity`, `higher_priority`,
`HotList::{with_budget, enforce, insert, insert_many, revalidate,
revalidate_members, retain_conditions}`, `default_contradiction_max` and
`HOT_LIST_MAX`. An entry is `(node, cond, payload, seq)`; its predicate is
the parameter `check`, which every detector builds from the node and constant
config. How recall consumes the list is `Recall.lean`; the daemon's cycle
is `Cycle.lean`.

Results: the bound and the contradiction budget (#196) always hold
(`enforce_length_le`, `enforce_countC_le`), with one entry per
`(node, condition)` (`upsert_keys_nodup`, `Inv`); what is evicted ranks at or below
everything kept, except a contradiction the budget drops (`enforce_evicts_lowest`,
`insert_evicted_lowest`), the older going within a tier
(`enforce_evicts_oldest_in_tier`); a flood of contradictions never evicts the
reserved other kinds (`enforce_keeps_reserved`); one insert evicts at most
one entry (`insertFull_evicts_le_one`), and a re-insert refreshes in place
(`insert_refresh`); while nothing is evicted insertion order does not matter
(`insertAll_order_irrelevant`), and once full it does
(`order_matters_when_full`). There is no timer: recall gets a payload for a
member exactly when an entry of that node holds at `now` (`mem_hotPayloads`),
and a clock-reading predicate lapses exactly outside its window
(`windowCheck_none_iff`); the cycle drops a GC-collected node once the
detectors report live nodes only (`cycle_drops_collected`).

Boundary: times are `Int` seconds and nodes an abstract type; the payload
keeps only what the model reads; `higher_priority`'s node-id arm never
decides on a reachable list (`prio_tie_inv`) and is dropped; `insert_many`'s
agreement with one-by-one inserts is checked by `decide` on examples only.
Not modelled: the detectors, the `ConditionCheck` closures (except
`windowCheck`), lock order, events and re-arm.
-/

namespace Lambo.Hotlist

open Lambo.Lib (eq_of_nodup_map length_le_of_nodup_subset)

/-! ## Conditions, payloads, entries -/

/-- `Condition` (`src/daemon/hotlist.rs`). -/
inductive Cond
  | conflict
  | highRisk
  | drift
  | stale
  | contradiction
  deriving DecidableEq, Repr

/-- `Condition::severity`: conflict and contradiction share the top tier.
Pinned by `formal_constants_tests::hotlist`. -/
def Cond.severity : Cond → Nat
  | .conflict => 3
  | .contradiction => 3
  | .highRisk => 2
  | .drift => 1
  | .stale => 0

/-- `HotListPayload` (`src/types/mod.rs`), keeping the fields the model reads. -/
inductive Payload (ν : Type)
  | conflict (secondsAgo : Nat)
  | highRisk
  | drift (hops : Nat)
  | stale (secondsInactive : Nat)
  | contradiction (byNode : ν) (at_ : Int)
  deriving DecidableEq, Repr

/-- The condition a payload belongs to. -/
def Payload.kind {ν : Type} : Payload ν → Cond
  | .conflict _ => .conflict
  | .highRisk => .highRisk
  | .drift _ => .drift
  | .stale _ => .stale
  | .contradiction _ _ => .contradiction

/-- `HotListEntry` without its predicate (the predicate is `check` below). -/
structure Entry (ν : Type) where
  node : ν
  cond : Cond
  payload : Payload ν
  seq : Nat
  deriving DecidableEq, Repr

/-- `HotList`. -/
structure HotList (ν : Type) where
  max : Nat
  cmax : Nat
  entries : List (Entry ν)
  nextSeq : Nat
  deriving DecidableEq, Repr

variable {ν : Type}

/-- The dedup key `(node, condition)`. -/
def key (e : Entry ν) : ν × Cond := (e.node, e.cond)

/-- `default_contradiction_max`: half the bound, rounded down.
Pinned by `formal_constants_tests::hotlist`. -/
def defaultCmax (max : Nat) : Nat := max / 2

/-- `HOT_LIST_MAX`.
Pinned by `formal_constants_tests::hotlist`. -/
def hotListMax : Nat := 1000

/-- `HotList::with_budget`. -/
def withBudget (max cmax : Nat) : HotList ν := ⟨max, cmax, [], 0⟩

/-- `HotList::with_max`. -/
def withMax (max : Nat) : HotList ν := withBudget max (defaultCmax max)

/-- `HotList::new`. -/
def new : HotList ν := withMax hotListMax

/-! ## Priority and the sort -/

/-- `higher_priority` as a "ranks at or above" test: severity desc, then seq
desc. The trailing node-id arm is left out; `prio_tie_inv` shows it never
decides on a reachable list. -/
def prioLe (a b : Entry ν) : Bool :=
  decide (b.cond.severity < a.cond.severity) ||
    (decide (a.cond.severity = b.cond.severity) && decide (b.seq ≤ a.seq))

theorem prioLe_total (a b : Entry ν) : prioLe a b = true ∨ prioLe b a = true := by
  unfold prioLe
  by_cases h1 : b.cond.severity < a.cond.severity
  · simp [h1]
  · by_cases h2 : a.cond.severity < b.cond.severity
    · simp [h2]
    · have he : a.cond.severity = b.cond.severity := by omega
      by_cases h3 : b.seq ≤ a.seq
      · simp [he, h3]
      · have : a.seq ≤ b.seq := by omega
        simp [he, this]

theorem prioLe_trans (a b c : Entry ν) (hab : prioLe a b = true) (hbc : prioLe b c = true) :
    prioLe a c = true := by
  unfold prioLe at *
  simp only [Bool.or_eq_true, Bool.and_eq_true, decide_eq_true_eq] at *
  omega

/-- Insert into a priority-descending list, before the first entry it ranks
at or above (so an earlier input stays ahead of an equal one: stable). -/
def insertPrio (x : Entry ν) : List (Entry ν) → List (Entry ν)
  | [] => [x]
  | y :: ys => if prioLe x y then x :: y :: ys else y :: insertPrio x ys

/-- `entries.sort_by(higher_priority)` (a stable sort). -/
def sortPrio : List (Entry ν) → List (Entry ν)
  | [] => []
  | x :: xs => insertPrio x (sortPrio xs)

theorem insertPrio_perm (x : Entry ν) : ∀ l : List (Entry ν), (insertPrio x l).Perm (x :: l)
  | [] => by simp [insertPrio]
  | y :: ys => by
    unfold insertPrio
    split
    · exact List.Perm.refl _
    · exact ((insertPrio_perm x ys).cons y).trans (List.Perm.swap x y ys)

theorem sortPrio_perm : ∀ l : List (Entry ν), (sortPrio l).Perm l
  | [] => by simp [sortPrio]
  | x :: xs => by
    unfold sortPrio
    exact (insertPrio_perm x _).trans ((sortPrio_perm xs).cons x)

/-- Ranks at or above, as a `Prop`. -/
def Above (a b : Entry ν) : Prop := prioLe a b = true

theorem insertPrio_pairwise (x : Entry ν) :
    ∀ l : List (Entry ν), l.Pairwise Above → (insertPrio x l).Pairwise Above
  | [], _ => by simp [insertPrio]
  | y :: ys, h => by
    unfold insertPrio
    split
    · rename_i hxy
      refine List.Pairwise.cons ?_ h
      intro z hz
      cases List.mem_cons.mp hz with
      | inl hz => subst hz; exact hxy
      | inr hz => exact prioLe_trans x y z hxy (List.rel_of_pairwise_cons h hz)
    · rename_i hxy
      have hyx : prioLe y x = true := by
        cases prioLe_total x y with
        | inl h' => exact absurd h' hxy
        | inr h' => exact h'
      refine List.Pairwise.cons ?_ (insertPrio_pairwise x ys (List.Pairwise.of_cons h))
      intro z hz
      have hz' := (insertPrio_perm x ys).mem_iff.mp hz
      cases List.mem_cons.mp hz' with
      | inl hz => subst hz; exact hyx
      | inr hz => exact List.rel_of_pairwise_cons h hz

/-- The sorted list is priority-descending. -/
theorem sortPrio_pairwise : ∀ l : List (Entry ν), (sortPrio l).Pairwise Above
  | [] => by simp [sortPrio]
  | x :: xs => insertPrio_pairwise x _ (sortPrio_pairwise xs)

/-! ## The contradiction budget and the bound (`enforce`) -/

/-- Is this a `Contradiction` entry? -/
def isC (e : Entry ν) : Bool := decide (e.cond = .contradiction)

/-- Number of contradiction entries. -/
def countC (l : List (Entry ν)) : Nat := (l.filter isC).length

/-- Number of other entries. -/
def countO (l : List (Entry ν)) : Nat := (l.filter (fun e => !isC e)).length

/-- The partition in `enforce`: a contradiction is kept while fewer than
`cmax` contradictions have been seen (`seen += 1; seen <= budget`). -/
def budgetKept (cmax : Nat) : Nat → List (Entry ν) → List (Entry ν)
  | _, [] => []
  | seen, e :: rest =>
    if isC e then
      if seen < cmax then e :: budgetKept cmax (seen + 1) rest
      else budgetKept cmax (seen + 1) rest
    else e :: budgetKept cmax seen rest

/-- What that partition drops. -/
def budgetDropped (cmax : Nat) : Nat → List (Entry ν) → List (Entry ν)
  | _, [] => []
  | seen, e :: rest =>
    if isC e then
      if seen < cmax then budgetDropped cmax (seen + 1) rest
      else e :: budgetDropped cmax (seen + 1) rest
    else budgetDropped cmax seen rest

/-- `HotList::enforce` on a list: sort, apply the budget, then the bound.
Returns `(kept, evicted)`, evicted being the budget's drops then the bound's. -/
def enforce (max cmax : Nat) (es : List (Entry ν)) : List (Entry ν) × List (Entry ν) :=
  let sorted := sortPrio es
  let kept := budgetKept cmax 0 sorted
  (kept.take max, budgetDropped cmax 0 sorted ++ kept.drop max)

section Budget

variable (cmax : Nat)

theorem budget_perm : ∀ (seen : Nat) (l : List (Entry ν)),
    (budgetKept cmax seen l ++ budgetDropped cmax seen l).Perm l
  | _, [] => by simp [budgetKept, budgetDropped]
  | seen, e :: rest => by
    unfold budgetKept budgetDropped
    by_cases hc : isC e = true
    · by_cases hs : seen < cmax
      · simp only [hc, hs, ite_true, List.cons_append]
        exact (budget_perm (seen + 1) rest).cons e
      · simp only [hc, hs, ite_true, ite_false]
        exact List.perm_middle.trans ((budget_perm (seen + 1) rest).cons e)
    · simp only [hc, ite_false, List.cons_append, Bool.false_eq_true]
      exact (budget_perm seen rest).cons e

theorem budgetKept_sublist : ∀ (seen : Nat) (l : List (Entry ν)),
    (budgetKept cmax seen l).Sublist l
  | _, [] => by simp [budgetKept]
  | seen, e :: rest => by
    unfold budgetKept
    split
    · split
      · exact (budgetKept_sublist (seen + 1) rest).cons_cons e
      · exact (budgetKept_sublist (seen + 1) rest).cons e
    · exact (budgetKept_sublist seen rest).cons_cons e

/-- The budget drops contradictions only. -/
theorem budgetDropped_isC : ∀ (seen : Nat) (l : List (Entry ν)) (d : Entry ν),
    d ∈ budgetDropped cmax seen l → isC d = true
  | _, [], d, h => by simp [budgetDropped] at h
  | seen, e :: rest, d, h => by
    unfold budgetDropped at h
    split at h
    · rename_i hc
      split at h
      · exact budgetDropped_isC (seen + 1) rest d h
      · cases List.mem_cons.mp h with
        | inl h => subst h; exact hc
        | inr h => exact budgetDropped_isC (seen + 1) rest d h
    · exact budgetDropped_isC seen rest d h

/-- Every entry of another kind survives the budget. -/
theorem mem_budgetKept_of_not_isC : ∀ (seen : Nat) (l : List (Entry ν)) (e : Entry ν),
    e ∈ l → isC e = false → e ∈ budgetKept cmax seen l
  | _, [], e, h, _ => by simp at h
  | seen, x :: rest, e, h, he => by
    unfold budgetKept
    cases List.mem_cons.mp h with
    | inl h =>
      subst h
      simp [he]
    | inr h =>
      split
      · split
        · exact List.mem_cons_of_mem _ (mem_budgetKept_of_not_isC (seen + 1) rest e h he)
        · exact mem_budgetKept_of_not_isC (seen + 1) rest e h he
      · exact List.mem_cons_of_mem _ (mem_budgetKept_of_not_isC seen rest e h he)

theorem countC_cons (e : Entry ν) (l : List (Entry ν)) :
    countC (e :: l) = (if isC e then 1 else 0) + countC l := by
  unfold countC
  by_cases h : isC e = true
  · simp [h]; omega
  · simp [h]

theorem countO_cons (e : Entry ν) (l : List (Entry ν)) :
    countO (e :: l) = (if isC e then 0 else 1) + countO l := by
  unfold countO
  by_cases h : isC e = true
  · simp [h]
  · simp [h]; omega

theorem countC_le_length (l : List (Entry ν)) : countC l ≤ l.length := by
  unfold countC; exact List.length_filter_le _ _

theorem countC_sublist {l₁ l₂ : List (Entry ν)} (h : l₁.Sublist l₂) : countC l₁ ≤ countC l₂ := by
  unfold countC; exact (h.filter _).length_le

theorem countC_perm {l₁ l₂ : List (Entry ν)} (h : l₁.Perm l₂) : countC l₁ = countC l₂ := by
  unfold countC; exact (h.filter _).length_eq

theorem countO_perm {l₁ l₂ : List (Entry ν)} (h : l₁.Perm l₂) : countO l₁ = countO l₂ := by
  unfold countO; exact (h.filter _).length_eq

/-- At most `cmax - seen` contradictions survive the budget. -/
theorem countC_budgetKept : ∀ (seen : Nat) (l : List (Entry ν)),
    countC (budgetKept cmax seen l) ≤ cmax - seen
  | _, [] => by simp [budgetKept, countC]
  | seen, e :: rest => by
    unfold budgetKept
    by_cases hc : isC e = true
    · by_cases hs : seen < cmax
      · simp only [hc, hs, ite_true]
        rw [countC_cons]
        have := countC_budgetKept (seen + 1) rest
        simp only [hc, ite_true]
        omega
      · simp only [hc, hs, ite_true, ite_false]
        have := countC_budgetKept (seen + 1) rest
        omega
    · simp only [hc, ite_false, Bool.false_eq_true]
      rw [countC_cons]
      have := countC_budgetKept seen rest
      simp only [hc, ite_false, Bool.false_eq_true]
      omega

/-- The budget's survivors number at most the other kinds plus `cmax - seen`. -/
theorem length_budgetKept : ∀ (seen : Nat) (l : List (Entry ν)),
    (budgetKept cmax seen l).length ≤ countO l + (cmax - seen)
  | _, [] => by simp [budgetKept]
  | seen, e :: rest => by
    unfold budgetKept
    rw [countO_cons]
    by_cases hc : isC e = true
    · by_cases hs : seen < cmax
      · simp only [hc, hs, ite_true, List.length_cons]
        have := length_budgetKept (seen + 1) rest
        omega
      · simp only [hc, hs, ite_true, ite_false]
        have := length_budgetKept (seen + 1) rest
        omega
    · simp only [hc, ite_false, Bool.false_eq_true, List.length_cons]
      have := length_budgetKept seen rest
      omega

/-- The budget drops exactly the contradictions beyond `cmax - seen`. -/
theorem length_budgetDropped : ∀ (seen : Nat) (l : List (Entry ν)),
    (budgetDropped cmax seen l).length = countC l - (cmax - seen)
  | _, [] => by simp [budgetDropped, countC]
  | seen, e :: rest => by
    unfold budgetDropped
    rw [countC_cons]
    by_cases hc : isC e = true
    · by_cases hs : seen < cmax
      · simp only [hc, hs, ite_true]
        have := length_budgetDropped (seen + 1) rest
        omega
      · simp only [hc, hs, ite_true, ite_false, List.length_cons]
        have := length_budgetDropped (seen + 1) rest
        omega
    · simp only [hc, ite_false, Bool.false_eq_true]
      have := length_budgetDropped seen rest
      omega

/-- Within the budget the partition keeps everything (the `over > 0` guard in
`enforce` is therefore only a shortcut). -/
theorem budgetKept_of_countC_le : ∀ (seen : Nat) (l : List (Entry ν)),
    seen + countC l ≤ cmax → budgetKept cmax seen l = l
  | _, [], _ => by simp [budgetKept]
  | seen, e :: rest, h => by
    unfold budgetKept
    rw [countC_cons] at h
    by_cases hc : isC e = true
    · simp only [hc, ite_true] at h ⊢
      have hs : seen < cmax := by omega
      simp only [hs, ite_true]
      rw [budgetKept_of_countC_le (seen + 1) rest (by omega)]
    · simp only [hc, ite_false, Bool.false_eq_true] at h ⊢
      rw [budgetKept_of_countC_le seen rest (by omega)]

/-- Once the budget is used up, no further contradiction is kept. -/
theorem budgetKept_no_isC_of_le : ∀ (seen : Nat) (l : List (Entry ν)) (k : Entry ν),
    cmax ≤ seen → k ∈ budgetKept cmax seen l → isC k = false
  | _, [], _, _, h => by simp [budgetKept] at h
  | seen, e :: rest, k, hle, h => by
    unfold budgetKept at h
    split at h
    · split at h
      · omega
      · exact budgetKept_no_isC_of_le (seen + 1) rest k (by omega) h
    · rename_i hc
      cases List.mem_cons.mp h with
      | inl h => subst h; simpa using hc
      | inr h => exact budgetKept_no_isC_of_le seen rest k hle h

/-- On a list ordered by `R`, every contradiction the budget keeps comes
before (is `R`-related to) every contradiction it drops. -/
theorem budget_kept_before_dropped {R : Entry ν → Entry ν → Prop} :
    ∀ (seen : Nat) (l : List (Entry ν)), l.Pairwise R →
      ∀ k ∈ budgetKept cmax seen l, isC k = true →
      ∀ d ∈ budgetDropped cmax seen l, R k d
  | _, [], _ => by simp [budgetKept]
  | seen, e :: rest, hp => by
    intro k hk hkc d hd
    unfold budgetKept at hk
    unfold budgetDropped at hd
    have hsub : ∀ {seen'}, d ∈ budgetDropped cmax seen' rest → d ∈ rest := fun h =>
      (List.Perm.mem_iff (budget_perm cmax _ rest)).mp (List.mem_append_right _ h)
    by_cases hc : isC e = true
    · by_cases hs : seen < cmax
      · simp only [hc, hs, ite_true] at hk hd
        cases List.mem_cons.mp hk with
        | inl h => subst h; exact List.rel_of_pairwise_cons hp (hsub hd)
        | inr h =>
          exact budget_kept_before_dropped (seen + 1) rest (List.Pairwise.of_cons hp) k h hkc d hd
      · simp only [hc, hs, ite_true, ite_false] at hk hd
        have := budgetKept_no_isC_of_le cmax (seen + 1) rest k (by omega) hk
        rw [hkc] at this
        exact absurd this (by decide)
    · simp only [hc, ite_false, Bool.false_eq_true] at hk hd
      cases List.mem_cons.mp hk with
      | inl h => subst h; rw [hkc] at hc; exact absurd rfl hc
      | inr h =>
        exact budget_kept_before_dropped seen rest (List.Pairwise.of_cons hp) k h hkc d hd

end Budget

section Enforce

variable (max cmax : Nat) (es : List (Entry ν))

/-- Nothing is lost or invented: kept and evicted together are the input. -/
theorem enforce_perm : ((enforce max cmax es).1 ++ (enforce max cmax es).2).Perm es := by
  unfold enforce
  simp only
  have hk := budget_perm cmax 0 (sortPrio es)
  have hs := sortPrio_perm es
  -- take ++ (dropped ++ drop) ~ (take ++ drop) ++ dropped = kept ++ dropped
  have h1 : ((budgetKept cmax 0 (sortPrio es)).take max ++
      (budgetDropped cmax 0 (sortPrio es) ++ (budgetKept cmax 0 (sortPrio es)).drop max)).Perm
      (budgetKept cmax 0 (sortPrio es) ++ budgetDropped cmax 0 (sortPrio es)) := by
    have := (List.perm_append_comm (l₁ := budgetDropped cmax 0 (sortPrio es))
      (l₂ := (budgetKept cmax 0 (sortPrio es)).drop max)).append_left
      ((budgetKept cmax 0 (sortPrio es)).take max)
    refine this.trans ?_
    rw [← List.append_assoc, List.take_append_drop]
  exact h1.trans (hk.trans hs)

/-- **The bound.** -/
theorem enforce_length_le : (enforce max cmax es).1.length ≤ max := by
  unfold enforce; simp only [List.length_take]; exact Nat.min_le_left _ _

/-- **The contradiction budget** (#196). -/
theorem enforce_countC_le : countC (enforce max cmax es).1 ≤ cmax := by
  unfold enforce
  simp only
  have := countC_budgetKept cmax 0 (sortPrio es)
  have h2 := countC_sublist (List.take_sublist max (budgetKept cmax 0 (sortPrio es)))
  omega

theorem enforce_sublist_sorted : (enforce max cmax es).1.Sublist (sortPrio es) := by
  unfold enforce
  exact (List.take_sublist _ _).trans (budgetKept_sublist cmax 0 _)

/-- `HotList::enforce` keeps its entries in priority order. -/
theorem enforce_pairwise : (enforce max cmax es).1.Pairwise Above :=
  (sortPrio_pairwise es).sublist (enforce_sublist_sorted max cmax es)

theorem mem_enforce {e : Entry ν} (h : e ∈ (enforce max cmax es).1) : e ∈ es :=
  (sortPrio_perm es).mem_iff.mp ((enforce_sublist_sorted max cmax es).subset h)

/-- **Lowest evicted.** Every evicted entry ranks at or below every kept
one, except a contradiction dropped by the budget, which may outrank a kept
entry of another kind (and still ranks below every kept contradiction). -/
theorem enforce_evicts_lowest (k d : Entry ν) (hk : k ∈ (enforce max cmax es).1)
    (hd : d ∈ (enforce max cmax es).2) :
    Above k d ∨ (isC d = true ∧ isC k = false) := by
  unfold enforce at hk hd
  simp only at hk hd
  have hpk : (budgetKept cmax 0 (sortPrio es)).Pairwise Above :=
    (sortPrio_pairwise es).sublist (budgetKept_sublist cmax 0 _)
  cases List.mem_append.mp hd with
  | inl hd =>
    have hdc := budgetDropped_isC cmax 0 _ d hd
    cases hkc : isC k with
    | false => exact Or.inr ⟨hdc, rfl⟩
    | true =>
      left
      exact budget_kept_before_dropped cmax 0 (sortPrio es) (sortPrio_pairwise es) k
        ((List.take_sublist _ _).subset hk) hkc d hd
  | inr hd =>
    left
    have := hpk
    rw [← List.take_append_drop max (budgetKept cmax 0 (sortPrio es))] at this
    exact (List.pairwise_append.mp this).2.2 k hk d hd

/-- `HotList::enforce`'s contradiction budget evicts contradictions only
(#196): an entry of another kind is evicted only by the bound. -/
theorem enforce_evicted_other_by_bound (d : Entry ν) (hd : d ∈ (enforce max cmax es).2)
    (hdc : isC d = false) : d ∈ (budgetKept cmax 0 (sortPrio es)).drop max := by
  unfold enforce at hd
  cases List.mem_append.mp hd with
  | inl h => rw [budgetDropped_isC cmax 0 _ d h] at hdc; exact absurd hdc (by decide)
  | inr h => exact h

/-- **The reserved part** (#196): when the other kinds number at most
`max - cmax`, no number of contradictions evicts any of them. -/
theorem enforce_keeps_reserved (hO : countO es ≤ max - cmax) (e : Entry ν) (he : e ∈ es)
    (hec : isC e = false) : e ∈ (enforce max cmax es).1 := by
  unfold enforce
  simp only
  have hmem : e ∈ budgetKept cmax 0 (sortPrio es) :=
    mem_budgetKept_of_not_isC cmax 0 _ e ((sortPrio_perm es).mem_iff.mpr he) hec
  have hlen := length_budgetKept cmax 0 (sortPrio es)
  have hO' : countO (sortPrio es) = countO es := countO_perm (sortPrio_perm es)
  by_cases hle : cmax ≤ max
  · have : (budgetKept cmax 0 (sortPrio es)).length ≤ max := by omega
    rw [List.take_of_length_le this]
    exact hmem
  · -- `max - cmax = 0`: there are no entries of another kind
    have h0 : countO es = 0 := by omega
    unfold countO at h0
    have := List.length_eq_zero_iff.mp h0
    have hf : e ∈ es.filter (fun e => !isC e) := List.mem_filter.mpr ⟨he, by simp [hec]⟩
    rw [this] at hf
    simp at hf

/-- **Within both limits nothing is evicted.** -/
theorem enforce_noop (hl : es.length ≤ max) (hc : countC es ≤ cmax) :
    (enforce max cmax es).2 = [] ∧ (enforce max cmax es).1.Perm es := by
  have hcs : countC (sortPrio es) ≤ cmax := by rw [countC_perm (sortPrio_perm es)]; exact hc
  have hb := budgetKept_of_countC_le cmax 0 (sortPrio es) (by omega)
  have hd : (budgetDropped cmax 0 (sortPrio es)).length = 0 := by
    rw [length_budgetDropped]; omega
  have hlen : (sortPrio es).length ≤ max := by rw [(sortPrio_perm es).length_eq]; exact hl
  unfold enforce
  simp only [hb, List.length_eq_zero_iff.mp hd, List.nil_append, List.take_of_length_le hlen,
    List.drop_of_length_le hlen]
  exact ⟨trivial, sortPrio_perm es⟩

/-- **Under the budget nothing is evicted that the bound alone would not
evict** (#196). -/
theorem enforce_under_budget (hc : countC es ≤ cmax) :
    (enforce max cmax es).1 = (sortPrio es).take max := by
  have hcs : countC (sortPrio es) ≤ cmax := by rw [countC_perm (sortPrio_perm es)]; exact hc
  unfold enforce
  simp only [budgetKept_of_countC_le cmax 0 (sortPrio es) (by omega)]

/-- How many are evicted: the contradictions over the budget, then whatever
still exceeds the bound. -/
theorem enforce_evicted_length :
    (enforce max cmax es).2.length
      = (countC es - cmax) + ((es.length - (countC es - cmax)) - max) := by
  have hp := budget_perm cmax 0 (sortPrio es)
  have hlen := hp.length_eq
  rw [List.length_append, length_budgetDropped, (sortPrio_perm es).length_eq,
    countC_perm (sortPrio_perm es)] at hlen
  unfold enforce
  simp only [List.length_append, List.length_drop, length_budgetDropped,
    countC_perm (sortPrio_perm es)]
  omega

end Enforce

/-! ## Insert, refresh, batch insert -/

section Insert

variable [DecidableEq ν]

/-- The `find`-then-replace-or-push of `insert`: the first entry with the same
`(node, condition)` is replaced in place, else the entry is pushed. -/
def upsert (e : Entry ν) : List (Entry ν) → List (Entry ν)
  | [] => [e]
  | x :: xs => if key x = key e then e :: xs else x :: upsert e xs

/-- The new entry for an insert: the next `seq`. -/
def stamp (l : HotList ν) (node : ν) (cond : Cond) (payload : Payload ν) : Entry ν :=
  ⟨node, cond, payload, l.nextSeq⟩

/-- `HotList::insert`, returning every entry `enforce` evicted. -/
def insertFull (l : HotList ν) (node : ν) (cond : Cond) (payload : Payload ν) :
    HotList ν × List (Entry ν) :=
  let r := enforce l.max l.cmax (upsert (stamp l node cond payload) l.entries)
  ({ l with entries := r.1, nextSeq := l.nextSeq + 1 }, r.2)

/-- `HotList::insert`: `self.enforce().pop()`, the last entry evicted. -/
def insert (l : HotList ν) (node : ν) (cond : Cond) (payload : Payload ν) :
    HotList ν × Option (Entry ν) :=
  let r := insertFull l node cond payload
  (r.1, r.2.getLast?)

/-- Insert each `(node, condition, payload)` in turn. -/
def insertAll (l : HotList ν) : List (ν × Cond × Payload ν) → HotList ν
  | [] => l
  | (n, c, p) :: rest => insertAll (insert l n c p).1 rest

/-- `HotList::insert_many`: each entry gets the next `seq` and is refreshed
in place or pushed, then the budget and the bound are enforced once. -/
def upsertAll (l : HotList ν) : List (ν × Cond × Payload ν) → HotList ν
  | [] => l
  | (n, c, p) :: rest =>
    upsertAll { l with entries := upsert (stamp l n c p) l.entries, nextSeq := l.nextSeq + 1 } rest

def insertMany (l : HotList ν) (es : List (ν × Cond × Payload ν)) : HotList ν × List (Entry ν) :=
  let u := upsertAll l es
  let r := enforce u.max u.cmax u.entries
  ({ u with entries := r.1 }, r.2)

theorem upsert_perm_of_mem (e : Entry ν) :
    ∀ l : List (Entry ν), key e ∈ l.map key → ∃ x ∈ l, key x = key e ∧ (upsert e l).Perm (e :: l.erase x)
  | [], h => by simp at h
  | x :: xs, h => by
    unfold upsert
    by_cases hx : key x = key e
    · simp only [hx, ite_true]
      exact ⟨x, List.mem_cons_self, hx, by simp⟩
    · simp only [hx, ite_false]
      have h' : key e ∈ xs.map key := by
        simp only [List.map_cons, List.mem_cons] at h
        cases h with
        | inl h => exact absurd h.symm hx
        | inr h => exact h
      obtain ⟨y, hy, hyk, hp⟩ := upsert_perm_of_mem e xs h'
      refine ⟨y, List.mem_cons_of_mem _ hy, hyk, ?_⟩
      have hne : x ≠ y := fun h => hx (h ▸ hyk)
      rw [List.erase_cons_tail (by simpa using hne)]
      exact (hp.cons x).trans (List.Perm.swap e x _)

theorem upsert_of_not_mem (e : Entry ν) :
    ∀ l : List (Entry ν), key e ∉ l.map key → upsert e l = l ++ [e]
  | [], _ => by simp [upsert]
  | x :: xs, h => by
    unfold upsert
    simp only [List.map_cons, List.mem_cons, not_or] at h
    simp only [Ne.symm h.1, ite_false, List.cons_append]
    rw [upsert_of_not_mem e xs h.2]

theorem mem_upsert {e y : Entry ν} : ∀ {l : List (Entry ν)}, y ∈ upsert e l → y = e ∨ y ∈ l
  | [], h => by simp [upsert] at h; exact Or.inl h
  | x :: xs, h => by
    unfold upsert at h
    split at h
    · cases List.mem_cons.mp h with
      | inl h => exact Or.inl h
      | inr h => exact Or.inr (List.mem_cons_of_mem _ h)
    · cases List.mem_cons.mp h with
      | inl h => exact Or.inr (h ▸ List.mem_cons_self)
      | inr h =>
        cases mem_upsert h with
        | inl h => exact Or.inl h
        | inr h => exact Or.inr (List.mem_cons_of_mem _ h)

theorem self_mem_upsert (e : Entry ν) : ∀ l : List (Entry ν), e ∈ upsert e l
  | [] => by simp [upsert]
  | x :: xs => by
    unfold upsert
    split
    · exact List.mem_cons_self
    · exact List.mem_cons_of_mem _ (self_mem_upsert e xs)

/-- The keys after an upsert: the old ones plus the new one. -/
theorem key_mem_upsert {e : Entry ν} {l : List (Entry ν)} (k : ν × Cond) :
    k ∈ (upsert e l).map key ↔ k ∈ l.map key ∨ k = key e := by
  by_cases hm : key e ∈ l.map key
  · obtain ⟨x, hx, hxk, hp⟩ := upsert_perm_of_mem e l hm
    rw [(hp.map key).mem_iff]
    constructor
    · intro h
      simp only [List.map_cons, List.mem_cons] at h
      cases h with
      | inl h => exact Or.inr h
      | inr h =>
        obtain ⟨y, hy, rfl⟩ := List.mem_map.mp h
        exact Or.inl (List.mem_map_of_mem (List.mem_of_mem_erase hy))
    · intro h
      simp only [List.map_cons, List.mem_cons]
      cases h with
      | inr h => exact Or.inl h
      | inl h =>
        obtain ⟨y, hy, rfl⟩ := List.mem_map.mp h
        by_cases hxy : y = x
        · subst hxy; exact Or.inl hxk
        · exact Or.inr (List.mem_map_of_mem ((List.mem_erase_of_ne hxy).mpr hy))
  · rw [upsert_of_not_mem e l hm]
    simp

/-- **One entry per `(node, condition)`**, preserved by upsert. -/
theorem upsert_keys_nodup (e : Entry ν) :
    ∀ l : List (Entry ν), (l.map key).Nodup → ((upsert e l).map key).Nodup
  | [], _ => by simp [upsert]
  | x :: xs, h => by
    unfold upsert
    have hx := List.nodup_cons.mp (show (key x :: xs.map key).Nodup from h)
    split
    · rename_i hk
      simpa [← hk] using h
    · rename_i hk
      simp only [List.map_cons]
      refine List.nodup_cons.mpr ⟨?_, upsert_keys_nodup e xs hx.2⟩
      intro hm
      cases (key_mem_upsert (key x)).mp hm with
      | inl h => exact hx.1 h
      | inr h => exact hk h

theorem length_upsert_le (e : Entry ν) (l : List (Entry ν)) :
    (upsert e l).length ≤ l.length + 1 := by
  by_cases hm : key e ∈ l.map key
  · obtain ⟨x, hx, _, hp⟩ := upsert_perm_of_mem e l hm
    rw [hp.length_eq, List.length_cons, List.length_erase_of_mem hx]
    have := List.length_pos_of_mem hx
    omega
  · rw [upsert_of_not_mem e l hm]; simp

theorem length_upsert_of_mem (e : Entry ν) (l : List (Entry ν)) (hm : key e ∈ l.map key) :
    (upsert e l).length = l.length := by
  obtain ⟨x, hx, _, hp⟩ := upsert_perm_of_mem e l hm
  rw [hp.length_eq, List.length_cons, List.length_erase_of_mem hx]
  have := List.length_pos_of_mem hx
  omega

omit [DecidableEq ν] in
theorem isC_of_key_eq {x e : Entry ν} (h : key x = key e) : isC x = isC e := by
  have : x.cond = e.cond := congrArg Prod.snd h
  simp [isC, this]

theorem countC_upsert_of_mem (e : Entry ν) (l : List (Entry ν)) (hm : key e ∈ l.map key) :
    countC (upsert e l) = countC l := by
  obtain ⟨x, hx, hxk, hp⟩ := upsert_perm_of_mem e l hm
  rw [countC_perm hp, countC_cons, countC_perm (List.perm_cons_erase hx), countC_cons,
    isC_of_key_eq hxk]

theorem countC_upsert_le (e : Entry ν) (l : List (Entry ν)) :
    countC (upsert e l) ≤ countC l + 1 := by
  by_cases hm : key e ∈ l.map key
  · rw [countC_upsert_of_mem e l hm]; omega
  · rw [upsert_of_not_mem e l hm, countC_perm (List.perm_append_singleton _ _), countC_cons]
    split <;> omega

theorem upsert_seq_nodup (e : Entry ν) :
    ∀ l : List (Entry ν), (l.map Entry.seq).Nodup → (∀ x ∈ l, x.seq < e.seq) →
      ((upsert e l).map Entry.seq).Nodup
  | [], _, _ => by simp [upsert]
  | x :: xs, h, hlt => by
    unfold upsert
    have hx := List.nodup_cons.mp (show (x.seq :: xs.map Entry.seq).Nodup from h)
    have hlt' : ∀ y ∈ xs, y.seq < e.seq := fun y hy => hlt y (List.mem_cons_of_mem _ hy)
    split
    · simp only [List.map_cons]
      refine List.nodup_cons.mpr ⟨?_, hx.2⟩
      intro hm
      obtain ⟨y, hy, hys⟩ := List.mem_map.mp hm
      have := hlt' y hy
      omega
    · simp only [List.map_cons]
      refine List.nodup_cons.mpr ⟨?_, upsert_seq_nodup e xs hx.2 hlt'⟩
      intro hm
      obtain ⟨y, hy, hys⟩ := List.mem_map.mp hm
      cases mem_upsert hy with
      | inl h' =>
        subst h'
        have := hlt x List.mem_cons_self
        omega
      | inr h' => exact hx.1 (List.mem_map.mpr ⟨y, h', hys⟩)

end Insert

/-! ## The invariant every reachable list satisfies -/

theorem enforce_map_nodup {α : Type} (f : Entry ν → α) (max cmax : Nat) (es : List (Entry ν))
    (h : (es.map f).Nodup) : ((enforce max cmax es).1.map f).Nodup :=
  (((sortPrio_perm es).map f).nodup_iff.mpr h).sublist
    ((enforce_sublist_sorted max cmax es).map f)

/-- What every reachable hot list satisfies. -/
structure Inv (l : HotList ν) : Prop where
  /-- One entry per `(node, condition)`. -/
  keys_nodup : (l.entries.map key).Nodup
  /-- No two entries share a `seq`. -/
  seq_nodup : (l.entries.map Entry.seq).Nodup
  /-- Every `seq` was issued. -/
  seq_lt : ∀ e ∈ l.entries, e.seq < l.nextSeq
  /-- The bound. -/
  len_le : l.entries.length ≤ l.max
  /-- The contradiction budget. -/
  countC_le : countC l.entries ≤ l.cmax

theorem inv_withBudget (max cmax : Nat) : Inv (withBudget max cmax : HotList ν) :=
  ⟨by simp [withBudget], by simp [withBudget], by simp [withBudget], by simp [withBudget],
    by simp [withBudget, countC]⟩

/-- A new `HotList` starts with its bound, budget and one-entry-per-condition
invariant holding. -/
theorem inv_new : Inv (new : HotList ν) := inv_withBudget _ _

/-- **No two live entries share a seq, so the comparator's node-id arm never
decides** (the comment on `higher_priority`): two entries of a reachable list
that rank at or above each other are the same entry. -/
theorem prio_tie_inv {l : HotList ν} (hl : Inv l) {a b : Entry ν} (ha : a ∈ l.entries)
    (hb : b ∈ l.entries) (hab : Above a b) (hba : Above b a) : a = b := by
  unfold Above prioLe at hab hba
  simp only [Bool.or_eq_true, Bool.and_eq_true, decide_eq_true_eq] at hab hba
  exact eq_of_nodup_map Entry.seq hl.seq_nodup ha hb (by omega)

section InsertInv

variable [DecidableEq ν]

theorem insertFull_entries (l : HotList ν) (n : ν) (c : Cond) (p : Payload ν) :
    (insertFull l n c p).1.entries = (enforce l.max l.cmax (upsert (stamp l n c p) l.entries)).1 :=
  rfl

theorem insertFull_inv {l : HotList ν} (hl : Inv l) (n : ν) (c : Cond) (p : Payload ν) :
    Inv (insertFull l n c p).1 := by
  refine ⟨?_, ?_, ?_, ?_, ?_⟩
  · rw [insertFull_entries]
    exact enforce_map_nodup key _ _ _ (upsert_keys_nodup _ _ hl.keys_nodup)
  · rw [insertFull_entries]
    exact enforce_map_nodup Entry.seq _ _ _
      (upsert_seq_nodup _ _ hl.seq_nodup (fun x hx => hl.seq_lt x hx))
  · intro e he
    rw [insertFull_entries] at he
    have := mem_upsert (mem_enforce _ _ _ he)
    show e.seq < l.nextSeq + 1
    cases this with
    | inl h => subst h; simp [stamp]
    | inr h => have := hl.seq_lt e h; omega
  · exact enforce_length_le _ _ _
  · exact enforce_countC_le _ _ _

theorem insert_inv {l : HotList ν} (hl : Inv l) (n : ν) (c : Cond) (p : Payload ν) :
    Inv (insert l n c p).1 := insertFull_inv hl n c p

theorem insertAll_inv : ∀ {l : HotList ν} (_ : Inv l) (es : List (ν × Cond × Payload ν)),
    Inv (insertAll l es)
  | _, hl, [] => hl
  | _, hl, (n, c, p) :: rest => insertAll_inv (insert_inv hl n c p) rest

/-- **At most one entry is evicted per insert** ("The list was within both and
one entry was added, so at most one goes"), so `pop()` loses nothing. -/
theorem insertFull_evicts_le_one {l : HotList ν} (hl : Inv l) (n : ν) (c : Cond)
    (p : Payload ν) : (insertFull l n c p).2.length ≤ 1 := by
  show (enforce l.max l.cmax (upsert (stamp l n c p) l.entries)).2.length ≤ 1
  rw [enforce_evicted_length]
  have h1 := length_upsert_le (stamp l n c p) l.entries
  have h2 := countC_upsert_le (stamp l n c p) l.entries
  have h3 := countC_le_length (upsert (stamp l n c p) l.entries)
  have := hl.len_le
  have := hl.countC_le
  omega

/-- **Refresh** (`refresh_replaces_payload_and_restores_recency`): re-inserting a
present `(node, condition)` evicts nothing, keeps the length, and leaves the new
entry (new payload, newest `seq`) on the list. -/
theorem insert_refresh {l : HotList ν} (hl : Inv l) (n : ν) (c : Cond) (p : Payload ν)
    (hm : (n, c) ∈ l.entries.map key) :
    (insertFull l n c p).2 = [] ∧ (insertFull l n c p).1.entries.length = l.entries.length ∧
      stamp l n c p ∈ (insertFull l n c p).1.entries := by
  have hm' : key (stamp l n c p) ∈ l.entries.map key := hm
  have hlen := length_upsert_of_mem _ _ hm'
  have hc := countC_upsert_of_mem _ _ hm'
  have := enforce_noop l.max l.cmax (upsert (stamp l n c p) l.entries)
    (by rw [hlen]; exact hl.len_le) (by rw [hc]; exact hl.countC_le)
  refine ⟨this.1, ?_, ?_⟩
  · rw [insertFull_entries, this.2.length_eq, hlen]
  · rw [insertFull_entries, this.2.mem_iff]; exact self_mem_upsert _ _

/-- The keys after an insert that evicted nothing: the old ones plus the new. -/
theorem insert_keys_of_no_evict (l : HotList ν) (n : ν) (c : Cond) (p : Payload ν)
    (h : (insertFull l n c p).2 = []) (k : ν × Cond) :
    k ∈ (insertFull l n c p).1.entries.map key ↔ k ∈ l.entries.map key ∨ k = (n, c) := by
  have hp := enforce_perm l.max l.cmax (upsert (stamp l n c p) l.entries)
  have h' : (enforce l.max l.cmax (upsert (stamp l n c p) l.entries)).2 = [] := h
  rw [h', List.append_nil] at hp
  rw [insertFull_entries, (hp.map key).mem_iff, key_mem_upsert]
  rfl

end InsertInv

/-! ## Insertion order -/

section Order

variable [DecidableEq ν]

/-- Everything a run of single inserts evicted, in order. -/
def insertAllEvicted (l : HotList ν) : List (ν × Cond × Payload ν) → List (Entry ν)
  | [] => []
  | (n, c, p) :: rest =>
    (insertFull l n c p).2 ++ insertAllEvicted (insert l n c p).1 rest

/-- The `(node, condition)` of an insert request. -/
def reqKey (r : ν × Cond × Payload ν) : ν × Cond := (r.1, r.2.1)

/-- **While nothing is evicted, the live keys are the old ones plus the
inserted ones**, whatever the order of the inserts. -/
theorem insertAll_keys_of_no_evict :
    ∀ (l : HotList ν) (es : List (ν × Cond × Payload ν)), insertAllEvicted l es = [] →
      ∀ k, k ∈ (insertAll l es).entries.map key ↔ k ∈ l.entries.map key ∨ k ∈ es.map reqKey
  | l, [], _, k => by simp [insertAll]
  | l, (n, c, p) :: rest, h, k => by
    simp only [insertAllEvicted, List.append_eq_nil_iff] at h
    simp only [insertAll]
    rw [insertAll_keys_of_no_evict (insert l n c p).1 rest h.2 k]
    have := insert_keys_of_no_evict l n c p h.1 k
    simp only [insert] at this ⊢
    rw [this]
    simp only [List.map_cons, List.mem_cons, reqKey]
    constructor
    · rintro ((h | h) | h)
      · exact Or.inl h
      · exact Or.inr (Or.inl h)
      · exact Or.inr (Or.inr h)
    · rintro (h | h | h)
      · exact Or.inl (Or.inl h)
      · exact Or.inl (Or.inr h)
      · exact Or.inr h

/-- **Order of insertion does not change the live set while nothing is
evicted**: two runs over the same requests in any order, neither of which
evicts, leave the same `(node, condition)` pairs. (Once an insert evicts,
order matters: `order_matters_when_full`.) -/
theorem insertAll_order_irrelevant (l : HotList ν) (es₁ es₂ : List (ν × Cond × Payload ν))
    (hp : es₁.Perm es₂) (h₁ : insertAllEvicted l es₁ = []) (h₂ : insertAllEvicted l es₂ = [])
    (k : ν × Cond) :
    k ∈ (insertAll l es₁).entries.map key ↔ k ∈ (insertAll l es₂).entries.map key := by
  rw [insertAll_keys_of_no_evict l es₁ h₁, insertAll_keys_of_no_evict l es₂ h₂,
    (hp.map reqKey).mem_iff]

end Order

/-! ## Lapsing: the cycle's sync and recall's re-validation -/

section Lapse

variable [DecidableEq ν]

/-- `HotList::retain_conditions`: keep an entry iff its `(condition, node)` is
in this cycle's set. -/
def retain (held : List (Cond × ν)) (l : HotList ν) : HotList ν :=
  { l with entries := l.entries.filter fun e => decide ((e.cond, e.node) ∈ held) }

theorem mem_retain {held : List (Cond × ν)} {l : HotList ν} {e : Entry ν} :
    e ∈ (retain held l).entries ↔ e ∈ l.entries ∧ (e.cond, e.node) ∈ held := by
  simp [retain]

theorem retain_inv {l : HotList ν} (hl : Inv l) (held : List (Cond × ν)) :
    Inv (retain held l) := by
  have hs : ((retain held l).entries).Sublist l.entries := List.filter_sublist
  exact ⟨hl.keys_nodup.sublist (hs.map key), hl.seq_nodup.sublist (hs.map _),
    fun e he => hl.seq_lt e (hs.subset he), Nat.le_trans hs.length_le hl.len_le,
    Nat.le_trans (countC_sublist hs) hl.countC_le⟩

/-- One daemon cycle's hot-list step (`detect_into_hot_list`): every
detector's hits are inserted, in the daemon's order, and only then is the
list cut down to the cycle's condition set. Every hit goes through a single
`insert` here; Rust inserts the contradictions with one `insert_many`, which
agrees with single inserts on the `decide` examples only (see the header). -/
def cycle (l : HotList ν) (hits : List (ν × Cond × Payload ν)) (held : List (Cond × ν)) :
    HotList ν :=
  retain held (insertAll l hits)

/-- After a cycle every entry is one the cycle detected. -/
theorem cycle_sound (l : HotList ν) (hits : List (ν × Cond × Payload ν))
    (held : List (Cond × ν)) (e : Entry ν) (he : e ∈ (cycle l hits held).entries) :
    (e.cond, e.node) ∈ held :=
  (mem_retain.mp he).2

/-- **GC.** `gc::run` never touches the hot list; an entry for a collected
node stays until the next cycle, which drops it when the detectors report
only live nodes. -/
theorem cycle_drops_collected (live : ν → Bool) (l : HotList ν)
    (hits : List (ν × Cond × Payload ν)) (held : List (Cond × ν))
    (hheld : ∀ q ∈ held, live q.2 = true) (e : Entry ν) (he : e ∈ (cycle l hits held).entries) :
    live e.node = true :=
  hheld _ (cycle_sound l hits held e he)

/-- A re-validation predicate (`ConditionCheck`): whether `(node, condition)`
still holds against the current graph at `now`, and its rebuilt payload. The
graph is fixed for one recall, so it is folded into the function; the
detector's captured parameters (windows, thresholds) are constant config. -/
abbrev Check (ν : Type) := ν → Cond → Int → Option (Payload ν)

/-- One entry under `HotList::revalidate`: an entry of a member is dropped
when its predicate fails at `now` and gets the rebuilt payload when it holds;
an entry of a non-member is left alone. -/
def revalidateOne (check : Check ν) (members : List ν) (now : Int) (e : Entry ν) :
    Option (Entry ν) :=
  if e.node ∈ members then (check e.node e.cond now).map fun p => { e with payload := p }
  else some e

/-- The entries after `HotList::revalidate_members` (each member's entries go
through `revalidate`; the members' entries are disjoint, so the order the
`HashSet` yields them in does not matter). -/
def revalidate (check : Check ν) (members : List ν) (now : Int) (l : HotList ν) : HotList ν :=
  { l with entries := l.entries.filterMap (revalidateOne check members now) }

/-- The payload map `revalidate_members` returns, read at `id` (an absent key
is the empty list): the surviving payloads of a member, in list order. -/
def payloadsOf (members : List ν) (l : HotList ν) (id : ν) : List (Payload ν) :=
  if id ∈ members then (l.entries.filter fun e => decide (e.node = id)).map (·.payload) else []

/-- `hot_payloads` as recall receives it. -/
def hotPayloads (check : Check ν) (members : List ν) (now : Int) (l : HotList ν) :
    ν → List (Payload ν) :=
  payloadsOf members (revalidate check members now l)

/-- **What recall sees.** A payload is in a member's list exactly when an entry
of that node is on the hot list and its predicate holds at `now`, with that
payload. -/
theorem mem_hotPayloads {check : Check ν} {members : List ν} {now : Int} {l : HotList ν}
    {id : ν} {p : Payload ν} :
    p ∈ hotPayloads check members now l id ↔
      id ∈ members ∧ ∃ e ∈ l.entries, e.node = id ∧ check id e.cond now = some p := by
  unfold hotPayloads payloadsOf revalidate revalidateOne
  by_cases hm : id ∈ members
  · simp only [hm, ite_true, true_and, List.mem_map, List.mem_filter, List.mem_filterMap,
      decide_eq_true_eq]
    constructor
    · rintro ⟨e', ⟨⟨e, he, hfe⟩, hid⟩, rfl⟩
      refine ⟨e, he, ?_⟩
      by_cases hem : e.node ∈ members
      · simp only [hem, ite_true, Option.map_eq_some_iff] at hfe
        obtain ⟨q, hq, rfl⟩ := hfe
        simp only at hid ⊢
        exact ⟨hid, hid ▸ hq⟩
      · simp only [hem, ite_false, Option.some.injEq] at hfe
        subst hfe
        exact absurd (hid ▸ hm) hem
    · rintro ⟨e, he, hid, hc⟩
      refine ⟨{ e with payload := p }, ⟨⟨e, he, ?_⟩, hid⟩, rfl⟩
      simp [hid, hm, hc]
  · simp [hm]

/-- **A lapsed entry never reaches recall**: when every entry of `id` fails its
predicate at `now`, recall gets no payload for it. -/
theorem hotPayloads_nil_of_lapsed {check : Check ν} {members : List ν} {now : Int}
    {l : HotList ν} {id : ν}
    (hlapse : ∀ e ∈ l.entries, e.node = id → check id e.cond now = none) :
    hotPayloads check members now l id = [] := by
  apply List.eq_nil_iff_forall_not_mem.mpr
  intro p hp
  obtain ⟨_, e, he, hid, hc⟩ := mem_hotPayloads.mp hp
  rw [hlapse e he hid] at hc
  exact absurd hc (by simp)

theorem revalidateOne_spec {check : Check ν} {members : List ν} {now : Int} {e e' : Entry ν}
    (h : revalidateOne check members now e = some e') :
    e'.node = e.node ∧ e'.cond = e.cond ∧ e'.seq = e.seq := by
  unfold revalidateOne at h
  split at h
  · obtain ⟨q, _, rfl⟩ := Option.map_eq_some_iff.mp h
    exact ⟨rfl, rfl, rfl⟩
  · cases h; exact ⟨rfl, rfl, rfl⟩

omit [DecidableEq ν] in
theorem filterMap_map_sublist {β : Type} (f : Entry ν → Option (Entry ν)) (g : Entry ν → β)
    (hf : ∀ e e', f e = some e' → g e' = g e) :
    ∀ es : List (Entry ν), ((es.filterMap f).map g).Sublist (es.map g)
  | [] => by simp
  | x :: xs => by
    simp only [List.filterMap_cons, List.map_cons]
    cases hx : f x with
    | none => exact (filterMap_map_sublist f g hf xs).cons _
    | some y =>
      simp only [List.map_cons, hf x y hx]
      exact (filterMap_map_sublist f g hf xs).cons_cons _

omit [DecidableEq ν] in
theorem countC_filterMap_le (f : Entry ν → Option (Entry ν))
    (hf : ∀ e e', f e = some e' → e'.cond = e.cond) :
    ∀ es : List (Entry ν), countC (es.filterMap f) ≤ countC es
  | [] => by simp [countC]
  | x :: xs => by
    have ih := countC_filterMap_le f hf xs
    simp only [List.filterMap_cons]
    rw [countC_cons]
    cases hx : f x with
    | none => simp only; omega
    | some y =>
      simp only
      rw [countC_cons]
      have : isC y = isC x := by simp [isC, hf x y hx]
      rw [this]
      omega

/-- `HotList::revalidate` keeps the hot list's invariant: it only drops
entries and rewrites payloads, and priority ignores the payload, so no re-sort
is needed. -/
theorem revalidate_inv {l : HotList ν} (hl : Inv l) (check : Check ν) (members : List ν)
    (now : Int) : Inv (revalidate check members now l) := by
  have hkeys := filterMap_map_sublist (revalidateOne check members now) key
    (fun e e' h => by
      obtain ⟨a, b, _⟩ := revalidateOne_spec h
      simp [key, a, b]) l.entries
  have hseqs := filterMap_map_sublist (revalidateOne check members now) Entry.seq
    (fun e e' h => (revalidateOne_spec h).2.2) l.entries
  refine ⟨hl.keys_nodup.sublist hkeys, hl.seq_nodup.sublist hseqs, ?_, ?_, ?_⟩
  · intro e he
    obtain ⟨e₀, he₀, hfe⟩ := List.mem_filterMap.mp he
    show e.seq < l.nextSeq
    rw [(revalidateOne_spec hfe).2.2]
    exact hl.seq_lt e₀ he₀
  · exact Nat.le_trans (List.length_filterMap_le _ _) hl.len_le
  · exact Nat.le_trans (countC_filterMap_le _ (fun e e' h => (revalidateOne_spec h).2.1) _)
      hl.countC_le

end Lapse

/-- A clock-reading predicate: it holds while `0 ≤ now - writtenAt ≤ window`
and rebuilds `seconds_ago` from `now`. This is the test fixture
`clock_reading_entry` (`src/daemon/hotlist.rs`), not a detector; the real
recency predicates have the same shape, a write time in `[now - window, now]`
(for example `high_risk_at` in `src/daemon/events.rs`). -/
def windowCheck (writtenAt window now : Int) : Option (Payload ν) :=
  if 0 ≤ now - writtenAt ∧ now - writtenAt ≤ window then some (.conflict (now - writtenAt).toNat)
  else none

/-- **The clock alone ages an entry out**: it lapses exactly when `now` is
before the write or more than `window` after it. -/
theorem windowCheck_none_iff (writtenAt window now : Int) :
    (windowCheck writtenAt window now : Option (Payload ν)) = none ↔
      now < writtenAt ∨ writtenAt + window < now := by
  unfold windowCheck
  split
  · rename_i h; simp only [reduceCtorEq, false_iff, not_or]; omega
  · rename_i h; simp only [true_iff]; omega

end Lambo.Hotlist
