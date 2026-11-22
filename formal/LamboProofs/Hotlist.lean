import LamboProofs.TopK

/-
# The daemon hot list and recall's force-inclusion

Model of the hot list in `src/daemon/hotlist.rs` (`Condition`,
`Condition::severity`, `higher_priority`, `HotList::{with_budget, enforce,
insert, insert_many, revalidate, revalidate_members, retain_conditions}`,
`default_contradiction_max`, `HOT_LIST_MAX`), of the cycle that maintains it
(`detect_into_hot_list` in `src/daemon/mod.rs`, which `prime_offline` shares:
contradictions, conflicts, drift, stale, high-risk inserted in that order, then
`retain_conditions`), both before #298 (`cycle`: every insert enforces the
budget and the bound, and the cut comes last) and since (`cycleFixed`,
`HotList::sync_cycle`: the inserts are deferred, evicting nothing, then the
cut, then one `enforce`), and
of how recall consumes it in `src/recall/assemble.rs`: `forces`,
`contradiction_companions`, the `hot` / `cold_forced` / `forced` predicates
handed to `emitted`, and the kinds of the hot-list warning lines (`format::
hot_warning`, `format::contradiction_warning`). `forces_inclusion`
(`src/daemon/contradiction.rs`: Canonical or Venerable) is the parameter
`pillar`. Detection itself (conflict, drift, stale, high-risk, contradiction)
is an input: a cycle is given its hits and its condition set.

## Model

* An entry is `(node, cond, payload, seq)`; the predicate is not stored but is
  the parameter `check : node → cond → now → Option payload` (`Check`). Every
  detector builds the predicate for `(node, cond)` from the node and constant
  config, so this is the same function; the graph is fixed for one recall.
* The payload keeps only what the model reads (`seconds_ago`, `hops`, the
  contradicting `by`, `at`); agents, writer and reason are left out.
* `prioLe` is `higher_priority` without its node-id arm. `prio_tie_inv` shows
  the arm never decides on a reachable list (no two entries share a `seq`), as
  the comment on `higher_priority` says.
* `sort_by` is a stable insertion sort (`sortPrio`). The `over > 0` guard in
  `enforce` is dropped: the partition keeps everything within the budget
  (`budgetKept_of_countC_le`).
* Times are `Int` seconds; nodes are an abstract type with decidable equality.
  There is no floating point.
* `insertMany` models `insert_many` (one sort for the batch). Its agreement
  with one-by-one `insert`s, which the Rust doc claims, is checked by `decide`
  only (the examples on `plan`, which refresh a seeded entry and repeat keys
  within the batch), not proved. `insertMany_inv` shows the batch keeps the
  invariant. `cycle` sends every hit through `insertAll`, one insert at a time,
  though Rust inserts a cycle's contradictions with one `insert_many`
  (`insert_contradictions`, `src/daemon/contradiction.rs`): `cycle_inv` is
  about that substitute cycle. `cycle_sound` and `cycle_drops_collected` hold
  for any insertion step (they only use the final `retain`), and
  `cycle_evicts_fresh_hit` inserts no contradiction.
* `cycleFixed` models the deferred inserts with `upsertAll`, which is what
  Rust runs inside `sync_cycle`: each `insert` and each entry of an
  `insert_many` gets the next `seq` and refreshes its pair in place (found
  through an index of the live pairs, the unique match `upsert` finds) or is
  pushed, with no sort and no eviction; the final `enforce` sorts once. A
  `decide` example checks that lifting the limits instead (single inserts on
  a list whose bound and budget are large) leaves the same entries. `cycle`
  is kept as the cycle before #298, with its theorems.
* Not modelled: the detectors, the `ConditionCheck` closures' bodies (except
  `windowCheck`, the shape of `clock_reading_entry`), lock order, events and
  re-arm, the companion step after the cut (`with_ordinary_companions`), and
  the token budget. There is no reservation condition on the hot list: a
  reservation warning comes from the graph (`active_reservation`), and a
  holder is forced only in cold mode (`forcedIn`).

## Results

The list (`Inv`, preserved by `insert`, `insertAll`, `retain`, `revalidate`,
`cycle`):

