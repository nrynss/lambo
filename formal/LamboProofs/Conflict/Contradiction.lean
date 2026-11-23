import LamboProofs.Conflict.Detect

/-
# Contradiction dating (`src/daemon/contradiction.rs`)

Models `contradiction_at_with` (`contradictionAt`): `declared_at`
(`declaredAt`, the event time clamped to the write time), `written_by`, the
age cutoff (`ChronoDuration::from_std(min_age)` then `checked_sub_signed`),
the evidence guards (`eligible`, `counts`), the baseline (`baselineOf`) and
the choice of the hit (`latest`, `admissible`, `rank`, `better`), over the
edges `Detect.lean`'s attribution reads.

Results: `declared_at` never dates a declaration after its write
(`declaredAt_le_createdAt`) and a re-send never re-dates it
(`declaredAt_resend`, since `Graph::record_edge` keeps a duplicate's
`created_at` and `event_time`); evidence counts by write time, so a backdated
citation does not skip the age floor (`eligible_eventTime`,
`backdated_not_eligible`); with `now - min_age` out of range nothing is
eligible (`unrepresentable_minAge`); a hit is an admissible declaration
written by `now`, dated strictly after the baseline
(`contradictionAt_sound`); more evidence only outweighs
(`admissible_of_more_evidence`); and the hit is the newest, then smallest key,
then smallest id, whatever the edge order (`contradictionAt_max`,
`contradictionAt_perm`). The concrete cases pin the Rust tests named in
their doc comments.

Boundary: canonical keys are `Nat` (the Rust compares `String`s); the inputs
are the node's edges already split by the Rust's first filters (evidence is
a dependence edge with the node as pillar, a declaration an inbound non-self
`Contradicts`); the remaining guards are Booleans in the Rust's order. Not
modelled: `insert_contradictions`, `forces_inclusion`, `contradicts_either_way`
and concurrency.
-/

namespace Lambo.Conflict

open Lambo.Lib (two_le_length_iff)
variable {A : Type} [DecidableEq A]

/-! ## Contradiction dating (`contradiction.rs`) -/

/-- `Edge::about_time`: the event time, else the write time. -/
def aboutTime (e : Edge A) : Int := e.eventTime.getD e.createdAt

/-- `declared_at`: the about-time clamped to the write time. -/
def declaredAt (e : Edge A) : Int := min (aboutTime e) e.createdAt

/-- `written_by`: the edge exists at `now` iff it was written by then. -/
def writtenBy (e : Edge A) (now : Int) : Bool := decide (e.createdAt ≤ now)

/-- `ChronoDuration::from_std(min_age).ok().and_then(|age|
now.checked_sub_signed(age))`: `none` when `min_age` is not representable or
`now - min_age` is below `MIN_UTC`. -/
def cutoff (lo : Int) (minAge : Option Nat) (now : Int) : Option Int :=
  match minAge with
  | none => none
  | some a => if now - a < lo then none else some (now - a)

