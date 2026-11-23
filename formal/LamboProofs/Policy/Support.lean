import LamboProofs.Policy.Sessions

/-
# The Solo policy's terms: support on event time (`src/canon/policy.rs`, `stage1.rs`)

Models `supporting_interaction_times` and `recurrence_sessions`
(`src/canon/policy.rs`), the dependent counts `dependent_origin` and, since
#300, `solo_dependent_interactions` (`src/canon/stage1.rs`), over
`Interaction::about_time` and `Edge::about_time` (`src/types/mod.rs`) and the
writing turn `WriterTimeline` resolves (`src/daemon/conflict.rs`). The
dating before #300 (by the dependent's origin interaction: `supportingTimes`,
`recurrenceSessions`, `dependentOrigins`) is kept beside the fixed one
(`supportingTimesW`, `recurrenceSessionsW`, `dependentOriginsW`); Swarm's
Stage 1 count and Stage 2's span still read the origin.

Results: edges other than `Derives`/`Dependency` never change the recurrence
term (`causal_not_recurrence`) and no edge lowers it (`recurrence_mono_edge`);
with event times on every interaction flush times are irrelevant
(`supportingTimes_reflush`, `recurrence_reflush`); the Rust tests' corpora
(`a_bulk_ingest_recurs_only_under_event_time`,
`causal_touches_are_not_recurrence`, `recurring_burst_once`). Since #300 a
reused action's later dependency is dated by the turn that wrote it
(`reused_action_dependency_is_dated_by_its_writer`,
`own_origin_is_the_writing_turn`), and the fixed terms agree with the old
ones whenever every dependence was written in its dependent's origin turn
(`supportingTimesW_eq`, `recurrenceSessionsW_eq`, `dependentOriginsW_eq`).

Findings: a `parent_of` child counts as a dependent but never as a
recurrence session, an undocumented asymmetry with `Edge::dependence`
(`hierarchical_child_is_dependent_not_recurrence`); before #300 a reused
action's dependency was dated by its origin
(`reused_action_dependency_is_dated_by_its_origin`); and the test comments
disagreed with the test code (`spread_days_doc_and_code`,
`spread_days_on_the_boundary`).

Boundary: graphs are lists with unique ids, an edge's stamps are a function
of the edge, and how `WriterTimeline` finds the writer (an exact `created_at`
match) is an input. Not modelled: the dependent count's `min_age` floor and
graph walk (an input except in `dependentOrigins`), and Stage 2/3.
-/

namespace Lambo.Policy

open Lambo.Lib (nodup_eraseDups length_eraseDups_le_cons)

/-! ## Supporting interactions, resolved on event time

`supporting_interaction_times` and `recurrence_sessions` in
`src/canon/policy.rs`, and `Interaction::about_time` in `src/types/mod.rs`. -/

/-- `EdgeType` (`src/types/mod.rs`). -/
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

/-- An interaction: its id, its flush time `created_at` and its optional
event time, as instants in seconds. -/
structure Interaction where
  id : Nat
  createdAt : Int
  eventTime : Option Int
  deriving DecidableEq, Repr

/-- `Interaction::about_time`: the event time, or the flush time when the
ingester supplied none (the fallback rule). -/
def Interaction.aboutTime (i : Interaction) : Int := i.eventTime.getD i.createdAt

/-- A concept, with only what the support rule reads: its origin interaction. -/
structure ConceptNode where
  id : Nat
  origin : Nat
  deriving DecidableEq, Repr

structure Edge where
  src : Nat
  tgt : Nat
  ty : EdgeType
  deriving DecidableEq, Repr

structure Graph where
  interactions : List Interaction
  concepts : List ConceptNode
  edges : List Edge
  deriving DecidableEq, Repr

/-- `graph.node(id)` when it is an interaction. -/
def Graph.interaction (g : Graph) (id : Nat) : Option Interaction :=
  g.interactions.find? (fun i => i.id == id)

/-- `graph.node(id)` when it is a concept. -/
def Graph.concept (g : Graph) (id : Nat) : Option ConceptNode :=
  g.concepts.find? (fun c => c.id == id)

/-- The interaction an edge credits: the source of a `Derives` edge, the
origin interaction of the concept a `Dependency` edge starts from, and
nothing for every other edge type. -/
def writer (g : Graph) (e : Edge) : Option Nat :=
  match e.ty with
  | .derives => some e.src
  | .dependency => (g.concept e.src).map (·.origin)
  | _ => none