* `enforce_length_le`, `enforce_countC_le`: the bound and the contradiction
  budget (#196) always hold.
* `upsert_keys_nodup`, `Inv.keys_nodup`: one entry per `(node, condition)`
  ("One entry per (node, condition)").
* `enforce_evicts_lowest`, `insert_evicted_lowest`: what is evicted ranks at or
  below everything kept, except that a contradiction dropped by the budget may
  outrank a kept entry of another kind (module doc, "Eviction on overflow
  removes the lowest"; tests `overflow_evicts_lowest_priority_and_keeps_bound`,
  `bound_enforced_under_mixed_severity_overflow`).
  `enforce_evicts_oldest_in_tier`: within a tier the older goes.
* `enforce_keeps_reserved`: when the other kinds number at most `max - cmax`,
  no number of contradictions evicts one of them ("no number of contradictions
  can evict them from it"; test `a_flood_of_contradictions_does_not_evict_the_other_kinds`).
  `enforce_under_budget`: under the budget only the bound evicts.
  `enforce_noop`: within both nothing is evicted.
* `insertFull_evicts_le_one`, `insert_evicted_eq`: one insert evicts at most
  one entry, so `pop()` loses nothing ("at most one goes").
* `insert_refresh`: re-inserting a present pair evicts nothing and leaves the
  new payload at the newest `seq` (test
  `refresh_replaces_payload_and_restores_recency`).
* `insertAll_keys_of_no_evict`, `insertAll_order_irrelevant`: while nothing is
  evicted, the live pairs are the old ones plus the inserted ones, in any
  insertion order; `order_matters_when_full` pins that once something is
  evicted the order decides what stays.

Lapsing: there is no timer. An entry leaves when the cycle no longer detects
its pair (`retain`, `cycle_sound`) or when recall re-validates a member and its
predicate fails at recall's `now`:

* `mem_hotPayloads`: recall gets a payload for a member exactly when an entry
  of that node holds at `now`, with the rebuilt payload.
* `hotPayloads_nil_of_lapsed`, `lapsed_not_forced`: a lapsed entry never
  reaches recall and never forces inclusion.
* `windowCheck_none_iff`: a clock-reading predicate lapses exactly when `now`
  is before the write or more than the window after it (test
  `revalidate_uses_the_callers_clock_and_refreshes_the_payload`).

Recall:

* `forces_iff`, `forcesPayload_iff`: a member is forced by its payloads exactly
  when one of them is a conflict, high-risk, drift or stale payload, or a
  contradiction on a Canonical or Venerable concept (`forces`; #179).
  `ordinary_contradiction_not_forced`.
* `hot_member_emitted`, `hot_member_emittedR`: every member forced by a live
  entry is emitted at any `top_k`, in blended and cold mode, with or without
  the redundancy skip (`TopK.forced_mem_emit`, `TopK.forced_mem_emitR`).
* `forced_has_line`: a member forced by its payloads carries a hot-list line;
  `hot_member_never_skipped`: a member with a payload is never skipped as
  redundant; `annot_*`: the line kinds (conflict, hot, contradiction).

GC (`gc::run` never touches the hot list):

* `collected_not_emitted`: a collected node is never emitted, even while its
  entry is still on the list (`TopK.emit_all_concepts`);
  `cycle_drops_collected`: the next cycle drops it when the detectors report
  live nodes only.

What the model shows about the cycle before #298:

* `cycle_evicts_fresh_hit`: the cycle inserts before it retains, so an entry
  no longer detected can evict a fresh hit from a full list and is then
  dropped itself; after that cycle the list holds fewer entries than the
  bound although every fresh hit fits. The module doc says the loop "keeps the
  list equal to the current cycle's fresh detector hits".
  Only a fresh high-risk, drift or stale hit can be lost this way, and only to
  an entry no longer detected of a strictly higher tier: a fresh hit outranks
  an old entry of its own tier by `seq`, fresh contradictions and conflicts are
  top-tier, and the budget never drops a fresh contradiction for an old one
  (the cycle re-inserts every held contradiction after the old ones). Events
  are unaffected (they are driven by the detected set); what is missing until
  a later cycle is the hit's warning lines and its force-inclusion. The next
  cycle restores it only if no further higher-tier entry lapses in that cycle,
  so under steady churn the loss can recur. At the defaults (bound 1000,
  budget 500) it needs at least 1000 old and fresh entries in one cycle, for
  example a full contradiction budget and about 500 conflict, high-risk or
  drift entries; with a small `hot_list_max` it is easy to reach.

The cycle since #298 (`cycleFixed`) cuts the list to the cycle's condition set
before it enforces the budget and the bound:

* `cycleFixed_inv`: it keeps the invariant (`Inv`: the bound, the budget, one
  entry per pair, unique issued `seq`s).
* `cycleFixed_sound`: every entry after it is one the cycle detected.
* `cycleFixed_evicts_only_for_held`: what it evicts was detected this cycle
  and ranks at or below every kept entry, each detected too (the budget's
  exception as in `enforce_evicts_lowest`): an entry no longer detected never
  evicts a fresh hit.
* `cycleFixed_keeps_hits`: when the condition set fits within the bound and
  its contradictions within the budget, nothing is evicted and every hit
  whose pair is in the set is on the list.
* `cycleFixed_keys_eq_hits`: with the condition set the daemon builds (every
  hit's pair, `heldOf`) and the hits within the bound and the budget, the
  pairs on the list are exactly the hits' pairs, as the module doc of
  `src/daemon/hotlist.rs` says.
* `cycleFixed_perm_cycle_of_no_evict`: when the cycle before #298 evicts
  nothing while inserting, the fixed one evicts nothing and leaves the same
  entries, so the fix changes nothing there. This is the narrow agreement.
  The fair one, that the two cycles agree whenever no entry lapses (every
  pair on the list is held and the held set is the hits' pairs) even when the
  fresh hits overflow, reduces to the equivalence of `insert_many` with
  one-by-one inserts, which is checked by `decide` only (see above), not
  proved; a `decide` example checks it for this cycle on the `plan` hits and
  several bounds and budgets.
* `cycleFixed_keeps_fresh_hit`: the issue's bound-2 scenario keeps both n2's
  conflict and n3's drift (the cycle before #298 keeps only the conflict).
  Two more examples show that when the fresh hits themselves overflow, the
  lowest of them goes, and that the budget still holds.

Events are not modelled; they are driven by the detected set, which the fix
does not change.
-/

namespace Lambo.Hotlist

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

theorem mem_sortPrio {e : Entry ν} {l : List (Entry ν)} : e ∈ sortPrio l ↔ e ∈ l :=
  (sortPrio_perm l).mem_iff

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

theorem countC_add_countO (l : List (Entry ν)) : countC l + countO l = l.length := by
  induction l with
  | nil => simp [countC, countO]
  | cons e rest ih =>
    rw [countC_cons, countO_cons]
    simp only [List.length_cons]
    split <;> omega

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

/-- What is kept is in priority order. -/
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

/-- The budget evicts contradictions only: an entry of another kind is
evicted only by the bound. -/
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

/-- The seqs after an upsert are the old ones plus the new entry's. -/
theorem seq_mem_upsert {e y : Entry ν} {l : List (Entry ν)} (hy : y ∈ upsert e l) :
    y = e ∨ y ∈ l := mem_upsert hy

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

theorem eq_of_nodup_map {α β : Type} (f : α → β) :
    ∀ {l : List α}, (l.map f).Nodup → ∀ {x y : α}, x ∈ l → y ∈ l → f x = f y → x = y
  | [], _, _, _, hx, _, _ => by simp at hx
  | z :: zs, h, x, y, hx, hy, hf => by
    have hz := List.nodup_cons.mp (show (f z :: zs.map f).Nodup from h)
    rcases List.mem_cons.mp hx with hx1 | hx2
    · rcases List.mem_cons.mp hy with hy1 | hy2
      · rw [hx1, hy1]
      · rw [hx1] at hf; exact absurd (hf ▸ List.mem_map_of_mem hy2) hz.1
    · rcases List.mem_cons.mp hy with hy1 | hy2
      · rw [hy1] at hf; exact absurd (hf.symm ▸ List.mem_map_of_mem hx2) hz.1
      · exact eq_of_nodup_map f hz.2 hx2 hy2 hf

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

theorem insert_evicted_eq {l : HotList ν} (hl : Inv l) (n : ν) (c : Cond) (p : Payload ν) :
    (insertFull l n c p).2 = (insert l n c p).2.toList := by
  have h := insertFull_evicts_le_one hl n c p
  show _ = ((insertFull l n c p).2.getLast?).toList
  match hr : (insertFull l n c p).2, h with
  | [], _ => rfl
  | [d], _ => rfl
  | _ :: _ :: _, h => simp at h

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

/-- Re-validation keeps the invariant: it only drops entries and rewrites
payloads (no re-sort is needed, since priority ignores the payload). -/
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


/-! ## Recall: force-inclusion and warning lines -/

/-- `forces`, per payload: every kind forces, except a contradiction of a
concept that is not Canonical or Venerable (`pillar`,
`daemon::contradiction::forces_inclusion`; #179). -/
def forcesPayload (pillar : Bool) : Payload ν → Bool
  | .contradiction _ _ => pillar
  | _ => true

/-- `forces(graph, id, hot_payloads)`. `pillar id` is "a graph concept whose
status is Canonical or Venerable" (false for a graph-missing id). -/
def forces (pillar : ν → Bool) (hp : ν → List (Payload ν)) (id : ν) : Bool :=
  (hp id).any (forcesPayload (pillar id))

theorem forcesPayload_iff (pillar : Bool) (p : Payload ν) :
    forcesPayload pillar p = true ↔ p.kind ≠ .contradiction ∨ pillar = true := by
  cases p <;> simp [forcesPayload, Payload.kind]

/-- **Which kinds force**: conflict, high-risk, drift and stale always; a
contradiction only on a Canonical or Venerable concept. -/
theorem forces_iff (pillar : ν → Bool) (hp : ν → List (Payload ν)) (id : ν) :
    forces pillar hp id = true ↔ ∃ p ∈ hp id, p.kind ≠ .contradiction ∨ pillar id = true := by
  unfold forces
  simp only [List.any_eq_true, forcesPayload_iff]

/-- A well-formed entry's payload is of its condition's kind (every detector
builds them so; `HotListEntry::new` does not check it). -/
def WellFormed (e : Entry ν) : Prop := e.payload.kind = e.cond

theorem forcesPayload_of_wf (pillar : Bool) (e : Entry ν) (h : WellFormed e) :
    forcesPayload pillar e.payload = true ↔ e.cond ≠ .contradiction ∨ pillar = true := by
  rw [forcesPayload_iff, h]

/-- An ordinary contradicted member is not forced (#179). -/
theorem ordinary_contradiction_not_forced (pillar : ν → Bool) (hp : ν → List (Payload ν))
    (id : ν) (hpil : pillar id = false) (hk : ∀ p ∈ hp id, p.kind = .contradiction) :
    forces pillar hp id = false := by
  cases h : forces pillar hp id with
  | false => rfl
  | true =>
    obtain ⟨p, hp', hor⟩ := (forces_iff pillar hp id).mp h
    rw [hpil] at hor
    simp only [Bool.false_eq_true, or_false] at hor
    exact absurd (hk p hp') hor

section Recall

variable [DecidableEq ν]

/-- `contradiction_companions(..).contains_key(b)`: `b` is the `by` of a
`Contradiction` payload on a Canonical or Venerable member, and a live
concept of that member's session (`liveBy t b`, `same_session_concept`). -/
def companion (pillar : ν → Bool) (liveBy : ν → ν → Bool) (hp : ν → List (Payload ν))
    (members : List ν) (b : ν) : Bool :=
  members.any fun t => pillar t && (hp t).any fun p =>
    match p with
    | .contradiction b' _ => decide (b' = b) && liveBy t b'
    | _ => false

/-- `hot`: forced by its own hot-list payloads, or as a contradiction
companion. -/
def hot (pillar : ν → Bool) (liveBy : ν → ν → Bool) (hp : ν → List (Payload ν))
    (members : List ν) (id : ν) : Bool :=
  forces pillar hp id || companion pillar liveBy hp members id

/-- `forced`: in cold mode the kept reservation holders are forced too. -/
def forcedIn (cold : Bool) (reservedKeep : ν → Bool) (hotF : ν → Bool) (id : ν) : Bool :=
  if cold then hotF id || reservedKeep id else hotF id

omit [DecidableEq ν] in
theorem forcedIn_of_hot (cold : Bool) (reservedKeep hotF : ν → Bool) (id : ν)
    (h : hotF id = true) : forcedIn cold reservedKeep hotF id = true := by
  cases cold <;> simp [forcedIn, h]

/-- The force predicate recall builds from the hot list: re-validate at `now`,
then `forces` or companion, in either mode. -/
def recallForced (pillar : ν → Bool) (liveBy : ν → ν → Bool) (check : Check ν)
    (members : List ν) (now : Int) (l : HotList ν) (cold : Bool) (reservedKeep : ν → Bool) :
    ν → Bool :=
  forcedIn cold reservedKeep (hot pillar liveBy (hotPayloads check members now l) members)

/-- A member is forced by its own hot-list entries exactly when one of them is
on the list, holds at `now`, and its rebuilt payload forces. -/
theorem forces_hotPayloads_iff (pillar : ν → Bool) (check : Check ν) (members : List ν)
    (now : Int) (l : HotList ν) (id : ν) :
    forces pillar (hotPayloads check members now l) id = true ↔
      id ∈ members ∧ ∃ e ∈ l.entries, ∃ p, e.node = id ∧ check id e.cond now = some p ∧
        forcesPayload (pillar id) p = true := by
  unfold forces
  simp only [List.any_eq_true, mem_hotPayloads]
  constructor
  · rintro ⟨p, ⟨hm, e, he, hid, hc⟩, hf⟩
    exact ⟨hm, e, he, p, hid, hc, hf⟩
  · rintro ⟨hm, e, he, p, hid, hc, hf⟩
    exact ⟨p, ⟨hm, e, he, hid, hc⟩, hf⟩

/-- **A lapsed entry never forces inclusion**: a member whose entries all fail
their predicate at `now` is not forced by the hot list (it can still be
forced as a contradiction companion, or as a cold-mode reservation holder). -/
theorem lapsed_not_forced (pillar : ν → Bool) (check : Check ν) (members : List ν) (now : Int)
    (l : HotList ν) (id : ν)
    (hlapse : ∀ e ∈ l.entries, e.node = id → check id e.cond now = none) :
    forces pillar (hotPayloads check members now l) id = false := by
  unfold forces
  rw [hotPayloads_nil_of_lapsed hlapse]
  rfl

/-- **Every forced hot-list member is emitted**, past `top_k`, at any `top_k`
and in either scoring mode (`TopK.forced_mem_emit`). -/
theorem hot_member_emitted (isConcept pillar : ν → Bool) (liveBy : ν → ν → Bool)
    (check : Check ν) (members : List ν) (now : Int) (l : HotList ν) (cold : Bool)
    (reservedKeep : ν → Bool) (topK : Nat) (ranked : List ν) (id : ν)
    (e : Entry ν) (he : e ∈ l.entries) (hid : e.node = id) (hm : id ∈ members) (p : Payload ν)
    (hc : check id e.cond now = some p) (hf : forcesPayload (pillar id) p = true)
    (hcon : isConcept id = true) (hr : id ∈ ranked) :
    id ∈ TopK.emit isConcept (recallForced pillar liveBy check members now l cold reservedKeep)
      topK 0 ranked := by
  apply TopK.forced_mem_emit _ _ topK id hcon _ ranked 0 hr
  apply forcedIn_of_hot
  unfold hot
  rw [(forces_hotPayloads_iff pillar check members now l id).mpr ⟨hm, e, he, p, hid, hc, hf⟩]
  rfl

/-- The same with the redundancy skip on (#181): a forced member is never
skipped (`TopK.forced_mem_emitR`). -/
theorem hot_member_emittedR (isConcept pillar warns : ν → Bool) (liveBy : ν → ν → Bool)
    (red : ν → ν → Bool) (check : Check ν) (members : List ν) (now : Int) (l : HotList ν)
    (cold : Bool) (reservedKeep : ν → Bool) (topK win wmax : Nat) (ranked : List ν) (id : ν)
    (e : Entry ν) (he : e ∈ l.entries) (hid : e.node = id) (hm : id ∈ members) (p : Payload ν)
    (hc : check id e.cond now = some p) (hf : forcesPayload (pillar id) p = true)
    (hcon : isConcept id = true) (hr : id ∈ ranked) :
    id ∈ TopK.emitR isConcept (recallForced pillar liveBy check members now l cold reservedKeep)
      warns red topK win wmax 0 0 [] ranked := by
  apply TopK.forced_mem_emitR _ _ warns red topK win wmax id hcon _ ranked 0 0 [] hr
  apply forcedIn_of_hot
  unfold hot
  rw [(forces_hotPayloads_iff pillar check members now l id).mpr ⟨hm, e, he, p, hid, hc, hf⟩]
  rfl

omit [DecidableEq ν] in
/-- **A node removed by GC is never emitted**, even while its entry is still on
the hot list (`TopK.emit_all_concepts`). -/
theorem collected_not_emitted (isConcept forced : ν → Bool) (topK : Nat) (ranked : List ν)
    (id : ν) (hgone : isConcept id = false) : id ∉ TopK.emit isConcept forced topK 0 ranked := by
  intro h
  rw [TopK.emit_all_concepts isConcept forced topK id ranked 0 h] at hgone
  exact absurd hgone (by decide)

end Recall

/-- The annotation kind of a hot-list warning line (`AnnotationKind`). -/
inductive Annot
  | conflict
  | hot
  | contradiction
  deriving DecidableEq, Repr

/-- The kind recall attaches to a payload's line (`format::hot_warning` /
`format::contradiction_warning`). -/
def annot : Payload ν → Annot
  | .conflict _ => .conflict
  | .highRisk => .hot
  | .drift _ => .hot
  | .stale _ => .hot
  | .contradiction _ _ => .contradiction

/-- The hot-list warning lines of a shown hit, one per payload, in payload
order (the lines between ⚑ and the companion line). -/
def hotLines (hp : ν → List (Payload ν)) (id : ν) : List Annot := (hp id).map annot

theorem hotLines_length (hp : ν → List (Payload ν)) (id : ν) :
    (hotLines hp id).length = (hp id).length := by
  simp [hotLines]

theorem annot_eq_contradiction_iff (p : Payload ν) :
    annot p = .contradiction ↔ p.kind = .contradiction := by
  cases p <;> simp [annot, Payload.kind]

theorem annot_eq_conflict_iff (p : Payload ν) :
    annot p = .conflict ↔ p.kind = .conflict := by
  cases p <;> simp [annot, Payload.kind]

/-- **A forced hit always says why**: one forced by its own payloads carries at
least one hot-list line. -/
theorem forced_has_line (pillar : ν → Bool) (hp : ν → List (Payload ν)) (id : ν)
    (h : forces pillar hp id = true) : hotLines hp id ≠ [] := by
  obtain ⟨p, hp', _⟩ := (forces_iff pillar hp id).mp h
  intro hnil
  unfold hotLines at hnil
  rw [List.map_eq_nil_iff] at hnil
  rw [hnil] at hp'
  simp at hp'

/-- With the redundancy skip, a member that carries a hot-list payload is never
skipped as redundant (`warns` counts a non-empty payload list; `TopK.skipsR_spec`),
whether or not that payload forces it. -/
theorem hot_member_never_skipped (isConcept forced warns : ν → Bool) (red : ν → ν → Bool)
    (topK win wmax : Nat) (ranked : List ν) (hp : ν → List (Payload ν))
    (hw : ∀ x, hp x ≠ [] → warns x = true) (k id : ν) (hne : hp id ≠ []) :
    (k, id) ∉ TopK.skipsR isConcept forced warns red topK win wmax 0 0 [] ranked := by
  intro h
  have := (TopK.skipsR_spec isConcept forced warns red topK win wmax k id ranked 0 0 [] h).2.2.1
  rw [hw id hne] at this
  exact absurd this (by decide)

/-! ## Corollaries on reachable lists -/

/-- **Oldest evicted within a tier**: of two entries of one severity and of
the same kind class (both contradictions or neither), the evicted one is the
older. -/
theorem enforce_evicts_oldest_in_tier (max cmax : Nat) (es : List (Entry ν)) (k d : Entry ν)
    (hk : k ∈ (enforce max cmax es).1) (hd : d ∈ (enforce max cmax es).2)
    (hsev : k.cond.severity = d.cond.severity) (hc : isC d = isC k) : d.seq ≤ k.seq := by
  cases enforce_evicts_lowest max cmax es k d hk hd with
  | inl h =>
    unfold Above prioLe at h
    simp only [Bool.or_eq_true, Bool.and_eq_true, decide_eq_true_eq] at h
    omega
  | inr h => rw [h.1, h.2] at hc; exact absurd hc (by decide)

section Reachable

variable [DecidableEq ν]

/-- What `insert` returns ranks at or below everything it keeps, unless it is a
contradiction dropped by the budget and the kept entry is of another kind. -/
theorem insert_evicted_lowest (l : HotList ν) (n : ν) (c : Cond) (p : Payload ν) (d : Entry ν)
    (hd : (insert l n c p).2 = some d) (k : Entry ν) (hk : k ∈ (insert l n c p).1.entries) :
    Above k d ∨ (isC d = true ∧ isC k = false) := by
  have hd' : d ∈ (insertFull l n c p).2 := List.mem_of_getLast? hd
  exact enforce_evicts_lowest _ _ _ k d hk hd'

theorem cycle_inv {l : HotList ν} (hl : Inv l) (hits : List (ν × Cond × Payload ν))
    (held : List (Cond × ν)) : Inv (cycle l hits held) :=
  retain_inv (insertAll_inv hl hits) held

/-- What `upsertAll` keeps: one entry per pair, unique and issued `seq`s. -/
theorem upsertAll_keys : ∀ (l : HotList ν) (es : List (ν × Cond × Payload ν)),
    (l.entries.map key).Nodup → (l.entries.map Entry.seq).Nodup →
      (∀ e ∈ l.entries, e.seq < l.nextSeq) →
      ((upsertAll l es).entries.map key).Nodup ∧ ((upsertAll l es).entries.map Entry.seq).Nodup ∧
        (upsertAll l es).max = l.max ∧ (upsertAll l es).cmax = l.cmax
  | l, [], hk, hs, _ => ⟨hk, hs, rfl, rfl⟩
  | l, (n, c, p) :: rest, hk, hs, hlt => by
    simp only [upsertAll]
    refine upsertAll_keys _ rest (upsert_keys_nodup _ _ hk)
      (upsert_seq_nodup _ _ hs (fun x hx => hlt x hx)) ?_
    intro e he
    show e.seq < l.nextSeq + 1
    cases mem_upsert he with
    | inl h => subst h; simp [stamp]
    | inr h => have := hlt e h; omega

theorem upsertAll_seq_lt : ∀ (l : HotList ν) (es : List (ν × Cond × Payload ν)),
    (∀ e ∈ l.entries, e.seq < l.nextSeq) →
      ∀ e ∈ (upsertAll l es).entries, e.seq < (upsertAll l es).nextSeq
  | l, [], hlt => hlt
  | l, (n, c, p) :: rest, hlt => by
    simp only [upsertAll]
    apply upsertAll_seq_lt
    intro e he
    show e.seq < l.nextSeq + 1
    cases mem_upsert he with
    | inl h => subst h; simp [stamp]
    | inr h => have := hlt e h; omega

/-- The batch insert keeps the invariant. -/
theorem insertMany_inv {l : HotList ν} (hl : Inv l) (es : List (ν × Cond × Payload ν)) :
    Inv (insertMany l es).1 := by
  have hu := upsertAll_keys l es hl.keys_nodup hl.seq_nodup hl.seq_lt
  have hlt := upsertAll_seq_lt l es hl.seq_lt
  refine ⟨enforce_map_nodup key _ _ _ hu.1, enforce_map_nodup Entry.seq _ _ _ hu.2.1, ?_,
    enforce_length_le _ _ _, enforce_countC_le _ _ _⟩
  intro e he
  exact hlt e (mem_enforce _ _ _ he)

end Reachable

/-! ## The cycle since #298: retain, then enforce once -/

theorem length_le_of_nodup_subset {α : Type} [DecidableEq α] :
    ∀ {l₁ : List α} (l₂ : List α), l₁.Nodup → (∀ x ∈ l₁, x ∈ l₂) → l₁.length ≤ l₂.length
  | [], _, _, _ => by simp
  | a :: t, l₂, hn, hs => by
    have hn' := List.nodup_cons.mp hn
    have ha : a ∈ l₂ := hs a List.mem_cons_self
    have ht : ∀ x ∈ t, x ∈ l₂.erase a := fun x hx =>
      (List.mem_erase_of_ne (fun h : x = a => hn'.1 (by rw [← h]; exact hx))).mpr (hs x (List.mem_cons_of_mem _ hx))
    have h1 := length_le_of_nodup_subset (l₂.erase a) hn'.2 ht
    rw [List.length_erase_of_mem ha] at h1
    have := List.length_pos_of_mem ha
    simp only [List.length_cons]
    omega

section Fixed

variable [DecidableEq ν]

/-- The cycle's condition set as Rust builds it: the `(condition, node)` pair
of every hit (`condition_set`, plus the contradiction hits' pairs). -/
def heldOf (hits : List (ν × Cond × Payload ν)) : List (Cond × ν) :=
  hits.map fun r => (r.2.1, r.1)

/-- How many pairs of a condition set are contradictions. -/
def countCHeld (held : List (Cond × ν)) : Nat :=
  (held.filter fun q => decide (q.1 = .contradiction)).length

/-- One daemon cycle's hot-list step since #298 (`HotList::sync_cycle`, which
`detect_into_hot_list` runs): every hit is inserted with the sort, the budget
and the bound deferred, so each refreshes its pair in place or is pushed,
with the next `seq`, and nothing is evicted (`upsertAll`); the list is cut to the cycle's
condition set; only then are the budget and the bound enforced, once. Returns
the list and what that `enforce` evicted. -/
def cycleFixedFull (l : HotList ν) (hits : List (ν × Cond × Payload ν))
    (held : List (Cond × ν)) : HotList ν × List (Entry ν) :=
  let r := retain held (upsertAll l hits)
  let e := enforce r.max r.cmax r.entries
  ({ r with entries := e.1 }, e.2)

/-- The list after a cycle since #298. -/
def cycleFixed (l : HotList ν) (hits : List (ν × Cond × Payload ν)) (held : List (Cond × ν)) :
    HotList ν :=
  (cycleFixedFull l hits held).1

theorem upsertAll_limits : ∀ (l : HotList ν) (es : List (ν × Cond × Payload ν)),
    (upsertAll l es).max = l.max ∧ (upsertAll l es).cmax = l.cmax
  | _, [] => ⟨rfl, rfl⟩
  | l, (n, c, p) :: rest => by
    simp only [upsertAll]
    exact upsertAll_limits _ rest

theorem insertAll_limits : ∀ (l : HotList ν) (es : List (ν × Cond × Payload ν)),
    (insertAll l es).max = l.max ∧ (insertAll l es).cmax = l.cmax
  | _, [] => ⟨rfl, rfl⟩
  | l, (n, c, p) :: rest => by
    simp only [insertAll]
    exact insertAll_limits _ rest

/-- The keys after a batch of upserts: the old ones plus the inserted ones. -/
theorem key_mem_upsertAll : ∀ (l : HotList ν) (es : List (ν × Cond × Payload ν)) (k : ν × Cond),
    k ∈ (upsertAll l es).entries.map key ↔ k ∈ l.entries.map key ∨ k ∈ es.map reqKey
  | l, [], k => by simp [upsertAll]
  | l, (n, c, p) :: rest, k => by
    simp only [upsertAll]
    rw [key_mem_upsertAll _ rest k]
    show k ∈ (upsert (stamp l n c p) l.entries).map key ∨ _ ↔ _
    rw [key_mem_upsert]
    simp only [List.map_cons, List.mem_cons, reqKey, key, stamp]
    constructor
    · rintro ((h | h) | h)
      · exact Or.inl h
      · exact Or.inr (Or.inl h)
      · exact Or.inr (Or.inr h)
    · rintro (h | h | h)
      · exact Or.inl (Or.inl h)
      · exact Or.inl (Or.inr h)
      · exact Or.inr h

/-- **The invariant holds after a cycle since #298**: one entry per pair,
unique and issued `seq`s, the bound and the contradiction budget. -/
theorem cycleFixed_inv {l : HotList ν} (hl : Inv l) (hits : List (ν × Cond × Payload ν))
    (held : List (Cond × ν)) : Inv (cycleFixed l hits held) := by
  have hu := upsertAll_keys l hits hl.keys_nodup hl.seq_nodup hl.seq_lt
  have hlt := upsertAll_seq_lt l hits hl.seq_lt
  have hs : (retain held (upsertAll l hits)).entries.Sublist (upsertAll l hits).entries :=
    List.filter_sublist
  refine ⟨enforce_map_nodup key _ _ _ (hu.1.sublist (hs.map key)),
    enforce_map_nodup Entry.seq _ _ _ (hu.2.1.sublist (hs.map _)), ?_,
    enforce_length_le _ _ _, enforce_countC_le _ _ _⟩
  intro e he
  exact hlt e (hs.subset (mem_enforce _ _ _ he))

/-- After a cycle since #298 every entry is one the cycle detected. -/
theorem cycleFixed_sound (l : HotList ν) (hits : List (ν × Cond × Payload ν))
    (held : List (Cond × ν)) (e : Entry ν) (he : e ∈ (cycleFixed l hits held).entries) :
    (e.cond, e.node) ∈ held :=
  (mem_retain.mp (mem_enforce _ _ _ he)).2

/-- **Only a detected entry evicts a fresh hit.** Since #298 what the cycle
evicts is itself detected this cycle, and ranks at or below every entry kept,
each of them detected too (except a contradiction dropped by the budget,
which may outrank a kept entry of another kind): an entry the cycle no longer
detects is gone before the budget and the bound are enforced, so it can no
longer push a fresh hit off the list (`cycle_evicts_fresh_hit`). -/
theorem cycleFixed_evicts_only_for_held (l : HotList ν) (hits : List (ν × Cond × Payload ν))
    (held : List (Cond × ν)) (d : Entry ν) (hd : d ∈ (cycleFixedFull l hits held).2) :
    (d.cond, d.node) ∈ held ∧ ∀ k ∈ (cycleFixed l hits held).entries,
      (k.cond, k.node) ∈ held ∧ (Above k d ∨ (isC d = true ∧ isC k = false)) := by
  have hp := enforce_perm (retain held (upsertAll l hits)).max
    (retain held (upsertAll l hits)).cmax (retain held (upsertAll l hits)).entries
  refine ⟨(mem_retain.mp (hp.mem_iff.mp (List.mem_append_right _ hd))).2, ?_⟩
  intro k hk
  exact ⟨cycleFixed_sound l hits held k hk, enforce_evicts_lowest _ _ _ k d hk hd⟩

/-- **Every fresh hit that fits is kept.** When the cycle's condition set fits
within the bound and its contradictions within the budget, the cycle since
#298 evicts nothing, and every hit whose pair is in the set is on the list,
whatever entries the cycle no longer detects were on it before. -/
theorem cycleFixed_keeps_hits {l : HotList ν} (hl : Inv l) (hits : List (ν × Cond × Payload ν))
    (held : List (Cond × ν)) (hlen : held.length ≤ l.max) (hcnt : countCHeld held ≤ l.cmax) :
    (cycleFixedFull l hits held).2 = [] ∧
      ∀ r ∈ hits, (r.2.1, r.1) ∈ held → reqKey r ∈ (cycleFixed l hits held).entries.map key := by
  have hu := upsertAll_keys l hits hl.keys_nodup hl.seq_nodup hl.seq_lt
  have hlim := upsertAll_limits l hits
  have hs : (retain held (upsertAll l hits)).entries.Sublist (upsertAll l hits).entries :=
    List.filter_sublist
  have hnd : ((retain held (upsertAll l hits)).entries.map key).Nodup := hu.1.sublist (hs.map key)
  -- the retained list fits
  have hR : (retain held (upsertAll l hits)).entries.length ≤ l.max := by
    have h := length_le_of_nodup_subset (held.map Prod.swap) hnd (by
      intro k hk
      obtain ⟨e, he, rfl⟩ := List.mem_map.mp hk
      exact List.mem_map.mpr ⟨_, (mem_retain.mp he).2, rfl⟩)
    simp only [List.length_map] at h
    omega
  have hRc : countC (retain held (upsertAll l hits)).entries ≤ l.cmax := by
    have hnd' : (((retain held (upsertAll l hits)).entries.filter isC).map key).Nodup :=
      hnd.sublist (List.filter_sublist.map key)
    have h := length_le_of_nodup_subset
      ((held.filter fun q => decide (q.1 = .contradiction)).map Prod.swap) hnd' (by
        intro k hk
        obtain ⟨e, he, rfl⟩ := List.mem_map.mp hk
        obtain ⟨he, hc⟩ := List.mem_filter.mp he
        refine List.mem_map.mpr ⟨(e.cond, e.node), List.mem_filter.mpr
          ⟨(mem_retain.mp he).2, ?_⟩, rfl⟩
        simpa [isC] using hc)
    simp only [List.length_map] at h
    unfold countC
    unfold countCHeld at hcnt
    omega
  have hno := enforce_noop (retain held (upsertAll l hits)).max
    (retain held (upsertAll l hits)).cmax (retain held (upsertAll l hits)).entries
    (by show _ ≤ (upsertAll l hits).max; rw [hlim.1]; exact hR)
    (by show _ ≤ (upsertAll l hits).cmax; rw [hlim.2]; exact hRc)
  refine ⟨hno.1, ?_⟩
  intro r hr hheld
  obtain ⟨e, he, hek⟩ := List.mem_map.mp
    ((key_mem_upsertAll l hits (reqKey r)).mpr (Or.inr (List.mem_map_of_mem hr)))
  have hn : e.node = r.1 := congrArg Prod.fst hek
  have hc : e.cond = r.2.1 := congrArg Prod.snd hek
  have heR : e ∈ (retain held (upsertAll l hits)).entries :=
    mem_retain.mpr ⟨he, by rw [hn, hc]; exact hheld⟩
  rw [← hek]
  exact List.mem_map_of_mem (hno.2.mem_iff.mpr heR)

/-- **The list equals the cycle's fresh hits, up to the bound**
(`src/daemon/hotlist.rs`, module doc): with the condition set the daemon
builds (every hit's pair) and the hits within the bound and the budget, the
pairs on the list after a cycle since #298 are exactly the hits' pairs. -/
theorem cycleFixed_keys_eq_hits {l : HotList ν} (hl : Inv l)
    (hits : List (ν × Cond × Payload ν)) (hlen : hits.length ≤ l.max)
    (hcnt : countCHeld (heldOf hits) ≤ l.cmax) (k : ν × Cond) :
    k ∈ (cycleFixed l hits (heldOf hits)).entries.map key ↔ k ∈ hits.map reqKey := by
  constructor
  · intro hk
    obtain ⟨e, he, rfl⟩ := List.mem_map.mp hk
    obtain ⟨r, hr, hre⟩ := List.mem_map.mp (cycleFixed_sound l hits (heldOf hits) e he)
    refine List.mem_map.mpr ⟨r, hr, ?_⟩
    simp only [Prod.mk.injEq] at hre
    simp [reqKey, key, hre.1, hre.2]
  · intro hk
    obtain ⟨r, hr, rfl⟩ := List.mem_map.mp hk
    exact (cycleFixed_keeps_hits hl hits (heldOf hits) (by simpa [heldOf] using hlen) hcnt).2 r hr
      (List.mem_map_of_mem hr)

theorem upsert_perm_congr (e : Entry ν) {l₁ l₂ : List (Entry ν)} (hp : l₁.Perm l₂)
    (hn : (l₁.map key).Nodup) : (upsert e l₁).Perm (upsert e l₂) := by
  by_cases hm : key e ∈ l₁.map key
  · have hm₂ : key e ∈ l₂.map key := (hp.map key).mem_iff.mp hm
    obtain ⟨x, hx, hxk, h1⟩ := upsert_perm_of_mem e l₁ hm
    obtain ⟨y, hy, hyk, h2⟩ := upsert_perm_of_mem e l₂ hm₂
    have hxy : x = y := eq_of_nodup_map key hn hx (hp.mem_iff.mpr hy) (hxk.trans hyk.symm)
    subst hxy
    exact h1.trans (((hp.erase x).cons e).trans h2.symm)
  · have hm₂ : key e ∉ l₂.map key := fun h => hm ((hp.map key).mem_iff.mpr h)
    rw [upsert_of_not_mem e l₁ hm, upsert_of_not_mem e l₂ hm₂]
    exact hp.append_right [e]

/-- While nothing is evicted, single inserts leave the entries a batch of
upserts leaves (up to order). -/
theorem insertAll_perm_upsertAll : ∀ (l u : HotList ν) (es : List (ν × Cond × Payload ν)),
    Inv l → u.nextSeq = l.nextSeq → l.entries.Perm u.entries → insertAllEvicted l es = [] →
      (insertAll l es).entries.Perm (upsertAll u es).entries
  | _, _, [], _, _, hp, _ => hp
  | l, u, (n, c, p) :: rest, hl, hs, hp, hev => by
    simp only [insertAllEvicted, List.append_eq_nil_iff] at hev
    simp only [insertAll, upsertAll]
    refine insertAll_perm_upsertAll _ _ rest (insert_inv hl n c p)
      (by show u.nextSeq + 1 = l.nextSeq + 1; rw [hs]) ?_ hev.2
    have he := enforce_perm l.max l.cmax (upsert (stamp l n c p) l.entries)
    have h0 : (enforce l.max l.cmax (upsert (stamp l n c p) l.entries)).2 = [] := hev.1
    rw [h0, List.append_nil] at he
    have hst : stamp u n c p = stamp l n c p := by simp [stamp, hs]
    show (enforce l.max l.cmax (upsert (stamp l n c p) l.entries)).1.Perm
      (upsert (stamp u n c p) u.entries)
    rw [hst]
    exact he.trans (upsert_perm_congr _ hp hl.keys_nodup)

/-- **Agreement where nothing was lost.** When the cycle before #298 evicts
nothing while inserting, the cycle since #298 evicts nothing either and
leaves the same entries. -/
theorem cycleFixed_perm_cycle_of_no_evict {l : HotList ν} (hl : Inv l)
    (hits : List (ν × Cond × Payload ν)) (held : List (Cond × ν))
    (h : insertAllEvicted l hits = []) :
    (cycleFixedFull l hits held).2 = [] ∧
      (cycleFixed l hits held).entries.Perm (cycle l hits held).entries := by
  have hp := insertAll_perm_upsertAll l l hits hl rfl (List.Perm.refl _) h
  have hf : (retain held (upsertAll l hits)).entries.Perm (cycle l hits held).entries :=
    (hp.filter _).symm
  have hc := cycle_inv hl hits held
  have hlim := upsertAll_limits l hits
  have hlim' := insertAll_limits l hits
  have hno := enforce_noop (retain held (upsertAll l hits)).max
    (retain held (upsertAll l hits)).cmax (retain held (upsertAll l hits)).entries
    (by
      show _ ≤ (upsertAll l hits).max
      rw [hf.length_eq, hlim.1, ← hlim'.1]
      exact hc.len_le)
    (by
      show _ ≤ (upsertAll l hits).cmax
      rw [countC_perm hf, hlim.2, ← hlim'.2]
      exact hc.countC_le)
  exact ⟨hno.1, hno.2.trans hf⟩

end Fixed

/-! ## Concrete examples (nodes are `Nat`) -/

section Examples

/-- An insert request with a payload of the condition's kind. -/
def req (n : Nat) : Cond → Nat × Cond × Payload Nat
  | .conflict => (n, .conflict, .conflict 5)
  | .highRisk => (n, .highRisk, .highRisk)
  | .drift => (n, .drift, .drift 4)
  | .stale => (n, .stale, .stale 60)
  | .contradiction => (n, .contradiction, .contradiction 0 0)

def nodes (l : HotList Nat) : List Nat := l.entries.map (·.node)

def keys (l : HotList Nat) : List (Nat × Cond) := l.entries.map key

def reqs (ns : List Nat) (c : Cond) : List (Nat × Cond × Payload Nat) := ns.map (req · c)

/-- `default_contradiction_max`: `the_default_contradiction_budget_is_half_the_bound`. -/
example : (new : HotList Nat).cmax = 500 ∧ defaultCmax 7 = 3 ∧ defaultCmax 0 = 0 := by decide

/-- `overflow_evicts_lowest_priority_and_keeps_bound`: same condition, so the
oldest goes; the list is `[n5, n4, n3]`. -/
example :
    let l3 := insertAll (withMax 3) (reqs [1, 2, 3] .conflict)
    let r4 := insert l3 4 .conflict (.conflict 5)
    let r5 := insert r4.1 5 .conflict (.conflict 5)
    (insertAllEvicted (withMax 3) (reqs [1, 2, 3] .conflict)).length = 0 ∧
      r4.2.map (·.node) = some 1 ∧ r5.2.map (·.node) = some 2 ∧ nodes r5.1 = [5, 4, 3] := by
  decide

/-- `zero_bound_evicts_every_insert`. -/
example :
    let r1 := insert (withMax 0) 1 .conflict (.conflict 5)
    let r2 := insert r1.1 2 .conflict (.conflict 5)
    r1.2.map (·.node) = some 1 ∧ r2.2.map (·.node) = some 2 ∧ r2.1.entries = [] := by decide

/-- `bound_enforced_under_mixed_severity_overflow`: the least severe goes. -/
example :
    let l := insertAll (withMax 3) [req 1 .conflict, req 2 .drift, req 3 .stale]
    (insert l 4 .stale (.stale 60)).2.map (·.cond) = some .stale := by decide

/-- `severity_outranks_recency`. -/
example :
    keys (insertAll new [req 1 .stale, req 2 .drift, req 3 .conflict])
      = [(3, .conflict), (2, .drift), (1, .stale)] := by decide

/-- `refresh_replaces_payload_and_restores_recency`: no duplicate, no eviction,
the new payload, and the refreshed entry now outranks the later `n2`. -/
example :
    let l := insertAll new [(1, .conflict, .conflict 30), req 2 .conflict]
    let r := insertFull l 1 .conflict (.conflict 11)
    r.2 = [] ∧ nodes r.1 = [1, 2] ∧ (r.1.entries.head?.map (·.payload)) = some (.conflict 11) := by
  decide

/-- `a_flood_of_contradictions_does_not_evict_the_other_kinds` (20
contradictions instead of 100): five contradictions stay, the latest
inserted; drift, stale and high-risk stay; only contradictions are evicted. -/
example :
    let l := insertAll (withMax 10) [req 1 .drift, req 2 .stale, req 3 .highRisk]
    let r := insertMany l (reqs (List.range' 100 20) .contradiction)
    countC r.1.entries = 5 ∧ r.2.length = 15 ∧ r.2.all isC ∧
      (r.1.entries.filter isC).map (·.node) = [119, 118, 117, 116, 115] ∧
      (1, .drift) ∈ keys r.1 ∧ (2, .stale) ∈ keys r.1 ∧ (3, .highRisk) ∈ keys r.1 ∧
      (insert r.1 500 .contradiction (.contradiction 0 0)).2.map (·.node) = some 115 := by
  decide

/-- `under_the_budget_nothing_changes`: conflicts are not counted against the
budget; over the bound the lowest goes as before. -/
example :
    let l := insertAll (withMax 6) (reqs [1, 2, 3] .contradiction ++ reqs [10, 11, 12] .conflict)
    insertAllEvicted (withMax 6) (reqs [1, 2, 3] .contradiction ++ reqs [10, 11, 12] .conflict)
        = [] ∧ (insert l 20 .conflict (.conflict 5)).2.map (·.node) = some 1 := by
  decide

/-- `other_kinds_can_use_the_whole_list`. -/
example :
    let l := insertAll (withMax 4) (reqs [1, 2, 3, 4] .drift)
    (insert l 9 .contradiction (.contradiction 0 0)).2.map (·.node) = some 1 := by decide

/-- `a_budget_at_the_bound_reserves_nothing`. -/
example :
    let r := insertMany (insertAll (withBudget 3 3) [req 1 .drift])
      (reqs [10, 11, 12] .contradiction)
    countC r.1.entries = 3 ∧ (1, .drift) ∉ keys r.1 := by decide

/-- The daemon inserts contradictions before conflicts, so on a full list the
contradiction goes first (api.mdx). -/
example :
    keys (insertAll (withBudget 1 1) [req 1 .contradiction, req 2 .conflict]) = [(2, .conflict)] := by
  decide

/-- The plan of `insert_many_matches_inserting_one_by_one` (shortened), with
repeated `(node, condition)` keys inside the batch and a refresh of the seeded
`(99, drift)` entry, so the refresh-in-place path is exercised: the batch
leaves exactly the list one-by-one inserts leave (entries, payloads, `seq`s),
for several bounds and budgets. -/
def plan : List (Nat × Cond × Payload Nat) :=
  (List.range 16).map (fun i =>
    req ((i * 7) % 13) ([Cond.contradiction, .stale, .conflict, .drift, .highRisk].getD ((i * 3) % 5) .drift))
  ++ [(0, .contradiction, .contradiction 5 3), (7, .drift, .drift 9), (99, .drift, .drift 7),
      (1, .stale, .stale 90), (0, .contradiction, .contradiction 6 4), (12, .conflict, .conflict 2)]

example :
    [(0, 0), (3, 0), (3, 1), (3, 3), (8, 2), (8, 4), (8, 8), (20, 3), (20, 10), (100, 50)].all
      fun (m, b) =>
        let l := insertAll (withBudget m b) [req 99 .drift]
        (insertMany l plan).1 = insertAll l plan := by
  decide +kernel

/-- The plan repeats keys, and with room to spare the seeded entry is refreshed
in place with its new payload. -/
example :
    let l := insertAll (withBudget 100 50) [req 99 .drift]
    (plan.map reqKey).length = 22 ∧ (keys (insertMany l plan).1).length = 18 ∧
      ((insertMany l plan).1.entries.find? (·.node = 99)).map (·.payload) = some (.drift 7) := by
  decide

/-- **Order matters once the list is full**: two drift entries into a list of
one keep whichever came last. -/
theorem order_matters_when_full :
    keys (insertAll (withMax 1) [req 1 .drift, req 2 .drift]) = [(2, .drift)] ∧
      keys (insertAll (withMax 1) [req 2 .drift, req 1 .drift]) = [(1, .drift)] := by
  decide

/-- **The cycle inserts before it retains** (`detect_into_hot_list`), so an
entry that is no longer detected can push a fresh hit off a full list, and is
then dropped itself: after cycle 2 the list (bound 2) holds one entry, though
both fresh hits fit. The drift entry is lost to the old conflict, a higher tier;
an old entry of its own tier could not evict it, since the fresh one has the
newer `seq`. The next cycle restores it (no further entry lapses there). -/
theorem cycle_evicts_fresh_hit :
    let l1 := cycle (withMax 2) [req 1 .conflict, req 2 .conflict] [(.conflict, 1), (.conflict, 2)]
    let l2 := cycle l1 [req 2 .conflict, req 3 .drift] [(.conflict, 2), (.drift, 3)]
    let l3 := cycle l2 [req 2 .conflict, req 3 .drift] [(.conflict, 2), (.drift, 3)]
    keys l1 = [(2, .conflict), (1, .conflict)] ∧ keys l2 = [(2, .conflict)] ∧
      keys l3 = [(2, .conflict), (3, .drift)] := by
  decide

/-- **#298 fixed** (`sync_cycle_cuts_before_it_enforces` in
`src/daemon/hotlist.rs`; `a_lapsed_entry_never_evicts_a_fresh_hit` in
`src/daemon/tests/conditions.rs` runs it through `detect_into_hot_list`, with
a stale hit in place of the drift): the scenario of `cycle_evicts_fresh_hit` under the
cycle since #298. Cycle 2 drops the old conflict on n1 before enforcing the
bound, so both fresh hits stay: n2's conflict and n3's drift. -/
theorem cycleFixed_keeps_fresh_hit :
    let l1 := cycleFixed (withMax 2) [req 1 .conflict, req 2 .conflict]
      [(.conflict, 1), (.conflict, 2)]
    let l2 := cycleFixed l1 [req 2 .conflict, req 3 .drift] [(.conflict, 2), (.drift, 3)]
    keys l1 = [(2, .conflict), (1, .conflict)] ∧ keys l2 = [(2, .conflict), (3, .drift)] ∧
      (cycleFixedFull l1 [req 2 .conflict, req 3 .drift] [(.conflict, 2), (.drift, 3)]).2 = [] ∧
      keys (cycle l1 [req 2 .conflict, req 3 .drift] [(.conflict, 2), (.drift, 3)])
        = [(2, .conflict)] := by
  decide

/-- When the fresh hits themselves overflow, the bound evicts the lowest of
them, never for an entry the cycle no longer detects: bound 2, an old
high-risk entry on n9 lapses, and of three fresh hits the stale one goes. -/
example :
    let l1 := cycleFixed (withMax 2) [req 9 .highRisk] (heldOf [req 9 .highRisk])
    let hits := [req 1 .conflict, req 2 .drift, req 3 .stale]
    let r := cycleFixedFull l1 hits (heldOf hits)
    keys r.1 = [(1, .conflict), (2, .drift)] ∧ r.2.map key = [(3, .stale)] := by
  decide

/-- The budget still holds: bound 3, budget 1, two fresh contradictions and a
drift after an old conflict lapses; the later-declared contradiction stays. -/
example :
    let l1 := cycleFixed (withBudget 3 1) [req 9 .conflict] (heldOf [req 9 .conflict])
    let hits := [req 1 .contradiction, req 2 .contradiction, req 3 .drift]
    let r := cycleFixedFull l1 hits (heldOf hits)
    keys r.1 = [(2, .contradiction), (3, .drift)] ∧ r.2.map key = [(1, .contradiction)] := by
  decide

/-- Deferring the limits or lifting them leaves the same entries: single
inserts on the list with a large bound and budget leave the entries
`upsertAll` leaves, here with a refresh of a present pair. -/
example :
    let l1 := insertAll (withMax 2) [req 1 .conflict, req 2 .conflict]
    let hits := [req 2 .conflict, req 3 .drift, req 1 .stale]
    sortPrio (insertAll { l1 with max := 100, cmax := 100 } hits).entries
      = sortPrio (upsertAll l1 hits).entries := by
  decide

/-- **No lapsed entry, overflowing hits**: when every pair on the list is held
and the held set is the hits' pairs, the cycles before and since #298 leave
the same list, even where the hits overflow the bound or the budget. Checked
on the `plan` hits (which repeat pairs and refresh the seeded one) for several
bounds and budgets; proving it in general needs the `insert_many`
equivalence, which is not proved (see the header). -/
example :
    [(0, 0), (3, 0), (3, 1), (3, 3), (8, 2), (8, 4), (8, 8), (20, 3), (20, 10), (100, 50)].all
      fun (m, b) =>
        let l := insertAll (withBudget m b) [req 99 .drift]
        let hits := req 99 .drift :: plan
        (cycleFixed l hits (heldOf hits)).entries = (cycle l hits (heldOf hits)).entries := by
  decide +kernel

/-- `revalidate_uses_the_callers_clock_and_refreshes_the_payload`: written at
1000 with a 30 s window, the entry reads 11 then 25 seconds, and has lapsed at
1031. -/
example :
    (windowCheck 1000 30 1011 : Option (Payload Nat)) = some (.conflict 11) ∧
      (windowCheck 1000 30 1025 : Option (Payload Nat)) = some (.conflict 25) ∧
      (windowCheck 1000 30 1031 : Option (Payload Nat)) = none := by decide

/-- Write times for the next example. -/
def writtenAt : Nat → Int
  | 1 => 49
  | _ => 29

def exCheck : Check Nat := fun n _ now => windowCheck (writtenAt n) 30 now

/-- `revalidate_members_keeps_live_and_drops_lapsed`: at `now = 60`, members
`[1, 2, 3, 1]`; `n1` is live (11 s), `n2` lapsed (31 s) and is evicted, `n3` is
not hot, `n4` lapsed but is not a member, so it is left alone. -/
example :
    let l := insertAll new [req 1 .conflict, req 2 .conflict, req 4 .conflict]
    let m := [1, 2, 3, 1]
    hotPayloads exCheck m 60 l 1 = [.conflict 11] ∧ hotPayloads exCheck m 60 l 2 = [] ∧
      hotPayloads exCheck m 60 l 3 = [] ∧ hotPayloads exCheck m 60 l 4 = [] ∧
      nodes (revalidate exCheck m 60 l) = [4, 1] := by
  decide

/-- Which payloads force: everything but a contradiction of an ordinary
concept. -/
example :
    let ord : Nat → Bool := fun _ => false
    let pil : Nat → Bool := fun _ => true
    forces ord (fun _ => [.conflict 11]) 1 = true ∧ forces ord (fun _ => [.highRisk]) 1 = true ∧
      forces ord (fun _ => [.drift 4]) 1 = true ∧ forces ord (fun _ => [.stale 60]) 1 = true ∧
      forces ord (fun _ => [.contradiction 2 0]) 1 = false ∧
      forces pil (fun _ => [.contradiction 2 0]) 1 = true ∧
      forces ord (fun _ => [.contradiction 2 0, .stale 60]) 1 = true := by
  decide

/-- An ordinary contradicted hit is not forced but, once shown, carries its
line (`an_ordinary_contradicted_hit_below_the_cut_is_not_forced`). -/
example :
    let hp : Nat → List (Payload Nat) := fun n => if n = 3 then [.contradiction 9 0] else []
    forces (fun _ => false) hp 3 = false ∧ hotLines hp 3 = [.contradiction] ∧
      TopK.emit (fun _ => true) (hot (fun _ => false) (fun _ _ => true) hp [1, 2, 3]) 1 0 [1, 2, 3]
        = [1] := by
  decide

/-- A Venerable contradicted hit below the cut is forced and pulls its
contradicting concept `9` in, both past `top_k` (#19). -/
example :
    let hp : Nat → List (Payload Nat) := fun n => if n = 3 then [.contradiction 9 0] else []
    TopK.emit (fun _ => true) (hot (fun n => n = 3) (fun _ _ => true) hp [1, 2, 3]) 1 0
        [1, 2, 3, 9] = [1, 3, 9] := by
  decide

/-- `hot_force_include_keeps_live_and_drops_lapsed`: with `top_k = 1`, the
live conflict member `n1` ranked last is still emitted; the lapsed `n2` is not
forced. -/
example :
    let l := insertAll new [req 1 .conflict, req 2 .conflict]
    TopK.emit (fun _ => true)
        (recallForced (fun _ => false) (fun _ _ => true) exCheck [3, 2, 1] 60 l false (fun _ => false))
        1 0 [3, 2, 1] = [3, 1] := by
  decide

/-- GC: the entry for collected `n1` is still on the list, but `n1` is not
emitted. -/
example :
    let l := insertAll new [req 1 .conflict]
    (1, .conflict) ∈ keys l ∧
      TopK.emit (fun n => n ≠ 1)
        (recallForced (fun _ => false) (fun _ _ => true) (fun _ _ _ => some (.conflict 5)) [1, 2]
          60 l false (fun _ => false)) 0 0 [1, 2] = [] := by
  decide

end Examples

end Lambo.Hotlist
