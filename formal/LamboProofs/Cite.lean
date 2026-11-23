/-
# Citations keep the dependency graph acyclic

Model of the write-time cycle checks and of the load-time cycle check.

* `closing_edge` (`src/graph/action.rs`), which `pin_scoped` and
  `write_edges` (`src/graph/cite.rs`) run for every `depends_on` citation,
  and `record_action` for its planned `Causal` and `Dependency` edges. It
  looks at each planned edge `(src, tgt)` and refuses the write if
  `src == tgt` or if a search from `tgt` over the existing `Causal` and
  `Dependency` edges *plus all the planned edges* reaches `src`. If no
  planned edge is refused, the write is committed.
* `reject_closing_edge` (`src/graph/derive/cycles.rs`), which refuses a
  `parent_of` pair `parent -> child` when `child` already reaches `parent`
  over `Hierarchical` edges (unless the edge exists, which reinforces).
* The alias redirect (`src/canon/alias.rs`), which moves a citation onto a
  ruling unless the ruling reaches the citer over `Causal`/`Dependency`
  edges, and retires the alias's edge.
* The load check `Graph::assert_invariants` (`src/graph/graph.rs`,
  `invariants.rs`), which every session load runs.

## The load check (#325)

The two write gates check two separate conditions: `Causal ∪ Dependency` is
acyclic (spec §5.7: "No cycles in `Causal` or `Dependency`, enforced at
write time by BFS"), and `Hierarchical` is acyclic (the hierarchy is a
DAG). Before #325 the load check walked the union of all three kinds
(`LoadOkUnion`), which is stricter than what the gates guarantee: a child
citing its own parent (`P -Hierarchical-> C`, `C -Dependency-> P`) passes
both gates and was then refused at every load. The load check now checks the
same two conditions separately (`LoadOk`). A cycle that mixes the kinds is
allowed: no consumer needs the union acyclic (every walk that crosses kinds
keeps a visited set or is one hop), and a `parent_of` pair says nothing
about what the child may cite.

## Results

* `batch_acyclic`: if the existing graph is acyclic and no planned edge is
  refused, the graph after the write is acyclic. By induction over writes the
  `Causal`/`Dependency` graph is acyclic at all times, which is what makes
  "dependents" and "blast radius" well-founded notions.
* `single_edge_acyclic`: the one-edge special case; `add_single_acyclic`, the
  same when the check walks the graph without the new edge.
