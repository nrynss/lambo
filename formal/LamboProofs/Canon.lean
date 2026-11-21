/-
# Canonization: the lifecycle ladder and one hop per cycle

Model of the status write gate (`legal_canonization_transition` and
`Graph::apply_canonization_transition`, `src/graph/graph/transitions.rs`), of
the apply phase of a canonization cycle (`apply` in `src/canon/eval.rs`) and of
the stage predicates (`src/canon/stage1.rs`, `stage2.rs`, `stage3.rs`).

## Results

* `legal_demotion_unique`: the gate "admits exactly one rank-decreasing
  transition, Canonical → None" (`policy.rs`).
* `canonical_only_from_venerable`, `legal_rank_step`: no transition skips into
  Canonical, and none climbs more than two ranks (the two-rank one is
  `None → Venerable`, used only by `set_root_goal`).
* `gate_some`: a transition whose `from_status` does not match the concept's
  current status is rejected ("fabricated transition rejected").
* `cycle_one_hop`: in one canonization cycle a concept's status is unchanged
  or moves along exactly one edge of the eval loop's state machine ("a node may
  take at most one legal edge of the state machine per tick").
* `cycle_never_skips`, `cycle_rank_le`: consequently a cycle raises a concept
  by at most one rank, so `None` cannot reach `Canonical` in fewer than three
  cycles.
* `stage3_promotions_le`, `stage3_no_overflow`: Stage 3 promotes at most the
  remaining budget, so "a cycle can never create the overflow it then
  demotes".
* boundary facts for the three stage predicates.
-/

namespace Lambo.Canon

/-- `CanonizationStatus` (`src/types/mod.rs`). -/
inductive Status where
  | none
  | candidate
  | venerable
  | canonical
  deriving DecidableEq, Repr

/-- `status_rank` (`src/canon/policy.rs`). -/
def Status.rank : Status → Nat
  | .none => 0
  | .candidate => 1
  | .venerable => 2
  | .canonical => 3

/-- `legal_canonization_transition`: the five pairs the write gate admits. -/
def legal : Status → Status → Bool
  | .none, .candidate => true
  | .none, .venerable => true
  | .candidate, .venerable => true
  | .venerable, .canonical => true
  | .canonical, .none => true
  | _, _ => false

/-! ## The gate's table -/

/-- Self-loops are rejected. -/
theorem legal_irrefl (s : Status) : legal s s = false := by
  cases s <;> rfl

/-- **One demotion.** The only legal transition that lowers the rank is
`Canonical → None`. There is no `Venerable → Candidate`, no
`Canonical → Venerable` and no `Candidate → None`. -/
theorem legal_demotion_unique (a b : Status) (h : legal a b = true) (hdown : b.rank < a.rank) :
    a = .canonical ∧ b = .none := by
  revert h hdown
  cases a <;> cases b <;> decide

/-- **No skipping into Canonical.** The only way in is from Venerable. -/
theorem canonical_only_from_venerable (a : Status) (h : legal a .canonical = true) :
    a = .venerable := by
  revert h
  cases a <;> decide

/-- A legal promotion climbs one rank, or two when it is `None → Venerable`. -/
theorem legal_rank_step (a b : Status) (h : legal a b = true) (hup : a.rank < b.rank) :
    b.rank = a.rank + 1 ∨ (a = .none ∧ b = .venerable) := by
  revert h hup
  cases a <;> cases b <;> decide

/-- Every legal transition changes the status. -/
theorem legal_ne (a b : Status) (h : legal a b = true) : a ≠ b := by
  revert h
  cases a <;> cases b <;> decide

/-- A demotion always lands on `None`, the bottom of the ladder: a demoted
concept has to earn every stage again. -/
theorem legal_to_none (a : Status) (h : legal a .none = true) : a = .canonical := by
  revert h
  cases a <;> decide

/-! ## The write gate -/

/-- `apply_canonization_transition`, on the status alone: the event must name
the concept's current status as its `from`, and the pair must be legal. -/
def gate (current from_ to_ : Status) : Option Status :=
  if current = from_ ∧ legal from_ to_ = true then some to_ else none

