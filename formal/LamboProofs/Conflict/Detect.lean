import LamboProofs.Lib.List

/-
# Conflict detection (`src/daemon/conflict.rs`)

Models `conflict_at_with` (`conflictAt`, and since #297 `conflictAtFixed`):
the write-time attribution of an edge to its agents (`WriterTimeline::acting`,
`edge_writers`, `attribute_at`), the recency window (`chrono_window`,
`window_start`, `CONFLICT_RECENCY_WINDOW`) and the hit's payload; and
`HotList::revalidate`'s re-run of the check at the caller's `now`. Contradiction
dating, which shares the attribution, is `Contradiction.lean`.

Results: a conflict fires iff two distinct agents are resolved writers of
non-excluded edges incident to the node and one `Causal`/`Dependency` write is
in the window (`conflictAt_isSome_iff`), the window is inclusive at both ends
(`qualifies_iff`), the verdict, agents and age do not depend on edge order
(`conflictAt_perm_*`), `Contradicts`/`SuspectedSupersedes` edges change
nothing (`excluded_edge_irrelevant`), and a conflict lapses once its newest
write leaves the window (`lapse_of_old`, `lapse_permanent`,
`secondsAgo_advances`). Before #297 a re-send by another agent dropped the
original writer (`resend_drops_original_writer`); the fixed rule keeps the
creator and adds the re-sender (`resend_keeps_creator`,
`resend_adds_resender`) and never loses a conflict the old rule found
(`conflictAtFixed_of_conflictAt`); under either rule the named writer is one
of the contesting agents (`writer_mem_agents`, `writer_mem_agents_fixed`).
Pinned as they are: only one agent need
write inside the window (`old_edge_agent_counts`), an edge written after `now`
still adds its agent (`future_edge_agent_counts`), ties between edges go to
the smallest edge id (`writer_order_dependent`), and a writer between an
edge's first and latest write is not recoverable
(`second_resend_drops_first_resender`).

Boundary: time is whole seconds and chrono's range enters only as its lower
end `lo`; a `Duration` chrono cannot represent saturates the window to all of
time. The per-node check takes `incident_edges` (id-ascending) as input and
assumes every edge's source is in the graph (`assert_invariants`). Not
modelled: `detect`'s walk and sort, `insert_conflicts` (hot-list insertion is
`Hotlist`), the daemon's emit-on-transition arming, `SharedTimeline` caching,
`stale_citations`, and concurrency (each check is one read of the graph).
-/

namespace Lambo.Conflict

open Lambo.Lib (two_le_length_iff)

/-! ## Edges, sources, the timeline -/

/-- `EdgeType` (`src/types/mod.rs`), all nine variants. -/
inductive EdgeType where
  | temporal | derives | coOccurrence | causal | dependency | hierarchical | semantic
  | contradicts | suspectedSupersedes
  deriving DecidableEq, Repr

namespace EdgeType

/-- `edge_writers` returns `None` for these: their writer never joins the
conflict agent set. -/
def excluded : EdgeType → Bool
  | contradicts | suspectedSupersedes => true
  | _ => false

/-- The types whose write activity can make a conflict. -/
def qualifying : EdgeType → Bool
  | causal | dependency => true
  | _ => false

theorem excluded_false_of_qualifying {t : EdgeType} (h : t.qualifying = true) :
    t.excluded = false := by
  cases t <;> simp_all [qualifying, excluded]

end EdgeType

/-- What attribution sees of an edge's source node. -/
inductive Source (A : Type) where
  /-- An `Interaction`: the edge is that interaction's agent's. -/
  | interaction (agent : A)
  /-- A `Concept` with this `origin_agent`. -/
  | concept (origin : A)
  deriving DecidableEq, Repr

structure Edge (A : Type) where
  id : Nat
  ty : EdgeType
  source : Source A
  createdAt : Int
  lastReinforced : Int
  eventTime : Option Int := none
  deriving DecidableEq, Repr

/-- One entry of `WriterTimeline::by_time`: an interaction, its agent, its
`created_at`. -/
structure Interaction (A : Type) where
  id : Nat
  agent : A
  createdAt : Int
  deriving DecidableEq, Repr

/-- Spec §9 `conflict_recency_window`, `CONFLICT_RECENCY_WINDOW` (30 s).
Pinned by `formal_constants_tests::conflict`. -/
def recencyWindow : Option Nat := some 30

variable {A : Type} [DecidableEq A]

/-- An edge's latest write, `edge.last_reinforced.max(edge.created_at)`. -/
def lastWrite (e : Edge A) : Int := max e.lastReinforced e.createdAt

/-- `WriterTimeline::acting`: every interaction written at exactly `t`. -/
def acting (tl : List (Interaction A)) (t : Int) : List (Interaction A) :=
  tl.filter (fun i => i.createdAt == t)

/-- The smallest-id interaction of a non-empty list (the first among equal
ids; interaction ids are unique). The Rust sorts by id and takes `[0]`. -/
def minIdOf (i : Interaction A) (is : List (Interaction A)) : Interaction A :=
  is.foldl (fun b j => if j.id < b.id then j else b) i

/-- `attribute_at`: every candidate writer (`EdgeWriters::all`) and the named
one (`EdgeWriters::named`) of an edge with this source, resolved at the
write instant `t`. -/
def attributeAt (tl : List (Interaction A)) (src : Source A) (t : Int) : List A × A :=
  match src with
  | .interaction a => ([a], a)
  | .concept o =>
    match acting tl t with
    | [] => ([o], o)
    | i :: is => ((i :: is).map (·.agent), (minIdOf i is).agent)

omit [DecidableEq A] in
theorem minIdOf_mem (i : Interaction A) (is : List (Interaction A)) :
    minIdOf i is ∈ i :: is := by
  unfold minIdOf
  induction is generalizing i with
  | nil => simp
  | cons j js ih =>
    simp only [List.foldl_cons]
    split
    · have := ih j
      simp only [List.mem_cons] at this ⊢
      rcases this with h | h <;> simp [h]
    · have := ih i
      simp only [List.mem_cons] at this ⊢
      rcases this with h | h <;> simp [h]

omit [DecidableEq A] in
/-- The named writer is always one of the candidates. -/
theorem attributeAt_named_mem (tl : List (Interaction A)) (src : Source A) (t : Int) :
    (attributeAt tl src t).2 ∈ (attributeAt tl src t).1 := by
  unfold attributeAt
  cases src with
  | interaction a => simp
  | concept o =>
    simp only
    split
    · simp
    · rename_i i is _
      exact List.mem_map_of_mem (minIdOf_mem i is)

/-! ## Conflict detection (`conflict_at_with`) -/

/-- `window_start`: `now - window`, or `MIN_UTC = lo` when the window is not
representable or the subtraction underflows. -/
def windowStart (lo : Int) (w : Option Nat) (now : Int) : Int :=
  match w with
  | none => lo
  | some w => max lo (now - w)

/-- The `qualifying` test of `conflict_at_with`. -/
def qualifies (lo : Int) (w : Option Nat) (now : Int) (e : Edge A) : Bool :=
  e.ty.qualifying && decide (lastWrite e ≤ now) && decide (windowStart lo w now ≤ lastWrite e)

/-- `edge_writers` for a non-excluded edge: resolved at the latest write. -/
def writersOf (tl : List (Interaction A)) (e : Edge A) : List A × A :=
  attributeAt tl e.source (lastWrite e)

/-- `if !agents.contains(agent) { agents.push(agent.clone()) }`. -/
def pushNew (as : List A) (a : A) : List A := if a ∈ as then as else as ++ [a]

def agentsStep (tl : List (Interaction A)) (as : List A) (e : Edge A) : List A :=
  if e.ty.excluded then as else (writersOf tl e).1.foldl pushNew as

/-- The agent set of `conflict_at_with`, before its sort. -/
def agentsOf (tl : List (Interaction A)) (es : List (Edge A)) : List A :=
  es.foldl (agentsStep tl) []

/-- `newest.as_ref().is_none_or(|(t, _)| last_write > *t)`. -/
def beats (acc : Option (Int × A)) (t : Int) : Bool :=
  match acc with
  | none => true
  | some (s, _) => decide (s < t)

def newestStep (lo : Int) (w : Option Nat) (now : Int) (tl : List (Interaction A))
    (acc : Option (Int × A)) (e : Edge A) : Option (Int × A) :=
  if e.ty.excluded then acc
  else if qualifies lo w now e && beats acc (lastWrite e) then
    some (lastWrite e, (writersOf tl e).2)
  else acc

/-- The newest qualifying write and the agent that made it: a strictly
newer write replaces the current one, so among equal times the first edge in
`incident_edges` order (smallest edge id) wins. -/
def newestOf (lo : Int) (w : Option Nat) (now : Int) (tl : List (Interaction A))
    (es : List (Edge A)) : Option (Int × A) :=
  es.foldl (newestStep lo w now tl) none

/-- `ConflictHit` without its node; also the `HotListPayload::Conflict` and
the `DaemonEvent::Conflict` detail fields. -/
structure Hit (A : Type) where
  agents : List A
  writer : A
  secondsAgo : Nat
  deriving DecidableEq, Repr

section Sorted

variable [Ord A]

/-- Insert `x` before the first element it does not compare greater than. -/
def insertAgent (x : A) : List A → List A
  | [] => [x]
  | y :: ys => if compare x y = .gt then y :: insertAgent x ys else x :: y :: ys

/-- `agents.sort_by(|a, b| a.as_str().cmp(b.as_str()))`, as an insertion
sort. -/
def sortAgents : List A → List A
  | [] => []
  | x :: xs => insertAgent x (sortAgents xs)

omit [DecidableEq A] in
theorem insertAgent_perm (x : A) : ∀ l : List A, (insertAgent x l).Perm (x :: l)
  | [] => List.Perm.refl _
  | y :: ys => by
    unfold insertAgent
    split
    · exact ((insertAgent_perm x ys).cons y).trans (List.Perm.swap x y ys)
    · exact List.Perm.refl _

omit [DecidableEq A] in
theorem sortAgents_perm : ∀ l : List A, (sortAgents l).Perm l
  | [] => List.Perm.refl _
  | x :: xs => (insertAgent_perm x _).trans ((sortAgents_perm xs).cons x)

