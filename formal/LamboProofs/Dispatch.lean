import LamboProofs.Order

/-
# Dependency-question dispatch

Model of the structural arm of recall, `src/recall/dispatch.rs`, and of the
daemon's routing around it in `Daemon::recall_routed` (`src/daemon/mod.rs`):

* `classify` and `STRUCTURAL_MARKERS`: a query is `Structural` when its
  lowercased text contains one of seven marker phrasings. `classify` is the
  code before #299, a plain substring test; `classifyWB` is the fixed code,
  which counts a marker only on word boundaries;
* `dependents` over `format::dependents_of` (`src/recall/format.rs`) and
  `Edge::dependence` / `EdgeType::dependent_end` (`src/types/mod.rs`): the
  anchor's direct structural dependents, each scored by its strongest edge,
  sorted Canonical first, then score descending, then `tie_break_by_key`
  (`dependentsOf`, `ranked`);
* `try_structural_with` and `try_structural`: the traversal answer, cut to
  `top_k`, or a refusal that falls through to the blend (`tryStructuralWith`,
  `tryStructural`);
* `fits_structural_with`, `anchor_dispatches` and `fits_structural`: the
  gather-skip predicate (`fitsStructuralWith`, `anchorDispatches`,
  `fitsStructural`);
* the routing in `recall_routed`: classify, pre-check under a brief graph
  read, skip the store gather only when the pre-check says the dispatch is
  about to fire, retry under its own guard and re-gather when a write in
  between makes it decline (#263), then the final guard (`recallRouted`).

## Model

* **Strings.** A query is a `List Char`. `lower` is `Char.toLower` on every
  character, which is ASCII-only; Rust's `str::to_lowercase` is full Unicode.
  The two can only disagree on whether a marker occurs where a non-ASCII
  character lowercases to ASCII. The markers use only the letters
  `a b d e f h i l n o p r s t u w`, space and `-`; the only non-ASCII
  characters whose Unicode lowercase contains an ASCII letter are U+212A
  KELVIN SIGN (to `k`, not a marker letter) and U+0130 (to `i` followed by
  U+0307, so the `i` of `radius` would be followed by a combining mark, not
  `u`). So on every query the two agree on where a marker occurs. The
  word-bounded classifier (`classifyWB`) also reads the characters next to an
  occurrence, which may be non-ASCII and which `lower` leaves alone; that
  reading still holds, because lowercasing keeps a character a word or a
  non-word character (a letter lowercases to letters, U+0130 to `i` and
  U+0307, both word characters; a non-letter, the hyphen included, is
  unchanged), and the boundary test asks only that. The model does not prove
  any of this; it is the reading above.
* **Matching** before #299 was `str::contains`: a plain substring test
  (`containsSub`, proved equal to `List.IsInfix`), with no word boundaries
  (`classify`). The fix (#299) matches each marker with the word-bounded rule
  of `word_bounded_spans`, the one anchor resolution uses (`classifyWB`,
  `wordAt`, `leftOk`, `rightOk`). The word-character test `is_word_char`
  (Unicode letters and digits, `_`, combining marks) is a parameter `isWord`;
  the theorems hold for every `isWord` (some assume that lowercase ASCII
  letters are word characters, as they are), and the `decide` examples use
  `asciiWord`, which is `is_word_char` on ASCII. `marker_spans`, which anchor
  resolution measures from, is fixed the same way but is not modelled (see
  anchor resolution below).
* **Graph.** Node ids are `Nat`. A graph is its edge list, which concepts it
  holds (`isConcept`), which are Canonical, and each node's canonical key with
  a key order (`tie_form` comparison, a lawful parameter as in `Order.lean`).
  The Rust walks `incident_edges(anchor)`; every edge naming the anchor as its
  pillar touches the anchor, so walking all edges gives the same set.
* **Weights** are `Nat` (fixed point, as elsewhere): `f64` maximum and
  `total_cmp` agree with `Nat` order on finite non-negative weights; NaN,
  negative zero and rounding are outside the model.
* **Sorting.** The Rust's `sort_by` is a stable merge sort; the model uses an
  insertion sort (`isort`) because the kernel can evaluate it for the `decide`
  examples. The comparator is `Order.rankMembers` itself, lawful and
  separating, and the dependents are distinct, so any correct sort returns the
  same list (`ranked_unique`, from `Order.sorted_unique`).
* **Anchor resolution** (`resolve_anchor_choice`: word-bounded spans, marker
  spans, stop words, the security-group prose fallback, #129 ambiguity) is not
  modelled: the resolved subject is an input, `Option Choice`. In
  `recallRouted` it is a function of the graph snapshot, which is what
  `ResolvedAnchor::current` gives when the epoch tracks every mutation (#171).
* **Rendering** is not modelled: the per-hit `RecallHit` fields, blast-radius
  banners, names, and the token-budget cut (`fit_blocks`, see `Fit.lean`),
  which shortens the rendered context but not the `hits` list. The `?` in
  `graph.node(s.item)?` never fires: every dependent is a concept
  (`ranked_concepts`).
* **The blend** is opaque: the routing result is either a traversal answer or
  "the blended pipeline over this phase-1 input". The input is the store
  gather's result, `gathered` (whatever it holds, a degraded read included),
  when the recall has a vector leg (a store that searches vectors and a query
  embedding), and the default, `empty`, when it does not: `gather_from`
  (`src/recall/candidates.rs`) returns `Phase1Input::default()` in that case,
  and the recent-leg score `Phase1Gather::run` stamps on it is then
  `RECENT_SCORE`, the default's, so skipping the gather and running it give
  the same input.
* **Early refusals** of `recall_routed` are not modelled: the
  `validate_vector_candidate_limit` and caller/graph session-mismatch
  returns (`DetailedRecall::warn_only`, `src/daemon/mod.rs`), which answer
  before any routing. The gather's `Err` arm (`Phase1Gather::run`) is left
  out too; that is harmless here, because only `Route::ByVector` returns it
  and that route never routes structural, so it never meets a skipped
  gather.
* Concurrency: the three graph reads of `recall_routed` (pre-check, retry
  guard, final guard) are three snapshots `g₁ g₂ g₃`, any graphs at all.

## Results

Classifier:
* `classify_iff`: exactly the queries whose lowercase text contains a marker
  classify `Structural`; `classify_marker`: every marker, with any text around
  it and in any ASCII case, does (`classify_each_marker`: each marker alone).
* `depend_not_marker` (doc: "bare `"depend"` is NOT a marker"), with the test
  `marker_bearing_non_dependency_questions_stay_general` as `decide` examples.
* `classify_inside_word`, `classify_into_word` (the code before #299):
  matching is a substring test, so a marker still classifies `Structural`
  when it is glued to the end of a longer word or runs into the start of the
  next one. The second is the more frequent misfire: "the answer depends only
  on the score" is `Structural` because "depends only" contains "depends on"
  (`depends_only_structural`); "is it unsafe to delete it" is too
  (`unsafe_to_delete_structural`, harmless). Glued at the front,
  "independents of" and "somewhat uses" route to the structural arm
  (`independents_of_structural`, `somewhat_uses_structural`). The doc comment
  said every marker is "never an ordinary substring" and that marker-bearing
  non-dependency questions stay `General`; that held for "independent" only
  because no marker is a substring of it.

Fixed classifier (#299):
* `classifyWB_iff`: exactly the queries whose lowercase text has a marker
  occurrence on word boundaries classify `Structural`; `wb_inside_word`,
  `wb_into_word`: an occurrence with a word character just before or just
  after it never counts, nor does one hyphenated onto a word
  (`wb_hyphen_before`, `wb_hyphen_after`). `issue_queries_general`: the
  issue's four queries classify `General`.
* `classifyWB_marker`: every marker as whole words (the text around it ends
  in a non-word, non-hyphen character, or nothing), in any ASCII case,
  classifies `Structural` for every `isWord`; `classifyWB_each_marker`: each
  marker alone. The tests' and the docs' dependency questions stay
  `Structural` and their general questions `General` (`decide` examples).
* `classifyWB_structural`, `classifyWB_general`: the fix only narrows;
  `classifyWB_agrees`: when every marker occurrence is bounded, the fixed and
  the old classifier agree.

Traversal:
* `dependence_types`: only `Dependency` and `Hierarchical` edges declare a
  dependence (the module doc also names `Causal`);
* `mem_ranked`: the dependents are exactly the anchor's direct structural
  dependents (sound and complete, one edge, so no transitive node), and
  `ranked_nodup`, `anchor_not_mem_ranked`;
* `weight_spec`: each is scored by the strongest edge declaring it;
* `ranked_sorted`, `ranked_canonical_first`, `ranked_weight_desc`,
  `ranked_unique`: `rankMembers` order, determined by the set;
* `traversal_sound`, `traversal_complete`, `traversal_exact`: the hits are
  direct dependents only; a direct dependent that is not a hit was cut by
  `top_k` behind `top_k` hits that all rank before it; with `top_k` at least
  the dependent count the hits are exactly the direct dependents. All hold for
  every `traversal_depth` (`tryStructuralWith_depth`).
* `traversal_length_le`, `traversal_canonical_first`, `traversal_sorted`.
* `no_dependents_refuses`, `zero_topK_refuses`, `ambiguous_answers`.

Gather skip and routing:
* `fitsStructuralWith_iff` (test
  `the_gather_skip_predicate_matches_the_dispatch_at_every_top_k`): the
  pre-check is true exactly when the dispatch answers, at every `top_k`;
  `fits_skip_fires`; `fitsStructural_iff` for the public predicate at a
  positive `top_k`.
* `recall_blend_input`: whatever writes land between the three graph reads,
  a recall that blends does so over the input the store gather returns, never
  the skipped gather's empty one when a vector leg exists ("never a degraded
  keyword-only answer").
* `recall_one_snapshot`: on one graph, the routed recall is the recall with
  no pre-check; `recall_skip_fires`: when the gather is skipped, the answer is
  the traversal.
* `marker_no_dependents_blends`: a marker-bearing query whose anchor has no
  structural dependent falls through to the full blend; `general_blends`: a
  general query always does, even when its anchor has dependents.

Dispatch over the fixed classifier (#299): `tryStructuralBy`,
`fitsStructuralBy` and `recallRoutedBy` take the classifier as a parameter and
are the definitions above at `classify` (`tryStructural_eq`,
`fitsStructural_eq`, `recallRouted_eq`). `traversalBy_exact`,
`fitsStructuralBy_iff`, `recallBy_blend_input`, `generalBy_blends` hold for
every classifier, so for the fixed one: `traversal_exact_wb`,
`fitsStructuralWB_iff`, `recall_blend_input_wb`, `general_blends_wb`
(`fitsStructuralWith_iff` and `traversal_exact` read no classifier at all).
The misrouted "independents of" question of the examples blends under the
fixed classifier.
-/

namespace Lambo.Dispatch

open Lambo.Order

/-! ## The classifier -/

/-- `RecallKind`. -/
inductive Kind where
  | general
  | structural
  deriving DecidableEq, Repr

/-- `STRUCTURAL_MARKERS`, verbatim.
Pinned by `formal_constants_tests::dispatch`. -/
def markerStrings : List String :=
  ["depends on", "depend on", "depends-on", "dependents of", "blast radius",
    "safe to delete", "what uses"]

def markers : List (List Char) := markerStrings.map String.toList

/-- `to_lowercase`, per character (ASCII; see the header). -/
def lower (s : List Char) : List Char := s.map Char.toLower

/-- `hay.contains(needle)`: `needle` occurs contiguously somewhere in `hay`. -/
def containsSub (needle : List Char) : List Char → Bool
  | [] => needle.isPrefixOf []
  | h :: t => needle.isPrefixOf (h :: t) || containsSub needle t

/-- `classify`. -/
def classify (q : List Char) : Kind :=
  if markers.any (fun m => containsSub m (lower q)) then .structural else .general

theorem containsSub_iff (n : List Char) : ∀ hay, containsSub n hay = true ↔ n <:+: hay
  | [] => by
    simp only [containsSub, List.isPrefixOf_iff_prefix, List.prefix_nil, List.infix_nil]
  | h :: t => by
    simp only [containsSub, Bool.or_eq_true, List.isPrefixOf_iff_prefix, containsSub_iff n t,
      List.infix_cons_iff]

/-- **Exactness.** A query is structural iff its lowercase text contains a
marker. -/
theorem classify_iff (q : List Char) :
    classify q = .structural ↔ ∃ m ∈ markers, m <:+: lower q := by
  unfold classify
  constructor
  · intro h
    split at h
    · rename_i hany
      obtain ⟨m, hm, hc⟩ := List.any_eq_true.mp hany
      exact ⟨m, hm, (containsSub_iff m _).mp hc⟩
    · exact absurd h (by decide)
  · rintro ⟨m, hm, hi⟩
    have : markers.any (fun m => containsSub m (lower q)) = true :=
      List.any_eq_true.mpr ⟨m, hm, (containsSub_iff m _).mpr hi⟩
    rw [ite_eq_left this]

theorem classify_general_iff (q : List Char) :
    classify q = .general ↔ ∀ m ∈ markers, ¬ m <:+: lower q := by
  have h := classify_iff q
  constructor
  · intro hg m hm hi
    have := h.mpr ⟨m, hm, hi⟩
    rw [hg] at this
    exact absurd this (by decide)
  · intro hn
    cases hc : classify q with
    | general => rfl
    | structural =>
      obtain ⟨m, hm, hi⟩ := h.mp hc
      exact absurd hi (hn m hm)

/-- The markers are already lowercase. -/
theorem markers_lower : ∀ m ∈ markers, lower m = m := by decide

theorem lower_append (a b : List Char) : lower (a ++ b) = lower a ++ lower b :=
  List.map_append

/-- **Every marker classifies structural**, in any ASCII case and whatever
surrounds it: `m` is any text whose lowercase is a marker. -/
theorem classify_marker {m : List Char} (hm : lower m ∈ markers) (pre post : List Char) :
    classify (pre ++ m ++ post) = .structural := by
  apply (classify_iff _).mpr
  refine ⟨lower m, hm, lower pre, lower post, ?_⟩
  rw [lower_append, lower_append]

/-- Each marker on its own is a structural query. -/
theorem classify_each_marker : ∀ m ∈ markers, classify m = .structural := by
  intro m hm
  have := classify_marker (m := m) (by rw [markers_lower m hm]; exact hm) [] []
  simpa using this

/-- **Matching ignores word boundaries.** A marker glued to the end of a
longer word (`c` is any character, a letter included) still classifies
structural: `classify` is a substring test, not the word-bounded match anchor
resolution uses. -/
theorem classify_inside_word {m : List Char} (hm : lower m ∈ markers) (pre : List Char) (c : Char)
    (post : List Char) : classify (pre ++ c :: m ++ post) = .structural := by
  have := classify_marker hm (pre ++ [c]) post
  simpa using this

/-- **Matching ignores word boundaries, at the other end too.** A marker
that runs into the start of the next word (`c` is any character, a letter
included) still classifies structural. -/
theorem classify_into_word {m : List Char} (hm : lower m ∈ markers) (pre : List Char) (c : Char)
    (post : List Char) : classify (pre ++ m ++ c :: post) = .structural :=
  classify_marker hm pre (c :: post)

/-- "depends only" contains the marker `depends on`: a statement about what a
value is computed from routes to the structural arm. -/
theorem depends_only_structural :
    classify "the answer depends only on the score".toList = .structural := by decide

/-- "unsafe to delete" contains the marker `safe to delete` (harmless: it is
still a delete-safety question). -/
theorem unsafe_to_delete_structural :
    classify "is it unsafe to delete it".toList = .structural := by decide

/-- "independents of" contains the marker `dependents of`. -/
theorem independents_of_structural :
    classify "who are the independents of the senate".toList = .structural := by decide

/-- "somewhat uses" contains the marker `what uses`. -/
theorem somewhat_uses_structural :
    classify "the team somewhat uses the old schema".toList = .structural := by decide

/-- **Bare `depend` is not a marker**, and the doc's example question with
"independent" stays general: no marker is a substring of it. -/
theorem depend_not_marker :
    "depend".toList ∉ markers ∧
      classify "is the system independent of a single region".toList = .general := by
  decide

/-! Tests `classifies_dependency_and_delete_safety_questions_structural` and
`marker_bearing_non_dependency_questions_stay_general`. -/

example : classify "what depends on SG-Base-VPC".toList = .structural := by decide
example : classify "is it safe to delete the shared security group".toList = .structural := by
  decide
example : classify "does anything depend on this".toList = .structural := by decide
example : classify "update user schema".toList = .general := by decide
example : classify "pagination".toList = .general := by decide
example : classify "the report references the changelog".toList = .general := by decide
/-- Case does not matter. -/
example : classify "WHAT DEPENDS ON SG-Base-VPC".toList = .structural := by decide
example : classify "What Uses the VPC".toList = .structural := by decide
/-- The doc's other dropped marker, "is it safe", alone is general. -/
example : classify "is it safe to rotate the key".toList = .general := by decide

/-! ## The word-bounded classifier (#299)

The fixed `classify` counts a marker only where it stands on word boundaries,
by the rule `word_bounded_spans` applies to anchor names: a side of the marker
that ends in a word character must meet the start or end of the query, or a
character that is not part of a word. `isWord` is `is_word_char` (Unicode
letters and digits, `_`, combining marks); it is a parameter, so every theorem
below holds for whatever the Rust counts as a word character, and the `decide`
examples use `asciiWord`, which agrees with `is_word_char` on ASCII. -/

/-- `word_at`: the character `c`, with `prev` before it and `next` after it,
belongs to a word: a word character, or a hyphen between two word characters. -/
def wordAt (isWord : Char → Bool) (prev : Option Char) (c : Char) (next : Option Char) : Bool :=
  isWord c || (c == '-' && prev.any isWord && next.any isWord)

/-- `word_bounded_spans`'s left test: `rb` is the text before the occurrence,
reversed (its head is the character just before it). -/
def leftOk (isWord : Char → Bool) (m rb : List Char) : Bool :=
  match m, rb with
  | [], _ => true
  | _, [] => true
  | f :: _, p :: pp => !isWord f || !wordAt isWord pp.head? p (some f)

/-- `word_bounded_spans`'s right test: `post` is the text after the
occurrence. -/
def rightOk (isWord : Char → Bool) (m post : List Char) : Bool :=
  match m.getLast?, post with
  | none, _ => true
  | _, [] => true
  | some l, c :: cs => !isWord l || !wordAt isWord (some l) c cs.head?

/-- The scan of `word_bounded_spans`, stopping at the first bounded
occurrence: `rb` is the text already passed, reversed. -/
def scanWB (isWord : Char → Bool) (m : List Char) : List Char → List Char → Bool
  | rb, [] => m.isPrefixOf [] && leftOk isWord m rb && rightOk isWord m []
  | rb, h :: t =>
    (m.isPrefixOf (h :: t) && leftOk isWord m rb && rightOk isWord m ((h :: t).drop m.length)) ||
      scanWB isWord m (h :: rb) t

/-- `!word_bounded_spans(hay, m).is_empty()`, which is empty for an empty
needle. -/
def containsWB (isWord : Char → Bool) (m hay : List Char) : Bool :=
  !m.isEmpty && scanWB isWord m [] hay

/-- `classify` after #299. -/
def classifyWB (isWord : Char → Bool) (q : List Char) : Kind :=
  if markers.any (fun m => containsWB isWord m (lower q)) then .structural else .general

/-- `m` occurs in `hay` on word boundaries. -/
def BoundedOcc (isWord : Char → Bool) (m hay : List Char) : Prop :=
  m ≠ [] ∧ ∃ pre post, hay = pre ++ m ++ post ∧ leftOk isWord m pre.reverse = true ∧
    rightOk isWord m post = true

theorem scanWB_iff (isWord : Char → Bool) (m : List Char) :
    ∀ hay rb : List Char, scanWB isWord m rb hay = true ↔
      ∃ pre post, hay = pre ++ m ++ post ∧ leftOk isWord m (pre.reverse ++ rb) = true ∧
        rightOk isWord m post = true
  | [], rb => by
    simp only [scanWB, Bool.and_eq_true, List.isPrefixOf_iff_prefix, List.prefix_nil]
    constructor
    · rintro ⟨⟨rfl, hl⟩, hr⟩
      exact ⟨[], [], rfl, hl, hr⟩
    · rintro ⟨pre, post, h, hl, hr⟩
      obtain ⟨hpm, rfl⟩ := List.append_eq_nil_iff.mp h.symm
      obtain ⟨rfl, rfl⟩ := List.append_eq_nil_iff.mp hpm
      exact ⟨⟨rfl, by simpa using hl⟩, hr⟩
  | h :: t, rb => by
    have ih := scanWB_iff isWord m t (h :: rb)
    simp only [scanWB, Bool.or_eq_true, Bool.and_eq_true, List.isPrefixOf_iff_prefix, ih]
    constructor
    · rintro (⟨⟨hp, hl⟩, hr⟩ | ⟨pre, post, ht, hl, hr⟩)
      · refine ⟨[], (h :: t).drop m.length, ?_, by simpa using hl, hr⟩
        rw [List.nil_append]
        exact (List.prefix_iff_eq_append.mp hp).symm
      · refine ⟨h :: pre, post, by rw [ht]; simp, ?_, hr⟩
        simpa using hl
    · rintro ⟨pre, post, hh, hl, hr⟩
      cases pre with
      | nil =>
        left
        rw [List.nil_append] at hh
        refine ⟨⟨?_, by simpa using hl⟩, ?_⟩
        · rw [hh]; exact List.prefix_append m post
        · rw [hh, List.drop_left]; exact hr
      | cons p pre' =>
        right
        simp only [List.cons_append, List.cons.injEq] at hh
        obtain ⟨rfl, ht⟩ := hh
        exact ⟨pre', post, ht, by simpa using hl, hr⟩

theorem containsWB_iff (isWord : Char → Bool) (m hay : List Char) :
    containsWB isWord m hay = true ↔ BoundedOcc isWord m hay := by
  unfold containsWB BoundedOcc
  rw [Bool.and_eq_true, scanWB_iff]
  simp only [List.append_nil, Bool.not_eq_true', List.isEmpty_eq_false_iff]

/-- **Exactness.** A query is structural iff a marker occurs in its lowercase
text on word boundaries. -/
theorem classifyWB_iff (isWord : Char → Bool) (q : List Char) :
    classifyWB isWord q = .structural ↔ ∃ m ∈ markers, BoundedOcc isWord m (lower q) := by
  unfold classifyWB
  constructor
  · intro h
    split at h
    · rename_i hany
      obtain ⟨m, hm, hc⟩ := List.any_eq_true.mp hany
      exact ⟨m, hm, (containsWB_iff isWord m _).mp hc⟩
    · exact absurd h (by decide)
  · rintro ⟨m, hm, hb⟩
    have : markers.any (fun m => containsWB isWord m (lower q)) = true :=
      List.any_eq_true.mpr ⟨m, hm, (containsWB_iff isWord m _).mpr hb⟩
    rw [ite_eq_left this]

/-- **The fix only narrows.** A query the fixed classifier calls structural
was structural before: a bounded occurrence is an occurrence. -/
theorem classifyWB_structural {isWord : Char → Bool} {q : List Char}
    (h : classifyWB isWord q = .structural) : classify q = .structural := by
  obtain ⟨m, hm, _, pre, post, he, _, _⟩ := (classifyWB_iff isWord q).mp h
  exact (classify_iff q).mpr ⟨m, hm, pre, post, he.symm⟩

/-- A general query stays general. -/
theorem classifyWB_general {isWord : Char → Bool} {q : List Char}
    (h : classify q = .general) : classifyWB isWord q = .general := by
  cases hc : classifyWB isWord q with
  | general => rfl
  | structural => rw [classifyWB_structural hc] at h; exact absurd h (by decide)

/-- **Agreement.** When every marker occurrence in a query stands on word
boundaries, the fixed classifier answers as the old one did. -/
theorem classifyWB_agrees (isWord : Char → Bool) (q : List Char)
    (hb : ∀ m ∈ markers, ∀ pre post, lower q = pre ++ m ++ post →
      leftOk isWord m pre.reverse = true ∧ rightOk isWord m post = true) :
    classifyWB isWord q = classify q := by
  cases hc : classify q with
  | general => exact classifyWB_general hc
  | structural =>
    obtain ⟨m, hm, pre, post, he⟩ := (classify_iff q).mp hc
    obtain ⟨hl, hr⟩ := hb m hm pre post he.symm
    have hne : m ≠ [] := by
      intro h0; subst h0; exact absurd hm (by decide)
    exact (classifyWB_iff isWord q).mpr ⟨m, hm, hne, pre, post, he.symm, hl, hr⟩

/-- Every marker is non-empty and starts and ends with a lowercase letter. -/
theorem markers_ends :
    ∀ m ∈ markers, m ≠ [] ∧ m.head?.all Char.isLower = true ∧ m.getLast?.all Char.isLower = true := by
  decide

/-- **A marker glued to the end of a word does not count.** An occurrence
with a word character just before it is not bounded, whenever letters are
word characters (`is_word_char` counts every alphanumeric). -/
theorem wb_inside_word {isWord : Char → Bool} (hL : ∀ c : Char, c.isLower = true → isWord c = true)
    {m : List Char} (hm : m ∈ markers) {c : Char} (hc : isWord c = true) (rb : List Char) :
    leftOk isWord m (c :: rb) = false := by
  obtain ⟨hne, hh, _⟩ := markers_ends m hm
  match m, hne, hh with
  | [], hne, _ => exact absurd rfl hne
  | f :: _, _, hh =>
    have hf : isWord f = true := hL f (by simpa using hh)
    simp [leftOk, wordAt, hf, hc]

/-- **A marker running into the next word does not count.** An occurrence
with a word character just after it is not bounded. -/
theorem wb_into_word {isWord : Char → Bool} (hL : ∀ c : Char, c.isLower = true → isWord c = true)
    {m : List Char} (hm : m ∈ markers) {c : Char} (hc : isWord c = true) (cs : List Char) :
    rightOk isWord m (c :: cs) = false := by
  obtain ⟨hne, _, hl⟩ := markers_ends m hm
  cases hg : m.getLast? with
  | none => exact absurd (List.getLast?_eq_none_iff.mp hg) hne
  | some l =>
    rw [hg] at hl
    have hlw : isWord l = true := hL l (by simpa using hl)
    simp [rightOk, hg, wordAt, hlw, hc]

/-- **A hyphen between word characters joins them**, so a marker hyphenated
onto a word ("co-dependents of") does not count: the hyphen before it belongs
to the word. -/
theorem wb_hyphen_before {isWord : Char → Bool}
    (hL : ∀ c : Char, c.isLower = true → isWord c = true) {m : List Char} (hm : m ∈ markers)
    {c : Char} (hc : isWord c = true) (rb : List Char) :
    leftOk isWord m ('-' :: c :: rb) = false := by
  obtain ⟨hne, hh, _⟩ := markers_ends m hm
  match m, hne, hh with
  | [], hne, _ => exact absurd rfl hne
  | f :: _, _, hh =>
    have hf : isWord f = true := hL f (by simpa using hh)
    simp [leftOk, wordAt, hf, hc]

/-- The mirror: a marker hyphenated onto the next word does not count. -/
theorem wb_hyphen_after {isWord : Char → Bool}
    (hL : ∀ c : Char, c.isLower = true → isWord c = true) {m : List Char} (hm : m ∈ markers)
    {c : Char} (hc : isWord c = true) (cs : List Char) :
    rightOk isWord m ('-' :: c :: cs) = false := by
  obtain ⟨hne, _, hl⟩ := markers_ends m hm
  cases hg : m.getLast? with
  | none => exact absurd (List.getLast?_eq_none_iff.mp hg) hne
  | some l =>
    rw [hg] at hl
    have hlw : isWord l = true := hL l (by simpa using hl)
    simp [rightOk, hg, wordAt, hlw, hc]

/-- A character that ends a word: not a word character, not a hyphen. -/
def Sep (isWord : Char → Bool) (c : Char) : Prop := isWord c = false ∧ c ≠ '-'

theorem leftOk_sep {isWord : Char → Bool} (m : List Char) {rb : List Char}
    (h : ∀ c ∈ rb.head?, Sep isWord c) : leftOk isWord m rb = true := by
  match m, rb with
  | [], _ => rfl
  | _ :: _, [] => rfl
  | f :: _, p :: pp =>
    obtain ⟨h1, h2⟩ := h p rfl
    simp [leftOk, wordAt, h1, h2]

theorem rightOk_sep {isWord : Char → Bool} (m : List Char) {post : List Char}
    (h : ∀ c ∈ post.head?, Sep isWord c) : rightOk isWord m post = true := by
  unfold rightOk
  match m.getLast?, post with
  | none, _ => rfl
  | some _, [] => rfl
  | some l, c :: cs =>
    obtain ⟨h1, h2⟩ := h c rfl
    simp [wordAt, h1, h2]

/-- **Every marker as whole words classifies structural**, in any ASCII
case, for any word-character rule: `m` is any text whose lowercase is a
marker, and the text around it ends (and starts) with a character that ends a
word, or nothing. -/
theorem classifyWB_marker (isWord : Char → Bool) {m : List Char} (hm : lower m ∈ markers)
    (pre post : List Char) (hpre : ∀ c ∈ (lower pre).getLast?, Sep isWord c)
    (hpost : ∀ c ∈ (lower post).head?, Sep isWord c) :
    classifyWB isWord (pre ++ m ++ post) = .structural := by
  apply (classifyWB_iff isWord _).mpr
  have hne : lower m ≠ [] := (markers_ends _ hm).1
  refine ⟨lower m, hm, hne, lower pre, lower post, by rw [lower_append, lower_append], ?_, ?_⟩
  · apply leftOk_sep
    rw [List.head?_reverse]; exact hpre
  · exact rightOk_sep _ hpost

/-- Each marker on its own is a structural query, for any word-character
rule. -/
theorem classifyWB_each_marker (isWord : Char → Bool) :
    ∀ m ∈ markers, classifyWB isWord m = .structural := by
  intro m hm
  have := classifyWB_marker isWord (m := m) (by rw [markers_lower m hm]; exact hm) [] []
    (by simp [lower]) (by simp [lower])
  simpa using this

/-- `is_word_char` on ASCII: letters, digits and `_`. -/
def asciiWord (c : Char) : Bool := c.isAlphanum || c == '_'

theorem asciiWord_letters : ∀ c : Char, c.isLower = true → asciiWord c = true := by
  intro c h; simp [asciiWord, Char.isAlphanum, Char.isAlpha, h]

/-- **The issue's four queries classify general** under the fixed classifier:
each marker in them is glued to a longer word. -/
theorem issue_queries_general :
    classifyWB asciiWord "the answer depends only on the score".toList = .general ∧
      classifyWB asciiWord "who are the independents of the senate".toList = .general ∧
      classifyWB asciiWord "the team somewhat uses the old schema".toList = .general ∧
      classifyWB asciiWord "is it unsafe to delete it".toList = .general := by
  decide

/-! The fixed classifier on the tests' and the docs' queries: every
dependency question stays structural, every general one stays general. -/

example : classifyWB asciiWord "what depends on SG-Base-VPC".toList = .structural := by decide
example : classifyWB asciiWord "is it safe to delete the shared security group".toList =
    .structural := by decide
example : classifyWB asciiWord "does anything depend on this".toList = .structural := by decide
example : classifyWB asciiWord "WHAT DEPENDS ON SG-Base-VPC".toList = .structural := by decide
example : classifyWB asciiWord "What Uses the VPC".toList = .structural := by decide
example : classifyWB asciiWord "dependents of SG-Base-VPC".toList = .structural := by decide
example : classifyWB asciiWord "blast radius of user schema".toList = .structural := by decide
example : classifyWB asciiWord "x depends-on y".toList = .structural := by decide
example : classifyWB asciiWord "what depends on (SG-Base) - db".toList = .structural := by decide
example : classifyWB asciiWord "is it safe to delete?".toList = .structural := by decide
example : classifyWB asciiWord "update user schema".toList = .general := by decide
example : classifyWB asciiWord "pagination".toList = .general := by decide
example : classifyWB asciiWord "the report references the changelog".toList = .general := by
  decide
example : classifyWB asciiWord "is the system independent of a single region".toList =
    .general := by decide
example : classifyWB asciiWord "is it safe to rotate the key".toList = .general := by decide
/-- A hyphen between word characters joins them, so a marker hyphenated onto
a word does not count either. -/
example : classifyWB asciiWord "co-dependents of the senate".toList = .general := by decide

/-! ## The graph and the dependents relation -/

/-- `EdgeType`, every variant. -/
inductive EdgeType where
  | dependency
  | hierarchical
  | temporal
  | derives
  | coOccurrence
  | causal
  | semantic
  | contradicts
  | suspectedSupersedes
  deriving DecidableEq, Repr

/-- `DependentEnd`. -/
inductive DependentEnd where
  | source
  | target
  deriving DecidableEq, Repr

/-- `EdgeType::dependent_end`: `Dependency`'s source rests on its target,
`Hierarchical`'s target (the child) on its source; no other type declares a
dependence. -/
def EdgeType.dependentEnd : EdgeType → Option DependentEnd
  | .dependency => some .source
  | .hierarchical => some .target
  | _ => none

structure Edge where
  src : Nat
  tgt : Nat
  ty : EdgeType
  w : Nat
  deriving DecidableEq, Repr

/-- `Edge::dependence`: `(pillar, dependent)`, never for a self-loop. -/
def Edge.dependence (e : Edge) : Option (Nat × Nat) :=
  if e.src = e.tgt then none
  else
    match e.ty.dependentEnd with
    | some .source => some (e.tgt, e.src)
    | some .target => some (e.src, e.tgt)
    | none => none

theorem Edge.dependence_ne {e : Edge} {p d : Nat} (h : e.dependence = some (p, d)) : p ≠ d := by
  unfold Edge.dependence at h
  split at h
  · exact absurd h (by simp)
  · rename_i hne
    split at h <;> simp only [Option.some.injEq, Prod.mk.injEq, reduceCtorEq] at h
    · intro hpd; exact hne (by omega)
    · intro hpd; exact hne (by omega)

/-- **Only `Dependency` and `Hierarchical` declare a dependence.** `Causal`,
`Derives`, `Temporal`, `CoOccurrence`, `Semantic`, `Contradicts` and
`SuspectedSupersedes` edges never do. (The module doc of `dispatch.rs` lists
`Causal` among the traversed edge types; the code does not count it.) -/
theorem dependence_types {e : Edge} {p : Nat × Nat} (h : e.dependence = some p) :
    e.ty = .dependency ∨ e.ty = .hierarchical := by
  unfold Edge.dependence at h
  split at h
  · simp at h
  · cases hty : e.ty <;> simp [hty, EdgeType.dependentEnd] at h ⊢

/-- A graph: its edges, which ids are concepts, which are Canonical, and the
canonical keys with their order. -/
structure Graph (κ : Type) where
  isConcept : Nat → Bool
  canonical : Nat → Bool
  key : Nat → Option κ
  keyCmp : κ → κ → Ordering
  edges : List Edge

variable {κ : Type}

/-- `dependent_via`: the dependent this edge declares on `a`, when `a` is the
pillar and both ends are concepts. -/
def dependentVia (g : Graph κ) (a : Nat) (e : Edge) : Option Nat :=
  match e.dependence with
  | some (p, d) => if p = a ∧ g.isConcept p = true ∧ g.isConcept d = true then some d else none
  | none => none

/-- `d` is a direct structural dependent of `a`: one edge of the graph
declares it (spec §4.1 with the #36 errata). -/
def DirectDep (g : Graph κ) (a d : Nat) : Prop :=
  ∃ e ∈ g.edges, e.dependence = some (a, d) ∧ g.isConcept a = true ∧ g.isConcept d = true

theorem dependentVia_eq_some {g : Graph κ} {a d : Nat} {e : Edge} :
    dependentVia g a e = some d ↔
      e.dependence = some (a, d) ∧ g.isConcept a = true ∧ g.isConcept d = true := by
  unfold dependentVia
  split
  · rename_i p d' hdep
    rw [hdep]
    constructor
    · intro h
      split at h
      · rename_i hc
        simp only [Option.some.injEq] at h
        subst h
        obtain ⟨rfl, h1, h2⟩ := hc
        exact ⟨rfl, h1, h2⟩
      · exact absurd h (by simp)
    · rintro ⟨h, h1, h2⟩
      simp only [Option.some.injEq, Prod.mk.injEq] at h
      obtain ⟨rfl, rfl⟩ := h
      rw [ite_eq_left ⟨rfl, h1, h2⟩]
  · rename_i hdep
    rw [hdep]
    simp

/-- Fold one declaring edge into the `(dependent, weight)` table: a new
dependent enters with the edge's weight, a known one keeps the larger. -/
def insertMax (d w : Nat) : List (Nat × Nat) → List (Nat × Nat)
  | [] => [(d, w)]
  | (x, v) :: rest => if x = d then (x, max v w) :: rest else (x, v) :: insertMax d w rest

/-- `dependents_of`'s map, built over an edge list. -/
def collect (g : Graph κ) (a : Nat) : List Edge → List (Nat × Nat)
  | [] => []
  | e :: es =>
    match dependentVia g a e with
    | some d => insertMax d e.w (collect g a es)
    | none => collect g a es

/-- `format::dependents_of`: each direct dependent with its strongest weight.
(The Rust then sorts by id; `dependents` re-sorts, so that order is
irrelevant.) -/
def dependentsOf (g : Graph κ) (a : Nat) : List (Nat × Nat) := collect g a g.edges

/-- A dependent's score: its weight in `dependentsOf`. -/
def weight (g : Graph κ) (a d : Nat) : Nat := ((dependentsOf g a).lookup d).getD 0

theorem mem_fst_insertMax (d w x : Nat) :
    ∀ l : List (Nat × Nat), x ∈ (insertMax d w l).map Prod.fst ↔ x = d ∨ x ∈ l.map Prod.fst
  | [] => by simp [insertMax]
  | (y, v) :: rest => by
    unfold insertMax
    split
    · rename_i hy
      subst hy
      simp only [List.map_cons, List.mem_cons]
      constructor
      · intro h; exact Or.inr h
      · intro h
        cases h with
        | inl h => exact Or.inl h
        | inr h => exact h
    · simp only [List.map_cons, List.mem_cons, mem_fst_insertMax d w x rest]
      constructor
      · intro h
        rcases h with h | h | h
        · exact Or.inr (Or.inl h)
        · exact Or.inl h
        · exact Or.inr (Or.inr h)
      · intro h
        rcases h with h | h | h
        · exact Or.inr (Or.inl h)
        · exact Or.inl h
        · exact Or.inr (Or.inr h)

theorem nodup_insertMax (d w : Nat) :
    ∀ l : List (Nat × Nat), (l.map Prod.fst).Nodup → ((insertMax d w l).map Prod.fst).Nodup
  | [] => by intro _; simp [insertMax]
  | (y, v) :: rest => by
    intro h
    unfold insertMax
    split
    · simpa using h
    · rename_i hy
      simp only [List.map_cons, List.nodup_cons] at h ⊢
      refine ⟨?_, nodup_insertMax d w rest h.2⟩
      intro hm
      cases (mem_fst_insertMax d w y rest).mp hm with
      | inl e => exact hy e
      | inr e => exact h.1 e

theorem lookup_insertMax (d w x : Nat) :
    ∀ l : List (Nat × Nat), (insertMax d w l).lookup x =
      if x = d then some (match l.lookup d with | some v => max v w | none => w)
      else l.lookup x
  | [] => by
    by_cases hx : x = d
    · subst hx; simp [insertMax, List.lookup]
    · simp [insertMax, List.lookup, hx]
  | (y, v) :: rest => by
    unfold insertMax
    by_cases hy : y = d
    · subst hy
      rw [ite_eq_left rfl]
      by_cases hx : x = y
      · subst hx; simp [List.lookup]
      · have hb : (x == y) = false := by simp [hx]
        simp only [List.lookup, hb, ite_eq_right hx]
    · rw [ite_eq_right hy]
      have ih := lookup_insertMax d w x rest
      by_cases hx : x = d
      · subst hx
        have hxy : (x == y) = false := by simp [Ne.symm hy]
        simp only [List.lookup, hxy, ih]
      · by_cases hxy : x = y
        · subst hxy
          simp [List.lookup, hx]
        · have hb : (x == y) = false := by simp [hxy]
          simp only [List.lookup, hb, ih, ite_eq_right hx]

theorem mem_collect (g : Graph κ) (a x : Nat) :
    ∀ es : List Edge,
      x ∈ (collect g a es).map Prod.fst ↔ ∃ e ∈ es, dependentVia g a e = some x
  | [] => by simp [collect]
  | e :: es => by
    have ih := mem_collect g a x es
    unfold collect
    split
    · rename_i d hd
      rw [mem_fst_insertMax, ih]
      constructor
      · rintro (h | ⟨e', he', h⟩)
        · subst h; exact ⟨e, List.mem_cons_self, hd⟩
        · exact ⟨e', List.mem_cons_of_mem _ he', h⟩
      · rintro ⟨e', he', h⟩
        cases List.mem_cons.mp he' with
        | inl h' =>
          subst h'
          rw [hd] at h
          exact Or.inl (Option.some.inj h).symm
        | inr h' => exact Or.inr ⟨e', h', h⟩
    · rename_i hd
      rw [ih]
      constructor
      · rintro ⟨e', he', h⟩; exact ⟨e', List.mem_cons_of_mem _ he', h⟩
      · rintro ⟨e', he', h⟩
        cases List.mem_cons.mp he' with
        | inl h' => subst h'; rw [hd] at h; exact absurd h (by simp)
        | inr h' => exact ⟨e', h', h⟩

theorem nodup_collect (g : Graph κ) (a : Nat) :
    ∀ es : List Edge, ((collect g a es).map Prod.fst).Nodup
  | [] => by simp [collect]
  | e :: es => by
    unfold collect
    split
    · exact nodup_insertMax _ _ _ (nodup_collect g a es)
    · exact nodup_collect g a es

/-- The weight a dependent gets is the largest weight among the edges
declaring it, and one of them carries it. -/
theorem lookup_collect (g : Graph κ) (a d : Nat) :
    ∀ es : List Edge, ∀ w, (collect g a es).lookup d = some w →
      (∃ e ∈ es, dependentVia g a e = some d ∧ e.w = w) ∧
        ∀ e ∈ es, dependentVia g a e = some d → e.w ≤ w
  | [] => by intro w h; simp [collect] at h
  | e :: es => by
    intro w h
    have ih := lookup_collect g a d es
    unfold collect at h
    split at h
    · rename_i d' hd
      rw [lookup_insertMax] at h
      by_cases hdd : d = d'
      · subst hdd
        rw [ite_eq_left rfl] at h
        cases hl : (collect g a es).lookup d with
        | none =>
          rw [hl] at h
          simp only [Option.some.injEq] at h
          subst h
          refine ⟨⟨e, List.mem_cons_self, hd, rfl⟩, ?_⟩
          intro e' he' hd'
          cases List.mem_cons.mp he' with
          | inl h' => subst h'; exact Nat.le_refl _
          | inr h' =>
            have hnone : d ∈ (collect g a es).map Prod.fst :=
              (mem_collect g a d es).mpr ⟨e', h', hd'⟩
            obtain ⟨⟨k, v⟩, hkv, hk⟩ := List.mem_map.mp hnone
            simp only at hk
            subst hk
            have : (collect g a es).lookup k ≠ none := by
              intro hn
              have := List.lookup_eq_none_iff.mp hn
              exact absurd (this (k, v) hkv) (by simp)
            exact absurd hl this
        | some v =>
          rw [hl] at h
          simp only [Option.some.injEq] at h
          subst h
          obtain ⟨⟨e₀, he₀, hd₀, hw₀⟩, hmax⟩ := ih v hl
          refine ⟨?_, ?_⟩
          · by_cases hvw : e.w ≤ v
            · exact ⟨e₀, List.mem_cons_of_mem _ he₀, hd₀, by rw [hw₀]; omega⟩
            · exact ⟨e, List.mem_cons_self, hd, by omega⟩
          · intro e' he' hd'
            cases List.mem_cons.mp he' with
            | inl h' => subst h'; omega
            | inr h' => have := hmax e' h' hd'; omega
      · rw [ite_eq_right hdd] at h
        obtain ⟨⟨e₀, he₀, hd₀, hw₀⟩, hmax⟩ := ih w h
        refine ⟨⟨e₀, List.mem_cons_of_mem _ he₀, hd₀, hw₀⟩, ?_⟩
        intro e' he' hd'
        cases List.mem_cons.mp he' with
        | inl h' =>
          subst h'
          rw [hd] at hd'
          exact absurd (Option.some.inj hd').symm hdd
        | inr h' => exact hmax e' h' hd'
    · rename_i hd
      obtain ⟨⟨e₀, he₀, hd₀, hw₀⟩, hmax⟩ := ih w h
      refine ⟨⟨e₀, List.mem_cons_of_mem _ he₀, hd₀, hw₀⟩, ?_⟩
      intro e' he' hd'
      cases List.mem_cons.mp he' with
      | inl h' => subst h'; rw [hd] at hd'; exact absurd hd' (by simp)
      | inr h' => exact hmax e' h' hd'

/-! ## Sorting -/

/-- Insert into a list sorted by `le`, ahead of the first element it does not
come after. -/
def insertBy {α : Type} (le : α → α → Bool) (x : α) : List α → List α
  | [] => [x]
  | y :: ys => if le x y then x :: y :: ys else y :: insertBy le x ys

/-- Insertion sort. -/
def isort {α : Type} (le : α → α → Bool) : List α → List α
  | [] => []
  | x :: xs => insertBy le x (isort le xs)

section Sorting

variable {α : Type} {le : α → α → Bool}

theorem insertBy_perm (x : α) : ∀ l : List α, (insertBy le x l).Perm (x :: l)
  | [] => List.Perm.refl _
  | y :: ys => by
    unfold insertBy
    split
    · exact List.Perm.refl _
    · exact ((insertBy_perm x ys).cons y).trans (List.Perm.swap x y ys)

theorem isort_perm : ∀ l : List α, (isort le l).Perm l
  | [] => List.Perm.refl _
  | x :: xs => (insertBy_perm x _).trans ((isort_perm xs).cons x)

theorem insertBy_sorted
    (trans : ∀ a b c, le a b = true → le b c = true → le a c = true)
    (total : ∀ a b, le a b = false → le b a = true) (x : α) :
    ∀ l : List α, l.Pairwise (fun a b => le a b = true) →
      (insertBy le x l).Pairwise (fun a b => le a b = true)
  | [] => fun _ => List.pairwise_singleton _ _
  | y :: ys => by
    intro h
    have h' := List.pairwise_cons.mp h
    unfold insertBy
    split
    · rename_i hxy
      refine List.pairwise_cons.mpr ⟨?_, h⟩
      intro z hz
      cases List.mem_cons.mp hz with
      | inl e => subst e; exact hxy
      | inr m => exact trans x y z hxy (h'.1 z m)
    · rename_i hxy
      refine List.pairwise_cons.mpr ⟨?_, insertBy_sorted trans total x ys h'.2⟩
      intro z hz
      cases List.mem_cons.mp ((insertBy_perm x ys).mem_iff.mp hz) with
      | inl e => rw [e]; exact total x y (by simpa using hxy)
      | inr m => exact h'.1 z m

theorem isort_sorted
    (trans : ∀ a b c, le a b = true → le b c = true → le a c = true)
    (total : ∀ a b, le a b = false → le b a = true) :
    ∀ l : List α, (isort le l).Pairwise (fun a b => le a b = true)
  | [] => List.Pairwise.nil
  | x :: xs => insertBy_sorted trans total x _ (isort_sorted trans total xs)

end Sorting

section Comparator

variable {α : Type} {cmp : α → α → Ordering}

/-- The `le` a sort by `cmp` uses: "not after". -/
def leOf (cmp : α → α → Ordering) (a b : α) : Bool := cmp a b != .gt

theorem leOf_trans (h : Lawful cmp) (a b c : α) (hab : leOf cmp a b = true)
    (hbc : leOf cmp b c = true) : leOf cmp a c = true := by
  unfold leOf at *
  cases hx : cmp a b with
  | gt => rw [hx] at hab; exact absurd hab (by decide)
  | eq => rw [h.eq_left a b c hx]; exact hbc
  | lt =>
    cases hy : cmp b c with
    | gt => rw [hy] at hbc; exact absurd hbc (by decide)
    | eq => rw [h.eq_right a b c hy, hx]; decide
    | lt => rw [h.lt_trans a b c hx hy]; decide

theorem leOf_total (h : Lawful cmp) (a b : α) (hab : leOf cmp a b = false) :
    leOf cmp b a = true := by
  unfold leOf at *
  rw [h.swap a b]
  cases hx : cmp a b with
  | gt => decide
  | eq => rw [hx] at hab; exact absurd hab (by decide)
  | lt => rw [hx] at hab; exact absurd hab (by decide)

/-- Sorted by "not after", without repeats, under a separating comparator:
strictly sorted. -/
theorem strict_of_leOf (hsep : Separating cmp) {l : List α}
    (hs : l.Pairwise (fun a b => leOf cmp a b = true)) (hnd : l.Nodup) :
    l.Pairwise (fun a b => cmp a b = .lt) := by
  refine (hs.and hnd).imp ?_
  intro a b ⟨hle, hne⟩
  unfold leOf at hle
  cases hx : cmp a b with
  | lt => rfl
  | eq => exact absurd (hsep a b hx) hne
  | gt => rw [hx] at hle; exact absurd hle (by decide)

end Comparator

/-! ## `dependents`: the ranked direct dependents -/

/-- `dependents`'s comparator: `Order.rankMembers` with Canonical status, the
dependent's weight as score (descending), the canonical key and the id. -/
def rankCmp (g : Graph κ) (a : Nat) : Nat → Nat → Ordering :=
  rankMembers g.canonical natCmp (weight g a) g.keyCmp g.key natCmp

/-- `dependents(graph, anchor)`, as ids. -/
def ranked (g : Graph κ) (a : Nat) : List Nat :=
  isort (leOf (rankCmp g a)) ((dependentsOf g a).map Prod.fst)

theorem rankCmp_lawful (g : Graph κ) (hk : Lawful g.keyCmp) (a : Nat) :
    Lawful (rankCmp g a) :=
  rankMembers_lawful g.canonical natCmp_lawful (weight g a) hk g.key natCmp_lawful

theorem rankCmp_separating (g : Graph κ) (a : Nat) : Separating (rankCmp g a) :=
  rankMembers_separating g.canonical natCmp (weight g a) g.keyCmp g.key natCmp_separating

/-- **Sound and complete.** The dependents are exactly the anchor's direct
structural dependents: a node is listed iff a single edge declares it (so no
node two hops away is), and both ends are concepts. -/
theorem mem_ranked (g : Graph κ) (a x : Nat) : x ∈ ranked g a ↔ DirectDep g a x := by
  unfold ranked
  rw [(isort_perm _).mem_iff, dependentsOf, mem_collect]
  constructor
  · rintro ⟨e, he, h⟩
    exact ⟨e, he, dependentVia_eq_some.mp h⟩
  · rintro ⟨e, he, h⟩
    exact ⟨e, he, dependentVia_eq_some.mpr h⟩

theorem ranked_nodup (g : Graph κ) (a : Nat) : (ranked g a).Nodup :=
  (isort_perm _).nodup_iff.mpr (nodup_collect g a g.edges)

/-- Every dependent is a concept, so the traversal's `graph.node(s.item)?`
never refuses. -/
theorem ranked_concepts (g : Graph κ) (a x : Nat) (h : x ∈ ranked g a) :
    g.isConcept x = true := by
  obtain ⟨_, _, _, _, hx⟩ := (mem_ranked g a x).mp h
  exact hx

/-- "The anchor itself is never its own dependent." -/
theorem anchor_not_mem_ranked (g : Graph κ) (a : Nat) : a ∉ ranked g a := by
  intro h
  obtain ⟨e, _, hd, _, _⟩ := (mem_ranked g a a).mp h
  exact Edge.dependence_ne hd rfl

/-- **Strongest edge.** "Score is the strongest weight among the edges
declaring the dependence." -/
theorem weight_spec (g : Graph κ) (a x : Nat) (h : x ∈ ranked g a) :
    (∃ e ∈ g.edges, dependentVia g a e = some x ∧ e.w = weight g a x) ∧
      ∀ e ∈ g.edges, dependentVia g a e = some x → e.w ≤ weight g a x := by
  have hm : x ∈ (dependentsOf g a).map Prod.fst := (isort_perm _).mem_iff.mp h
  obtain ⟨⟨k, v⟩, hkv, hk⟩ := List.mem_map.mp hm
  simp only at hk
  subst hk
  cases hl : (dependentsOf g a).lookup k with
  | none =>
    have := List.lookup_eq_none_iff.mp hl (k, v) hkv
    exact absurd this (by simp)
  | some w =>
    have hw : weight g a k = w := by simp [weight, hl]
    rw [hw]
    exact lookup_collect g a k g.edges w hl

/-- **Canonical-first order.** The dependents are strictly sorted by
`rank_members`'s comparator. -/
theorem ranked_sorted (g : Graph κ) (hk : Lawful g.keyCmp) (a : Nat) :
    (ranked g a).Pairwise (fun x y => rankCmp g a x y = .lt) :=
  strict_of_leOf (rankCmp_separating g a)
    (isort_sorted (leOf_trans (rankCmp_lawful g hk a)) (leOf_total (rankCmp_lawful g hk a)) _)
    (ranked_nodup g a)

/-- No non-Canonical dependent comes before a Canonical one. -/
theorem ranked_canonical_first (g : Graph κ) (hk : Lawful g.keyCmp) (a : Nat) :
    (ranked g a).Pairwise (fun x y => g.canonical y = true → g.canonical x = true) := by
  refine (ranked_sorted g hk a).imp ?_
  intro x y hxy hy
  cases hx : g.canonical x with
  | true => rfl
  | false =>
    have hyx := rankMembers_canonical_first g.canonical natCmp (weight g a) g.keyCmp g.key
      natCmp hy hx
    have := (rankCmp_lawful g hk a).gt_of_lt hxy
    change rankCmp g a y x = .gt at this
    rw [show rankCmp g a y x = .lt from hyx] at this
    exact absurd this (by decide)

/-- Within one Canonical class the stronger-bound dependent comes first. -/
theorem ranked_weight_desc (g : Graph κ) (hk : Lawful g.keyCmp) (a : Nat) :
    (ranked g a).Pairwise
      (fun x y => g.canonical x = g.canonical y → weight g a y ≤ weight g a x) := by
  refine (ranked_sorted g hk a).imp ?_
  intro x y hxy hc
  apply Nat.le_of_not_lt
  intro hlt
  have hs : natCmp (weight g a x) (weight g a y) = .lt := by simp [natCmp, hlt]
  have hyx := rankMembers_score_desc g.canonical natCmp (weight g a) g.keyCmp g.key natCmp
    hc.symm hs
  have := (rankCmp_lawful g hk a).gt_of_lt hxy
  change rankCmp g a y x = .gt at this
  rw [show rankCmp g a y x = .lt from hyx] at this
  exact absurd this (by decide)

/-- **Determinism.** Any strictly sorted list of the direct dependents is
`ranked`: the Rust's merge sort and this insertion sort agree. -/
theorem ranked_unique (g : Graph κ) (hk : Lawful g.keyCmp) (a : Nat) (l : List Nat)
    (hs : l.Pairwise (fun x y => rankCmp g a x y = .lt)) (hm : ∀ x, x ∈ l ↔ DirectDep g a x) :
    l = ranked g a :=
  sorted_unique (rankCmp_lawful g hk a) l (ranked g a) hs (ranked_sorted g hk a)
    (fun x => (hm x).trans (mem_ranked g a x).symm)

/-! ## The traversal answer -/

/-- `AnchorChoice`. -/
inductive Choice where
  | one (anchor : Nat)
  | ambiguous (ids : List Nat)
  deriving DecidableEq, Repr

/-- What a dispatched recall answers: the anchor's dependents in rank order,
or the #129 warning-only answer for an ambiguous subject. -/
inductive Answer where
  | traversal (anchor : Nat) (hits : List Nat)
  | ambiguous (ids : List Nat)
  deriving DecidableEq, Repr

/-- `try_structural_with`. `traversalDepth` is the query's
`traversal_depth`, which the Rust does not read ("intentionally not
honored"); it is a parameter here so that `tryStructuralWith_depth` can say
so. -/
def tryStructuralWith (g : Graph κ) (choice : Option Choice) (topK _traversalDepth : Nat) :
    Option Answer :=
  match choice with
  | none => none
  | some (.ambiguous ids) => some (.ambiguous ids)
  | some (.one a) =>
    if ranked g a = [] then none
    else if (ranked g a).take topK = [] then none
    else some (.traversal a ((ranked g a).take topK))

/-- `try_structural`: only a query that classifies structural is tried.
`resolve` is the anchor resolution, not modelled. -/
def tryStructural (g : Graph κ) (resolve : List Char → Option Choice) (q : List Char)
    (topK traversalDepth : Nat) : Option Answer :=
  if classify q = .structural then tryStructuralWith g (resolve q) topK traversalDepth else none

/-- `anchor_dispatches`. -/
def anchorDispatches (g : Graph κ) : Option Choice → Bool
  | some (.one a) => !(ranked g a).isEmpty
  | some (.ambiguous _) => true
  | none => false

/-- `fits_structural_with`. -/
def fitsStructuralWith (g : Graph κ) (choice : Option Choice) (topK : Nat) : Bool :=
  match choice with
  | some (.one _) => if topK = 0 then false else anchorDispatches g choice
  | _ => anchorDispatches g choice

/-- `fits_structural`. -/
def fitsStructural (g : Graph κ) (resolve : List Char → Option Choice) (q : List Char) : Bool :=
  classify q == .structural && anchorDispatches g (resolve q)

/-- **`traversal_depth` is not honored**: the answer is the same at every
depth. -/
theorem tryStructuralWith_depth (g : Graph κ) (c : Option Choice) (k d₁ d₂ : Nat) :
    tryStructuralWith g c k d₁ = tryStructuralWith g c k d₂ := rfl

/-- A single-anchor answer is the first `top_k` ranked dependents, and is not
empty. -/
theorem traversal_hits {g : Graph κ} {a k d : Nat} {ans : Answer}
    (h : tryStructuralWith g (some (.one a)) k d = some ans) :
    ans = .traversal a ((ranked g a).take k) ∧ (ranked g a).take k ≠ [] := by
  change (if ranked g a = [] then none
    else if (ranked g a).take k = [] then none
    else some (Answer.traversal a ((ranked g a).take k))) = some ans at h
  split at h
  · exact absurd h (by simp)
  · split at h
    · exact absurd h (by simp)
    · rename_i hne
      exact ⟨(Option.some.inj h).symm, hne⟩

/-- **Sound.** Every hit is a direct structural dependent of the anchor. -/
theorem traversal_sound {g : Graph κ} {a a' k d : Nat} {hits : List Nat}
    (h : tryStructuralWith g (some (.one a)) k d = some (.traversal a' hits)) :
    a' = a ∧ ∀ x ∈ hits, DirectDep g a x := by
  obtain ⟨he, _⟩ := traversal_hits h
  simp only [Answer.traversal.injEq] at he
  obtain ⟨h1, h2⟩ := he
  refine ⟨h1, fun x hx => ?_⟩
  rw [h2] at hx
  exact (mem_ranked g a x).mp (List.mem_of_mem_take hx)

/-- **At most `top_k` hits.** -/
theorem traversal_length_le {g : Graph κ} {a a' k d : Nat} {hits : List Nat}
    (h : tryStructuralWith g (some (.one a)) k d = some (.traversal a' hits)) :
    hits.length ≤ k := by
  obtain ⟨he, _⟩ := traversal_hits h
  simp only [Answer.traversal.injEq] at he
  rw [he.2, List.length_take]
  exact Nat.min_le_left _ _

/-- The hits are strictly in `rank_members` order. -/
theorem traversal_sorted {g : Graph κ} (hk : Lawful g.keyCmp) {a a' k d : Nat}
    {hits : List Nat}
    (h : tryStructuralWith g (some (.one a)) k d = some (.traversal a' hits)) :
    hits.Pairwise (fun x y => rankCmp g a x y = .lt) := by
  obtain ⟨he, _⟩ := traversal_hits h
  simp only [Answer.traversal.injEq] at he
  rw [he.2]
  exact (ranked_sorted g hk a).sublist (List.take_sublist _ _)

/-- **Canonical first** in the answer. -/
theorem traversal_canonical_first {g : Graph κ} (hk : Lawful g.keyCmp) {a a' k d : Nat}
    {hits : List Nat}
    (h : tryStructuralWith g (some (.one a)) k d = some (.traversal a' hits)) :
    hits.Pairwise (fun x y => g.canonical y = true → g.canonical x = true) := by
  obtain ⟨he, _⟩ := traversal_hits h
  simp only [Answer.traversal.injEq] at he
  rw [he.2]
  exact (ranked_canonical_first g hk a).sublist (List.take_sublist _ _)

/-- **Complete up to the cut.** At a positive `top_k`, a direct dependent
makes the dispatch fire, and it is either a hit or was cut behind `top_k`
hits that all rank before it. -/
theorem traversal_complete (g : Graph κ) (hk : Lawful g.keyCmp) {a x k : Nat} (d : Nat)
    (hx : DirectDep g a x) (hk0 : 0 < k) :
    ∃ hits, tryStructuralWith g (some (.one a)) k d = some (.traversal a hits) ∧
      (x ∈ hits ∨ (hits.length = k ∧ ∀ y ∈ hits, rankCmp g a y x = .lt)) := by
  have hm : x ∈ ranked g a := (mem_ranked g a x).mpr hx
  have hne : ranked g a ≠ [] := List.ne_nil_of_mem hm
  have htk : (ranked g a).take k ≠ [] := by
    intro h0
    rcases List.take_eq_nil_iff.mp h0 with h0 | h0
    · omega
    · exact hne h0
  refine ⟨(ranked g a).take k, ?_, ?_⟩
  · simp [tryStructuralWith, hne, htk]
  · by_cases hin : x ∈ (ranked g a).take k
    · exact Or.inl hin
    · right
      have hsplit := List.take_append_drop k (ranked g a)
      have hdrop : x ∈ (ranked g a).drop k := by
        rw [← hsplit] at hm
        rcases List.mem_append.mp hm with h1 | h1
        · exact absurd h1 hin
        · exact h1
      have hlen : k < (ranked g a).length := by
        rcases Nat.lt_or_ge k (ranked g a).length with h1 | h1
        · exact h1
        · rw [List.drop_eq_nil_of_le h1] at hdrop; simp at hdrop
      refine ⟨by rw [List.length_take]; omega, ?_⟩
      intro y hy
      have hs := ranked_sorted g hk a
      rw [← hsplit] at hs
      exact (List.pairwise_append.mp hs).2.2 y hy x hdrop

/-- **Exact.** When `top_k` covers every dependent, the hits are exactly the
direct dependents, for any `traversal_depth`. -/
theorem traversal_exact {g : Graph κ} {a a' k d : Nat} {hits : List Nat}
    (h : tryStructuralWith g (some (.one a)) k d = some (.traversal a' hits))
    (hk : (ranked g a).length ≤ k) (x : Nat) : x ∈ hits ↔ DirectDep g a x := by
  obtain ⟨he, _⟩ := traversal_hits h
  simp only [Answer.traversal.injEq] at he
  rw [he.2, List.take_of_length_le hk, mem_ranked]

/-- No direct dependent: the dispatch refuses. -/
theorem no_dependents_refuses (g : Graph κ) (a k d : Nat) (h : ∀ x, ¬ DirectDep g a x) :
    tryStructuralWith g (some (.one a)) k d = none := by
  have : ranked g a = [] := by
    cases hr : ranked g a with
    | nil => rfl
    | cons y _ =>
      have := (mem_ranked g a y).mp (by rw [hr]; exact List.mem_cons_self)
      exact absurd this (h y)
  simp [tryStructuralWith, this]

/-- At `top_k = 0` a single anchor takes no dependent and declines (#263). -/
theorem zero_topK_refuses (g : Graph κ) (a d : Nat) :
    tryStructuralWith g (some (.one a)) 0 d = none := by
  simp [tryStructuralWith]

/-- An ambiguous subject gets the warning-only answer at every `top_k` (#129). -/
theorem ambiguous_answers (g : Graph κ) (ids : List Nat) (k d : Nat) :
    tryStructuralWith g (some (.ambiguous ids)) k d = some (.ambiguous ids) := rfl

/-! ## The gather-skip predicate -/

/-- **The pre-check is the dispatch.** `fits_structural_with` is true exactly
when `try_structural_with` answers, at every `top_k` (#263). -/
theorem fitsStructuralWith_iff (g : Graph κ) (c : Option Choice) (k d : Nat) :
    fitsStructuralWith g c k = true ↔ (tryStructuralWith g c k d).isSome := by
  match c with
  | none => simp [fitsStructuralWith, anchorDispatches, tryStructuralWith]
  | some (.ambiguous ids) => simp [fitsStructuralWith, anchorDispatches, tryStructuralWith]
  | some (.one a) =>
    by_cases hk : k = 0
    · subst hk; simp [fitsStructuralWith, tryStructuralWith]
    · by_cases hr : ranked g a = []
      · simp [fitsStructuralWith, anchorDispatches, tryStructuralWith, hr]
      · have htk : (ranked g a).take k ≠ [] := by
          intro h0
          rcases List.take_eq_nil_iff.mp h0 with h0 | h0
          · exact hk h0
          · exact hr h0
        simp [fitsStructuralWith, anchorDispatches, tryStructuralWith, hk, hr, htk]

/-- **Skipping implies firing**: when the pre-check passes, the dispatch on
the same graph answers. -/
theorem fits_skip_fires (g : Graph κ) (c : Option Choice) (k d : Nat)
    (h : fitsStructuralWith g c k = true) : ∃ ans, tryStructuralWith g c k d = some ans :=
  Option.isSome_iff_exists.mp ((fitsStructuralWith_iff g c k d).mp h)

/-- `fits_structural` "answers for a positive `top_k`". -/
theorem fitsStructural_iff (g : Graph κ) (resolve : List Char → Option Choice) (q : List Char)
    {k : Nat} (hk : 0 < k) (d : Nat) :
    fitsStructural g resolve q = true ↔ (tryStructural g resolve q k d).isSome := by
  unfold fitsStructural tryStructural
  cases hc : classify q with
  | general => simp
  | structural =>
    simp only [ite_eq_left, beq_self_eq_true, Bool.true_and]
    rw [← fitsStructuralWith_iff g (resolve q) k d]
    have hk' : k ≠ 0 := by omega
    match resolve q with
    | none => simp [fitsStructuralWith]
    | some (.ambiguous _) => simp [fitsStructuralWith]
    | some (.one _) => simp [fitsStructuralWith, hk']

/-! ## The daemon's routing -/

/-- What phase 1 runs on: the store gather's result, or `Phase1Input`'s
default (no vector candidates). -/
inductive Input where
  | gathered
  | empty
  deriving DecidableEq, Repr

/-- What the store gather returns: with no vector leg, `gather_from` returns
the default. -/
def gatherResult (vectorLeg : Bool) : Input := if vectorLeg then .gathered else .empty

/-- The routed recall's outcome: a traversal answer, or the blended pipeline
over a phase-1 input. -/
inductive Outcome where
  | traversal (ans : Answer)
  | blend (input : Input)
  deriving DecidableEq, Repr

/-- The final graph guard: dispatch if structural and it answers, else blend. -/
def finalStep (g : Graph κ) (choice : Option Choice) (structural : Bool) (topK depth : Nat)
    (input : Input) : Outcome :=
  match (if structural then tryStructuralWith g choice topK depth else none) with
  | some ans => .traversal ans
  | none => .blend input

/-- `recall_routed`, from the routing decision to the answer. `resolve` is
the query's anchor resolution on a graph snapshot; `routesStructural` is
`Route::routes_structural`; `vectorLeg` is "a store that searches vectors and
a query embedding". `g₁` is the pre-check's graph, `g₂` the retry guard's
(#263), `g₃` the final guard's. Not modelled: the early `warn_only` refusals
(`validate_vector_candidate_limit`, session mismatch) that return before
routing, and the gather's `Err` arm, which only `Route::ByVector` (never
structural) can take. -/
def recallRouted (resolve : Graph κ → Option Choice) (routesStructural vectorLeg : Bool)
    (q : List Char) (topK depth : Nat) (g₁ g₂ g₃ : Graph κ) : Outcome :=
  let structural := routesStructural && classify q == .structural
  let ready := structural && fitsStructuralWith g₁ (resolve g₁) topK
  if ready && vectorLeg then
    match tryStructuralWith g₂ (resolve g₂) topK depth with
    | some ans => .traversal ans
    | none => finalStep g₃ (resolve g₃) structural topK depth (gatherResult vectorLeg)
  else
    finalStep g₃ (resolve g₃) structural topK depth
      (if ready then .empty else gatherResult vectorLeg)

theorem finalStep_blend {g : Graph κ} {c : Option Choice} {s : Bool} {k d : Nat}
    {i i' : Input} (h : finalStep g c s k d i = .blend i') : i' = i := by
  unfold finalStep at h
  split at h
  · exact absurd h (by simp)
  · exact (Outcome.blend.inj h).symm

/-- **Never degraded by the skip.** Whatever writes land between the three
graph reads, a recall that ends in the blend runs it on exactly what the store
gather returns: the gather is skipped only when it would return the default
anyway, or the recall gathers again before blending. -/
theorem recall_blend_input (resolve : Graph κ → Option Choice) (rs vl : Bool) (q : List Char)
    (k d : Nat) (g₁ g₂ g₃ : Graph κ) (i : Input)
    (h : recallRouted resolve rs vl q k d g₁ g₂ g₃ = .blend i) : i = gatherResult vl := by
  unfold recallRouted at h
  simp only at h
  split at h
  · split at h
    · exact absurd h (by simp)
    · exact finalStep_blend h
  · rename_i hrv
    have hi := finalStep_blend h
    rw [hi]
    split
    · rename_i hr
      cases vl with
      | true => simp [hr] at hrv
      | false => rfl
    · rfl

/-- **One snapshot.** With no concurrent write, the routed recall is the
recall with no pre-check: dispatch if structural and it answers, else the
blend on the gather. -/
theorem recall_one_snapshot (resolve : Graph κ → Option Choice) (rs vl : Bool) (q : List Char)
    (k d : Nat) (g : Graph κ) :
    recallRouted resolve rs vl q k d g g g =
      finalStep g (resolve g) (rs && classify q == .structural) k d (gatherResult vl) := by
  unfold recallRouted
  simp only
  split
  · rename_i hrv
    have hs : (rs && classify q == .structural) = true := by
      simp only [Bool.and_eq_true] at hrv ⊢; exact hrv.1.1
    split
    · rename_i ans hans
      simp [finalStep, hs, hans]
    · rfl
  · rename_i hrv
    split
    · rename_i hr
      cases vl with
      | true => simp [hr] at hrv
      | false => rfl
    · rfl

/-- **The skip fires.** On one snapshot, when the pre-check lets the gather
be skipped, the answer is the traversal. -/
theorem recall_skip_fires (resolve : Graph κ → Option Choice) (rs vl : Bool) (q : List Char)
    (k d : Nat) (g : Graph κ)
    (hready : (rs && classify q == .structural && fitsStructuralWith g (resolve g) k) = true) :
    ∃ ans, recallRouted resolve rs vl q k d g g g = .traversal ans := by
  simp only [Bool.and_eq_true] at hready
  obtain ⟨ans, hans⟩ := fits_skip_fires g (resolve g) k d hready.2
  refine ⟨ans, ?_⟩
  rw [recall_one_snapshot]
  have hs : (rs && classify q == .structural) = true := by
    simp only [Bool.and_eq_true]; exact hready.1
  simp [finalStep, hs, hans]

/-- **Refusal falls through.** A marker-bearing query whose anchor has no
structural dependent goes to the full blend, on the gathered input. -/
theorem marker_no_dependents_blends (resolve : Graph κ → Option Choice) (rs vl : Bool)
    (q : List Char) (k d : Nat) (g : Graph κ) (a : Nat) (hres : resolve g = some (.one a))
    (hnone : ∀ x, ¬ DirectDep g a x) :
    recallRouted resolve rs vl q k d g g g = .blend (gatherResult vl) := by
  rw [recall_one_snapshot, hres]
  simp [finalStep, no_dependents_refuses g a k d hnone]

/-- A general query always blends on the gathered input, at any snapshots,
even when its anchor has dependents. -/
theorem general_blends (resolve : Graph κ → Option Choice) (rs vl : Bool) (q : List Char)
    (k d : Nat) (g₁ g₂ g₃ : Graph κ) (hq : classify q = .general) :
    recallRouted resolve rs vl q k d g₁ g₂ g₃ = .blend (gatherResult vl) := by
  simp [recallRouted, finalStep, hq]

/-! ## The dispatch over any classifier (#299)

`tryStructural`, `fitsStructural` and `recallRouted` with the classifier a
parameter. At `classify` they are the definitions above (`tryStructural_eq`,
`fitsStructural_eq`, `recallRouted_eq`); at `classifyWB isWord` they are the
fixed code. The dispatch theorems hold for every classifier, so they hold for
the fixed one (`traversal_exact_wb`, `fitsStructuralWB_iff`,
`recall_blend_input_wb`, `general_blends_wb`). -/

/-- `try_structural` over the classifier `cls`. -/
def tryStructuralBy (cls : List Char → Kind) (g : Graph κ) (resolve : List Char → Option Choice)
    (q : List Char) (topK traversalDepth : Nat) : Option Answer :=
  if cls q = .structural then tryStructuralWith g (resolve q) topK traversalDepth else none

/-- `fits_structural` over the classifier `cls`. -/
def fitsStructuralBy (cls : List Char → Kind) (g : Graph κ) (resolve : List Char → Option Choice)
    (q : List Char) : Bool :=
  cls q == .structural && anchorDispatches g (resolve q)

/-- `recall_routed` over the classifier `cls`. -/
def recallRoutedBy (cls : List Char → Kind) (resolve : Graph κ → Option Choice)
    (routesStructural vectorLeg : Bool) (q : List Char) (topK depth : Nat) (g₁ g₂ g₃ : Graph κ) :
    Outcome :=
  let structural := routesStructural && cls q == .structural
  let ready := structural && fitsStructuralWith g₁ (resolve g₁) topK
  if ready && vectorLeg then
    match tryStructuralWith g₂ (resolve g₂) topK depth with
    | some ans => .traversal ans
    | none => finalStep g₃ (resolve g₃) structural topK depth (gatherResult vectorLeg)
  else
    finalStep g₃ (resolve g₃) structural topK depth
      (if ready then .empty else gatherResult vectorLeg)

theorem tryStructural_eq (g : Graph κ) (resolve : List Char → Option Choice) (q : List Char)
    (k d : Nat) : tryStructural g resolve q k d = tryStructuralBy classify g resolve q k d := rfl

theorem fitsStructural_eq (g : Graph κ) (resolve : List Char → Option Choice) (q : List Char) :
    fitsStructural g resolve q = fitsStructuralBy classify g resolve q := rfl

theorem recallRouted_eq (resolve : Graph κ → Option Choice) (rs vl : Bool) (q : List Char)
    (k d : Nat) (g₁ g₂ g₃ : Graph κ) :
    recallRouted resolve rs vl q k d g₁ g₂ g₃ = recallRoutedBy classify resolve rs vl q k d g₁ g₂ g₃ :=
  rfl

/-- `traversal_exact` through the classifier: a dispatched answer for a
single anchor, with `top_k` covering every dependent, is exactly its direct
dependents. -/
theorem traversalBy_exact (cls : List Char → Kind) {g : Graph κ}
    {resolve : List Char → Option Choice} {q : List Char} {a a' k d : Nat} {hits : List Nat}
    (hres : resolve q = some (.one a))
    (h : tryStructuralBy cls g resolve q k d = some (.traversal a' hits))
    (hk : (ranked g a).length ≤ k) (x : Nat) : x ∈ hits ↔ DirectDep g a x := by
  unfold tryStructuralBy at h
  split at h
  · rw [hres] at h; exact traversal_exact h hk x
  · exact absurd h (by simp)

/-- `fitsStructural_iff` for any classifier. -/
theorem fitsStructuralBy_iff (cls : List Char → Kind) (g : Graph κ)
    (resolve : List Char → Option Choice) (q : List Char) {k : Nat} (hk : 0 < k) (d : Nat) :
    fitsStructuralBy cls g resolve q = true ↔ (tryStructuralBy cls g resolve q k d).isSome := by
  unfold fitsStructuralBy tryStructuralBy
  cases hc : cls q with
  | general => simp
  | structural =>
    simp only [ite_eq_left, beq_self_eq_true, Bool.true_and]
    rw [← fitsStructuralWith_iff g (resolve q) k d]
    have hk' : k ≠ 0 := by omega
    match resolve q with
    | none => simp [fitsStructuralWith]
    | some (.ambiguous _) => simp [fitsStructuralWith]
    | some (.one _) => simp [fitsStructuralWith, hk']

/-- `recall_blend_input` for any classifier. -/
theorem recallBy_blend_input (cls : List Char → Kind) (resolve : Graph κ → Option Choice)
    (rs vl : Bool) (q : List Char) (k d : Nat) (g₁ g₂ g₃ : Graph κ) (i : Input)
    (h : recallRoutedBy cls resolve rs vl q k d g₁ g₂ g₃ = .blend i) : i = gatherResult vl := by
  unfold recallRoutedBy at h
  simp only at h
  split at h
  · split at h
    · exact absurd h (by simp)
    · exact finalStep_blend h
  · rename_i hrv
    have hi := finalStep_blend h
    rw [hi]
    split
    · rename_i hr
      cases vl with
      | true => simp [hr] at hrv
      | false => rfl
    · rfl

/-- `general_blends` for any classifier. -/
theorem generalBy_blends (cls : List Char → Kind) (resolve : Graph κ → Option Choice)
    (rs vl : Bool) (q : List Char) (k d : Nat) (g₁ g₂ g₃ : Graph κ) (hq : cls q = .general) :
    recallRoutedBy cls resolve rs vl q k d g₁ g₂ g₃ = .blend (gatherResult vl) := by
  simp [recallRoutedBy, finalStep, hq]

/-- **`traversal_exact` for the fixed classifier.** -/
theorem traversal_exact_wb (isWord : Char → Bool) {g : Graph κ}
    {resolve : List Char → Option Choice} {q : List Char} {a a' k d : Nat} {hits : List Nat}
    (hres : resolve q = some (.one a))
    (h : tryStructuralBy (classifyWB isWord) g resolve q k d = some (.traversal a' hits))
    (hk : (ranked g a).length ≤ k) (x : Nat) : x ∈ hits ↔ DirectDep g a x :=
  traversalBy_exact _ hres h hk x

/-- **The pre-check is the dispatch, for the fixed classifier** (with
`fitsStructuralWith_iff`, which no classifier enters). -/
theorem fitsStructuralWB_iff (isWord : Char → Bool) (g : Graph κ)
    (resolve : List Char → Option Choice) (q : List Char) {k : Nat} (hk : 0 < k) (d : Nat) :
    fitsStructuralBy (classifyWB isWord) g resolve q = true ↔
      (tryStructuralBy (classifyWB isWord) g resolve q k d).isSome :=
  fitsStructuralBy_iff _ g resolve q hk d

/-- **Never degraded by the skip, for the fixed classifier.** -/
theorem recall_blend_input_wb (isWord : Char → Bool) (resolve : Graph κ → Option Choice)
    (rs vl : Bool) (q : List Char) (k d : Nat) (g₁ g₂ g₃ : Graph κ) (i : Input)
    (h : recallRoutedBy (classifyWB isWord) resolve rs vl q k d g₁ g₂ g₃ = .blend i) :
    i = gatherResult vl :=
  recallBy_blend_input _ resolve rs vl q k d g₁ g₂ g₃ i h

/-- A query the fixed classifier calls general always blends on the gathered
input, even when its anchor has dependents. -/
theorem general_blends_wb (isWord : Char → Bool) (resolve : Graph κ → Option Choice)
    (rs vl : Bool) (q : List Char) (k d : Nat) (g₁ g₂ g₃ : Graph κ)
    (hq : classifyWB isWord q = .general) :
    recallRoutedBy (classifyWB isWord) resolve rs vl q k d g₁ g₂ g₃ = .blend (gatherResult vl) :=
  generalBy_blends _ resolve rs vl q k d g₁ g₂ g₃ hq

/-! ## Examples: the cloudops exhibit

The test fixture `exhibit()`: `SG-Base-VPC` (10) is the `Hierarchical` parent
of its alias (11), its three rules (12, 13, 14, weight 4.5) and
`RDS-Lambo-Demo-DB` (30, weight 9.5); RDS is the parent of its alias (31,
4.5) and a constraint (32, 4.0); `SG-PublicWeb` (20) owns 21 and 22. Weights
are in tenths. Keys are ranked by the id, which here is the order of the
lowercased contents (`sg-base-vpc = …` < `… egress …` < `… ingress all …` <
`… ingress tcp …`). -/

section Examples

def exEdges : List Edge :=
  [⟨10, 12, .hierarchical, 45⟩, ⟨10, 13, .hierarchical, 45⟩, ⟨10, 14, .hierarchical, 45⟩,
    ⟨10, 11, .hierarchical, 45⟩, ⟨10, 30, .hierarchical, 95⟩, ⟨30, 31, .hierarchical, 45⟩,
    ⟨30, 32, .hierarchical, 40⟩, ⟨20, 22, .hierarchical, 45⟩, ⟨20, 21, .hierarchical, 45⟩]

def exConcepts : List Nat := [10, 11, 12, 13, 14, 20, 21, 22, 30, 31, 32]

def exhibit (canon : List Nat) : Graph Nat where
  isConcept i := exConcepts.contains i
  canonical i := canon.contains i
  key i := some i
  keyCmp := natCmp
  edges := exEdges

/-- Test `dependency_question_returns_structural_dependent_by_traversal`: RDS
first (9.5 beats 4.5), the anchor not echoed; and with `traversal_depth` 2 the
two-hop nodes 31 and 32 are still absent. -/
example : tryStructuralWith (exhibit []) (some (.one 10)) 5 2 =
    some (.traversal 10 [30, 11, 12, 13, 14]) := by decide

/-- `top_k` cuts the ranked list. -/
example : tryStructuralWith (exhibit []) (some (.one 10)) 2 2 =
    some (.traversal 10 [30, 11]) := by decide

/-- A Canonical rule is promoted ahead of the stronger-bound RDS. -/
example : ranked (exhibit [13]) 10 = [13, 30, 11, 12, 14] := by decide

/-- A concept with no dependents refuses, so the recall blends. -/
example : tryStructuralWith (exhibit []) (some (.one 31)) 5 2 = none := by decide

/-- The #263 pre-check agrees with the dispatch at `top_k` 0, 1 and 5. -/
example : fitsStructuralWith (exhibit []) (some (.one 10)) 0 = false ∧
    fitsStructuralWith (exhibit []) (some (.one 10)) 1 = true ∧
    fitsStructuralWith (exhibit []) (some (.ambiguous [10, 20])) 0 = true := by decide

/-- A `Dependency` edge counts its source as the dependent, `Causal` and
`Semantic` never count, and a self-loop never makes a node its own dependent;
of two edges declaring one dependent the stronger sets its weight. -/
def exMixed : Graph Nat where
  isConcept i := i ≤ 9
  canonical _ := false
  key i := some i
  keyCmp := natCmp
  edges := [⟨2, 1, .dependency, 30⟩, ⟨1, 2, .hierarchical, 70⟩, ⟨3, 1, .causal, 99⟩,
    ⟨1, 4, .semantic, 99⟩, ⟨1, 1, .dependency, 99⟩, ⟨5, 1, .dependency, 50⟩,
    ⟨1, 42, .hierarchical, 99⟩]

example : ranked exMixed 1 = [2, 5] ∧ weight exMixed 1 2 = 70 := by decide

/-- A `Causal` edge, whichever way it points, makes no dependent. -/
example : ranked { exhibit [] with edges := [⟨10, 30, .causal, 95⟩, ⟨30, 10, .causal, 95⟩] } 10
    = [] := by decide

/-- `fits_structural`'s doc says "an anchor resolves WITH at least one
structural dependent"; an ambiguous subject (#129) fits with none. -/
example : fitsStructural { exhibit [] with edges := [] } (fun _ => some (.ambiguous [10, 20]))
    "what depends on README.md".toList = true := by decide

/-- The routing on the exhibit: a dependency question dispatches; the same
question with an unresolvable subject, and a general question, blend on the
gathered input. -/
example : recallRouted (fun _ => some (.one 10)) true true "what depends on SG-Base-VPC".toList
    5 2 (exhibit []) (exhibit []) (exhibit []) =
      .traversal (.traversal 10 [30, 11, 12, 13, 14]) := by decide

example : recallRouted (fun _ => none) true true "what depends on nothing-imaginary".toList
    5 2 (exhibit []) (exhibit []) (exhibit []) = .blend .gathered := by decide

example : recallRouted (fun _ => some (.one 10)) true true
    "is the system independent of a single region".toList 5 2
    (exhibit []) (exhibit []) (exhibit []) = .blend .gathered := by decide

/-- #263's race: the pre-check saw dependents, a write removed them before
the retry guard; the recall re-gathers and blends on the gathered input. -/
example : recallRouted (fun _ => some (.one 10)) true true "what depends on SG-Base-VPC".toList
    5 2 (exhibit []) { exhibit [] with edges := [] } { exhibit [] with edges := [] } =
      .blend .gathered := by decide

/-- A misrouted question: "independents of" classifies structural, so with an
anchor that has dependents the recall answers by traversal, not by the blend. -/
example : recallRouted (fun _ => some (.one 10)) true true
    "who are the independents of SG-Base-VPC".toList 5 2
    (exhibit []) (exhibit []) (exhibit []) = .traversal (.traversal 10 [30, 11, 12, 13, 14]) := by
  decide

/-- #299 fixed: the same misrouted questions classify general under the
word-bounded classifier and blend on the gathered input, though the anchor
has dependents; the whole-word dependency question still dispatches. -/
example : recallRoutedBy (classifyWB asciiWord) (fun _ => some (.one 10)) true true
    "who are the independents of SG-Base-VPC".toList 5 2
    (exhibit []) (exhibit []) (exhibit []) = .blend .gathered := by decide

example : recallRoutedBy (classifyWB asciiWord) (fun _ => some (.one 10)) true true
    "the answer depends only on SG-Base-VPC".toList 5 2
    (exhibit []) (exhibit []) (exhibit []) = .blend .gathered := by decide

example : recallRoutedBy (classifyWB asciiWord) (fun _ => some (.one 10)) true true
    "what depends on SG-Base-VPC".toList 5 2 (exhibit []) (exhibit []) (exhibit []) =
      .traversal (.traversal 10 [30, 11, 12, 13, 14]) := by decide

end Examples

end Lambo.Dispatch
