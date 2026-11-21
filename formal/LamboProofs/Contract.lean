/-
# The embedding contract: one vector space per session

Model of `EmbeddingContract::ensure_compatible` (`src/types/mod.rs`),
`Graph::stamp_embedding` (`src/graph/graph/embeddings.rs`),
`session_embedding_compatibility` and `check_vector_compatibility`
(`src/resolve.rs`).

"One session never mixes two model spaces."

## Model

A contract is `(kind, model, dim)`. `stamp` is `stamp_embedding`: an unstamped
session takes the offered contract; a stamped one accepts only an equal
contract and is left unchanged; anything else is an error and the session is
left unchanged.

Not modelled: the operator override (`allow_embedding_mismatch`), which
re-labels a session under the writer lease and is refused across widths;
re-embed mode.

## Results

* `compatible_iff_eq`: compatibility is equality of all three fields; a
  missing model name is not a wildcard, and "same dim does not mean
  interchangeable models".
* `stamp_keeps`, `stampAll_keeps`: once stamped, no sequence of ordinary
  stamps changes or clears the contract ("ordinary callers cannot clear or
  replace the contract").
* `stamp_ok_eq`: every stamp that succeeds on a stamped session offered that
  very contract, so all vectors accepted into a session share one space.
* `classify_*`: the attach-time classification; only an unstamped (legacy)
  session is a wildcard.
* `vector_check_*`: the resolve-time width check is store-authoritative.
-/

namespace Lambo.Contract

/-- `EmbeddingContract`. `K` is the provider kind, `M` the model identity. -/
structure Contract (K M : Type) where
  kind : K
  model : Option M
  dim : Nat
  deriving DecidableEq

variable {K M : Type} [DecidableEq K] [DecidableEq M]

/-- `ensure_compatible`: errors iff `dim`, `kind` or `model` differs. -/
def compatible (a b : Contract K M) : Bool :=
  decide (a.dim = b.dim) && decide (a.kind = b.kind) && decide (a.model = b.model)

/-- Compatibility is plain equality. -/
theorem compatible_iff_eq (a b : Contract K M) : compatible a b = true ↔ a = b := by
  cases a with
  | mk ak am ad =>
    cases b with
    | mk bk bm bd =>
      by_cases hd : ad = bd <;> by_cases hk : ak = bk <;> by_cases hm : am = bm <;>
        simp [compatible, hd, hk, hm]

/-- Same width, different model: incompatible. -/
theorem same_dim_not_enough (a b : Contract K M) (hm : a.model ≠ b.model) :
    compatible a b = false := by
  cases hc : compatible a b with
  | false => rfl
  | true =>
    have e : a = b := (compatible_iff_eq a b).mp hc
    rw [e] at hm
    exact absurd rfl hm

/-- `stamp_embedding`: the session's contract afterwards, and whether the
stamp was accepted. -/
def stamp (stored : Option (Contract K M)) (offered : Contract K M) :
    Option (Contract K M) × Bool :=
  match stored with
  | none => (some offered, true)
  | some c => if c = offered then (some c, true) else (some c, false)

/-- **A stamped contract stays.** Whatever is offered, a stamped session keeps
exactly the contract it had. -/
theorem stamp_keeps (c offered : Contract K M) : (stamp (some c) offered).1 = some c := by
  show (if c = offered then (some c, true) else (some c, false)).1 = some c
  by_cases h : c = offered
  · rw [if_pos h] <;> rfl
  · rw [if_neg h] <;> rfl

/-- A stamp on a stamped session succeeds only for the stored contract. -/
theorem stamp_ok_eq (c offered : Contract K M) (h : (stamp (some c) offered).2 = true) :
    offered = c := by
  by_cases he : c = offered
  · exact he.symm
  · have e : stamp (some c) offered = (some c, false) := by
      show (if c = offered then (some c, true) else (some c, false)) = (some c, false)
      rw [if_neg he]
    rw [e] at h
    have h' : false = true := h
    exact absurd h' (by decide)

/-- The first stamp on an unstamped session records the offered contract. -/
theorem stamp_first (offered : Contract K M) : stamp none offered = (some offered, true) := rfl

/-- A sequence of stamp attempts, keeping only the stored contract. -/
def stampAll (stored : Option (Contract K M)) : List (Contract K M) → Option (Contract K M)
  | [] => stored
  | offered :: rest => stampAll (stamp stored offered).1 rest

/-- **One space per session.** After the session is stamped, no sequence of
stamp attempts changes or clears its contract. -/
theorem stampAll_keeps (c : Contract K M) :
    ∀ (attempts : List (Contract K M)), stampAll (some c) attempts = some c := by
  intro attempts
  induction attempts with
  | nil => rfl
  | cons offered rest ih =>
    show stampAll (stamp (some c) offered).1 rest = some c
    rw [stamp_keeps c offered]
    exact ih

/-- Whoever stamps first decides: "only the winner may stamp its vector
space". -/
theorem first_stamp_wins (first : Contract K M) (later : List (Contract K M)) :
    stampAll none (first :: later) = some first := by
  show stampAll (stamp none first).1 later = some first
  exact stampAll_keeps first later

/-! ## Attach-time classification -/

/-- `SessionEmbeddingCompatibility`. -/
inductive Compat where
  | unrecorded
  | compatible
  | mismatch
  deriving DecidableEq, Repr

/-- `session_embedding_compatibility`. -/
def classify (stored : Option (Contract K M)) (live : Contract K M) : Compat :=
  match stored with
  | none => .unrecorded
  | some s => if s = live then .compatible else .mismatch

/-- The only wildcard is a session with no recorded contract. -/
theorem classify_unrecorded_iff (stored : Option (Contract K M)) (live : Contract K M) :
    classify stored live = .unrecorded ↔ stored = none := by
  cases stored with
  | none => exact ⟨fun _ => rfl, fun _ => rfl⟩
  | some s =>
    constructor
    · intro h
      have h' : (if s = live then Compat.compatible else Compat.mismatch) = .unrecorded := h
      by_cases hs : s = live
      · rw [if_pos hs] at h'
        exact Compat.noConfusion h'
      · rw [if_neg hs] at h'
        exact Compat.noConfusion h'
    · intro h
      exact Option.noConfusion h

/-- A recorded contract is `Compatible` exactly when it equals the live one. -/
theorem classify_compatible_iff (s live : Contract K M) :
    classify (some s) live = .compatible ↔ s = live := by
  constructor
  · intro h
    have h' : (if s = live then Compat.compatible else Compat.mismatch) = .compatible := h
    by_cases hs : s = live
    · exact hs
    · rw [if_neg hs] at h'
      exact Compat.noConfusion h'
  · intro hs
    show (if s = live then Compat.compatible else Compat.mismatch) = .compatible
    rw [if_pos hs]

/-! ## Store-authoritative vector width -/

/-- `check_vector_compatibility`: a zero-width embedder is refused; a store
that declares a width must match the embedder; a store that declares none
passes. -/
def vectorCheck (storeDim : Option Nat) (embedderDim : Nat) : Bool :=
  match storeDim with
  | none => decide (embedderDim ≠ 0)
  | some d => decide (embedderDim ≠ 0) && decide (d = embedderDim)

/-- When the store declares a width, a passing check means the embedder has
exactly that width: the store's `VECTOR(n)` column is the authority. -/
theorem vector_check_store_authoritative (d embedderDim : Nat)
    (h : vectorCheck (some d) embedderDim = true) : embedderDim = d ∧ embedderDim ≠ 0 := by
  simp [vectorCheck] at h
  omega

/-- A zero-width embedder never passes. -/
theorem vector_check_zero (storeDim : Option Nat) : vectorCheck storeDim 0 = false := by
  cases storeDim <;> simp [vectorCheck]

end Lambo.Contract
