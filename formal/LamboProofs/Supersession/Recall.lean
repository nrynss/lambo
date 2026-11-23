import LamboProofs.Supersession.Pass

/-
# How recall reads supersession

Models `never_redundant` and `Redundancy::signal` (`src/recall/redundancy.rs`),
`in_suspected_pair`, `possibly_superseded_by` and `superseded_by`
(`src/recall/assemble.rs`), and the line gate of `src/recall/suspected.rs` as
a Boolean, over `Suspected.lean`'s edges and `Pass.lean`'s demotions; and the
concrete cases of all three files.

Results: a suspected pair is never redundant under any signal, and neither
end is skipped (`signal_suspected`, `suspected_ends_not_skipped`); the
"possibly superseded" warning is on the older concept and names the newest
shown newer one (`pss_sound`, `pss_newer`, `pss_newest`); the structured
field does not depend on the line gate (`annotate_field`); the #20 history
line follows the latest demotion and is never on a Canonical concept
(`budget_clears`, `supersededLine_canonical`).

Boundary: the line gate is a given Boolean, and recall's ranking and budget
are `TopK` and `Fit`.
-/

namespace Lambo.Supersession

open Lambo.Canon (Status legal gate)

/-! ## Recall

Recall reads the two mechanisms differently: the #198 edge only adds a
warning (and vetoes the #181 redundancy skip); a #20 supersession is history
in the canonization audit. One session; `same_session_concept` is implicit. -/

/-- `never_redundant`: a `Contradicts` edge either way between two different
concepts, or a `SuspectedSupersedes` edge either way. -/
def neverRedundant (E : List Edge) (a b : Nat) : Bool :=
  (a != b && joined E .contradicts a b) || suspectedPair E a b

/-- `never_redundant` (`src/recall/redundancy.rs`) is symmetric in its two
concepts. -/
theorem neverRedundant_symm (E : List Edge) (a b : Nat) :
    neverRedundant E a b = neverRedundant E b a := by
  unfold neverRedundant
  rw [joined_symm E .contradicts a b, suspectedPair_symm E a b, bne_comm (a := a)]

/-- `RedundancySignal`. -/
inductive Signal where
  | sameContent
  | semanticEdge
  | cosine
  deriving DecidableEq, Repr

/-- `Redundancy::signal`: the first signal that finds `b` redundant with
`a`, cheapest first, unless the graph or the texts rule the pair out.
`sameContent`, `cosineAt` and `distinct` (`distinct_claims`) are given; the
`Semantic` edge is read from the graph. -/
def signal (E : List Edge) (similar : Bool) (a b : Nat) (sameContent cosineAt distinct : Bool) :
    Option Signal :=
  if a = b then none
  else
    let s : Option Signal :=
      if sameContent then some .sameContent
      else if !similar then none
      else if joined E .semantic a b then some .semanticEdge
      else if cosineAt then some .cosine
      else none
    match s with
    | none => none
    | some x => if neverRedundant E a b || distinct then none else some x

/-- **A suspected pair is never redundant**, under any signal, identical
texts included (owner decision 5 on #198). -/
theorem signal_suspected (E : List Edge) (similar : Bool) (a b : Nat)
    (sameContent cosineAt distinct : Bool) (h : suspectedPair E a b = true) :
    signal E similar a b sameContent cosineAt distinct = none := by
  unfold signal
  split
  · rfl
  · dsimp only
    split
    · rfl
    · simp [neverRedundant, h]

/-- `in_suspected_pair`: either end of a `SuspectedSupersedes` edge. -/
def inSuspectedPair (E : List Edge) (n : Nat) : Bool :=
  E.any fun e => e.ty == .suspected && (e.src == n || e.dst == n)

/-- Whether assembly skips a member found redundant: a signal, and no warning
line of its own (`warns`, of which `in_suspected_pair` is one; the others are
given as `other`). -/
def skipped (E : List Edge) (sig : Option Signal) (other : Bool) (n : Nat) : Bool :=
  sig.isSome && !(other || inSuspectedPair E n)

/-- Neither end of a suspected pair is skipped as redundant, whichever hit
it would be redundant with. -/
theorem suspected_ends_not_skipped (E : List Edge) (e : Edge) (he : e ∈ E) (ht : e.ty = .suspected)
    (sig : Option Signal) (other : Bool) :
    skipped E sig other e.src = false ∧ skipped E sig other e.dst = false := by
  have h1 : inSuspectedPair E e.src = true := by
    simp only [inSuspectedPair, List.any_eq_true, Bool.and_eq_true, beq_iff_eq, Bool.or_eq_true]
    exact ⟨e, he, ht, Or.inl rfl⟩
  have h2 : inSuspectedPair E e.dst = true := by
    simp only [inSuspectedPair, List.any_eq_true, Bool.and_eq_true, beq_iff_eq, Bool.or_eq_true]
    exact ⟨e, he, ht, Or.inr rfl⟩
  simp [skipped, h1, h2]

/-- The newest of a list by `date`, then the smallest id (`max_by` with
`dated_at`, then `b.id.cmp(&a.id)`). -/
def newestOf (date : Nat → Int) : List Nat → Option Nat
  | [] => none
  | x :: xs =>
    some (xs.foldl (fun cur y => if date cur < date y ∨ (date cur = date y ∧ y < cur) then y else cur) x)

theorem newestOf_mem (date : Nat → Int) : ∀ (l : List Nat) (b : Nat), newestOf date l = some b → b ∈ l
  | [], _, h => by cases h
  | x :: xs, b, h => by
    simp only [newestOf, Option.some.injEq] at h
    subst h
    suffices ∀ (cur : Nat), cur ∈ x :: xs → ∀ ys : List Nat, (∀ y ∈ ys, y ∈ x :: xs) →
        ys.foldl (fun cur y => if date cur < date y ∨ (date cur = date y ∧ y < cur) then y else cur)
          cur ∈ x :: xs from this x (List.mem_cons_self ..) xs (fun y hy => List.mem_cons_of_mem _ hy)
    intro cur hc ys hys
    induction ys generalizing cur with
    | nil => exact hc
    | cons y ys ih =>
      simp only [List.foldl_cons]
      apply ih
      · split
        · exact hys y (List.mem_cons_self ..)
        · exact hc
      · exact fun z hz => hys z (List.mem_cons_of_mem _ hz)

theorem newestOf_max (date : Nat → Int) : ∀ (l : List Nat) (b : Nat), newestOf date l = some b →
    ∀ y ∈ l, date y ≤ date b
  | [], _, h => by cases h
  | x :: xs, b, h => by
    simp only [newestOf, Option.some.injEq] at h
    subst h
    suffices ∀ (cur : Nat) (seen : List Nat), (∀ y ∈ seen, date y ≤ date cur) →
        ∀ ys : List Nat, ∀ y ∈ seen ++ ys, date y ≤ date (ys.foldl
          (fun cur y => if date cur < date y ∨ (date cur = date y ∧ y < cur) then y else cur) cur) by
      intro y hy
      exact this x [x] (by simp) xs y (by simpa using hy)
    intro cur seen hseen ys
    induction ys generalizing cur seen with
    | nil => simpa using hseen
    | cons z zs ih =>
      intro y hy
      simp only [List.foldl_cons]
      apply ih _ (seen ++ [z]) _ y (by simpa using hy)
      intro w hw
      simp only [List.mem_append, List.mem_singleton] at hw
      split
      · rename_i hz
        rcases hw with hw | hw
        · have := hseen w hw; omega
        · subst hw; exact Int.le_refl _
      · rename_i hz
        rcases hw with hw | hw
        · exact hseen w hw
        · subst hw; omega

/-- `possibly_superseded_by`: the shown concept suspected of replacing `of`,
the newest by `dated_at`, then the smallest id. -/
def possiblySupersededBy (E : List Edge) (date : Nat → Int) (shown : List Nat) (of : Nat) :
    Option Nat :=
  newestOf date ((E.filter fun e => e.ty == .suspected && e.dst == of && decide (e.src ∈ shown)).map
    (·.src))

/-- The concept a line names is shown, and its edge points at `of`. -/
theorem pss_sound (E : List Edge) (date : Nat → Int) (shown : List Nat) (of b : Nat)
    (h : possiblySupersededBy E date shown of = some b) :
    b ∈ shown ∧ ∃ e ∈ E, e.ty = .suspected ∧ e.src = b ∧ e.dst = of := by
  have hm := newestOf_mem date _ b h
  simp only [List.mem_map, List.mem_filter, Bool.and_eq_true, beq_iff_eq,
    decide_eq_true_eq] at hm
  obtain ⟨e, ⟨he, ⟨ht, hd⟩, hs⟩, rfl⟩ := hm
  exact ⟨hs, e, he, ht, rfl, hd⟩

/-- **The warning is on the older concept and names a newer one.** -/
theorem pss_newer (E : List Edge) (date : Nat → Int) (hE : WellDated date E) (shown : List Nat)
    (of b : Nat) (h : possiblySupersededBy E date shown of = some b) : date of < date b := by
  obtain ⟨_, e, he, ht, hs, hd⟩ := pss_sound E date shown of b h
  have := hE e he ht
  rw [hs, hd] at this; exact this

/-- The newer end of a pair never carries a line naming the older end. -/
theorem pss_not_on_newer (E : List Edge) (date : Nat → Int) (hE : WellDated date E)
    (shown : List Nat) (e : Edge) (he : e ∈ E) (ht : e.ty = .suspected) :
    possiblySupersededBy E date shown e.src ≠ some e.dst := by
  intro h
  have := pss_newer E date hE shown e.src e.dst h
  have := hE e he ht
  omega

/-- The line names the newest shown suspected replacement. -/
theorem pss_newest (E : List Edge) (date : Nat → Int) (shown : List Nat) (of b : Nat)
    (h : possiblySupersededBy E date shown of = some b) (e : Edge) (he : e ∈ E)
    (ht : e.ty = .suspected) (hd : e.dst = of) (hs : e.src ∈ shown) : date e.src ≤ date b := by
  apply newestOf_max date _ b h
  simp only [List.mem_map, List.mem_filter, Bool.and_eq_true, beq_iff_eq, decide_eq_true_eq]
  exact ⟨e, ⟨he, ⟨ht, hd⟩, hs⟩, rfl⟩

/-- The hit's structured field and its prose line: the field is set whenever
the replacement is shown, the line only where `suspected_supersession_line`
allows it (`shows_line`). -/
def annotate (showsLine : Bool) (pss : Option Nat) : Option Nat × Option Nat :=
  (pss, if showsLine then pss else none)

/-- Recall's annotation passes `possibly_superseded_by` through unchanged and
renders the prose line only where `suspected_supersession_line` allows it. -/
theorem annotate_field (showsLine : Bool) (pss : Option Nat) :
    (annotate showsLine pss).1 = pss ∧ ((annotate showsLine pss).2.isSome → showsLine = true) := by
  cases showsLine <;> simp [annotate]

/-- `DemotionReason`. -/
inductive Reason where
  | superseded (by_ : Nat)
  | budget
  | unknown
  deriving DecidableEq, Repr

/-- One audit row: the concept and its demotion reason (`none` for a
promotion). -/
structure AuditRow where
  node : Nat
  reason : Option Reason
  deriving DecidableEq, Repr

/-- `superseded_by`: per concept, whether its latest demotion in the audit
was a supersession, and by whom. A later budget or unreadable demotion
replaces it; a promotion leaves it. -/
def supersededBy (rows : List AuditRow) (n : Nat) : Option Nat :=
  rows.foldl
    (fun acc r =>
      if r.node = n then
        match r.reason with
        | some (.superseded b) => some b
        | some _ => none
        | none => acc
      else acc)
    none

/-- The #20 history line: only on a concept that is not Canonical now. -/
def supersededLine (rows : List AuditRow) (st : Status) (n : Nat) : Option Nat :=
  if st = .canonical then none else supersededBy rows n

theorem supersededBy_append (rows : List AuditRow) (r : AuditRow) (n : Nat) :
    supersededBy (rows ++ [r]) n =
      if r.node = n then
        match r.reason with
        | some (.superseded b) => some b
        | some _ => none
        | none => supersededBy rows n
      else supersededBy rows n := by
  simp [supersededBy, List.foldl_append]

/-- A later budget demotion replaces the supersession line. -/
theorem budget_clears (rows : List AuditRow) (n : Nat) :
    supersededBy (rows ++ [⟨n, some .budget⟩]) n = none := by
  rw [supersededBy_append]; simp

/-- A Canonical concept never carries the line, whatever its history. -/
theorem supersededLine_canonical (rows : List AuditRow) (n : Nat) :
    supersededLine rows .canonical n = none := rfl

/-! ## Examples -/

section Examples

/-- A concept of the session, `Logic`, with no scope. -/
def mk (id : Nat) (dated : Int) (idents : Nat := id) : Concept :=
  ⟨id, 0, .logic, none, idents, id, dated, false⟩

def anySlot : Nat → Nat → Bool := fun _ _ => true

/-- Dating (test `direction_and_ties`): a future event time is clamped to the
write time; a backfilled one is kept. -/
example : datedAt 0 (some 500) = 0 := by decide
example : datedAt 10 (some (-30)) = -30 := by decide
example : datedAt 10 none = 10 := by decide

/-- Below the merge threshold, or merged by the identifier guard, nothing
reaches the trigger. -/
example : refusedHits 8500 (fun i => i != 3) [(1, 8499), (2, 8500), (3, 9900)] = [2] := by decide

/-- Direction, tie and self (test `direction_and_ties`). -/
example : candidate anySlot [] (mk 1 0) (mk 2 10) = some ⟨2, 1⟩ := by decide
example : candidate anySlot [] (mk 2 10) (mk 1 0) = some ⟨2, 1⟩ := by decide
example : candidate anySlot [] (mk 1 (-30)) (mk 2 0) = some ⟨2, 1⟩ := by decide
example : candidate anySlot [] (mk 1 5) (mk 2 5) = none := by decide
example : candidate anySlot [] (mk 1 5) (mk 1 5) = none := by decide
/-- Another type, an ineligible type, the identifier guard, a settled pair. -/
example : candidate anySlot [] (mk 1 0) { mk 2 10 with ty := .constraint } = none := by decide
example : candidate anySlot [] { mk 1 0 with ty := .entity } { mk 2 10 with ty := .entity } = none := by
  decide
example : candidate anySlot [] (mk 1 0 7) (mk 2 10 7) = none := by decide
example : candidate anySlot [⟨9, 1, 2, .dependency, 0⟩] (mk 1 0) (mk 2 10) = none := by decide
/-- The slot rule reads (older, newer). -/
example : candidate (fun o n => o == 1 && n == 2) [] (mk 2 10) (mk 1 0) = some ⟨2, 1⟩ := by decide
example : candidate (fun o n => o == 2 && n == 1) [] (mk 2 10) (mk 1 0) = none := by decide

/-- The trigger: a backfilled write points the edge at itself. -/
example :
    recordRefused anySlot (fun i => if i = 1 then some (mk 1 0) else none) (mk 2 (-30))
      (fun k => 100 + k) 5 [] [1, 1] = [⟨100, 1, 2, .suspected, 5⟩] := by decide
example :
    recordRefused anySlot (fun i => if i = 1 then some (mk 1 0) else none) (mk 2 10)
      (fun k => 100 + k) 5 [⟨7, 2, 1, .suspected, 3⟩] [1] = [⟨7, 2, 1, .suspected, 3⟩] := by decide
/-- `settle` lapses the suspicion and keeps the rest. -/
example : settle [⟨7, 2, 1, .suspected, 3⟩, ⟨8, 2, 1, .contradicts, 4⟩] 1 2 =
    [⟨8, 2, 1, .contradicts, 4⟩] := by decide

def lookOld (dated : Int) (i : Nat) : Option Concept :=
  if i = 1 then some (mk 1 dated) else if i ≤ 5 then some (mk i 0) else none

/-- `newer`, both ways, and a tie (test
`a_later_dated_candidate_is_marked_newer_both_ways_and_a_tie_is_not`). -/
example : (select [] (lookOld 0) (mk 0 10) [⟨1, 9000, true⟩] 8500 [] []).map (·.newer) = [false] := by
  decide
example : (select [] (lookOld 0) (mk 0 (-30)) [⟨1, 9000, true⟩] 8500 [] []).map (·.newer) = [true] := by
  decide
example : (select [] (lookOld 0) (mk 0 0) [⟨1, 9000, true⟩] 8500 [] []).map (·.newer) = [false] := by
  decide
/-- The `newer` candidate above is the replacement, not the replaced: the
trigger points the edge from it to the new concept. -/
example : candidate anySlot [] (mk 0 (-30)) (mk 1 0) = some ⟨1, 0⟩ := by decide
/-- The floor, the clamp and non-finite scores (test `the_floor`). -/
example : (select [] (lookOld 0) (mk 0 10) [⟨1, 8499, true⟩] 8500 [] []).length = 0 := by decide
example : (select [] (lookOld 0) (mk 0 10) [⟨1, 15000, true⟩] 8500 [] []).map (·.cosine) = [10000] := by
  decide
example : (select [] (lookOld 0) (mk 0 10) [⟨1, 9000, false⟩] 8500 [] []).length = 0 := by decide
/-- Ranked by cosine then id, capped, a repeated hit once (test
`ordering_cap_and_dedup`). -/
example : (select [] (lookOld 0) (mk 0 10)
    [⟨4, 8800, true⟩, ⟨2, 9100, true⟩, ⟨5, 8600, true⟩, ⟨3, 9100, true⟩, ⟨1, 8700, true⟩,
      ⟨2, 9100, true⟩] 8500 [] []).map (fun c => (c.id, c.cosine)) =
    [(2, 9100), (3, 9100), (4, 8800)] := by decide
/-- The previous version: a repeated id keeps its first score, not its best,
so the list's cosine depends on the order the store returns duplicates in. -/
example : (select [] (lookOld 0) (mk 0 10) [⟨1, 8600, true⟩, ⟨1, 9500, true⟩] 8500 [] []).map
    (·.cosine) = [8600] := by decide

/-- After the fix (#302): a repeated id is listed at its best score in either
order, and it can then outrank a hit it trailed before (test
`a_duplicate_id_keeps_its_highest_cosine`). -/
example : (selectF [] (lookOld 0) (mk 0 10) [⟨1, 8600, true⟩, ⟨1, 9500, true⟩] 8500 [] []).map
    (·.cosine) = [9500] := by decide
example : (selectF [] (lookOld 0) (mk 0 10) [⟨1, 9500, true⟩, ⟨1, 8600, true⟩] 8500 [] []).map
    (·.cosine) = [9500] := by decide
example : (selectF [] (lookOld 0) (mk 0 10)
    [⟨1, 8600, true⟩, ⟨2, 9000, true⟩, ⟨3, 8900, true⟩, ⟨4, 8800, true⟩, ⟨1, 9500, true⟩]
      8500 [] []).map (fun c => (c.id, c.cosine)) = [(1, 9500), (2, 9000), (3, 8900)] := by decide
example : (select [] (lookOld 0) (mk 0 10)
    [⟨1, 8600, true⟩, ⟨2, 9000, true⟩, ⟨3, 8900, true⟩, ⟨4, 8800, true⟩, ⟨1, 9500, true⟩]
      8500 [] []).map (fun c => (c.id, c.cosine)) = [(2, 9000), (3, 8900), (4, 8800)] := by decide
/-- Without a repeated id with different scores nothing changes (test
`ordering_cap_and_dedup`). -/
example : selectF [] (lookOld 0) (mk 0 10)
    [⟨4, 8800, true⟩, ⟨2, 9100, true⟩, ⟨5, 8600, true⟩, ⟨3, 9100, true⟩, ⟨1, 8700, true⟩,
      ⟨2, 9100, true⟩] 8500 [] [] =
    select [] (lookOld 0) (mk 0 10)
    [⟨4, 8800, true⟩, ⟨2, 9100, true⟩, ⟨5, 8600, true⟩, ⟨3, 9100, true⟩, ⟨1, 8700, true⟩,
      ⟨2, 9100, true⟩] 8500 [] [] := by decide

/-- Two concepts' evidence. Concept 1 (C) is supported at 25, 55, 85 and
concept 2 (S) at 20, 50, 80, each from three origins; session extent 0..100;
coverage window 1000 (so a span of 30 covers 0.3). -/
def evC : Evidence :=
  ⟨[⟨11, 25, 101, some 25, true⟩, ⟨12, 55, 102, some 55, true⟩, ⟨13, 85, 103, some 85, true⟩],
    some (0, 100)⟩
def evS : Evidence :=
  ⟨[⟨21, 20, 201, some 20, true⟩, ⟨22, 50, 202, some 50, true⟩, ⟨23, 80, 203, some 80, true⟩],
    some (0, 100)⟩
def ev2 (i : Nat) : Evidence := if i = 1 then evC else if i = 2 then evS else ⟨[], none⟩

/-- One-way: S contradicts C, C's watermark is 85, S's support is older: no
verdict (test `older_support_does_not_count`). -/
example : decideV ev2 1000 [⟨1, 2, 7, 30, 0, [], false⟩] = [] := by decide
/-- One-way: C's own support is excluded (self-suppression), so the watermark
is the baseline 0 and S's support is newer: C is demoted. -/
example : decideV ev2 1000 [⟨1, 2, 7, 30, 0, [11, 12, 13], false⟩] = [⟨1, 2, 30, 0⟩] := by decide
/-- Reciprocal: each qualifies against the other; C's newest counted evidence
(85) beats S's (80), so only C acts: S is demoted (test
`reciprocal_contradictions_flip_at_most_once_per_cooldown`). -/
example : decideV ev2 1000
    [⟨1, 2, 7, 30, 0, [11, 12, 13], false⟩, ⟨2, 1, 8, 20, 0, [21, 22, 23], false⟩] =
    [⟨2, 1, 20, 0⟩] := by decide
/-- A reverse probe only blocks; it never becomes a verdict. -/
example : decideV ev2 1000
    [⟨1, 2, 7, 30, 0, [11, 12, 13], false⟩, ⟨2, 1, 8, 20, 0, [21, 22, 23], true⟩] = [] := by decide
/-- Two sources, tied on the newest counted evidence: the smaller key wins
(test `the_source_with_the_newest_evidence_wins`). -/
example : decideV (fun i => if i = 1 then ⟨[], none⟩ else evS) 1000
    [⟨1, 2, 9, 30, 0, [], false⟩, ⟨1, 3, 5, 31, 0, [], false⟩] = [⟨1, 3, 31, 0⟩] := by decide
/-- Evidence at the watermark instant does not count. -/
example : decideV ev2 1000 [⟨1, 2, 7, 30, 20, [11, 12, 13], false⟩] = [] := by decide

/-- A world: C (1) and S (2) both Canonical, S contradicts C; a suspected
edge 2 → 1 and a self-contradiction of 1. -/
def W0 : World :=
  ⟨0, [⟨1, 0, .canonical, none, 30, 0⟩, ⟨2, 0, .canonical, none, 20, 0⟩],
    [⟨7, 2, 1, .contradicts, 5⟩, ⟨9, 2, 1, .suspected, 5⟩, ⟨8, 1, 1, .contradicts, 5⟩],
    fun _ => some []⟩

example : gather W0 true 60 100 = [⟨1, 2, 20, 7, 0, [], false⟩] := by decide
example : gather W0 false 60 100 = [] := by decide
/-- A source inside its cooldown cannot supersede (test
`a_source_inside_its_cooldown_cannot_supersede`). -/
example : gather { W0 with nodes := [⟨1, 0, .canonical, none, 30, 0⟩, ⟨2, 0, .canonical, some 90, 20, 0⟩] }
    true 60 100 = [] := by decide
/-- An ambiguous attribution is not probed. -/
example : gather { W0 with suppressed := fun _ => none } true 60 100 = [] := by decide
/-- A suspected edge alone probes nothing. -/
example : gather { W0 with edges := [⟨9, 2, 1, .suspected, 5⟩] } true 60 100 = [] := by decide

/-- Apply: a Canonical target goes to None; a target no longer Canonical is
left alone; the source is untouched. -/
def st0 (n : Nat) : Status := if n = 1 then .canonical else if n = 2 then .venerable else .none
example : (applySup (fun _ => true) st0 [⟨1, 2, 7, 0⟩]) 1 = .none := by decide
example : (applySup (fun _ => true) st0 [⟨1, 2, 7, 0⟩]) 2 = .venerable := by decide
example : (applySup (fun _ => true) st0 [⟨2, 1, 7, 0⟩]) 2 = .venerable := by decide
example : (applySup (fun _ => false) st0 [⟨1, 2, 7, 0⟩]) 1 = .canonical := by decide

/-- Recall: a suspected pair with identical text is never redundant; the line
names the newest shown replacement; a later budget demotion clears #20's
line. -/
example : signal [⟨9, 2, 1, .suspected, 5⟩] true 1 2 true true false = none := by decide
example : signal [] true 1 2 true true false = some .sameContent := by decide
example : possiblySupersededBy [⟨9, 2, 1, .suspected, 5⟩, ⟨10, 3, 1, .suspected, 5⟩]
    (fun i => (i : Int)) [1, 2, 3] 1 = some 3 := by decide
example : possiblySupersededBy [⟨9, 2, 1, .suspected, 5⟩, ⟨10, 3, 1, .suspected, 5⟩]
    (fun i => (i : Int)) [1, 2] 1 = some 2 := by decide
example : possiblySupersededBy [⟨9, 2, 1, .suspected, 5⟩] (fun i => (i : Int)) [1] 1 = none := by
  decide
example : supersededLine [⟨1, some (.superseded 2)⟩, ⟨1, none⟩] .none 1 = some 2 := by decide
example : supersededLine [⟨1, some (.superseded 2)⟩, ⟨1, some .budget⟩] .none 1 = none := by decide

end Examples

end Lambo.Supersession
