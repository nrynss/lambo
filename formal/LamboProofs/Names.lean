/-
# Durable names are permanent

Model of the name registry (`src/graph/graph/names.rs`) and of how a reference
resolves through it (`resolve_reference_scoped`, `src/surface/resolve.rs`).

"Names reserve a spelling for an immutable id even after its target is
collected."

## Model

* `κ` is the type of normalized name keys (`name_key`), `ι` of node ids.
* `Registry.names` is `Graph::names`; `Registry.live i` says `i` is a concept
  currently in the graph; `Registry.keyOf i` is the concept's own canonical
  key, which never changes.
* `declare k t` is `declare_name` after normalization: the target must be a
  live concept, the key must be unbound or already bound to this target, and
  no *other* live concept may have `k` as its canonical key.
* `setLive` stands for every change to which concepts exist: creation, and
  removal by garbage collection or alias upgrade. `Graph::remove_node` does not
  touch `names`, so this step leaves them alone.

Not modelled: transaction rollback (which restores the registry to its state
at the start of the transaction, and is the only thing that removes an entry),
retired aliases, and the id-shaped spellings `name_key` refuses.

## Results

* `names_permanent`: once a key is bound to an id, it is bound to that id in
  every later state. A name is never re-targeted and never freed, even after
  its target is collected.
* `resolve_never_rebinds`: from then on the name resolves to that same concept
  or is refused as retired; it never resolves to a different concept and never
  reads as unbound ("Never fall through and bind that spelling to a new
  concept").
* `declare_idempotent`: "an identical repeated binding is a no-op".
* `declare_no_shadow`: a name cannot be given to one concept while it is the
  canonical key of another.
-/

namespace Lambo.Names

/-- The part of the graph the name registry depends on. -/
structure Registry (κ ι : Type) where
  names : κ → Option ι
  live : ι → Bool
  keyOf : ι → κ

variable {κ ι : Type} [DecidableEq κ]

/-- Bind `k` to `t`. -/
def bind (s : Registry κ ι) (k : κ) (t : ι) : Registry κ ι :=
  { names := fun k' => if k' = k then some t else s.names k', live := s.live, keyOf := s.keyOf }

/-- One change to the graph. -/
inductive Step : Registry κ ι → Registry κ ι → Prop
  | declare (s : Registry κ ι) (k : κ) (t : ι) :
      s.live t = true →
      (s.names k = none ∨ s.names k = some t) →
      (∀ c, s.live c = true → s.keyOf c = k → c = t) →
      Step s (bind s k t)
  | setLive (s : Registry κ ι) (live' : ι → Bool) :
      Step s { names := s.names, live := live', keyOf := s.keyOf }

/-- Any finite history. -/
inductive Steps : Registry κ ι → Registry κ ι → Prop
  | refl (s : Registry κ ι) : Steps s s
  | tail {a b c : Registry κ ι} : Steps a b → Step b c → Steps a c

theorem bind_same (s : Registry κ ι) (k : κ) (t : ι) : (bind s k t).names k = some t := by
  show (if k = k then some t else s.names k) = some t
  exact ite_eq_left rfl

theorem bind_other (s : Registry κ ι) (k k' : κ) (t : ι) (h : k' ≠ k) :
    (bind s k t).names k' = s.names k' := by
  show (if k' = k then some t else s.names k') = s.names k'
  exact ite_eq_right h

/-- One step never changes an existing binding. -/
theorem step_keeps_binding {a b : Registry κ ι} (h : Step a b) (k : κ) (i : ι)
    (hk : a.names k = some i) : b.names k = some i := by
  cases h with
  | declare k' t _ hfree _ =>
    by_cases hkk : k = k'
    · rw [hkk] at hk ⊢
      rw [bind_same]
      cases hfree with
      | inl hnone =>
        rw [hnone] at hk
        cases hk
      | inr hsome =>
        rw [hsome] at hk
        exact hk
    · rw [bind_other _ k' k t hkk]
      exact hk
  | setLive live' => exact hk

/-- **Permanence.** A binding, once made, holds in every later state: garbage
collecting the target, declaring other names and re-declaring this one all
leave it in place. -/
theorem names_permanent {a b : Registry κ ι} (h : Steps a b) (k : κ) (i : ι)
    (hk : a.names k = some i) : b.names k = some i := by
  induction h with
  | refl => exact hk
  | tail _ hstep ih => exact step_keeps_binding hstep k i ih

/-- How a reference that is a registered name resolves. -/
inductive Resolution (ι : Type) where
  | unbound
  | concept (id : ι)
  | retired

/-- The name leg of `resolve_reference_scoped`: a registered name resolves to
its target if that is still a live concept, and is refused as `Retired`
otherwise. -/
def resolve (s : Registry κ ι) (k : κ) : Resolution ι :=
  match s.names k with
  | none => .unbound
  | some i => if s.live i = true then .concept i else .retired

omit [DecidableEq κ] in
/-- While its target is live, the name resolves to it. -/
theorem resolve_live (s : Registry κ ι) (k : κ) (i : ι) (hk : s.names k = some i)
    (hlive : s.live i = true) : resolve s k = .concept i := by
  first
    | (unfold resolve; rw [hk]; exact ite_eq_left hlive)
    | simp [resolve, hk, hlive]

omit [DecidableEq κ] in
/-- A name whose target was collected fails closed (Rust test
`deleted_named_targets_remain_reserved_and_fail_closed`). -/
theorem resolve_collected (s : Registry κ ι) (k : κ) (i : ι) (hk : s.names k = some i)
    (hdead : ¬ s.live i = true) : resolve s k = .retired := by
  first
    | (unfold resolve; rw [hk]; exact ite_eq_right hdead)
    | simp [resolve, hk, hdead]

/-- **A name never re-binds.** After a key has been bound to `i`, in every
later state it resolves to `i` or is refused as retired. In particular it
never names a different concept and never falls through to "unbound", which
is the case that would let `record_action` mint a new concept under a
collected concept's name. -/
theorem resolve_never_rebinds {a b : Registry κ ι} (h : Steps a b) (k : κ) (i : ι)
    (hk : a.names k = some i) : resolve b k = .concept i ∨ resolve b k = .retired := by
  have hb := names_permanent h k i hk
  by_cases hlive : b.live i = true
  · exact Or.inl (resolve_live b k i hb hlive)
  · exact Or.inr (resolve_collected b k i hb hlive)

/-- Re-declaring an existing name binding (`src/graph/graph/names.rs`) changes
no name. -/
theorem declare_idempotent (s : Registry κ ι) (k : κ) (t : ι) (h : s.names k = some t) (k' : κ) :
    (bind s k t).names k' = s.names k' := by
  by_cases hkk : k' = k
  · rw [hkk, bind_same, h]
  · exact bind_other s k k' t hkk

/-- A name cannot shadow another concept: right after a declaration, every
live concept whose own canonical key is the declared key is the name's
target. So a name and a canonical key never point a reference at two
different live concepts (Rust test
`names_cannot_shadow_concepts_observations_or_identifiers`). -/
theorem declare_no_shadow (s : Registry κ ι) (k : κ) (t : ι)
    (hshadow : ∀ c, s.live c = true → s.keyOf c = k → c = t)
    (c : ι) (hlive : (bind s k t).live c = true) (hkey : (bind s k t).keyOf c = k) :
    (bind s k t).names k = some c := by
  have hc : c = t := hshadow c hlive hkey
  rw [hc]
  exact bind_same s k t

end Lambo.Names
