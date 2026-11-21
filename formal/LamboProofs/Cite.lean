/-
# Citations keep the dependency graph acyclic

Model of the write-time cycle check `closing_edge` (`src/graph/action.rs`),
which `pin_scoped` and `write_edges` (`src/graph/cite.rs`) run for every
`depends_on` citation, and `record_action` for its planned `Causal` and
`Dependency` edges.

`closing_edge(graph, planned)` looks at each planned edge `(src, tgt)` and
refuses the write if `src == tgt` or if a search from `tgt` over the existing
`Causal` and `Dependency` edges *plus all the planned edges* reaches `src`. If
no planned edge is refused, the write is committed.

## Model

* `E x y`: an existing `Causal` or `Dependency` edge from `x` to `y`.
* `P x y`: a planned edge of the write being checked.
* `Combined E P`: the graph after the write.
* `Reach R x y`: `y` is reachable from `x` along `R`, in zero or more steps.
* `Acyclic R`: no edge of `R` can be closed into a cycle, that is, no edge
  `x → y` has `y` reaching `x`. Because `Reach` is reflexive this also rules
  out self-loops.

The breadth-first search that *decides* reachability is not modelled; the
theorem takes its answer as the hypothesis `hcheck`. `Expand.lean` proves that
style of search correct for the recall expansion.

## Results

* `batch_acyclic`: if the existing graph is acyclic and no planned edge is
  refused, the graph after the write is acyclic. By induction over writes the
  `Causal`/`Dependency` graph is acyclic at all times, which is what makes
  "dependents" and "blast radius" well-founded notions.
* `single_edge_acyclic`: the one-edge special case.
* `refuses_self_loop`: a planned self-loop always fails the check.
* `no_mutual_dependency`: in an acyclic graph two concepts never depend on
  each other. (`Contradicts` edges are not subject to this check: "mutual
  contradiction is allowed".)
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

/-- An acyclic graph has no self-loop. -/
theorem Acyclic.irrefl {R : ι → ι → Prop} (h : Acyclic R) (x : ι) : ¬ R x x :=
  fun hxx => h x x hxx (Reach.refl x)

/-- **No mutual dependency.** In an acyclic graph, if `x` depends on `y` then
`y` does not depend on `x`. -/
theorem no_mutual_dependency {R : ι → ι → Prop} (h : Acyclic R) {x y : ι} (hxy : R x y) :
    ¬ R y x :=
  fun hyx => h x y hxy (Reach.single hyx)

/-- More generally, nothing a concept (transitively) depends on depends back
on it. -/
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

end Lambo.Cite
