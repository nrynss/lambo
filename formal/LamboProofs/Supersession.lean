/-
# Supersession: the #198 suspicion and the #20 demotion

Lambo has two mechanisms called supersession, and this file models both and
how recall reads them.

**Suspected supersession (#198)**, `src/graph/suspected.rs`,
`src/graph/suspected/slot.rs`, `src/graph/suspected/replacement.rs`: a newer
fact written without `against` that looks like a new value of an older one
gets a warning-only `SuspectedSupersedes` edge, newer → older. Modelled:
`eligible_type`, `repo_scope` (as an abstract scope), `dated_at`, `settled`,
`is_suspected_pair`, `candidate`, `record`, `record_refused_hits`, `settle`,
and replacement candidates: `eligible_pair` and `select`, with its floor,
clamp, skip, dedup, ranking, cap and the `newer` and `merged` marks. `keep`
and `select` are the version before #302's fix, which kept a repeated id's
first score; `keepF` and `selectF` are the fixed one, which keeps its highest.

**Evidence-driven supersession (#20)**, `src/canon/supersession.rs`, behind
`Config::canonization_supersede` (default off): a Canonical concept whose
contradicting concept has a body of newer dependent evidence goes
`Canonical → None`. Modelled: the evidence rules of `src/store/evidence.rs`
(`latest_at`, `counted`, `newest_counted`, `span_since` with
`stage2_evidence_passes`), `watermark`, `contradictions`, `usable`,
`probes_for` (forward probes, reverse probes, ambiguity), `gather`,
`gather_holds`, `decide` (the pair rule `outranks` and one winner per
target), and the supersession pass of `apply` in `src/canon/eval.rs`
through the write gate (`Canon.gate`).

**Recall**: `never_redundant` and `Redundancy::signal`
(`src/recall/redundancy.rs`), `in_suspected_pair`, `possibly_superseded_by`
and `superseded_by` (`src/recall/assemble.rs`), and the line gate of
`src/recall/suspected.rs` as a Boolean.

## Results

* `candidate_self`, `candidate_symm`, `candidate_dir`, `candidate_tie`,
  `candidate_conditions`: the #198 pair condition. A concept is never paired
  with itself; the pair does not depend on argument order; the strictly
  later-dated concept is the suspected replacement and a tie records nothing
  (module doc, test `direction_and_ties`); every condition (session, type,
  eligible type, scope, identifier guard, not settled) held.
* `recordRefused_extends`, `recordRefused_wellDated`, `reach_date`,
  `suspected_acyclic`, `suspected_not_both_ways`: the trigger adds only
  `SuspectedSupersedes` edges, each from a strictly later-dated concept to an
  earlier one, so the relation is acyclic: nothing suspects itself and no pair
  is suspected both ways. `settle_clears`, `settle_keeps_other`: the lapse.
* `select_mem`, `select_length`: a replacement candidate is an eligible
  concept of the session, at or above the floor, not skipped, not the new
  concept, at most `REPLACEMENT_CANDIDATE_LIMIT` of them; its `newer` mark is
  exactly "dated strictly after the new concept" (its last conjunct).
  `select_newer_never_replaced_by_new`: a candidate marked `newer` is dated
  after the new concept, and on any graph, so on any later call, a suspected
  pair of the two runs from the candidate to the new concept, never the other
  way: the rule "never cite a `newer` candidate in `against`" (AGENTS.md,
  `docs/reference/mcp.mdx`) rests on this and on `select_mem`.
  `select_newer_matches_trigger`, `select_newer_not_replaced`: `newer` agrees
  with the trigger's direction on the write's own graph.
  `select_tie_not_newer`: a tie is not newer (test
  `a_later_dated_candidate_is_marked_newer_both_ways_and_a_tie_is_not`).
* #302's fix to `select` (`keepF`, `selectF`): `keepF_best`: the fixed loop
  keeps exactly the concepts some hit qualifies as, each once, at the highest
  score of the hits on its id; so `selectF_order_independent`: two hit lists
  with the same members, in any order and with any repetition, give the same
  candidates (`selectF_swap`: in particular two hits on one id). The theorems
  above carry over: `selectF_mem`, `selectF_length`,
  `selectF_newer_matches_trigger`, `selectF_newer_not_replaced`,
  `selectF_newer_never_replaced_by_new`, `selectF_tie_not_newer`. The old
  `select` keeps a repeated id's first score (the examples, test
  `a_duplicate_id_keeps_its_highest_cosine`).
* `stage2Since_newest`, `evaluate_some`, `verdict_sound`: a #20 verdict
  comes from a forward probe whose source's newest counted evidence is
  strictly after the target's watermark, itself at least the target's
  baseline ("newer dependent evidence"; tests `older_support_does_not_count`,
  `support_at_the_watermark_instant_does_not_count`).
* `probesFor_ok`, `verdict_ne`: every probe rests on a `Contradicts` edge
  between two different concepts, so a concept is never superseded by itself.
* `pair_rule`: never verdicts both ways for one pair in a cycle ("only one of
  them may act on the other"). `verdict_outranks_reverse`: when both qualify,
  the demoted one is the one whose newest counted evidence is older, ties
  going to the smaller key, then the smaller id (test
  `reciprocal_contradictions_flip_at_most_once_per_cooldown`).
  `verdict_best`: of several sources for one target, the newest counted
  evidence wins (test `the_source_with_the_newest_evidence_wins`).
* `applySup_legal`, `applySup_rank_le`, `applySup_untouched`,
  `applySup_source`: the pass moves a concept only `Canonical → None`, an edge
  the gate admits and, by `Canon.legal_demotion_unique`, the only demotion
  there is; it never promotes or skips, and touches only verdict targets ("`S`
  is not promoted"). `applySup_eq_cycle`: it is `Canon.cycle`'s pass 0 with
  the supersede flag set from the verdicts that still hold.
* `gather_off`, `gatherHolds_off`, `off_no_demotion`, `off_agrees`,
  `held_off`: with `canonization_supersede` off nothing is probed, decided,
  demoted or held, and the cycle is `Canon.cycle` with no supersession. They
  are definitional (`rfl`), as the Rust's early returns in `gather` and
  `gather_holds` are (`if !params.supersede { return Probes::default(); }`).
  The option defaults to off (`canonization_supersede: false` in
  `Config::default`, `src/config.rs`; test
  `canonization_supersede_defaults_off_and_parses_from_the_file`). Pinned by
  `formal_constants_tests::supersession`.
* `contradictions_ignore_suspected`, `probesFor_ignore_suspected`: a
  `SuspectedSupersedes` edge is never read as a contradiction, so #198 never
  demotes anything ("informational ... never read as one").
* `signal_suspected`, `suspected_ends_not_skipped`: a suspected pair is never
  redundant under any signal, and neither end is skipped (owner decision 5 on
  #198). `pss_sound`, `pss_newer`, `pss_not_on_newer`, `pss_newest`: the
  "possibly superseded" warning is on the older concept, names a shown newer
  one, the newest by `dated_at`. `annotate_field`: the structured field does
  not depend on the line gate. `budget_clears`, `supersededLine_canonical`:
  the #20 history line follows the latest demotion and is never on a
  Canonical concept.

## Model and number model

Dates are integers (milliseconds for #20's evidence). A cosine is a
fixed-point integer, `one = 10000` being `1.0`; the clamp into `[0, 1]` and
the floor are exact; `f64` rounding is outside the model. Stage 2's coverage
`span / min(session, window) >= 0.3` is compared by cross-multiplication;
it can differ from the `f64` quotient only at the boundary, by rounding.
Canonical keys are numbers ordered as their `tie_form`. One session; graph
edges are unique per `(source, target, type)` (`upsert_edge` reinforces a
duplicate), which `record` relies on.

## Left out

* The text rule (`one_span_value_slot`: tokens, the `difflib` alignment,
  `PREDICATE_CUES`) is an abstract predicate `slot older newer`; the identifier
  guard is equality of an abstract signature; cosines are given.
* `scan`, `pair_stamp`, `settle_edges` (the alias overlay), the receipt
  report (`for_receipt`, `describe`, byte caps) and the excerpt.
* Writer attribution (`attribute_at`, `self_suppressed`): modelled as a given
  function from a contradiction edge to its excluded evidence, or `none` when
  ambiguous. The evidence rows are given already reduced (eligibility by the
  age floor and own origin, the origin's date when it counts).
* The ring window, batch size, ring cursor, `MAX_SUPERSESSION_PROBES`, the
  hold's deferrals and cursor, and the evidence cache: they choose which
  targets are probed in a cycle, and every theorem about `probesFor` holds for
  any target list. `still_holds` is the given predicate `holds` plus the
  explicit target-status check.
* `gather_hold_for`, inspect's explanation of a hold: it returns no probes
  unless the option is on and the node is Venerable
  (`if !params.supersede || !venerable`), then probes that one node with a
  fresh writer timeline and the full probe budget. It is not modelled; it
  reuses `probes_for`, which is.
* Stage 3's use of the hold beyond `held` (`Canon.lean` models Stage 3), the
  store-capability fallback and the audit write.
-/

import LamboProofs.Canon

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

/-! ## Evidence-driven supersession (#20): the evidence

Dates are integer milliseconds. A row is one dependence edge into a node, as
`DependentEvidence` carries it, already reduced to what the rules read:

* `at_` is the edge's `declared` date (about-time clamped to write time);
* `eligible` is "written by the cutoff and not from the node's own origin"
  (`eligible` in `src/store/evidence.rs`);
* `originAt` is the dependent's origin interaction's declared date when that
  origin is a session interaction written by the cutoff, `none` otherwise
  (`counted` drops such a row).

`coveragePasses` takes the minimum and maximum over the rows' origin dates,
where `span_since` takes them over a per-origin map (one entry per origin).
The two agree when every row of one origin carries that origin's date. The
`Row` type does not enforce it; the Rust gives it by construction (a row's
`origin_dates` are its origin interaction's, and one interaction has one
date). No theorem in this file depends on it: they use only the count of
distinct origins and the edge dates. -/

structure Row where
  edge : Nat
  at_ : Int
  origin : Nat
  originAt : Option Int
  eligible : Bool
  deriving DecidableEq, Repr

structure Evidence where
  rows : List Row
  /-- The session's extent on about-times, `None` without interactions. -/
  extent : Option (Int × Int)
  deriving DecidableEq, Repr

/-- The running maximum of a fold (`Iterator::max`). -/
def maxOpt : Option Int → Int → Option Int
  | none, x => some x
  | some m, x => some (max m x)

/-- `latest_at`: the newest declared date of the eligible evidence, ignoring
the edges in `exclude`. -/
def latestAt (ev : Evidence) (exclude : List Nat) : Option Int :=
  (ev.rows.filter fun r => r.eligible && !decide (r.edge ∈ exclude)).foldl
    (fun m r => maxOpt m r.at_) none

/-- `counted`: eligible rows whose edge and whose origin are both dated
strictly after `since`, with the origin's date. -/
def counted (ev : Evidence) (since : Int) : List (Row × Int) :=
  ev.rows.filterMap fun r =>
    if r.eligible && decide (since < r.at_) then
      match r.originAt with
      | some d => if since < d then some (r, d) else none
      | none => none
    else none

/-- `newest_counted`: the newest edge date among the counted rows. -/
def newestCounted (ev : Evidence) (since : Int) : Option Int :=
  (counted ev since).foldl (fun m x => maxOpt m x.1.at_) none

/-- `coverage(span, sess_span, window) >= 0.3` on the counted origins'
dates, exactly: a session of no extent covers fully, otherwise
`span / min(session, window) >= 0.3`. The clamp into `[0, 1]` does not move
the comparison. `window` is positive (`validate_coverage_window`).
Pinned by `formal_constants_tests::supersession`. -/
def coveragePasses (ev : Evidence) (window : Int) (c : List (Row × Int)) : Bool :=
  match c.map (·.2) with
  | [] => false
  | d :: ds =>
    let lo := ds.foldl min d
    let hi := ds.foldl max d
    let ext := ev.extent.getD (lo, hi)
    let sess := max 0 (ext.2 - ext.1)
    let span := max 0 (hi - lo)
    if sess ≤ 0 then true else decide (3 * min sess window ≤ 10 * span)

/-- `stage2_evidence_passes(span_since(...))`: at least three distinct
origins after `since`, covering at least `0.3`.
Pinned by `formal_constants_tests::supersession`. -/
def stage2Since (ev : Evidence) (window since : Int) : Bool :=
  let c := counted ev since
  decide (3 ≤ (sortDedup (c.map (·.1.origin))).length) && coveragePasses ev window c

theorem foldl_maxOpt_isSome {α : Type} (f : α → Int) :
    ∀ (l : List α) (m : Option Int), (m.isSome ∨ l ≠ []) →
      ∃ n, l.foldl (fun m x => maxOpt m (f x)) m = some n := by
  intro l
  induction l with
  | nil =>
    intro m h
    cases m with
    | none => simp at h
    | some n => exact ⟨n, rfl⟩
  | cons x xs ih =>
    intro m _
    simp only [List.foldl_cons]
    apply ih
    left
    cases m <;> simp [maxOpt]

theorem foldl_maxOpt_gt {α : Type} (f : α → Int) (s : Int) :
    ∀ (l : List α) (m : Option Int), (∀ x, m = some x → s < x) → (∀ x ∈ l, s < f x) →
      ∀ n, l.foldl (fun m x => maxOpt m (f x)) m = some n → s < n := by
  intro l
  induction l with
  | nil => intro m hm _ n hn; exact hm n hn
  | cons x xs ih =>
    intro m hm hl n hn
    simp only [List.foldl_cons] at hn
    apply ih _ _ (fun y hy => hl y (List.mem_cons_of_mem _ hy)) n hn
    intro y hy
    have hx := hl x (List.mem_cons_self ..)
    cases m with
    | none => simp [maxOpt] at hy; omega
    | some m' =>
      have := hm m' rfl
      simp [maxOpt] at hy; omega

theorem counted_after (ev : Evidence) (since : Int) (x : Row × Int) (hx : x ∈ counted ev since) :
    since < x.1.at_ ∧ since < x.2 := by
  simp only [counted, List.mem_filterMap] at hx
  obtain ⟨r, _, hr⟩ := hx
  split at hr
  · rename_i hc
    simp only [Bool.and_eq_true, decide_eq_true_eq] at hc
    split at hr
    · rename_i d _
      split at hr
      · rename_i hd
        simp only [Option.some.injEq] at hr
        subst hr
        exact ⟨hc.2, hd⟩
      · cases hr
    · cases hr
  · cases hr

/-- A passing span counted something, and the newest counted evidence is
strictly after `since` ("newer support"). -/
theorem stage2Since_newest (ev : Evidence) (window since : Int)
    (h : stage2Since ev window since = true) :
    ∃ n, newestCounted ev since = some n ∧ since < n := by
  have hne : counted ev since ≠ [] := by
    intro he
    simp [stage2Since, he, sortDedup] at h
  obtain ⟨n, hn⟩ := foldl_maxOpt_isSome (fun x : Row × Int => x.1.at_) (counted ev since) none
    (Or.inr hne)
  refine ⟨n, hn, ?_⟩
  exact foldl_maxOpt_gt (fun x : Row × Int => x.1.at_) since _ none (by simp)
    (fun x hx => (counted_after ev since x hx).1) n hn

/-- Evidence at the watermark instant does not count (Rust test
`support_at_the_watermark_instant_does_not_count`). -/
theorem counted_at_instant (since : Int) (r : Row) (h : r.at_ = since) :
    counted ⟨[r], none⟩ since = [] := by
  simp [counted, h]

/-! ## The probe and the verdicts -/

/-- One `(target, source)` pair to probe. `key` is the source's canonical
key in `tie_form`, as a number with the same order. -/
structure Probe where
  target : Nat
  source : Nat
  key : Nat
  edge : Nat
  baseline : Int
  exclude : List Nat
  reverseOnly : Bool
  deriving DecidableEq, Repr

/-- A passing probe with its watermark and newest counted evidence. -/
structure Pass where
  probe : Probe
  watermark : Int
  newest : Int
  deriving DecidableEq, Repr

/-- The watermark: the later of the target's baseline and its latest
eligible evidence, the self-suppressed edges excluded. -/
def watermark (ev : Nat → Evidence) (p : Probe) : Int :=
  match latestAt (ev p.target) p.exclude with
  | some a => max a p.baseline
  | none => p.baseline

/-- One probe of `decide`: the source's span strictly after the target's
watermark must pass Stage 2. -/
def evaluate (ev : Nat → Evidence) (window : Int) (p : Probe) : Option Pass :=
  let w := watermark ev p
  if stage2Since (ev p.source) window w then
    some ⟨p, w, (newestCounted (ev p.source) w).getD w⟩
  else none

/-- `outranks`: newer counted evidence, then the smaller key, then the
smaller source id. -/
def outranks (a b : Pass) : Bool :=
  decide (b.newest < a.newest ∨
    (a.newest = b.newest ∧ (a.probe.key < b.probe.key ∨
      (a.probe.key = b.probe.key ∧ a.probe.source < b.probe.source))))

theorem outranks_irrefl (a : Pass) : outranks a a = false := by
  simp [outranks]

theorem outranks_trans (a b c : Pass) (h1 : outranks a b = true) (h2 : outranks b c = true) :
    outranks a c = true := by
  simp only [outranks, decide_eq_true_eq] at *
  omega

/-- Two passes with different sources are always ordered one way. -/
theorem outranks_total (a b : Pass) (h : a.probe.source ≠ b.probe.source) :
    outranks a b = true ∨ outranks b a = true := by
  simp only [outranks, decide_eq_true_eq]
  omega

/-- `outranks` (newer counted evidence, then the smaller key, then the smaller
source id) never ranks each of two supersession passes over the other. -/
theorem outranks_asymm (a b : Pass) (h : outranks a b = true) : outranks b a = false := by
  simp only [outranks, decide_eq_true_eq, decide_eq_false_iff_not] at *
  omega

/-- The pair rule: `a` is beaten when the reverse pass outranks it. -/
def beaten (passing : List Pass) (a : Pass) : Bool :=
  passing.any fun b =>
    b.probe.target == a.probe.source && b.probe.source == a.probe.target && outranks b a

/-- The best of a list: the first one no later one outranks. -/
def best : List Pass → Option Pass
  | [] => none
  | p :: ps => some (ps.foldl (fun cur q => if outranks q cur then q else cur) p)

/-- A demotion verdict. -/
structure Verdict where
  target : Nat
  source : Nat
  edge : Nat
  watermark : Int
  deriving DecidableEq, Repr

def Pass.verdict (w : Pass) : Verdict := ⟨w.probe.target, w.probe.source, w.probe.edge, w.watermark⟩

/-- The passing probes. -/
def passing (ev : Nat → Evidence) (window : Int) (probes : List Probe) : List Pass :=
  probes.filterMap (evaluate ev window)

/-- The forward passes the pair rule leaves standing. -/
def winners (ev : Nat → Evidence) (window : Int) (probes : List Probe) : List Pass :=
  let ps := passing ev window probes
  ps.filter fun p => !p.probe.reverseOnly && !beaten ps p

/-- `decide`: every passing probe, the pair rule, then one winning source per
forward target, in target order. -/
def decideV (ev : Nat → Evidence) (window : Int) (probes : List Probe) : List Verdict :=
  let ws := winners ev window probes
  (sortDedup (ws.map (·.probe.target))).filterMap fun t =>
    (best (ws.filter (·.probe.target == t))).map Pass.verdict

theorem evaluate_some (ev : Nat → Evidence) (window : Int) (p : Probe) (a : Pass)
    (h : evaluate ev window p = some a) :
    a.probe = p ∧ p.baseline ≤ a.watermark ∧ a.watermark < a.newest := by
  unfold evaluate at h
  dsimp only at h
  split at h
  · rename_i hs
    simp only [Option.some.injEq] at h
    subst h
    obtain ⟨n, hn, hlt⟩ := stage2Since_newest _ _ _ hs
    refine ⟨rfl, ?_, ?_⟩
    · simp only [watermark]; split <;> omega
    · simp only [hn, Option.getD_some]; exact hlt
  · cases h

theorem best_mem : ∀ (l : List Pass) (b : Pass), best l = some b → b ∈ l
  | [], _, h => by cases h
  | p :: ps, b, h => by
    simp only [best, Option.some.injEq] at h
    subst h
    suffices ∀ (cur : Pass), (cur = p ∨ cur ∈ ps) → ∀ (qs : List Pass), (∀ q ∈ qs, q ∈ ps) →
        (qs.foldl (fun cur q => if outranks q cur then q else cur) cur = p ∨
          qs.foldl (fun cur q => if outranks q cur then q else cur) cur ∈ ps) by
      rcases this p (Or.inl rfl) ps (fun q hq => hq) with h | h
      · rw [h]; exact List.mem_cons_self ..
      · exact List.mem_cons_of_mem _ h
    intro cur hcur qs hqs
    induction qs generalizing cur with
    | nil => exact hcur
    | cons q qs ih =>
      simp only [List.foldl_cons]
      apply ih
      · split
        · exact Or.inr (hqs q (List.mem_cons_self ..))
        · exact hcur
      · exact fun x hx => hqs x (List.mem_cons_of_mem _ hx)

/-- Nothing in the list outranks its best. -/
theorem best_max : ∀ (l : List Pass) (b : Pass), best l = some b → ∀ q ∈ l, outranks q b = false
  | [], _, h => by cases h
  | p :: ps, b, h => by
    simp only [best, Option.some.injEq] at h
    subst h
    suffices ∀ (cur : Pass) (seen : List Pass), (∀ q ∈ seen, outranks q cur = false) →
        outranks cur cur = false →
        ∀ (qs : List Pass), ∀ q ∈ seen ++ qs,
          outranks q (qs.foldl (fun cur q => if outranks q cur then q else cur) cur) = false by
      intro q hq
      have := this p [p] (by simp [outranks_irrefl]) (outranks_irrefl p) ps q
        (by simpa using hq)
      exact this
    intro cur seen hseen hcc qs
    induction qs generalizing cur seen with
    | nil => simpa using hseen
    | cons x xs ih =>
      intro q hq
      simp only [List.foldl_cons]
      have hq' : q ∈ (seen ++ [x]) ++ xs := by simpa using hq
      apply ih _ (seen ++ [x]) _ (by
        split
        · exact outranks_irrefl x
        · exact hcc) q hq'
      intro y hy
      simp only [List.mem_append, List.mem_singleton] at hy
      split
      · rename_i hx
        rcases hy with hy | hy
        · cases hyx : outranks y x
          · rfl
          · have := outranks_trans y x cur hyx hx
            rw [hseen y hy] at this; cases this
        · subst hy; exact outranks_irrefl y
      · rename_i hx
        rcases hy with hy | hy
        · exact hseen y hy
        · subst hy; simpa using hx

theorem mem_decideV (ev : Nat → Evidence) (window : Int) (probes : List Probe) (v : Verdict)
    (hv : v ∈ decideV ev window probes) :
    ∃ w ∈ winners ev window probes, v = w.verdict ∧
      ∀ q ∈ winners ev window probes, q.probe.target = w.probe.target → outranks q w = false := by
  simp only [decideV, List.mem_filterMap, Option.map_eq_some_iff] at hv
  obtain ⟨t, _, w, hb, rfl⟩ := hv
  have hw := best_mem _ w hb
  rw [List.mem_filter] at hw
  refine ⟨w, hw.1, rfl, ?_⟩
  intro q hq hqt
  apply best_max _ w hb q
  rw [List.mem_filter]
  refine ⟨hq, ?_⟩
  have := hw.2
  simp only [beq_iff_eq] at this ⊢
  rw [hqt, this]

theorem mem_winners (ev : Nat → Evidence) (window : Int) (probes : List Probe) (w : Pass)
    (hw : w ∈ winners ev window probes) :
    w ∈ passing ev window probes ∧ w.probe.reverseOnly = false ∧
      beaten (passing ev window probes) w = false := by
  simp only [winners, List.mem_filter, Bool.and_eq_true, Bool.not_eq_true'] at hw
  exact ⟨hw.1, hw.2.1, hw.2.2⟩

theorem mem_passing (ev : Nat → Evidence) (window : Int) (probes : List Probe) (w : Pass)
    (hw : w ∈ passing ev window probes) :
    w.probe ∈ probes ∧ w.probe.baseline ≤ w.watermark ∧ w.watermark < w.newest := by
  simp only [passing, List.mem_filterMap] at hw
  obtain ⟨p, hp, he⟩ := hw
  obtain ⟨h1, h2, h3⟩ := evaluate_some ev window p w he
  rw [h1]; exact ⟨hp, h2, h3⟩

/-- No probe pairs a concept with itself (proved of the gathered probes by
`probesFor_ok`). -/
def NoSelf (probes : List Probe) : Prop := ∀ p ∈ probes, p.target ≠ p.source

/-- A verdict comes from a forward probe of the list, and the target's
support is older than the source's: the source's newest counted evidence is
strictly after the target's watermark, which is at least its baseline. -/
theorem verdict_sound (ev : Nat → Evidence) (window : Int) (probes : List Probe) (v : Verdict)
    (hv : v ∈ decideV ev window probes) :
    ∃ w ∈ passing ev window probes, v = w.verdict ∧ w.probe ∈ probes ∧
      w.probe.reverseOnly = false ∧ w.probe.baseline ≤ v.watermark ∧ v.watermark < w.newest := by
  obtain ⟨w, hw, rfl, _⟩ := mem_decideV ev window probes v hv
  obtain ⟨hp, hr, _⟩ := mem_winners ev window probes w hw
  obtain ⟨h1, h2, h3⟩ := mem_passing ev window probes w hp
  exact ⟨w, hp, rfl, h1, hr, h2, h3⟩

/-- **Never by itself.** -/
theorem verdict_ne (ev : Nat → Evidence) (window : Int) (probes : List Probe) (hn : NoSelf probes)
    (v : Verdict) (hv : v ∈ decideV ev window probes) : v.target ≠ v.source := by
  obtain ⟨w, _, rfl, hp, _⟩ := verdict_sound ev window probes v hv
  exact hn _ hp

/-- **The pair rule.** Of two concepts, at most one supersedes the other in
a cycle: there are never verdicts both ways. -/
theorem pair_rule (ev : Nat → Evidence) (window : Int) (probes : List Probe) (hn : NoSelf probes)
    (v u : Verdict) (hv : v ∈ decideV ev window probes) (hu : u ∈ decideV ev window probes)
    (h1 : u.target = v.source) (h2 : u.source = v.target) : False := by
  obtain ⟨w, hw, rfl, _⟩ := mem_decideV ev window probes v hv
  obtain ⟨x, hx, rfl, _⟩ := mem_decideV ev window probes u hu
  obtain ⟨hwp, _, hwb⟩ := mem_winners ev window probes w hw
  obtain ⟨hxp, _, hxb⟩ := mem_winners ev window probes x hx
  have hne : w.probe.target ≠ w.probe.source := hn _ (mem_passing ev window probes w hwp).1
  simp only [Pass.verdict] at h1 h2
  have hs : w.probe.source ≠ x.probe.source := by rw [h2]; exact Ne.symm hne
  have bw : ∀ b ∈ passing ev window probes, b.probe.target = w.probe.source →
      b.probe.source = w.probe.target → outranks b w = false := by
    intro b hb ht hs'
    cases ho : outranks b w
    · rfl
    · have : beaten (passing ev window probes) w = true := by
        simp only [beaten, List.any_eq_true, Bool.and_eq_true, beq_iff_eq]
        exact ⟨b, hb, ⟨ht, hs'⟩, ho⟩
      rw [hwb] at this; cases this
  have bx : ∀ b ∈ passing ev window probes, b.probe.target = x.probe.source →
      b.probe.source = x.probe.target → outranks b x = false := by
    intro b hb ht hs'
    cases ho : outranks b x
    · rfl
    · have : beaten (passing ev window probes) x = true := by
        simp only [beaten, List.any_eq_true, Bool.and_eq_true, beq_iff_eq]
        exact ⟨b, hb, ⟨ht, hs'⟩, ho⟩
      rw [hxb] at this; cases this
  rcases outranks_total w x hs with h | h
  · have := bx w hwp (by rw [h2]) (by rw [h1]); rw [h] at this; cases this
  · have := bw x hxp h1 h2; rw [h] at this; cases this

/-- **The older side is demoted.** When the reverse probe of a verdict
passes too, the verdict's own pass outranks it: the demoted target's newest
counted evidence against the source is at most the source's, and on a tie
the source has the smaller key, then the smaller id. -/
theorem verdict_outranks_reverse (ev : Nat → Evidence) (window : Int) (probes : List Probe)
    (hn : NoSelf probes) (v : Verdict) (hv : v ∈ decideV ev window probes) :
    ∃ w ∈ passing ev window probes, v = w.verdict ∧
      ∀ b ∈ passing ev window probes, b.probe.target = v.source → b.probe.source = v.target →
        outranks w b = true ∧ b.newest ≤ w.newest := by
  obtain ⟨w, hw, rfl, _⟩ := mem_decideV ev window probes v hv
  obtain ⟨hwp, _, hwb⟩ := mem_winners ev window probes w hw
  refine ⟨w, hwp, rfl, ?_⟩
  intro b hb ht hs
  simp only [Pass.verdict] at ht hs
  have hne : w.probe.target ≠ w.probe.source := hn _ (mem_passing ev window probes w hwp).1
  have hnb : outranks b w = false := by
    cases ho : outranks b w
    · rfl
    · have : beaten (passing ev window probes) w = true := by
        simp only [beaten, List.any_eq_true, Bool.and_eq_true, beq_iff_eq]
        exact ⟨b, hb, ⟨ht, hs⟩, ho⟩
      rw [hwb] at this; cases this
  have hsrc : w.probe.source ≠ b.probe.source := by rw [hs]; exact Ne.symm hne
  rcases outranks_total w b hsrc with h | h
  · refine ⟨h, ?_⟩
    simp only [outranks, decide_eq_true_eq] at h
    omega
  · rw [hnb] at h; cases h

/-- **The newest evidence wins** a target several sources qualify for
(Rust test `the_source_with_the_newest_evidence_wins`): no other standing
forward pass for the same target outranks the winner. -/
theorem verdict_best (ev : Nat → Evidence) (window : Int) (probes : List Probe) (v : Verdict)
    (hv : v ∈ decideV ev window probes) :
    ∃ w ∈ winners ev window probes, v = w.verdict ∧
      ∀ q ∈ winners ev window probes, q.probe.target = v.target → q.newest ≤ w.newest := by
  obtain ⟨w, hw, rfl, hmax⟩ := mem_decideV ev window probes v hv
  refine ⟨w, hw, rfl, ?_⟩
  intro q hq hqt
  have := hmax q hq hqt
  simp only [outranks, decide_eq_false_iff_not] at this
  omega

/-! ## Gathering the probes

A node is what the gather reads of a concept. `suppressed` is
`self_suppressed` by contradiction edge id: the dependence edges into the
target that do not count against that contradiction, or `none` when the
declarer, or a piece of evidence that might be the declarer's, cannot be told
apart: an ambiguity never demotes. Writer attribution itself is not modelled. -/

structure Node where
  id : Nat
  session : Nat
  status : Status
  lastDemotion : Option Int
  key : Nat
  /-- `baseline`: the origin about-time clamped, or the write time. -/
  baseline : Int
  deriving DecidableEq, Repr

structure World where
  session : Nat
  nodes : List Node
  edges : List Edge
  suppressed : Nat → Option (List Nat)

/-- `graph.node(id)` as a concept. -/
def World.node (W : World) (i : Nat) : Option Node := W.nodes.find? (·.id == i)

theorem World.node_id (W : World) (i : Nat) (c : Node) (h : W.node i = some c) : c.id = i := by
  have := List.find?_some h
  simpa using this

/-- `usable`: a concept of the session outside its re-promotion cooldown. -/
def usable (W : World) (cooldown now : Int) (i : Nat) : Option Node :=
  match W.node i with
  | some c =>
    if c.session = W.session ∧ Canon.inCooldown c.lastDemotion cooldown now = false then some c
    else none
  | none => none

/-- `contradictions`: the `Contradicts` edges into `t` from another concept,
written by `now`, whose source is usable, with that source. The Rust sorts
them by edge id; the order matters only for two edges joining the same
pair. -/
def contradictions (W : World) (cooldown now : Int) (t : Node) : List (Edge × Node) :=
  (W.edges.filter fun e =>
      e.ty == .contradicts && e.dst == t.id && e.src != t.id && decide (e.created ≤ now)).filterMap
    fun e => (usable W cooldown now e.src).map fun s => (e, s)

/-- `probe`: `None` when the attribution is ambiguous. -/
def mkProbe (supp : Nat → Option (List Nat)) (t : Node) (e : Edge) (s : Node) (rev : Bool) :
    Option Probe :=
  (supp e.id).map fun ex => ⟨t.id, s.id, s.key, e.id, t.baseline, ex, rev⟩

/-- The forward probes of one target and the ambiguous `(target, source)`
pairs met on the way. -/
def forwardOf (node : Nat → Option Node) (contra : Node → List (Edge × Node))
    (supp : Nat → Option (List Nat)) (t : Nat) : List Probe × List (Nat × Nat) :=
  match node t with
  | none => ([], [])
  | some tn =>
    (contra tn).foldl
      (fun acc es =>
        match mkProbe supp tn es.1 es.2 false with
        | some p => (acc.1 ++ [p], acc.2)
        | none => (acc.1, acc.2 ++ [(tn.id, es.2.id)]))
      ([], [])

/-- The reverse probe of a forward probe whose target also contradicts its
source, for the pair rule. -/
def reverseOf (node : Nat → Option Node) (contra : Node → List (Edge × Node))
    (supp : Nat → Option (List Nat)) (fwd : List Probe) (p : Probe) :
    List Probe × List (Nat × Nat) :=
  if fwd.any (fun q => q.target == p.source && q.source == p.target) then ([], [])
  else
    match node p.source, node p.target with
    | some t, some _ =>
      match (contra t).find? (fun es => es.2.id == p.target) with
      | some (e, s) =>
        match mkProbe supp t e s true with
        | some r => ([r], [])
        | none => ([], [(t.id, s.id)])
      | none => ([], [])
    | _, _ => ([], [])

/-- `probes_for` without the probe cap: the forward probes of `targets` in
order, the reverse probes, and neither direction of an ambiguous pair. -/
def probesWith (node : Nat → Option Node) (contra : Node → List (Edge × Node))
    (supp : Nat → Option (List Nat)) (targets : List Nat) : List Probe :=
  let fw := targets.map (forwardOf node contra supp)
  let fwd := (fw.map (·.1)).flatten
  let rv := fwd.map (reverseOf node contra supp fwd)
  let amb := (fw.map (·.2)).flatten ++ (rv.map (·.2)).flatten
  (fwd ++ (rv.map (·.1)).flatten).filter fun p => !amb.contains (p.source, p.target)

def probesFor (W : World) (cooldown now : Int) (targets : List Nat) : List Probe :=
  probesWith W.node (contradictions W cooldown now) W.suppressed targets

/-- The ring of `gather`: Canonical, usable, with a live contradiction. The
batch window, the ring cursor and the probe cap choose a sublist of it;
every result below about `probesFor` holds for any target list. -/
def ring (W : World) (cooldown now : Int) : List Nat :=
  sortDedup ((W.nodes.filter fun c =>
      c.status == .canonical && (usable W cooldown now c.id).isSome &&
        !(contradictions W cooldown now c).isEmpty).map (·.id))

/-- `gather`: nothing unless `canonization_supersede` is on. -/
def gather (W : World) (supersede : Bool) (cooldown now : Int) : List Probe :=
  if supersede then probesFor W cooldown now (ring W cooldown now) else []

/-- `gather_holds`: the hold's probes for Stage 3's window, nothing unless
the option is on. -/
def gatherHolds (W : World) (supersede : Bool) (cooldown now : Int) (window : List Nat) :
    List Probe :=
  if supersede then
    probesFor W cooldown now (window.filter fun i => (usable W cooldown now i).isSome)
  else []

/-- What a gathered probe rests on: a `Contradicts` edge of the world from
its source into its target, two different concepts. -/
def ProbeOK (W : World) (p : Probe) : Prop :=
  p.target ≠ p.source ∧
    ∃ e ∈ W.edges, e.ty = .contradicts ∧ e.id = p.edge ∧ e.src = p.source ∧ e.dst = p.target

theorem mem_contradictions (W : World) (cooldown now : Int) (t : Node) (e : Edge) (s : Node)
    (h : (e, s) ∈ contradictions W cooldown now t) :
    e ∈ W.edges ∧ e.ty = .contradicts ∧ e.dst = t.id ∧ e.src ≠ t.id ∧ s.id = e.src := by
  simp only [contradictions, List.mem_filterMap, List.mem_filter, Option.map_eq_some_iff] at h
  obtain ⟨e', ⟨he', hc⟩, s', hs', heq⟩ := h
  simp only [Prod.mk.injEq] at heq
  obtain ⟨rfl, rfl⟩ := heq
  simp only [Bool.and_eq_true, beq_iff_eq, bne_iff_ne, ne_eq, decide_eq_true_eq] at hc
  refine ⟨he', hc.1.1.1, hc.1.1.2, hc.1.2, ?_⟩
  unfold usable at hs'
  split at hs'
  · rename_i c hc'
    split at hs'
    · simp only [Option.some.injEq] at hs'; subst hs'; exact World.node_id W _ _ hc'
    · cases hs'
  · cases hs'

theorem mkProbe_ok (W : World) (cooldown now : Int) (t : Node) (e : Edge) (s : Node) (rev : Bool)
    (p : Probe) (hm : (e, s) ∈ contradictions W cooldown now t)
    (hp : mkProbe W.suppressed t e s rev = some p) : ProbeOK W p := by
  obtain ⟨he, ht, hd, hne, hs⟩ := mem_contradictions W cooldown now t e s hm
  simp only [mkProbe, Option.map_eq_some_iff] at hp
  obtain ⟨ex, _, rfl⟩ := hp
  refine ⟨?_, e, he, ht, rfl, hs.symm, hd⟩
  simp only []; rw [hs, ← hd]; exact Ne.symm (hd ▸ hne)

theorem forwardOf_ok (W : World) (cooldown now : Int) (t : Nat) (p : Probe)
    (hp : p ∈ (forwardOf W.node (contradictions W cooldown now) W.suppressed t).1) :
    ProbeOK W p := by
  unfold forwardOf at hp
  split at hp
  · simp at hp
  · rename_i tn _
    suffices ∀ (l : List (Edge × Node)) (acc : List Probe × List (Nat × Nat)),
        (∀ es ∈ l, es ∈ contradictions W cooldown now tn) → (∀ q ∈ acc.1, ProbeOK W q) →
        ∀ q ∈ (l.foldl (fun acc es =>
          match mkProbe W.suppressed tn es.1 es.2 false with
          | some p => (acc.1 ++ [p], acc.2)
          | none => (acc.1, acc.2 ++ [(tn.id, es.2.id)])) acc).1, ProbeOK W q by
      exact this _ _ (fun es h => h) (by simp) p hp
    intro l
    induction l with
    | nil => intro acc _ hacc; exact hacc
    | cons es rest ih =>
      intro acc hl hacc
      simp only [List.foldl_cons]
      apply ih _ (fun x hx => hl x (List.mem_cons_of_mem _ hx))
      cases hm : mkProbe W.suppressed tn es.1 es.2 false with
      | none => exact hacc
      | some q =>
        intro x hx
        simp only [List.mem_append, List.mem_singleton] at hx
        rcases hx with hx | hx
        · exact hacc x hx
        · subst hx
          exact mkProbe_ok W cooldown now tn es.1 es.2 false _ (hl es (List.mem_cons_self ..)) hm

theorem reverseOf_ok (W : World) (cooldown now : Int) (fwd : List Probe) (q p : Probe)
    (hp : p ∈ (reverseOf W.node (contradictions W cooldown now) W.suppressed fwd q).1) :
    ProbeOK W p := by
  unfold reverseOf at hp
  split at hp
  · simp at hp
  · split at hp
    · rename_i t _ _ _
      split at hp
      · rename_i e s hf
        have hm := List.mem_of_find?_eq_some hf
        split at hp
        · rename_i r hr
          simp only [List.mem_singleton] at hp; subst hp
          exact mkProbe_ok W cooldown now t e s true _ hm hr
        · simp at hp
      · simp at hp
    · simp at hp

/-- **Every gathered probe rests on a `Contradicts` edge between two
different concepts**, so no concept is ever probed, and so never superseded,
by itself. -/
theorem probesFor_ok (W : World) (cooldown now : Int) (targets : List Nat) (p : Probe)
    (hp : p ∈ probesFor W cooldown now targets) : ProbeOK W p := by
  simp only [probesFor, probesWith, List.mem_filter, List.mem_append, List.mem_flatten,
    List.mem_map] at hp
  obtain ⟨hp | hp, _⟩ := hp
  · obtain ⟨l, ⟨fw, ⟨t, _, rfl⟩, rfl⟩, hl⟩ := hp
    exact forwardOf_ok W cooldown now t p hl
  · obtain ⟨l, ⟨rv, ⟨q, _, rfl⟩, rfl⟩, hl⟩ := hp
    exact reverseOf_ok W cooldown now _ q p hl

/-- The supersession pass never probes a concept against itself. -/
theorem probesFor_noSelf (W : World) (cooldown now : Int) (targets : List Nat) :
    NoSelf (probesFor W cooldown now targets) :=
  fun p hp => (probesFor_ok W cooldown now targets p hp).1

/-- **Option off: nothing is probed** (`gather` and `gather_holds` return
`Probes::default()`), so nothing is decided. -/
theorem gather_off (W : World) (cooldown now : Int) : gather W false cooldown now = [] := rfl

/-- With the supersession option off, `gather_holds` returns no probes. -/
theorem gatherHolds_off (W : World) (cooldown now : Int) (window : List Nat) :
    gatherHolds W false cooldown now window = [] := rfl

/-- **The #198 edge is never read as a contradiction.** Adding
`SuspectedSupersedes` edges changes no contradiction, so no probe and no
supersession verdict. -/
theorem contradictions_ignore_suspected (W : World) (F : List Edge)
    (hF : ∀ e ∈ F, e.ty = .suspected) (cooldown now : Int) :
    contradictions { W with edges := W.edges ++ F } cooldown now =
      contradictions W cooldown now := by
  funext t
  simp only [contradictions, List.filter_append, List.filterMap_append]
  have : F.filter (fun e => e.ty == .contradicts && e.dst == t.id && e.src != t.id &&
      decide (e.created ≤ now)) = [] := by
    rw [List.filter_eq_nil_iff]
    intro e he
    simp [hF e he]
  rw [this]
  simp
  rfl

/-- The supersession pass's probes ignore `SuspectedSupersedes` edges: adding
any changes no probe. -/
theorem probesFor_ignore_suspected (W : World) (F : List Edge)
    (hF : ∀ e ∈ F, e.ty = .suspected) (cooldown now : Int) (targets : List Nat) :
    probesFor { W with edges := W.edges ++ F } cooldown now targets =
      probesFor W cooldown now targets := by
  unfold probesFor
  rw [contradictions_ignore_suspected W F hF]
  rfl

/-! ## Applying the verdicts

Each verdict is re-checked at apply time (`still_holds`). `holds v` stands
for every check but the target's status: it reads the edge, the evidence,
sessions and the cooldowns as of the start of the cycle, none of which an
earlier supersession of the cycle changes. The status check is explicit: the
target must be Canonical *now*. The event then goes through the write gate
(`Canon.gate`) as `Canonical → None`. Supersession writes no edge and changes
no other concept. -/

/-- The supersession pass of `apply`, on the statuses. -/
def applySup (holds : Verdict → Bool) : (Nat → Status) → List Verdict → (Nat → Status)
  | st, [] => st
  | st, v :: vs =>
    let st' : Nat → Status :=
      if holds v ∧ st v.target = .canonical then
        match gate (st v.target) .canonical .none with
        | some s => fun n => if n = v.target then s else st n
        | none => st
      else st
    applySup holds st' vs

/-- Whether some verdict that holds names `n`. -/
def superFlag (holds : Verdict → Bool) (vs : List Verdict) (n : Nat) : Bool :=
  vs.any fun v => v.target == n && holds v

theorem applySup_eq (holds : Verdict → Bool) :
    ∀ (vs : List Verdict) (st : Nat → Status) (n : Nat),
      applySup holds st vs n =
        if st n = .canonical ∧ superFlag holds vs n = true then .none else st n := by
  intro vs
  induction vs with
  | nil => intro st n; simp [applySup, superFlag]
  | cons v vs ih =>
    intro st n
    simp only [applySup]
    rw [ih]
    by_cases hc : holds v = true ∧ st v.target = .canonical
    · rw [ite_eq_left hc]
      have hg : gate (st v.target) .canonical .none = some .none := by
        simp [gate, hc.2, legal]
      rw [hg]
      by_cases hn : n = v.target
      · subst hn
        simp [superFlag, hc.1, hc.2]
      · have hb : (v.target == n) = false := by simp [Ne.symm hn]
        simp [hn, superFlag, hb]
    · rw [ite_eq_right hc]
      by_cases hn : n = v.target
      · subst hn
        by_cases hst : st v.target = .canonical
        · have : holds v = false := by
            cases h : holds v
            · rfl
            · exact absurd ⟨h, hst⟩ hc
          simp [superFlag, this]
        · simp [hst]
      · have hb : (v.target == n) = false := by simp [Ne.symm hn]
        simp [superFlag, hb]

/-- **Agreement with `Canon.cycle`'s supersession pass.** On every concept the
pass is `Canon.cycle` with the supersede flag "some holding verdict names it"
and every other flag off. -/
theorem applySup_eq_cycle (holds : Verdict → Bool) (vs : List Verdict) (st : Nat → Status)
    (n : Nat) :
    applySup holds st vs n = Canon.cycle (st n) ⟨superFlag holds vs n, false, false, false, false⟩ := by
  rw [applySup_eq]
  cases h : st n <;> cases superFlag holds vs n <;> rfl

/-- **Only along the one legal demotion.** A concept the pass changes was
Canonical and is now None, a transition the write gate admits; by
`Canon.legal_demotion_unique` it is the only rank-decreasing one there is. -/
theorem applySup_legal (holds : Verdict → Bool) (vs : List Verdict) (st : Nat → Status) (n : Nat)
    (h : applySup holds st vs n ≠ st n) :
    st n = .canonical ∧ applySup holds st vs n = .none ∧
      legal (st n) (applySup holds st vs n) = true := by
  rw [applySup_eq] at h ⊢
  by_cases hc : st n = .canonical ∧ superFlag holds vs n = true
  · rw [ite_eq_left hc]; exact ⟨hc.1, rfl, by rw [hc.1]; rfl⟩
  · rw [ite_eq_right hc] at h; exact absurd rfl h

/-- **The one legal demotion.** Whatever the pass changes moves along the
unique rank-decreasing edge of the gate (`Canon.legal_demotion_unique`): it
lowers the rank, and every legal demotion is this one. -/
theorem applySup_unique (holds : Verdict → Bool) (vs : List Verdict) (st : Nat → Status) (n : Nat)
    (h : applySup holds st vs n ≠ st n) :
    (applySup holds st vs n).rank < (st n).rank ∧
      ∀ a b, legal a b = true → b.rank < a.rank → a = st n ∧ b = applySup holds st vs n := by
  obtain ⟨h1, h2, _⟩ := applySup_legal holds vs st n h
  refine ⟨by rw [h1, h2]; decide, ?_⟩
  intro a b hl hr
  obtain ⟨ha, hb⟩ := Canon.legal_demotion_unique a b hl hr
  rw [ha, hb, h1, h2]; exact ⟨rfl, rfl⟩

/-- **Never promotes.** No concept's rank rises, the source's included. -/
theorem applySup_rank_le (holds : Verdict → Bool) (vs : List Verdict) (st : Nat → Status) (n : Nat) :
    (applySup holds st vs n).rank ≤ (st n).rank := by
  rw [applySup_eq]
  split
  · simp [Status.rank]
  · exact Nat.le_refl _

/-- Only a verdict's target can change. -/
theorem applySup_untouched (holds : Verdict → Bool) (vs : List Verdict) (st : Nat → Status)
    (n : Nat) (h : ∀ v ∈ vs, v.target ≠ n) : applySup holds st vs n = st n := by
  rw [applySup_eq]
  have : superFlag holds vs n = false := by
    simp only [superFlag, List.any_eq_false, Bool.and_eq_true, beq_iff_eq, not_and]
    intro v hv ht; exact absurd ht (h v hv)
  simp [this]

/-- **The source is not promoted, nor touched**, unless another verdict
demotes it. -/
theorem applySup_source (holds : Verdict → Bool) (vs : List Verdict) (st : Nat → Status)
    (v : Verdict) (_ : v ∈ vs) (h : ∀ u ∈ vs, u.target ≠ v.source) :
    applySup holds st vs v.source = st v.source :=
  applySup_untouched holds vs st v.source h

/-- **Option off agrees with plain canonization.** With
`canonization_supersede` off the probes are empty, so no verdict, so every
concept's supersede flag is false and the cycle is `Canon.cycle` with no
supersession; the hold is empty too, so Stage 3 is unchanged. -/
theorem off_agrees (W : World) (ev : Nat → Evidence) (window cooldown now : Int)
    (holds : Verdict → Bool) (n : Nat) (s : Status) (f : Canon.Flags) :
    Canon.cycle s { f with supersede := superFlag holds (decideV ev window (gather W false cooldown now)) n } =
      Canon.cycle s { f with supersede := false } := rfl

/-- With the supersession option off, the canon supersession pass demotes
nothing: `gather` probes nothing, so no verdict is applied. -/
theorem off_no_demotion (W : World) (ev : Nat → Evidence) (window cooldown now : Int)
    (holds : Verdict → Bool) (st : Nat → Status) :
    applySup holds st (decideV ev window (gather W false cooldown now)) = st := rfl

/-- The held Venerables: the targets of the hold's verdicts. Stage 3 skips
them. -/
def held (ev : Nat → Evidence) (window : Int) (probes : List Probe) (n : Nat) : Bool :=
  (decideV ev window probes).any fun v => v.target == n

/-- With the supersession option off, `gather_holds` holds no concept, so no
replacement is held off. -/
theorem held_off (W : World) (ev : Nat → Evidence) (window cooldown now : Int) (win : List Nat)
    (n : Nat) : held ev window (gatherHolds W false cooldown now win) n = false := rfl

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