/-- The about-time one incident edge contributes to concept `c`. -/
def supportTime (g : Graph) (c : Nat) (e : Edge) : Option Int :=
  if e.tgt = c then (writer g e).bind (fun id => (g.interaction id).map (·.aboutTime)) else none

/-- `supporting_interaction_times`: sorted, deduplicated about-times. -/
def supportingTimes (g : Graph) (c : Nat) : List Int :=
  canon (g.edges.filterMap (supportTime g c))

/-- `recurrence_sessions`. -/
def recurrenceSessions (g : Graph) (c : Nat) : Nat :=
  sessionCount sessionSeparation (supportingTimes g c)

/-- Only `Derives` and `Dependency` edges into the concept are support. -/
theorem supportTime_none (g : Graph) (c : Nat) (e : Edge)
    (h : e.ty ≠ .derives ∧ e.ty ≠ .dependency) : supportTime g c e = none := by
  unfold supportTime writer
  cases hty : e.ty <;> simp_all

/-- `supportTime` reads only interactions and concepts, never the edge list. -/
theorem supportTime_edges (g : Graph) (es : List Edge) :
    supportTime { g with edges := es } = supportTime g := rfl

/-- An edge that is not support leaves the supporting times unchanged. -/
theorem supportingTimes_cons_none (g : Graph) (c : Nat) (e : Edge)
    (h : supportTime g c e = none) :
    supportingTimes { g with edges := e :: g.edges } c = supportingTimes g c := by
  unfold supportingTimes
  rw [supportTime_edges]
  simp [h]

/-- **Causal touches are not recurrence** (#36 PR 3, Rust test
`causal_touches_are_not_recurrence`): a `Causal` edge, from any action on any
day, leaves the recurrence term unchanged. So does every other non-support
type (`Contradicts`: test
`contradictions_do_not_supply_solo_support_or_dependents`). -/
theorem causal_not_recurrence (g : Graph) (c : Nat) (e : Edge)
    (h : e.ty ≠ .derives ∧ e.ty ≠ .dependency) :
    recurrenceSessions { g with edges := e :: g.edges } c = recurrenceSessions g c := by
  unfold recurrenceSessions
  rw [supportingTimes_cons_none g c e (supportTime_none g c e h)]

/-- Adding an edge to the graph never lowers a concept's
`recurrence_sessions`. -/
theorem recurrence_mono_edge (g : Graph) (c : Nat) (e : Edge) :
    recurrenceSessions g c ≤ recurrenceSessions { g with edges := e :: g.edges } c := by
  unfold recurrenceSessions
  apply sessionCount_mono
  intro x hx
  unfold supportingTimes at hx ⊢
  rw [mem_canon] at hx ⊢
  rw [supportTime_edges, List.filterMap_cons]
  split
  · exact hx
  · exact List.mem_cons_of_mem _ hx

/-- Re-stamp every interaction's flush time. -/
def Graph.reflush (g : Graph) (f : Nat → Int) : Graph :=
  { g with interactions := g.interactions.map (fun i => { i with createdAt := f i.id }) }

/-- Strip every event time: the pre-event-time reading of the same rows. -/
def Graph.strip (g : Graph) : Graph :=
  { g with interactions := g.interactions.map (fun i => { i with eventTime := none }) }

/-- `Interaction::about_time` falls back to the flush time (`created_at`) when
the interaction carries no event time. -/
theorem aboutTime_fallback (i : Interaction) (h : i.eventTime = none) :
    i.aboutTime = i.createdAt := by
  simp [Interaction.aboutTime, h]

/-- **Recurrence is measured on event time.** When every interaction carries
an event time, its flush time is irrelevant: re-stamping every flush time
(say, a bulk ingest flushing a decade inside a minute) leaves the supporting
times, and so the recurrence term, unchanged. -/
theorem supportingTimes_reflush (g : Graph) (f : Nat → Int) (c : Nat)
    (h : ∀ i, i ∈ g.interactions → i.eventTime.isSome) :
    supportingTimes (g.reflush f) c = supportingTimes g c := by
  have hst : supportTime (g.reflush f) c = supportTime g c := by
    funext e
    unfold supportTime
    have hw : writer (g.reflush f) e = writer g e := rfl
    rw [hw]
    split
    · cases writer g e with
      | none => rfl
      | some id =>
        simp only [Option.bind_some]
        unfold Graph.interaction Graph.reflush
        rw [List.find?_map]
        cases hf : (g.interactions.find? ((fun i : Interaction => i.id == id) ∘
            (fun i : Interaction => { i with createdAt := f i.id }))) with
        | none =>
          have : g.interactions.find? (fun i => i.id == id) = none := hf
          simp [this]
        | some i =>
          have hfi : g.interactions.find? (fun i => i.id == id) = some i := hf
          have hmem := List.mem_of_find?_eq_some hfi
          have hsome := h i hmem
          simp only [hfi, Option.map_some]
          congr 1
          cases he : i.eventTime with
          | none => rw [he] at hsome; cases hsome
          | some t => simp [Interaction.aboutTime, he]
    · rfl
  unfold supportingTimes
  rw [hst]
  rfl

