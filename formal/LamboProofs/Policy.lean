import LamboProofs.Canon

/-
# The solo canonization policy: sessions, score, bands, admission

Model of

* `separated_session_count` (`src/canon/event_time.rs`): the greedy count of
  `gap`-separated session starts;
* `supporting_interaction_times`, `recurrence_sessions`, `revert_count`,
  `raw_solo_score`, `solo_score`, `classify`, `SoloScorer::candidates`,
  `SoloScorer::admits_hop`, `SwarmScorer`, the weights `SESSION_WEIGHT`,
  `HUMAN_CONFIRMED_WEIGHT`, `DEPENDENT_INTERACTION_WEIGHT`, `REVERT_WEIGHT`,
  the bars `CANDIDATE_BAR`, `VENERABLE_BAR`, `CANONICAL_BAR`,
  `SESSION_SEPARATION`, and `PromotionPolicy` (`ALL`, `#[default]`, `scorer`,
  `as_str`, `from_str`) (`src/canon/policy.rs`);
* `Interaction::about_time` and `ConceptType::eviction_resistance`
  (`src/types/mod.rs`), and `Edge::dependence` / `EdgeType::dependent_end`
  for one comparison;
* how `Evaluator::gather` and `apply` (`src/canon/eval.rs`) consult the policy,
  as `Canon.Flags`, so `Canon.cycle` applies unchanged;
* since #300, the Solo terms' dating of a dependence edge by the turn that
  wrote it: `Edge::about_time` and the writing interaction
  `WriterTimeline` resolves (`src/daemon/conflict.rs`), read by
  `supporting_interaction_times` and `solo_dependent_interactions`
  (`src/canon/stage1.rs`). The previous dating (by the dependent's origin
  interaction) stays beside it: `writer`, `supportingTimes`,
  `recurrenceSessions`, `dependentOrigins`. Swarm's Stage 1 count and
  Stage 2's span still read the dependent's origin, which is what
  `dependentOrigins` models.

The design is `dev-diary/lambo-for-mooshik/C-solopolicy.md` (spec §3.2's
formula, with the valid-action term replaced by dependent interactions in #36
PR 3) and `D-event-clock.md` (event time).

## Model

Instants are integers (seconds in the examples; any unit works) and `gap` a
natural number. Scores use exact arithmetic: every eviction resistance is a
whole number of tenths, so `score10 = 10 × solo_score` exactly, and the bars
are 30, 60 and 100. The Rust computes in `f64`.

* **Bands: `f64` agrees with the model.** The weights are whole numbers and
  the terms integer counts, so the raw sum is exact in `f64` (below `2^53`).
  The exact resistant score is a multiple of 0.1, and the `f64` product is
  within a tiny relative error of it, so the `f64` band can differ from the
  exact band only when the exact score sits exactly on a bar. The integer
  raw scores that land on a bar under a non-unit resistance are `5 × 1.2`,
  `2 × 1.5` and `4 × 1.5` (1.1 and 0.7 never land on 3, 6 or 10; 1.0 is
  exact), and these compute to exactly `6.0`, `3.0` and `6.0` in `f64`. (An
  argument, not a Lean proof: `f64` is not modelled.)
* **Order: `f64` does not agree with the model on exact ties.** Two concepts
  of different types with *equal* exact scores can get different `f64`
  scores: Logic raw 7 gives `7.700000000000001` and Observation raw 11 gives
  `7.699999999999999` (both exactly 7.7); Entity raw 11 gives `13.2` and Logic
  raw 12 gives `13.200000000000001`. `SoloScorer::candidates` sorts by
  `sb.total_cmp(sa)` first, so Rust puts the Logic concept first whatever the
  keys, while the model sees a tie and orders by key
  (`exact_tie_orders_by_key`). `soloCandidates_sorted` is the order in exact
  arithmetic. Membership (`mem_soloCandidates`) is unaffected; only which
  members fit under the evaluator's `batch_size` can differ.

Graphs are lists: interactions (id, flush time, optional event time),
concepts (id, origin interaction) and edges (source, target, type). Lookups
use the first match, so ids are taken to be unique, as `Graph` guarantees.
An edge's stamps (about-time, resolved writer) are a function of the edge,
since a graph holds one edge per source, target and type; how
`WriterTimeline` finds the writer (an exact `created_at` match) is not
modelled, its answer is the input `Stamp.writer`. Keys and ids in the Stage-1 order are natural numbers standing for their
orders. Strings are lists of characters; `str::trim`'s whitespace is the
Unicode `White_Space` set and `to_ascii_lowercase` is `Char.toLower`.

Left out: the dependent-interaction count's `min_age` floor and its graph
walk (`dependent_interactions` is an input to the score, except in
`dependentOrigins`, which clears the floor), the evaluator's batch cap, ring
cursors and Stage 2/3 measurements (their verdicts are inputs, as in
`Canon.lean`), `chrono::Duration::MAX` saturation of `gap` (no two instants
are that far apart, so it changes nothing), error message text, and serde.

## Results

The session counter (`sessionCount gap starts`):