/-- A transition is applied only from the concept's actual status and only
along a legal edge. -/
theorem gate_some (current from_ to_ result : Status)
    (h : gate current from_ to_ = some result) :
    current = from_ ∧ result = to_ ∧ legal current result = true := by
  unfold gate at h
  by_cases hc : current = from_ ∧ legal from_ to_ = true
  · rw [if_pos hc] at h
    have e : to_ = result := Option.some.inj h
    rw [← e, hc.1]
    exact ⟨rfl, rfl, hc.2⟩
  · rw [if_neg hc] at h
    exact Option.noConfusion h

/-- A fabricated transition, one whose `from` is not the current status, is
rejected whatever it claims. -/
theorem gate_rejects_mismatch (current from_ to_ : Status) (h : current ≠ from_) :
    gate current from_ to_ = none := by
  unfold gate
  exact if_neg (fun hc => h hc.1)

/-! ## One canonization cycle

`apply` runs five passes under one write lock, each re-reading the concept's
*current* status:

0. supersession: `Canonical → None` for each verdict that still holds;
1. Stage 1: `None → Candidate` for members of `plan.stage1`;
2. Stage 2: `Candidate → Venerable` for members of `plan.stage2` that passed;
3. Stage 3: `Venerable → Canonical` for window members that passed, while the
   budget lasts;
4. budget: `Canonical → None` for members of `demotion_ranked`, while over.

The plan lists are gathered earlier, from the status each concept had at
gather time: `plan.stage1` holds only concepts that were `None`, `plan.stage2`
only `Candidate`, the Stage 3 window only `Venerable`, and the supersession
targets and `demotion_ranked` only `Canonical`. -/

/-- The eval loop's own edges. It never uses `None → Venerable`. -/
def evalHop : Status → Status → Bool
  | .none, .candidate => true
  | .candidate, .venerable => true
  | .venerable, .canonical => true
  | .canonical, .none => true
  | _, _ => false

/-- Every edge the eval loop takes is one the write gate admits. -/
theorem evalHop_legal (a b : Status) (h : evalHop a b = true) : legal a b = true := by
  revert h
  cases a <;> cases b <;> decide

/-- The flags the apply phase consults for one concept: whether it is a
supersession target that still holds, whether each stage would promote it, and
whether the budget pass would demote it. -/
structure Flags where
  supersede : Bool
  stage1 : Bool
  stage2 : Bool
  stage3 : Bool
  budget : Bool

/-- The five passes, on one concept, starting from its status `s` when the
write lock is taken. -/
def cycle (s : Status) (f : Flags) : Status :=
  let s0 := if s = .canonical ∧ f.supersede = true then Status.none else s
  let s1 := if s0 = .none ∧ f.supersede = false ∧ f.stage1 = true then Status.candidate else s0
  let s2 := if s1 = .candidate ∧ f.stage2 = true then Status.venerable else s1
  let s3 := if s2 = .venerable ∧ f.stage3 = true then Status.canonical else s2
  if s3 = .canonical ∧ f.budget = true then Status.none else s3

/-- The flags are consistent with a gather-time status `g`: a concept is on a
plan list only if it had that list's status when the plan was gathered. -/
abbrev Consistent (g : Status) (f : Flags) : Prop :=
  (f.supersede = true → g = .canonical) ∧
  (f.stage1 = true → g = .none) ∧
  (f.stage2 = true → g = .candidate) ∧
  (f.stage3 = true → g = .venerable) ∧
  (f.budget = true → g = .canonical)

/-- **One hop per cycle.** Whatever status `s` the concept has when the apply
phase starts (it may differ from the gather-time status `g` if `set_root_goal`
ran in between), the cycle leaves it unchanged or moves it along exactly one
eval-loop edge. A concept promoted by Stage 1 is not promoted again by Stage 2
in the same cycle, one promoted by Stage 3 is not demoted by the budget pass,
and so on. -/
theorem cycle_one_hop (g s : Status) (f : Flags) (h : Consistent g f) :
    cycle s f = s ∨ evalHop s (cycle s f) = true := by
  revert h
  cases f with
  | mk supersede stage1 stage2 stage3 budget =>
    cases g <;> cases s <;> cases supersede <;> cases stage1 <;> cases stage2 <;>
      cases stage3 <;> cases budget <;> decide