/-- Once every interaction carries an event time, a re-flush (new flush times)
changes no `recurrence_sessions` count: the solo term reads
`Interaction::about_time`, not the flush time. -/
theorem recurrence_reflush (g : Graph) (f : Nat → Int) (c : Nat)
    (h : ∀ i, i ∈ g.interactions → i.eventTime.isSome) :
    recurrenceSessions (g.reflush f) c = recurrenceSessions g c := by
  unfold recurrenceSessions; rw [supportingTimes_reflush g f c h]

/-! ### The Rust test corpora -/

/-- `day(n)` in the tests: `ts(0)` plus `n` days. -/
def day (n : Nat) : Int := base + n * 86400

/-- `spread_support_graph_with(_, with_event_time)`: four interactions flushed
15 s apart; hub `10` derived from interaction 1 (`insert_concept` writes the
`Derives` edge) and re-derived by interactions 2, 3 and 4. The event time of
turn `n ≥ 2` is `day(n - 1) + 48h`, i.e. days 0, 3, 4 and 5. -/
def spreadGraph (withEvent : Bool) : Graph where
  interactions :=
    [⟨1, base, if withEvent then some (day 0) else none⟩,
     ⟨2, base + 15, if withEvent then some (day 1 + 48 * 3600) else none⟩,
     ⟨3, base + 30, if withEvent then some (day 2 + 48 * 3600) else none⟩,
     ⟨4, base + 45, if withEvent then some (day 3 + 48 * 3600) else none⟩]
  concepts := [⟨10, 1⟩]
  edges := [⟨1, 10, .derives⟩, ⟨2, 10, .derives⟩, ⟨3, 10, .derives⟩, ⟨4, 10, .derives⟩]

/-- **Event time, not flush time** (Rust test
`a_bulk_ingest_recurs_only_under_event_time`): the same four turns, flushed
inside one minute, are four sessions on event time and one on flush time. -/
theorem a_bulk_ingest_recurs_only_under_event_time :
    recurrenceSessions (spreadGraph true) 10 = 4 ∧
      recurrenceSessions (spreadGraph false) 10 = 1 ∧
      spreadGraph false = (spreadGraph true).strip := by
  decide

/-- Finding 3: the bulk-ingest test's comment gives the about-times as days
0/2/4/6 while `supporting_interaction_times` computes days 0/3/4/5; both count
four sessions in `separated_session_count`, so the test holds on either
reading. -/
theorem spread_days_doc_and_code :
    sessionCount sessionSeparation [day 0, day 2, day 4, day 6] = 4 ∧
      supportingTimes (spreadGraph true) 10 = [day 0, day 3, day 4, day 5] := by
  decide

/-- Rust test `causal_touches_are_not_recurrence`: concept `40`, derived in
turn 1, modified (`Causal`) by actions `42`, `43`, `44` from turns 2, 3, 4
has one session; the same actions declaring `depends_on` it give four. -/
def causalGraph (dependency : Bool) : Graph where
  interactions := (spreadGraph true).interactions
  concepts := [⟨10, 1⟩, ⟨40, 1⟩, ⟨42, 2⟩, ⟨43, 3⟩, ⟨44, 4⟩]
  edges := (spreadGraph true).edges ++ [⟨1, 40, .derives⟩,
    ⟨42, 40, .causal⟩, ⟨43, 40, .causal⟩, ⟨44, 40, .causal⟩] ++
    (if dependency then [⟨42, 40, .dependency⟩, ⟨43, 40, .dependency⟩, ⟨44, 40, .dependency⟩]
     else [])

