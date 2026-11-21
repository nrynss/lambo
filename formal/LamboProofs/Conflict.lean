/-
# Conflict detection and contradiction dating

Model of two daemon detectors that share write-time attribution:

* `src/daemon/conflict.rs`: `WriterTimeline::acting`, `EdgeWriters::{all,named}`,
  `edge_writers` / `attribute_at` (`attributeAt`, `writersOf`), `chrono_window`
  and `window_start` (`windowStart`), the per-node check `conflict_at_with`
  (`conflictAt`, split into the two independent accumulations of its loop,
  `agentsOf` and `newestOf`), `CONFLICT_RECENCY_WINDOW` (`recencyWindow`), and
  the payload `payload_of` / `events::conflict_event` read from a `Hit`.
* `src/daemon/contradiction.rs`: `declared_at` (`declaredAt`), `written_by`
  (`writtenBy`), the cutoff `ChronoDuration::from_std(min_age)` then
  `now.checked_sub_signed` (`cutoff`), the evidence guards (`eligible`,
  `counts`), the baseline (`baselineOf`), and `contradiction_at_with`
  (`contradictionAt`, with `latest`, `admissible`, `rank`, `better`).
* The hot list's use of both (`src/daemon/hotlist.rs`, `HotList::revalidate`):
  an entry's predicate re-runs the per-node check at the **caller's** `now`
  and the entry is dropped when it returns `None`. That is the function
  `fun now => conflictAt lo w now tl es`; the lapse theorems below are about
  it. Priority, eviction and `retain_conditions` belong to the hot-list model.

## Model

* **Time** is `Int` seconds. Rust times are `DateTime<Utc>` with nanoseconds
  and `seconds_ago` is `num_seconds()` (truncated); with whole seconds the
  truncation is the identity. chrono's representable range enters only through
  its lower end `lo` (`DateTime::<Utc>::MIN_UTC`), a parameter: every time in
  the graph is assumed `≥ lo`, as chrono guarantees.
* **Durations.** A `std::time::Duration` is `Option Nat`: `some s` when
  `ChronoDuration::from_std` accepts it, `none` when it does not (for example
  `Duration::MAX`). For the conflict window `none` saturates to
  `ChronoDuration::MAX`, and `now - MAX` is always out of chrono's range, so
  `window_start` is `MIN_UTC = lo`; a representable window whose start
  underflows also gives `lo`. For the contradiction `min_age` either failure
  gives no cutoff at all.
* **Agents** are an abstract type with decidable equality and an `Ord`
  instance standing for the Rust's `a.as_str().cmp(b.as_str())`. As in the
  Rust, the agent set is collected deduplicated in discovery order
  (`agentsOf`), tested for two members, then sorted (`sortAgents`, an
  insertion sort the kernel can evaluate for the `decide` examples; the Rust's
  `sort_by` is a stable merge sort, and on duplicate-free agents under a total
  order every correct sort gives the same list). The general theorems about
  `agents` are about membership and duplicate-freedom; the `decide` examples
  state the exact sorted list.
* **Graph.** The per-node check walks `incident_edges(node)`, which is
  id-ascending; the model takes that list as input. An edge's source is
  `Source.interaction agent` (an `Interaction` node: certain attribution) or
  `Source.concept origin` (a concept with that `origin_agent`). The timeline
  is the session's interaction list. A source missing from the graph
  (`attribute_at` returning `None`) is excluded by `assert_invariants` and is
  not modelled.
* **Contradiction inputs** are the node's incident edges already split by the
  Rust's own first filters: `Evidence` is a dependence edge with the node as
  pillar (`Edge::dependence`), `Decl` an inbound non-self `Contradicts` edge.
  The remaining guards (concept, session, own origin, age, written-by-now,
  self-suppression, the strict date test) are modelled as Booleans and
  arithmetic in the order the Rust applies them. Canonical keys are `Nat`
  under their order (the Rust compares `String`s).

