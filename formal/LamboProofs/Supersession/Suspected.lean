import LamboProofs.Canon

/-
# Suspected supersession (#198) and replacement candidates

Models the warning-only `SuspectedSupersedes` trigger
(`src/graph/suspected.rs`, `suspected/slot.rs`): `eligible_type`,
`repo_scope`, `dated_at`, `settled`, `is_suspected_pair`, `candidate`,
`record`, `record_refused_hits` and `settle`; and the replacement candidates
a derive returns (`src/graph/suspected/replacement.rs`: `eligible_pair`,
`select` with its floor, clamp, skip, dedup, ranking, cap and the `newer` and
`merged` marks), before (`keep`, `select`) and after #302's fix (`keepF`,
`selectF`, which keeps a repeated id's highest score).

Results: a concept is never paired with itself, the pair does not depend on
argument order, and the strictly later-dated concept is the suspected
replacement, a tie recording nothing (`candidate_self`, `candidate_symm`,
`candidate_dir`, `candidate_tie`); the trigger adds only `SuspectedSupersedes`
edges, later to earlier, so the relation is acyclic (`recordRefused_extends`,
`suspected_acyclic`); a replacement candidate is an eligible concept at or
above the floor, at most `REPLACEMENT_CANDIDATE_LIMIT` of them
(`select_mem`, `select_length`), and one marked `newer` is never one a later
call could record as replaced by the new concept, the rule "never cite a
`newer` candidate in `against`" rests on (`select_newer_never_replaced_by_new`);
since #302 the candidates do not depend on the hits' order or repetition
(`selectF_order_independent`).

Boundary: cosines are fixed-point integers (`one = 10000`) and given; the text
rule (`one_span_value_slot`) is an abstract predicate `slot` and the
identifier guard equality of an abstract signature; one session, with edges
unique per `(source, target, type)`. Not modelled: `scan`, `pair_stamp`,
`settle_edges`, the receipt report and the excerpt.
-/

namespace Lambo.Supersession

open Lambo.Canon (Status legal gate)

/-! ## Graph pieces shared by both mechanisms -/

/-- `ConceptType` (`src/types/mod.rs`). -/
inductive CType where
  | logic
  | constraint
  | observation
  | resource
  | entity
  deriving DecidableEq, Repr

/-- `eligible_type` (`src/graph/suspected.rs`): `Logic`, `Constraint` and
`Observation` only. -/
def eligibleType : CType → Bool
  | .logic => true
  | .constraint => true
  | .observation => true
  | .resource => false
  | .entity => false

/-- The edge types this file distinguishes (`EdgeType`); every other type is
`other`. -/
inductive EType where
  | contradicts
  | dependency
  | suspected
  | semantic
  | other
  deriving DecidableEq, Repr

/-- An edge: id, ends, type and write time (`created_at`). -/
structure Edge where
  id : Nat
  src : Nat
  dst : Nat
  ty : EType
  created : Int
  deriving DecidableEq, Repr

/-- `graph.edge_between(a, b, ty).is_some()`. -/
def edgeBetween (E : List Edge) (a b : Nat) (t : EType) : Bool :=
  E.any fun e => e.src == a && e.dst == b && e.ty == t

/-- An edge of type `t` between `a` and `b`, either way. -/
def joined (E : List Edge) (t : EType) (a b : Nat) : Bool :=
  edgeBetween E a b t || edgeBetween E b a t

/-- `settled`: an `against` (`Contradicts`) or `depends_on` (`Dependency`)
edge joins the two, either way (`SETTLING`). -/
def settled (E : List Edge) (a b : Nat) : Bool :=
  joined E .contradicts a b || joined E .dependency a b

theorem joined_symm (E : List Edge) (t : EType) (a b : Nat) :
    joined E t a b = joined E t b a := by
  unfold joined; exact Bool.or_comm _ _

theorem settled_symm (E : List Edge) (a b : Nat) : settled E a b = settled E b a := by
  unfold settled; rw [joined_symm E .contradicts, joined_symm E .dependency]

/-- `is_suspected_pair`: a `SuspectedSupersedes` edge either way. -/
def suspectedPair (E : List Edge) (a b : Nat) : Bool := joined E .suspected a b

theorem suspectedPair_symm (E : List Edge) (a b : Nat) :
    suspectedPair E a b = suspectedPair E b a := joined_symm E _ a b

/-! ## Suspected supersession (#198): the pair condition

A concept is reduced to what `candidate` reads. `content` stands for the text
the slot rule reads, `idents` for its identifier signature (`same_identifiers`
compares digit runs and identifier tokens for equality, so it is equality of
a signature), and `dated` for `dated_at`: the origin interaction's about-time
clamped to its write time, or the concept's write time. -/

/-- `dated_at` from its parts: the origin interaction's about-time (its
`event_time`, else its write time) clamped to its write time. A concept whose
origin is not in the graph is dated by its own write time; interactions are
append-only, so a concept's date never changes. -/
def datedAt (written : Int) (eventTime : Option Int) : Int := min (eventTime.getD written) written

/-- A fact is never dated after it was written: a future `event_time` does
not make a write the newer one. -/
theorem datedAt_le (written : Int) (eventTime : Option Int) : datedAt written eventTime ≤ written :=
  Int.min_le_right _ _

/-- A backfilled `event_time` (at or before the write) is the date. -/
theorem datedAt_backfill (written t : Int) (h : t ≤ written) : datedAt written (some t) = t := by
  simp [datedAt]; omega

/-- What `candidate` and `select` read of a concept. -/
structure Concept where
  id : Nat
  session : Nat
  ty : CType
  /-- `repo_scope`: `None` for every eligible type today. -/
  scope : Option Nat
  /-- The identifier signature `same_identifiers` compares. -/
  idents : Nat
  /-- The text, abstract: only the slot rule reads it. -/
  content : Nat
  /-- `dated_at`, an integer instant. -/
  dated : Int
  /-- `embedding_source.is_some()`: an image concept. -/
  image : Bool
  deriving DecidableEq, Repr

/-- One suspected pair: `newer` is suspected of replacing `older`. -/
structure Pair where
  newer : Nat
  older : Nat
  deriving DecidableEq, Repr

/-- The guard of `candidate`, in the Rust's order: the same concept, another
session, another type, an ineligible type, another scope, the identifier
guard would have merged them, or a declared relation settles them. -/
def blocked (E : List Edge) (a b : Concept) : Bool :=
  a.id == b.id || a.session != b.session || a.ty != b.ty || !eligibleType a.ty ||
    a.scope != b.scope || a.idents == b.idents || settled E a.id b.id

/-- `candidate`: every condition of the trigger but the vector one. `slot o n`
is `one_span_value_slot(older, newer)` on the two texts, an abstract
predicate. The direction is `ta.cmp(&tb)`: the later-dated concept is the
suspected replacement, and a tie records nothing. -/
def candidate (slot : Nat → Nat → Bool) (E : List Edge) (a b : Concept) : Option Pair :=
  if blocked E a b then none
  else if b.dated < a.dated then
    if slot b.content a.content then some ⟨a.id, b.id⟩ else none
  else if a.dated < b.dated then
    if slot a.content b.content then some ⟨b.id, a.id⟩ else none
  else none

theorem blocked_symm (E : List Edge) (a b : Concept) : blocked E a b = blocked E b a := by
  unfold blocked
  rw [settled_symm E a.id b.id]
  by_cases hty : a.ty = b.ty
  · rw [hty]
    rw [BEq.comm (a := a.id), bne_comm (a := a.session), bne_comm (a := a.scope),
      BEq.comm (a := a.idents)]
  · have h1 : (a.ty != b.ty) = true := by simp [hty]
    have h2 : (b.ty != a.ty) = true := by simp [Ne.symm hty]
    simp [h1, h2]

/-- The #198 trigger (`candidate`) never suspects a concept of superseding
itself. -/
theorem candidate_self (slot : Nat → Nat → Bool) (E : List Edge) (a : Concept) :
    candidate slot E a a = none := by
  simp [candidate, blocked]