/-- The `<=` mutant of the loop (`Some(a) if t - a <= gap => {}`). -/
def greedyLeMutant (gap : Nat) : Option Int → List Int → Nat
  | _, [] => 0
  | none, t :: rest => greedyLeMutant gap (some t) rest + 1
  | some a, t :: rest =>
    if t - a ≤ (gap : Int) then greedyLeMutant gap (some a) rest
    else greedyLeMutant gap (some t) rest + 1

/-- Days 3, 4 and 5 are exactly one separation apart, so the bulk-ingest
corpus sits on the inclusive boundary: the `<=` mutant counts 3, not 4
(finding 3). -/
theorem spread_days_on_the_boundary :
    day 4 - day 3 = sessionSeparation ∧ day 5 - day 4 = sessionSeparation ∧
      greedyLeMutant sessionSeparation none (supportingTimes (spreadGraph true) 10) = 3 := by
  decide

/-- Rust test `causal_touches_are_not_recurrence`: `Causal` modifications of a
concept by later turns add no recurrence session, while the same actions
declaring `depends_on` it count four. -/
theorem causal_touches_are_not_recurrence :
    recurrenceSessions (causalGraph false) 40 = 1 ∧ recurrenceSessions (causalGraph true) 40 = 4 := by
  decide

/-- Rust test `solo_admits_the_recurring_fact_and_refuses_the_burst_and_the_one_off`:
`recurring` (10) re-derived on the spread days, `burst` (11) re-derived as
often within one afternoon of day 0, `once` (12) derived once. Equal support
counts, four sessions against one. -/
def mixedGraph : Graph where
  interactions := (spreadGraph true).interactions ++
    [⟨5, base + 60, some (day 0 + 20 * 60)⟩, ⟨6, base + 75, some (day 0 + 40 * 60)⟩,
     ⟨7, base + 90, some (day 0 + 60 * 60)⟩]
  concepts := [⟨10, 1⟩, ⟨11, 1⟩, ⟨12, 1⟩]
  edges := (spreadGraph true).edges ++
    [⟨1, 11, .derives⟩, ⟨5, 11, .derives⟩, ⟨6, 11, .derives⟩, ⟨7, 11, .derives⟩,
     ⟨1, 12, .derives⟩]

/-- In `recurrence_sessions`, a concept re-derived on four spread days counts
four sessions, while one re-derived as often in a single burst counts one (the
graph of Rust test
`solo_admits_the_recurring_fact_and_refuses_the_burst_and_the_one_off`). -/
theorem recurring_burst_once :
    (supportingTimes mixedGraph 10).length = (supportingTimes mixedGraph 11).length ∧
      recurrenceSessions mixedGraph 10 = 4 ∧ recurrenceSessions mixedGraph 11 = 1 ∧
      recurrenceSessions mixedGraph 12 = 1 := by
  decide

/-! ### Dependence that is not recurrence

`EdgeType::dependent_end` says it is "the definition every implementation
follows" of which edges declare a dependence: `Dependency` (the source rests
on the target) and `Hierarchical` (the child, the target, rests on the
parent). The solo score's dependents term reads it (through
`dependent_interactions`); `supporting_interaction_times` does not: it credits
`Dependency` edges into the concept and ignores `Hierarchical` ones. Below,
`dependentOrigins` is `dependent_interactions` with every edge and interaction
taken as old enough (the `min_age` floor is not modelled). -/

/-- `Edge::dependence`: `(pillar, dependent)`, none for a self-loop. -/
def Edge.dependence (e : Edge) : Option (Nat × Nat) :=
  if e.src = e.tgt then none
  else match e.ty with
    | .dependency => some (e.tgt, e.src)
    | .hierarchical => some (e.src, e.tgt)
    | _ => none

/-- The distinct origin interactions of `c`'s dependents, other than `c`'s own
origin: `dependent_interactions` with the age floor cleared. -/
def dependentOrigins (g : Graph) (c : Nat) : List Nat :=
  (g.edges.filterMap fun e =>
    match e.dependence with
    | some (p, d) =>
      if p = c then
        match g.concept p, g.concept d with
        | some pc, some dc =>
          if dc.origin = pc.origin then none
          else (g.interaction dc.origin).map (·.id)
        | _, _ => none
      else none
    | none => none).eraseDups