/-- A cycle raises a concept by at most one rank. -/
theorem cycle_rank_le (g s : Status) (f : Flags) (h : Consistent g f) :
    (cycle s f).rank ≤ s.rank + 1 := by
  revert h
  cases f with
  | mk supersede stage1 stage2 stage3 budget =>
    cases g <;> cases s <;> cases supersede <;> cases stage1 <;> cases stage2 <;>
      cases stage3 <;> cases budget <;> decide

/-- **No stage is skipped.** A concept is Canonical after a cycle only if it
was already Canonical or was Venerable when the cycle's apply phase began. -/
theorem cycle_never_skips (g s : Status) (f : Flags) (h : Consistent g f)
    (hc : cycle s f = .canonical) : s = .canonical ∨ s = .venerable := by
  revert h hc
  cases f with
  | mk supersede stage1 stage2 stage3 budget =>
    cases g <;> cases s <;> cases supersede <;> cases stage1 <;> cases stage2 <;>
      cases stage3 <;> cases budget <;> decide

/-- A concept superseded in a cycle is not re-promoted in that same cycle
(Rust test `a_superseded_concept_is_not_re_promoted_in_the_same_cycle`). This
one needs no consistency assumption: Stage 1 checks `superseded` directly. -/
theorem cycle_superseded_stays_none (f : Flags) (hs : f.supersede = true) :
    cycle .canonical f = .none := by
  revert hs
  cases f with
  | mk supersede stage1 stage2 stage3 budget =>
    cases supersede <;> cases stage1 <;> cases stage2 <;> cases stage3 <;> cases budget <;>
      decide

/-- With no flag set, the cycle changes nothing. -/
theorem cycle_idle (s : Status) :
    cycle s ⟨false, false, false, false, false⟩ = s := by
  cases s <;> decide

/-! ## Stage 3 and the Canonical budget

```rust
let mut remaining = max_canonical_nodes.saturating_sub(canonical_count(graph));
for probe in stage3 {
    if remaining == 0 { break; }
    if /* not eligible */ { continue; }
    /* promote */; remaining -= 1;
}
```
-/

/-- Number of promotions the Stage 3 loop makes: `eligible` lists, in window
order, whether each probe passes every check in the loop body. -/
def stage3Promotions : Nat → List Bool → Nat
  | 0, _ => 0
  | _ + 1, [] => 0
  | remaining + 1, true :: rest => stage3Promotions remaining rest + 1
  | remaining + 1, false :: rest => stage3Promotions (remaining + 1) rest

/-- Stage 3 promotes at most the remaining budget. -/
theorem stage3_promotions_le :
    ∀ (eligible : List Bool) (remaining : Nat),
      stage3Promotions remaining eligible ≤ remaining := by
  intro eligible
  induction eligible with
  | nil =>
    intro remaining
    cases remaining <;> simp [stage3Promotions]
  | cons e rest ih =>
    intro remaining
    cases remaining with
    | zero => simp [stage3Promotions]
    | succ r =>
      cases e with
      | true =>
        have := ih r
        simp only [stage3Promotions]
        omega
      | false =>
        have := ih (r + 1)
        simp only [stage3Promotions]
        omega

/-- **No self-inflicted overflow.** If the Canonical count is within budget
before Stage 3, it is within budget after it, so the budget pass of the same
cycle demotes nothing that Stage 3 just promoted. -/
theorem stage3_no_overflow (maxCanonical count : Nat) (eligible : List Bool)
    (h : count ≤ maxCanonical) :
    count + stage3Promotions (maxCanonical - count) eligible ≤ maxCanonical := by
  have := stage3_promotions_le eligible (maxCanonical - count)
  omega