/-- The pair does not depend on which concept is the write's: the trigger at
write time and the scan over a store (`scan`, which meets a pair in id order)
agree. -/
theorem candidate_symm (slot : Nat → Nat → Bool) (E : List Edge) (a b : Concept) :
    candidate slot E a b = candidate slot E b a := by
  unfold candidate
  rw [blocked_symm E a b]
  cases blocked E b a
  · simp only [Bool.false_eq_true, ite_false]
    by_cases h1 : b.dated < a.dated
    · have h2 : ¬ a.dated < b.dated := by omega
      simp [h1, h2]
    · by_cases h2 : a.dated < b.dated
      · simp [h1, h2]
      · simp [h1, h2]
  · rfl

/-- **Direction by the dates.** A recorded pair runs from the strictly
later-dated concept to the earlier one ("the later one is the suspected
replacement"). -/
theorem candidate_dir (slot : Nat → Nat → Bool) (E : List Edge) (a b : Concept) (p : Pair)
    (h : candidate slot E a b = some p) :
    (p.newer = a.id ∧ p.older = b.id ∧ b.dated < a.dated) ∨
      (p.newer = b.id ∧ p.older = a.id ∧ a.dated < b.dated) := by
  unfold candidate at h
  cases hb : blocked E a b
  · simp only [hb, Bool.false_eq_true, ite_false] at h
    by_cases h1 : b.dated < a.dated
    · simp only [h1, ite_true] at h
      by_cases hs : slot b.content a.content
      · simp only [hs, ite_true, Option.some.injEq] at h
        subst h; exact Or.inl ⟨rfl, rfl, h1⟩
      · simp [hs] at h
    · by_cases h2 : a.dated < b.dated
      · simp only [h1, h2, ite_true, ite_false] at h
        by_cases hs : slot a.content b.content
        · simp only [hs, ite_true, Option.some.injEq] at h
          subst h; exact Or.inr ⟨rfl, rfl, h2⟩
        · simp [hs] at h
      · simp [h1, h2] at h
  · simp [hb] at h

/-- **A tie records nothing** ("a tie records nothing", test `direction_and_ties`). -/
theorem candidate_tie (slot : Nat → Nat → Bool) (E : List Edge) (a b : Concept)
    (h : a.dated = b.dated) : candidate slot E a b = none := by
  cases hp : candidate slot E a b with
  | none => rfl
  | some p =>
    rcases candidate_dir slot E a b p hp with ⟨_, _, h'⟩ | ⟨_, _, h'⟩ <;> omega

/-- A recorded pair passed every condition of the trigger. -/
theorem candidate_conditions (slot : Nat → Nat → Bool) (E : List Edge) (a b : Concept) (p : Pair)
    (h : candidate slot E a b = some p) :
    a.id ≠ b.id ∧ a.session = b.session ∧ a.ty = b.ty ∧ eligibleType a.ty = true ∧
      a.scope = b.scope ∧ a.idents ≠ b.idents ∧ settled E a.id b.id = false := by
  unfold candidate at h
  cases hb : blocked E a b
  · simp only [blocked, Bool.or_eq_false_iff] at hb
    obtain ⟨⟨⟨⟨⟨⟨h1, h2⟩, h3⟩, h4⟩, h5⟩, h6⟩, h7⟩ := hb
    simp at h1 h2 h3 h4 h5 h6
    exact ⟨h1, h2, h3, h4, h5, h6, h7⟩
  · simp [hb] at h

/-- A settled pair (an `against` or `depends_on` either way) is never
suspected. -/
theorem candidate_settled (slot : Nat → Nat → Bool) (E : List Edge) (a b : Concept)
    (h : settled E a.id b.id = true) : candidate slot E a b = none := by
  cases hp : candidate slot E a b with
  | none => rfl
  | some p =>
    have := (candidate_conditions slot E a b p hp).2.2.2.2.2.2
    rw [h] at this; cases this

/-! ## Recording the edge -/

/-- `record`: write `newer -SuspectedSupersedes-> older` unless it is there.
The new edge's id and write time come from the write (`Stamp`). -/
def record (E : List Edge) (p : Pair) (id : Nat) (at_ : Int) : List Edge :=
  if edgeBetween E p.newer p.older .suspected then E
  else E ++ [⟨id, p.newer, p.older, .suspected, at_⟩]

/-- Insert into an ascending list of naturals, dropping a duplicate. -/
def insertDedup (x : Nat) : List Nat → List Nat
  | [] => [x]
  | y :: ys => if x < y then x :: y :: ys else if x = y then y :: ys else y :: insertDedup x ys

/-- `sort_by_key(id)` then `dedup`. -/
def sortDedup (l : List Nat) : List Nat := l.foldr insertDedup []

/-- One hit of `record_refused_hits`: record the `candidate` pair of the
created concept `new` and the hit, if any. `look` resolves an id to a concept
of the session (`None`: no longer a concept), `fresh k` is the id of the
`k`-th edge the write mints, `at_` the write's time. -/
def step (slot : Nat → Nat → Bool) (look : Nat → Option Concept) (new : Concept)
    (fresh : Nat → Nat) (at_ : Int) (acc : List Edge) (hit : Nat) : List Edge :=
  match look hit with
  | some old =>
    match candidate slot acc new old with
    | some p => record acc p (fresh acc.length) at_
    | none => acc
  | none => acc

/-- `record_refused_hits`: for the created concept `new` and the vector hits
the identifier guard refused at or above the merge threshold, record every
`candidate` pair, hits judged in id order. -/
def recordRefused (slot : Nat → Nat → Bool) (look : Nat → Option Concept) (new : Concept)
    (fresh : Nat → Nat) (at_ : Int) (E : List Edge) (refused : List Nat) : List Edge :=
  (sortDedup refused).foldl (step slot look new fresh at_) E

/-- The vector condition the write path supplies: the hits at or above the
session's effective merge threshold that the identifier guard refused
(`guardRefused`, `same_identifiers` false). `candidate` checks the guard
again. -/
def refusedHits (threshold : Int) (guardRefused : Nat → Bool) (hits : List (Nat × Int)) : List Nat :=
  (hits.filter fun h => decide (threshold ≤ h.2) && guardRefused h.1).map (·.1)

/-- Every suspected edge runs from a strictly later-dated concept to an
earlier one, by the date function `date` (`dated_at` by id). -/
def WellDated (date : Nat → Int) (E : List Edge) : Prop :=
  ∀ e ∈ E, e.ty = .suspected → date e.dst < date e.src

/-- `look` and the created concept agree with `date`. -/
def DatesAgree (date : Nat → Int) (look : Nat → Option Concept) (new : Concept) : Prop :=
  new.dated = date new.id ∧ ∀ i c, look i = some c → c.dated = date c.id

theorem record_wellDated (date : Nat → Int) (E : List Edge) (p : Pair) (id : Nat) (at_ : Int)
    (hE : WellDated date E) (hp : date p.older < date p.newer) :
    WellDated date (record E p id at_) := by
  unfold record
  split
  · exact hE
  · intro e he hty
    rcases List.mem_append.mp he with h | h
    · exact hE e h hty
    · simp at h; subst h; exact hp

theorem step_extends (slot : Nat → Nat → Bool) (look : Nat → Option Concept) (new : Concept)
    (fresh : Nat → Nat) (at_ : Int) (E : List Edge) (hit : Nat) :
    ∃ added, step slot look new fresh at_ E hit = E ++ added ∧
      ∀ e ∈ added, e.ty = .suspected := by
  unfold step
  cases hl : look hit with
  | none => exact ⟨[], by simp, by simp⟩
  | some old =>
    simp only []
    cases hc : candidate slot E new old with
    | none => exact ⟨[], by simp, by simp⟩
    | some p =>
      simp only [record]
      split
      · exact ⟨[], by simp, by simp⟩
      · exact ⟨[_], rfl, by simp⟩

theorem step_wellDated (slot : Nat → Nat → Bool) (date : Nat → Int)
    (look : Nat → Option Concept) (new : Concept) (fresh : Nat → Nat) (at_ : Int)
    (E : List Edge) (hit : Nat) (hd : DatesAgree date look new) (hE : WellDated date E) :
    WellDated date (step slot look new fresh at_ E hit) := by
  unfold step
  cases hl : look hit with
  | none => exact hE
  | some old =>
    simp only []
    cases hc : candidate slot E new old with
    | none => exact hE
    | some p =>
      apply record_wellDated date E p _ _ hE
      have ho := hd.2 hit old hl
      rcases candidate_dir slot E new old p hc with ⟨h1, h2, h3⟩ | ⟨h1, h2, h3⟩
      · rw [h1, h2, ← ho, ← hd.1]; exact h3
      · rw [h1, h2, ← ho, ← hd.1]; exact h3

/-- `record_refused_hits` only appends `SuspectedSupersedes` edges, after the
existing ones, and never an edge of another type. -/
theorem recordRefused_extends (slot : Nat → Nat → Bool) (look : Nat → Option Concept)
    (new : Concept) (fresh : Nat → Nat) (at_ : Int) (E : List Edge) (refused : List Nat) :
    ∃ added, recordRefused slot look new fresh at_ E refused = E ++ added ∧
      ∀ e ∈ added, e.ty = .suspected := by
  unfold recordRefused
  generalize sortDedup refused = hits
  induction hits generalizing E with
  | nil => exact ⟨[], by simp, by simp⟩
  | cons h hs ih =>
    simp only [List.foldl_cons]
    obtain ⟨a1, h1, h1s⟩ := step_extends slot look new fresh at_ E h
    rw [h1]
    obtain ⟨a2, h2, h2s⟩ := ih (E ++ a1)
    refine ⟨a1 ++ a2, by rw [h2, List.append_assoc], ?_⟩
    intro e he
    rcases List.mem_append.mp he with h | h
    · exact h1s e h
    · exact h2s e h

/-- The trigger keeps every suspected edge pointing from the later-dated
concept to the earlier one. -/
theorem recordRefused_wellDated (slot : Nat → Nat → Bool) (date : Nat → Int)
    (look : Nat → Option Concept) (new : Concept) (fresh : Nat → Nat) (at_ : Int)
    (E : List Edge) (refused : List Nat) (hd : DatesAgree date look new)
    (hE : WellDated date E) :
    WellDated date (recordRefused slot look new fresh at_ E refused) := by
  unfold recordRefused
  generalize sortDedup refused = hits
  induction hits generalizing E with
  | nil => exact hE
  | cons h hs ih =>
    simp only [List.foldl_cons]
    exact ih _ (step_wellDated slot date look new fresh at_ E h hd hE)

/-- A walk of one or more `SuspectedSupersedes` edges from `x` to `y`. -/
inductive Reach (E : List Edge) : Nat → Nat → Prop
  | edge {e : Edge} : e ∈ E → e.ty = .suspected → Reach E e.src e.dst
  | step {e : Edge} {z : Nat} : e ∈ E → e.ty = .suspected → Reach E e.dst z → Reach E e.src z

/-- Along suspected edges the date strictly falls. -/
theorem reach_date (date : Nat → Int) (E : List Edge) (hE : WellDated date E) {x y : Nat}
    (h : Reach E x y) : date y < date x := by
  induction h with
  | edge he ht => exact hE _ he ht
  | step he ht _ ih => have := hE _ he ht; omega

/-- **Acyclic.** No concept reaches itself along suspected edges: in
particular no concept is suspected of superseding itself, and no pair is
suspected both ways. -/
theorem suspected_acyclic (date : Nat → Int) (E : List Edge) (hE : WellDated date E) (x : Nat) :
    ¬ Reach E x x := by
  intro h
  have := reach_date date E hE h
  omega

/-- On a well-dated graph the #198 trigger never leaves `SuspectedSupersedes`
edges both ways between two concepts: a suspected pair runs from the
later-dated concept to the earlier. -/
theorem suspected_not_both_ways (date : Nat → Int) (E : List Edge) (hE : WellDated date E)
    (e f : Edge) (he : e ∈ E) (hf : f ∈ E) (hte : e.ty = .suspected) (htf : f.ty = .suspected)
    (h1 : f.src = e.dst) (h2 : f.dst = e.src) : False := by
  have a := hE e he hte
  have b := hE f hf htf
  rw [h1, h2] at b
  omega

/-! ## Lapse: `settle` -/

/-- `settle`: remove the suspected edges between `a` and `b`, either way. The
Rust removes the one edge `edge_between` finds each way; `record` never
writes a second, so on a store the trigger wrote the two agree. -/
def settle (E : List Edge) (a b : Nat) : List Edge :=
  E.filter fun e =>
    !(e.ty == .suspected && ((e.src == a && e.dst == b) || (e.src == b && e.dst == a)))

/-- After `settle` the two are no longer a suspected pair. -/
theorem settle_clears (E : List Edge) (a b : Nat) : suspectedPair (settle E a b) a b = false := by
  simp only [suspectedPair, joined, edgeBetween, settle, Bool.or_eq_false_iff]
  constructor <;>
  · rw [List.any_eq_false]
    intro e he h
    rw [List.mem_filter] at he
    have h' := he.2
    simp only [Bool.and_eq_true, beq_iff_eq] at h
    obtain ⟨⟨h1, h2⟩, h3⟩ := h
    simp [h1, h2, h3] at h'

/-- `settle` removes only `SuspectedSupersedes` edges between the pair; every
edge of another type stays. -/
theorem settle_keeps_other (E : List Edge) (a b : Nat) (e : Edge) (he : e ∈ E)
    (ht : e.ty ≠ .suspected) : e ∈ settle E a b := by
  rw [settle, List.mem_filter]
  refine ⟨he, ?_⟩
  simp [ht]

/-! ## Replacement candidates (#198 part c)

A cosine is a fixed-point integer, `one = 10000` being `1.0` (the four
decimals the receipt reports). `Hit.finite` is `score.is_finite()`; a
non-finite score is dropped before anything else. -/

/-- `1.0` in fixed point. -/
def one : Int := 10000

/-- `REPLACEMENT_CANDIDATE_LIMIT`.
Pinned by `formal_constants_tests::supersession`. -/
def limit : Nat := 3

/-- One vector hit as the store returned it. -/
structure Hit where
  item : Nat
  score : Int
  finite : Bool
  deriving DecidableEq, Repr

/-- `ReplacementCandidate`, without the excerpt. -/
structure Cand where
  new : Nat
  id : Nat
  cosine : Int
  newer : Bool
  merged : Bool
  deriving DecidableEq, Repr

/-- `eligible_pair`: every condition but the vector ones. Unlike `candidate`
it does not consult the identifier guard (a merged pair is listed), and it
refuses an image concept. -/
def eligiblePair (E : List Edge) (new old : Concept) : Bool :=
  new.id != old.id && new.session == old.session && new.ty == old.ty && eligibleType new.ty &&
    new.scope == old.scope && !old.image && !settled E new.id old.id

/-- `score.clamp(0.0, 1.0)`. -/
def clampScore (s : Int) : Int := max 0 (min s one)

/-- The loop of `select` over the hits, in the order the store returned
them: non-finite and below-floor hits are dropped, then a skipped id or one
already kept, then anything that is not an eligible concept. -/
def keep (E : List Edge) (look : Nat → Option Concept) (new : Concept) (floor : Int)
    (skip : List Nat) : List (Concept × Int) → List Hit → List (Concept × Int)
  | kept, [] => kept
  | kept, h :: hs =>
    if h.finite = false then keep E look new floor skip kept hs
    else if clampScore h.score < floor then keep E look new floor skip kept hs
    else if h.item ∈ skip ∨ kept.any (fun k => k.1.id == h.item) = true then
      keep E look new floor skip kept hs
    else
      match look h.item with
      | some old =>
        if eligiblePair E new old then keep E look new floor skip (kept ++ [(old, clampScore h.score)]) hs
        else keep E look new floor skip kept hs
      | none => keep E look new floor skip kept hs

/-- Cosine descending, then id ascending. -/
def ahead (x y : Concept × Int) : Bool :=
  decide (y.2 < x.2) || (decide (x.2 = y.2) && decide (x.1.id < y.1.id))

def insertBy (x : Concept × Int) : List (Concept × Int) → List (Concept × Int)
  | [] => [x]
  | y :: ys => if ahead x y then x :: y :: ys else y :: insertBy x ys

/-- `kept.sort_by(...)`, as an insertion sort (the order is total on distinct
ids, and `keep` keeps one entry per id). -/
def sortKept (l : List (Concept × Int)) : List (Concept × Int) := l.foldr insertBy []

/-- `select`: the candidates for the created concept `new` among its vector
hits, at or above `floor`, never one in `skip`, those in `merged` marked;
ranked, at most `limit`. `newer` is `dated_at(old) > dated_at(new)`. -/
def select (E : List Edge) (look : Nat → Option Concept) (new : Concept) (hits : List Hit)
    (floor : Int) (skip merged : List Nat) : List Cand :=
  ((sortKept (keep E look new floor skip [] hits)).take limit).map fun k =>
    { new := new.id, id := k.1.id, cosine := k.2, newer := decide (new.dated < k.1.dated),
      merged := decide (k.1.id ∈ merged) }

theorem mem_insertBy (x z : Concept × Int) (l : List (Concept × Int)) :
    z ∈ insertBy x l ↔ z = x ∨ z ∈ l := by
  induction l with
  | nil => simp [insertBy]
  | cons y ys ih =>
    unfold insertBy
    split
    · simp
    · simp only [List.mem_cons, ih]
      constructor
      · rintro (h | h | h) <;> simp [h]
      · rintro (h | h | h) <;> simp [h]

theorem mem_sortKept (z : Concept × Int) (l : List (Concept × Int)) : z ∈ sortKept l ↔ z ∈ l := by
  induction l with
  | nil => simp [sortKept]
  | cons y ys ih =>
    simp only [sortKept, List.foldr_cons] at ih ⊢
    rw [mem_insertBy, ih]; simp

/-- `look` returns the concept with the id it was asked for (`g.node(id)`). -/
def LookById (look : Nat → Option Concept) : Prop := ∀ i c, look i = some c → c.id = i

/-- What every kept entry satisfies. -/
def Kept (E : List Edge) (look : Nat → Option Concept) (new : Concept) (floor : Int)
    (skip : List Nat) (k : Concept × Int) : Prop :=
  look k.1.id = some k.1 ∧ eligiblePair E new k.1 = true ∧ floor ≤ k.2 ∧ k.2 ≤ one ∧
    k.1.id ∉ skip

theorem keep_inv (E : List Edge) (look : Nat → Option Concept) (new : Concept) (floor : Int)
    (skip : List Nat) (hl : LookById look) :
    ∀ (hits : List Hit) (kept : List (Concept × Int)),
      (∀ k ∈ kept, Kept E look new floor skip k) →
      ∀ k ∈ keep E look new floor skip kept hits, Kept E look new floor skip k := by
  intro hits
  induction hits with
  | nil => intro kept hk; simpa [keep] using hk
  | cons h hs ih =>
    intro kept hk
    unfold keep
    split
    · exact ih kept hk
    · split
      · exact ih kept hk
      · rename_i hfloor
        split
        · exact ih kept hk
        · rename_i hskip
          cases hlook : look h.item with
          | none => exact ih kept hk
          | some old =>
            simp only []
            split
            · rename_i hel
              apply ih
              intro k hkm
              rcases List.mem_append.mp hkm with hk' | hk'
              · exact hk k hk'
              · simp at hk'
                subst hk'
                have hid := hl _ _ hlook
                refine ⟨?_, hel, by omega, ?_, ?_⟩
                · simp only []; rw [hid]; exact hlook
                · simp only [clampScore, one]; omega
                · simp only []; rw [hid]; intro hm; exact hskip (Or.inl hm)
            · exact ih kept hk

/-- At most `REPLACEMENT_CANDIDATE_LIMIT` candidates. -/
theorem select_length (E : List Edge) (look : Nat → Option Concept) (new : Concept)
    (hits : List Hit) (floor : Int) (skip merged : List Nat) :
    (select E look new hits floor skip merged).length ≤ limit := by
  simp only [select, List.length_map]
  exact List.length_take_le _ _

/-- Every listed candidate is an eligible concept, at or above the floor,
not skipped, not the new concept itself, and its `newer` mark is exactly
"dated strictly after the new concept". -/
theorem select_mem (E : List Edge) (look : Nat → Option Concept) (new : Concept)
    (hits : List Hit) (floor : Int) (skip merged : List Nat) (hl : LookById look)
    (c : Cand) (hc : c ∈ select E look new hits floor skip merged) :
    ∃ old, look c.id = some old ∧ old.id = c.id ∧ eligiblePair E new old = true ∧
      c.id ≠ new.id ∧ floor ≤ c.cosine ∧ c.cosine ≤ one ∧ c.id ∉ skip ∧ c.new = new.id ∧
      (c.newer = true ↔ new.dated < old.dated) := by
  simp only [select, List.mem_map] at hc
  obtain ⟨k, hk, rfl⟩ := hc
  have hk' := (mem_sortKept k _).mp (List.mem_of_mem_take hk)
  obtain ⟨h1, h2, h3, h4, h5⟩ :=
    keep_inv E look new floor skip hl hits [] (by simp) k hk'
  refine ⟨k.1, h1, rfl, h2, ?_, h3, h4, h5, rfl, by simp⟩
  intro heq
  have hne : new.id ≠ k.1.id := by
    have h2' := h2
    simp only [eligiblePair, Bool.and_eq_true, bne_iff_ne, ne_eq] at h2'
    exact h2'.1.1.1.1.1.1
  exact hne (by simpa using heq.symm)

/-- **`newer` agrees with the trigger.** For a listed candidate the trigger
would pair with the new concept, the candidate is marked `newer` exactly when
the trigger makes the candidate the suspected replacement, and unmarked
exactly when the new concept is. -/
theorem select_newer_matches_trigger (slot : Nat → Nat → Bool) (E : List Edge)
    (look : Nat → Option Concept) (new : Concept) (hits : List Hit) (floor : Int)
    (skip merged : List Nat) (hl : LookById look) (c : Cand)
    (hc : c ∈ select E look new hits floor skip merged) (old : Concept)
    (hold : look c.id = some old) (p : Pair) (hp : candidate slot E new old = some p) :
    (c.newer = true ↔ p.newer = old.id) ∧ (c.newer = false ↔ p.newer = new.id) := by
  obtain ⟨o, ho, _, _, hne, _, _, _, _, hnewer⟩ := select_mem E look new hits floor skip merged hl c hc
  rw [hold] at ho; injection ho with ho; subst ho
  have hid : new.id ≠ old.id := (candidate_conditions slot E new old p hp).1
  rcases candidate_dir slot E new old p hp with ⟨h1, _, h3⟩ | ⟨h1, _, h3⟩
  · have hn : c.newer = false := by
      cases h : c.newer
      · rfl
      · have := hnewer.mp h; omega
    rw [hn, h1]
    exact ⟨by simp [hid], by simp⟩
  · have hn : c.newer = true := hnewer.mpr h3
    rw [hn, h1]
    exact ⟨by simp, by simp [Ne.symm hid]⟩

/-- A candidate marked `newer` is never one the trigger, judged against the
edges `E` of the same write, would record as replaced by the new concept. This
holds trivially whenever `candidate` is `none` for another reason (slot rule,
equal identifiers as for a `merged` candidate, a settled pair); the rule
agents rely on is `select_newer_never_replaced_by_new`. -/
theorem select_newer_not_replaced (slot : Nat → Nat → Bool) (E : List Edge)
    (look : Nat → Option Concept) (new : Concept) (hits : List Hit) (floor : Int)
    (skip merged : List Nat) (hl : LookById look) (c : Cand)
    (hc : c ∈ select E look new hits floor skip merged) (hn : c.newer = true) (old : Concept)
    (hold : look c.id = some old) : candidate slot E new old ≠ some ⟨new.id, old.id⟩ := by
  intro hp
  have := (select_newer_matches_trigger slot E look new hits floor skip merged hl c hc old hold _ hp).2
  rw [hn] at this
  simp at this

/-- **Never cite a newer candidate in `against`** (AGENTS.md, and
`docs/reference/mcp.mdx`: "never cite it in `against`"). A candidate marked
`newer` is dated strictly after the new concept, and on **any** graph (any
edges `E'`, so on any later call too, whatever the slot rule) a suspected pair
of the two can only run from the candidate to the new concept: the candidate
is the possible replacement, never the replaced. -/
theorem select_newer_never_replaced_by_new (E : List Edge) (look : Nat → Option Concept)
    (new : Concept) (hits : List Hit) (floor : Int) (skip merged : List Nat)
    (hl : LookById look) (c : Cand) (hc : c ∈ select E look new hits floor skip merged)
    (old : Concept) (hold : look c.id = some old) (hn : c.newer = true) :
    new.dated < old.dated ∧
      ∀ (slot : Nat → Nat → Bool) (E' : List Edge) (p : Pair),
        candidate slot E' new old = some p → p.newer = old.id ∧ p.older = new.id := by
  obtain ⟨o, ho, _, _, _, _, _, _, _, hnewer⟩ := select_mem E look new hits floor skip merged hl c hc
  rw [hold] at ho; injection ho with ho; subst ho
  have hlt := hnewer.mp hn
  refine ⟨hlt, ?_⟩
  intro slot E' p hp
  rcases candidate_dir slot E' new old p hp with ⟨_, _, h3⟩ | ⟨h1, h2, _⟩
  · omega
  · exact ⟨h1, h2⟩

/-- A tie is not newer ("a tie is not marked newer"). -/
theorem select_tie_not_newer (E : List Edge) (look : Nat → Option Concept) (new : Concept)
    (hits : List Hit) (floor : Int) (skip merged : List Nat) (hl : LookById look) (c : Cand)
    (hc : c ∈ select E look new hits floor skip merged) (old : Concept)
    (hold : look c.id = some old) (ht : old.dated = new.dated) : c.newer = false := by
  obtain ⟨o, ho, _, _, _, _, _, _, _, hnewer⟩ := select_mem E look new hits floor skip merged hl c hc
  rw [hold] at ho; injection ho with ho; subst ho
  cases h : c.newer
  · rfl
  · have := hnewer.mp h; omega

/-! ### The fix for #302: a repeated id keeps its highest score

`select` documents its hits as taken "in any order" and its candidates as
"ranked by cosine", but `keep` keeps a repeated id's first score. The fixed
loop `keepF` raises a kept id's score to the maximum instead (`raiseScore`).
`keep`, `select` and every theorem above stay, as statements about the
previous version. The kept list is then characterized by the hits as a set
(`keepF_best`), so the fixed `selectF` does not depend on the order the
store returns them in, nor on how often it repeats one
(`selectF_order_independent`). -/

/-- Raise the kept score of id `i` to at least `s`. -/
def raiseScore (i : Nat) (s : Int) (kept : List (Concept × Int)) : List (Concept × Int) :=
  kept.map fun k => if k.1.id = i then (k.1, max k.2 s) else k

/-- The fixed loop: as `keep`, except that a hit on an id already kept raises
its score to the maximum. -/
def keepF (E : List Edge) (look : Nat → Option Concept) (new : Concept) (floor : Int)
    (skip : List Nat) : List (Concept × Int) → List Hit → List (Concept × Int)
  | kept, [] => kept
  | kept, h :: hs =>
    if h.finite = false then keepF E look new floor skip kept hs
    else if clampScore h.score < floor then keepF E look new floor skip kept hs
    else if h.item ∈ skip then keepF E look new floor skip kept hs
    else if kept.any (fun k => k.1.id == h.item) = true then
      keepF E look new floor skip (raiseScore h.item (clampScore h.score) kept) hs
    else
      match look h.item with
      | some old =>
        if eligiblePair E new old then keepF E look new floor skip (kept ++ [(old, clampScore h.score)]) hs
        else keepF E look new floor skip kept hs
      | none => keepF E look new floor skip kept hs

/-- `select` after the fix. -/
def selectF (E : List Edge) (look : Nat → Option Concept) (new : Concept) (hits : List Hit)
    (floor : Int) (skip merged : List Nat) : List Cand :=
  ((sortKept (keepF E look new floor skip [] hits)).take limit).map fun k =>
    { new := new.id, id := k.1.id, cosine := k.2, newer := decide (new.dated < k.1.dated),
      merged := decide (k.1.id ∈ merged) }

section fixed

variable {E : List Edge} {look : Nat → Option Concept} {new : Concept} {floor : Int}
  {skip : List Nat}

/-- A hit `select` can list as `old`: finite, at or above the floor, not
skipped, and an eligible concept. -/
def Qual (E : List Edge) (look : Nat → Option Concept) (new : Concept) (floor : Int)
    (skip : List Nat) (h : Hit) (old : Concept) : Prop :=
  h.finite = true ∧ floor ≤ clampScore h.score ∧ h.item ∉ skip ∧ look h.item = some old ∧
    eligiblePair E new old = true

/-- `k` is a concept some hit qualifies as, at the highest score of the hits
on its id. -/
def Best (E : List Edge) (look : Nat → Option Concept) (new : Concept) (floor : Int)
    (skip : List Nat) (hits : List Hit) (k : Concept × Int) : Prop :=
  (∃ h ∈ hits, Qual E look new floor skip h k.1 ∧ clampScore h.score = k.2) ∧
    ∀ h ∈ hits, ∀ o, Qual E look new floor skip h o → h.item = k.1.id → clampScore h.score ≤ k.2

/-- The loop invariant of `keepF` after the hits `pre`. -/
def KInv (E : List Edge) (look : Nat → Option Concept) (new : Concept) (floor : Int)
    (skip : List Nat) (pre : List Hit) (kept : List (Concept × Int)) : Prop :=
  (∀ k, k ∈ kept ↔ Best E look new floor skip pre k) ∧ (kept.map (·.1.id)).Nodup ∧
    ∀ h ∈ pre, ∀ o, Qual E look new floor skip h o → ∃ k ∈ kept, k.1.id = h.item

theorem qual_id (hl : LookById look) {h : Hit} {o : Concept}
    (hq : Qual E look new floor skip h o) : o.id = h.item :=
  hl _ _ hq.2.2.2.1

theorem best_snoc_of_not_qual {pre : List Hit} {h : Hit}
    (hq : ∀ o, ¬ Qual E look new floor skip h o) (k : Concept × Int) :
    Best E look new floor skip (pre ++ [h]) k ↔ Best E look new floor skip pre k := by
  constructor
  · rintro ⟨⟨h0, hm, hq0, hs⟩, hmax⟩
    rcases List.mem_append.mp hm with hm | hm
    · exact ⟨⟨h0, hm, hq0, hs⟩, fun h' hm' o hq' hi => hmax h' (by simp [hm']) o hq' hi⟩
    · simp at hm; subst hm; exact absurd hq0 (hq _)
  · rintro ⟨⟨h0, hm, hq0, hs⟩, hmax⟩
    refine ⟨⟨h0, by simp [hm], hq0, hs⟩, fun h' hm' o hq' hi => ?_⟩
    rcases List.mem_append.mp hm' with hm' | hm'
    · exact hmax h' hm' o hq' hi
    · simp at hm'; subst hm'; exact absurd hq' (hq _)

/-- A hit on an id other than `k`'s does not change whether `k` is best. -/
theorem best_snoc_other (hl : LookById look) {pre : List Hit} {h : Hit} {k : Concept × Int}
    (hne : k.1.id ≠ h.item) :
    Best E look new floor skip (pre ++ [h]) k ↔ Best E look new floor skip pre k := by
  constructor
  · rintro ⟨⟨h0, hm, hq0, hs⟩, hmax⟩
    rcases List.mem_append.mp hm with hm | hm
    · exact ⟨⟨h0, hm, hq0, hs⟩, fun h' hm' o hq' hi => hmax h' (by simp [hm']) o hq' hi⟩
    · simp at hm; subst hm; exact absurd (qual_id hl hq0) hne
  · rintro ⟨⟨h0, hm, hq0, hs⟩, hmax⟩
    refine ⟨⟨h0, by simp [hm], hq0, hs⟩, fun h' hm' o hq' hi => ?_⟩
    rcases List.mem_append.mp hm' with hm' | hm'
    · exact hmax h' hm' o hq' hi
    · simp at hm'; subst hm'; exact absurd hi.symm hne

theorem raiseScore_ids (i : Nat) (s : Int) (kept : List (Concept × Int)) :
    (raiseScore i s kept).map (·.1.id) = kept.map (·.1.id) := by
  simp only [raiseScore, List.map_map]
  apply List.map_congr_left
  intro k _
  simp only [Function.comp_apply]
  split <;> rfl

theorem mem_raiseScore {i : Nat} {s : Int} {kept : List (Concept × Int)} {k : Concept × Int} :
    k ∈ raiseScore i s kept ↔ ∃ k0 ∈ kept, k = if k0.1.id = i then (k0.1, max k0.2 s) else k0 := by
  simp only [raiseScore, List.mem_map]
  constructor
  · rintro ⟨k0, hk0, rfl⟩; exact ⟨k0, hk0, rfl⟩
  · rintro ⟨k0, hk0, rfl⟩; exact ⟨k0, hk0, rfl⟩

theorem nodup_ids_eq {kept : List (Concept × Int)} (hn : (kept.map (·.1.id)).Nodup)
    {a b : Concept × Int} (ha : a ∈ kept) (hb : b ∈ kept) (hab : a.1.id = b.1.id) : a = b := by
  induction kept with
  | nil => simp at ha
  | cons x xs ih =>
    simp only [List.map_cons, List.nodup_cons, List.mem_map] at hn
    simp only [List.mem_cons] at ha hb
    rcases ha with rfl | ha <;> rcases hb with rfl | hb
    · rfl
    · exact absurd ⟨b, hb, hab.symm⟩ hn.1
    · exact absurd ⟨a, ha, hab⟩ hn.1
    · exact ih hn.2 ha hb

/-- One step of `keepF` keeps the invariant. -/
theorem kinv_step (hl : LookById look) {pre : List Hit} {kept : List (Concept × Int)}
    (hi : KInv E look new floor skip pre kept) (h : Hit) :
    ∃ kept', KInv E look new floor skip (pre ++ [h]) kept' ∧
      ∀ hs, keepF E look new floor skip kept (h :: hs) = keepF E look new floor skip kept' hs := by
  obtain ⟨hmem, hnd, hcov⟩ := hi
  -- the hit qualifies for no concept: nothing changes
  have nochange : (∀ o, ¬ Qual E look new floor skip h o) →
      KInv E look new floor skip (pre ++ [h]) kept := by
    intro hq
    refine ⟨fun k => (hmem k).trans (best_snoc_of_not_qual hq k).symm, hnd, ?_⟩
    intro h0 hm o hq0
    rcases List.mem_append.mp hm with hm | hm
    · exact hcov h0 hm o hq0
    · simp at hm; subst hm; exact absurd hq0 (hq _)
  by_cases hfin : h.finite = false
  · refine ⟨kept, nochange (fun o hq => by rw [hq.1] at hfin; cases hfin), fun hs => ?_⟩
    simp [keepF, hfin]
  by_cases hfl : clampScore h.score < floor
  · refine ⟨kept, nochange (fun o hq => by have := hq.2.1; omega), fun hs => ?_⟩
    simp [keepF, hfin, hfl]
  by_cases hsk : h.item ∈ skip
  · refine ⟨kept, nochange (fun o hq => hq.2.2.1 hsk), fun hs => ?_⟩
    simp [keepF, hfin, hfl, hsk]
  have hfin' : h.finite = true := by simpa using hfin
  have hfl' : floor ≤ clampScore h.score := by omega
  by_cases hany : kept.any (fun k => k.1.id == h.item) = true
  · -- raise the kept entry
    refine ⟨raiseScore h.item (clampScore h.score) kept, ?_, fun hs => by
      simp [keepF, hfin, hfl, hsk, hany]⟩
    obtain ⟨e, he, heid⟩ := List.any_eq_true.mp hany
    have heid : e.1.id = h.item := by simpa using heid
    obtain ⟨⟨he0, he0m, he0q, he0s⟩, he0max⟩ := (hmem e).mp he
    have hlook : look h.item = some e.1 := by
      have h1 := he0q.2.2.2.1
      rw [← qual_id hl he0q, heid] at h1
      exact h1
    have hqe : Qual E look new floor skip h e.1 := ⟨hfin', hfl', hsk, hlook, he0q.2.2.2.2⟩
    refine ⟨fun k => ?_, by rw [raiseScore_ids]; exact hnd, ?_⟩
    · rw [mem_raiseScore]
      constructor
      · rintro ⟨k0, hk0, rfl⟩
        by_cases hk0i : k0.1.id = h.item
        · have hk0e : k0 = e := nodup_ids_eq hnd hk0 he (hk0i.trans heid.symm)
          subst hk0e
          simp only [hk0i, ite_true]
          refine ⟨?_, fun h' hm' o hq' hi' => ?_⟩
          · by_cases hle : clampScore h.score ≤ k0.2
            · exact ⟨he0, by simp [he0m], he0q, by rw [he0s]; omega⟩
            · exact ⟨h, by simp, hqe, by omega⟩
          · rcases List.mem_append.mp hm' with hm' | hm'
            · have := he0max h' hm' o hq' hi'; simp only []; omega
            · simp at hm'; subst hm'; simp only []; omega
        · simp only [hk0i, ite_false]
          exact (best_snoc_other hl (by simpa using hk0i)).mpr ((hmem k0).mp hk0)
      · intro hb
        by_cases hki : k.1.id = h.item
        · refine ⟨e, he, ?_⟩
          simp only [heid, ite_true]
          obtain ⟨⟨h0, hm0, hq0, hs0⟩, hmax⟩ := hb
          have hk1 : k.1 = e.1 := by
            have h1 := hq0.2.2.2.1
            rw [← qual_id hl hq0, hki, hlook] at h1
            injection h1 with h1; exact h1.symm
          have hge1 : e.2 ≤ k.2 := by
            rw [← he0s]; exact hmax he0 (by simp [he0m]) e.1 he0q (by rw [← qual_id hl he0q, hk1])
          have hge2 : clampScore h.score ≤ k.2 := hmax h (by simp) e.1 hqe hki.symm
          have hle : k.2 ≤ max e.2 (clampScore h.score) := by
            rcases List.mem_append.mp hm0 with hm0 | hm0
            · have := he0max h0 hm0 k.1 hq0 (by rw [← qual_id hl hq0, hk1]); omega
            · simp at hm0; subst hm0; omega
          apply Prod.ext
          · exact hk1
          · simp only []; omega
        · have hk : k ∈ kept := (hmem k).mpr ((best_snoc_other hl hki).mp hb)
          refine ⟨k, hk, ?_⟩
          simp [hki]
    · intro h0 hm o hq0
      rw [List.any_eq_true] at hany
      rcases List.mem_append.mp hm with hm | hm
      · obtain ⟨k, hk, hkid⟩ := hcov h0 hm o hq0
        refine ⟨if k.1.id = h.item then (k.1, max k.2 (clampScore h.score)) else k,
          mem_raiseScore.mpr ⟨k, hk, rfl⟩, ?_⟩
        split <;> simp_all
      · simp at hm; subst hm
        refine ⟨(e.1, max e.2 (clampScore h0.score)), mem_raiseScore.mpr ⟨e, he, by simp [heid]⟩, ?_⟩
        exact heid
  · -- a new id
    have hnone : ∀ k ∈ kept, k.1.id ≠ h.item := by
      intro k hk hki
      exact hany (List.any_eq_true.mpr ⟨k, hk, by simp [hki]⟩)
    have hany' : kept.any (fun k => k.1.id == h.item) = false := by simpa using hany
    cases hlook : look h.item with
    | none =>
      refine ⟨kept, nochange (fun o hq => by have h1 := hq.2.2.2.1; rw [hlook] at h1; cases h1),
        fun hs => ?_⟩
      simp [keepF, hfin, hfl, hsk, hany', hlook]
    | some old =>
      by_cases hel : eligiblePair E new old = true
      · refine ⟨kept ++ [(old, clampScore h.score)], ?_, fun hs => by
          simp [keepF, hfin, hfl, hsk, hany', hlook, hel]⟩
        have hqo : Qual E look new floor skip h old := ⟨hfin', hfl', hsk, hlook, hel⟩
        have holdid : old.id = h.item := qual_id hl hqo
        -- no hit of `pre` qualifies on this id
        have hpre : ∀ h0 ∈ pre, ∀ o, Qual E look new floor skip h0 o → h0.item ≠ h.item := by
          intro h0 hm o hq0 heq
          obtain ⟨k, hk, hkid⟩ := hcov h0 hm o hq0
          exact hnone k hk (hkid.trans heq)
        refine ⟨fun k => ?_, ?_, ?_⟩
        · simp only [List.mem_append, List.mem_singleton]
          by_cases hki : k.1.id = h.item
          · constructor
            · rintro (hk | rfl)
              · exact absurd hki (hnone k hk)
              · refine ⟨⟨h, by simp, hqo, rfl⟩, fun h' hm' o hq' hi' => ?_⟩
                rcases List.mem_append.mp hm' with hm' | hm'
                · exact absurd (hi'.trans holdid) (hpre h' hm' o hq')
                · simp at hm'; subst hm'; exact Int.le_refl _
            · rintro ⟨⟨h0, hm0, hq0, hs0⟩, hmax⟩
              right
              rcases List.mem_append.mp hm0 with hm0 | hm0
              · exact absurd ((qual_id hl hq0).symm.trans hki) (hpre h0 hm0 _ hq0)
              · simp at hm0; subst hm0
                have : look h0.item = some k.1 := hq0.2.2.2.1
                rw [hlook] at this; injection this with this
                apply Prod.ext
                · exact this.symm
                · exact hs0.symm
          · rw [best_snoc_other hl hki, ← hmem k]
            constructor
            · rintro (hk | rfl)
              · exact hk
              · exact absurd holdid hki
            · exact Or.inl
        · rw [List.map_append, List.nodup_append]
          refine ⟨hnd, by simp, ?_⟩
          intro a ha b hb
          simp only [List.map_cons, List.map_nil, List.mem_singleton] at hb
          subst hb
          obtain ⟨k, hk, rfl⟩ := List.mem_map.mp ha
          rw [holdid]; exact hnone k hk
        · intro h0 hm o hq0
          rcases List.mem_append.mp hm with hm | hm
          · obtain ⟨k, hk, hkid⟩ := hcov h0 hm o hq0
            exact ⟨k, by simp [hk], hkid⟩
          · simp at hm; subst hm
            exact ⟨(old, clampScore h0.score), by simp, holdid⟩
      · refine ⟨kept, nochange (fun o hq => by
          have h1 := hq.2.2.2.1; rw [hlook] at h1; injection h1 with h1; subst h1
          exact hel hq.2.2.2.2), fun hs => ?_⟩
        simp [keepF, hfin, hfl, hsk, hany', hlook, hel]

theorem kinv_run (hl : LookById look) :
    ∀ (hs pre : List Hit) (kept : List (Concept × Int)), KInv E look new floor skip pre kept →
      KInv E look new floor skip (pre ++ hs) (keepF E look new floor skip kept hs)
  | [], pre, kept, hi => by simpa [keepF] using hi
  | h :: hs, pre, kept, hi => by
    obtain ⟨kept', hi', heq⟩ := kinv_step hl hi h
    rw [heq]
    have := kinv_run hl hs (pre ++ [h]) kept' hi'
    simpa using this

/-- **What the fixed loop keeps**: exactly the concepts some hit qualifies as,
each once, at the highest score of the hits on its id. -/
theorem keepF_best (hl : LookById look) (hits : List Hit) :
    (∀ k, k ∈ keepF E look new floor skip [] hits ↔ Best E look new floor skip hits k) ∧
      ((keepF E look new floor skip [] hits).map (·.1.id)).Nodup := by
  have h0 : KInv E look new floor skip [] [] := by
    refine ⟨fun k => ?_, by simp, by simp⟩
    simp [Best]
  have := kinv_run hl hits [] [] h0
  simp only [List.nil_append] at this
  exact ⟨this.1, this.2.1⟩

/-! #### Sorting kept entries -/

theorem ahead_asymm {x y : Concept × Int} (h : ahead x y = true) : ahead y x = false := by
  simp only [ahead, Bool.or_eq_true, Bool.and_eq_true, decide_eq_true_eq] at h
  simp only [ahead, Bool.or_eq_false_iff, Bool.and_eq_false_iff, decide_eq_false_iff_not]
  omega

theorem ahead_total {x y : Concept × Int} (h : x.1.id ≠ y.1.id) :
    ahead x y = true ∨ ahead y x = true := by
  simp only [ahead, Bool.or_eq_true, Bool.and_eq_true, decide_eq_true_eq]
  omega

theorem ahead_trans {x y z : Concept × Int} (h1 : ahead x y = true) (h2 : ahead y z = true) :
    ahead x z = true := by
  simp only [ahead, Bool.or_eq_true, Bool.and_eq_true, decide_eq_true_eq] at *
  omega

theorem perm_insertBy (x : Concept × Int) : ∀ l, (insertBy x l).Perm (x :: l)
  | [] => List.Perm.refl _
  | y :: ys => by
    simp only [insertBy]
    split
    · exact List.Perm.refl _
    · exact ((perm_insertBy x ys).cons y).trans (List.Perm.swap x y ys)

theorem perm_sortKept : ∀ l : List (Concept × Int), (sortKept l).Perm l
  | [] => List.Perm.refl _
  | x :: xs => by
    simp only [sortKept, List.foldr_cons]
    exact (perm_insertBy x _).trans ((perm_sortKept xs).cons x)

theorem insertBy_sorted (x : Concept × Int) :
    ∀ l : List (Concept × Int), l.Pairwise (fun a b => ahead a b = true) →
      (∀ y ∈ l, y.1.id ≠ x.1.id) → (insertBy x l).Pairwise (fun a b => ahead a b = true)
  | [], _, _ => by simp [insertBy]
  | y :: ys, hs, hid => by
    simp only [insertBy]
    have hsy := List.pairwise_cons.mp hs
    split
    · rename_i hxy
      refine List.pairwise_cons.mpr ⟨?_, hs⟩
      intro z hz
      simp only [List.mem_cons] at hz
      rcases hz with rfl | hz
      · exact hxy
      · exact ahead_trans hxy (hsy.1 z hz)
    · rename_i hxy
      have hyx : ahead y x = true := by
        rcases ahead_total (x := y) (y := x) (hid y (by simp)) with h | h
        · exact h
        · simp_all
      refine List.pairwise_cons.mpr ⟨?_, insertBy_sorted x ys hsy.2 (fun z hz => hid z (by simp [hz]))⟩
      intro z hz
      rcases List.mem_cons.mp ((perm_insertBy x ys).mem_iff.mp hz) with rfl | hz
      · exact hyx
      · exact hsy.1 z hz

theorem sortKept_sorted : ∀ l : List (Concept × Int), (l.map (·.1.id)).Nodup →
    (sortKept l).Pairwise (fun a b => ahead a b = true)
  | [], _ => by simp [sortKept]
  | x :: xs, hn => by
    simp only [List.map_cons, List.nodup_cons, List.mem_map] at hn
    simp only [sortKept, List.foldr_cons]
    apply insertBy_sorted x _ (sortKept_sorted xs hn.2)
    intro y hy hyx
    exact hn.1 ⟨y, (perm_sortKept xs).mem_iff.mp hy, hyx⟩

theorem nodup_of_ids {l : List (Concept × Int)} (h : (l.map (·.1.id)).Nodup) : l.Nodup :=
  List.Pairwise.of_map (·.1.id) (fun a b hab heq => hab (by rw [heq])) h

/-- Lists of kept entries with distinct ids sort alike when they hold the
same entries. -/
theorem sortKept_eq {l l' : List (Concept × Int)} (hn : (l.map (·.1.id)).Nodup)
    (hn' : (l'.map (·.1.id)).Nodup) (hp : l.Perm l') : sortKept l = sortKept l' :=
  List.Perm.eq_of_pairwise (le := fun a b => ahead a b = true)
    (fun a b _ _ hab hba => by rw [ahead_asymm hab] at hba; cases hba)
    (sortKept_sorted l hn) (sortKept_sorted l' hn')
    ((perm_sortKept l).trans (hp.trans (perm_sortKept l').symm))

/-- **The fixed `select` does not depend on hit order**: two hit lists with
the same members (in any order, with any repetition) give the same
candidates. -/
theorem selectF_order_independent (hl : LookById look) {hits hits' : List Hit}
    (h : ∀ x, x ∈ hits ↔ x ∈ hits') (merged : List Nat) :
    selectF E look new hits floor skip merged = selectF E look new hits' floor skip merged := by
  obtain ⟨hm, hn⟩ := keepF_best (E := E) (new := new) (floor := floor) (skip := skip) hl hits
  obtain ⟨hm', hn'⟩ := keepF_best (E := E) (new := new) (floor := floor) (skip := skip) hl hits'
  have hbest : ∀ k, Best E look new floor skip hits k ↔ Best E look new floor skip hits' k := by
    intro k
    simp only [Best, h]
  have hp : (keepF E look new floor skip [] hits).Perm (keepF E look new floor skip [] hits') :=
    (List.perm_ext_iff_of_nodup (nodup_of_ids hn) (nodup_of_ids hn')).mpr
      (fun k => (hm k).trans ((hbest k).trans (hm' k).symm))
  simp only [selectF, sortKept_eq hn hn' hp]

/-- A repeated id is listed at its highest score: in particular the order of
two hits on one id does not matter. -/
theorem selectF_swap (hl : LookById look) (a b : Hit) (hs : List Hit) (merged : List Nat) :
    selectF E look new (a :: b :: hs) floor skip merged =
      selectF E look new (b :: a :: hs) floor skip merged :=
  selectF_order_independent hl (fun _ => (List.Perm.swap b a hs).mem_iff) merged

theorem keepF_kept (hl : LookById look) (hits : List Hit) :
    ∀ k ∈ keepF E look new floor skip [] hits, Kept E look new floor skip k := by
  intro k hk
  obtain ⟨⟨h0, _, hq, hs⟩, _⟩ := ((keepF_best hl hits).1 k).mp hk
  have hid := qual_id hl hq
  have hlook : look k.1.id = some k.1 := by rw [hid]; exact hq.2.2.2.1
  have hfl : floor ≤ k.2 := by rw [← hs]; exact hq.2.1
  have hone : k.2 ≤ one := by rw [← hs]; simp only [clampScore, one]; omega
  have hsk : k.1.id ∉ skip := by rw [hid]; exact hq.2.2.1
  exact ⟨hlook, hq.2.2.2.2, hfl, hone, hsk⟩

end fixed

/-- At most `REPLACEMENT_CANDIDATE_LIMIT` candidates, after the fix. -/
theorem selectF_length (E : List Edge) (look : Nat → Option Concept) (new : Concept)
    (hits : List Hit) (floor : Int) (skip merged : List Nat) :
    (selectF E look new hits floor skip merged).length ≤ limit := by
  simp only [selectF, List.length_map]
  exact List.length_take_le _ _

/-- `select_mem` after the fix. -/
theorem selectF_mem (E : List Edge) (look : Nat → Option Concept) (new : Concept)
    (hits : List Hit) (floor : Int) (skip merged : List Nat) (hl : LookById look)
    (c : Cand) (hc : c ∈ selectF E look new hits floor skip merged) :
    ∃ old, look c.id = some old ∧ old.id = c.id ∧ eligiblePair E new old = true ∧
      c.id ≠ new.id ∧ floor ≤ c.cosine ∧ c.cosine ≤ one ∧ c.id ∉ skip ∧ c.new = new.id ∧
      (c.newer = true ↔ new.dated < old.dated) := by
  simp only [selectF, List.mem_map] at hc
  obtain ⟨k, hk, rfl⟩ := hc
  have hk' := (mem_sortKept k _).mp (List.mem_of_mem_take hk)
  obtain ⟨h1, h2, h3, h4, h5⟩ := keepF_kept hl hits k hk'
  refine ⟨k.1, h1, rfl, h2, ?_, h3, h4, h5, rfl, by simp⟩
  intro heq
  have hne : new.id ≠ k.1.id := by
    have h2' := h2
    simp only [eligiblePair, Bool.and_eq_true, bne_iff_ne, ne_eq] at h2'
    exact h2'.1.1.1.1.1.1
  exact hne (by simpa using heq.symm)

/-- `select_newer_matches_trigger` after the fix. -/
theorem selectF_newer_matches_trigger (slot : Nat → Nat → Bool) (E : List Edge)
    (look : Nat → Option Concept) (new : Concept) (hits : List Hit) (floor : Int)
    (skip merged : List Nat) (hl : LookById look) (c : Cand)
    (hc : c ∈ selectF E look new hits floor skip merged) (old : Concept)
    (hold : look c.id = some old) (p : Pair) (hp : candidate slot E new old = some p) :
    (c.newer = true ↔ p.newer = old.id) ∧ (c.newer = false ↔ p.newer = new.id) := by
  obtain ⟨o, ho, _, _, hne, _, _, _, _, hnewer⟩ := selectF_mem E look new hits floor skip merged hl c hc
  rw [hold] at ho; injection ho with ho; subst ho
  have hid : new.id ≠ old.id := (candidate_conditions slot E new old p hp).1
  rcases candidate_dir slot E new old p hp with ⟨h1, _, h3⟩ | ⟨h1, _, h3⟩
  · have hn : c.newer = false := by
      cases h : c.newer
      · rfl
      · have := hnewer.mp h; omega
    rw [hn, h1]
    exact ⟨by simp [hid], by simp⟩
  · have hn : c.newer = true := hnewer.mpr h3
    rw [hn, h1]
    exact ⟨by simp, by simp [Ne.symm hid]⟩

/-- `select_newer_not_replaced` after the fix. -/
theorem selectF_newer_not_replaced (slot : Nat → Nat → Bool) (E : List Edge)
    (look : Nat → Option Concept) (new : Concept) (hits : List Hit) (floor : Int)
    (skip merged : List Nat) (hl : LookById look) (c : Cand)
    (hc : c ∈ selectF E look new hits floor skip merged) (hn : c.newer = true) (old : Concept)
    (hold : look c.id = some old) : candidate slot E new old ≠ some ⟨new.id, old.id⟩ := by
  intro hp
  have := (selectF_newer_matches_trigger slot E look new hits floor skip merged hl c hc old hold _ hp).2
  rw [hn] at this
  simp at this

/-- `select_newer_never_replaced_by_new` after the fix: never cite a newer
candidate in `against`. -/
theorem selectF_newer_never_replaced_by_new (E : List Edge) (look : Nat → Option Concept)
    (new : Concept) (hits : List Hit) (floor : Int) (skip merged : List Nat)
    (hl : LookById look) (c : Cand) (hc : c ∈ selectF E look new hits floor skip merged)
    (old : Concept) (hold : look c.id = some old) (hn : c.newer = true) :
    new.dated < old.dated ∧
      ∀ (slot : Nat → Nat → Bool) (E' : List Edge) (p : Pair),
        candidate slot E' new old = some p → p.newer = old.id ∧ p.older = new.id := by
  obtain ⟨o, ho, _, _, _, _, _, _, _, hnewer⟩ := selectF_mem E look new hits floor skip merged hl c hc
  rw [hold] at ho; injection ho with ho; subst ho
  have hlt := hnewer.mp hn
  refine ⟨hlt, ?_⟩
  intro slot E' p hp
  rcases candidate_dir slot E' new old p hp with ⟨_, _, h3⟩ | ⟨h1, h2, _⟩
  · omega
  · exact ⟨h1, h2⟩

/-- `select_tie_not_newer` after the fix. -/
theorem selectF_tie_not_newer (E : List Edge) (look : Nat → Option Concept) (new : Concept)
    (hits : List Hit) (floor : Int) (skip merged : List Nat) (hl : LookById look) (c : Cand)
    (hc : c ∈ selectF E look new hits floor skip merged) (old : Concept)
    (hold : look c.id = some old) (ht : old.dated = new.dated) : c.newer = false := by
  obtain ⟨o, ho, _, _, _, _, _, _, _, hnewer⟩ := selectF_mem E look new hits floor skip merged hl c hc
  rw [hold] at ho; injection ho with ho; subst ho
  cases h : c.newer
  · rfl
  · have := hnewer.mp h; omega

end Lambo.Supersession