/-- Concept `10` of the spread corpus gains a child `50` (`parent_of`: a
`Hierarchical` edge `10 → 50`) derived in a fifth turn about day 9. The child
is a dependent interaction (2.0 more in the score), but its turn is not a
supporting session: recurrence stays at four, where a `Dependency` edge
`50 → 10` from the same turn makes it five. -/
def childGraph (hierarchical : Bool) : Graph where
  interactions := (spreadGraph true).interactions ++ [⟨5, base + 60, some (day 9)⟩]
  concepts := [⟨10, 1⟩, ⟨50, 5⟩]
  edges := (spreadGraph true).edges ++ [⟨5, 50, .derives⟩,
    if hierarchical then ⟨10, 50, .hierarchical⟩ else ⟨50, 10, .dependency⟩]

/-- A hierarchical child written in another turn counts toward its parent's
dependent origins (`dependent_origin`) but adds no recurrence session, while a
`Dependency` from it adds both (`supporting_interaction_times`,
`recurrence_sessions`). -/
theorem hierarchical_child_is_dependent_not_recurrence :
    dependentOrigins (childGraph true) 10 = [5] ∧ recurrenceSessions (childGraph true) 10 = 4 ∧
      dependentOrigins (childGraph false) 10 = [5] ∧ recurrenceSessions (childGraph false) 10 = 5 := by
  decide

/-- Finding 2. Turn 1 (day 0) derives `X` (60), `Y` (61) and the action
"deploy api" (62), which depends on `X`. On day 5 a turn repeats the action,
`record_action` reuses node 62, and it now also depends on `Y`. With
`reused = true` the new edge `62 → 61` is credited to turn 1: `Y` gets one
session and no dependent. With `reused = false` the day-5 turn creates a
fresh action node (63), and `Y` gets two sessions and one dependent. -/
def reusedActionGraph (reused : Bool) : Graph where
  interactions := [⟨1, base, some (day 0)⟩, ⟨5, base + 60, some (day 5)⟩]
  concepts := [⟨60, 1⟩, ⟨61, 1⟩, ⟨62, 1⟩] ++ (if reused then [] else [⟨63, 5⟩])
  edges := [⟨1, 60, .derives⟩, ⟨1, 61, .derives⟩, ⟨1, 62, .derives⟩, ⟨62, 60, .dependency⟩] ++
    (if reused then [⟨62, 61, .dependency⟩] else [⟨5, 63, .derives⟩, ⟨63, 61, .dependency⟩])

/-- A `depends_on` from an action reused across turns is dated by the reused
action's origin turn, so it adds no supporting time, recurrence session or
dependent origin to its target; a new action written in a later turn adds
them. -/
theorem reused_action_dependency_is_dated_by_its_origin :
    supportingTimes (reusedActionGraph true) 61 = [day 0] ∧
      recurrenceSessions (reusedActionGraph true) 61 = 1 ∧
      dependentOrigins (reusedActionGraph true) 61 = [] ∧
      supportingTimes (reusedActionGraph false) 61 = [day 0, day 5] ∧
      recurrenceSessions (reusedActionGraph false) 61 = 2 ∧
      dependentOrigins (reusedActionGraph false) 61 = [5] := by
  decide

/-! ### The fixed dating (#300)

Since #300 the Solo policy's two time-spread terms date a dependence edge by
the turn that **wrote** it, not by its dependent's origin interaction
(`event_time.rs`: "An edge is about what the turn that wrote it is about").
`writer`, `supportingTimes`, `recurrenceSessions` and `dependentOrigins`
above stay as the previous version; this section is the fixed one.