## What is left out

`detect` (the walk over every node and the sort by node id), `insert_conflicts`
/ `insert_contradictions` (hot-list insertion order and dedup), the daemon's
emit-on-transition arming (`Daemon` cycle, `cs.armed`), `SharedTimeline`
caching (a performance device: it returns the same timeline per epoch),
`stale_citations` / `stale_report` (#207), `forces_inclusion` and
`contradicts_either_way`.

## Headline theorems, and the Rust sentence or test each pins

Conflict:
* `conflictAt_isSome_iff`: a conflict fires iff two distinct agents are
  resolved writers of non-excluded edges incident to the node, and at least
  one `Causal`/`Dependency` write lies in the window (module doc,
  "A conflict is: ...").
* `single_agent_never_conflicts` (`single_agent_does_not_fire`), and
  `conflictAt_two_agents`: a hit's agent list is duplicate-free and holds two
  distinct agents; `writer_mem_agents`: the named writer is one of them.
* `qualifies_iff`, `window_lower_inclusive`, `window_upper_inclusive`,
  `window_below_excluded`, `future_excluded`: write activity counts iff it is
  in `[now - window, now]`, both ends inclusive
  (`window_boundaries_are_inclusive`, `stale_window_does_not_fire`,
  `future_dated_edges_are_outside_the_window`).
* `unbounded_window_qualifies_iff` (`an_unrepresentable_window_saturates_...`).
* `conflictAt_perm_isSome`, `conflictAt_perm_agents`,
  `conflictAt_perm_secondsAgo`: the verdict, the agent set and `seconds_ago`
  do not depend on the order of the incident edges. `writer_perm_of_unique`:
  neither does the writer when no two newest writes name different agents;
  `writer_order_dependent` shows the order matters otherwise.
* `excluded_edge_irrelevant`: a `Contradicts` or `SuspectedSupersedes` edge
  changes nothing (#19(c) owner decision 2, #198).
* `secondsAgo_le_window`, `lapse_of_old`, `lapse_permanent`,
  `secondsAgo_advances`: the hot-list predicate drops a conflict once its
  newest write leaves the window, it stays dropped while nothing is written,
  and until then its age grows with the caller's clock, never the frozen
  detection-time value
  (`revalidate_ages_a_conflict_out_when_only_the_clock_advances`).
* Behaviour pinned as it is now:
  `old_edge_agent_counts` (only one of the two agents need write inside the
  window, the documented "Active agent" reading); `future_edge_agent_counts`
  (an edge written after `now` still adds its agent, as documented; a
  design-consistency question against `written_by` in `contradiction.rs`,
  reachable only with a mocked clock or fixture); `resend_drops_original_writer`
  (an edge is attributed at its latest write, so a re-send by another agent
  removes the original writer from the edge's target node); and
  `writer_order_dependent` (with no interaction at the newest instant, ties
  between edges go to the smallest edge id, a rule the docs do not state;
  fixture-only, see its doc).

Contradiction:
* `declaredAt_le_createdAt`, `declaredAt_of_le`, `declaredAt_of_ge`,
  `declaredAt_none`: the clamp (`declared_at`'s doc).
* `declaredAt_mono`, `declaredAt_mono_write`: a later write never gets an
  earlier date than the clamp gives; `declaredAt_resend`: re-sending does not
  re-date (module doc, "Dating").
* `declaredAt_resend` and `eligible_eventTime` hold by `rfl`: they follow from
  the modelling choice that a re-send changes only `lastReinforced` (and
  `weight`, not modelled), which is what `Graph::record_edge` does on a
  duplicate natural key (`src/graph/graph.rs`: the original id, `created_at`
  and `event_time` are kept), and from `eligible` reading only `createdAt`.
* `eligible_eventTime`, `eligible_le`, `backdated_not_eligible`: eligibility
  is by write time (`a_backdated_citation_does_not_skip_the_age_floor`).
* `cutoff_none_no_evidence`, `contradictionAt_no_cutoff`,
  `unrepresentable_minAge`: with `now - min_age` out of range nothing is
  eligible (`an_unrepresentable_min_age_makes_no_evidence_eligible`).
* `contradictionAt_sound`: a hit comes from an admissible declaration written
  by `now`, dated strictly after the baseline and not after `now`;
  `not_written_not_admissible` (PR #154 review), `admissible_strict`.
* `admissible_selfSuppressed`, `admissible_of_more_evidence`: self-suppression
  and evidence monotonicity.
* `contradictionAt_max`, `contradictionAt_perm`: the hit is the newest, then
  smallest key, then smallest id, whatever the edge order.
-/

namespace Lambo.Conflict

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

/-- Spec §9 `conflict_recency_window`, `CONFLICT_RECENCY_WINDOW` (30 s). -/
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
/-- A duplicate-free list has two elements iff it holds two distinct ones. -/
theorem two_le_length_iff {l : List A} (h : l.Nodup) :
    2 ≤ l.length ↔ ∃ a ∈ l, ∃ b ∈ l, a ≠ b := by
  constructor
  · intro hl
    match l, hl, h with
    | a :: b :: _, _, h =>
      rw [List.nodup_cons] at h
      exact ⟨a, by simp, b, by simp, fun hab => h.1 (by simp [hab])⟩
  · rintro ⟨a, ha, b, hb, hab⟩
    match l, ha, hb with
    | [], ha, _ => simp at ha
    | [c], ha, hb =>
      simp at ha hb
      exact absurd (ha.trans hb.symm) hab
    | _ :: _ :: _, _, _ => simp

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

/-- A hit's agent list is duplicate-free and holds two distinct agents. -/
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
/-- Inclusive lower bound: a write exactly `window` ago counts. -/
theorem window_lower_inclusive (lo : Int) (w : Nat) (now : Int) (e : Edge A)
    (hq : e.ty.qualifying = true) (hlo : lo ≤ now - w) (he : lastWrite e = now - w) :
    qualifies lo (some w) now e = true := by
  rw [qualifies_iff]; refine ⟨hq, ?_, ?_⟩ <;> omega

omit [DecidableEq A] [Ord A] in
/-- Inclusive upper bound: a write exactly at `now` counts. -/
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
/-- A write after `now` (a mocked clock) is outside the window, for every
window including the unrepresentable one. -/
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

/-- The age of the newest qualifying write is at most the window. -/
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

/-- The verdict does not depend on the order of the incident edges. -/
theorem conflictAt_perm_isSome (lo : Int) (w : Option Nat) (now : Int)
    (tl : List (Interaction A)) {es es' : List (Edge A)} (hp : es.Perm es') :
    (conflictAt lo w now tl es).isSome = (conflictAt lo w now tl es').isSome := by
  apply Bool.eq_iff_iff.2
  rw [conflictAt_isSome_iff, conflictAt_isSome_iff]
  simp only [hp.mem_iff]

/-- Nor does the agent set. -/
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

/-- Nor does the named writer, when no two qualifying writes at the same
instant name different agents. -/
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

/-- Once every write is older than the window, the predicate returns `None`
and `revalidate` drops the entry. -/
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

/-- Until it lapses, a re-validated conflict's age is the read-time age: with
nothing written after `now₁`, at a later clock it is older by exactly the
elapsed time, never the frozen detection-time value. -/
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

end Sorted

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
theorem declaredAt_none {e : Edge A} (h : e.eventTime = none) : declaredAt e = e.createdAt := by
  simp [declaredAt, aboutTime, h]

omit [DecidableEq A] in
/-- The date is monotone in the write time and the about-time. -/
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

/-- `an_unrepresentable_min_age_makes_no_evidence_eligible`. -/
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
