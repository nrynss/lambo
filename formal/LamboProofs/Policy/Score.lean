import LamboProofs.Policy.Support

/-
# The Solo policy's score, bands and admission (`src/canon/policy.rs`)

Models `raw_solo_score` and `solo_score` (with `eviction_resistance`), the
weights `SESSION_WEIGHT`, `HUMAN_CONFIRMED_WEIGHT`,
`DEPENDENT_INTERACTION_WEIGHT`, `REVERT_WEIGHT`, the bars `CANDIDATE_BAR`,
`VENERABLE_BAR`, `CANONICAL_BAR`, `classify`, `revert_count`,
`SoloScorer::candidates` and `admits_hop`, `SwarmScorer`, `PromotionPolicy`
(`ALL`, the default, `scorer`, `as_str`, `from_str`), and how
`Evaluator::gather` and `apply` (`src/canon/eval.rs`) consult the policy, as
`Canon.Flags`, so `Canon.cycle` applies unchanged.

Results: the score is monotone in sessions, confirmations and dependent
interactions and antitone in reverts, each moving it by exactly its weight
times the resistance (`score10_mono`, `score10_steps`); the bands partition
the score range (`classify_*_iff`); Stage 1 under Solo is exactly
"resistant score ≥ 3.0" (`soloStage1_eq_admitsHop`, `mem_soloCandidates`);
under either policy a cycle moves a concept at most one rung
(`policy_cycle_one_hop`), a Solo promotion lands only on a rung its band
admits (`solo_cycle_capped`) and no policy promotes past a failed verdict
(`promotion_needs_verdict`); `PromotionPolicy::from_str` is fail-closed and
trims whitespace (`parse_sound`, `parse_padded`), and the Rust test table.

Boundary: scores are exact (tenths); the Rust computes in `f64`. The bands
agree with `f64` (the raw sum is exact and the three integer scores that land
on a bar under a non-unit resistance compute exactly; an argument, not a
proof), but the order does not on exact ties: `SoloScorer::candidates` sorts
by `f64` first, where the model ties and orders by key
(`exact_tie_orders_by_key`), which can change which members fit under the
evaluator's `batch_size`. Not modelled: the batch cap, ring cursors, Stage
2/3 measurements (their verdicts are inputs), error text, serde, and the
environment overlay's treatment of an all-whitespace
`LAMBO_PROMOTION_POLICY` as unset (`src/config.rs`), where `lambo.toml`
refuses it.
-/

namespace Lambo.Policy

open Lambo.Lib (nodup_eraseDups length_eraseDups_le_cons)

/-! ## The solo score

```text
(Sessions × 1.0) + (Human Confirmed × 4.0)
    + (Dependent Interactions × 2.0) − (Reverts × 3.0)
```

times the concept type's eviction resistance, compared with the bars 3.0,
6.0 and 10.0. Every resistance is a whole number of tenths, so the model
works in tenths: `score10 = 10 × score`, exactly. -/

/-- `ConceptType` (`src/types/mod.rs`). -/
inductive ConceptType where
  | entity
  | logic
  | constraint
  | resource
  | observation
  deriving DecidableEq, Repr

/-- `eviction_resistance`, in tenths: Constraint 1.5, Entity 1.2, Logic 1.1,
Resource 1.0, Observation 0.7.
Pinned by `formal_constants_tests::policy`. -/
def ConceptType.resistance10 : ConceptType → Int
  | .constraint => 15
  | .entity => 12
  | .logic => 11
  | .resource => 10
  | .observation => 7

/-- `SESSION_WEIGHT`.
Pinned by `formal_constants_tests::policy`. -/
def sessionWeight : Int := 1
/-- `HUMAN_CONFIRMED_WEIGHT`.
Pinned by `formal_constants_tests::policy`. -/
def humanConfirmedWeight : Int := 4
/-- `DEPENDENT_INTERACTION_WEIGHT`.
Pinned by `formal_constants_tests::policy`. -/
def dependentInteractionWeight : Int := 2
/-- `REVERT_WEIGHT`.
Pinned by `formal_constants_tests::policy`. -/
def revertWeight : Int := 3

/-- `CANDIDATE_BAR`, `VENERABLE_BAR`, `CANONICAL_BAR`, in tenths.
Pinned by `formal_constants_tests::policy`. -/
def candidateBar10 : Int := 30
/-- `VENERABLE_BAR`, in tenths. Pinned by `formal_constants_tests::policy`. -/
def venerableBar10 : Int := 60
/-- `CANONICAL_BAR`, in tenths. Pinned by `formal_constants_tests::policy`. -/
def canonicalBar10 : Int := 100

