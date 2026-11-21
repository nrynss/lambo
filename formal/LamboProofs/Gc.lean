import LamboProofs.Canon

/-
# Garbage collection: what a sweep may and may not collect

Model of the classification inside `gc::run` (`src/daemon/gc.rs`), of the
collection cap, of the survivor bump (`Graph::bump_gc_survived`,
`src/graph/graph/transitions.rs`) and of the trigger `sweep_due`.

## Model

A sweep removes a concept in one of three ways: as an *orphan* (no incident
edge that counts toward GC connectivity), as part of a *disconnected
component* (not reachable from the temporal chain), or by the *score cut*
(its eviction score is below its type's bar). `collectable` is the union of
the three conditions for one concept, before the per-sweep cap is applied:

* `prot`: the concept is in the protected set;
* `conn`: `has_gc_connection`;
* `reach`: reachable from the temporal chain;
* `exempt`: `exempt_from_gc_score_cut` (Logic, Constraint, Observation);
* `below`: `score < bar`;
* `dependedOn`: a Resource with dependents (`resources_with_dependents`).

Not modelled: how the score and the reachable set are computed, the edge
cleanup of step 1, and the cascade pass (which applies the same
"not protected" filter).

## Results

* `protected_not_collectable`, `venerable_canonical_never_collected`: a
  protected concept, in particular any Venerable or Canonical one, is never
  collected, by any of the three routes.
* `score_cut_only_entities_and_resources`: a connected, reachable concept of
  an exempt type is never collected ("the score cut therefore only removes
  Entities and Resources").
* `dependents_spare_only_the_score_cut`: being depended on protects a Resource
  from the score cut and from nothing else.
* `contradiction_keeps_nothing_alive`: connectivity is the only structural
  protection, and `Contradicts` edges do not count toward it.
* `taken_le_budget`, `cap_ge_floor`: a sweep collects at most its cap.
* `bump_mono`, `bump_never_resets`: `gc_survived` only grows.
* `idle_never_sweeps`: "an idle session never sweeps on time alone".
-/

namespace Lambo.Gc

open Lambo.Canon (Status)

/-! ## Classification -/

/-- Whether one sweep may collect a concept, before the cap. -/
def collectable (prot conn reach exempt below dependedOn : Bool) : Bool :=
  !prot && (!conn || !reach || (!exempt && below && !dependedOn))

/-- **Protection is absolute.** A protected concept is "never collected as an
orphan, by the score cut or as a disconnected component". -/
theorem protected_not_collectable (conn reach exempt below dependedOn : Bool) :
    collectable true conn reach exempt below dependedOn = false := by
  cases conn <;> cases reach <;> cases exempt <;> cases below <;> cases dependedOn <;> rfl

/-- `is_protected`, plus the two contradiction-based protections: a concept is
protected when it is Venerable or Canonical, or a root-goal concept, or it
contradicts a promoted concept, or it is a live correction. -/
def isProtected (status : Status) (rootGoal contradictsPromoted liveCorrection : Bool) : Bool :=
  decide (status = .venerable) || decide (status = .canonical) || rootGoal
    || contradictsPromoted || liveCorrection

/-- A Venerable or Canonical concept is always in the protected set. -/
theorem promoted_is_protected (status : Status) (rootGoal contradictsPromoted liveCorrection : Bool)
    (h : status = .venerable ∨ status = .canonical) :
    isProtected status rootGoal contradictsPromoted liveCorrection = true := by
  cases h with
  | inl hv =>
    rw [hv]
    cases rootGoal <;> cases contradictsPromoted <;> cases liveCorrection <;> decide
  | inr hc =>
    rw [hc]
    cases rootGoal <;> cases contradictsPromoted <;> cases liveCorrection <;> decide

/-- **GC never collects a Venerable or Canonical concept**, whatever its score
and however disconnected it is. Promotion is how a concept earns exemption
from collection. -/
theorem venerable_canonical_never_collected (status : Status)
    (rootGoal contradictsPromoted liveCorrection conn reach exempt below dependedOn : Bool)
    (h : status = .venerable ∨ status = .canonical) :
    collectable (isProtected status rootGoal contradictsPromoted liveCorrection)
      conn reach exempt below dependedOn = false := by
  rw [promoted_is_protected status rootGoal contradictsPromoted liveCorrection h]
  exact protected_not_collectable conn reach exempt below dependedOn

/-- A `Candidate` (or `None`) status confers no protection by itself. -/
theorem candidate_not_protected :
    isProtected .candidate false false false = false ∧
    isProtected .none false false false = false := by
  decide

/-- **The score cut only removes Entities and Resources.** A concept of an
exempt type that is connected and reachable is never collected. -/
theorem score_cut_only_entities_and_resources (prot below dependedOn : Bool) :
    collectable prot true true true below dependedOn = false := by
  cases prot <;> cases below <;> cases dependedOn <;> rfl

/-- Exempt types are still collected as orphans and as disconnected islands
(Rust test `exempt_types_are_still_collected_as_orphans_and_islands`). -/
theorem exempt_still_collected_structurally (below dependedOn : Bool) :
    collectable false false true true below dependedOn = true ∧
    collectable false true false true below dependedOn = true := by
  cases below <;> cases dependedOn <;> decide

/-- **Dependents spare only the score cut.** A connected, reachable Resource
with dependents is not collected; the same Resource is collected once it is
unreachable from the temporal chain ("only the score cut honours this"). -/
theorem dependents_spare_only_the_score_cut (below : Bool) :
    collectable false true true false below true = false ∧
    collectable false true false false below true = true := by
  cases below <;> decide

/-- A connected, reachable concept at or above its bar is kept. The bar
comparison is strict (`score < bar`), so a score exactly on the bar survives. -/
theorem at_or_above_bar_survives (prot exempt dependedOn : Bool) :
    collectable prot true true exempt false dependedOn = false := by
  cases prot <;> cases exempt <;> cases dependedOn <;> rfl

/-- An unprotected concept with no GC connection is collected whatever its
type and score. `conn` counts every incident edge type except `Contradicts`
and `SuspectedSupersedes`, so a contradiction "keeps neither of its concepts
alive". -/
theorem contradiction_keeps_nothing_alive (reach exempt below dependedOn : Bool) :
    collectable false false reach exempt below dependedOn = true := by
  cases reach <;> cases exempt <;> cases below <;> cases dependedOn <;> rfl

/-- `EdgeType`, as far as GC connectivity is concerned. -/
inductive EdgeType where
  | temporal
  | derives
  | coOccurrence
  | causal
  | dependency
  | hierarchical
  | semantic
  | contradicts
  | suspectedSupersedes
  deriving DecidableEq, Repr

/-- `counts_toward_gc_connectivity` (`src/types/mod.rs`). -/
def EdgeType.countsTowardGcConnectivity : EdgeType → Bool
  | .contradicts => false
  | .suspectedSupersedes => false
  | _ => true

/-- `has_gc_connection`: some incident edge counts. -/
def hasGcConnection (incident : List EdgeType) : Bool :=
  incident.any EdgeType.countsTowardGcConnectivity

/-- A concept whose only incident edges are contradictions and suspected
supersessions has no GC connection: it is an orphan. -/
theorem only_contradictions_is_orphan :
    ∀ (incident : List EdgeType),
      (∀ e, e ∈ incident → e = .contradicts ∨ e = .suspectedSupersedes) →
      hasGcConnection incident = false := by
  intro incident
  induction incident with
  | nil =>
    intro _
    rfl
  | cons e rest ih =>
    intro h
    have he := h e (List.mem_cons.mpr (Or.inl rfl))
    have hrest : hasGcConnection rest = false :=
      ih (fun x hx => h x (List.mem_cons.mpr (Or.inr hx)))
    have hhead : e.countsTowardGcConnectivity = false := by
      cases he with
      | inl h1 => rw [h1] <;> rfl
      | inr h2 => rw [h2] <;> rfl
    show (e.countsTowardGcConnectivity || rest.any EdgeType.countsTowardGcConnectivity) = false
    rw [hhead]
    exact hrest

/-! ## The cap -/

/-- `collection_cap`: the larger of the floor and the fractional cap. -/
def collectionCap (minCollectCap fractional : Nat) : Nat := max minCollectCap fractional

theorem cap_ge_floor (minCollectCap fractional : Nat) :
    minCollectCap ≤ collectionCap minCollectCap fractional := by
  unfold collectionCap
  exact Nat.le_max_left minCollectCap fractional

/-- `candidates[..take]` with `take = min(len, budget)`: a sweep removes at
most its budget, and what it removes is a prefix of the candidate order
(orphans, then disconnected components, then the score cut's weakest first). -/
theorem taken_le_budget {ι : Type} (candidates : List ι) (budget : Nat) :
    (candidates.take (min candidates.length budget)).length ≤ budget := by
  first
    | (rw [List.length_take]; omega)
    | exact Nat.le_trans (List.length_take_le _ _) (Nat.min_le_right _ _)

/-- Nothing is held back exactly when the candidates fit in the budget. -/
theorem none_deferred_iff {ι : Type} (candidates : List ι) (budget : Nat) :
    candidates.length - min candidates.length budget = 0 ↔ candidates.length ≤ budget := by
  omega

/-! ## Survivor bumps -/

/-- `i32::MAX`. -/
def i32Max : Nat := 2147483647

/-- `gc_survived.saturating_add(1)`. -/
def bump (gcSurvived : Nat) : Nat := if gcSurvived < i32Max then gcSurvived + 1 else gcSurvived

/-- A bump never lowers the count. `bump_gc_survived` is the only writer of
`gc_survived`, so the count never decreases: a concept that has met Stage 1's
survival bar (`Canon.stage1_mono`) cannot fall back below it. -/
theorem bump_mono (g : Nat) : g ≤ bump g := by
  unfold bump
  repeat' split
  all_goals omega

/-- No number of bumps resets the count. -/
theorem bump_never_resets (g : Nat) : ∀ (n : Nat), g ≤ Nat.repeat bump n g := by
  intro n
  induction n with
  | zero => exact Nat.le_refl g
  | succ n ih =>
    show g ≤ bump (Nat.repeat bump n g)
    exact Nat.le_trans ih (bump_mono _)

/-- Below saturation a bump adds exactly one survival. -/
theorem bump_adds_one (g : Nat) (h : g < i32Max) : bump g = g + 1 := by
  unfold bump
  exact if_pos h

/-- Three sweeps survived are enough for Stage 1's `gc_survived >= 3`, and two
are not. -/
theorem three_sweeps_reach_stage_one :
    Nat.repeat bump 3 0 = 3 ∧ Nat.repeat bump 2 0 = 2 := by
  first
    | decide
    | exact ⟨rfl, rfl⟩

/-! ## The trigger -/

/-- Why a sweep runs. -/
inductive Trigger where
  | mutations
  | elapsed
  deriving DecidableEq, Repr

/-- `sweep_due`: `since` is the number of mutations since the last sweep;
`longEnough` says `gc_max_interval` has elapsed since it. -/
def sweepDue (since gcInterval idleFloor : Nat) (longEnough : Bool) : Option Trigger :=
  if gcInterval ≤ since then some .mutations
  else if since < idleFloor then none
  else if longEnough = true then some .elapsed
  else none

/-- **An idle session never sweeps on time alone.** With fewer mutations than
the idle floor since the last sweep, no amount of elapsed time triggers one. -/
theorem idle_never_sweeps (since gcInterval idleFloor : Nat) (longEnough : Bool)
    (hidle : since < idleFloor) (hfloor : idleFloor ≤ gcInterval) :
    sweepDue since gcInterval idleFloor longEnough = none := by
  have h1 : ¬ gcInterval ≤ since := by omega
  unfold sweepDue
  rw [if_neg h1, if_pos hidle]

/-- The mutation trigger is ungated: enough mutations always sweep (Rust test
`sweep_due_mutation_trigger_is_unchanged_and_ungated`). -/
theorem mutations_always_sweep (since gcInterval idleFloor : Nat) (longEnough : Bool)
    (h : gcInterval ≤ since) :
    sweepDue since gcInterval idleFloor longEnough = some .mutations := by
  unfold sweepDue
  rw [if_pos h]

/-- The time trigger needs both the interval and the floor. -/
theorem time_trigger_needs_both (since gcInterval idleFloor : Nat) (longEnough : Bool)
    (h : sweepDue since gcInterval idleFloor longEnough = some .elapsed) :
    idleFloor ≤ since ∧ longEnough = true := by
  unfold sweepDue at h
  by_cases h1 : gcInterval ≤ since
  · rw [if_pos h1] at h
    have h' : Trigger.mutations = Trigger.elapsed := Option.some.inj h
    exact absurd h' (by decide)
  · rw [if_neg h1] at h
    by_cases h2 : since < idleFloor
    · rw [if_pos h2] at h
      cases h
    · rw [if_neg h2] at h
      by_cases h3 : longEnough = true
      · exact ⟨by omega, h3⟩
      · rw [if_neg h3] at h
        cases h

end Lambo.Gc