* `refuses_self_loop`: a planned self-loop always fails the check.
* `no_mutual_dependency`: in an acyclic graph two concepts never depend on
  each other. (`Contradicts` edges are not subject to this check: "mutual
  contradiction is allowed".)
* `cite_gate_preserves`, `parent_gate_preserves`, `redirect_preserves`,
  `drop_preserves`: each write gate, and removing edges, keeps both
  conditions of `LoadOk`.
* `written_loadOk`: every graph the gated writes can produce passes the new
  load check.
* `loadOk_of_loadOkUnion`: the new check accepts everything the old one did.
* `child_cites_parent_written`, `child_cites_parent_written'`,
  `child_cites_parent_loadOk`: the issue's scenario, in either order, is
  written through the gates and passes the new check.
* `child_cites_parent_union_rejected`, `union_check_rejects_written`: the
  previous version. The old union check refuses that written session.
* `hierarchical_cycle_not_loadOk`, `dependency_cycle_not_loadOk`: a pure
  cycle of either condition still fails the new check.

Boundary: the breadth-first searches are not modelled for the general
theorems, which take their answers as hypotheses (`hcheck`; `Expand.lean`
proves that style of search correct for the recall expansion); the `decide`
scenarios use Boolean searches whose "not reachable" answers are certified by
a closed set (`unreachB_sound`). `Contradicts` edges are outside the check.
-/

namespace Lambo.Cite

variable {ι : Type}

/-- Reflexive-transitive closure of `R`. -/
inductive Reach (R : ι → ι → Prop) : ι → ι → Prop
  | refl (x : ι) : Reach R x x
  | step {x y z : ι} : R x y → Reach R y z → Reach R x z

theorem Reach.trans {R : ι → ι → Prop} {x y z : ι} (h1 : Reach R x y) :
    Reach R y z → Reach R x z := by
  induction h1 with
  | refl x =>
    intro h
    exact h
  | step e _ ih =>
    intro h
    exact Reach.step e (ih h)

theorem Reach.single {R : ι → ι → Prop} {x y : ι} (h : R x y) : Reach R x y :=
  Reach.step h (Reach.refl y)

/-- No edge can be closed into a cycle. -/
def Acyclic (R : ι → ι → Prop) : Prop := ∀ x y, R x y → ¬ Reach R y x

/-- The edge relation after a write: the existing edges and the planned ones. -/
abbrev Combined (E P : ι → ι → Prop) : ι → ι → Prop := fun x y => E x y ∨ P x y

/-- A path in the combined graph either uses only existing edges, or passes
through some planned edge `s → t`. -/
theorem reach_combined_split {E P : ι → ι → Prop} {y x : ι} (h : Reach (Combined E P) y x) :
    Reach E y x ∨ ∃ s t, P s t ∧ Reach (Combined E P) y s ∧ Reach (Combined E P) t x := by
  induction h with
  | refl x => exact Or.inl (Reach.refl x)
  | step e hrest ih =>
    cases e with
    | inl he =>
      cases ih with
      | inl h => exact Or.inl (Reach.step he h)
      | inr h =>
        cases h with
        | intro s hs =>
          cases hs with
          | intro t ht =>
            exact Or.inr ⟨s, t, ht.1, Reach.step (Or.inl he) ht.2.1, ht.2.2⟩
    | inr hp => exact Or.inr ⟨_, _, hp, Reach.refl _, hrest⟩

/-- **A checked write keeps the graph acyclic.** `hcheck` is the statement
that `closing_edge` returned `None`: for no planned edge `s → t` does `t`
reach `s` in the combined graph. -/
theorem batch_acyclic (E P : ι → ι → Prop) (hE : Acyclic E)
    (hcheck : ∀ s t, P s t → ¬ Reach (Combined E P) t s) : Acyclic (Combined E P) := by
  unfold Acyclic at hE ⊢
  intro x y hxy hback
  cases hxy with
  | inr hp => exact hcheck x y hp hback
  | inl he =>
    cases reach_combined_split hback with
    | inl h => exact hE x y he h
    | inr h =>
      cases h with
      | intro s hs =>
        cases hs with
        | intro t ht =>
          exact hcheck s t ht.1 (Reach.trans ht.2.2 (Reach.step (Or.inl he) ht.2.1))

/-- The relation holding exactly the one edge `s → t`. -/
def Single (s t : ι) : ι → ι → Prop := fun x y => x = s ∧ y = t

/-- One citation `s depends_on t`: if `t` does not already reach `s`, adding
the edge creates no cycle. -/
theorem single_edge_acyclic (E : ι → ι → Prop) (hE : Acyclic E) (s t : ι)
    (hcheck : ¬ Reach (Combined E (Single s t)) t s) : Acyclic (Combined E (Single s t)) := by
  apply batch_acyclic E (Single s t) hE
  intro s' t' hp
  have h1 : s' = s := hp.1
  have h2 : t' = t := hp.2
  rw [h1, h2]
  exact hcheck

/-- A planned self-loop can never pass the check (`if src == tgt { return
Some(..) }`): a node trivially reaches itself, so `hcheck` fails for it. -/
theorem refuses_self_loop (E P : ι → ι → Prop) (s : ι) (hp : P s s)
    (hcheck : ∀ s t, P s t → ¬ Reach (Combined E P) t s) : False :=
  hcheck s s hp (Reach.refl s)

/-- On the `Causal`/`Dependency` graph the write gates keep acyclic, no
concept depends on itself: `closing_edge` refuses the edge `s → s` as a
one-edge cycle. -/
theorem Acyclic.irrefl {R : ι → ι → Prop} (h : Acyclic R) (x : ι) : ¬ R x x :=
  fun hxx => h x x hxx (Reach.refl x)

/-- **No mutual dependency.** On the graph the write gates keep acyclic, if
`x` depends on `y` then `y` does not depend on `x`. -/
theorem no_mutual_dependency {R : ι → ι → Prop} (h : Acyclic R) {x y : ι} (hxy : R x y) :
    ¬ R y x :=
  fun hyx => h x y hxy (Reach.single hyx)

/-- On the `Causal`/`Dependency` graph the write gates (`closing_edge`) keep
acyclic, nothing a concept transitively depends on depends back on it. -/
theorem no_dependency_cycle {R : ι → ι → Prop} (h : Acyclic R) {x y z : ι} (hxy : R x y)
    (hyz : Reach R y z) : ¬ R z x :=
  fun hzx => h x y hxy (Reach.trans hyz (Reach.single hzx))

/-! ## The other refusals of `pin_scoped`

These are checks on a single call's references rather than on the graph. -/

/-- `refuse_cited_and_contradicted`: does any `(content, target)` pair the call
contradicts also appear among the pairs it cites? -/
def citedAndContradicted {α : Type} [DecidableEq α] : List α → List α → Bool
  | [], _ => false
  | p :: ps, cites => decide (p ∈ cites) || citedAndContradicted ps cites

/-- If the check passes, no `(content, target)` pair is both cited and
contradicted: "one write cannot both rest on a concept and contradict it". -/
theorem not_cited_and_contradicted {α : Type} [DecidableEq α] (cites : List α) :
    ∀ (contradicts : List α), citedAndContradicted contradicts cites = false →
      ∀ p, p ∈ contradicts → p ∉ cites := by
  intro contradicts
  induction contradicts with
  | nil =>
    intro _ p hp
    simp at hp
  | cons q qs ih =>
    intro h p hc
    simp only [citedAndContradicted, Bool.or_eq_false_iff] at h
    cases List.mem_cons.mp hc with
    | inl e =>
      rw [e]
      exact of_decide_eq_false h.1
    | inr m => exact ih h.2 p m

/-! ## Edge kinds: the write gates and the load check (#325)

Three edge kinds take part in cycle checks. The two write gates each walk
their own kinds, and the load check now checks the same two conditions
separately instead of their union. -/

/-- The edge kinds that cycle checks look at. -/
inductive Kind
  | causal
  | dependency
  | hierarchical
  deriving DecidableEq, Repr

/-- The kinds `closing_edge` walks: `Causal` and `Dependency`. -/
def Kind.cd : Kind → Bool
  | .hierarchical => false
  | _ => true

/-- The kind `reject_closing_edge` walks: `Hierarchical`. -/
def Kind.hier : Kind → Bool
  | .hierarchical => true
  | _ => false

/-- All three kinds: what `cycle_neighbors` walked before #325. -/
def Kind.union : Kind → Bool := fun _ => true

theorem Kind.hier_false_of_cd {k : Kind} (h : Kind.cd k = true) : Kind.hier k = false := by
  cases k <;> simp_all [Kind.cd, Kind.hier]

/-- A typed edge. -/
structure Edge (ι : Type) where
  src : ι
  tgt : ι
  kind : Kind
  deriving DecidableEq, Repr

/-- A graph is its list of typed edges (duplicates change nothing below). -/
abbrev Graph (ι : Type) := List (Edge ι)

/-- The edges of `g` whose kind `ks` selects, as a relation. -/
def rel (g : Graph ι) (ks : Kind → Bool) (x y : ι) : Prop :=
  ∃ e ∈ g, e.src = x ∧ e.tgt = y ∧ ks e.kind = true

/-- A `parent_of` pair's edge, `parent -Hierarchical-> child`. -/
def hEdge (p c : ι) : Edge ι := ⟨p, c, .hierarchical⟩

/-- A citation's edge, `citer -Dependency-> cited`. -/
def dEdge (s t : ι) : Edge ι := ⟨s, t, .dependency⟩

/-- The two-condition load invariant (#325): `Causal ∪ Dependency` is acyclic
and, separately, `Hierarchical` is acyclic. A cycle that mixes the kinds is
allowed. This is what `Graph::assert_invariants` checks after the fix. -/
def LoadOk (g : Graph ι) : Prop := Acyclic (rel g Kind.cd) ∧ Acyclic (rel g Kind.hier)

/-- The load check before #325: the union of the three kinds is acyclic. -/
def LoadOkUnion (g : Graph ι) : Prop := Acyclic (rel g Kind.union)

theorem Reach.mono {R S : ι → ι → Prop} (hsub : ∀ x y, R x y → S x y) {x y : ι}
    (h : Reach R x y) : Reach S x y := by
  induction h with
  | refl x => exact Reach.refl x
  | step e _ ih => exact Reach.step (hsub _ _ e) ih

/-- Acyclicity passes to a smaller relation. -/
theorem acyclic_of_sub {R S : ι → ι → Prop} (hsub : ∀ x y, R x y → S x y) (h : Acyclic S) :
    Acyclic R :=
  fun x y hxy hback => h x y (hsub x y hxy) (Reach.mono hsub hback)

theorem rel_append {g h : Graph ι} {ks : Kind → Bool} {x y : ι} :
    rel (g ++ h) ks x y ↔ rel g ks x y ∨ rel h ks x y := by
  constructor
  · intro ⟨e, he, hs, ht, hk⟩
    cases List.mem_append.mp he with
    | inl hg => exact Or.inl ⟨e, hg, hs, ht, hk⟩
    | inr hh => exact Or.inr ⟨e, hh, hs, ht, hk⟩
  · intro h'
    cases h' with
    | inl hg =>
      obtain ⟨e, he, hs, ht, hk⟩ := hg
      exact ⟨e, List.mem_append.mpr (Or.inl he), hs, ht, hk⟩
    | inr hh =>
      obtain ⟨e, he, hs, ht, hk⟩ := hh
      exact ⟨e, List.mem_append.mpr (Or.inr he), hs, ht, hk⟩

/-- **Agreement.** Every graph the old union check accepted, the new check
accepts: narrowing the check only admits mixed cycles. -/
theorem loadOk_of_loadOkUnion {g : Graph ι} (h : LoadOkUnion g) : LoadOk g := by
  constructor
  · exact acyclic_of_sub (fun x y ⟨e, he, hs, ht, _⟩ => ⟨e, he, hs, ht, rfl⟩) h
  · exact acyclic_of_sub (fun x y ⟨e, he, hs, ht, _⟩ => ⟨e, he, hs, ht, rfl⟩) h

/-- A path in `E` plus one edge `s → t` either avoids the edge, or reaches
`s` in `E` and continues from `t` in `E`. -/
theorem reach_add_single {E : ι → ι → Prop} {s t x y : ι}
    (h : Reach (Combined E (Single s t)) x y) :
    Reach E x y ∨ (Reach E x s ∧ Reach E t y) := by
  induction h with
  | refl x => exact Or.inl (Reach.refl x)
  | @step x z y e _ ih =>
    cases e with
    | inl he =>
      cases ih with
      | inl h => exact Or.inl (Reach.step he h)
      | inr h => exact Or.inr ⟨Reach.step he h.1, h.2⟩
    | inr hp =>
      have hx : x = s := hp.1
      have hz : z = t := hp.2
      subst hx
      subst hz
      cases ih with
      | inl h => exact Or.inr ⟨Reach.refl x, h⟩
      | inr h => exact Or.inr ⟨Reach.refl x, h.2⟩

/-- Adding one edge `s → t` keeps a relation acyclic when `t` does not reach
`s` in the graph *without* the edge, which is what `reject_closing_edge`
(`hierarchical_path` on the live graph) and the alias redirect check. -/
theorem add_single_acyclic (E : ι → ι → Prop) (hE : Acyclic E) (s t : ι)
    (hcheck : ¬ Reach E t s) : Acyclic (Combined E (Single s t)) := by
  apply single_edge_acyclic E hE s t
  intro h
  cases reach_add_single h with
  | inl h => exact hcheck h
  | inr h => exact hcheck h.1

/-! ### The write gates preserve the two-condition invariant -/

/-- **The citation gate.** `closing_edge` (for `record_action`'s planned
`Causal`/`Dependency` edges and for `depends_on` citations) plans only
`Causal`/`Dependency` edges and passes when no planned edge `s → t` has `t`
reaching `s` over the existing and planned `Causal`/`Dependency` edges. Such
a write keeps both conditions. -/
theorem cite_gate_preserves (g planned : Graph ι) (hg : LoadOk g)
    (hkind : ∀ e ∈ planned, Kind.cd e.kind = true)
    (hcheck : ∀ e ∈ planned, ¬ Reach (rel (g ++ planned) Kind.cd) e.tgt e.src) :
    LoadOk (g ++ planned) := by
  constructor
  · have hc : Acyclic (Combined (rel g Kind.cd) (rel planned Kind.cd)) := by
      apply batch_acyclic _ _ hg.1
      intro s t hp hback
      obtain ⟨e, he, hs, ht, _⟩ := hp
      apply hcheck e he
      rw [hs, ht]
      exact Reach.mono (fun _ _ h => rel_append.mpr h) hback
    exact acyclic_of_sub (fun _ _ h => rel_append.mp h) hc
  · apply acyclic_of_sub _ hg.2
    intro x y h
    cases rel_append.mp h with
    | inl h => exact h
    | inr h =>
      obtain ⟨e, he, _, _, hk⟩ := h
      have := Kind.hier_false_of_cd (hkind e he)
      rw [this] at hk
      exact absurd hk Bool.false_ne_true

/-- **The `parent_of` gate.** `reject_closing_edge` passes `parent → child`
when the edge already exists (a reinforcement) or `child` does not reach
`parent` over the live `Hierarchical` edges. Such a write keeps both
conditions: it adds no `Causal`/`Dependency` edge. -/
theorem parent_gate_preserves (g : Graph ι) (p c : ι) (hg : LoadOk g)
    (hcheck : hEdge p c ∈ g ∨ ¬ Reach (rel g Kind.hier) c p) :
    LoadOk (g ++ [hEdge p c]) := by
  constructor
  · apply acyclic_of_sub _ hg.1
    intro x y h
    cases rel_append.mp h with
    | inl h => exact h
    | inr h =>
      obtain ⟨e, he, _, _, hk⟩ := h
      rw [List.mem_singleton.mp he] at hk
      exact absurd hk (by simp [hEdge, Kind.cd])
  · cases hcheck with
    | inl hin =>
      apply acyclic_of_sub _ hg.2
      intro x y h
      cases rel_append.mp h with
      | inl h => exact h
      | inr h =>
        obtain ⟨e, he, hs, ht, hk⟩ := h
        exact ⟨e, by rw [List.mem_singleton.mp he]; exact hin, hs, ht, hk⟩
    | inr hnot =>
      apply acyclic_of_sub _ (add_single_acyclic _ hg.2 p c hnot)
      intro x y h
      cases rel_append.mp h with
      | inl h => exact Or.inl h
      | inr h =>
        obtain ⟨e, he, hs, ht, _⟩ := h
        rw [List.mem_singleton.mp he] at hs ht
        exact Or.inr ⟨hs.symm, ht.symm⟩

/-- **The alias redirect** (`canon/alias.rs`) adds one citation
`citer -Dependency-> ruling` when `ruling` does not reach `citer` over the
`Causal`/`Dependency` edges, as the plan has changed them. It adds no
`Hierarchical` edge, so that is the only check it needs. -/
theorem redirect_preserves (g : Graph ι) (citer ruling : ι) (hg : LoadOk g)
    (hcheck : ¬ Reach (rel g Kind.cd) ruling citer) :
    LoadOk (g ++ [dEdge citer ruling]) := by
  constructor
  · apply acyclic_of_sub _ (add_single_acyclic _ hg.1 citer ruling hcheck)
    intro x y h
    cases rel_append.mp h with
    | inl h => exact Or.inl h
    | inr h =>
      obtain ⟨e, he, hs, ht, _⟩ := h
      rw [List.mem_singleton.mp he] at hs ht
      exact Or.inr ⟨hs.symm, ht.symm⟩
  · apply acyclic_of_sub _ hg.2
    intro x y h
    cases rel_append.mp h with
    | inl h => exact h
    | inr h =>
      obtain ⟨e, he, _, _, hk⟩ := h
      rw [List.mem_singleton.mp he] at hk
      exact absurd hk (by simp [dEdge, Kind.hier])

/-- Removing edges (an alias citation retired, GC deleting a node and its
edges) keeps both conditions. -/
theorem drop_preserves {g g' : Graph ι} (hg : LoadOk g) (hsub : ∀ e ∈ g', e ∈ g) :
    LoadOk g' :=
  ⟨acyclic_of_sub (fun _ _ ⟨e, he, h⟩ => ⟨e, hsub e he, h⟩) hg.1,
   acyclic_of_sub (fun _ _ ⟨e, he, h⟩ => ⟨e, hsub e he, h⟩) hg.2⟩

/-! ### Executable checks

The searches the Rust runs, as Boolean functions, so the scenario below can
be checked by `decide`. "Not reachable" is certified by a set that contains
the start, is closed under the walked edges and misses the goal, so its
soundness needs no bound on the search. -/

section Exec

variable [DecidableEq ι]

/-- `x`'s successors over the kinds `ks` selects. -/
def post (g : Graph ι) (ks : Kind → Bool) (x : ι) : List ι :=
  (g.filter fun e => decide (e.src = x) && ks e.kind).map Edge.tgt

theorem mem_post {g : Graph ι} {ks : Kind → Bool} {x y : ι} (h : rel g ks x y) :
    y ∈ post g ks x := by
  obtain ⟨e, he, hs, ht, hk⟩ := h
  unfold post
  exact List.mem_map.mpr ⟨e, List.mem_filter.mpr ⟨he, by simp [hs, hk]⟩, ht⟩

/-- One search level: `S` and every successor of a member. -/
def grow (g : Graph ι) (ks : Kind → Bool) (S : List ι) : List ι :=
  S ++ S.flatMap (post g ks)

/-- `n` search levels from `S`. -/
def closure (g : Graph ι) (ks : Kind → Bool) : Nat → List ι → List ι
  | 0, S => S
  | n + 1, S => closure g ks n (grow g ks S)

/-- Every successor of a member of `S` is in `S`. -/
def closedB (g : Graph ι) (ks : Kind → Bool) (S : List ι) : Bool :=
  S.all fun x => (post g ks x).all fun y => decide (y ∈ S)

/-- `y` is certainly not reachable from `x` over `ks`: the search from `x`
closed without meeting `y`. -/
def unreachB (g : Graph ι) (ks : Kind → Bool) (x y : ι) : Bool :=
  closedB g ks (closure g ks g.length [x]) && !decide (y ∈ closure g ks g.length [x])

/-- Every graph edge of a selected kind passes `unreachB` backwards: the
Boolean acyclicity check. -/
def acyclicB (g : Graph ι) (ks : Kind → Bool) : Bool :=
  g.all fun e => !ks e.kind || unreachB g ks e.tgt e.src

/-- The load check after #325: both conditions, separately. -/
def loadOkB (g : Graph ι) : Bool := acyclicB g Kind.cd && acyclicB g Kind.hier

/-- The load check before #325: the union. -/
def loadOkUnionB (g : Graph ι) : Bool := acyclicB g Kind.union

/-- `closing_edge` returned `None`: every planned edge is `Causal` or
`Dependency`, and none can be closed over the existing and planned ones. -/
def citeGateB (g planned : Graph ι) : Bool :=
  planned.all fun e => Kind.cd e.kind && unreachB (g ++ planned) Kind.cd e.tgt e.src

/-- `reject_closing_edge` returned `Ok`: the edge exists, or the child does
not reach the parent over `Hierarchical` edges. -/
def parentGateB (g : Graph ι) (p c : ι) : Bool :=
  decide (hEdge p c ∈ g) || unreachB g Kind.hier c p

/-- The alias redirect's check: the ruling does not reach the citer over
`Causal`/`Dependency` edges. -/
def redirectGateB (g : Graph ι) (citer ruling : ι) : Bool :=
  unreachB g Kind.cd ruling citer

theorem mem_closure_of_mem {g : Graph ι} {ks : Kind → Bool} {x : ι} :
    ∀ (n : Nat) (S : List ι), x ∈ S → x ∈ closure g ks n S := by
  intro n
  induction n with
  | zero => intro S h; exact h
  | succ n ih =>
    intro S h
    exact ih (grow g ks S) (List.mem_append.mpr (Or.inl h))

theorem reach_stays_in_closed {g : Graph ι} {ks : Kind → Bool} {S : List ι}
    (hS : closedB g ks S = true) {x y : ι} (h : Reach (rel g ks) x y) : x ∈ S → y ∈ S := by
  induction h with
  | refl x => exact id
  | step e _ ih =>
    intro hx
    apply ih
    have h1 := List.all_eq_true.mp hS _ hx
    have h2 := List.all_eq_true.mp h1 _ (mem_post e)
    exact of_decide_eq_true h2

theorem unreachB_sound {g : Graph ι} {ks : Kind → Bool} {x y : ι}
    (h : unreachB g ks x y = true) : ¬ Reach (rel g ks) x y := by
  intro hr
  simp only [unreachB, Bool.and_eq_true, Bool.not_eq_true', decide_eq_false_iff_not] at h
  exact h.2 (reach_stays_in_closed h.1 hr (mem_closure_of_mem _ _ (List.mem_singleton.mpr rfl)))

theorem acyclicB_sound {g : Graph ι} {ks : Kind → Bool} (h : acyclicB g ks = true) :
    Acyclic (rel g ks) := by
  intro x y hxy
  obtain ⟨e, he, hs, ht, hk⟩ := hxy
  have h1 := List.all_eq_true.mp h e he
  rw [hk] at h1
  simp only [Bool.not_true, Bool.false_or] at h1
  rw [← hs, ← ht]
  exact unreachB_sound h1

/-- The load check (`Graph::assert_invariants`' two cycle passes) is sound: a
graph it accepts has no `Causal`/`Dependency` cycle and no `Hierarchical`
cycle. -/
theorem loadOkB_sound {g : Graph ι} (h : loadOkB g = true) : LoadOk g := by
  simp only [loadOkB, Bool.and_eq_true] at h
  exact ⟨acyclicB_sound h.1, acyclicB_sound h.2⟩

theorem citeGateB_sound {g planned : Graph ι} (h : citeGateB g planned = true) :
    (∀ e ∈ planned, Kind.cd e.kind = true) ∧
      ∀ e ∈ planned, ¬ Reach (rel (g ++ planned) Kind.cd) e.tgt e.src := by
  constructor
  · intro e he
    have := List.all_eq_true.mp h e he
    simp only [Bool.and_eq_true] at this
    exact this.1
  · intro e he
    have := List.all_eq_true.mp h e he
    simp only [Bool.and_eq_true] at this
    exact unreachB_sound this.2

theorem parentGateB_sound {g : Graph ι} {p c : ι} (h : parentGateB g p c = true) :
    hEdge p c ∈ g ∨ ¬ Reach (rel g Kind.hier) c p := by
  simp only [parentGateB, Bool.or_eq_true, decide_eq_true_eq] at h
  cases h with
  | inl h => exact Or.inl h
  | inr h => exact Or.inr (unreachB_sound h)

/-- The graphs the write path can produce: start empty, then any sequence of
gated citation writes, gated `parent_of` edges, gated alias redirects and
edge removals. -/
inductive Written : Graph ι → Prop
  | empty : Written []
  | cite {g planned : Graph ι} : Written g → citeGateB g planned = true → Written (g ++ planned)
  | parent {g : Graph ι} {p c : ι} : Written g → parentGateB g p c = true →
      Written (g ++ [hEdge p c])
  | redirect {g : Graph ι} {citer ruling : ι} : Written g → redirectGateB g citer ruling = true →
      Written (g ++ [dEdge citer ruling])
  | drop {g g' : Graph ι} : Written g → (∀ e ∈ g', e ∈ g) → Written g'

/-- **Every written session passes the new load check.** By induction over
the writes, with the gates' preservation theorems. -/
theorem written_loadOk {g : Graph ι} (h : Written g) : LoadOk g := by
  induction h with
  | empty =>
    constructor <;> intro x y hxy <;> obtain ⟨e, he, _⟩ := hxy <;> simp at he
  | cite _ hgate ih =>
    have hs := citeGateB_sound hgate
    exact cite_gate_preserves _ _ ih hs.1 hs.2
  | parent _ hgate ih => exact parent_gate_preserves _ _ _ ih (parentGateB_sound hgate)
  | redirect _ hgate ih => exact redirect_preserves _ _ _ ih (unreachB_sound hgate)
  | drop _ hsub ih => exact drop_preserves ih hsub

end Exec

/-! ### The issue's scenario: a child citing its own parent

`P = 0`, `C = 1`. `C` is derived with `depends_on: [P]` and
`parent_of: [(P, C)]`, which writes `P -Hierarchical-> C` and
`C -Dependency-> P`. -/

/-- The session after the write. -/
def childCitesParent : Graph Nat := [hEdge 0 1, dEdge 1 0]

/-- The same edges written the other way round (the citation first, the
`parent_of` pair in a later call). -/
def childCitesParent' : Graph Nat := [dEdge 1 0, hEdge 0 1]

/-- Both gates accept both orders, the new load check accepts both results,
and the old union check refuses them. -/
example :
    parentGateB ([] : Graph Nat) 0 1 = true ∧
    citeGateB [hEdge 0 1] [dEdge 1 0] = true ∧
    citeGateB ([] : Graph Nat) [dEdge 1 0] = true ∧
    parentGateB [dEdge 1 0] 0 1 = true ∧
    loadOkB childCitesParent = true ∧ loadOkB childCitesParent' = true ∧
    loadOkUnionB childCitesParent = false ∧ loadOkUnionB childCitesParent' = false := by
  decide

/-- The new check still refuses a pure cycle of either condition. -/
example :
    loadOkB ([hEdge 0 1, hEdge 1 0] : Graph Nat) = false ∧
    loadOkB ([dEdge 0 1, dEdge 1 0] : Graph Nat) = false ∧
    loadOkB ([dEdge 0 1, ⟨1, 0, .causal⟩] : Graph Nat) = false ∧
    loadOkB ([hEdge 0 0] : Graph Nat) = false := by
  decide

/-- The scenario is reachable through the gates, in one call (the
`parent_of` edge, then the citation). -/
theorem child_cites_parent_written : Written childCitesParent :=
  Written.cite (planned := [dEdge 1 0])
    (Written.parent (p := 0) (c := 1) Written.empty (by decide)) (by decide)

/-- And in two calls the other way round. -/
theorem child_cites_parent_written' : Written childCitesParent' :=
  Written.parent (p := 0) (c := 1)
    (Written.cite (planned := [dEdge 1 0]) Written.empty (by decide)) (by decide)

/-- The written session passes the new load check. -/
theorem child_cites_parent_loadOk : LoadOk childCitesParent :=
  written_loadOk child_cites_parent_written

/-- **The bug, before #325.** The old union check refuses the session the
gates wrote: `0 -Hierarchical-> 1 -Dependency-> 0` is a union cycle. -/
theorem child_cites_parent_union_rejected : ¬ LoadOkUnion childCitesParent := by
  intro h
  apply h 0 1 ⟨hEdge 0 1, by simp [childCitesParent], rfl, rfl, rfl⟩
  exact Reach.single ⟨dEdge 1 0, by simp [childCitesParent], rfl, rfl, rfl⟩

/-- So the old pair of write gates and the old load check disagreed: a
session the gates accept could not be loaded. -/
theorem union_check_rejects_written : ∃ g : Graph Nat, Written g ∧ ¬ LoadOkUnion g :=
  ⟨childCitesParent, child_cites_parent_written, child_cites_parent_union_rejected⟩

/-- A pure `Hierarchical` 2-cycle still fails the new check. -/
theorem hierarchical_cycle_not_loadOk : ¬ LoadOk ([hEdge 0 1, hEdge 1 0] : Graph Nat) := by
  intro h
  apply h.2 0 1 ⟨hEdge 0 1, by simp, rfl, rfl, rfl⟩
  exact Reach.single ⟨hEdge 1 0, by simp, rfl, rfl, rfl⟩

/-- A pure `Dependency` 2-cycle still fails the new check. -/
theorem dependency_cycle_not_loadOk : ¬ LoadOk ([dEdge 0 1, dEdge 1 0] : Graph Nat) := by
  intro h
  apply h.1 0 1 ⟨dEdge 0 1, by simp, rfl, rfl, rfl⟩
  exact Reach.single ⟨dEdge 1 0, by simp, rfl, rfl, rfl⟩

end Lambo.Cite