/-- The four terms (`SoloBreakdown`): `sessions` is `recurrence_sessions`,
`dependents` the aged, own-origin-excluded dependent-interaction count,
`reverts` is `revert_count`. `human_confirmed` is an `i32`. -/
structure Terms where
  sessions : Nat
  humanConfirmed : Int
  dependents : Nat
  reverts : Nat
  deriving DecidableEq, Repr

/-- `raw_solo_score`. -/
def rawScore (t : Terms) : Int :=
  (t.sessions : Int) * sessionWeight + t.humanConfirmed * humanConfirmedWeight +
    (t.dependents : Int) * dependentInteractionWeight - (t.reverts : Int) * revertWeight

/-- `solo_score`, in tenths. -/
def score10 (ty : ConceptType) (t : Terms) : Int := rawScore t * ty.resistance10

/-- `classify`, on a resistant score in tenths. -/
def classify (s : Int) : Canon.Status :=
  if canonicalBar10 ≤ s then .canonical
  else if venerableBar10 ≤ s then .venerable
  else if candidateBar10 ≤ s then .candidate
  else .none

/-- The band a concept's terms put it in. -/
def band (ty : ConceptType) (t : Terms) : Canon.Status := classify (score10 ty t)

theorem resistance10_pos (ty : ConceptType) : 0 < ty.resistance10 := by
  cases ty <;> decide