/-- A full budget promotes nobody (Rust test
`full_budget_evaluates_nothing_and_does_not_rotate_the_ring`). -/
theorem stage3_full_budget (maxCanonical count : Nat) (eligible : List Bool)
    (h : maxCanonical ≤ count) : stage3Promotions (maxCanonical - count) eligible = 0 := by
  have := stage3_promotions_le eligible (maxCanonical - count)
  omega

/-! ## Stage predicates

Thresholds as in the source. Coverage is the fraction `num / den`, compared by
cross-multiplication so no division appears. -/

/-- Stage 1 (Swarm), `stage1_candidates`: `gc_survived >= 3 &&
dependent_interactions >= 2`. -/
def stage1Passes (gcSurvived dependentInteractions : Nat) : Bool :=
  decide (3 ≤ gcSurvived) && decide (2 ≤ dependentInteractions)

/-- Stage 2, `stage2_evidence_passes`: `distinct >= 3 && coverage >= 0.3`. -/
def stage2Passes (distinct coverageNum coverageDen : Nat) : Bool :=
  decide (3 ≤ distinct) && decide (3 * coverageDen ≤ 10 * coverageNum)

/-- `in_repromotion_cooldown`: `Some(t)` and `now < t + cooldown`. -/
def inCooldown (lastDemotion : Option Int) (cooldown now : Int) : Bool :=
  match lastDemotion with
  | none => false
  | some t => decide (now < t + cooldown)

/-- Stage 3, `stage3_passes`: not cooling down, and `blast > 5`. -/
def stage3Passes (lastDemotion : Option Int) (cooldown now : Int) (blast : Nat) : Bool :=
  !inCooldown lastDemotion cooldown now && decide (5 < blast)

/-- More evidence never revokes a Stage 1 pass: `gc_survived` only grows
(`bump_gc_survived` is its only writer and it saturates), so a concept that
once met the survival bar keeps meeting it. -/
theorem stage1_mono {g g' d d' : Nat} (hg : g ≤ g') (hd : d ≤ d')
    (h : stage1Passes g d = true) : stage1Passes g' d' = true := by
  simp [stage1Passes] at h ⊢ <;> omega

/-- The Stage 1 bars are inclusive: exactly 3 survivals and 2 dependent
interactions pass, one fewer of either does not. -/
theorem stage1_boundary :
    stage1Passes 3 2 = true ∧ stage1Passes 2 2 = false ∧ stage1Passes 3 1 = false := by
  decide

/-- The Stage 2 bars are inclusive (Rust test
`reusable_predicate_keeps_threshold_equality_and_refuses_nonfinite`): three
distinct interactions at coverage exactly `0.3` pass. -/
theorem stage2_boundary :
    stage2Passes 3 3 10 = true ∧ stage2Passes 2 3 10 = false ∧ stage2Passes 3 29 100 = false := by
  decide

/-- The Stage 3 bar is strict: a blast radius of exactly 5 does not promote. -/
theorem stage3_boundary (cooldown now : Int) :
    stage3Passes none cooldown now 5 = false ∧ stage3Passes none cooldown now 6 = true := by
  constructor <;> first | rfl | simp [stage3Passes, inCooldown]

/-- "`None` is not a cooldown": a concept never demoted is never cooling. -/
theorem no_demotion_no_cooldown (cooldown now : Int) : inCooldown none cooldown now = false := by
  rfl

/-- A concept in its re-promotion cooldown cannot reach Canonical, whatever
its blast radius (Rust test `solo_cannot_bypass_the_repromotion_cooldown`). -/
theorem stage3_blocked_by_cooldown (t cooldown now : Int) (blast : Nat)
    (h : now < t + cooldown) : stage3Passes (some t) cooldown now blast = false := by
  simp [stage3Passes, inCooldown, h]

/-- The cooldown ends exactly at `t + cooldown`: the comparison is strict. -/
theorem cooldown_ends (t cooldown : Int) : inCooldown (some t) cooldown (t + cooldown) = false := by
  simp [inCooldown]

end Lambo.Canon