/-- `conflict_at_with` on one node's incident edges. -/
def conflictAt (lo : Int) (w : Option Nat) (now : Int) (tl : List (Interaction A))
    (es : List (Edge A)) : Option (Hit A) :=
  let agents := agentsOf tl es
  if agents.length < 2 then none
  else
    match newestOf lo w now tl es with
    | none => none
    | some (t, wr) => some ⟨sortAgents agents, wr, (now - t).toNat⟩

/-! ### The agent set -/

omit [Ord A] in
theorem mem_pushNew (as : List A) (a x : A) : x ∈ pushNew as a ↔ x ∈ as ∨ x = a := by
  unfold pushNew
  split
  · constructor
    · exact Or.inl
    · rintro (h | rfl) <;> assumption
  · simp

omit [Ord A] in
theorem nodup_pushNew {as : List A} (h : as.Nodup) (a : A) : (pushNew as a).Nodup := by
  unfold pushNew
  split
  · exact h
  · rename_i ha
    rw [List.nodup_append]
    refine ⟨h, by simp, ?_⟩
    intro x hx y hy
    simp at hy
    subst hy
    intro hxy
    subst hxy
    exact ha hx

omit [Ord A] in
theorem mem_foldl_pushNew (l as : List A) (x : A) :
    x ∈ l.foldl pushNew as ↔ x ∈ as ∨ x ∈ l := by
  induction l generalizing as with
  | nil => simp
  | cons a l ih =>
    simp only [List.foldl_cons, ih, mem_pushNew, List.mem_cons]
    constructor
    · rintro ((h | h) | h)
      · exact Or.inl h
      · exact Or.inr (Or.inl h)
      · exact Or.inr (Or.inr h)
    · rintro (h | h | h)
      · exact Or.inl (Or.inl h)
      · exact Or.inl (Or.inr h)
      · exact Or.inr h

omit [Ord A] in
theorem nodup_foldl_pushNew (l : List A) {as : List A} (h : as.Nodup) :
    (l.foldl pushNew as).Nodup := by
  induction l generalizing as with
  | nil => exact h
  | cons a l ih => exact ih (nodup_pushNew h a)

omit [Ord A] in
theorem mem_foldl_agentsStep (tl : List (Interaction A)) (es : List (Edge A)) (as : List A)
    (x : A) :
    x ∈ es.foldl (agentsStep tl) as ↔
      x ∈ as ∨ ∃ e ∈ es, e.ty.excluded = false ∧ x ∈ (writersOf tl e).1 := by
  induction es generalizing as with
  | nil => simp
  | cons e es ih =>
    simp only [List.foldl_cons, ih, List.mem_cons]
    unfold agentsStep
    split
    · rename_i hex
      constructor
      · rintro (h | ⟨e', he', h1, h2⟩)
        · exact Or.inl h
        · exact Or.inr ⟨e', Or.inr he', h1, h2⟩
      · rintro (h | ⟨e', (rfl | he'), h1, h2⟩)
        · exact Or.inl h
        · simp [hex] at h1
        · exact Or.inr ⟨e', he', h1, h2⟩
    · rename_i hex
      rw [mem_foldl_pushNew]
      constructor
      · rintro ((h | h) | ⟨e', he', h1, h2⟩)
        · exact Or.inl h
        · exact Or.inr ⟨e, Or.inl rfl, by simpa using hex, h⟩
        · exact Or.inr ⟨e', Or.inr he', h1, h2⟩
      · rintro (h | ⟨e', (rfl | he'), h1, h2⟩)
        · exact Or.inl (Or.inl h)
        · exact Or.inl (Or.inr h2)
        · exact Or.inr ⟨e', he', h1, h2⟩

omit [Ord A] in
/-- "Agent X has an edge to node N" == X is a resolved writer of at least one
non-excluded edge incident to N (module doc). Any age, any type but the two
excluded ones. -/
theorem mem_agentsOf (tl : List (Interaction A)) (es : List (Edge A)) (x : A) :
    x ∈ agentsOf tl es ↔ ∃ e ∈ es, e.ty.excluded = false ∧ x ∈ (writersOf tl e).1 := by
  unfold agentsOf
  rw [mem_foldl_agentsStep]
  simp

omit [Ord A] in
theorem agentsOf_nodup (tl : List (Interaction A)) (es : List (Edge A)) :
    (agentsOf tl es).Nodup := by
  unfold agentsOf
  suffices ∀ as : List A, as.Nodup → (es.foldl (agentsStep tl) as).Nodup from this [] (by simp)
  induction es with
  | nil => intro as h; exact h
  | cons e es ih =>
    intro as h
    simp only [List.foldl_cons]
    apply ih
    unfold agentsStep
    split
    · exact h
    · exact nodup_foldl_pushNew _ h

omit [DecidableEq A] [Ord A] in
/-! ### The newest qualifying write -/

omit [DecidableEq A] [Ord A] in
theorem newestStep_excluded {lo : Int} {w : Option Nat} {now : Int} {tl : List (Interaction A)}
    {e : Edge A} (h : e.ty.excluded = true) (acc : Option (Int × A)) :
    newestStep lo w now tl acc e = acc := by
  simp [newestStep, h]

omit [DecidableEq A] [Ord A] in
theorem qualifies_false_of_excluded {lo : Int} {w : Option Nat} {now : Int} {e : Edge A}
    (h : e.ty.excluded = true) : qualifies lo w now e = false := by
  cases hq : e.ty.qualifying
  · simp [qualifies, hq]
  · have := EdgeType.excluded_false_of_qualifying hq
    rw [this] at h
    exact absurd h (by decide)

omit [DecidableEq A] [Ord A] in
theorem newestStep_eq (lo : Int) (w : Option Nat) (now : Int) (tl : List (Interaction A))
    (acc : Option (Int × A)) (e : Edge A) :
    newestStep lo w now tl acc e =
      if (qualifies lo w now e && beats acc (lastWrite e)) = true then
        some (lastWrite e, (writersOf tl e).2)
      else acc := by
  cases hx : e.ty.excluded
  · simp [newestStep, hx]
  · rw [newestStep_excluded hx, qualifies_false_of_excluded hx]
    simp

omit [DecidableEq A] [Ord A] in
theorem newestStep_none (lo : Int) (w : Option Nat) (now : Int) (tl : List (Interaction A))
    (acc : Option (Int × A)) (e : Edge A) :
    newestStep lo w now tl acc e = none ↔ acc = none ∧ qualifies lo w now e = false := by
  rw [newestStep_eq]
  by_cases hc : (qualifies lo w now e && beats acc (lastWrite e)) = true
  · simp only [hc, ↓reduceIte]
    simp only [Bool.and_eq_true] at hc
    simp [hc.1]
  · simp only [hc]
    constructor
    · intro h
      subst h
      simp only [beats, Bool.and_true] at hc
      exact ⟨rfl, by simpa using hc⟩
    · exact fun h => h.1

omit [DecidableEq A] [Ord A] in
theorem newest_fold_none (lo : Int) (w : Option Nat) (now : Int) (tl : List (Interaction A))
    (es : List (Edge A)) (acc : Option (Int × A)) :
    es.foldl (newestStep lo w now tl) acc = none ↔
      acc = none ∧ ∀ e ∈ es, qualifies lo w now e = false := by
  induction es generalizing acc with
  | nil => simp
  | cons e es ih =>
    simp only [List.foldl_cons, ih, newestStep_none, List.mem_cons, forall_eq_or_imp, and_assoc]

omit [DecidableEq A] [Ord A] in
theorem newest_fold_ge (lo : Int) (w : Option Nat) (now : Int) (tl : List (Interaction A))
    (es : List (Edge A)) (acc : Option (Int × A)) (t : Int) (wr : A)
    (h : es.foldl (newestStep lo w now tl) acc = some (t, wr)) :
    (∀ s, acc = some s → s.1 ≤ t) ∧ ∀ e ∈ es, qualifies lo w now e = true → lastWrite e ≤ t := by
  induction es generalizing acc with
  | nil =>
    simp at h
    subst h
    simp
  | cons e es ih =>
    simp only [List.foldl_cons] at h
    have ⟨h1, h2⟩ := ih _ h
    rw [newestStep_eq] at h1
    by_cases hc : (qualifies lo w now e && beats acc (lastWrite e)) = true
    · simp only [hc, ↓reduceIte] at h1
      have hle := h1 _ rfl
      simp only at hle
      simp only [Bool.and_eq_true] at hc
      refine ⟨?_, ?_⟩
      · rintro ⟨s1, s2⟩ rfl
        simp [beats] at hc
        simp only
        omega
      · intro e' he' hq
        simp only [List.mem_cons] at he'
        rcases he' with rfl | he'
        · exact hle
        · exact h2 e' he' hq
    · simp only [hc] at h1
      refine ⟨h1, ?_⟩
      intro e' he' hq
      simp only [List.mem_cons] at he'
      rcases he' with rfl | he'
      · cases acc with
        | none => simp [beats, hq] at hc
        | some s =>
          obtain ⟨s1, s2⟩ := s
          have := h1 _ rfl
          simp [beats, hq] at hc
          simp only at this
          omega
      · exact h2 e' he' hq

omit [DecidableEq A] [Ord A] in
theorem newest_fold_src (lo : Int) (w : Option Nat) (now : Int) (tl : List (Interaction A))
    (es : List (Edge A)) (acc : Option (Int × A)) (t : Int) (wr : A)
    (h : es.foldl (newestStep lo w now tl) acc = some (t, wr)) :
    acc = some (t, wr) ∨
      ∃ e ∈ es, qualifies lo w now e = true ∧ lastWrite e = t ∧ (writersOf tl e).2 = wr := by
  induction es generalizing acc with
  | nil => simp at h; exact Or.inl h
  | cons e es ih =>
    simp only [List.foldl_cons] at h
    rcases ih _ h with h' | ⟨e', he', h1, h2, h3⟩
    · rw [newestStep_eq] at h'
      by_cases hc : (qualifies lo w now e && beats acc (lastWrite e)) = true
      · simp only [hc, ↓reduceIte] at h'
        simp only [Option.some.injEq, Prod.mk.injEq] at h'
        simp only [Bool.and_eq_true] at hc
        exact Or.inr ⟨e, by simp, hc.1, h'.1, h'.2⟩
      · simp only [hc] at h'
        exact Or.inl h'
    · exact Or.inr ⟨e', by simp [he'], h1, h2, h3⟩