The Rust edge carries, or lets the graph resolve, two stamps the structural
`Edge` above leaves out. They are modelled as a function of the edge, which
is a function of the row because a graph holds one edge per source, target
and type (`Graph`'s edge key):

* `time`: `Edge::about_time`, stamped from the writing interaction's clock
  when the edge is created (`record_action`, `derive`'s `depends_on` and
  `parent_of` all copy the interaction's `created_at` and `event_time`);
* `writer`: the writing interaction as `WriterTimeline` finds it, the one
  interaction flushed at exactly the edge's `created_at`; `none` when no
  interaction or several were (a hand-built or rebased edge, or a
  same-instant collision), and then the dependent's origin is credited, as
  before (the structural fallback `attribute_at` uses too).

A repeated identical citation reinforces the existing edge and keeps both
stamps, so it is not re-dated: no writer history is kept, and none is
invented. Only a new edge is new evidence. -/

/-- The stamps of one edge: its about-time and its resolved writer. -/
structure Stamp where
  time : Int
  writer : Option Nat
  deriving DecidableEq, Repr

/-- The fixed support time: a `Derives` edge as before (its source is the
writing interaction), a `Dependency` edge from a concept by its own
about-time. -/
def supportTimeW (g : Graph) (st : Edge → Stamp) (c : Nat) (e : Edge) : Option Int :=
  if e.tgt = c then
    match e.ty with
    | .derives => (g.interaction e.src).map (·.aboutTime)
    | .dependency => (g.concept e.src).map (fun _ => (st e).time)
    | _ => none
  else none

/-- The fixed `supporting_interaction_times`. -/
def supportingTimesW (g : Graph) (st : Edge → Stamp) (c : Nat) : List Int :=
  canon (g.edges.filterMap (supportTimeW g st c))

/-- The fixed `recurrence_sessions`. -/
def recurrenceSessionsW (g : Graph) (st : Edge → Stamp) (c : Nat) : Nat :=
  sessionCount sessionSeparation (supportingTimesW g st c)

/-- The interaction a dependence edge is credited to: its resolved writer,
else (no unique attribution) its dependent's origin. -/
def credited (st : Edge → Stamp) (e : Edge) (dc : ConceptNode) : Nat :=
  (st e).writer.getD dc.origin

/-- The interaction one edge credits to `c` under the fixed rule, if any
(`dependent_origin` with `Credit::Writer`, age floor cleared). -/
def depOriginW (g : Graph) (st : Edge → Stamp) (c : Nat) (e : Edge) : Option Nat :=
  match e.dependence with
  | some (p, d) =>
    if p = c then
      match g.concept p, g.concept d with
      | some pc, some dc =>
        if credited st e dc = pc.origin then none
        else (g.interaction (credited st e dc)).map (·.id)
      | _, _ => none
    else none
  | none => none

/-- The fixed Solo dependents term (`solo_dependent_interactions`, age floor
cleared): the distinct interactions that wrote `c`'s dependence edges, other
than `c`'s own origin. -/
def dependentOriginsW (g : Graph) (st : Edge → Stamp) (c : Nat) : List Nat :=
  (g.edges.filterMap (depOriginW g st c)).eraseDups

/-- The dependent end of a dependence edge, self-loops included: the source
of a `Dependency`, the child (target) of a `Hierarchical`. -/
def dependentEnd (e : Edge) : Option Nat :=
  match e.ty with
  | .dependency => some e.src
  | .hierarchical => some e.tgt
  | _ => none

theorem dependence_dependentEnd {e : Edge} {p d : Nat} (h : e.dependence = some (p, d)) :
    dependentEnd e = some d := by
  unfold Edge.dependence at h
  unfold dependentEnd
  split at h
  · cases h
  · cases hty : e.ty <;> rw [hty] at h <;> simp_all

/-- **The common case**: every dependence edge was written in its dependent's
origin turn, so it is attributed to that turn and carries its about-time. -/
def WrittenAtOrigin (g : Graph) (st : Edge → Stamp) : Prop :=
  ∀ e, e ∈ g.edges → ∀ d dc, dependentEnd e = some d → g.concept d = some dc →
    ∃ i, g.interaction dc.origin = some i ∧ st e = ⟨i.aboutTime, some dc.origin⟩

/-- `filterMap` with functions that agree on the list. -/
theorem filterMap_congr' {α β : Type} {f f' : α → Option β} :
    ∀ l : List α, (∀ x, x ∈ l → f x = f' x) → l.filterMap f = l.filterMap f'
  | [], _ => rfl
  | x :: xs, h => by
    rw [List.filterMap_cons, List.filterMap_cons, h x (by simp),
      filterMap_congr' xs (fun y hy => h y (List.mem_cons_of_mem _ hy))]

theorem supportTimeW_eq (g : Graph) (st : Edge → Stamp) (c : Nat) (e : Edge)
    (he : e ∈ g.edges) (h : WrittenAtOrigin g st) :
    supportTimeW g st c e = supportTime g c e := by
  unfold supportTimeW supportTime writer
  split
  · cases hty : e.ty with
    | derives => simp
    | dependency =>
      cases hc : g.concept e.src with
      | none => simp
      | some sc =>
        obtain ⟨i, hi, hst⟩ := h e he e.src sc (by simp [dependentEnd, hty]) hc
        simp [hi, hst]
    | _ => simp
  · rfl

/-- **Agreement with the old dating** in the common case: when every
dependence edge was written in its dependent's origin turn, the fixed
supporting times, and so the recurrence term, are the old ones. -/
theorem supportingTimesW_eq (g : Graph) (st : Edge → Stamp) (c : Nat)
    (h : WrittenAtOrigin g st) : supportingTimesW g st c = supportingTimes g c := by
  unfold supportingTimesW supportingTimes
  congr 1
  exact filterMap_congr' _ (fun e he => supportTimeW_eq g st c e he h)

/-- When every dependence edge was written in its dependent's origin turn,
#300's writer dating gives `recurrence_sessions` the same count as the origin
dating did. -/
theorem recurrenceSessionsW_eq (g : Graph) (st : Edge → Stamp) (c : Nat)
    (h : WrittenAtOrigin g st) : recurrenceSessionsW g st c = recurrenceSessions g c := by
  unfold recurrenceSessionsW recurrenceSessions; rw [supportingTimesW_eq g st c h]

/-- ... and the fixed dependents term is the old one. -/
theorem dependentOriginsW_eq (g : Graph) (st : Edge → Stamp) (c : Nat)
    (h : WrittenAtOrigin g st) : dependentOriginsW g st c = dependentOrigins g c := by
  unfold dependentOriginsW dependentOrigins
  congr 1
  apply filterMap_congr'
  intro e he
  unfold depOriginW
  cases hd : e.dependence with
  | none => rfl
  | some pd =>
    obtain ⟨p, d⟩ := pd
    simp only []
    split
    · cases hp : g.concept p with
      | none => rfl
      | some pc =>
        cases hdc : g.concept d with
        | none => rfl
        | some dc =>
          obtain ⟨i, _, hst⟩ := h e he d dc (dependence_dependentEnd hd) hdc
          simp [credited, hst]
    · rfl

theorem interaction_id {g : Graph} {x w : Nat} (h : (g.interaction x).map (·.id) = some w) :
    w = x := by
  unfold Graph.interaction at h
  cases hf : g.interactions.find? (fun i => i.id == x) with
  | none => rw [hf] at h; cases h
  | some i =>
    rw [hf] at h
    have := List.find?_some hf
    simp only [Option.map_some, Option.some.injEq] at h
    simp only [beq_iff_eq] at this
    omega

/-- **Own origin is the writing turn**: no interaction the fixed dependents
term counts is the pillar's own origin, whichever concept the edge starts
from. -/
theorem dependentOriginsW_not_own (g : Graph) (st : Edge → Stamp) (c : Nat) (pc : ConceptNode)
    (hc : g.concept c = some pc) (w : Nat) (hw : w ∈ dependentOriginsW g st c) :
    w ≠ pc.origin := by
  unfold dependentOriginsW at hw
  rw [List.mem_eraseDups, List.mem_filterMap] at hw
  obtain ⟨e, _, he⟩ := hw
  revert he
  unfold depOriginW
  cases e.dependence with
  | none => simp
  | some pd =>
    obtain ⟨p, d⟩ := pd
    simp only []
    split
    · rename_i hpc
      subst hpc
      rw [hc]
      cases g.concept d with
      | none => simp
      | some dc =>
        simp only []
        split
        · simp
        · rename_i hne
          intro hm
          rw [interaction_id hm]
          exact hne
    · simp

/-- **A dependency written in a later turn supports on that turn's date**:
the edge's own about-time is one of the supporting times. -/
theorem dependency_time_supports (g : Graph) (st : Edge → Stamp) (c : Nat) (e : Edge)
    (he : e ∈ g.edges) (hty : e.ty = .dependency) (ht : e.tgt = c) (sc : ConceptNode)
    (hs : g.concept e.src = some sc) : (st e).time ∈ supportingTimesW g st c := by
  unfold supportingTimesW
  rw [mem_canon, List.mem_filterMap]
  exact ⟨e, he, by simp [supportTimeW, ht, hty, hs]⟩

theorem supportTimeW_edges (g : Graph) (st : Edge → Stamp) (es : List Edge) :
    supportTimeW { g with edges := es } st = supportTimeW g st := rfl

/-- Under the fixed dating, adding any edge still never lowers the
recurrence term ... -/
theorem recurrenceW_mono_edge (g : Graph) (st : Edge → Stamp) (c : Nat) (e : Edge) :
    recurrenceSessionsW g st c ≤ recurrenceSessionsW { g with edges := e :: g.edges } st c := by
  unfold recurrenceSessionsW
  apply sessionCount_mono
  intro x hx
  unfold supportingTimesW at hx ⊢
  rw [mem_canon] at hx ⊢
  rw [supportTimeW_edges, List.filterMap_cons]
  split
  · exact hx
  · exact List.mem_cons_of_mem _ hx

/-- ... and causal touches are still not recurrence. -/
theorem causal_not_recurrenceW (g : Graph) (st : Edge → Stamp) (c : Nat) (e : Edge)
    (h : e.ty ≠ .derives ∧ e.ty ≠ .dependency) :
    recurrenceSessionsW { g with edges := e :: g.edges } st c = recurrenceSessionsW g st c := by
  have hn : supportTimeW g st c e = none := by
    unfold supportTimeW
    split
    · cases hty : e.ty <;> simp_all
    · rfl
  unfold recurrenceSessionsW supportingTimesW
  rw [supportTimeW_edges]
  simp [hn]

theorem depOriginW_edges (g : Graph) (st : Edge → Stamp) (es : List Edge) :
    depOriginW { g with edges := es } st = depOriginW g st := rfl

/-- Adding any edge never lowers the fixed dependents term: an edge's
stamps are its own, so the other edges credit what they credited. -/
theorem dependentOriginsW_mono_edge (g : Graph) (st : Edge → Stamp) (c : Nat) (e : Edge) :
    (dependentOriginsW g st c).length ≤
      (dependentOriginsW { g with edges := e :: g.edges } st c).length := by
  unfold dependentOriginsW
  rw [depOriginW_edges, List.filterMap_cons]
  split
  · exact Nat.le_refl _
  · exact length_eraseDups_le_cons _ _

/-- The stamps of the reused-action corpus: the day-5 turn (5) wrote the
edges into `Y` (`62 → 61`, and `63 → 61` from the fresh node); turn 1 wrote
the rest. -/
def reusedStamp (e : Edge) : Stamp :=
  if e.tgt = 61 ∧ (e.src = 62 ∨ e.src = 63) then ⟨day 5, some 5⟩ else ⟨day 0, some 1⟩

/-- **The fix on the issue's scenario.** Dated by its writing turn, the
reused action's day-5 dependency on `Y` adds the day-5 session and the
day-5 turn as a dependent interaction, exactly as a fresh action node from
that turn does. `X`, cited only by turn 1 (its own origin), is unchanged. -/
theorem reused_action_dependency_is_dated_by_its_writer :
    supportingTimesW (reusedActionGraph true) reusedStamp 61 = [day 0, day 5] ∧
      recurrenceSessionsW (reusedActionGraph true) reusedStamp 61 = 2 ∧
      dependentOriginsW (reusedActionGraph true) reusedStamp 61 = [5] ∧
      supportingTimesW (reusedActionGraph false) reusedStamp 61 = [day 0, day 5] ∧
      recurrenceSessionsW (reusedActionGraph false) reusedStamp 61 = 2 ∧
      dependentOriginsW (reusedActionGraph false) reusedStamp 61 = [5] ∧
      recurrenceSessionsW (reusedActionGraph true) reusedStamp 60 = 1 ∧
      dependentOriginsW (reusedActionGraph true) reusedStamp 60 = [] := by
  decide

/-- The other direction of the own-origin rule. Turn 0 records action `70`;
turn 1 derives `Y` (71) and repeats the action, which now depends on `Y`.
The old rule credits the action's origin, turn 0, as a dependent
interaction; the fixed rule sees that `Y`'s own turn wrote the dependence
and counts nothing (and dates the support on day 1, not day 0). -/
def ownTurnGraph : Graph where
  interactions := [⟨0, base, some (day 0)⟩, ⟨1, base + 60, some (day 1)⟩]
  concepts := [⟨70, 0⟩, ⟨71, 1⟩]
  edges := [⟨0, 70, .derives⟩, ⟨1, 71, .derives⟩, ⟨70, 71, .dependency⟩]

def ownTurnStamp (e : Edge) : Stamp :=
  if e.src = 70 then ⟨day 1, some 1⟩ else ⟨day 0, some 0⟩

/-- #300: a dependence edge written in its dependent's origin turn, dated by
the turn that wrote it (`WriterTimeline`), adds no Solo dependent interaction
and no extra supporting time, where the origin dating counted both. -/
theorem own_origin_is_the_writing_turn :
    dependentOrigins ownTurnGraph 71 = [0] ∧ dependentOriginsW ownTurnGraph ownTurnStamp 71 = [] ∧
      supportingTimes ownTurnGraph 71 = [day 0, day 1] ∧
      supportingTimesW ownTurnGraph ownTurnStamp 71 = [day 1] := by
  decide

end Lambo.Policy