* `sessionCount_congr`, `sessionCount_perm`, `sessionCount_dup`: it depends
  only on which instants occur, so it is order-free and ignores exact repeats
  ("The rule is greedy and order-free ... the same instant is one session
  start"). `canon_eq_of_strict`: any "sort, then dedup" gives the same list.
* `sessionCount_pos`, `sessionCount_le_distinct`, `sessionCount_le_length`,
  `sessionCount_nil`: at least 1 for non-empty input, at most the number of
  distinct starts.
* `sessionCount_max`, `sessionCount_attained`: **the greedy count is the
  maximum number of pairwise `gap`-separated starts**; it is exact, not a
  lower bound.
* `sessionCount_mono`, `sessionCount_le_cons`, `sessionCount_cons_le`: adding
  a start never lowers the count and raises it by at most one.
  `sessionCount_antitone_gap`: a wider gap never counts more.
* `sessionCount_zero_gap`: with gap 0 it counts distinct starts.
* `sessionCount_window`: starts all less than `gap` after the earliest count
  as one ("Dense clusters contribute one session each");
  `window_needs_earliest`: measured from the first start in input order this
  fails. `chain_counts_more_than_one`: a chain of starts 23 h apart, one
  cluster by single linkage, counts three.
* `separation_counts_clusters_once_and_stragglers_each`: the Rust test.

Support and event time:

* `causal_not_recurrence` (test `causal_touches_are_not_recurrence`, #36 PR
  3): an edge other than `Derives`/`Dependency` (`Causal`, `Contradicts`, ...)
  never changes the recurrence term; `recurrence_mono_edge`: no edge lowers it.
* `supportingTimes_reflush`, `recurrence_reflush`: when every interaction has
  an event time, flush times are irrelevant ("never flush stamps");
  `aboutTime_fallback`: without one, the flush time is used.
* `a_bulk_ingest_recurs_only_under_event_time` (`decide`, the Rust test): the
  same four turns flushed within a minute are four sessions on event time and
  one on flush time; `bulk_ingest_band`: Candidate (4.8) against None (1.2).
  `causal_touches_are_not_recurrence`, `recurring_burst_once`: the other two
  corpora.
* `hierarchical_child_is_dependent_not_recurrence`,
  `reused_action_dependency_is_dated_by_its_origin`: see "Findings".

The fixed dating (#300; `supportingTimesW`, `recurrenceSessionsW`,
`dependentOriginsW`, over per-edge `Stamp`s):

* `reused_action_dependency_is_dated_by_its_writer` (`decide`, the issue's
  scenario): the reused action's day-5 dependency on `Y` adds the day-5
  session and the day-5 turn as a dependent, exactly as a fresh action node
  does; `dependency_time_supports`: in general, a `Dependency` edge supports
  on its own about-time.
* `dependentOriginsW_not_own`: the own-origin exclusion is about the
  writing turn; `own_origin_is_the_writing_turn` (`decide`): a dependence
  written in the pillar's own turn no longer counts, even from an older
  concept.
* `supportingTimesW_eq`, `recurrenceSessionsW_eq`, `dependentOriginsW_eq`:
  **agreement with the old dating** whenever every dependence edge was
  written in its dependent's origin turn (`WrittenAtOrigin`).
* `recurrenceW_mono_edge`, `dependentOriginsW_mono_edge`,
  `causal_not_recurrenceW`: no edge lowers either fixed term, and causal
  touches still are not recurrence.
* `policy_cycle_one_hop_fixed`, `band_mono_fixed_edge`: the score, band and
  cycle results take any terms, so they hold for the fixed ones.

The score and bands:

* `score10_mono`, `rawScore_mono`, `band_mono`: monotone in sessions,
  confirmations and dependent interactions, antitone in reverts;
  `score10_steps`: each unit moves the score by exactly its weight times the
  resistance.
* `classify_none_iff`, `classify_candidate_iff`, `classify_venerable_iff`,
  `classify_canonical_iff`: the bands are the intervals `(-∞, 3)`, `[3, 6)`,
  `[6, 10)`, `[10, ∞)`, so they partition the score range; `classify_mono`.
* `band_boundaries_are_inclusive`, `score_examples`: the Rust tests' numbers.
* `revert_is_demotion`: through the write gate every counted revert is
  `Canonical → None` (`Canon.legal_demotion_unique`); `revertCount_append`,
  `promotion_not_revert`.

Admission:

* `soloStage1_eq_admitsHop`, `mem_soloCandidates`: Stage 1 under Solo is
  exactly "resistant score ≥ 3.0". `soloCandidates_sorted`: in exact
  arithmetic, the admitted concepts come out in score, key, id order
  (`exact_tie_orders_by_key` shows where `f64` differs). `admitsHop_down`,
  `admitsHop_mono`.
* `policy_cycle_one_hop`, `policy_cycle_rank_le`: under either policy a cycle
  moves a concept along at most one eval-loop edge (`Canon.cycle_one_hop`), so
  `admits_hop` never lets a concept skip a rung; `solo_cycle_capped`: a Solo
  promotion lands only on a rung its band admits; `promotion_needs_verdict`:
  no policy promotes past a failed verdict; `solo_three_cycles`.
* `default_is_swarm`, `mem_all`, `swarm_admits_every_hop`,
  `solo_admits_what_swarm_refuses`, `parse_asStr`, `parse_sound` (fail-closed:
  no fallback to the default), `parse_padded`, and the Rust test table
  `from_str_is_lenient_about_shape_and_strict_about_membership`.

## Findings

1. **Hierarchical children: an undocumented asymmetry (open design
   question).** `supporting_interaction_times` credits `Dependency` edges into
   the concept and ignores `Hierarchical` ones; its comment says so
   ("`Dependency` into it"), and `EdgeType::dependent_end`'s list of
   implementations does not name the recurrence term, so this is not a plain
   doc/code mismatch. But every other dependence reader goes through
   `Edge::dependence`, which makes a `Hierarchical` child (the `parent_of`
   target) a dependent: the score's dependents term
   (`dependent_interactions`) and Stage 2's span (`interaction_span` in
   `src/store/evidence.rs`, via `Edge::dependence`) both credit the child's
   origin interaction. So Solo's time-spread term and Stage 2's disagree on
   `parent_of` children: a child declared in a later turn adds a dependent
   interaction (+2.0) and Stage 2 support, but no recurrence session
   (`hierarchical_child_is_dependent_not_recurrence`). Whether children
   should count as recurrence is not stated anywhere.