omit [DecidableEq A] [Ord A] in
theorem newestOf_none_iff (lo : Int) (w : Option Nat) (now : Int) (tl : List (Interaction A))
    (es : List (Edge A)) :
    newestOf lo w now tl es = none ↔ ∀ e ∈ es, qualifies lo w now e = false := by
  unfold newestOf
  rw [newest_fold_none]
  simp

omit [DecidableEq A] [Ord A] in
/-- The newest qualifying write is the latest qualifying write time, made by
the named writer of a qualifying edge with that time. -/
theorem newestOf_some (lo : Int) (w : Option Nat) (now : Int) (tl : List (Interaction A))
    (es : List (Edge A)) (t : Int) (wr : A) (h : newestOf lo w now tl es = some (t, wr)) :
    (∃ e ∈ es, qualifies lo w now e = true ∧ lastWrite e = t ∧ (writersOf tl e).2 = wr) ∧
      ∀ e ∈ es, qualifies lo w now e = true → lastWrite e ≤ t := by
  unfold newestOf at h
  refine ⟨?_, (newest_fold_ge lo w now tl es none t wr h).2⟩
  rcases newest_fold_src lo w now tl es none t wr h with h' | h'
  · simp at h'
  · exact h'

/-! ### The verdict -/

theorem conflictAt_eq_some {lo : Int} {w : Option Nat} {now : Int} {tl : List (Interaction A)}
    {es : List (Edge A)} {h : Hit A} (hc : conflictAt lo w now tl es = some h) :
    2 ≤ (agentsOf tl es).length ∧ h.agents = sortAgents (agentsOf tl es) ∧
      ∃ t, newestOf lo w now tl es = some (t, h.writer) ∧ h.secondsAgo = (now - t).toNat := by
  unfold conflictAt at hc
  simp only at hc
  split at hc
  · simp at hc
  · rename_i hlen
    split at hc
    · simp at hc
    · rename_i t wr hn
      simp only [Option.some.injEq] at hc
      subst hc
      exact ⟨by omega, rfl, t, hn, rfl⟩

/-- **The conflict rule.** A conflict fires iff two distinct agents are
resolved writers of non-excluded edges incident to the node (any age), and at
least one `Causal`/`Dependency` write is inside the window. -/
theorem conflictAt_isSome_iff (lo : Int) (w : Option Nat) (now : Int)
    (tl : List (Interaction A)) (es : List (Edge A)) :
    (conflictAt lo w now tl es).isSome ↔
      (∃ a b, a ≠ b ∧ (∃ e ∈ es, e.ty.excluded = false ∧ a ∈ (writersOf tl e).1) ∧
        (∃ e ∈ es, e.ty.excluded = false ∧ b ∈ (writersOf tl e).1)) ∧
      ∃ e ∈ es, qualifies lo w now e = true := by
  have hlen := two_le_length_iff (agentsOf_nodup tl es)
  unfold conflictAt
  simp only
  split
  · rename_i hl
    simp only [Option.isSome_none, Bool.false_eq_true, false_iff, not_and]
    intro ⟨a, b, hab, ha, hb⟩
    exact absurd (hlen.2 ⟨a, (mem_agentsOf _ _ _).2 ha, b, (mem_agentsOf _ _ _).2 hb, hab⟩)
      (by omega)
  · rename_i hl
    have ⟨a, ha, b, hb, hab⟩ := hlen.1 (by omega)
    have two : ∃ a b, a ≠ b ∧ (∃ e ∈ es, e.ty.excluded = false ∧ a ∈ (writersOf tl e).1) ∧
        (∃ e ∈ es, e.ty.excluded = false ∧ b ∈ (writersOf tl e).1) :=
      ⟨a, b, hab, (mem_agentsOf _ _ _).1 ha, (mem_agentsOf _ _ _).1 hb⟩
    split
    · rename_i hn
      rw [newestOf_none_iff] at hn
      simp only [Option.isSome_none, Bool.false_eq_true, false_iff, not_and]
      intro _ ⟨e, he, hq⟩
      rw [hn e he] at hq
      exact absurd hq (by decide)
    · rename_i t wr hn
      have := (newestOf_some lo w now tl es t wr hn).1
      obtain ⟨e, he, hq, _⟩ := this
      simp only [Option.isSome_some, true_iff]
      exact ⟨two, e, he, hq⟩

/-- `single_agent_does_not_fire`: when every resolved writer of every
non-excluded edge is the one agent `a`, there is no conflict, however many
fresh writes it makes. -/
theorem single_agent_never_conflicts (lo : Int) (w : Option Nat) (now : Int)
    (tl : List (Interaction A)) (es : List (Edge A)) (a : A)
    (h : ∀ e ∈ es, e.ty.excluded = false → ∀ x ∈ (writersOf tl e).1, x = a) :
    conflictAt lo w now tl es = none := by
  cases hc : conflictAt lo w now tl es with
  | none => rfl
  | some hit =>
    have := (conflictAt_isSome_iff lo w now tl es).1 (by simp [hc])
    obtain ⟨⟨x, y, hxy, ⟨e, he, hx, hxw⟩, ⟨f, hf, hy, hyw⟩⟩, _⟩ := this
    exact absurd ((h e he hx x hxw).trans (h f hf hy y hyw).symm) hxy

/-- `conflict_at` fires only when at least two distinct agents contest the
node, each listed once in `ConflictHit::agents`. -/
theorem conflictAt_two_agents {lo : Int} {w : Option Nat} {now : Int}
    {tl : List (Interaction A)} {es : List (Edge A)} {h : Hit A}
    (hc : conflictAt lo w now tl es = some h) :
    h.agents.Nodup ∧ ∃ a ∈ h.agents, ∃ b ∈ h.agents, a ≠ b := by
  obtain ⟨hl, ha, _⟩ := conflictAt_eq_some hc
  rw [ha]
  have hp := sortAgents_perm (agentsOf tl es)
  obtain ⟨a, ma, b, mb, hab⟩ := (two_le_length_iff (agentsOf_nodup tl es)).1 hl
  exact ⟨hp.nodup_iff.2 (agentsOf_nodup tl es), a, hp.mem_iff.2 ma, b, hp.mem_iff.2 mb, hab⟩

/-- The named writer is one of the contesting agents (`ConflictHit::writer`'s
doc: "Every candidate is in `agents`"). -/
theorem writer_mem_agents {lo : Int} {w : Option Nat} {now : Int}
    {tl : List (Interaction A)} {es : List (Edge A)} {h : Hit A}
    (hc : conflictAt lo w now tl es = some h) : h.writer ∈ h.agents := by
  obtain ⟨_, ha, t, hn, _⟩ := conflictAt_eq_some hc
  obtain ⟨⟨e, he, hq, _, hw⟩, _⟩ := newestOf_some lo w now tl es t h.writer hn
  rw [ha, (sortAgents_perm _).mem_iff, mem_agentsOf]
  refine ⟨e, he, ?_, ?_⟩
  · cases hx : e.ty.excluded
    · rfl
    · rw [qualifies_false_of_excluded hx] at hq; exact absurd hq (by decide)
  · rw [← hw]; exact attributeAt_named_mem _ _ _

/-! ### The window -/

omit [DecidableEq A] [Ord A] in
/-- Write activity is inside the window iff its type qualifies and its latest
write is in `[max lo (now - window), now]`. -/
theorem qualifies_iff (lo : Int) (w : Nat) (now : Int) (e : Edge A) :
    qualifies lo (some w) now e = true ↔
      e.ty.qualifying = true ∧ max lo (now - w) ≤ lastWrite e ∧ lastWrite e ≤ now := by
  unfold qualifies windowStart
  simp only [Bool.and_eq_true, decide_eq_true_eq]
  exact ⟨fun ⟨⟨a, b⟩, c⟩ => ⟨a, c, b⟩, fun ⟨a, c, b⟩ => ⟨⟨a, b⟩, c⟩⟩

omit [DecidableEq A] [Ord A] in
/-- `conflict_at` counts a write made exactly one recency window before `now`
(test `window_boundaries_are_inclusive`). -/
theorem window_lower_inclusive (lo : Int) (w : Nat) (now : Int) (e : Edge A)
    (hq : e.ty.qualifying = true) (hlo : lo ≤ now - w) (he : lastWrite e = now - w) :
    qualifies lo (some w) now e = true := by
  rw [qualifies_iff]; refine ⟨hq, ?_, ?_⟩ <;> omega

omit [DecidableEq A] [Ord A] in
/-- `conflict_at` counts a write made exactly at `now` (test
`window_boundaries_are_inclusive`). -/
theorem window_upper_inclusive (lo : Int) (w : Nat) (now : Int) (e : Edge A)
    (hq : e.ty.qualifying = true) (hlo : lo ≤ now) (he : lastWrite e = now) :
    qualifies lo (some w) now e = true := by
  rw [qualifies_iff]; refine ⟨hq, ?_, ?_⟩ <;> omega

omit [DecidableEq A] [Ord A] in
/-- One second older than the window is outside it. -/
theorem window_below_excluded (lo : Int) (w : Nat) (now : Int) (e : Edge A)
    (he : lastWrite e < now - w) : qualifies lo (some w) now e = false := by
  cases h : qualifies lo (some w) now e
  · rfl
  · rw [qualifies_iff] at h; omega

omit [DecidableEq A] [Ord A] in
/-- A write dated after `now` (a mocked clock) is outside `conflict_at`'s
window, the unrepresentable window included (test
`future_dated_edges_are_outside_the_window`). -/
theorem future_excluded (lo : Int) (w : Option Nat) (now : Int) (e : Edge A)
    (he : now < lastWrite e) : qualifies lo w now e = false := by
  simp [qualifies]
  omega