/-- The node's origin about-time, clamped like every date here (`baseline`
in `contradiction_at_with`): the origin interaction's `(created_at,
event_time)` when it is in the graph, else the node's own `created_at`. -/
def baselineOf (origin : Option (Int × Option Int)) (targetCreated : Int) : Int :=
  match origin with
  | some (c, ev) => min (ev.getD c) c
  | none => targetCreated

/-- A dependence edge with the node as pillar (`Edge::dependence`), and what
the guards read about its dependent. -/
structure Evidence (A : Type) where
  edge : Edge A
  dependent : Nat
  dependentIsConcept : Bool := true
  sameSession : Bool := true
  /-- `dep.origin_interaction == target.origin_interaction`. -/
  ownOrigin : Bool := false
  deriving DecidableEq, Repr

/-- An inbound non-self `Contradicts` edge, and what the Rust reads about its
source concept `by`. -/
structure Decl (A : Type) where
  edge : Edge A
  byId : Nat
  /-- `by.canonical_key`, as a number under the same order. -/
  key : Nat
  byIsConcept : Bool := true
  sameSession : Bool := true
  deriving DecidableEq, Repr

/-- The evidence guards in order: a concept of the session, not from the
node's own origin, and **written** at or before the cutoff. -/
def eligible (lo now : Int) (minAge : Option Nat) (ev : Evidence A) : Bool :=
  ev.dependentIsConcept && ev.sameSession && !ev.ownOrigin &&
    match cutoff lo minAge now with
    | none => false
    | some c => decide (ev.edge.createdAt ≤ c)

/-- Attribution at the first write (`attribute_at(.., e.created_at, ..)`). -/
def firstWriters (tl : List (Interaction A)) (e : Edge A) : List A × A :=
  attributeAt tl e.source e.createdAt

/-- Self-suppression: evidence counts against `d` unless its dependent is
`by` itself or one of its writers is one of `d`'s declarers. -/
def counts (tl : List (Interaction A)) (d : Decl A) (ev : Evidence A) : Bool :=
  ev.dependent != d.byId &&
    !((firstWriters tl ev.edge).1.any (fun w => (firstWriters tl d.edge).1.contains w))

/-- The latest date `d` must beat: the newest eligible, counting evidence, or
the baseline. -/
def latest (lo now : Int) (minAge : Option Nat) (baseline : Int) (tl : List (Interaction A))
    (evs : List (Evidence A)) (d : Decl A) : Int :=
  ((evs.filter (eligible lo now minAge)).filter (counts tl d)).foldl
    (fun m ev => max m (declaredAt ev.edge)) baseline

/-- Whether `d` is a qualifying contradiction at `now`. -/
def admissible (lo now : Int) (minAge : Option Nat) (baseline : Int)
    (tl : List (Interaction A)) (evs : List (Evidence A)) (d : Decl A) : Bool :=
  d.byIsConcept && d.sameSession && writtenBy d.edge now &&
    decide (latest lo now minAge baseline tl evs d < declaredAt d.edge)

/-- The order key: newest date, then smallest key, then smallest id. -/
def rank (d : Decl A) : Int × Nat × Nat := (declaredAt d.edge, d.key, d.byId)

/-- `at.cmp(best_at).then(best_by.key.cmp(by.key)).then(best_by.id.cmp(by.id)).is_gt()`. -/
def better (c b : Int × Nat × Nat) : Bool :=
  decide (b.1 < c.1) || (decide (c.1 = b.1) &&
    (decide (c.2.1 < b.2.1) || (decide (c.2.1 = b.2.1) && decide (c.2.2 < b.2.2))))

def selStep (adm : Decl A → Bool) (best : Option (Decl A)) (d : Decl A) : Option (Decl A) :=
  if adm d then
    match best with
    | none => some d
    | some b => if better (rank d) (rank b) then some d else best
  else best

/-- `ContradictionHit` without its node, and the
`HotListPayload::Contradiction` fields. -/
structure CHit (A : Type) where
  byId : Nat
  writer : A
  date : Int
  deriving DecidableEq, Repr

/-- `contradiction_at_with`. -/
def contradictionAt (lo now : Int) (minAge : Option Nat) (baseline : Int)
    (tl : List (Interaction A)) (evs : List (Evidence A)) (decls : List (Decl A)) :
    Option (CHit A) :=
  (decls.foldl (selStep (admissible lo now minAge baseline tl evs)) none).map
    fun d => ⟨d.byId, (firstWriters tl d.edge).2, declaredAt d.edge⟩

/-! ### The clamp -/

omit [DecidableEq A] in
theorem declaredAt_le_createdAt (e : Edge A) : declaredAt e ≤ e.createdAt := by
  simp [declaredAt]; omega

omit [DecidableEq A] in
/-- `declared_at` never dates a declaration later than its event time. -/
theorem declaredAt_le_aboutTime (e : Edge A) : declaredAt e ≤ aboutTime e := by
  simp [declaredAt]; omega

omit [DecidableEq A] in
/-- A backdated (or same-instant) event time is the date. -/
theorem declaredAt_of_le {e : Edge A} {t : Int} (h : e.eventTime = some t)
    (ht : t ≤ e.createdAt) : declaredAt e = t := by
  simp [declaredAt, aboutTime, h]; omega

omit [DecidableEq A] in
/-- A future event time is clamped to the write time. -/
theorem declaredAt_of_ge {e : Edge A} {t : Int} (h : e.eventTime = some t)
    (ht : e.createdAt ≤ t) : declaredAt e = e.createdAt := by
  simp [declaredAt, aboutTime, h]; omega

omit [DecidableEq A] in
/-- `declared_at` dates a declaration with no event time at its write time. -/
theorem declaredAt_none {e : Edge A} (h : e.eventTime = none) : declaredAt e = e.createdAt := by
  simp [declaredAt, aboutTime, h]

omit [DecidableEq A] in
/-- `declared_at` (`contradiction.rs`) is monotone: a later write time and a
later event time never give an earlier date. -/
theorem declaredAt_mono {e₁ e₂ : Edge A} (hc : e₁.createdAt ≤ e₂.createdAt)
    (ha : aboutTime e₁ ≤ aboutTime e₂) : declaredAt e₁ ≤ declaredAt e₂ := by
  simp only [declaredAt]; omega

omit [DecidableEq A] in
/-- A later write of the same declaration never gets an earlier date. -/
theorem declaredAt_mono_write (e : Edge A) {c₁ c₂ : Int} (h : c₁ ≤ c₂) :
    declaredAt { e with createdAt := c₁ } ≤ declaredAt { e with createdAt := c₂ } := by
  obtain ⟨id, ty, src, c, r, ev⟩ := e
  cases ev <;> simp [declaredAt, aboutTime] <;> omega

omit [DecidableEq A] in
/-- Re-sending reinforces the edge (`last_reinforced`) and does not re-date
or re-attribute it. -/
theorem declaredAt_resend (e : Edge A) (r : Int) :
    declaredAt { e with lastReinforced := r } = declaredAt e ∧
      ∀ tl : List (Interaction A),
        firstWriters tl { e with lastReinforced := r } = firstWriters tl e :=
  ⟨rfl, fun _ => rfl⟩

/-! ### Eligibility is by write time -/

omit [DecidableEq A] in
/-- The event time never enters eligibility. -/
theorem eligible_eventTime (lo now : Int) (minAge : Option Nat) (ev : Evidence A)
    (t : Option Int) :
    eligible lo now minAge { ev with edge := { ev.edge with eventTime := t } } =
      eligible lo now minAge ev := rfl

omit [DecidableEq A] in
/-- Eligible evidence was written at least `min_age` before `now`. -/
theorem eligible_le {lo now : Int} {minAge : Option Nat} {ev : Evidence A}
    (h : eligible lo now minAge ev = true) :
    ∃ a, minAge = some a ∧ ev.edge.createdAt + a ≤ now := by
  unfold eligible cutoff at h
  cases minAge with
  | none => simp at h
  | some a =>
    refine ⟨a, rfl, ?_⟩
    by_cases hl : now - (a : Int) < lo
    · simp [hl] at h
    · simp [hl] at h; omega

omit [DecidableEq A] in
/-- An edge written less than `min_age` ago is not eligible,
whatever its event time says. -/
theorem backdated_not_eligible (lo now : Int) (a : Nat) (ev : Evidence A)
    (h : now < ev.edge.createdAt + a) (t : Option Int) :
    eligible lo now (some a) { ev with edge := { ev.edge with eventTime := t } } = false := by
  have key : ∀ ev' : Evidence A, ev'.edge.createdAt = ev.edge.createdAt →
      eligible lo now (some a) ev' = false := by
    intro ev' hc
    cases he : eligible lo now (some a) ev'
    · rfl
    · obtain ⟨b, hb, hle⟩ := eligible_le he
      simp only [Option.some.injEq] at hb
      subst hb
      omega
  exact key _ rfl

omit [DecidableEq A] in
/-- With `now - min_age` out of chrono's range, or `min_age` not
representable, nothing is eligible. -/
theorem cutoff_none_no_evidence {lo now : Int} {minAge : Option Nat}
    (h : cutoff lo minAge now = none) (ev : Evidence A) : eligible lo now minAge ev = false := by
  simp [eligible, h]

/-- When `now - min_age` falls below `MIN_UTC`, `checked_sub_signed` gives
`contradiction.rs` no age cutoff (the case `cutoff_none_no_evidence` then
reads as no evidence eligible). -/
theorem cutoff_none_of_out_of_range {lo now : Int} {a : Nat} (h : now - a < lo) :
    cutoff lo (some a) now = none := by
  simp [cutoff, h]

/-- So the verdict is the one with no evidence at all. -/
theorem contradictionAt_no_cutoff {lo now : Int} {minAge : Option Nat} (baseline : Int)
    (tl : List (Interaction A)) (evs : List (Evidence A)) (decls : List (Decl A))
    (h : cutoff lo minAge now = none) :
    contradictionAt lo now minAge baseline tl evs decls =
      contradictionAt lo now minAge baseline tl [] decls := by
  have : evs.filter (eligible lo now minAge) = [] := by
    rw [List.filter_eq_nil_iff]
    intro ev _
    simp [cutoff_none_no_evidence h]
  have hadm : admissible lo now minAge baseline tl evs =
      admissible lo now minAge baseline tl [] := by
    funext d
    unfold admissible latest
    rw [this]
    rfl
  simp only [contradictionAt, hadm]

/-- An unrepresentable `min_age` makes no evidence eligible: the verdict is
the one with no evidence at all (test
`an_unrepresentable_min_age_makes_no_evidence_eligible`). -/
theorem unrepresentable_minAge (lo now baseline : Int) (tl : List (Interaction A))
    (evs : List (Evidence A)) (decls : List (Decl A)) :
    contradictionAt lo now none baseline tl evs decls =
      contradictionAt lo now none baseline tl [] decls :=
  contradictionAt_no_cutoff baseline tl evs decls rfl

/-! ### What a hit is -/

theorem latest_ge_baseline (lo now : Int) (minAge : Option Nat) (baseline : Int)
    (tl : List (Interaction A)) (evs : List (Evidence A)) (d : Decl A) :
    baseline ≤ latest lo now minAge baseline tl evs d := by
  unfold latest
  generalize baseline = b
  generalize (List.filter (counts tl d) (List.filter (eligible lo now minAge) evs)) = l
  induction l generalizing b with
  | nil => simp
  | cons x l ih =>
    simp only [List.foldl_cons]
    exact Int.le_trans (Int.le_max_left _ _) (ih _)

theorem latest_ge_mem {lo now : Int} {minAge : Option Nat} {baseline : Int}
    {tl : List (Interaction A)} {evs : List (Evidence A)} {d : Decl A} {ev : Evidence A}
    (hev : ev ∈ evs) (he : eligible lo now minAge ev = true) (hc : counts tl d ev = true) :
    declaredAt ev.edge ≤ latest lo now minAge baseline tl evs d := by
  unfold latest
  have hm : ev ∈ (evs.filter (eligible lo now minAge)).filter (counts tl d) := by
    simp [hev, he, hc]
  generalize baseline = b
  generalize (List.filter (counts tl d) (List.filter (eligible lo now minAge) evs)) = l at hm
  induction l generalizing b with
  | nil => simp at hm
  | cons x l ih =>
    simp only [List.foldl_cons]
    simp only [List.mem_cons] at hm
    rcases hm with rfl | hm
    · have := latest_ge_baseline_aux l (max b (declaredAt ev.edge))
      exact Int.le_trans (Int.le_max_right _ _) this
    · exact ih _ hm
where
  latest_ge_baseline_aux (l : List (Evidence A)) (b : Int) :
      b ≤ l.foldl (fun m ev => max m (declaredAt ev.edge)) b := by
    induction l generalizing b with
    | nil => simp
    | cons x l ih => exact Int.le_trans (Int.le_max_left _ _) (ih _)

omit [DecidableEq A] in
theorem sel_fold_src (adm : Decl A → Bool) (decls : List (Decl A)) (acc : Option (Decl A))
    (d : Decl A) (h : decls.foldl (selStep adm) acc = some d) :
    acc = some d ∨ (d ∈ decls ∧ adm d = true) := by
  induction decls generalizing acc with
  | nil => simp at h; exact Or.inl h
  | cons x xs ih =>
    simp only [List.foldl_cons] at h
    rcases ih _ h with h' | ⟨hm, ha⟩
    · unfold selStep at h'
      split at h'
      · rename_i hx
        split at h'
        · simp at h'; subst h'; exact Or.inr ⟨by simp, hx⟩
        · split at h'
          · simp at h'; subst h'; exact Or.inr ⟨by simp, hx⟩
          · exact Or.inl h'
      · exact Or.inl h'
    · exact Or.inr ⟨by simp [hm], ha⟩

/-- **A hit is sound.** It comes from a declaration of the list whose source
is a concept of the session, written by `now`, dated strictly after every
eligible counting piece of evidence and the baseline, and its date is never
after `now` (the clamp). The writer is the declaration's first writer. -/
theorem contradictionAt_sound {lo now : Int} {minAge : Option Nat} {baseline : Int}
    {tl : List (Interaction A)} {evs : List (Evidence A)} {decls : List (Decl A)} {h : CHit A}
    (hc : contradictionAt lo now minAge baseline tl evs decls = some h) :
    ∃ d ∈ decls, d.byIsConcept = true ∧ d.sameSession = true ∧ d.edge.createdAt ≤ now ∧
      h.byId = d.byId ∧ h.writer = (firstWriters tl d.edge).2 ∧ h.date = declaredAt d.edge ∧
      baseline < h.date ∧ h.date ≤ now ∧
      ∀ ev ∈ evs, eligible lo now minAge ev = true → counts tl d ev = true →
        declaredAt ev.edge < h.date := by
  unfold contradictionAt at hc
  rw [Option.map_eq_some_iff] at hc
  obtain ⟨d, hd, rfl⟩ := hc
  rcases sel_fold_src _ decls none d hd with h' | ⟨hm, ha⟩
  · simp at h'
  · simp only [admissible, writtenBy, Bool.and_eq_true, decide_eq_true_eq] at ha
    obtain ⟨⟨⟨h1, h2⟩, h3⟩, h4⟩ := ha
    have hb := latest_ge_baseline lo now minAge baseline tl evs d
    have hcr := declaredAt_le_createdAt d.edge
    refine ⟨d, hm, h1, h2, h3, rfl, rfl, rfl, by simp; omega, by simp; omega, ?_⟩
    intro ev hev he hcnt
    have := latest_ge_mem (baseline := baseline) hev he hcnt
    simp; omega

/-- PR #154 review: a declaration written after `now` is not admissible,
whatever its event time. -/
theorem not_written_not_admissible (lo now : Int) (minAge : Option Nat) (baseline : Int)
    (tl : List (Interaction A)) (evs : List (Evidence A)) (d : Decl A)
    (h : now < d.edge.createdAt) : admissible lo now minAge baseline tl evs d = false := by
  simp [admissible, writtenBy]
  intros
  omega

/-- Strictly later: a declaration dated at or before what it must beat is not
admissible (`evidence_at_the_same_instant_outweighs_it`). -/
theorem admissible_strict (lo now : Int) (minAge : Option Nat) (baseline : Int)
    (tl : List (Interaction A)) (evs : List (Evidence A)) (d : Decl A)
    (h : declaredAt d.edge ≤ latest lo now minAge baseline tl evs d) :
    admissible lo now minAge baseline tl evs d = false := by
  simp [admissible]
  intros
  omega

/-! ### Self-suppression and evidence monotonicity -/

theorem latest_cons_not_counts {lo now : Int} {minAge : Option Nat} {baseline : Int}
    {tl : List (Interaction A)} {evs : List (Evidence A)} {d : Decl A} {ev : Evidence A}
    (h : counts tl d ev = false) :
    latest lo now minAge baseline tl (ev :: evs) d = latest lo now minAge baseline tl evs d := by
  unfold latest
  simp only [List.filter_cons]
  split <;> simp [h]

/-- Evidence written by one of the declarers, or whose dependent is the
contradicting concept itself, never changes whether that declaration
qualifies (`a_writer_cannot_outweigh_its_own_contradiction`). -/
theorem admissible_selfSuppressed (lo now : Int) (minAge : Option Nat) (baseline : Int)
    (tl : List (Interaction A)) (evs : List (Evidence A)) (d : Decl A) (ev : Evidence A)
    (h : ev.dependent = d.byId ∨
      ∃ w ∈ (firstWriters tl ev.edge).1, w ∈ (firstWriters tl d.edge).1) :
    admissible lo now minAge baseline tl (ev :: evs) d =
      admissible lo now minAge baseline tl evs d := by
  have hc : counts tl d ev = false := by
    unfold counts
    rcases h with h | ⟨w, hw, hd⟩
    · simp [h]
    · simp only [Bool.and_eq_false_iff, bne_eq_false_iff_eq, Bool.not_eq_false',
        List.any_eq_true, List.contains_iff_mem]
      exact Or.inr ⟨w, hw, by simpa using hd⟩
  unfold admissible
  rw [latest_cons_not_counts hc]

omit [DecidableEq A] in
theorem foldl_max_mono (l : List (Evidence A)) {b₁ b₂ : Int} (h : b₁ ≤ b₂) :
    l.foldl (fun m ev => max m (declaredAt ev.edge)) b₁ ≤
      l.foldl (fun m ev => max m (declaredAt ev.edge)) b₂ := by
  induction l generalizing b₁ b₂ with
  | nil => simpa
  | cons x l ih =>
    simp only [List.foldl_cons]
    apply ih
    omega

/-- More evidence never makes a declaration qualify: adding a piece of
evidence can only raise what it must beat
(`later_evidence_outweighs_an_older_contradiction`). -/
theorem latest_le_cons (lo now : Int) (minAge : Option Nat) (baseline : Int)
    (tl : List (Interaction A)) (evs : List (Evidence A)) (d : Decl A) (ev : Evidence A) :
    latest lo now minAge baseline tl evs d ≤ latest lo now minAge baseline tl (ev :: evs) d := by
  unfold latest
  simp only [List.filter_cons]
  split
  · simp only [List.filter_cons]
    split
    · simp only [List.foldl_cons]
      exact foldl_max_mono _ (Int.le_max_left _ _)
    · exact Int.le_refl _
  · exact Int.le_refl _

/-- In `contradiction.rs`, evidence only ever outweighs: a contradiction that
qualifies with one more piece of evidence still qualifies without it. -/
theorem admissible_of_more_evidence {lo now : Int} {minAge : Option Nat} {baseline : Int}
    {tl : List (Interaction A)} {evs : List (Evidence A)} {d : Decl A} (ev : Evidence A)
    (h : admissible lo now minAge baseline tl (ev :: evs) d = true) :
    admissible lo now minAge baseline tl evs d = true := by
  have := latest_le_cons lo now minAge baseline tl evs d ev
  simp only [admissible, Bool.and_eq_true, decide_eq_true_eq] at h ⊢
  exact ⟨h.1, by omega⟩

/-! ### Which contradiction is named -/

theorem better_irrefl (r : Int × Nat × Nat) : better r r = false := by
  simp [better]

theorem better_trans {a b c : Int × Nat × Nat} (h1 : better a b = true) (h2 : better b c = true) :
    better a c = true := by
  simp only [better, Bool.or_eq_true, Bool.and_eq_true, decide_eq_true_eq] at *
  omega

/-- Two ranks neither of which is better are equal. -/
theorem better_total {a b : Int × Nat × Nat} (h1 : better a b = false) (h2 : better b a = false) :
    a = b := by
  obtain ⟨a1, a2, a3⟩ := a
  obtain ⟨b1, b2, b3⟩ := b
  simp only [better, Bool.or_eq_false_iff, Bool.and_eq_false_iff, decide_eq_false_iff_not] at h1 h2
  have e1 : a1 = b1 := by omega
  have e2 : a2 = b2 := by omega
  have e3 : a3 = b3 := by omega
  subst e1 e2 e3; rfl

omit [DecidableEq A] in
theorem sel_fold_max (adm : Decl A → Bool) (decls : List (Decl A)) (acc : Option (Decl A))
    (d : Decl A) (h : decls.foldl (selStep adm) acc = some d) :
    (∀ b, acc = some b → better (rank b) (rank d) = false) ∧
      ∀ x ∈ decls, adm x = true → better (rank x) (rank d) = false := by
  induction decls generalizing acc with
  | nil =>
    simp at h; subst h
    simp [better_irrefl]
  | cons x xs ih =>
    simp only [List.foldl_cons] at h
    have ⟨h1, h2⟩ := ih _ h
    refine ⟨?_, ?_⟩
    · intro b hb
      subst hb
      unfold selStep at h1
      split at h1
      · simp only at h1
        split at h1
        · rename_i hbx
          have := h1 x rfl
          cases hb' : better (rank b) (rank d)
          · rfl
          · exact absurd (better_trans hbx hb') (by simp [this])
        · exact h1 _ rfl
      · exact h1 _ rfl
    · intro y hy hay
      simp only [List.mem_cons] at hy
      rcases hy with rfl | hy
      · unfold selStep at h1
        simp only [hay, ↓reduceIte] at h1
        cases acc with
        | none => exact h1 _ rfl
        | some b =>
          simp only at h1
          split at h1
          · exact h1 _ rfl
          · rename_i hnb
            have hb := h1 b rfl
            cases hyd : better (rank y) (rank d)
            · rfl
            · -- y beats d but not b, and b does not beat d
              simp only [Bool.not_eq_true] at hnb
              cases hby : better (rank b) (rank y)
              · have := better_total hnb hby
                rw [this, hb] at hyd
                exact absurd hyd (by decide)
              · exact absurd (better_trans hby hyd) (by simp [hb])
      · exact h2 y hy hay

/-- **The named contradiction is the best qualifying one**: no qualifying
declaration has a newer date, or the same date and a smaller key, or the same
date and key and a smaller id (module doc, "Several contradictions";
`several_contradictions_resolve_to_the_newest_then_key_then_id`). -/
theorem contradictionAt_max {lo now : Int} {minAge : Option Nat} {baseline : Int}
    {tl : List (Interaction A)} {evs : List (Evidence A)} {decls : List (Decl A)}
    {d : Decl A}
    (hd : decls.foldl (selStep (admissible lo now minAge baseline tl evs)) none = some d) :
    d ∈ decls ∧ admissible lo now minAge baseline tl evs d = true ∧
      ∀ x ∈ decls, admissible lo now minAge baseline tl evs x = true →
        better (rank x) (rank d) = false := by
  rcases sel_fold_src _ decls none d hd with h | ⟨hm, ha⟩
  · simp at h
  · exact ⟨hm, ha, (sel_fold_max _ decls none d hd).2⟩

omit [DecidableEq A] in
theorem selStep_none (adm : Decl A → Bool) (acc : Option (Decl A)) (x : Decl A) :
    selStep adm acc x = none ↔ acc = none ∧ adm x = false := by
  unfold selStep
  cases hx : adm x
  · simp
  · cases acc with
    | none => simp
    | some b => by_cases hb : better (rank x) (rank b) = true <;> simp [hb]

omit [DecidableEq A] in
theorem sel_fold_none (adm : Decl A → Bool) (decls : List (Decl A)) (acc : Option (Decl A)) :
    decls.foldl (selStep adm) acc = none ↔ acc = none ∧ ∀ x ∈ decls, adm x = false := by
  induction decls generalizing acc with
  | nil => simp
  | cons x xs ih =>
    simp only [List.foldl_cons, ih, selStep_none, List.mem_cons, forall_eq_or_imp, and_assoc]

omit [DecidableEq A] in
theorem eq_of_nodup_map {f : Decl A → Nat} :
    ∀ {l : List (Decl A)}, (l.map f).Nodup → ∀ {x y}, x ∈ l → y ∈ l → f x = f y → x = y
  | [], _, _, _, hx, _, _ => by simp at hx
  | z :: zs, hn, x, y, hx, hy, hf => by
    simp only [List.map_cons, List.nodup_cons, List.mem_map, not_exists, not_and] at hn
    simp only [List.mem_cons] at hx hy
    rcases hx with rfl | hx <;> rcases hy with rfl | hy
    · rfl
    · exact absurd hf.symm (hn.1 y hy)
    · exact absurd hf (hn.1 x hx)
    · exact eq_of_nodup_map hn.2 hx hy hf

/-- **The verdict does not depend on edge order.** At most one `Contradicts`
edge runs from each concept to the node (the edge natural key), so the `by`
ids are distinct, and then any order of the declarations gives the same hit. -/
theorem contradictionAt_perm (lo now : Int) (minAge : Option Nat) (baseline : Int)
    (tl : List (Interaction A)) (evs : List (Evidence A)) {decls decls' : List (Decl A)}
    (hp : decls.Perm decls') (hn : (decls.map (·.byId)).Nodup) :
    contradictionAt lo now minAge baseline tl evs decls =
      contradictionAt lo now minAge baseline tl evs decls' := by
  unfold contradictionAt
  congr 1
  generalize hadm : admissible lo now minAge baseline tl evs = adm
  cases h : decls.foldl (selStep adm) none with
  | none =>
    rw [sel_fold_none] at h
    symm
    rw [sel_fold_none]
    exact ⟨rfl, fun x hx => h.2 x (hp.mem_iff.2 hx)⟩
  | some d =>
    cases h' : decls'.foldl (selStep adm) none with
    | none =>
      rw [sel_fold_none] at h'
      rcases sel_fold_src adm decls none d h with h0 | ⟨hm, ha⟩
      · simp at h0
      · rw [h'.2 d (hp.mem_iff.1 hm)] at ha; exact absurd ha (by decide)
    | some d' =>
      rcases sel_fold_src adm decls none d h with h0 | ⟨hm, ha⟩
      · simp at h0
      rcases sel_fold_src adm decls' none d' h' with h0' | ⟨hm', ha'⟩
      · simp at h0'
      have m1 := (sel_fold_max adm decls none d h).2 d' (hp.mem_iff.2 hm') ha'
      have m2 := (sel_fold_max adm decls' none d' h').2 d (hp.mem_iff.1 hm) ha
      have hr := better_total m2 m1
      have hid : d.byId = d'.byId := by
        simp only [rank, Prod.mk.injEq] at hr; exact hr.2.2
      rw [eq_of_nodup_map hn hm (hp.mem_iff.2 hm') hid]

/-! ### Concrete cases (`src/daemon/contradiction/tests.rs`)

Agents: agent-a `1`, agent-b `2`, agent-c `3`, agent-d `4`. `MIN_AGE` is
60 s. The base shape at `now = 1000`: i1 (agent-a, t0) derives X; i2
(agent-b, t100) derives D, which depends on X at t100; i3 (agent-c, t200)
derives Y, which contradicts X at t200. -/

def baseTl : List (Interaction Nat) := [⟨1, 1, 0⟩, ⟨2, 2, 100⟩, ⟨3, 3, 200⟩]

/-- A `Dependency` from a concept with `origin`, as evidence. -/
def cite (id dependent origin : Nat) (t : Int) (ev : Option Int := none) : Evidence Nat :=
  { edge := ⟨id, .dependency, .concept origin, t, t, ev⟩, dependent }

/-- A `Contradicts` from concept `byId` (origin `origin`, key `key`). -/
def contra (id byId key origin : Nat) (t : Int) (ev : Option Int := none) : Decl Nat :=
  { edge := ⟨id, .contradicts, .concept origin, t, t, ev⟩, byId, key }

def baseEv : List (Evidence Nat) := [cite 10 2 2 100]
/-- Y's key: "brand x fits true to size" (2) sorts after "brand x fits large" (1). -/
def baseDecl : Decl Nat := contra 11 3 2 3 200

-- `a_newer_contradiction_of_a_canonical_or_venerable_concept_fires`.
example : contradictionAt lo₀ 1000 (some 60) 0 baseTl baseEv [baseDecl] = some ⟨3, 3, 200⟩ := by
  decide
-- `later_evidence_outweighs_an_older_contradiction`.
example : contradictionAt lo₀ 1000 (some 60) 0 (baseTl ++ [⟨4, 4, 300⟩])
    (baseEv ++ [cite 12 4 4 300]) [baseDecl] = none := by decide

/-- `evidence_at_the_same_instant_outweighs_it`: a citation written at t300
dated t200, equal to the contradiction's date, keeps it quiet (strictly
later). -/
theorem same_instant_evidence_outweighs :
    contradictionAt lo₀ 1000 (some 60) 0 (baseTl ++ [⟨4, 4, 300⟩])
      (baseEv ++ [cite 12 4 4 300 (some 200)]) [baseDecl] = none := by decide

-- `future_dates_are_clamped_to_the_write_time`.
example : contradictionAt lo₀ 1000 (some 60) 0 (baseTl ++ [⟨4, 5, 300⟩]) baseEv
    [baseDecl, contra 12 4 1 4 300 (some 5000)] = some ⟨4, 5, 300⟩ := by decide
example : contradictionAt lo₀ 1000 (some 60) 0 (baseTl ++ [⟨4, 4, 300⟩])
    (baseEv ++ [cite 12 4 4 300 (some 5000)]) [baseDecl] = none := by decide

/-- `a_contradiction_dated_30_days_ahead_does_not_outrank_earlier_writes`:
written at t200, dated 30 days ahead, it is dated t200; a
citation written at t300 dated t500 outweighs it now and in 30 days. -/
theorem thirty_days_ahead_clamped :
    let ahead : Int := 30 * 86400 + 200
    let tl : List (Interaction Nat) := [⟨1, 1, 0⟩, ⟨3, 3, 200⟩, ⟨4, 4, 300⟩]
    let d := contra 11 3 1 3 200 (some ahead)
    contradictionAt lo₀ 1000 (some 60) 0 tl [cite 12 4 4 300 (some 500)] [d] = none ∧
      contradictionAt lo₀ (ahead + 1000) (some 60) 0 tl [cite 12 4 4 300 (some 500)] [d] = none ∧
      contradictionAt lo₀ 1000 (some 60) 0 tl [] [d] = some ⟨3, 3, 200⟩ := by decide

/-- `a_backdated_citation_does_not_skip_the_age_floor`: written at
t990 and dated t300, the citation is not eligible 10 s later; 60 s after its
write it counts, dated t300, and outweighs Y (t200). -/
theorem backdated_citation_waits :
    contradictionAt lo₀ 1000 (some 60) 0 (baseTl ++ [⟨4, 4, 990⟩])
        (baseEv ++ [cite 12 4 4 990 (some 300)]) [baseDecl] = some ⟨3, 3, 200⟩ ∧
      contradictionAt lo₀ 1050 (some 60) 0 (baseTl ++ [⟨4, 4, 990⟩])
        (baseEv ++ [cite 12 4 4 990 (some 300)]) [baseDecl] = none := by decide

-- `evidence_counts_only_once_it_is_age_eligible`: the floor is inclusive.
example : contradictionAt lo₀ 1000 (some 60) 0 (baseTl ++ [⟨4, 4, 970⟩])
    (baseEv ++ [cite 12 4 4 970]) [baseDecl] = some ⟨3, 3, 200⟩ := by decide
example : contradictionAt lo₀ 1030 (some 60) 0 (baseTl ++ [⟨4, 4, 970⟩])
    (baseEv ++ [cite 12 4 4 970]) [baseDecl] = none := by decide
example : contradictionAt lo₀ 1000 (some 0) 0 (baseTl ++ [⟨4, 4, 970⟩])
    (baseEv ++ [cite 12 4 4 970]) [baseDecl] = none := by decide
-- `an_unrepresentable_min_age_makes_no_evidence_eligible`; and a representable
-- `min_age` whose cutoff falls below `MIN_UTC` does the same.
example : contradictionAt lo₀ 1000 none 0 (baseTl ++ [⟨4, 4, 300⟩])
    (baseEv ++ [cite 12 4 4 300]) [baseDecl] = some ⟨3, 3, 200⟩ := by decide
example : contradictionAt lo₀ 1000 (some 20000000000000) 0 (baseTl ++ [⟨4, 4, 300⟩])
    (baseEv ++ [cite 12 4 4 300]) [baseDecl] = some ⟨3, 3, 200⟩ := by decide

-- `a_dependent_from_the_targets_own_origin_is_not_evidence`.
example : contradictionAt lo₀ 1000 (some 60) 0 (baseTl ++ [⟨4, 4, 300⟩])
    (baseEv ++ [{ cite 12 5 1 300 with ownOrigin := true }]) [baseDecl] =
    some ⟨3, 3, 200⟩ := by decide

/-- `a_backdated_contradiction_is_outweighed_by_later_dated_evidence`: the
evidence is dated -5000; a contradiction written last but dated -10000 is
outweighed, one dated -1000 fires, dated by its event time. -/
theorem backdated_contradiction :
    let tl : List (Interaction Nat) := [⟨1, 1, 0⟩, ⟨2, 2, 100⟩, ⟨3, 3, 200⟩]
    let ev := [cite 10 2 2 100 (some (-5000))]
    contradictionAt lo₀ 1000 (some 60) (-30000) tl ev [contra 11 3 1 3 200 (some (-10000))] =
        none ∧
      contradictionAt lo₀ 1000 (some 60) (-30000) tl ev [contra 11 3 1 3 200 (some (-1000))] =
        some ⟨3, 3, -1000⟩ := by decide

/-- `re_sending_a_contradiction_does_not_re_date_or_re_attribute_it`: agent-e
(`5`) re-sends Y's contradiction at t400; the edge keeps its first dates and
its first writer. -/
example : contradictionAt lo₀ 1000 (some 60) 0 (baseTl ++ [⟨5, 5, 400⟩]) baseEv
    [{ baseDecl with edge := { baseDecl.edge with lastReinforced := 400 } }] =
    some ⟨3, 3, 200⟩ := by decide

/-- `a_writer_cannot_outweigh_its_own_contradiction`: agent-c cites X at
t300 from another concept, and Y itself cites X at t300 by agent-d; neither
silences Y's contradiction. -/
theorem own_citation_does_not_silence :
    contradictionAt lo₀ 1000 (some 60) 0 (baseTl ++ [⟨4, 3, 300⟩])
        (baseEv ++ [cite 12 4 3 300]) [baseDecl] = some ⟨3, 3, 200⟩ ∧
      contradictionAt lo₀ 1000 (some 60) 0 (baseTl ++ [⟨4, 4, 300⟩])
        (baseEv ++ [cite 12 3 3 300]) [baseDecl] = some ⟨3, 3, 200⟩ := by decide

-- `several_contradictions_resolve_to_the_newest_then_key_then_id`.
example : contradictionAt lo₀ 1000 (some 60) 0 (baseTl ++ [⟨4, 4, 250⟩]) baseEv
    [baseDecl, contra 12 4 1 4 250] = some ⟨4, 4, 250⟩ := by decide
example : contradictionAt lo₀ 1000 (some 60) 0 (baseTl ++ [⟨4, 4, 250⟩]) baseEv
    [baseDecl, contra 12 4 1 4 250 (some 200)] = some ⟨4, 4, 200⟩ := by decide
example : contradictionAt lo₀ 1000 (some 60) 0 (baseTl ++ [⟨4, 4, 250⟩]) baseEv
    [contra 12 4 1 4 250 (some 200), baseDecl] = some ⟨4, 4, 200⟩ := by decide

-- `the_writer_falls_back_to_the_contradicting_concepts_origin_agent`.
example : contradictionAt lo₀ 1000 (some 60) 0 (baseTl ++ [⟨4, 4, 300⟩]) baseEv
    [baseDecl, contra 12 4 1 4 350] = some ⟨4, 4, 350⟩ := by decide

-- `self_suppression_is_judged_per_contradiction`: agent-c's later citation
-- silences agent-d's contradiction (t250) but not agent-c's own (t200).
example : contradictionAt lo₀ 1000 (some 60) 0 (baseTl ++ [⟨4, 4, 250⟩, ⟨5, 3, 300⟩])
    (baseEv ++ [cite 13 5 3 300]) [baseDecl, contra 12 4 1 4 250] = some ⟨3, 3, 200⟩ := by
  decide

/-- `a_contradiction_written_after_now_does_not_fire_even_if_backdated`
(PR #154 review). -/
theorem written_after_now :
    let tl : List (Interaction Nat) := [⟨1, 1, 0⟩, ⟨3, 3, 2000⟩]
    let d := contra 11 3 1 3 2000 (some 200)
    contradictionAt lo₀ 1000 (some 60) 0 tl [] [d] = none ∧
      contradictionAt lo₀ 3000 (some 60) 0 tl [] [d] = some ⟨3, 3, 200⟩ := by decide

/-- Without evidence the origin's clamped about-time is the baseline
(`without_evidence_the_targets_origin_about_time_is_the_baseline`): a future
origin event time is clamped to the origin's write. -/
example : baselineOf (some (0, some 5000)) 0 = 0 ∧ baselineOf (some (100, some 40)) 100 = 40 ∧
    baselineOf none 7 = 7 := by decide

end Lambo.Conflict