2. **A `Dependency` edge is dated by its source's origin, not by the turn
   that wrote it** (fixed for the Solo terms in #300; see "The fixed
   dating"; Swarm's Stage 1 count and Stage 2's span keep the origin
   dating, an owner question). Before #300, both terms credit a `Dependency` edge to the source
   concept's `origin_interaction`: recurrence in
   `supporting_interaction_times` (`src/canon/policy.rs`, the `Dependency`
   arm) and dependents in `dependent_origin` (`src/canon/stage1.rs`).
   `record_action` reuses an existing action node on a repeat (test
   `record_action_repeat_is_reinforcement_not_duplication`,
   `src/graph/action.rs`). So: day 0, turn 1, action "deploy api" depends on
   X; day 5, the same action depends on Y. The new action → Y edge is
   credited to turn 1 and day 0. Y gains no recurrence session, and if Y also
   originated in turn 1, `dependent_origin` drops the edge as own-origin, so
   Y gains no dependent either
   (`reused_action_dependency_is_dated_by_its_origin`; a fresh action node
   from the day-5 turn gives both). This contradicts `event_time.rs`'s "An
   edge is about what the turn that wrote it is about": `Edge::about_time`
   already holds the writing turn's time, and neither term reads it. Caveat:
   a repeated *identical* citation reinforces the existing edge without
   re-dating it, so reading the edge's time would not help that case.