omit [DecidableEq A] [Ord A] in
/-- An unrepresentable window (`Duration::MAX`) covers all of time up to
`now` (`an_unrepresentable_window_saturates_instead_of_panicking`). -/
theorem unbounded_window_qualifies_iff (lo now : Int) (e : Edge A) (hlo : lo ≤ lastWrite e) :
    qualifies lo none now e = true ↔ e.ty.qualifying = true ∧ lastWrite e ≤ now := by
  simp [qualifies, windowStart, hlo]

/-- `ConflictHit::seconds_ago` (the newest qualifying write's age) is at most
the recency window. -/
theorem secondsAgo_le_window {lo : Int} {w : Nat} {now : Int} {tl : List (Interaction A)}
    {es : List (Edge A)} {h : Hit A} (hc : conflictAt lo (some w) now tl es = some h) :
    h.secondsAgo ≤ w := by
  obtain ⟨_, _, t, hn, hs⟩ := conflictAt_eq_some hc
  obtain ⟨⟨e, _, hq, ht, _⟩, _⟩ := newestOf_some lo (some w) now tl es t h.writer hn
  rw [qualifies_iff] at hq
  rw [hs]
  omega

/-! ### Order independence -/

omit [DecidableEq A] [Ord A] in
theorem newestOf_perm_time {lo : Int} {w : Option Nat} {now : Int} {tl : List (Interaction A)}
    {es es' : List (Edge A)} (hp : es.Perm es') {t t' : Int} {wr wr' : A}
    (h : newestOf lo w now tl es = some (t, wr)) (h' : newestOf lo w now tl es' = some (t', wr')) :
    t = t' := by
  obtain ⟨⟨e, he, hq, ht, _⟩, hmax⟩ := newestOf_some lo w now tl es t wr h
  obtain ⟨⟨e', he', hq', ht', _⟩, hmax'⟩ := newestOf_some lo w now tl es' t' wr' h'
  have h1 := hmax e' (hp.mem_iff.2 he') hq'
  have h2 := hmax' e (hp.mem_iff.1 he) hq
  omega