/-- Componentwise "more evidence": every positive term at least as large,
reverts no larger. -/
structure Terms.Le (t t' : Terms) : Prop where
  sessions : t.sessions ≤ t'.sessions
  humanConfirmed : t.humanConfirmed ≤ t'.humanConfirmed
  dependents : t.dependents ≤ t'.dependents
  reverts : t'.reverts ≤ t.reverts

theorem rawScore_mono {t t' : Terms} (h : t.Le t') : rawScore t ≤ rawScore t' := by
  have := h.sessions; have := h.humanConfirmed; have := h.dependents; have := h.reverts
  simp only [rawScore, sessionWeight, humanConfirmedWeight, dependentInteractionWeight,
    revertWeight]
  omega

/-- **Monotone in each positive term, antitone in reverts.** -/
theorem score10_mono (ty : ConceptType) {t t' : Terms} (h : t.Le t') :
    score10 ty t ≤ score10 ty t' :=
  Int.mul_le_mul_of_nonneg_right (rawScore_mono h) (Int.le_of_lt (resistance10_pos ty))

/-- `raw_solo_score` moves by exactly its weight times `eviction_resistance`
per term: one more session `SESSION_WEIGHT` (1), a confirmation
`HUMAN_CONFIRMED_WEIGHT` (4), a dependent interaction
`DEPENDENT_INTERACTION_WEIGHT` (2), a revert `REVERT_WEIGHT` (−3), so the
score is strictly monotone in each. -/
theorem score10_steps (ty : ConceptType) (t : Terms) :
    score10 ty { t with sessions := t.sessions + 1 } = score10 ty t + ty.resistance10 ∧
      score10 ty { t with humanConfirmed := t.humanConfirmed + 1 } =
        score10 ty t + 4 * ty.resistance10 ∧
      score10 ty { t with dependents := t.dependents + 1 } = score10 ty t + 2 * ty.resistance10 ∧
      score10 ty { t with reverts := t.reverts + 1 } = score10 ty t - 3 * ty.resistance10 := by
  simp only [score10, rawScore, sessionWeight, humanConfirmedWeight, dependentInteractionWeight,
    revertWeight]
  refine ⟨?_, ?_, ?_, ?_⟩ <;> push_cast <;> simp only [Int.add_mul, Int.sub_mul, Int.mul_one] <;>
    omega

/-- **The bands partition the score range**: each band is an interval, and
every score is in exactly the band `classify` names. -/
theorem classify_none_iff (s : Int) : classify s = .none ↔ s < candidateBar10 := by
  unfold classify candidateBar10 venerableBar10 canonicalBar10
  repeat' split
  all_goals simp
  all_goals omega

/-- `classify` (`src/canon/policy.rs`) puts a solo score in the Candidate band
exactly when it is at least `CANDIDATE_BAR` and below `VENERABLE_BAR`. -/
theorem classify_candidate_iff (s : Int) :
    classify s = .candidate ↔ candidateBar10 ≤ s ∧ s < venerableBar10 := by
  unfold classify candidateBar10 venerableBar10 canonicalBar10
  repeat' split
  all_goals simp
  all_goals omega

/-- `classify` puts a solo score in the Venerable band exactly when it is at
least `VENERABLE_BAR` and below `CANONICAL_BAR`. -/
theorem classify_venerable_iff (s : Int) :
    classify s = .venerable ↔ venerableBar10 ≤ s ∧ s < canonicalBar10 := by
  unfold classify candidateBar10 venerableBar10 canonicalBar10
  repeat' split
  all_goals simp
  all_goals omega

/-- `classify` puts a solo score in the Canonical band exactly when it is at
least `CANONICAL_BAR`. -/
theorem classify_canonical_iff (s : Int) : classify s = .canonical ↔ canonicalBar10 ≤ s := by
  unfold classify candidateBar10 venerableBar10 canonicalBar10
  repeat' split
  all_goals simp
  all_goals omega

/-- The band is monotone in the score. -/
theorem classify_mono {s s' : Int} (h : s ≤ s') : (classify s).rank ≤ (classify s').rank := by
  unfold classify candidateBar10 venerableBar10 canonicalBar10
  repeat' split
  all_goals simp [Canon.Status.rank]
  all_goals omega

/-- More evidence never lowers the band. -/
theorem band_mono (ty : ConceptType) {t t' : Terms} (h : t.Le t') :
    (band ty t).rank ≤ (band ty t').rank :=
  classify_mono (score10_mono ty h)

/-- Rust test `band_boundaries_are_inclusive`: every bar belongs to the band
above it; a reverted (negative) score is `None`. -/
theorem band_boundaries_are_inclusive :
    classify 29 = .none ∧ classify 0 = .none ∧ classify (-40) = .none ∧
      classify 30 = .candidate ∧ classify 35 = .candidate ∧
      classify 59 = .candidate ∧ classify 60 = .venerable ∧ classify 99 = .venerable ∧
      classify 100 = .canonical ∧ classify 135 = .canonical := by
  decide

/-- Terms with only a session count. -/
def sessionsOnly (n : Nat) : Terms := ⟨n, 0, 0, 0⟩

/-- Rust tests `eviction_resistance_multiplies_before_the_band_comparison`,
`integer_evidence_lands_exactly_on_bars`, `human_confirmations_are_real_and_weighted`
and `reverts_subtract_and_promotions_do_not_count`: four sessions are 4.8 for
an Entity (Candidate) and 2.8 for an Observation (None); a Resource with one
confirmation is 8.0 (Venerable) and with one dependent interaction more
exactly 10.0 (Canonical); two confirmations make 12.0; one revert takes four
sessions to 1.0. -/
theorem score_examples :
    score10 .entity (sessionsOnly 4) = 48 ∧ band .entity (sessionsOnly 4) = .candidate ∧
      score10 .observation (sessionsOnly 4) = 28 ∧ band .observation (sessionsOnly 4) = .none ∧
      band .resource ⟨4, 1, 0, 0⟩ = .venerable ∧
      score10 .resource ⟨4, 1, 1, 0⟩ = 100 ∧ band .resource ⟨4, 1, 1, 0⟩ = .canonical ∧
      score10 .resource ⟨4, 2, 0, 0⟩ = 120 ∧
      score10 .resource ⟨4, 0, 0, 1⟩ = 10 ∧ band .resource ⟨4, 0, 0, 1⟩ = .none := by
  decide

/-- The bulk-ingest corpus under `classify`: the Entity hub is a Candidate
counted on event time (4.8) and ordinary memory counted on flush time (1.2),
which is why the solo terms read `about_time`. -/
theorem bulk_ingest_band :
    band .entity (sessionsOnly (recurrenceSessions (spreadGraph true) 10)) = .candidate ∧
      band .entity (sessionsOnly (recurrenceSessions (spreadGraph false) 10)) = .none := by
  decide

/-! ### Reverts

`revert_count`: audited transitions of the node whose `to_status` ranks
strictly below its `from_status`. -/

/-- A `CanonizationEvent`, on the fields `revert_count` reads. -/
structure Event where
  node : Nat
  fromS : Canon.Status
  toS : Canon.Status
  deriving DecidableEq, Repr

def isRevert (id : Nat) (e : Event) : Bool := e.node == id && decide (e.toS.rank < e.fromS.rank)

/-- `revert_count`. -/
def revertCount (evs : List Event) (id : Nat) : Nat := (evs.filter (isRevert id)).length

/-- Through the write gate, every counted revert is `Canonical → None`
(whatever its reason: budget demotion or supersession, #20). -/
theorem revert_is_demotion (id : Nat) (e : Event) (h : isRevert id e = true)
    (hlegal : Canon.legal e.fromS e.toS = true) :
    e.fromS = .canonical ∧ e.toS = .none := by
  simp only [isRevert, Bool.and_eq_true, decide_eq_true_eq] at h
  exact Canon.legal_demotion_unique _ _ hlegal h.2

/-- `revert_count` counts a recorded status event exactly when it lowers the
concept's rank. -/
theorem revertCount_append (evs : List Event) (id : Nat) (e : Event) :
    revertCount (evs ++ [e]) id = revertCount evs id + (if isRevert id e then 1 else 0) := by
  unfold revertCount
  rw [List.filter_append, List.length_append]
  cases h : isRevert id e <;> simp [List.filter, h]

/-- `revert_count` never counts a promotion. -/
theorem promotion_not_revert (id : Nat) (e : Event) (h : e.fromS.rank ≤ e.toS.rank) :
    isRevert id e = false := by
  simp only [isRevert, Bool.and_eq_false_iff, decide_eq_false_iff_not]
  right; omega

/-! ## Admission

`SoloScorer::candidates` admits a concept to Stage 1 when its band is not
`None`; `SoloScorer::admits_hop` admits the hop to `to` when the band ranks at
or above `to`. `SwarmScorer` admits every hop (the stage verdict, which the
evaluator requires first, is the decision) and admits to Stage 1 on
`Canon.stage1Passes`. -/

/-- `SoloScorer::admits_hop`, given the node's band. -/
def admitsHop (b to : Canon.Status) : Bool := decide (to.rank ≤ b.rank)

/-- Stage-1 admission under Solo: `classify(score) != None`. -/
def soloStage1 (b : Canon.Status) : Bool := b != .none

/-- Stage-1 admission is admission of the hop to Candidate. -/
theorem soloStage1_eq_admitsHop (b : Canon.Status) : soloStage1 b = admitsHop b .candidate := by
  cases b <;> rfl

/-- `SoloScorer::admits_hop`: a band that admits a hop to a status admits
every hop to a lower one. -/
theorem admitsHop_down {b to to' : Canon.Status} (h : admitsHop b to = true)
    (h' : to'.rank ≤ to.rank) : admitsHop b to' = true := by
  simp only [admitsHop, decide_eq_true_eq] at h ⊢; omega

/-- `SoloScorer::admits_hop`: more evidence (sessions, confirmations,
dependent interactions, fewer reverts) never withdraws an admission. -/
theorem admitsHop_mono (ty : ConceptType) {t t' : Terms} (h : t.Le t') (to : Canon.Status)
    (ha : admitsHop (band ty t) to = true) : admitsHop (band ty t') to = true := by
  have := band_mono ty h
  simp only [admitsHop, decide_eq_true_eq] at ha ⊢; omega

/-- Rust test `solo_admission_climbs_with_the_band`: a Resource hub with four
sessions (4.0) is admitted to Candidate but not Venerable; one confirmation
(8.0) admits Venerable but not Canonical; one dependent interaction more
(exactly 10.0) admits Canonical. -/
theorem solo_admission_climbs_with_the_band :
    admitsHop (band .resource ⟨4, 0, 0, 0⟩) .candidate = true ∧
      admitsHop (band .resource ⟨4, 0, 0, 0⟩) .venerable = false ∧
      admitsHop (band .resource ⟨4, 1, 0, 0⟩) .venerable = true ∧
      admitsHop (band .resource ⟨4, 1, 0, 0⟩) .canonical = false ∧
      admitsHop (band .resource ⟨4, 1, 1, 0⟩) .canonical = true := by
  decide

/-! ### Policy selection

`PromotionPolicy` and `PromotionPolicy::scorer`. -/

/-- `PromotionPolicy`. -/
inductive PromotionPolicy where
  | swarm
  | solo
  deriving DecidableEq, Repr

/-- `#[default]`.
Pinned by `formal_constants_tests::policy`. -/
def PromotionPolicy.default : PromotionPolicy := .swarm

/-- `PromotionPolicy::ALL`. -/
def PromotionPolicy.all : List PromotionPolicy := [.swarm, .solo]

/-- `PromotionScorer::admits_hop` through `scorer()`. -/
def PromotionPolicy.admitsHop : PromotionPolicy → (b to : Canon.Status) → Bool
  | .swarm, _, _ => true
  | .solo, b, to => Lambo.Policy.admitsHop b to

/-- Stage-1 admission through `scorer()`: survival and dependents under
Swarm, the band under Solo. -/
def PromotionPolicy.stage1 : PromotionPolicy → (gcSurvived dependents : Nat) → Canon.Status → Bool
  | .swarm, gc, deps, _ => Canon.stage1Passes gc deps
  | .solo, _, _, b => soloStage1 b

/-- `PromotionPolicy::ALL` names every variant (Rust test
`all_covers_every_variant_and_round_trips_through_from_str`). -/
theorem mem_all (p : PromotionPolicy) : p ∈ PromotionPolicy.all := by
  cases p <;> decide

/-- The default is Swarm (Rust test `the_default_policy_is_swarm`). -/
theorem default_is_swarm : PromotionPolicy.default = .swarm := rfl

/-- Swarm's admission is the stage verdict (Rust test
`swarm_admission_is_the_evidence_verdict`). -/
theorem swarm_admits_every_hop (b to : Canon.Status) : PromotionPolicy.swarm.admitsHop b to = true :=
  rfl

/-- Rust test `solo_admits_what_swarm_refuses_on_the_same_corpus`: the
event-timed Entity hub (one writer, `gc_survived = 0`, no dependents) is a
Solo Candidate and not a Swarm one. -/
theorem solo_admits_what_swarm_refuses :
    PromotionPolicy.solo.stage1 0 0
        (band .entity (sessionsOnly (recurrenceSessions (spreadGraph true) 10))) = true ∧
      PromotionPolicy.swarm.stage1 0 0
        (band .entity (sessionsOnly (recurrenceSessions (spreadGraph true) 10))) = false := by
  decide

/-! ### Admission inside a canonization cycle

`Evaluator::gather` builds Stage 1 from `scorer.candidates` (then keeps the
still-`None` members), and keeps in the Stage 2 and Stage 3 rings only the
members `admits_hop` admits; `apply` promotes a Stage 2 or Stage 3 member only
on a passing verdict **and** a second `admits_hop` under the write lock.
`policyFlags` turns that into `Canon.Flags`: `bandG` is the band at gather,
`bandA` at apply, `v2`, `v3` the stage verdicts. -/

def policyFlags (p : PromotionPolicy) (g : Canon.Status) (gcSurvived dependents : Nat)
    (bandG bandA : Canon.Status) (v2 v3 supersede budget : Bool) : Canon.Flags where
  supersede := decide (g = .canonical) && supersede
  stage1 := decide (g = .none) && p.stage1 gcSurvived dependents bandG
  stage2 := decide (g = .candidate) && p.admitsHop bandG .venerable && v2 &&
    p.admitsHop bandA .venerable
  stage3 := decide (g = .venerable) && p.admitsHop bandG .canonical && v3 &&
    p.admitsHop bandA .canonical
  budget := decide (g = .canonical) && budget

theorem policyFlags_consistent (p : PromotionPolicy) (g : Canon.Status) (gc deps : Nat)
    (bandG bandA : Canon.Status) (v2 v3 sup bud : Bool) :
    Canon.Consistent g (policyFlags p g gc deps bandG bandA v2 v3 sup bud) := by
  refine ⟨?_, ?_, ?_, ?_, ?_⟩ <;> intro h <;>
    simp only [policyFlags, Bool.and_eq_true, decide_eq_true_eq] at h
  · exact h.1
  · exact h.1
  · exact h.1.1.1
  · exact h.1.1.1
  · exact h.1

/-- **No policy skips a rung** (`Canon.cycle_one_hop`): under either policy, a
cycle leaves a concept where it is or moves it along one eval-loop edge. -/
theorem policy_cycle_one_hop (p : PromotionPolicy) (g s : Canon.Status) (gc deps : Nat)
    (bandG bandA : Canon.Status) (v2 v3 sup bud : Bool) :
    let f := policyFlags p g gc deps bandG bandA v2 v3 sup bud
    Canon.cycle s f = s ∨ Canon.evalHop s (Canon.cycle s f) = true :=
  Canon.cycle_one_hop g s _ (policyFlags_consistent p g gc deps bandG bandA v2 v3 sup bud)

/-- Under either `PromotionPolicy`, one `Evaluator` cycle raises a concept's
status by at most one rank. -/
theorem policy_cycle_rank_le (p : PromotionPolicy) (g s : Canon.Status) (gc deps : Nat)
    (bandG bandA : Canon.Status) (v2 v3 sup bud : Bool) :
    (Canon.cycle s (policyFlags p g gc deps bandG bandA v2 v3 sup bud)).rank ≤ s.rank + 1 :=
  Canon.cycle_rank_le g s _ (policyFlags_consistent p g gc deps bandG bandA v2 v3 sup bud)

set_option maxHeartbeats 1000000 in
/-- A cycle raises a status only through the matching stage flag. -/
theorem cycle_up_flag (s : Canon.Status) (f : Canon.Flags)
    (h : s.rank < (Canon.cycle s f).rank) :
    (Canon.cycle s f = .candidate ∧ f.stage1 = true) ∨
      (Canon.cycle s f = .venerable ∧ f.stage2 = true) ∨
      (Canon.cycle s f = .canonical ∧ f.stage3 = true) := by
  revert h
  cases f with
  | mk supersede stage1 stage2 stage3 budget =>
    cases s <;> cases supersede <;> cases stage1 <;> cases stage2 <;> cases stage3 <;>
      cases budget <;> decide

/-- **The band caps the climb.** Under Solo a cycle promotes a concept only
into a rung its band admits, at gather and, for the later hops, at apply. -/
theorem solo_cycle_capped (g s : Canon.Status) (gc deps : Nat) (bandG bandA : Canon.Status)
    (v2 v3 sup bud : Bool)
    (h : s.rank < (Canon.cycle s (policyFlags .solo g gc deps bandG bandA v2 v3 sup bud)).rank) :
    let t := Canon.cycle s (policyFlags .solo g gc deps bandG bandA v2 v3 sup bud)
    admitsHop bandG t = true ∧ (t = .candidate ∨ admitsHop bandA t = true) := by
  intro t
  rcases cycle_up_flag s _ h with ⟨ht, hf⟩ | ⟨ht, hf⟩ | ⟨ht, hf⟩ <;>
    simp only [t] at ht ⊢ <;> rw [ht] <;>
    simp only [policyFlags, PromotionPolicy.stage1, PromotionPolicy.admitsHop, Bool.and_eq_true,
      soloStage1_eq_admitsHop] at hf
  · exact ⟨hf.2, Or.inl rfl⟩
  · exact ⟨hf.1.1.2, Or.inr hf.2⟩
  · exact ⟨hf.1.1.2, Or.inr hf.2⟩

/-- **No policy replaces the evidence**: a Stage 2 or Stage 3 promotion needs
its verdict whatever the policy and band. -/
theorem promotion_needs_verdict (p : PromotionPolicy) (g : Canon.Status) (gc deps : Nat)
    (bandG bandA : Canon.Status) (v2 v3 sup bud : Bool) :
    ((policyFlags p g gc deps bandG bandA v2 v3 sup bud).stage2 = true → v2 = true) ∧
      ((policyFlags p g gc deps bandG bandA v2 v3 sup bud).stage3 = true → v3 = true) := by
  constructor <;> intro h <;> simp only [policyFlags, Bool.and_eq_true] at h
  · exact h.1.2
  · exact h.1.2

/-- A Canonical-band concept with every verdict passing climbs one rung per
cycle: `None` reaches `Canonical` on the third cycle, not before. -/
def soloRun (b : Canon.Status) : Canon.Status → Nat → Canon.Status
  | s, 0 => s
  | s, n + 1 =>
    soloRun b (Canon.cycle s (policyFlags .solo s 0 0 b b true true false false)) n

/-- Under `PromotionPolicy::Solo`, a concept whose band is Canonical climbs
one rung per `Evaluator` cycle (Candidate, Venerable, then Canonical on the
third), and one whose band is Venerable stops there. -/
theorem solo_three_cycles :
    soloRun .canonical .none 1 = .candidate ∧ soloRun .canonical .none 2 = .venerable ∧
      soloRun .canonical .none 3 = .canonical ∧ soloRun .venerable .none 3 = .venerable := by
  decide

/-! ### The fixed terms through the score and the cycle (#300)

The score, the bands and the cycle read only the four terms, so every result
above about them (`score10_mono`, `band_mono`, `admitsHop_mono`,
`policy_cycle_one_hop`, `solo_cycle_capped`, `promotion_needs_verdict`)
holds whatever measured the terms, the fixed dating included. Spelled out
for the fixed terms: -/

/-- The Solo terms under the fixed dating (age floor cleared). -/
def termsW (g : Graph) (st : Edge → Stamp) (c : Nat) (hc : Int) (reverts : Nat) : Terms :=
  ⟨recurrenceSessionsW g st c, hc, (dependentOriginsW g st c).length, reverts⟩

/-- No skipped rung with bands from the fixed terms, at gather and apply. -/
theorem policy_cycle_one_hop_fixed (p : PromotionPolicy) (gs s : Canon.Status) (gc deps : Nat)
    (ty : ConceptType) (gG gA : Graph) (st : Edge → Stamp) (c : Nat) (hc : Int) (r : Nat)
    (v2 v3 sup bud : Bool) :
    let f := policyFlags p gs gc deps (band ty (termsW gG st c hc r))
      (band ty (termsW gA st c hc r)) v2 v3 sup bud
    Canon.cycle s f = s ∨ Canon.evalHop s (Canon.cycle s f) = true :=
  policy_cycle_one_hop p gs s gc deps _ _ v2 v3 sup bud

/-- An extra edge never lowers either fixed term, so it never lowers the
band. -/
theorem band_mono_fixed_edge (ty : ConceptType) (g : Graph) (st : Edge → Stamp) (c : Nat)
    (e : Edge) (hc : Int) (r : Nat) :
    (band ty (termsW g st c hc r)).rank ≤
      (band ty (termsW { g with edges := e :: g.edges } st c hc r)).rank :=
  band_mono ty ⟨recurrenceW_mono_edge g st c e, Int.le_refl _,
    dependentOriginsW_mono_edge g st c e, Nat.le_refl _⟩

/-! ### Stage-1 order under Solo

`SoloScorer::candidates`: every concept whose band is not `None`, ordered by
resistant score (highest first), then canonical key, then id. Keys and ids are
natural numbers standing for their orders (`str` and `Uuid` comparison). -/

/-- A concept as Stage 1 sees it. -/
structure Cand where
  id : Nat
  key : Nat
  ty : ConceptType
  terms : Terms
  deriving DecidableEq, Repr

def Cand.score (c : Cand) : Int := score10 c.ty c.terms

/-- `sb.total_cmp(sa).then_with(tie_break_by_key ...)`, as "orders at or
before". -/
def candLe (a b : Cand) : Bool :=
  decide (b.score < a.score) ||
    (decide (a.score = b.score) && (decide (a.key < b.key) || (decide (a.key = b.key) && decide (a.id ≤ b.id))))

theorem candLe_trans (a b c : Cand) (h1 : candLe a b = true) (h2 : candLe b c = true) :
    candLe a c = true := by
  simp only [candLe, Bool.or_eq_true, Bool.and_eq_true, decide_eq_true_eq] at h1 h2 ⊢
  omega

theorem candLe_total (a b : Cand) : (candLe a b || candLe b a) = true := by
  simp only [candLe, Bool.or_eq_true, Bool.and_eq_true, decide_eq_true_eq]
  omega

/-- `SoloScorer::candidates`, before the evaluator keeps the still-`None` members. -/
def soloCandidates (cs : List Cand) : List Cand :=
  (cs.filter (fun c => soloStage1 (classify c.score))).mergeSort candLe

/-- **Stage-1 admission** is exactly "the band is not `None`", i.e. a
resistant score of at least 3.0. -/
theorem mem_soloCandidates (cs : List Cand) (c : Cand) :
    c ∈ soloCandidates cs ↔ c ∈ cs ∧ candidateBar10 ≤ c.score := by
  unfold soloCandidates
  rw [(List.mergeSort_perm _ _).mem_iff, List.mem_filter]
  have := classify_none_iff c.score
  constructor
  · rintro ⟨hm, hs⟩
    refine ⟨hm, ?_⟩
    simp only [soloStage1, bne_iff_ne, ne_eq] at hs
    rcases Int.lt_or_le c.score candidateBar10 with hlt | hle
    · exact absurd (this.mpr hlt) hs
    · exact hle
  · rintro ⟨hm, hs⟩
    refine ⟨hm, ?_⟩
    simp only [soloStage1, bne_iff_ne, ne_eq]
    intro h; have := this.mp h; omega

/-- In exact arithmetic, the admitted concepts come out in the policy's
order (score, key, id). The Rust sorts on `f64` scores, which can break an
exact tie by type (see `exact_tie_orders_by_key`). -/
theorem soloCandidates_sorted (cs : List Cand) :
    (soloCandidates cs).Pairwise (fun a b => candLe a b = true) :=
  List.pairwise_mergeSort candLe_trans candLe_total _

/-! ### Parsing `promotion_policy`

`impl FromStr for PromotionPolicy`: trim (Unicode `White_Space`, as
`str::trim`), refuse the empty string, then compare ASCII-lowercased against
each `as_str()` in `ALL`. Strings are lists of characters. -/

/-- `as_str`. -/
def PromotionPolicy.asStr : PromotionPolicy → List Char
  | .swarm => "Swarm".toList
  | .solo => "Solo".toList

/-- `char::is_whitespace`: the Unicode `White_Space` property. -/
def isWs (c : Char) : Bool :=
  let n := c.toNat
  (9 ≤ n && n ≤ 13) || n == 0x20 || n == 0x85 || n == 0xA0 || n == 0x1680 ||
    (0x2000 ≤ n && n ≤ 0x200A) || n == 0x2028 || n == 0x2029 || n == 0x202F || n == 0x205F ||
    n == 0x3000

/-- `str::trim`. -/
def trim (cs : List Char) : List Char :=
  ((cs.dropWhile isWs).reverse.dropWhile isWs).reverse

/-- `to_ascii_lowercase` (`Char.toLower` changes only `A`–`Z`). -/
def lower (cs : List Char) : List Char := cs.map Char.toLower

/-- `PromotionPolicy::from_str`, with every refusal as `none`. -/
def parse (cs : List Char) : Option PromotionPolicy :=
  if trim cs = [] then none
  else PromotionPolicy.all.find? (fun p => lower p.asStr == lower (trim cs))

/-- Every value round-trips through its own spelling (Rust test
`all_covers_every_variant_and_round_trips_through_from_str`). -/
theorem parse_asStr (p : PromotionPolicy) : parse p.asStr = some p := by
  cases p <;> decide

/-- **Fail-closed.** A value is accepted only if, trimmed and lowercased, it
*is* a valid spelling: nothing unrecognised falls back to the default. -/
theorem parse_sound (cs : List Char) (p : PromotionPolicy) (h : parse cs = some p) :
    lower (trim cs) = lower p.asStr := by
  unfold parse at h
  split at h
  · cases h
  · have := List.find?_some h
    simp only [beq_iff_eq] at this
    exact this.symm

theorem dropWhile_ws_prefix (ws l : List Char) (h : ∀ c, c ∈ ws → isWs c = true) :
    (ws ++ l).dropWhile isWs = l.dropWhile isWs := by
  induction ws with
  | nil => rfl
  | cons c ws ih =>
    rw [List.cons_append, List.dropWhile_cons, ite_eq_left (h c (by simp))]
    exact ih (fun d hd => h d (List.mem_cons_of_mem _ hd))

/-- Leading whitespace is trimmed away. -/
theorem trim_ws_prefix (ws l : List Char) (h : ∀ c, c ∈ ws → isWs c = true) :
    trim (ws ++ l) = trim l := by
  unfold trim; rw [dropWhile_ws_prefix ws l h]

/-- Rust test `from_str_is_lenient_about_shape_and_strict_about_membership`. -/
theorem from_str_is_lenient_about_shape_and_strict_about_membership :
    parse "Swarm".toList = some .swarm ∧ parse "swarm".toList = some .swarm ∧
      parse "SWARM".toList = some .swarm ∧ parse "  sWaRm\t".toList = some .swarm ∧
      parse "Solo".toList = some .solo ∧ parse "solo".toList = some .solo ∧
      parse "\n SOLO ".toList = some .solo ∧
      parse "".toList = none ∧ parse "   ".toList = none ∧ parse "\t\n".toList = none ∧
      parse "Solitary".toList = none ∧ parse "swarms".toList = none ∧
      parse "sol o".toList = none ∧ parse "Both".toList = none := by
  decide

/-- `str::trim` also strips Unicode spaces such as U+00A0 and U+3000. -/
theorem parse_unicode_space : parse [' ', 'S', 'o', 'l', 'o', '　'] = some .solo := by
  decide

theorem dropWhile_all_ws (ws : List Char) (h : ∀ c, c ∈ ws → isWs c = true) :
    ws.dropWhile isWs = [] := by
  have := dropWhile_ws_prefix ws [] h
  simpa using this

/-- Trailing whitespace is trimmed away. -/
theorem trim_ws_suffix (l ws : List Char) (h : ∀ c, c ∈ ws → isWs c = true) :
    trim (l ++ ws) = trim l := by
  unfold trim
  rw [List.dropWhile_append]
  split
  · rename_i he
    rw [List.isEmpty_iff] at he
    rw [he, dropWhile_all_ws ws h]
  · rw [List.reverse_append, dropWhile_ws_prefix ws.reverse _
      (fun c hc => h c (List.mem_reverse.mp hc))]

/-- `PromotionPolicy::from_str` trims whitespace (`str::trim`), so padding
around a configured value never changes the policy it parses to. -/
theorem parse_padded (ws cs ws' : List Char) (h : ∀ c, c ∈ ws → isWs c = true)
    (h' : ∀ c, c ∈ ws' → isWs c = true) : parse (ws ++ cs ++ ws') = parse cs := by
  unfold parse
  rw [List.append_assoc, trim_ws_prefix ws _ h, trim_ws_suffix cs ws' h']

/-- An exact tie that `f64` breaks: a Logic concept with raw 7 and an
Observation concept with raw 11 both score exactly 7.7, so the model orders
them by key and puts the Observation (key 1) first. In `f64` the Logic score
is `7.700000000000001` and the Observation score `7.699999999999999`, and
`sb.total_cmp(sa)` puts the Logic concept first whatever the keys. -/
theorem exact_tie_orders_by_key :
    score10 .logic (sessionsOnly 7) = score10 .observation (sessionsOnly 11) ∧
      (soloCandidates [⟨1, 2, .logic, sessionsOnly 7⟩,
        ⟨2, 1, .observation, sessionsOnly 11⟩]).map (·.id) = [2, 1] := by
  refine ⟨by decide, ?_⟩
  simp [soloCandidates, candLe, Cand.score, score10, rawScore,
    sessionsOnly, ConceptType.resistance10, classify, soloStage1, sessionWeight,
    humanConfirmedWeight, dependentInteractionWeight, revertWeight, candidateBar10,
    venerableBar10, canonicalBar10]

end Lambo.Policy