3. **Test comments disagreed with the test code** (the tests still
   passed; the comments were corrected in #300):
   * `spread_support_graph` documents about-times on days 0/2/4/6; the code
     (`day(n - 1) + 48h`) gives days 0/3/4/5 (`spread_days_doc_and_code`).
     The same "days 0/2/4/6" or "days 2/4/6" wording recurs in
     `causal_touches_are_not_recurrence`,
     `a_bulk_ingest_recurs_only_under_event_time` (doc and assertion
     message) and `solo_admits_the_recurring_fact_and_refuses_the_burst_and_the_one_off`.
   * `a_bulk_ingest_recurs_only_under_event_time` says the about-times "sit
     ≥48h apart"; days 3, 4 and 5 are exactly 24 h apart, on the inclusive
     boundary, so the corpus depends on the `<` in `t - a < gap`: a `<=`
     mutant counts 3, not 4 (`spread_days_on_the_boundary`).
   * `eviction_resistance_multiplies_before_the_band_comparison` describes
     three sessions (raw 3.0, Entity 3.6, Observation 2.1, additive mutant
     3.15) but asserts on four (4.0, 4.8, 2.8).
4. **Outside the model** (documented in the code, and fail-closed): an
   all-whitespace `LAMBO_PROMOTION_POLICY` is treated as unset
   (`src/config.rs`, the env overlay), while `promotion_policy = "  "` in
   `lambo.toml` is a hard error. That is a difference between the two
   surfaces that `from_str`'s "One parser serves both surfaces" does not
   mention.
-/

namespace Lambo.Policy

/-! ## Sort ascending, deduplicate -/

/-- Insert `x` into a strictly increasing list, dropping it if already present. -/
def ins (x : Int) : List Int → List Int
  | [] => [x]
  | y :: ys => if x < y then x :: y :: ys else if x = y then y :: ys else y :: ins x ys

/-- `sorted.sort(); sorted.dedup();`: the strictly increasing list of the
distinct values of `l`. -/
def canon (l : List Int) : List Int := l.foldr ins []

/-- Strictly increasing. -/
abbrev Strict (l : List Int) : Prop := l.Pairwise (· < ·)

theorem mem_ins (x z : Int) : ∀ l : List Int, z ∈ ins x l ↔ z = x ∨ z ∈ l
  | [] => by simp [ins]
  | y :: ys => by
    unfold ins
    by_cases h1 : x < y
    · rw [ite_eq_left h1]; simp
    · by_cases h2 : x = y
      · rw [ite_eq_right h1, ite_eq_left h2]; subst h2; simp
      · have ih := mem_ins x z ys
        rw [ite_eq_right h1, ite_eq_right h2]
        simp only [List.mem_cons, ih]
        constructor
        · intro h; rcases h with h | h | h <;> simp [h]
        · intro h; rcases h with h | h | h <;> simp [h]

theorem strict_ins (x : Int) : ∀ l : List Int, Strict l → Strict (ins x l)
  | [] => fun _ => by simp [ins]
  | y :: ys => by
    intro h
    rw [Strict, List.pairwise_cons] at h
    unfold ins
    by_cases h1 : x < y
    · rw [ite_eq_left h1]
      simp only [Strict, List.pairwise_cons]
      refine ⟨?_, h.1, h.2⟩
      intro a ha
      rcases List.mem_cons.mp ha with ha | ha
      · omega
      · have := h.1 a ha; omega
    · by_cases h2 : x = y
      · rw [ite_eq_right h1, ite_eq_left h2, Strict, List.pairwise_cons]
        exact h
      · rw [ite_eq_right h1, ite_eq_right h2, Strict, List.pairwise_cons]
        refine ⟨?_, strict_ins x ys h.2⟩
        intro a ha
        rcases (mem_ins x a ys).mp ha with ha | ha
        · omega
        · exact h.1 a ha

theorem mem_canon (z : Int) : ∀ l : List Int, z ∈ canon l ↔ z ∈ l
  | [] => by simp [canon]
  | x :: xs => by
    have ih := mem_canon z xs
    simp only [canon, List.foldr_cons] at ih ⊢
    rw [mem_ins, ih, List.mem_cons]

theorem strict_canon : ∀ l : List Int, Strict (canon l)
  | [] => by simp [canon, Strict]
  | x :: xs => by
    simp only [canon, List.foldr_cons]
    exact strict_ins x _ (strict_canon xs)

/-- Two strictly increasing lists with the same members are equal. -/
theorem strict_unique : ∀ (a b : List Int), Strict a → Strict b →
    (∀ z, z ∈ a ↔ z ∈ b) → a = b
  | [], [], _, _, _ => rfl
  | [], y :: _, _, _, h => by have := (h y).mpr (by simp); simp at this
  | x :: _, [], _, _, h => by have := (h x).mp (by simp); simp at this
  | x :: xs, y :: ys, ha, hb, h => by
    rw [Strict, List.pairwise_cons] at ha hb
    have hxy : x = y := by
      have h1 := (h x).mp (by simp)
      have h2 := (h y).mpr (by simp)
      rcases List.mem_cons.mp h1 with h1 | h1
      · exact h1
      rcases List.mem_cons.mp h2 with h2 | h2
      · exact h2.symm
      have := hb.1 x h1
      have := ha.1 y h2
      omega
    subst hxy
    have rest : xs = ys := by
      apply strict_unique xs ys ha.2 hb.2
      intro z
      constructor
      · intro hz
        have hlt := ha.1 z hz
        rcases List.mem_cons.mp ((h z).mp (List.mem_cons_of_mem _ hz)) with e | e
        · omega
        · exact e
      · intro hz
        have hlt := hb.1 z hz
        rcases List.mem_cons.mp ((h z).mpr (List.mem_cons_of_mem _ hz)) with e | e
        · omega
        · exact e
    rw [rest]

theorem length_ins (x : Int) : ∀ l : List Int, (ins x l).length ≤ l.length + 1
  | [] => by simp [ins]
  | y :: ys => by
    unfold ins
    by_cases h1 : x < y
    · rw [ite_eq_left h1]; simp
    · by_cases h2 : x = y
      · rw [ite_eq_right h1, ite_eq_left h2]; simp
      · rw [ite_eq_right h1, ite_eq_right h2]
        have := length_ins x ys
        simp only [List.length_cons]
        omega

/-- At most as many distinct starts as starts. -/
theorem length_canon_le : ∀ l : List Int, (canon l).length ≤ l.length
  | [] => by simp [canon]
  | x :: xs => by
    have ih := length_canon_le xs
    have := length_ins x (canon xs)
    simp only [canon, List.foldr_cons, List.length_cons] at ih this ⊢
    omega

/-- `canon` is whatever "sort ascending, then dedup" produces: any strictly
increasing list with the members of `l` is `canon l`. -/
theorem canon_eq_of_strict (l L : List Int) (hs : Strict L) (hm : ∀ z, z ∈ L ↔ z ∈ l) :
    canon l = L :=
  strict_unique _ _ (strict_canon l) hs (fun z => (mem_canon z l).trans (hm z).symm)

theorem canon_congr (l l' : List Int) (h : ∀ z, z ∈ l ↔ z ∈ l') : canon l = canon l' :=
  canon_eq_of_strict l (canon l') (strict_canon l')
    (fun z => (mem_canon z l').trans (h z).symm)

/-- A strictly increasing list whose members all occur in a strictly
increasing `L` is a subsequence of `L`. -/
theorem sublist_of_strict : ∀ (S L : List Int), Strict S → Strict L →
    (∀ x, x ∈ S → x ∈ L) → S.Sublist L
  | [], _, _, _, _ => List.nil_sublist _
  | s :: S', [], _, _, h => by have := h s (by simp); simp at this
  | s :: S', t :: L', hS, hL, h => by
    rw [Strict, List.pairwise_cons] at hS hL
    by_cases hst : s = t
    · subst hst
      apply List.Sublist.cons_cons
      apply sublist_of_strict S' L' hS.2 hL.2
      intro x hx
      have := hS.1 x hx
      rcases List.mem_cons.mp (h x (List.mem_cons_of_mem _ hx)) with e | e
      · omega
      · exact e
    · apply List.Sublist.cons
      have hs : s ∈ L' := by
        rcases List.mem_cons.mp (h s (by simp)) with e | e
        · exact absurd e hst
        · exact e
      have hts := hL.1 s hs
      apply sublist_of_strict (s :: S') L' (by rw [Strict, List.pairwise_cons]; exact hS) hL.2
      intro x hx
      rcases List.mem_cons.mp hx with e | e
      · subst e; exact hs
      · have := hS.1 x e
        rcases List.mem_cons.mp (h x hx) with e' | e'
        · omega
        · exact e'

/-! ## The greedy counter

```rust
let gap = chrono::Duration::from_std(gap).unwrap_or(chrono::Duration::MAX);
let mut sorted = starts.to_vec();
sorted.sort();
sorted.dedup();
let mut count = 0;
let mut anchor: Option<DateTime<Utc>> = None;
for t in sorted {
    match anchor {
        Some(a) if t - a < gap => {}
        _ => { count += 1; anchor = Some(t); }
    }
}
count
```
-/

/-- The loop over the sorted, deduplicated starts, from a state whose last
counted start is `anchor`. -/
def greedy (gap : Nat) : Option Int → List Int → Nat
  | _, [] => 0
  | none, t :: rest => greedy gap (some t) rest + 1
  | some a, t :: rest =>
    if t - a < (gap : Int) then greedy gap (some a) rest else greedy gap (some t) rest + 1

/-- `separated_session_count(starts, gap)`. -/
def sessionCount (gap : Nat) (starts : List Int) : Nat := greedy gap none (canon starts)

/-- The starts the loop counts. -/
def picks (gap : Nat) : Option Int → List Int → List Int
  | _, [] => []
  | none, t :: rest => t :: picks gap (some t) rest
  | some a, t :: rest =>
    if t - a < (gap : Int) then picks gap (some a) rest else t :: picks gap (some t) rest

/-- Gap-separated: increasing, and each start at least `gap` after every
earlier one. -/
def Sep (gap : Nat) (S : List Int) : Prop := S.Pairwise (fun x y : Int => x < y ∧ x + (gap : Int) ≤ y)

theorem greedy_eq_length_picks (gap : Nat) :
    ∀ (anchor : Option Int) (L : List Int), greedy gap anchor L = (picks gap anchor L).length
  | _, [] => by cases ‹Option Int› <;> rfl
  | none, t :: rest => by
    simp only [greedy, picks, List.length_cons, greedy_eq_length_picks gap (some t) rest]
  | some a, t :: rest => by
    simp only [greedy, picks]
    by_cases h : t - a < (gap : Int)
    · rw [ite_eq_left h, ite_eq_left h]; exact greedy_eq_length_picks gap (some a) rest
    · rw [ite_eq_right h, ite_eq_right h, List.length_cons,
        greedy_eq_length_picks gap (some t) rest]

theorem picks_sublist (gap : Nat) :
    ∀ (anchor : Option Int) (L : List Int), (picks gap anchor L).Sublist L
  | _, [] => by cases ‹Option Int› <;> exact List.Sublist.slnil
  | none, t :: rest => List.Sublist.cons_cons _ (picks_sublist gap (some t) rest)
  | some a, t :: rest => by
    simp only [picks]
    by_cases h : t - a < (gap : Int)
    · rw [ite_eq_left h]; exact List.Sublist.cons _ (picks_sublist gap (some a) rest)
    · rw [ite_eq_right h]; exact List.Sublist.cons_cons _ (picks_sublist gap (some t) rest)

/-- Every start counted after anchor `a` is at least `gap` after it. -/
theorem picks_anchor (gap : Nat) :
    ∀ (a : Int) (L : List Int), ∀ p, p ∈ picks gap (some a) L → a + gap ≤ p
  | _, [] => by simp [picks]
  | a, t :: rest => by
    intro p hp
    simp only [picks] at hp
    by_cases h : t - a < (gap : Int)
    · rw [ite_eq_left h] at hp; exact picks_anchor gap a rest p hp
    · rw [ite_eq_right h] at hp
      rcases List.mem_cons.mp hp with e | e
      · omega
      · have := picks_anchor gap t rest p e; omega

/-- The counted starts are gap-separated. -/
theorem picks_sep (gap : Nat) :
    ∀ (anchor : Option Int) (L : List Int), Strict L → Sep gap (picks gap anchor L)
  | _, [], _ => by cases ‹Option Int› <;> simp [picks, Sep]
  | none, t :: rest, hL => by
    rw [Strict, List.pairwise_cons] at hL
    show Sep gap (t :: picks gap (some t) rest)
    rw [Sep, List.pairwise_cons]
    refine ⟨fun p hp => ⟨hL.1 p ((picks_sublist gap (some t) rest).subset hp),
      picks_anchor gap t rest p hp⟩, picks_sep gap (some t) rest hL.2⟩
  | some a, t :: rest, hL => by
    rw [Strict, List.pairwise_cons] at hL
    simp only [picks]
    by_cases h : t - a < (gap : Int)
    · rw [ite_eq_left h]; exact picks_sep gap (some a) rest hL.2
    · rw [ite_eq_right h, Sep, List.pairwise_cons]
      refine ⟨fun p hp => ⟨hL.1 p ((picks_sublist gap (some t) rest).subset hp),
        picks_anchor gap t rest p hp⟩, picks_sep gap (some t) rest hL.2⟩

/-- **Optimality** of the greedy rule: no gap-separated subsequence of the
sorted starts (lying past the anchor) is longer than the count. -/
theorem greedy_ge (gap : Nat) :
    ∀ (L : List Int) (anchor : Option Int) (S : List Int), Strict L → S.Sublist L →
      Sep gap S → (∀ a, anchor = some a → ∀ x, x ∈ S → a + gap ≤ x) →
      S.length ≤ greedy gap anchor L
  | [], _, S, _, hsub, _, _ => by
    rw [List.sublist_nil.mp hsub]; simp
  | t :: rest, anchor, S, hL, hsub, hsep, hanc => by
    rw [Strict, List.pairwise_cons] at hL
    -- the step that counts `t`: `S` minus its head fits after anchor `t`
    have counted : S.length ≤ greedy gap (some t) rest + 1 := by
      cases S with
      | nil => simp
      | cons s S' =>
        have hs' : S'.Sublist rest := by
          rcases List.sublist_cons_iff.mp hsub with h | ⟨r, hr, h⟩
          · exact (List.sublist_cons_self s S').trans h
          · cases hr; exact h
        have hts : t ≤ s := by
          rcases List.mem_cons.mp (hsub.subset (List.mem_cons_self)) with e | e
          · omega
          · have := hL.1 s e; omega
        rw [Sep, List.pairwise_cons] at hsep
        have := greedy_ge gap rest (some t) S' hL.2 hs' hsep.2 (by
          intro a ha x hx
          cases ha
          have := (hsep.1 x hx).2
          omega)
        simp only [List.length_cons]
        omega
    cases anchor with
    | none => exact counted
    | some a =>
      simp only [greedy]
      by_cases h : t - a < (gap : Int)
      · rw [ite_eq_left h]
        have hnot : t ∉ S := fun ht => by have := hanc a rfl t ht; omega
        have hs' : S.Sublist rest := by
          rcases List.sublist_cons_iff.mp hsub with h' | ⟨r, hr, _⟩
          · exact h'
          · exact absurd (hr ▸ List.mem_cons_self) hnot
        exact greedy_ge gap rest (some a) S hL.2 hs' hsep hanc
      · rw [ite_eq_right h]; exact counted

theorem greedy_le_length (gap : Nat) (anchor : Option Int) (L : List Int) :
    greedy gap anchor L ≤ L.length := by
  rw [greedy_eq_length_picks]; exact (picks_sublist gap anchor L).length_le

theorem greedy_zero (a : Int) :
    ∀ L : List Int, Strict L → (∀ y, y ∈ L → a < y) → greedy 0 (some a) L = L.length
  | [], _, _ => rfl
  | t :: rest, hL, ha => by
    rw [Strict, List.pairwise_cons] at hL
    have hat := ha t (by simp)
    simp only [greedy]
    have : ¬ (t - a < ((0 : Nat) : Int)) := by omega
    rw [ite_eq_right this, greedy_zero t rest hL.2 hL.1, List.length_cons]

/-! ### What the count is -/

/-- **Order-free.** The count depends only on which instants occur: not on
their order, not on how often each repeats. -/
theorem sessionCount_congr (gap : Nat) (l l' : List Int) (h : ∀ z, z ∈ l ↔ z ∈ l') :
    sessionCount gap l = sessionCount gap l' := by
  unfold sessionCount; rw [canon_congr l l' h]

/-- Permutation-invariant. -/
theorem sessionCount_perm (gap : Nat) {l l' : List Int} (h : l.Perm l') :
    sessionCount gap l = sessionCount gap l' :=
  sessionCount_congr gap l l' (fun _ => h.mem_iff)

/-- An exact duplicate is one session start ("the same instant is one session
start"). -/
theorem sessionCount_dup (gap : Nat) (x : Int) (l : List Int) :
    sessionCount gap (x :: x :: l) = sessionCount gap (x :: l) :=
  sessionCount_congr gap _ _ (fun z => by simp)

/-- Repeating the whole input changes nothing. -/
theorem sessionCount_append_self (gap : Nat) (l : List Int) :
    sessionCount gap (l ++ l) = sessionCount gap l :=
  sessionCount_congr gap _ _ (fun z => by simp)

/-- No starts, no sessions (Rust test
`separation_counts_clusters_once_and_stragglers_each`). -/
theorem sessionCount_nil (gap : Nat) : sessionCount gap [] = 0 := rfl

/-- Some start, at least one session. -/
theorem sessionCount_pos (gap : Nat) (l : List Int) (h : l ≠ []) : 1 ≤ sessionCount gap l := by
  unfold sessionCount
  cases hc : canon l with
  | nil =>
    cases l with
    | nil => exact absurd rfl h
    | cons x xs =>
      have := (mem_canon x (x :: xs)).mpr (by simp)
      rw [hc] at this; simp at this
  | cons t rest => simp [greedy]

/-- At most one session per distinct start. -/
theorem sessionCount_le_distinct (gap : Nat) (l : List Int) :
    sessionCount gap l ≤ (canon l).length :=
  greedy_le_length gap none (canon l)

/-- Hence at most one per start. -/
theorem sessionCount_le_length (gap : Nat) (l : List Int) : sessionCount gap l ≤ l.length :=
  Nat.le_trans (sessionCount_le_distinct gap l) (length_canon_le l)

/-- With `gap = 0` every distinct start is a session. -/
theorem sessionCount_zero_gap (l : List Int) : sessionCount 0 l = (canon l).length := by
  unfold sessionCount
  have hs := strict_canon l
  cases hc : canon l with
  | nil => rfl
  | cons t rest =>
    rw [hc, Strict, List.pairwise_cons] at hs
    simp only [greedy, List.length_cons]
    rw [greedy_zero t rest hs.2 hs.1]

/-- **The count is a maximum.** No gap-separated set of starts is larger
than the count ... -/
theorem sessionCount_max (gap : Nat) (l S : List Int) (hS : Sep gap S)
    (hm : ∀ x, x ∈ S → x ∈ l) : S.length ≤ sessionCount gap l := by
  have hstrict : Strict S := hS.imp (fun h => h.1)
  have hsub : S.Sublist (canon l) :=
    sublist_of_strict S (canon l) hstrict (strict_canon l)
      (fun x hx => (mem_canon x l).mpr (hm x hx))
  exact greedy_ge gap (canon l) none S (strict_canon l) hsub hS (fun _ h => by cases h)

/-- ... and the starts the loop counts are such a set. -/
theorem sessionCount_attained (gap : Nat) (l : List Int) :
    ∃ S, Sep gap S ∧ (∀ x, x ∈ S → x ∈ l) ∧ S.length = sessionCount gap l :=
  ⟨picks gap none (canon l), picks_sep gap none (canon l) (strict_canon l),
    fun x hx => (mem_canon x l).mp ((picks_sublist gap none (canon l)).subset hx),
    (greedy_eq_length_picks gap none (canon l)).symm⟩

/-- **Monotone.** More starts never mean fewer sessions. -/
theorem sessionCount_mono (gap : Nat) (l l' : List Int) (h : ∀ x, x ∈ l → x ∈ l') :
    sessionCount gap l ≤ sessionCount gap l' := by
  obtain ⟨S, hS, hm, hlen⟩ := sessionCount_attained gap l
  rw [← hlen]
  exact sessionCount_max gap l' S hS (fun x hx => h x (hm x hx))

/-- Adding a start never lowers the count. -/
theorem sessionCount_le_cons (gap : Nat) (x : Int) (l : List Int) :
    sessionCount gap l ≤ sessionCount gap (x :: l) :=
  sessionCount_mono gap l (x :: l) (fun _ h => List.mem_cons_of_mem _ h)

theorem length_le_filter_ne (x : Int) :
    ∀ S : List Int, Strict S → S.length ≤ (S.filter (fun y => y != x)).length + 1
  | [], _ => by simp
  | s :: S', hS => by
    rw [Strict, List.pairwise_cons] at hS
    by_cases hsx : s = x
    · subst hsx
      have : S'.filter (fun y => y != s) = S' :=
        List.filter_eq_self.mpr (fun a ha => by
          have := hS.1 a ha
          simp only [bne_iff_ne, ne_eq]; omega)
      simp [this]
    · have ih := length_le_filter_ne x S' hS.2
      have hb : (s != x) = true := by simp [hsx]
      simp only [List.filter_cons, hb, ite_true, List.length_cons]
      omega

/-- Adding a start raises the count by at most one. -/
theorem sessionCount_cons_le (gap : Nat) (x : Int) (l : List Int) :
    sessionCount gap (x :: l) ≤ sessionCount gap l + 1 := by
  obtain ⟨S, hS, hm, hlen⟩ := sessionCount_attained gap (x :: l)
  rw [← hlen]
  have hstrict : Strict S := hS.imp (fun h => h.1)
  have hf : Sep gap (S.filter (fun y => y != x)) := List.Pairwise.filter _ hS
  have hfm : ∀ y, y ∈ S.filter (fun y => y != x) → y ∈ l := by
    intro y hy
    rw [List.mem_filter] at hy
    rcases List.mem_cons.mp (hm y hy.1) with e | e
    · subst e; simp at hy
    · exact e
  have := sessionCount_max gap l _ hf hfm
  have := length_le_filter_ne x S hstrict
  omega

/-- A wider gap never counts more sessions. -/
theorem sessionCount_antitone_gap (g g' : Nat) (hg : g ≤ g') (l : List Int) :
    sessionCount g' l ≤ sessionCount g l := by
  obtain ⟨S, hS, hm, hlen⟩ := sessionCount_attained g' l
  rw [← hlen]
  exact sessionCount_max g l S (hS.imp (fun h => ⟨h.1, by have := h.2; omega⟩)) hm

/-- **One window, one session.** Starts that all lie less than `gap` after
the earliest one count as one session. -/
theorem sessionCount_window (gap : Nat) (l : List Int) (m : Int) (hm : m ∈ l)
    (h : ∀ x, x ∈ l → m ≤ x ∧ x < m + gap) : sessionCount gap l = 1 := by
  have hpos := sessionCount_pos gap l (fun e => by subst e; simp at hm)
  obtain ⟨S, hS, hmem, hlen⟩ := sessionCount_attained gap l
  have : S.length ≤ 1 := by
    match S, hS, hmem with
    | [], _, _ => simp
    | [_], _, _ => simp
    | a :: b :: _, hS, hmem =>
      rw [Sep, List.pairwise_cons] at hS
      have hab := (hS.1 b (by simp)).2
      have ha := h a (hmem a (by simp))
      have hb := h b (hmem b (by simp))
      omega
  omega

/-! ### The Rust tests, and what "sessions" means -/

/-- Seconds since the epoch of the Rust tests' `ts(0)`. -/
def base : Int := 1752000000

/-- One day: `SESSION_SEPARATION`, 86 400 s. -/
def sessionSeparation : Nat := 86400

/-- Rust test `separation_counts_clusters_once_and_stragglers_each`: a cluster
of three starts within an hour, a start exactly 24 h later (the boundary
counts, `>=`), and one 50 h after the first count as 3; no starts as 0; one
start as 1; three copies of one instant as 1. -/
theorem separation_counts_clusters_once_and_stragglers_each :
    sessionCount sessionSeparation
        [base, base + 600, base + 3600, base + 86400, base + 172800 + 7200] = 3 ∧
      sessionCount 0 [] = 0 ∧ sessionCount 0 [base + 5] = 1 ∧
      sessionCount 0 [base, base, base] = 1 := by
  decide

/-- The count is the largest gap-separated set of starts, not the number of
clusters joined by links shorter than `gap`: five starts 23 h apart are one
such cluster (every neighbour within 24 h) but three sessions (hours 0, 46
and 92). -/
theorem chain_counts_more_than_one :
    sessionCount sessionSeparation
      [base, base + 23 * 3600, base + 46 * 3600, base + 69 * 3600, base + 92 * 3600] = 3 := by
  decide

/-- `sessionCount_window` needs the *earliest* start: starts within `gap` of
the first one *in input order* can still count as two. -/
theorem window_needs_earliest :
    sessionCount 15 [10, 0, 20] = 2 := by
  decide

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

/-- Adding any edge never lowers the recurrence term. -/
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

/-- With no event time, the about-time is the flush time. -/
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

/-- The test's comment says the about-times are days 0/2/4/6; the code
computes days 0/3/4/5. Both are four sessions, so the test holds on either
reading (finding 3). -/
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

/-- `eraseDups` leaves no duplicates. -/
theorem nodup_eraseDups : ∀ (n : Nat) (l : List Nat), l.length ≤ n → l.eraseDups.Nodup
  | _, [], _ => by simp [List.eraseDups, List.eraseDupsBy, List.eraseDupsBy.loop]
  | 0, _ :: _, h => by simp at h
  | n + 1, a :: as, h => by
    rw [List.eraseDups_cons, List.nodup_cons]
    constructor
    · intro hm
      rw [List.mem_eraseDups, List.mem_filter] at hm
      simp at hm
    · apply nodup_eraseDups n
      have := List.length_filter_le (fun b => !b == a) as
      simp only [List.length_cons] at h
      omega

/-- One more element never shortens the deduplicated list. -/
theorem length_eraseDups_le_cons (x : Nat) (l : List Nat) :
    l.eraseDups.length ≤ (x :: l).eraseDups.length :=
  List.Nodup.length_le_of_subset (nodup_eraseDups _ l (Nat.le_refl _)) (fun y hy => by
    rw [List.mem_eraseDups] at hy ⊢
    exact List.mem_cons_of_mem _ hy)

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

theorem own_origin_is_the_writing_turn :
    dependentOrigins ownTurnGraph 71 = [0] ∧ dependentOriginsW ownTurnGraph ownTurnStamp 71 = [] ∧
      supportingTimes ownTurnGraph 71 = [day 0, day 1] ∧
      supportingTimesW ownTurnGraph ownTurnStamp 71 = [day 1] := by
  decide

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
Resource 1.0, Observation 0.7. -/
def ConceptType.resistance10 : ConceptType → Int
  | .constraint => 15
  | .entity => 12
  | .logic => 11
  | .resource => 10
  | .observation => 7

/-- `SESSION_WEIGHT`. -/
def sessionWeight : Int := 1
/-- `HUMAN_CONFIRMED_WEIGHT`. -/
def humanConfirmedWeight : Int := 4
/-- `DEPENDENT_INTERACTION_WEIGHT`. -/
def dependentInteractionWeight : Int := 2
/-- `REVERT_WEIGHT`. -/
def revertWeight : Int := 3

/-- `CANDIDATE_BAR`, `VENERABLE_BAR`, `CANONICAL_BAR`, in tenths. -/
def candidateBar10 : Int := 30
def venerableBar10 : Int := 60
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

/-- Each term moves the score by exactly its weight times the resistance:
one more session `+1`, confirmation `+4`, dependent interaction `+2`, revert
`−3` (in units of the resistance), so the score is strictly monotone in each. -/
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

theorem classify_candidate_iff (s : Int) :
    classify s = .candidate ↔ candidateBar10 ≤ s ∧ s < venerableBar10 := by
  unfold classify candidateBar10 venerableBar10 canonicalBar10
  repeat' split
  all_goals simp
  all_goals omega

theorem classify_venerable_iff (s : Int) :
    classify s = .venerable ↔ venerableBar10 ≤ s ∧ s < canonicalBar10 := by
  unfold classify candidateBar10 venerableBar10 canonicalBar10
  repeat' split
  all_goals simp
  all_goals omega

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

/-- The bulk-ingest corpus through the score: the Entity hub is a Candidate
on event time (4.8) and ordinary memory on flush time (1.2). -/
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

/-- Recording an event adds one revert exactly when it lowers the rank. -/
theorem revertCount_append (evs : List Event) (id : Nat) (e : Event) :
    revertCount (evs ++ [e]) id = revertCount evs id + (if isRevert id e then 1 else 0) := by
  unfold revertCount
  rw [List.filter_append, List.length_append]
  cases h : isRevert id e <;> simp [List.filter, h]

/-- Promotions never count as reverts. -/
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

/-- A band admitting a hop admits every lower one. -/
theorem admitsHop_down {b to to' : Canon.Status} (h : admitsHop b to = true)
    (h' : to'.rank ≤ to.rank) : admitsHop b to' = true := by
  simp only [admitsHop, decide_eq_true_eq] at h ⊢; omega

/-- More evidence never withdraws an admission. -/
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

/-- `#[default]`. -/
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

/-- `ALL` names every variant. -/
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

/-- **Padding is ignored**: whitespace around a value never changes what it
parses to. -/
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