/-- Whether `conflict_at` fires does not depend on the order `incident_edges`
returns a node's edges (test `detect_is_deterministic_and_sorted`). -/
theorem conflictAt_perm_isSome (lo : Int) (w : Option Nat) (now : Int)
    (tl : List (Interaction A)) {es es' : List (Edge A)} (hp : es.Perm es') :
    (conflictAt lo w now tl es).isSome = (conflictAt lo w now tl es').isSome := by
  apply Bool.eq_iff_iff.2
  rw [conflictAt_isSome_iff, conflictAt_isSome_iff]
  simp only [hp.mem_iff]

/-- `conflict_at`'s agent set (`ConflictHit::agents`) does not depend on the
order `incident_edges` returns a node's edges. -/
theorem conflictAt_perm_agents {lo : Int} {w : Option Nat} {now : Int}
    {tl : List (Interaction A)} {es es' : List (Edge A)} (hp : es.Perm es') {h h' : Hit A}
    (hc : conflictAt lo w now tl es = some h) (hc' : conflictAt lo w now tl es' = some h')
    (x : A) : x ∈ h.agents ↔ x ∈ h'.agents := by
  rw [(conflictAt_eq_some hc).2.1, (conflictAt_eq_some hc').2.1, (sortAgents_perm _).mem_iff,
    (sortAgents_perm _).mem_iff, mem_agentsOf, mem_agentsOf]
  simp only [hp.mem_iff]

/-- Nor does `seconds_ago`. -/
theorem conflictAt_perm_secondsAgo {lo : Int} {w : Option Nat} {now : Int}
    {tl : List (Interaction A)} {es es' : List (Edge A)} (hp : es.Perm es') {h h' : Hit A}
    (hc : conflictAt lo w now tl es = some h) (hc' : conflictAt lo w now tl es' = some h') :
    h.secondsAgo = h'.secondsAgo := by
  obtain ⟨_, _, t, hn, hs⟩ := conflictAt_eq_some hc
  obtain ⟨_, _, t', hn', hs'⟩ := conflictAt_eq_some hc'
  rw [hs, hs', newestOf_perm_time hp hn hn']

/-- `ConflictHit::writer` does not depend on `incident_edges` order as long as
no two qualifying writes at one instant name different agents; the tie is
`writer_order_dependent`. -/
theorem writer_perm_of_unique {lo : Int} {w : Option Nat} {now : Int}
    {tl : List (Interaction A)} {es es' : List (Edge A)} (hp : es.Perm es') {h h' : Hit A}
    (hu : ∀ e₁ ∈ es, ∀ e₂ ∈ es, qualifies lo w now e₁ = true → qualifies lo w now e₂ = true →
      lastWrite e₁ = lastWrite e₂ → (writersOf tl e₁).2 = (writersOf tl e₂).2)
    (hc : conflictAt lo w now tl es = some h) (hc' : conflictAt lo w now tl es' = some h') :
    h.writer = h'.writer := by
  obtain ⟨_, _, t, hn, _⟩ := conflictAt_eq_some hc
  obtain ⟨_, _, t', hn', _⟩ := conflictAt_eq_some hc'
  have htt := newestOf_perm_time hp hn hn'
  obtain ⟨⟨e, he, hq, ht, hw⟩, _⟩ := newestOf_some lo w now tl es t h.writer hn
  obtain ⟨⟨e', he', hq', ht', hw'⟩, _⟩ := newestOf_some lo w now tl es' t' h'.writer hn'
  rw [← hw, ← hw']
  exact hu e he e' (hp.mem_iff.2 he') hq hq' (by omega)

/-! ### Excluded edges -/

omit [Ord A] in
theorem agentsStep_excluded (tl : List (Interaction A)) (as : List A) {e : Edge A}
    (h : e.ty.excluded = true) : agentsStep tl as e = as := by
  simp [agentsStep, h]

/-- A `Contradicts` or `SuspectedSupersedes` edge, wherever it sits, changes
nothing (#19(c) owner decision 2, option B; #198;
`a_contradiction_writer_does_not_join_the_conflict_agent_set`). -/
theorem excluded_edge_irrelevant (lo : Int) (w : Option Nat) (now : Int)
    (tl : List (Interaction A)) (pre suf : List (Edge A)) {e : Edge A}
    (h : e.ty.excluded = true) :
    conflictAt lo w now tl (pre ++ e :: suf) = conflictAt lo w now tl (pre ++ suf) := by
  unfold conflictAt agentsOf newestOf
  simp only [List.foldl_append, List.foldl_cons, agentsStep_excluded tl _ h,
    newestStep_excluded h]

/-! ### Lapse: the hot-list predicate against the caller's clock -/

/-- Once every write to the node is older than the recency window,
`conflict_at` returns `None`, and `HotList::revalidate` drops the entry (test
`revalidate_ages_a_conflict_out_when_only_the_clock_advances`). -/
theorem lapse_of_old (lo : Int) (w : Nat) (now : Int) (tl : List (Interaction A))
    (es : List (Edge A)) (h : ∀ e ∈ es, lastWrite e < now - w) :
    conflictAt lo (some w) now tl es = none := by
  cases hc : conflictAt lo (some w) now tl es with
  | none => rfl
  | some hit =>
    have := ((conflictAt_isSome_iff lo (some w) now tl es).1 (by simp [hc])).2
    obtain ⟨e, he, hq⟩ := this
    rw [window_below_excluded lo w now e (h e he)] at hq
    exact absurd hq (by decide)

omit [DecidableEq A] [Ord A] in
theorem qualifies_later {lo : Int} {w : Nat} {now₁ now₂ : Int} {e : Edge A}
    (h12 : now₁ ≤ now₂) (hw : lastWrite e ≤ now₁) (h : qualifies lo (some w) now₂ e = true) :
    qualifies lo (some w) now₁ e = true := by
  rw [qualifies_iff] at h ⊢
  obtain ⟨hq, h1, _⟩ := h
  refine ⟨hq, Int.max_le.2 ⟨Int.le_trans (Int.le_max_left _ _) h1, ?_⟩, hw⟩
  have := Int.le_trans (Int.le_max_right lo (now₂ - w)) h1
  omega

/-- With nothing written after `now₁`, a conflict that has lapsed at `now₁`
stays lapsed at every later clock. -/
theorem lapse_permanent (lo : Int) (w : Nat) {now₁ now₂ : Int} (tl : List (Interaction A))
    (es : List (Edge A)) (h12 : now₁ ≤ now₂) (hw : ∀ e ∈ es, lastWrite e ≤ now₁)
    (h : conflictAt lo (some w) now₁ tl es = none) :
    conflictAt lo (some w) now₂ tl es = none := by
  cases hc : conflictAt lo (some w) now₂ tl es with
  | none => rfl
  | some hit =>
    have ⟨two, e, he, hq⟩ := (conflictAt_isSome_iff lo (some w) now₂ tl es).1 (by simp [hc])
    have : (conflictAt lo (some w) now₁ tl es).isSome :=
      (conflictAt_isSome_iff lo (some w) now₁ tl es).2
        ⟨two, e, he, qualifies_later h12 (hw e he) hq⟩
    rw [h] at this
    simp at this

/-- `HotList::revalidate` re-runs `conflict_at` at read time, so until a
conflict lapses its age grows by exactly the elapsed time, never the frozen
detection-time value. -/
theorem secondsAgo_advances (lo : Int) (w : Nat) {now₁ now₂ : Int} (tl : List (Interaction A))
    (es : List (Edge A)) (h12 : now₁ ≤ now₂) (hw : ∀ e ∈ es, lastWrite e ≤ now₁) {h₁ h₂ : Hit A}
    (hc₁ : conflictAt lo (some w) now₁ tl es = some h₁)
    (hc₂ : conflictAt lo (some w) now₂ tl es = some h₂) :
    h₂.secondsAgo = h₁.secondsAgo + (now₂ - now₁).toNat := by
  obtain ⟨_, _, t₁, hn₁, hs₁⟩ := conflictAt_eq_some hc₁
  obtain ⟨_, _, t₂, hn₂, hs₂⟩ := conflictAt_eq_some hc₂
  obtain ⟨⟨e₁, he₁, hq₁, ht₁, _⟩, hmax₁⟩ := newestOf_some lo (some w) now₁ tl es t₁ h₁.writer hn₁
  obtain ⟨⟨e₂, he₂, hq₂, ht₂, _⟩, hmax₂⟩ := newestOf_some lo (some w) now₂ tl es t₂ h₂.writer hn₂
  have a := hmax₁ e₂ he₂ (qualifies_later h12 (hw e₂ he₂) hq₂)
  have hq₂' := hq₂
  rw [qualifies_iff] at hq₂'
  have hq₁' := hq₁
  rw [qualifies_iff] at hq₁'
  have b : qualifies lo (some w) now₂ e₁ = true := by
    rw [qualifies_iff]
    have := hw e₁ he₁
    refine ⟨hq₁'.1, ?_, ?_⟩ <;> omega
  have c := hmax₂ e₁ he₁ b
  rw [hs₁, hs₂]
  have := hw e₁ he₁
  omega

/-! ### Concrete cases (the Rust tests, in model form)

Agents are `Nat`: agent-a is `1`, agent-b is `2`. Times are seconds relative
to the tests' base instant; `lo` stands for `MIN_UTC`. -/

/-- chrono's `MIN_UTC` in these examples: far below every time used. -/
def lo₀ : Int := -10000000000000

/-- A `Dependency` edge from a concept with `origin`, written once at `t`. -/
def dep (id : Nat) (origin : Nat) (t : Int) : Edge Nat :=
  ⟨id, .dependency, .concept origin, t, t, none⟩

/-- A `Derives` edge from an interaction of `agent`, written at `t`. -/
def derives (id : Nat) (agent : Nat) (t : Int) : Edge Nat :=
  ⟨id, .derives, .interaction agent, t, t, none⟩

/-- `two_agent_graph(c1_at, dep_at)`: agent-a's interaction derives c1 at
`c1At`; agent-b's two concepts each depend on c1 at `depAt`. The edges
incident to c1. -/
def twoAgent (c1At depAt : Int) : List (Edge Nat) :=
  [derives 1 1 c1At, dep 2 2 depAt, dep 3 2 depAt]

/-- The timeline of `two_agent_graph`: i1 (agent-a) at `c1At - 20`, i2
(agent-b) at `c1At - 10`. The dependency edges, written at `depAt` with no
interaction at that instant in any example below, fall back to their source
concepts' origin agent, agent-b. -/
def twoAgentTl (c1At : Int) : List (Interaction Nat) :=
  [⟨1, 1, c1At - 20⟩, ⟨2, 2, c1At - 10⟩]

-- `window_boundaries_are_inclusive`.
example : conflictAt lo₀ recencyWindow 3600 (twoAgentTl 3560) (twoAgent 3560 3570) =
    some ⟨[1, 2], 2, 30⟩ := by decide
example : conflictAt lo₀ recencyWindow 3600 (twoAgentTl 3560) (twoAgent 3560 3600) =
    some ⟨[1, 2], 2, 0⟩ := by decide
-- `stale_window_does_not_fire`.
example : conflictAt lo₀ recencyWindow 3600 (twoAgentTl 3560) (twoAgent 3560 3569) = none := by
  decide
-- `future_dated_edges_are_outside_the_window`.
example : conflictAt lo₀ recencyWindow 100 (twoAgentTl 110) (twoAgent 110 105) = none := by decide
example : conflictAt lo₀ recencyWindow 100 (twoAgentTl 60) (twoAgent 60 105) = none := by decide
-- `an_unrepresentable_window_saturates_instead_of_panicking`.
example : conflictAt lo₀ none 130 (twoAgentTl 100) (twoAgent 100 120) =
    some ⟨[1, 2], 2, 10⟩ := by decide
-- `single_agent_does_not_fire`: everything incident to c1 is agent-a's.
example : conflictAt lo₀ recencyWindow 3600 [⟨1, 1, 3540⟩, ⟨2, 2, 3550⟩]
    [derives 1 1 3560, dep 2 1 3589] = none := by decide
-- `a_contradiction_writer_does_not_join_the_conflict_agent_set`.
example : conflictAt lo₀ recencyWindow 3600 [⟨1, 1, 3540⟩, ⟨2, 2, 3580⟩]
    [derives 1 1 3540, dep 2 1 3575, ⟨3, .contradicts, .concept 2, 3585, 3585, none⟩,
      ⟨4, .suspectedSupersedes, .concept 2, 3588, 3588, none⟩] = none := by decide
example : conflictAt lo₀ recencyWindow 3600 [⟨1, 1, 3540⟩, ⟨2, 2, 3580⟩]
    [derives 1 1 3540, dep 2 1 3575, ⟨3, .contradicts, .concept 2, 3585, 3585, none⟩,
      dep 5 2 3590] = some ⟨[1, 2], 2, 10⟩ := by decide

/-- `same_instant_cross_agent_write_still_conflicts`: both
interactions at 3590, the cross-agent `Dependency` stamped with that instant;
every candidate joins the set and the writer is the smallest interaction id
(agent-a). -/
theorem same_instant_both_join :
    conflictAt lo₀ recencyWindow 3600 [⟨1, 1, 3590⟩, ⟨2, 2, 3590⟩]
      [derives 1 1 3590, dep 2 2 3590] = some ⟨[1, 2], 1, 10⟩ := by decide

/-- `resolved_concept_write_is_attributed_to_the_acting_agent`:
agent-b's interaction acts at 3589 on an edge whose source concept agent-a
created; the edge is agent-b's. -/
example : conflictAt lo₀ recencyWindow 3600 [⟨1, 1, 3500⟩, ⟨2, 2, 3589⟩]
    [derives 1 1 3500, dep 2 1 3589] = some ⟨[1, 2], 2, 11⟩ := by decide

/-- The planted shape of `fixtures/session-rest-api.json`, as the module doc's
"Active agent" section requires: agent-a's only edge to the node is an old
`Derives`, a year before; agent-b's `Dependency` is fresh. It fires. Only one
of the two agents wrote inside the window. -/
theorem old_edge_agent_counts :
    conflictAt lo₀ recencyWindow 3600 [⟨1, 1, -31536000⟩, ⟨2, 2, 3590⟩]
      [derives 1 1 (-31536000), dep 2 2 3590] = some ⟨[1, 2], 2, 10⟩ := by decide

/-- An agent whose only edge to the node is written **after** `now` (a mocked
clock: the edge "has not happened yet") still joins the agent set, as the
module doc says ("They still count for agent attribution"), and makes a
conflict out of agent-a's lone in-window write. Contrast `written_by` in
`contradiction.rs` (PR #154 review), which ignores an edge written after
`now`. Low impact: production `created_at` is server-stamped, so an edge
written after `now` arises only with a mocked clock or a fixture. -/
theorem future_edge_agent_counts :
    conflictAt lo₀ recencyWindow 3600 [⟨1, 1, 3500⟩, ⟨2, 2, 4000⟩]
      [derives 1 1 3500, dep 2 1 3590, dep 3 2 4000] = some ⟨[1, 2], 1, 10⟩ := by decide

/-- **Re-sending moves attribution.** Agent-b's concept depends on c1 at
3570 (no interaction then, so the origin fallback names agent-b); agent-a
then re-sends the same dependency at 3590, which reinforces the one edge.
The edge is resolved at its latest write, so it is agent-a's alone and
agent-b drops out of the set: two agents wrote that dependency inside the
window, and there is no conflict on c1. Without the re-send it fires.

The theorem is about the re-sent edge's target. In a whole-graph `detect`
the edge's source concept c2 may itself show both its creator (its `Derives`
edge) and the re-sender (this edge), and fire there: then the conflict moves
rather than vanishes. It is suppressed graph-wide when the original edge
writer is not the source concept's creator. For example: agent-a creates
"session store" and "user schema"; agent-b records an action "session store"
depends on "user schema"; agent-a then records the same action. The edge is
agent-a's, both concepts' `Derives` edges are agent-a's, and agent-b vanishes
from every node. Re-sends are reachable through `record_action` and through
`lambo_derive`'s `depends_on` (`src/graph/action.rs`, `src/graph/cite.rs`). -/
theorem resend_drops_original_writer :
    conflictAt lo₀ recencyWindow 3600 [⟨1, 1, 3500⟩, ⟨3, 1, 3590⟩]
        [derives 1 1 3500, ⟨2, .dependency, .concept 2, 3570, 3590, none⟩] = none ∧
      conflictAt lo₀ recencyWindow 3600 [⟨1, 1, 3500⟩]
        [derives 1 1 3500, ⟨2, .dependency, .concept 2, 3570, 3570, none⟩] =
        some ⟨[1, 2], 2, 30⟩ := by decide

/-- **The writer depends on edge order under a tie.** Two `Dependency`
edges written at the same instant, with no interaction at that instant, from
concepts with different origin agents: the named writer is the first edge's
(in `incident_edges` order, the smaller edge id), not "the smallest
interaction id at that instant" the docs describe, since there is none.

Fixture-only: on the production write paths every write stamps the edge with
the acting interaction's `created_at`, and `Causal`/`Dependency` edges are
concept to concept, so qualifying edges written at one instant all resolve
through the same timeline entry to the same named writer. The tie needs a
hand-built or time-rebased edge with no interaction at its instant. -/
theorem writer_order_dependent :
    conflictAt lo₀ recencyWindow 3600 [] [dep 1 1 3590, dep 2 2 3590] =
        some ⟨[1, 2], 1, 10⟩ ∧
      conflictAt lo₀ recencyWindow 3600 [] [dep 2 2 3590, dep 1 1 3590] =
        some ⟨[1, 2], 2, 10⟩ := by decide

/-- `revalidate_ages_a_conflict_out_when_only_the_clock_advances`, on its
`two_agent_graph(3560, 3589)`: the same graph re-validated at later caller
clocks renders the read-time age (11 s, then 21 s), and drops out at 3620,
when the newest write is 31 s old, one second past the window. -/
example : conflictAt lo₀ recencyWindow 3600 (twoAgentTl 3560) (twoAgent 3560 3589) =
    some ⟨[1, 2], 2, 11⟩ := by decide
example : conflictAt lo₀ recencyWindow 3610 (twoAgentTl 3560) (twoAgent 3560 3589) =
    some ⟨[1, 2], 2, 21⟩ := by decide
example : conflictAt lo₀ recencyWindow 3620 (twoAgentTl 3560) (twoAgent 3560 3589) = none := by
  decide

/-! ## The fix for #297: an edge's first writer stays in the agent set

`conflictAt` above is the rule before #297: an edge's writers are resolved at
its latest write only (`writersOf`), so a re-send by another agent replaces
the original writer (`resend_drops_original_writer`). The fixed
`conflict_at_with` resolves every non-excluded edge twice and adds both lists
to the agent set, creation first: at its `created_at` (`creatorsOf`, the
instant contradiction dating already attributes at, which no re-send moves)
and at its latest write (`writersOf`, as before). Everything else reads the
latest write, unchanged: a re-send inside the window is write activity
(`qualifies`), and the newest write, its named writer and `seconds_ago` come
from `newestOf` as before. -/

omit [Ord A] in
/-- The edge's writers at its first write: `attribute_at(edge.created_at)`. -/
def creatorsOf (tl : List (Interaction A)) (e : Edge A) : List A :=
  (attributeAt tl e.source e.createdAt).1

omit [Ord A] in
/-- Every agent the fixed rule credits with an edge: its first writers, then
its latest writers. -/
def candidatesOf (tl : List (Interaction A)) (e : Edge A) : List A :=
  creatorsOf tl e ++ (writersOf tl e).1

omit [Ord A] in
def agentsStepFixed (tl : List (Interaction A)) (as : List A) (e : Edge A) : List A :=
  if e.ty.excluded then as else (candidatesOf tl e).foldl pushNew as

omit [Ord A] in
/-- The fixed agent set, before its sort. -/
def agentsOfFixed (tl : List (Interaction A)) (es : List (Edge A)) : List A :=
  es.foldl (agentsStepFixed tl) []

/-- The fixed `conflict_at_with`: only the agent set changes. -/
def conflictAtFixed (lo : Int) (w : Option Nat) (now : Int) (tl : List (Interaction A))
    (es : List (Edge A)) : Option (Hit A) :=
  let agents := agentsOfFixed tl es
  if agents.length < 2 then none
  else
    match newestOf lo w now tl es with
    | none => none
    | some (t, wr) => some ⟨sortAgents agents, wr, (now - t).toNat⟩

omit [Ord A] in
/-- A re-send of `e` at `r`: on a duplicate natural key `Graph::record_edge`
sets `last_reinforced` to the new write's time and keeps the id, type,
source, `created_at` and `event_time`. -/
def resend (e : Edge A) (r : Int) : Edge A := { e with lastReinforced := r }

/-! ### The fixed agent set -/

omit [DecidableEq A] [Ord A] in
theorem mem_candidatesOf (tl : List (Interaction A)) (e : Edge A) (x : A) :
    x ∈ candidatesOf tl e ↔ x ∈ creatorsOf tl e ∨ x ∈ (writersOf tl e).1 := by
  simp [candidatesOf]

omit [Ord A] in
theorem mem_foldl_agentsStepFixed (tl : List (Interaction A)) (es : List (Edge A))
    (as : List A) (x : A) :
    x ∈ es.foldl (agentsStepFixed tl) as ↔
      x ∈ as ∨ ∃ e ∈ es, e.ty.excluded = false ∧ x ∈ candidatesOf tl e := by
  induction es generalizing as with
  | nil => simp
  | cons e es ih =>
    simp only [List.foldl_cons, ih, List.mem_cons]
    unfold agentsStepFixed
    split
    · rename_i hex
      constructor
      · rintro (h | ⟨e', he', h1, h2⟩)
        · exact Or.inl h
        · exact Or.inr ⟨e', Or.inr he', h1, h2⟩
      · rintro (h | ⟨e', (rfl | he'), h1, h2⟩)
        · exact Or.inl h
        · simp [hex] at h1
        · exact Or.inr ⟨e', he', h1, h2⟩
    · rename_i hex
      rw [mem_foldl_pushNew]
      constructor
      · rintro ((h | h) | ⟨e', he', h1, h2⟩)
        · exact Or.inl h
        · exact Or.inr ⟨e, Or.inl rfl, by simpa using hex, h⟩
        · exact Or.inr ⟨e', Or.inr he', h1, h2⟩
      · rintro (h | ⟨e', (rfl | he'), h1, h2⟩)
        · exact Or.inl (Or.inl h)
        · exact Or.inl (Or.inr h2)
        · exact Or.inr ⟨e', he', h1, h2⟩

omit [Ord A] in
/-- "Agent X has an edge to node N" under the fix: X is a writer of a
non-excluded edge incident to N, at its first write or at its latest. -/
theorem mem_agentsOfFixed (tl : List (Interaction A)) (es : List (Edge A)) (x : A) :
    x ∈ agentsOfFixed tl es ↔ ∃ e ∈ es, e.ty.excluded = false ∧ x ∈ candidatesOf tl e := by
  unfold agentsOfFixed
  rw [mem_foldl_agentsStepFixed]
  simp

omit [Ord A] in
theorem agentsOfFixed_nodup (tl : List (Interaction A)) (es : List (Edge A)) :
    (agentsOfFixed tl es).Nodup := by
  unfold agentsOfFixed
  suffices ∀ as : List A, as.Nodup → (es.foldl (agentsStepFixed tl) as).Nodup from
    this [] (by simp)
  induction es with
  | nil => intro as h; exact h
  | cons e es ih =>
    intro as h
    simp only [List.foldl_cons]
    apply ih
    unfold agentsStepFixed
    split
    · exact h
    · exact nodup_foldl_pushNew _ h

omit [Ord A] in
/-- The fixed attribution of a re-sent write (`edge_writers`, test
`a_resend_by_another_agent_keeps_the_original_writer`) only adds agents: every
agent the old rule counted still counts. -/
theorem agentsOf_sub_fixed (tl : List (Interaction A)) (es : List (Edge A)) {x : A}
    (h : x ∈ agentsOf tl es) : x ∈ agentsOfFixed tl es := by
  rw [mem_agentsOf] at h
  obtain ⟨e, he, hx, hw⟩ := h
  exact (mem_agentsOfFixed tl es x).2 ⟨e, he, hx, (mem_candidatesOf tl e x).2 (Or.inr hw)⟩

omit [Ord A] in
theorem foldl_pushNew_of_mem (l as : List A) (h : ∀ x ∈ l, x ∈ as) : l.foldl pushNew as = as := by
  induction l with
  | nil => rfl
  | cons a l ih =>
    simp only [List.foldl_cons]
    have ha : a ∈ as := h a (by simp)
    rw [show pushNew as a = as by simp [pushNew, ha]]
    exact ih (fun x hx => h x (by simp [hx]))

omit [Ord A] in
/-- On an edge never re-sent (`last_reinforced ≤ created_at`, as at insert)
the fixed step is the old one: the first write is the latest. -/
theorem agentsStepFixed_of_unreinforced (tl : List (Interaction A)) (as : List A) (e : Edge A)
    (h : e.lastReinforced ≤ e.createdAt) : agentsStepFixed tl as e = agentsStep tl as e := by
  have hlw : lastWrite e = e.createdAt := Int.max_eq_right h
  have hc : creatorsOf tl e = (writersOf tl e).1 := by
    simp only [creatorsOf, writersOf, hlw]
  unfold agentsStepFixed agentsStep candidatesOf
  split
  · rfl
  · rw [hc, List.foldl_append]
    apply foldl_pushNew_of_mem
    intro x hx
    rw [mem_foldl_pushNew]
    exact Or.inr hx

omit [Ord A] in
theorem agentsOfFixed_of_unreinforced (tl : List (Interaction A)) (es : List (Edge A))
    (h : ∀ e ∈ es, e.lastReinforced ≤ e.createdAt) : agentsOfFixed tl es = agentsOf tl es := by
  unfold agentsOfFixed agentsOf
  generalize ([] : List A) = as
  induction es generalizing as with
  | nil => rfl
  | cons e es ih =>
    simp only [List.foldl_cons]
    rw [agentsStepFixed_of_unreinforced tl as e (h e (by simp))]
    exact ih (fun f hf => h f (by simp [hf])) _

/-! ### The fixed verdict -/

/-- **Agreement.** Where no incident edge was ever re-sent, the fix changes
nothing: verdict, agents, writer and age are the old rule's. -/
theorem conflictAtFixed_of_unreinforced (lo : Int) (w : Option Nat) (now : Int)
    (tl : List (Interaction A)) (es : List (Edge A))
    (h : ∀ e ∈ es, e.lastReinforced ≤ e.createdAt) :
    conflictAtFixed lo w now tl es = conflictAt lo w now tl es := by
  unfold conflictAtFixed conflictAt
  rw [agentsOfFixed_of_unreinforced tl es h]

theorem conflictAtFixed_eq_some {lo : Int} {w : Option Nat} {now : Int}
    {tl : List (Interaction A)} {es : List (Edge A)} {h : Hit A}
    (hc : conflictAtFixed lo w now tl es = some h) :
    2 ≤ (agentsOfFixed tl es).length ∧ h.agents = sortAgents (agentsOfFixed tl es) ∧
      ∃ t, newestOf lo w now tl es = some (t, h.writer) ∧ h.secondsAgo = (now - t).toNat := by
  unfold conflictAtFixed at hc
  simp only at hc
  split at hc
  · simp at hc
  · rename_i hlen
    split at hc
    · simp at hc
    · rename_i t wr hn
      simp only [Option.some.injEq] at hc
      subst hc
      exact ⟨by omega, rfl, t, hn, rfl⟩

/-- **The fixed conflict rule.** A conflict fires iff two distinct agents
each wrote a non-excluded edge incident to the node (at its first write or its
latest, any age), and at least one `Causal`/`Dependency` write is inside the
window. -/
theorem conflictAtFixed_isSome_iff (lo : Int) (w : Option Nat) (now : Int)
    (tl : List (Interaction A)) (es : List (Edge A)) :
    (conflictAtFixed lo w now tl es).isSome ↔
      (∃ a b, a ≠ b ∧ (∃ e ∈ es, e.ty.excluded = false ∧ a ∈ candidatesOf tl e) ∧
        (∃ e ∈ es, e.ty.excluded = false ∧ b ∈ candidatesOf tl e)) ∧
      ∃ e ∈ es, qualifies lo w now e = true := by
  have hlen := two_le_length_iff (agentsOfFixed_nodup tl es)
  unfold conflictAtFixed
  simp only
  split
  · rename_i hl
    simp only [Option.isSome_none, Bool.false_eq_true, false_iff, not_and]
    intro ⟨a, b, hab, ha, hb⟩
    exact absurd (hlen.2 ⟨a, (mem_agentsOfFixed _ _ _).2 ha, b,
      (mem_agentsOfFixed _ _ _).2 hb, hab⟩) (by omega)
  · rename_i hl
    have ⟨a, ha, b, hb, hab⟩ := hlen.1 (by omega)
    have two : ∃ a b, a ≠ b ∧ (∃ e ∈ es, e.ty.excluded = false ∧ a ∈ candidatesOf tl e) ∧
        (∃ e ∈ es, e.ty.excluded = false ∧ b ∈ candidatesOf tl e) :=
      ⟨a, b, hab, (mem_agentsOfFixed _ _ _).1 ha, (mem_agentsOfFixed _ _ _).1 hb⟩
    split
    · rename_i hn
      rw [newestOf_none_iff] at hn
      simp only [Option.isSome_none, Bool.false_eq_true, false_iff, not_and]
      intro _ ⟨e, he, hq⟩
      rw [hn e he] at hq
      exact absurd hq (by decide)
    · rename_i t wr hn
      have := (newestOf_some lo w now tl es t wr hn).1
      obtain ⟨e, he, hq, _⟩ := this
      simp only [Option.isSome_some, true_iff]
      exact ⟨two, e, he, hq⟩

/-- The fix never loses a conflict the old rule found. -/
theorem conflictAtFixed_of_conflictAt {lo : Int} {w : Option Nat} {now : Int}
    {tl : List (Interaction A)} {es : List (Edge A)}
    (h : (conflictAt lo w now tl es).isSome) : (conflictAtFixed lo w now tl es).isSome := by
  rw [conflictAt_isSome_iff] at h
  rw [conflictAtFixed_isSome_iff]
  obtain ⟨⟨a, b, hab, ⟨e, he, hx, ha⟩, ⟨f, hf, hy, hb⟩⟩, hq⟩ := h
  exact ⟨⟨a, b, hab, ⟨e, he, hx, (mem_candidatesOf _ _ _).2 (Or.inr ha)⟩,
    ⟨f, hf, hy, (mem_candidatesOf _ _ _).2 (Or.inr hb)⟩⟩, hq⟩

/-- Under the fixed attribution, a node every qualifying edge of which has one
candidate writer never yields a `ConflictHit` (test
`single_agent_does_not_fire`). -/
theorem single_agent_never_conflicts_fixed (lo : Int) (w : Option Nat) (now : Int)
    (tl : List (Interaction A)) (es : List (Edge A)) (a : A)
    (h : ∀ e ∈ es, e.ty.excluded = false → ∀ x ∈ candidatesOf tl e, x = a) :
    conflictAtFixed lo w now tl es = none := by
  cases hc : conflictAtFixed lo w now tl es with
  | none => rfl
  | some hit =>
    have := (conflictAtFixed_isSome_iff lo w now tl es).1 (by simp [hc])
    obtain ⟨⟨x, y, hxy, ⟨e, he, hx, hxw⟩, ⟨f, hf, hy, hyw⟩⟩, _⟩ := this
    exact absurd ((h e he hx x hxw).trans (h f hf hy y hyw).symm) hxy

/-- The named writer is still one of the contesting agents: it is resolved at
the latest write, whose writers the fixed set keeps. -/
theorem writer_mem_agents_fixed {lo : Int} {w : Option Nat} {now : Int}
    {tl : List (Interaction A)} {es : List (Edge A)} {h : Hit A}
    (hc : conflictAtFixed lo w now tl es = some h) : h.writer ∈ h.agents := by
  obtain ⟨_, ha, t, hn, _⟩ := conflictAtFixed_eq_some hc
  obtain ⟨⟨e, he, hq, _, hw⟩, _⟩ := newestOf_some lo w now tl es t h.writer hn
  rw [ha, (sortAgents_perm _).mem_iff, mem_agentsOfFixed]
  refine ⟨e, he, ?_, ?_⟩
  · cases hx : e.ty.excluded
    · rfl
    · rw [qualifies_false_of_excluded hx] at hq; exact absurd hq (by decide)
  · rw [mem_candidatesOf]
    right
    rw [← hw]; exact attributeAt_named_mem _ _ _

/-- Under the fixed attribution, `ConflictHit::seconds_ago` is still at most
the recency window. -/
theorem secondsAgo_le_window_fixed {lo : Int} {w : Nat} {now : Int}
    {tl : List (Interaction A)} {es : List (Edge A)} {h : Hit A}
    (hc : conflictAtFixed lo (some w) now tl es = some h) : h.secondsAgo ≤ w := by
  obtain ⟨_, _, t, hn, hs⟩ := conflictAtFixed_eq_some hc
  obtain ⟨⟨e, _, hq, ht, _⟩, _⟩ := newestOf_some lo (some w) now tl es t h.writer hn
  rw [qualifies_iff] at hq
  rw [hs]
  omega

/-! ### Order independence, under the fix -/

/-- Whether the fixed `conflict_at_with` fires does not depend on
`incident_edges` order. -/
theorem conflictAtFixed_perm_isSome (lo : Int) (w : Option Nat) (now : Int)
    (tl : List (Interaction A)) {es es' : List (Edge A)} (hp : es.Perm es') :
    (conflictAtFixed lo w now tl es).isSome = (conflictAtFixed lo w now tl es').isSome := by
  apply Bool.eq_iff_iff.2
  rw [conflictAtFixed_isSome_iff, conflictAtFixed_isSome_iff]
  simp only [hp.mem_iff]

/-- The fixed `conflict_at_with`'s agent set does not depend on
`incident_edges` order. -/
theorem conflictAtFixed_perm_agents {lo : Int} {w : Option Nat} {now : Int}
    {tl : List (Interaction A)} {es es' : List (Edge A)} (hp : es.Perm es') {h h' : Hit A}
    (hc : conflictAtFixed lo w now tl es = some h)
    (hc' : conflictAtFixed lo w now tl es' = some h') (x : A) :
    x ∈ h.agents ↔ x ∈ h'.agents := by
  rw [(conflictAtFixed_eq_some hc).2.1, (conflictAtFixed_eq_some hc').2.1,
    (sortAgents_perm _).mem_iff, (sortAgents_perm _).mem_iff, mem_agentsOfFixed,
    mem_agentsOfFixed]
  simp only [hp.mem_iff]

/-- Nor does `seconds_ago`. -/
theorem conflictAtFixed_perm_secondsAgo {lo : Int} {w : Option Nat} {now : Int}
    {tl : List (Interaction A)} {es es' : List (Edge A)} (hp : es.Perm es') {h h' : Hit A}
    (hc : conflictAtFixed lo w now tl es = some h)
    (hc' : conflictAtFixed lo w now tl es' = some h') :
    h.secondsAgo = h'.secondsAgo := by
  obtain ⟨_, _, t, hn, hs⟩ := conflictAtFixed_eq_some hc
  obtain ⟨_, _, t', hn', hs'⟩ := conflictAtFixed_eq_some hc'
  rw [hs, hs', newestOf_perm_time hp hn hn']

/-- Nor does the named writer, under the same uniqueness condition as
`writer_perm_of_unique`. -/
theorem writer_perm_of_unique_fixed {lo : Int} {w : Option Nat} {now : Int}
    {tl : List (Interaction A)} {es es' : List (Edge A)} (hp : es.Perm es') {h h' : Hit A}
    (hu : ∀ e₁ ∈ es, ∀ e₂ ∈ es, qualifies lo w now e₁ = true → qualifies lo w now e₂ = true →
      lastWrite e₁ = lastWrite e₂ → (writersOf tl e₁).2 = (writersOf tl e₂).2)
    (hc : conflictAtFixed lo w now tl es = some h)
    (hc' : conflictAtFixed lo w now tl es' = some h') :
    h.writer = h'.writer := by
  obtain ⟨_, _, t, hn, _⟩ := conflictAtFixed_eq_some hc
  obtain ⟨_, _, t', hn', _⟩ := conflictAtFixed_eq_some hc'
  have htt := newestOf_perm_time hp hn hn'
  obtain ⟨⟨e, he, hq, ht, hw⟩, _⟩ := newestOf_some lo w now tl es t h.writer hn
  obtain ⟨⟨e', he', hq', ht', hw'⟩, _⟩ := newestOf_some lo w now tl es' t' h'.writer hn'
  rw [← hw, ← hw']
  exact hu e he e' (hp.mem_iff.2 he') hq hq' (by omega)

omit [Ord A] in
theorem agentsStepFixed_excluded (tl : List (Interaction A)) (as : List A) {e : Edge A}
    (h : e.ty.excluded = true) : agentsStepFixed tl as e = as := by
  simp [agentsStepFixed, h]

/-- Under the fixed attribution a `Contradicts` or `SuspectedSupersedes` edge
still never changes `conflict_at`'s answer (test
`a_contradiction_writer_does_not_join_the_conflict_agent_set`). -/
theorem excluded_edge_irrelevant_fixed (lo : Int) (w : Option Nat) (now : Int)
    (tl : List (Interaction A)) (pre suf : List (Edge A)) {e : Edge A}
    (h : e.ty.excluded = true) :
    conflictAtFixed lo w now tl (pre ++ e :: suf) = conflictAtFixed lo w now tl (pre ++ suf) := by
  unfold conflictAtFixed agentsOfFixed newestOf
  simp only [List.foldl_append, List.foldl_cons, agentsStepFixed_excluded tl _ h,
    newestStep_excluded h]

/-! ### Re-sends under the fix -/

omit [Ord A] in
/-- **The original writer stays.** Whoever the timeline resolves at an
edge's first write is in the agent set of every node the edge touches after a
re-send of it, by any agent at any time. A re-send keeps `created_at`, so this
applies again to each later re-send. -/
theorem resend_keeps_creator (tl : List (Interaction A)) (pre suf : List (Edge A))
    (e : Edge A) (r : Int) (hx : e.ty.excluded = false) {x : A} (h : x ∈ creatorsOf tl e) :
    x ∈ agentsOfFixed tl (pre ++ resend e r :: suf) :=
  (mem_agentsOfFixed tl _ x).2
    ⟨resend e r, by simp, hx, (mem_candidatesOf tl (resend e r) x).2 (Or.inl h)⟩

omit [Ord A] in
/-- The re-sender joins too: whoever the timeline resolves at the re-send's
instant (or at `created_at`, if the re-send is stamped earlier). -/
theorem resend_adds_resender (tl : List (Interaction A)) (pre suf : List (Edge A))
    (e : Edge A) (r : Int) (hx : e.ty.excluded = false) {x : A}
    (h : x ∈ (attributeAt tl e.source (max r e.createdAt)).1) :
    x ∈ agentsOfFixed tl (pre ++ resend e r :: suf) :=
  (mem_agentsOfFixed tl _ x).2
    ⟨resend e r, by simp, hx, (mem_candidatesOf tl (resend e r) x).2 (Or.inr h)⟩

omit [DecidableEq A] [Ord A] in
theorem qualifies_resend {lo : Int} {w : Option Nat} {now : Int} {e : Edge A} {r : Int}
    (hr : lastWrite e ≤ r) (hnow : r ≤ now) (h : qualifies lo w now e = true) :
    qualifies lo w now (resend e r) = true := by
  have hc : e.createdAt ≤ lastWrite e := Int.le_max_right _ _
  have hlw : lastWrite (resend e r) = r := by
    show max r e.createdAt = r
    exact Int.max_eq_left (by omega)
  unfold qualifies at h ⊢
  rw [hlw]
  simp only [Bool.and_eq_true, decide_eq_true_eq] at h ⊢
  exact ⟨⟨h.1.1, hnow⟩, Int.le_trans h.2 hr⟩

/-- **A conflict survives the first re-send.** If the node is in conflict
and an edge never re-sent before is re-sent, by any agent, at an instant no
earlier than its write and no later than `now`, the node is still in
conflict. (The old rule fails this: `resend_drops_original_writer`.) A
*second* re-send can drop the first re-sender, see
`second_resend_drops_first_resender`. -/
theorem resend_still_fires (lo : Int) (w : Option Nat) (now : Int) (tl : List (Interaction A))
    (pre suf : List (Edge A)) (e : Edge A) (r : Int)
    (hfresh : e.lastReinforced ≤ e.createdAt) (hr : lastWrite e ≤ r) (hnow : r ≤ now)
    (h : (conflictAtFixed lo w now tl (pre ++ e :: suf)).isSome) :
    (conflictAtFixed lo w now tl (pre ++ resend e r :: suf)).isSome := by
  have hlw : lastWrite e = e.createdAt := Int.max_eq_right hfresh
  -- Every edge of the old list has a counterpart in the new one with the same
  -- type, at least its candidates, and its qualification.
  have hmove : ∀ f ∈ pre ++ e :: suf, ∃ g ∈ pre ++ resend e r :: suf, g.ty = f.ty ∧
      (∀ x, x ∈ candidatesOf tl f → x ∈ candidatesOf tl g) ∧
      (qualifies lo w now f = true → qualifies lo w now g = true) := by
    intro f hf
    simp only [List.mem_append, List.mem_cons] at hf
    rcases hf with hf | hf | hf
    · exact ⟨f, by simp [hf], rfl, fun _ h => h, id⟩
    · rw [hf]
      refine ⟨resend e r, by simp, rfl, ?_, qualifies_resend hr hnow⟩
      intro x hx
      rw [mem_candidatesOf] at hx ⊢
      left
      show x ∈ creatorsOf tl e
      rcases hx with hx | hx
      · exact hx
      · simpa only [creatorsOf, writersOf, hlw] using hx
    · exact ⟨f, by simp [hf], rfl, fun _ h => h, id⟩
  rw [conflictAtFixed_isSome_iff] at h ⊢
  obtain ⟨⟨a, b, hab, ⟨ea, hea, hxa, ha⟩, ⟨eb, heb, hxb, hb⟩⟩, ⟨eq, heq, hq⟩⟩ := h
  obtain ⟨ga, hga, tya, ca, _⟩ := hmove ea hea
  obtain ⟨gb, hgb, tyb, cb, _⟩ := hmove eb heb
  obtain ⟨gq, hgq, _, _, qq⟩ := hmove eq heq
  exact ⟨⟨a, b, hab, ⟨ga, hga, by rw [tya]; exact hxa, ca a ha⟩,
    ⟨gb, hgb, by rw [tyb]; exact hxb, cb b hb⟩⟩, ⟨gq, hgq, qq hq⟩⟩

/-! ### Concrete cases of the fix -/

/-- **The issue's scenario.** Node "user schema" (u): agent-a's interaction
(i1, 3500) derives it; agent-b's `record_action` (i2, 3570) writes "session
store" (agent-a's concept) depends on u; agent-a records the same action (i3,
3590), which reinforces that edge. The old rule attributes the edge to
agent-a alone and fires nothing; the fix keeps agent-b, the edge's first
writer, and names agent-a, who wrote 10 s ago. The "session store" side is
the same edge with that concept's own `Derives`. -/
theorem issue_297_fixed :
    conflictAt lo₀ recencyWindow 3600 [⟨1, 1, 3500⟩, ⟨2, 2, 3570⟩, ⟨3, 1, 3590⟩]
        [derives 1 1 3500, ⟨3, .dependency, .concept 1, 3570, 3590, none⟩] = none ∧
      conflictAtFixed lo₀ recencyWindow 3600 [⟨1, 1, 3500⟩, ⟨2, 2, 3570⟩, ⟨3, 1, 3590⟩]
        [derives 1 1 3500, ⟨3, .dependency, .concept 1, 3570, 3590, none⟩] =
        some ⟨[1, 2], 1, 10⟩ := by decide

/-- `resend_drops_original_writer`'s graph under the fix: it fires after the
re-send as it did before, and before the re-send the two rules agree. -/
theorem resend_keeps_original_writer :
    conflictAtFixed lo₀ recencyWindow 3600 [⟨1, 1, 3500⟩, ⟨3, 1, 3590⟩]
        [derives 1 1 3500, ⟨2, .dependency, .concept 2, 3570, 3590, none⟩] =
        some ⟨[1, 2], 1, 10⟩ ∧
      conflictAtFixed lo₀ recencyWindow 3600 [⟨1, 1, 3500⟩]
        [derives 1 1 3500, ⟨2, .dependency, .concept 2, 3570, 3570, none⟩] =
        some ⟨[1, 2], 2, 30⟩ := by decide

/-- **Why not attribute at `created_at` alone.** The mirror of the issue:
agent-a writes the dependency (i2, 3570) and agent-b re-sends it (i3, 3590).
The old rule fires, naming agent-b; so does the fix. Resolving the edge at
`created_at` alone gives `[1]`: agent-b's only write to the node would drop
out and no conflict would fire, the same bug in the other order, and the
named writer (agent-b, resolved at the latest write) would not be in the
agent set. -/
theorem resend_by_other_joins :
    conflictAt lo₀ recencyWindow 3600 [⟨1, 1, 3500⟩, ⟨2, 1, 3570⟩, ⟨3, 2, 3590⟩]
        [derives 1 1 3500, ⟨3, .dependency, .concept 1, 3570, 3590, none⟩] =
        some ⟨[1, 2], 2, 10⟩ ∧
      conflictAtFixed lo₀ recencyWindow 3600 [⟨1, 1, 3500⟩, ⟨2, 1, 3570⟩, ⟨3, 2, 3590⟩]
        [derives 1 1 3500, ⟨3, .dependency, .concept 1, 3570, 3590, none⟩] =
        some ⟨[1, 2], 2, 10⟩ ∧
      creatorsOf [⟨1, 1, 3500⟩, ⟨2, 1, 3570⟩, ⟨3, 2, 3590⟩]
        (⟨3, .dependency, .concept 1, 3570, 3590, none⟩ : Edge Nat) = [1] := by decide

/-- **Residual: a second re-send can drop the first re-sender.** Agent-a
writes the dependency (3570), agent-b re-sends it (3580): both are in the set
and it fires. Agent-a re-sends again (3590): the edge keeps only its first
write and its latest, both agent-a's, so agent-b drops out. The graph records
no writer between the first and the latest write (`Edge` has no agent field
and `record_edge` keeps only the latest `last_reinforced`), so no rule over
the edge's recorded fields can keep agent-b: agent-b's interaction is still in
the timeline, but nothing links it to the edge. -/
theorem second_resend_drops_first_resender :
    conflictAtFixed lo₀ recencyWindow 3600
        [⟨1, 1, 3500⟩, ⟨2, 1, 3570⟩, ⟨3, 2, 3580⟩, ⟨4, 1, 3590⟩]
        [derives 1 1 3500, ⟨3, .dependency, .concept 1, 3570, 3580, none⟩] =
        some ⟨[1, 2], 2, 20⟩ ∧
      conflictAtFixed lo₀ recencyWindow 3600
        [⟨1, 1, 3500⟩, ⟨2, 1, 3570⟩, ⟨3, 2, 3580⟩, ⟨4, 1, 3590⟩]
        [derives 1 1 3500, ⟨3, .dependency, .concept 1, 3570, 3590, none⟩] = none := by
  decide

/-- The tests the old rule's examples mirror are unchanged under the fix
(every edge there is written once). -/
example : conflictAtFixed lo₀ recencyWindow 3600 (twoAgentTl 3560) (twoAgent 3560 3589) =
    some ⟨[1, 2], 2, 11⟩ := by decide
example : conflictAtFixed lo₀ recencyWindow 3600 [⟨1, 1, -31536000⟩, ⟨2, 2, 3590⟩]
    [derives 1 1 (-31536000), dep 2 2 3590] = some ⟨[1, 2], 2, 10⟩ := by decide

end Sorted

end Lambo.Conflict
