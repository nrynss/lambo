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

The second half (from "The operator override, re-embed and vector reads")
models what the first half left out: the operator override
(`--allow-embedding-mismatch`), the `lambo re-embed` migration and backfill,
the legacy-vector quarantine at load, the checked vector read and the
embedder canary gate. Its own header lists the Rust items and what it leaves
out.

The third part (from "The fix for #296") keeps all of that as the previous
version and adds the fixed `lambo re-embed`: an in-place rewrite for a
session already under the live contract (the canary-drift repair), the
canary gate on `--missing-only`, a guard against silently replacing a canary
pin other sessions rely on, and the override WARN logged only after the
relabel. Its header lists what each theorem proves.

The fourth part (from "The durable re-embed of a contract-less session") adds
the store: applying a flushed batch, with the first-stamp legacy quarantine.
It keeps the old `reembed` and pins #322 against it (a re-embed of a
contract-less session reports full coverage and leaves the store with the
live contract and no vector), then adds the fixed batch order
(`reembedOrdered`, contract first on a contract-less session) and proves the
store keeps every new vector, matches the graph, and is unchanged for a
stamped session.

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
  · rw [ite_eq_left h] <;> rfl
  · rw [ite_eq_right h] <;> rfl

/-- A stamp on a stamped session succeeds only for the stored contract. -/
theorem stamp_ok_eq (c offered : Contract K M) (h : (stamp (some c) offered).2 = true) :
    offered = c := by
  by_cases he : c = offered
  · exact he.symm
  · have e : stamp (some c) offered = (some c, false) := by
      show (if c = offered then (some c, true) else (some c, false)) = (some c, false)
      rw [ite_eq_right he]
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
      · rw [ite_eq_left hs] at h'
        exact Compat.noConfusion h'
      · rw [ite_eq_right hs] at h'
        exact Compat.noConfusion h'
    · intro h
      cases h

/-- A recorded contract is `Compatible` exactly when it equals the live one. -/
theorem classify_compatible_iff (s live : Contract K M) :
    classify (some s) live = .compatible ↔ s = live := by
  constructor
  · intro h
    have h' : (if s = live then Compat.compatible else Compat.mismatch) = .compatible := h
    by_cases hs : s = live
    · exact hs
    · rw [ite_eq_right hs] at h'
      exact Compat.noConfusion h'
  · intro hs
    show (if s = live then Compat.compatible else Compat.mismatch) = .compatible
    rw [ite_eq_left hs]

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

/-! # The operator override, re-embed and vector reads

Rust items modelled (paths and lines as at `703f703f`):

* `MemoryBuilder::build_attach`, step (2), the writer attach's contract
  check (`src/memory/builder.rs:638-690`): `attach`. An unstamped session is
  stamped; an equal contract passes; a mismatch is refused unless the
  override is on and the widths are equal, in which case it logs a WARN
  (`attachWarns`) and calls `Graph::replace_embedding_with_operator_override`
  (`src/graph/graph/embeddings.rs:80`, `overrideRelabel`), whose
  `SetEmbedding` is flushed durably before the attach returns.
* `Graph::replace_embedding_without_vectors` (`embeddings.rs:51`,
  `replaceNoVectors`): the public re-stamp gate, refused while any concept
  carries a vector. It has no production caller at `703f703f`.
* `Graph::reembed_all` / `reembed_all_dropping_image_vectors` / `reembed`
  (`embeddings.rs:148-296`, `reembed`): the atomic full re-embed, and
  `Graph::embed_missing` (`embeddings.rs:328`, `backfill`): the same-space
  backfill.
* `re_embed::run` (`src/cli/re_embed.rs:70`, `reembedCmd`): the CLI verb. It
  refuses `--allow-embedding-mismatch` outright, attaches with
  `reembed_mode` (no contract check, nothing relabelled) and then runs
  `reembed_all` over every text concept, or `embed_missing` with
  `--missing-only`.
* An ordinary write with a vector (`hybrid.rs:1481-1517`,
  `record_action_against` in `src/graph/action.rs:469`): `write`. It is
  `stamp_embedding`, the existing `stamp`: stamp an unstamped session, else
  `ensure_compatible` or refuse.
* `quarantine_legacy_vectors` (`src/store/load.rs:123`, `quarantine`).
* The checked vector read every source shares (`graph_vector_candidates`,
  `src/graph/vector_source.rs:101`; SQLite's `checked_vector_candidates`,
  `src/store/sqlite/vector_candidates.rs:124`): `vectorRead`. No durable
  contract: empty; a contract other than the expected one: refused;
  otherwise every stored vector.
* `canary_gate` (`src/memory/builder.rs:998`): `canaryAdmits`.

Where the override comes from: only the `--allow-embedding-mismatch` CLI flag
on writer commands (`Commands::allow_embedding_mismatch`, `src/main.rs:1043`:
`serve`, `demo`, `derive`, `derive-image`, `record-action`, `reserve`,
`release`, and `re-embed`, which refuses it), copied into
`ResolvedBackends::allow_embedding_mismatch`, and the library setter
`MemoryBuilder::allow_embedding_mismatch`. There is no `lambo.toml` key or
environment variable. Readers never carry it, and the checked read takes no
override at all.

## Model

A concept carries `vec`, the contract of the embedder that produced its
stored vector (`none`: no vector), and `image` (`embedding_source` set, #22).
"Mixing" is stated over these labels: `oneSpace s` says every stored vector
was produced under the session's contract. Whether two different contract
labels denote the same vector space is not something the code can know; the
override is the operator asserting that they do.

## Results

* `attach_no_override_ok_iff`, `attach_no_override_contract`: without the
  override the writer attach is exactly `stamp` (agreement with the first
  half).
* `attach_override_ok_iff`: with the override, a mismatched attach succeeds
  exactly when the widths are equal and either no concept carries a vector or
  the kind is unchanged (a model-identifier rename).
  `attach_override_width_refused` and `attach_override_cross_kind_refused`
  are the two refusals; `attach_ok_contract_live`: any successful writer
  attach leaves the session under the live contract.
* `attach_keeps_concepts`: the override relabels, it never rewrites or drops
  a vector. `attach_override_records`: a successful override emits exactly
  one `SetEmbedding` of the live contract and warns.
* `write_keeps_contract`, `backfill_keeps_contract`,
  `runAll_ordinary_keeps`: no ordinary write changes a stamped contract, with
  or without the override on (the override is attach-only).
  `contract_change_explicit`: a contract changes only by an override attach, a
  full re-embed, or the no-vectors re-stamp.
* `reembed_one_space`: a successful re-embed leaves every stored vector in the
  new space whatever it started from; `reembed_contract_last`: its batch is
  upserts with one trailing `SetEmbedding`.
* `runAll_oneSpace`, `read_one_space`, `no_mixing_without_override`: without
  the override, a vector read returns vectors of the expected contract only.
  `attach_override_relabel_mixes`, `override_mixes_reads`: with it, a
  same-kind rename over existing vectors puts two labels behind one contract,
  and a later read returns both.
* `reembed_refuses_same`, `reembedCmd_refuses_same_contract`: `re-embed`
  cannot rewrite a session already under the live contract, which is the
  state a canary drift is detected in (finding 1 below).
* `reembedCmd_full_never_warns`, `reembed_repins_without_warning`: every
  full re-embed that succeeds re-pins the canary and never prints the
  "other sessions under the same contract" warning (finding 2).
* `missingOnly_skips_canary`, `example_drifted_backfill`: the backfill runs
  whatever the canary says (finding 3).

## Findings about the Rust at `703f703f`

1. **Drift cannot be re-embedded in place.** The drift refusal says
   "Re-embed the store's sessions with `lambo re-embed`"
   (`src/memory/builder.rs:1028-1037`), and `docs/reference/config.mdx:263`
   speaks of "re-embedding one session under an unchanged contract name". A
   drift is only seen on a session whose stored contract equals the live one
   (the canary is keyed by the contract and runs after the contract check),
   and `Graph::reembed` refuses exactly that state (`embeddings.rs:184-188`);
   `re_embed::run` calls it with no special case (`re_embed.rs:347-350`).
   So the advice as written cannot be followed
   (`reembedCmd_refuses_same_contract`,
   `example_drift_not_reembeddable_in_place`). The only repair is
   undocumented: change the contract identity at the same width, then
   re-embed (`example_rename_hop`). Where the contract `model` is the
   free-form `[embedder] model` / `llama_model` (the `_ =>` arm of
   `resolve_embedder`, `src/resolve.rs:381-393`, e.g. `bge_m3`; `None` and
   `Some` differ), an alias of the same model makes stored and live differ
   and the re-embed succeeds, and a second re-embed back to the original
   name succeeds too. A same-width kind hop (`bge_m3` to `candle`, both
   BGE-M3 at 1024) also works (`example_reembed`). `candle` (source revision
   plus weights sha) and `gemini` (provider model id) stamp a derived
   identity and have no rename route.
2. **Silent re-pin.** After a successful full rewrite, `re_embed::run`
   (`re_embed.rs:211-230`) pins the canary for the live contract, replacing
   the store's pin for that name, and warns that other sessions under the
   name are no longer flagged only when `prior == live`. That never holds
   after a successful full rewrite (`reembedCmd_full_never_warns`). So any
   re-embed into a name other sessions already use re-pins it silently,
   unflagging their drift: the round trip's second hop (prior `A'`, live
   `A`) and the re-embed of an unrecorded session (prior `None`) are two
   routes (`example_silent_repin`). The store-wide pin table and the other
   sessions are not modelled; `repins` is the per-run decision only.
3. **`re-embed --missing-only` bypasses the canary.** It attaches in
   `reembed_mode` (`re_embed.rs:112-122`), which skips `canary_gate`
   (`builder.rs:717-720`), so under `canary = "verify"` it writes vectors
   from a drifted embedder into the session with no check, where every other
   writer attach is refused (`missingOnly_skips_canary`,
   `example_drifted_backfill`).
4. **The override's WARN precedes a relabel that may refuse**
   (`builder.rs:648-658`; `example_warn_without_relabel`).
5. **The override's same-kind guard is weak for generic kinds.** The guard
   compares the kind string only. For a kind whose `model` is free-form
   (`bge_m3`, which the `openai` alias also selects), vectors from one model
   can be relabelled as a different model of the same kind and width (BGE-M3
   at 1024 as qwen3-embedding-4b at 1024), and `attach_override_ok_iff`
   admits it (`example_same_kind_relabel`).

Not modelled: vector values, widths of individual vectors and finiteness
(each written vector is assumed to have its contract's width, which the
width checks enforce); node ids are assumed distinct, as the graph's node map
makes them; merges and edits that keep a concept's vector; the lease, the
flush and crash atomicity (the re-embed batch is one store transaction,
assumed); the canary's measurement (`drifted` is an input); the
`StoreAndUnflushed` union source.
-/

/-- A concept as far as vectors go. `vec` is the contract of the embedder
that produced its stored vector, `none` for no vector; `image` is
`embedding_source.is_some()` (#22). -/
structure Concept (K M : Type) where
  id : Nat
  image : Bool
  vec : Option (Contract K M)
  deriving DecidableEq

/-- A session's contract and concepts. -/
structure Session (K M : Type) where
  contract : Option (Contract K M)
  concepts : List (Concept K M)
  deriving DecidableEq

/-- The two mutations this half emits. -/
inductive Mut (K M : Type) where
  | upsert (id : Nat) (vec : Option (Contract K M))
  | setEmbedding (contract : Option (Contract K M))
  deriving DecidableEq

/-- Whether a mutation is a node upsert. -/
def Mut.isUpsert : Mut K M → Bool
  | .upsert _ _ => true
  | .setEmbedding _ => false

/-- `self.concepts().any(|c| c.embedding.is_some())`. -/
def hasVectors (s : Session K M) : Bool := s.concepts.any (fun c => c.vec.isSome)

/-- Every stored vector was produced under the session's contract. -/
def oneSpace (s : Session K M) : Bool :=
  s.concepts.all (fun c => c.vec.isNone || decide (c.vec = s.contract))

theorem oneSpace_iff (s : Session K M) :
    oneSpace s = true ↔ ∀ c ∈ s.concepts, c.vec = none ∨ c.vec = s.contract := by
  simp [oneSpace, List.all_eq_true, Option.isNone_iff_eq_none]

omit [DecidableEq K] [DecidableEq M] in
theorem hasVectors_false_iff (s : Session K M) :
    hasVectors s = false ↔ ∀ c ∈ s.concepts, c.vec = none := by
  simp [hasVectors, List.any_eq_false, Option.isSome_iff_ne_none]

/-- Set the contract, emitting `SetEmbedding` only when it changes (the
`if self.embedding != contract` guard both replace methods share). -/
def relabel (s : Session K M) (c : Option (Contract K M)) : Session K M × List (Mut K M) :=
  if s.contract = c then (s, []) else ({ s with contract := c }, [.setEmbedding c])

theorem relabel_spec {s s' : Session K M} {c : Option (Contract K M)} {m : List (Mut K M)}
    (h : relabel s c = (s', m)) :
    s'.contract = c ∧ s'.concepts = s.concepts ∧
      m = (if s.contract = c then [] else [.setEmbedding c]) := by
  unfold relabel at h
  by_cases hc : s.contract = c
  · rw [ite_eq_left hc] at h
    injection h with h1 h2
    subst h1; subst h2
    exact ⟨hc, rfl, by rw [ite_eq_left hc]⟩
  · rw [ite_eq_right hc] at h
    injection h with h1 h2
    subst h1; subst h2
    exact ⟨rfl, rfl, by rw [ite_eq_right hc]⟩

/-- `Graph::replace_embedding_with_operator_override`. -/
def overrideRelabel (s : Session K M) (live : Contract K M) :
    Option (Session K M × List (Mut K M)) :=
  match s.contract with
  | some e =>
    if e.dim = live.dim then
      if hasVectors s && !decide (e.kind = live.kind) then none
      else some (relabel s (some live))
    else none
  | none => some (relabel s (some live))

theorem overrideRelabel_spec {s : Session K M} {live : Contract K M}
    {r : Session K M × List (Mut K M)} (h : overrideRelabel s live = some r) :
    relabel s (some live) = r ∧
      ∀ e, s.contract = some e → e.dim = live.dim ∧ (hasVectors s = false ∨ e.kind = live.kind) := by
  unfold overrideRelabel at h
  cases hc : s.contract with
  | none =>
    rw [hc] at h
    dsimp only at h
    exact ⟨Option.some.inj h, fun e he => by cases he⟩
  | some e =>
    rw [hc] at h
    dsimp only at h
    by_cases hd : e.dim = live.dim
    · rw [ite_eq_left hd] at h
      by_cases hk : (hasVectors s && !decide (e.kind = live.kind)) = true
      · rw [ite_eq_left hk] at h; cases h
      · rw [ite_eq_right hk] at h
        refine ⟨Option.some.inj h, fun e' he' => ?_⟩
        injection he' with he'
        subst he'
        refine ⟨hd, ?_⟩
        cases hv : hasVectors s with
        | false => exact Or.inl rfl
        | true =>
          right
          rw [hv] at hk
          simpa using hk
    · rw [ite_eq_right hd] at h; cases h

/-- The writer attach's contract check (`build_attach` step (2), not in
re-embed mode). `allow` is `allow_embedding_mismatch`. -/
def attach (s : Session K M) (live : Contract K M) (allow : Bool) :
    Option (Session K M × List (Mut K M)) :=
  match s.contract with
  | none => some (relabel s (some live))
  | some e =>
    if e = live then some (s, [])
    else if allow && decide (e.dim = live.dim) then overrideRelabel s live
    else none

/-- The attach's WARN line ("operator allowed an embedding contract
mismatch; relabeling ..."), logged before the relabel is attempted. -/
def attachWarns (s : Session K M) (live : Contract K M) (allow : Bool) : Bool :=
  match s.contract with
  | none => false
  | some e => !decide (e = live) && allow && decide (e.dim = live.dim)

/-- Everything a successful attach guarantees: concepts untouched, the live
contract installed, and, when the stored contract differed, the override was
on, the widths agree, the kind is unchanged or no vector exists, and exactly
one `SetEmbedding` was emitted. -/
theorem attach_spec {s s' : Session K M} {live : Contract K M} {allow : Bool}
    {m : List (Mut K M)} (h : attach s live allow = some (s', m)) :
    s'.concepts = s.concepts ∧ s'.contract = some live ∧
      ∀ e, s.contract = some e → e ≠ live →
        allow = true ∧ e.dim = live.dim ∧ (hasVectors s = false ∨ e.kind = live.kind) ∧
          m = [.setEmbedding (some live)] := by
  unfold attach at h
  cases hc : s.contract with
  | none =>
    rw [hc] at h
    dsimp only at h
    obtain ⟨h1, h2, _⟩ := relabel_spec (Option.some.inj h)
    exact ⟨h2, h1, fun e he => by cases he⟩
  | some e =>
    rw [hc] at h
    dsimp only at h
    by_cases he : e = live
    · rw [ite_eq_left he] at h
      injection h with h
      injection h with h1 h2
      subst h1; subst h2
      refine ⟨rfl, by rw [hc, he], fun e' he' hne => ?_⟩
      injection he' with he'
      subst he'
      exact absurd he hne
    · rw [ite_eq_right he] at h
      by_cases ha : (allow && decide (e.dim = live.dim)) = true
      · rw [ite_eq_left ha] at h
        obtain ⟨hr, hdim⟩ := overrideRelabel_spec h
        obtain ⟨h1, h2, h3⟩ := relabel_spec hr
        refine ⟨h2, h1, fun e' he' _ => ?_⟩
        injection he' with he'
        subst he'
        obtain ⟨hd, hk⟩ := hdim e hc
        have hne : ¬ (some e = some live) := fun h' => he (Option.some.inj h')
        refine ⟨?_, hd, hk, ?_⟩
        · cases allow with
          | false => simp at ha
          | true => rfl
        · rw [h3, hc, ite_eq_right hne]
      · rw [ite_eq_right ha] at h; cases h

/-- An ordinary write of one concept, with a vector from the live embedder
(`withVec`) or none. -/
def write (s : Session K M) (live : Contract K M) (id : Nat) (image withVec : Bool) :
    Option (Session K M × List (Mut K M)) :=
  if withVec then
    if (stamp s.contract live).2 then
      some ({ contract := (stamp s.contract live).1,
              concepts := s.concepts ++ [⟨id, image, some live⟩] },
            (if s.contract = none then [.setEmbedding (some live)] else []) ++
              [.upsert id (some live)])
    else none
  else some ({ s with concepts := s.concepts ++ [⟨id, image, none⟩] }, [.upsert id none])

/-- Pairwise distinct (the `HashSet` insert checks). -/
def distinct : List Nat → Bool
  | [] => true
  | a :: t => decide (a ∉ t) && distinct t

/-- A duplicate-free list inside `m` with `m`'s length covers `m`: the
re-embed coverage check ("count distinct coverage, not list length"). -/
theorem cover_of_distinct : ∀ (l m : List Nat), distinct l = true → (∀ x ∈ l, x ∈ m) →
    l.length = m.length → ∀ x ∈ m, x ∈ l
  | [], m, _, _, hlen, x, hx => by
    cases m with
    | nil => cases hx
    | cons _ _ => simp at hlen
  | a :: t, m, hd, hsub, hlen, x, hx => by
    simp only [distinct, Bool.and_eq_true, decide_eq_true_eq] at hd
    have ha : a ∈ m := hsub a (by simp)
    have hlen' : t.length = (m.erase a).length := by
      rw [List.length_erase_of_mem ha]; simp at hlen; omega
    have hsub' : ∀ y ∈ t, y ∈ m.erase a := by
      intro y hy
      have hne : y ≠ a := fun h => hd.1 (h ▸ hy)
      exact (List.mem_erase_of_ne hne).mpr (hsub y (by simp [hy]))
    by_cases hxa : x = a
    · subst hxa; simp
    · have := cover_of_distinct t (m.erase a) hd.2 hsub' hlen' x
        ((List.mem_erase_of_ne hxa).mpr hx)
      simp [this]

/-- Give each listed concept a vector in `live`. -/
def fill (cs : List (Concept K M)) (ids : List Nat) (live : Contract K M) : List (Concept K M) :=
  cs.map (fun c => if decide (c.id ∈ ids) then { c with vec := some live } else c)

/-- `Graph::embed_missing`: fill vectors in the session's own space. Each id
must name a text concept with no vector. -/
def backfill (s : Session K M) (live : Contract K M) (ids : List Nat) :
    Option (Session K M × List (Mut K M)) :=
  if s.contract = some live && distinct ids &&
      ids.all (fun i => s.concepts.any (fun c => decide (c.id = i) && !c.image && c.vec.isNone))
  then some ({ s with concepts := fill s.concepts ids live }, ids.map (fun i => .upsert i (some live)))
  else none

/-- The ids of the text concepts (`embedding_source.is_none()`). -/
def textIds (s : Session K M) : List Nat :=
  (s.concepts.filter (fun c => !c.image)).map (·.id)

/-- The ids of the image concepts that carry a vector. -/
def imageVectorIds (s : Session K M) : List Nat :=
  (s.concepts.filter (fun c => c.image && c.vec.isSome)).map (·.id)

/-- `reembed`'s contract guard: same width, and not the stored contract. -/
def swapOk (stored : Option (Contract K M)) (live : Contract K M) : Bool :=
  match stored with
  | none => true
  | some e => decide (e.dim = live.dim) && !decide (e = live)

/-- Re-embed's rewrite: every listed text concept gets a `live` vector; with
`drop`, every image concept loses its vector. -/
def rewrite (cs : List (Concept K M)) (ids : List Nat) (live : Contract K M) (drop : Bool) :
    List (Concept K M) :=
  cs.map (fun c =>
    if c.image then (if drop then { c with vec := none } else c)
    else if decide (c.id ∈ ids) then { c with vec := some live } else c)

/-- Re-embed's batch: the upserts, then the contract. -/
def reembedBatch (s : Session K M) (live : Contract K M) (ids : List Nat) (drop : Bool) :
    List (Mut K M) :=
  ids.map (fun i => .upsert i (some live)) ++
    (if drop then (imageVectorIds s).map (fun i => .upsert i none) else []) ++
    [.setEmbedding (some live)]

/-- Re-embed's checks, in the Rust's order: the contract guard, image
vectors, duplicates, coverage count, and every id a text concept. -/
def reembedOk (s : Session K M) (live : Contract K M) (ids : List Nat) (drop : Bool) : Bool :=
  swapOk s.contract live && ((imageVectorIds s).isEmpty || drop) && distinct ids &&
    decide (ids.length = (textIds s).length) && ids.all (fun i => decide (i ∈ textIds s))

/-- `Graph::reembed` (`drop` = `reembed_all_dropping_image_vectors`). -/
def reembed (s : Session K M) (live : Contract K M) (ids : List Nat) (drop : Bool) :
    Option (Session K M × List (Mut K M)) :=
  if reembedOk s live ids drop then
    some (⟨some live, rewrite s.concepts ids live drop⟩, reembedBatch s live ids drop)
  else none

/-- `Graph::replace_embedding_without_vectors`. -/
def replaceNoVectors (s : Session K M) (c : Option (Contract K M)) :
    Option (Session K M × List (Mut K M)) :=
  if hasVectors s then none else some (relabel s c)

/-- `quarantine_legacy_vectors`: a session loaded with no contract has its
vectors stripped. -/
def quarantine (s : Session K M) : Session K M :=
  match s.contract with
  | some _ => s
  | none => { s with concepts := s.concepts.map (fun c => { c with vec := none }) }

/-- The checked vector read: `none` is the refusal, otherwise the label of
every vector the read ranks. -/
def vectorRead (s : Session K M) (expected : Contract K M) : Option (List (Contract K M)) :=
  match s.contract with
  | none => some []
  | some c => if c = expected then some (s.concepts.filterMap (·.vec)) else none

/-- `lambo re-embed`. `allow`, `missingOnly`, `drop` are its three flags. -/
def reembedCmd (s : Session K M) (live : Contract K M) (allow missingOnly drop : Bool) :
    Option (Session K M × List (Mut K M)) :=
  if allow then none
  else if missingOnly && drop then none
  else if !missingOnly && !(imageVectorIds s).isEmpty && !drop then none
  else if s.concepts.isEmpty then some (s, [])
  else if missingOnly then
    let ids := (s.concepts.filter (fun c => !c.image && c.vec.isNone)).map (·.id)
    if ids.isEmpty then some (s, []) else backfill s live ids
  else reembed s live (textIds s) drop

/-- A session-level operation. -/
inductive Op (K M : Type) where
  | attach (live : Contract K M) (allow : Bool)
  | write (live : Contract K M) (id : Nat) (image withVec : Bool)
  | backfill (live : Contract K M) (ids : List Nat)
  | reembed (live : Contract K M) (ids : List Nat) (drop : Bool)
  | replaceNoVectors (c : Option (Contract K M))

/-- Whether an operation carries the operator override. -/
def Op.overrides : Op K M → Bool
  | .attach _ allow => allow
  | _ => false

/-- Ordinary operations: writes, backfills and attaches. -/
def Op.ordinary : Op K M → Bool
  | .attach _ _ => true
  | .write .. => true
  | .backfill .. => true
  | _ => false

/-- One operation. -/
def step (s : Session K M) : Op K M → Option (Session K M × List (Mut K M))
  | .attach live allow => attach s live allow
  | .write live id image withVec => write s live id image withVec
  | .backfill live ids => backfill s live ids
  | .reembed live ids drop => reembed s live ids drop
  | .replaceNoVectors c => replaceNoVectors s c

/-- A sequence of operations; a refused one leaves the session as it was. -/
def runAll (s : Session K M) : List (Op K M) → Session K M
  | [] => s
  | op :: rest =>
    match step s op with
    | some (s', _) => runAll s' rest
    | none => runAll s rest

/-! ### The writer attach -/

/-- The attach follows `classify`: compatible passes unchanged, unrecorded is
stamped, a mismatch without the override is refused. -/
theorem attach_follows_classify (s : Session K M) (live : Contract K M) (allow : Bool) :
    (classify s.contract live = .compatible → attach s live allow = some (s, [])) ∧
    (classify s.contract live = .unrecorded →
      attach s live allow = some ({ s with contract := some live }, [.setEmbedding (some live)])) ∧
    (classify s.contract live = .mismatch → allow = false → attach s live allow = none) := by
  unfold classify attach
  cases hc : s.contract with
  | none => simp [relabel, hc]
  | some e => by_cases h : e = live <;> cases allow <;> simp [h]

/-- **Agreement, acceptance.** Without the override, the attach succeeds
exactly when `stamp` accepts. -/
theorem attach_no_override_ok_iff (s : Session K M) (live : Contract K M) :
    (attach s live false).isSome = (stamp s.contract live).2 := by
  unfold attach stamp
  cases s.contract with
  | none => rfl
  | some e => by_cases h : e = live <;> simp [h]

/-- **Agreement, result.** Without the override, a successful attach leaves
the contract `stamp` leaves and every concept as it was. -/
theorem attach_no_override_contract (s s' : Session K M) (live : Contract K M)
    (m : List (Mut K M)) (h : attach s live false = some (s', m)) :
    s'.contract = (stamp s.contract live).1 ∧ s'.concepts = s.concepts := by
  obtain ⟨hcs, hcon, hmis⟩ := attach_spec h
  refine ⟨?_, hcs⟩
  rw [hcon]
  unfold stamp
  cases hc : s.contract with
  | none => rfl
  | some e =>
    by_cases he : e = live
    · simp [he]
    · exact absurd (hmis e hc he).1 (by simp)

/-- Any successful writer attach, override on or off, leaves the session
under the live contract. -/
theorem attach_ok_contract_live (s s' : Session K M) (live : Contract K M) (allow : Bool)
    (m : List (Mut K M)) (h : attach s live allow = some (s', m)) :
    s'.contract = some live :=
  (attach_spec h).2.1

/-- The override relabels, it never rewrites or drops a vector: a successful
attach leaves every concept as it was. -/
theorem attach_keeps_concepts (s s' : Session K M) (live : Contract K M) (allow : Bool)
    (m : List (Mut K M)) (h : attach s live allow = some (s', m)) :
    s'.concepts = s.concepts :=
  (attach_spec h).1

/-- **What the override permits.** On a session stamped `e ≠ live`, the
attach with the override succeeds exactly when the widths agree and either no
concept carries a vector or only the model identifier changed. -/
theorem attach_override_ok_iff (s : Session K M) (e live : Contract K M)
    (hc : s.contract = some e) (hne : e ≠ live) :
    (attach s live true).isSome = true ↔
      e.dim = live.dim ∧ (hasVectors s = false ∨ e.kind = live.kind) := by
  unfold attach overrideRelabel
  rw [hc]
  by_cases hd : e.dim = live.dim
  · cases hv : hasVectors s with
    | true => by_cases hk : e.kind = live.kind <;> simp [hne, hd, hk]
    | false => simp [hne, hd]
  · simp [hne, hd]

/-- **Refused even with the override:** a width change. -/
theorem attach_override_width_refused (s : Session K M) (e live : Contract K M) (allow : Bool)
    (hc : s.contract = some e) (hd : e.dim ≠ live.dim) : attach s live allow = none := by
  unfold attach
  rw [hc]
  have hne : e ≠ live := fun h => hd (h ▸ rfl)
  simp [hne, hd]

/-- **Refused even with the override:** a kind change while any concept
carries a vector. -/
theorem attach_override_cross_kind_refused (s : Session K M) (e live : Contract K M)
    (allow : Bool) (hc : s.contract = some e) (hv : hasVectors s = true)
    (hk : e.kind ≠ live.kind) : attach s live allow = none := by
  unfold attach overrideRelabel
  rw [hc]
  have hne : e ≠ live := fun h => hk (h ▸ rfl)
  by_cases hd : e.dim = live.dim <;> simp [hne, hd, hv, hk]

/-- **What is recorded.** A successful override emits exactly one
`SetEmbedding` of the live contract (flushed before the attach returns) and
logs the WARN. -/
theorem attach_override_records (s s' : Session K M) (e live : Contract K M)
    (m : List (Mut K M)) (hc : s.contract = some e) (hne : e ≠ live)
    (h : attach s live true = some (s', m)) :
    m = [.setEmbedding (some live)] ∧ attachWarns s live true = true := by
  obtain ⟨_, hd, _, hm⟩ := (attach_spec h).2.2 e hc hne
  exact ⟨hm, by simp [attachWarns, hc, hne, hd]⟩

/-- The WARN fires exactly on a same-width mismatch with the override on,
including when the relabel that follows refuses. -/
theorem attachWarns_iff (s : Session K M) (live : Contract K M) (allow : Bool) :
    attachWarns s live allow = true ↔
      classify s.contract live = .mismatch ∧ allow = true ∧
        ∃ e, s.contract = some e ∧ e.dim = live.dim := by
  unfold attachWarns classify
  cases s.contract with
  | none => simp
  | some e => by_cases he : e = live <;> simp [he]

/-! ### Ordinary writes never move the contract -/

/-- An ordinary write never changes a stamped contract. Writes take no
override: the flag only reaches the attach. -/
theorem write_keeps_contract (s s' : Session K M) (c live : Contract K M) (id : Nat)
    (image withVec : Bool) (m : List (Mut K M)) (hc : s.contract = some c)
    (h : write s live id image withVec = some (s', m)) : s'.contract = some c := by
  unfold write at h
  cases withVec with
  | false =>
    simp only [Bool.false_eq_true, ite_false, Option.some.injEq, Prod.mk.injEq] at h
    obtain ⟨rfl, rfl⟩ := h
    exact hc
  | true =>
    simp only [ite_true] at h
    by_cases hs : (stamp s.contract live).2 = true
    · rw [ite_eq_left hs] at h
      simp only [Option.some.injEq, Prod.mk.injEq] at h
      obtain ⟨rfl, rfl⟩ := h
      show (stamp s.contract live).1 = some c
      rw [hc]; exact stamp_keeps c live
    · rw [ite_eq_right hs] at h; cases h

/-- A write of a vector from another space into a stamped session is refused. -/
theorem write_refuses_other_space (s : Session K M) (c live : Contract K M) (id : Nat)
    (image : Bool) (hc : s.contract = some c) (hne : c ≠ live) :
    write s live id image true = none := by
  unfold write stamp
  rw [hc]
  simp [hne]

/-- A backfill never changes the contract. -/
theorem backfill_keeps_contract (s s' : Session K M) (live : Contract K M) (ids : List Nat)
    (m : List (Mut K M)) (h : backfill s live ids = some (s', m)) :
    s'.contract = s.contract := by
  unfold backfill at h
  split at h
  · simp only [Option.some.injEq, Prod.mk.injEq] at h
    obtain ⟨rfl, rfl⟩ := h
    rfl
  · cases h

/-- A backfill emits no `SetEmbedding` ("emitting one would ... make the
batch look like a migration"). -/
theorem backfill_no_setEmbedding (s s' : Session K M) (live : Contract K M) (ids : List Nat)
    (m : List (Mut K M)) (h : backfill s live ids = some (s', m)) :
    ∀ x ∈ m, x.isUpsert = true := by
  unfold backfill at h
  split at h
  · simp only [Option.some.injEq, Prod.mk.injEq] at h
    obtain ⟨rfl, rfl⟩ := h
    intro x hx
    simp only [List.mem_map] at hx
    obtain ⟨i, _, rfl⟩ := hx
    rfl
  · cases h

/-- A backfill is refused unless the session is already stamped with the live
contract. -/
theorem backfill_needs_same_contract (s : Session K M) (live : Contract K M) (ids : List Nat)
    (h : s.contract ≠ some live) : backfill s live ids = none := by
  unfold backfill; simp [h]

/-- **Ordinary operations keep a stamped contract** (extends
`stampAll_keeps`): no sequence of writes, backfills and attaches without the
override changes it. -/
theorem runAll_ordinary_keeps (c : Contract K M) :
    ∀ (ops : List (Op K M)) (s : Session K M), s.contract = some c →
      (∀ op ∈ ops, op.ordinary = true ∧ op.overrides = false) →
      (runAll s ops).contract = some c := by
  intro ops
  induction ops with
  | nil => intro s hc _; exact hc
  | cons op rest ih =>
    intro s hc hops
    have hrest : ∀ op ∈ rest, op.ordinary = true ∧ op.overrides = false :=
      fun o ho => hops o (by simp [ho])
    have hop := hops op (by simp)
    unfold runAll
    split
    · rename_i s' m hstep
      apply ih s' _ hrest
      cases op with
      | attach live allow =>
        cases allow with
        | true => simp [Op.overrides] at hop
        | false =>
          have := (attach_no_override_contract s s' live m hstep).1
          rw [this, hc]; exact stamp_keeps c live
      | write live id image withVec =>
        exact write_keeps_contract s s' c live id image withVec m hc hstep
      | backfill live ids => rw [backfill_keeps_contract s s' live ids m hstep, hc]
      | reembed => simp [Op.ordinary] at hop
      | replaceNoVectors => simp [Op.ordinary] at hop
    · exact ih s hc hrest

/-! ### Re-embed -/

/-- A successful re-embed installs the live contract. -/
theorem reembed_contract (s s' : Session K M) (live : Contract K M) (ids : List Nat)
    (drop : Bool) (m : List (Mut K M)) (h : reembed s live ids drop = some (s', m)) :
    s'.contract = some live := by
  unfold reembed at h
  split at h
  · simp only [Option.some.injEq, Prod.mk.injEq] at h
    obtain ⟨rfl, rfl⟩ := h
    rfl
  · cases h

/-- Re-embed refuses a session already under the live contract. -/
theorem reembed_refuses_same (s : Session K M) (live : Contract K M) (ids : List Nat)
    (drop : Bool) (hc : s.contract = some live) : reembed s live ids drop = none := by
  unfold reembed reembedOk swapOk
  rw [hc]
  simp

/-- Re-embed never changes width. -/
theorem reembed_refuses_width (s : Session K M) (e live : Contract K M) (ids : List Nat)
    (drop : Bool) (hc : s.contract = some e) (hd : e.dim ≠ live.dim) :
    reembed s live ids drop = none := by
  unfold reembed reembedOk swapOk
  rw [hc]
  simp [hd]

/-- **Re-embed ends mixing.** Whatever the session held, a successful
re-embed leaves every stored vector in the new space: every text concept is
covered, and an image vector is either absent or dropped. -/
theorem reembed_one_space (s s' : Session K M) (live : Contract K M) (ids : List Nat)
    (drop : Bool) (m : List (Mut K M)) (h : reembed s live ids drop = some (s', m)) :
    oneSpace s' = true := by
  unfold reembed at h
  split at h
  · rename_i hg
    unfold reembedOk at hg
    simp only [Bool.and_eq_true, decide_eq_true_eq, List.all_eq_true] at hg
    obtain ⟨⟨⟨⟨_, himg⟩, hdist⟩, hlen⟩, hsub⟩ := hg
    simp only [Option.some.injEq, Prod.mk.injEq] at h
    obtain ⟨rfl, rfl⟩ := h
    rw [oneSpace_iff]
    intro c' hc'
    simp only [rewrite, List.mem_map] at hc'
    obtain ⟨c, hc, rfl⟩ := hc'
    cases himgc : c.image with
    | false =>
      have hct : c.id ∈ textIds s := by
        simp only [textIds, List.mem_map, List.mem_filter]
        exact ⟨c, ⟨hc, by simp [himgc]⟩, rfl⟩
      have hcov := cover_of_distinct ids (textIds s) hdist hsub hlen c.id hct
      simp [hcov]
    | true =>
      cases drop with
      | true => simp
      | false =>
        simp only [Bool.or_false] at himg
        have hnone : c.vec = none := by
          cases hv : c.vec with
          | none => rfl
          | some v =>
            have hmem : c.id ∈ imageVectorIds s := by
              simp only [imageVectorIds, List.mem_map, List.mem_filter]
              exact ⟨c, ⟨hc, by simp [himgc, hv]⟩, rfl⟩
            rw [List.isEmpty_iff] at himg
            rw [himg] at hmem; cases hmem
        simp [hnone]
  · cases h

/-- Re-embed's batch is node upserts with one trailing `SetEmbedding`
("vectors first, contract last"). -/
theorem reembed_contract_last (s s' : Session K M) (live : Contract K M) (ids : List Nat)
    (drop : Bool) (m : List (Mut K M)) (h : reembed s live ids drop = some (s', m)) :
    ∃ pre, m = pre ++ [.setEmbedding (some live)] ∧ ∀ x ∈ pre, x.isUpsert = true := by
  unfold reembed at h
  split at h
  · simp only [Option.some.injEq, Prod.mk.injEq] at h
    obtain ⟨rfl, rfl⟩ := h
    refine ⟨_, rfl, ?_⟩
    intro x hx
    simp only [List.mem_append, List.mem_map] at hx
    rcases hx with ⟨i, _, rfl⟩ | hx
    · rfl
    · cases drop with
      | true =>
        simp only [ite_true, List.mem_map] at hx
        obtain ⟨i, _, rfl⟩ := hx
        rfl
      | false => simp at hx
  · cases h

/-- The no-vectors re-stamp succeeds only on a session with no vector. -/
theorem replaceNoVectors_needs_empty (s s' : Session K M) (c : Option (Contract K M))
    (m : List (Mut K M)) (h : replaceNoVectors s c = some (s', m)) :
    hasVectors s = false := by
  unfold replaceNoVectors at h
  cases hv : hasVectors s with
  | false => rfl
  | true => rw [hv] at h; cases h

/-- **The contract moves only through the explicit paths.** If one operation
changes a stamped contract `c`, it is a same-width override attach, a
same-width full re-embed to a different contract, or the re-stamp of a session
with no vector. -/
theorem contract_change_explicit (s s' : Session K M) (c : Contract K M) (op : Op K M)
    (m : List (Mut K M)) (hc : s.contract = some c) (h : step s op = some (s', m))
    (hch : s'.contract ≠ some c) :
    (∃ live, op = .attach live true ∧ c.dim = live.dim ∧ s'.contract = some live) ∨
    (∃ live ids drop, op = .reembed live ids drop ∧ c.dim = live.dim ∧ c ≠ live ∧
      s'.contract = some live) ∨
    (∃ c', op = .replaceNoVectors c' ∧ hasVectors s = false) := by
  cases op with
  | attach live allow =>
    simp only [step] at h
    obtain ⟨_, hcon, hmis⟩ := attach_spec h
    have hne : c ≠ live := fun he => hch (by rw [hcon, he])
    obtain ⟨ha, hd, _, _⟩ := hmis c hc hne
    subst ha
    exact Or.inl ⟨live, rfl, hd, hcon⟩
  | write live id image withVec =>
    exact absurd (write_keeps_contract s s' c live id image withVec m hc h) hch
  | backfill live ids =>
    simp only [step] at h
    rw [backfill_keeps_contract s s' live ids m h] at hch
    exact absurd hc hch
  | reembed live ids drop =>
    simp only [step] at h
    right; left
    refine ⟨live, ids, drop, rfl, ?_, ?_, reembed_contract s s' live ids drop m h⟩
    · by_cases hd : c.dim = live.dim
      · exact hd
      · rw [reembed_refuses_width s c live ids drop hc hd] at h; cases h
    · intro he
      subst he
      rw [reembed_refuses_same s c ids drop hc] at h; cases h
  | replaceNoVectors c' =>
    right; right
    exact ⟨c', rfl, replaceNoVectors_needs_empty s s' c' m h⟩

/-! ### No mixing without the override -/

/-- Stamping a session that holds no vector, or relabelling to its own
contract, keeps it in one space. -/
theorem relabel_oneSpace (s : Session K M) (c : Option (Contract K M))
    (h : hasVectors s = false ∨ s.contract = c) (hs : oneSpace s = true)
    {s' : Session K M} {m : List (Mut K M)} (hr : relabel s c = (s', m)) :
    oneSpace s' = true := by
  obtain ⟨h1, h2, _⟩ := relabel_spec hr
  rw [oneSpace_iff, h2, h1]
  intro x hx
  rcases h with hv | he
  · left; exact (hasVectors_false_iff s).mp hv x hx
  · rw [← he]; exact (oneSpace_iff s).mp hs x hx

/-- Each operation without the override keeps the session in one space. -/
theorem step_oneSpace (s s' : Session K M) (op : Op K M) (m : List (Mut K M))
    (hop : op.overrides = false) (hs : oneSpace s = true) (h : step s op = some (s', m)) :
    oneSpace s' = true := by
  cases op with
  | attach live allow =>
    cases allow with
    | true => simp [Op.overrides] at hop
    | false =>
      simp only [step] at h
      obtain ⟨hcs, hcon, hmis⟩ := attach_spec h
      rw [oneSpace_iff, hcs, hcon]
      intro x hx
      rcases (oneSpace_iff s).mp hs x hx with h1 | h1
      · exact Or.inl h1
      · cases hc : s.contract with
        | none => rw [hc] at h1; exact Or.inl h1
        | some e =>
          by_cases he : e = live
          · rw [h1, hc, he]; exact Or.inr rfl
          · exact absurd (hmis e hc he).1 (by simp)
  | write live id image withVec =>
    simp only [step, write] at h
    cases withVec with
    | false =>
      simp only [Bool.false_eq_true, ite_false, Option.some.injEq, Prod.mk.injEq] at h
      obtain ⟨rfl, rfl⟩ := h
      rw [oneSpace_iff]
      intro x hx
      simp only [List.mem_append, List.mem_singleton] at hx
      rcases hx with hx | rfl
      · exact (oneSpace_iff s).mp hs x hx
      · exact Or.inl rfl
    | true =>
      simp only [ite_true] at h
      by_cases hst : (stamp s.contract live).2 = true
      · rw [ite_eq_left hst] at h
        simp only [Option.some.injEq, Prod.mk.injEq] at h
        obtain ⟨rfl, rfl⟩ := h
        -- the stamp accepted, so the session was unstamped or already `live`
        have hpre : s.contract = none ∨ s.contract = some live := by
          unfold stamp at hst
          cases hc : s.contract with
          | none => exact Or.inl rfl
          | some e =>
            rw [hc] at hst
            by_cases he : e = live
            · rw [he]; exact Or.inr rfl
            · simp [he] at hst
        have hl : (stamp s.contract live).1 = some live := by
          unfold stamp
          rcases hpre with hc | hc <;> rw [hc]; simp
        rw [oneSpace_iff]
        show ∀ x ∈ s.concepts ++ [⟨id, image, some live⟩],
          x.vec = none ∨ x.vec = (stamp s.contract live).1
        rw [hl]
        intro x hx
        simp only [List.mem_append, List.mem_singleton] at hx
        rcases hx with hx | rfl
        · rcases (oneSpace_iff s).mp hs x hx with h1 | h1
          · exact Or.inl h1
          · rcases hpre with hc | hc
            · rw [hc] at h1; exact Or.inl h1
            · rw [hc] at h1; exact Or.inr h1
        · exact Or.inr rfl
      · rw [ite_eq_right hst] at h; cases h
  | backfill live ids =>
    simp only [step] at h
    unfold backfill at h
    split at h
    · rename_i hg
      simp only [Bool.and_eq_true, decide_eq_true_eq] at hg
      obtain ⟨⟨hc, _⟩, _⟩ := hg
      simp only [Option.some.injEq, Prod.mk.injEq] at h
      obtain ⟨rfl, rfl⟩ := h
      rw [oneSpace_iff]
      intro x hx
      simp only [fill, List.mem_map] at hx
      obtain ⟨c, hcm, rfl⟩ := hx
      by_cases hi : c.id ∈ ids
      · simp [hi, hc]
      · simp only [hi, decide_false, Bool.false_eq_true, ite_false]
        exact (oneSpace_iff s).mp hs c hcm
    · cases h
  | reembed live ids drop =>
    exact reembed_one_space s s' live ids drop m h
  | replaceNoVectors c =>
    simp only [step, replaceNoVectors] at h
    cases hv : hasVectors s with
    | true => rw [hv] at h; cases h
    | false =>
      rw [hv] at h
      simp only [Bool.false_eq_true, ite_false, Option.some.injEq] at h
      exact relabel_oneSpace s c (Or.inl hv) hs h

/-- Without the override, a sequence of operations keeps the session in one
space. -/
theorem runAll_oneSpace :
    ∀ (ops : List (Op K M)) (s : Session K M), oneSpace s = true →
      (∀ op ∈ ops, op.overrides = false) → oneSpace (runAll s ops) = true := by
  intro ops
  induction ops with
  | nil => intro s hs _; exact hs
  | cons op rest ih =>
    intro s hs hops
    have hrest : ∀ op ∈ rest, op.overrides = false := fun o ho => hops o (by simp [ho])
    unfold runAll
    split
    · rename_i s' m hstep
      exact ih s' (step_oneSpace s s' op m (hops op (by simp)) hs hstep) hrest
    · exact ih s hs hrest

/-- The read refuses a session under another contract; it takes no
override. -/
theorem read_refuses_mismatch (s : Session K M) (c expected : Contract K M)
    (hc : s.contract = some c) (hne : c ≠ expected) : vectorRead s expected = none := by
  unfold vectorRead; rw [hc]; simp [hne]

/-- **One space per read.** In a one-space session, every vector a read
returns was produced under the contract the read expected. -/
theorem read_one_space (s : Session K M) (expected : Contract K M) (l : List (Contract K M))
    (hs : oneSpace s = true) (h : vectorRead s expected = some l) :
    ∀ x ∈ l, x = expected := by
  unfold vectorRead at h
  cases hc : s.contract with
  | none =>
    rw [hc] at h
    dsimp only at h
    injection h with h
    subst h
    intro x hx; cases hx
  | some c =>
    rw [hc] at h
    dsimp only at h
    by_cases he : c = expected
    · rw [ite_eq_left he] at h
      injection h with h
      subst h
      intro x hx
      simp only [List.mem_filterMap] at hx
      obtain ⟨y, hy, hyx⟩ := hx
      rcases (oneSpace_iff s).mp hs y hy with h1 | h1
      · rw [h1] at hyx; cases hyx
      · rw [hyx, hc, he] at h1; exact Option.some.inj h1
    · rw [ite_eq_right he] at h; cases h

omit [DecidableEq K] [DecidableEq M] in
/-- The legacy quarantine: a session loaded with no contract holds no vector,
so the stamp that follows cannot label a vector it did not produce. -/
theorem quarantine_no_vectors (s : Session K M) (hc : s.contract = none) :
    hasVectors (quarantine s) = false := by
  unfold quarantine; rw [hc]
  rw [hasVectors_false_iff]
  intro x hx
  simp only [List.mem_map] at hx
  obtain ⟨c, _, rfl⟩ := hx
  rfl

/-- **No mixing without the override.** From a one-space session (a fresh
one, or any session after the load quarantine), no sequence of operations
without the override lets a vector read return a vector produced under
another contract than the one the read expected. -/
theorem no_mixing_without_override (s : Session K M) (ops : List (Op K M))
    (expected : Contract K M) (l : List (Contract K M)) (hs : oneSpace s = true)
    (hops : ∀ op ∈ ops, op.overrides = false)
    (h : vectorRead (runAll s ops) expected = some l) : ∀ x ∈ l, x = expected :=
  read_one_space _ expected l (runAll_oneSpace ops s hs hops) h

/-! ### What the override lets through -/

/-- **The override's mixing case, pinned.** On a one-space session stamped
`e ≠ live` that holds vectors, a successful override attach leaves vectors
labelled `e` under the contract `live`: the session is no longer one space.
By `attach_override_ok_iff` this is exactly a same-width, same-kind
model-identifier rename; it is sound only if the operator's claim that the
two identifiers denote one vector space is true. -/
theorem attach_override_relabel_mixes (s s' : Session K M) (e live : Contract K M)
    (m : List (Mut K M)) (hc : s.contract = some e) (hne : e ≠ live)
    (hv : hasVectors s = true) (hs : oneSpace s = true)
    (h : attach s live true = some (s', m)) : oneSpace s' = false := by
  obtain ⟨hcs, hcon, _⟩ := attach_spec h
  cases ho : oneSpace s' with
  | false => rfl
  | true =>
    exfalso
    have hv' : hasVectors s = false := by
      rw [hasVectors_false_iff]
      intro x hx
      rcases (oneSpace_iff s).mp hs x hx with h1 | h1
      · exact h1
      · rw [hc] at h1
        rcases (oneSpace_iff s').mp ho x (hcs ▸ hx) with h2 | h2
        · exact h2
        · rw [hcon, h1] at h2; exact absurd (Option.some.inj h2) hne
    rw [hv] at hv'; cases hv'

/-- With the override on a session that holds no vector, nothing is mixed:
the relabel is a re-stamp of an empty space. -/
theorem attach_override_no_vectors_oneSpace (s s' : Session K M) (live : Contract K M)
    (m : List (Mut K M)) (hv : hasVectors s = false)
    (h : attach s live true = some (s', m)) : oneSpace s' = true := by
  rw [oneSpace_iff, attach_keeps_concepts s s' live true m h]
  intro x hx
  left; exact (hasVectors_false_iff s).mp hv x hx

/-! ### The `re-embed` verb -/

/-- `re-embed` refuses the override flag, whatever the session. -/
theorem reembedCmd_refuses_override (s : Session K M) (live : Contract K M)
    (missingOnly drop : Bool) : reembedCmd s live true missingOnly drop = none := by
  simp [reembedCmd]

/-- The full `re-embed` of a non-empty session is `reembed` over every text
concept, unless an image vector blocks it. -/
theorem reembedCmd_full (s : Session K M) (live : Contract K M) (drop : Bool)
    (hne : s.concepts ≠ []) :
    reembedCmd s live false false drop =
      if !(imageVectorIds s).isEmpty && !drop then none
      else reembed s live (textIds s) drop := by
  have hne' : s.concepts.isEmpty = false := by
    cases hcs : s.concepts with
    | nil => exact absurd hcs hne
    | cons _ _ => rfl
  unfold reembedCmd
  simp [hne']

/-- A successful full `re-embed` leaves the session in one space under the
live contract. -/
theorem reembedCmd_full_one_space (s s' : Session K M) (live : Contract K M) (drop : Bool)
    (m : List (Mut K M)) (hne : s.concepts ≠ [])
    (h : reembedCmd s live false false drop = some (s', m)) :
    oneSpace s' = true ∧ s'.contract = some live := by
  rw [reembedCmd_full s live drop hne] at h
  by_cases hi : (!(imageVectorIds s).isEmpty && !drop) = true
  · rw [ite_eq_left hi] at h; cases h
  · rw [ite_eq_right hi] at h
    exact ⟨reembed_one_space s s' live _ drop m h, reembed_contract s s' live _ drop m h⟩

/-- **`re-embed` cannot rewrite a session already under the live contract.**
A full re-embed of a non-empty session stamped with exactly the live contract
is refused. This is the state an embedder-canary drift (#176) is detected in:
the canary is keyed by the contract, and the attach only reached it because
the stored contract equals the live one. -/
theorem reembedCmd_refuses_same_contract (s : Session K M) (live : Contract K M)
    (drop : Bool) (hc : s.contract = some live) (hne : s.concepts ≠ []) :
    reembedCmd s live false false drop = none := by
  rw [reembedCmd_full s live drop hne]
  by_cases hi : (!(imageVectorIds s).isEmpty && !drop) = true
  · rw [ite_eq_left hi]
  · rw [ite_eq_right hi]; exact reembed_refuses_same s live _ drop hc

/-! ### The embedder canary -/

/-- `[embedder] canary`. -/
inductive CanaryMode where
  | off
  | warn
  | verify
  deriving DecidableEq, Repr

/-- `canary_gate`: whether the writer attach proceeds. `drifted` is the
verdict `Drifted`. -/
def canaryAdmits (mode : CanaryMode) (drifted allow : Bool) : Bool :=
  match mode with
  | .verify => !drifted || allow
  | _ => true

/-- Under `verify`, a drift refuses the attach unless the override is on. -/
theorem canary_verify_refuses_drift : canaryAdmits .verify true false = false := rfl

/-- The override admits a drifted embedder in every mode. -/
theorem canary_override_admits (mode : CanaryMode) (drifted : Bool) :
    canaryAdmits mode drifted true = true := by
  cases mode <;> simp [canaryAdmits]

/-- Only `verify` without the override ever refuses. -/
theorem canary_refuses_iff (mode : CanaryMode) (drifted allow : Bool) :
    canaryAdmits mode drifted allow = false ↔
      mode = .verify ∧ drifted = true ∧ allow = false := by
  cases mode <;> cases drifted <;> cases allow <;> simp [canaryAdmits]

/-- An ordinary writer attach with its canary gate (`build_attach` steps 2
and 2c): the contract check, then the canary. -/
def writerAttach (s : Session K M) (live : Contract K M) (allow : Bool) (mode : CanaryMode)
    (drifted : Bool) : Option (Session K M × List (Mut K M)) :=
  match attach s live allow with
  | some r => if canaryAdmits mode drifted allow then some r else none
  | none => none

/-- `lambo re-embed` with the canary inputs it is given. It attaches in
`reembed_mode`, which skips `canary_gate`, so `mode` and `drifted` are
accepted and unused: this is the current code, not a modelling shortcut. -/
def reembedCmdGated (s : Session K M) (live : Contract K M) (allow missingOnly drop : Bool)
    (_mode : CanaryMode) (_drifted : Bool) : Option (Session K M × List (Mut K M)) :=
  reembedCmd s live allow missingOnly drop

/-- **`--missing-only` bypasses the canary** (finding 3): the re-embed verb,
backfill included, does the same whatever the canary mode and verdict. -/
theorem missingOnly_skips_canary (s : Session K M) (live : Contract K M)
    (allow missingOnly drop : Bool) (mode : CanaryMode) (drifted : Bool) :
    reembedCmdGated s live allow missingOnly drop mode drifted =
      reembedCmdGated s live allow missingOnly drop .off false := rfl

/-- Under `verify` with a drift, every ordinary writer attach is refused. -/
theorem writerAttach_verify_drift (s : Session K M) (live : Contract K M) :
    writerAttach s live false .verify true = none := by
  unfold writerAttach
  cases attach s live false <;> rfl

/-- Whether a `re-embed` run pins the canary afterwards
(`re_embed.rs:209-215`): a full rewrite that succeeded on a non-empty
session, with the canary not `off`. -/
def repins (s : Session K M) (live : Contract K M) (missingOnly drop : Bool)
    (mode : CanaryMode) : Bool :=
  (reembedCmd s live false missingOnly drop).isSome && !missingOnly && !s.concepts.isEmpty &&
    !decide (mode = .off)

/-- Whether the run prints the "other sessions under the same contract"
warning: `prior_contract == Some(live)`. -/
def repinWarns (s : Session K M) (live : Contract K M) : Bool :=
  decide (s.contract = some live)

/-- After a successful full re-embed of a non-empty session the warning is
never printed: the session was not under the live contract. -/
theorem reembedCmd_full_never_warns (s : Session K M) (live : Contract K M) (drop : Bool)
    (r : Session K M × List (Mut K M)) (hne : s.concepts ≠ [])
    (h : reembedCmd s live false false drop = some r) : repinWarns s live = false := by
  unfold repinWarns
  cases hc : decide (s.contract = some live) with
  | false => rfl
  | true =>
    rw [decide_eq_true_eq] at hc
    rw [reembedCmd_refuses_same_contract s live drop hc hne] at h
    cases h

/-- **Silent re-pin** (finding 2): whenever a run re-pins, it does not warn. -/
theorem reembed_repins_without_warning (s : Session K M) (live : Contract K M)
    (missingOnly drop : Bool) (mode : CanaryMode)
    (h : repins s live missingOnly drop mode = true) : repinWarns s live = false := by
  unfold repins at h
  simp only [Bool.and_eq_true, Bool.not_eq_true', Option.isSome_iff_exists] at h
  obtain ⟨⟨⟨⟨r, hr⟩, hmo⟩, hne⟩, _⟩ := h
  subst hmo
  have hne' : s.concepts ≠ [] := by
    intro h0; rw [h0] at hne; cases hne
  exact reembedCmd_full_never_warns s live drop r hne' hr

/-! ### Concrete cases

Kinds and models are numbers: kind `0` and `1`, models `some 0` and
`some 1`, width `4`. -/

/-- `bge`/`m0` at width 4. -/
def cA : Contract Nat Nat := ⟨0, some 0, 4⟩
/-- Same kind and width, renamed model. -/
def cB : Contract Nat Nat := ⟨0, some 1, 4⟩
/-- Another kind at the same width. -/
def cC : Contract Nat Nat := ⟨1, some 0, 4⟩
/-- Another width. -/
def cD : Contract Nat Nat := ⟨0, some 0, 8⟩

/-- A session stamped `cA` with one text vector in its space. -/
def sA : Session Nat Nat := ⟨some cA, [⟨1, false, some cA⟩]⟩
/-- A session stamped `cA` with no vector. -/
def sEmpty : Session Nat Nat := ⟨some cA, [⟨1, false, none⟩]⟩

/-- Without the override the rename is refused; with it, it is relabelled
and recorded. -/
theorem example_rename :
    attach sA cB false = none ∧
    attach sA cB true = some (⟨some cB, sA.concepts⟩, [.setEmbedding (some cB)]) ∧
    attachWarns sA cB true = true := by decide

/-- Refused even with the override: a width change, and a kind change over
existing vectors. A kind change over an empty space is allowed. -/
theorem example_override_refusals :
    attach sA cD true = none ∧ attach sA cC true = none ∧
    (attach sEmpty cC true).isSome = true := by decide

/-- The WARN fires on the cross-kind attempt the relabel then refuses. -/
theorem example_warn_without_relabel :
    attachWarns sA cC true = true ∧ attach sA cC true = none := by decide

/-- **The override's mixing, end to end.** Rename `cA` to `cB` over an
existing vector, write one new vector, and read under `cB`: the read returns
one vector produced under `cA` and one under `cB`. -/
theorem override_mixes_reads :
    vectorRead (runAll sA [.attach cB true, .write cB 2 false true]) cB = some [cA, cB] := by
  decide

/-- The same operations without the override: the attach and the write are
refused, and the read under `cB` is refused too. -/
theorem example_no_override_no_mix :
    vectorRead (runAll sA [.attach cB false, .write cB 2 false true]) cB = none ∧
    vectorRead (runAll sA [.attach cB false, .write cB 2 false true]) cA = some [cA] := by
  decide

/-- Re-embed from `cA` to `cC` (another kind) rewrites the vector, then
swaps the contract; the read returns one space. -/
theorem example_reembed :
    reembedCmd sA cC false false false =
      some (⟨some cC, [⟨1, false, some cC⟩]⟩, [.upsert 1 (some cC), .setEmbedding (some cC)]) ∧
    vectorRead (runAll sA [.reembed cC [1] false]) cC = some [cC] := by decide

/-- Re-embed's coverage check: a duplicate id, or a missing one, is refused. -/
theorem example_reembed_coverage :
    reembed (⟨some cA, [⟨1, false, some cA⟩, ⟨2, false, some cA⟩]⟩ : Session Nat Nat) cC [1, 1]
      false = none ∧
    reembed (⟨some cA, [⟨1, false, some cA⟩, ⟨2, false, some cA⟩]⟩ : Session Nat Nat) cC [1]
      false = none := by decide

/-- Image vectors block a full re-embed unless dropped. -/
theorem example_reembed_images :
    reembed (⟨some cA, [⟨1, true, some cA⟩]⟩ : Session Nat Nat) cC [] false = none ∧
    reembed (⟨some cA, [⟨1, true, some cA⟩]⟩ : Session Nat Nat) cC [] true =
      some (⟨some cC, [⟨1, true, none⟩]⟩, [.upsert 1 none, .setEmbedding (some cC)]) := by
  decide

/-- The canary drift case: the session is under the live contract, and
`re-embed` refuses to rewrite it in place; the override flag is refused too.
The repair is a rename hop (`example_rename_hop`). -/
theorem example_drift_not_reembeddable_in_place :
    reembedCmd sA cA false false false = none ∧ reembedCmd sA cA true false false = none ∧
    canaryAdmits .verify true false = false := by decide

/-- An alias of `cA`'s model at the same kind and width (a renamed
`llama_model`). -/
def cA' : Contract Nat Nat := ⟨0, some 2, 4⟩

/-- **The undocumented repair** (finding 1): re-embed to an alias of the same
width succeeds, and re-embed back to the original name succeeds too. -/
theorem example_rename_hop :
    reembedCmd sA cA' false false false =
      some (⟨some cA', [⟨1, false, some cA'⟩]⟩, [.upsert 1 (some cA'), .setEmbedding (some cA')]) ∧
    reembedCmd ⟨some cA', [⟨1, false, some cA'⟩]⟩ cA false false false =
      some (⟨some cA, [⟨1, false, some cA⟩]⟩, [.upsert 1 (some cA), .setEmbedding (some cA)]) := by
  decide

/-- **Silent re-pin, two routes** (finding 2). The round trip's second hop
(prior `cA'`, live `cA`) and the re-embed of an unrecorded session with a
text concept (prior none) both re-pin `cA` without the warning. -/
theorem example_silent_repin :
    repins (⟨some cA', [⟨1, false, some cA'⟩]⟩ : Session Nat Nat) cA false false .verify = true ∧
    repinWarns (⟨some cA', [⟨1, false, some cA'⟩]⟩ : Session Nat Nat) cA = false ∧
    repins (⟨none, [⟨1, false, none⟩]⟩ : Session Nat Nat) cA false false .warn = true ∧
    repinWarns (⟨none, [⟨1, false, none⟩]⟩ : Session Nat Nat) cA = false := by decide

/-- **The drifted backfill** (finding 3): under `verify` with a drift, an
ordinary writer attach to `sEmpty` is refused, while `re-embed
--missing-only` writes a vector from the drifted embedder into it. -/
theorem example_drifted_backfill :
    writerAttach sEmpty cA false .verify true = none ∧
    reembedCmdGated sEmpty cA false true false .verify true =
      some (⟨some cA, [⟨1, false, some cA⟩]⟩, [.upsert 1 (some cA)]) := by decide

/-- The weak same-kind guard (finding 5): vectors from model `0` are
relabelled as model `1` of the same kind and width. -/
theorem example_same_kind_relabel :
    (attach sA cB true).isSome = true ∧ (attach sA cC true).isSome = false := by decide

/-- A backfill fills a missing vector in the session's own space and keeps
the contract; from another space it is refused. -/
theorem example_backfill :
    reembedCmd sEmpty cA false true false =
      some (⟨some cA, [⟨1, false, some cA⟩]⟩, [.upsert 1 (some cA)]) ∧
    reembedCmd sEmpty cB false true false = none := by decide

/-- An ordinary write after an override keeps the relabelled contract, and
cannot move it back. -/
theorem example_write_after_override :
    (runAll sA [.attach cB true, .write cA 2 false true]).contract = some cB := by decide

/-! # The fix for #296

Rust items modelled (at the fix commit):

* `Graph::reembed_in_place` / `reembed_in_place_dropping_image_vectors`
  (`src/graph/graph/embeddings.rs`): `reembedSame`. The same rewrite as
  `reembed` (every text concept covered once, image vectors refused unless
  dropped), but for a session that already carries the live contract and
  holds at least one vector. The contract does not move, so the batch has no
  `SetEmbedding`.
* `re_embed::run` with `--rewrite-same-contract` and `--repin-canary`
  (`src/cli/re_embed.rs`): `reembedCmdFixed`. Before it writes, a full
  rewrite reads what the store's canary pin for the live contract says about
  the live embedder (`PinState`, `canary::compare_pin`) and refuses to
  replace a pin it cannot confirm (one that drifted or could not be checked)
  unless `--repin-canary` is given.
* `--missing-only` attaches in `reembed_mode` with the configured canary, and
  the builder runs `canary_gate` in `reembed_mode` when the session already
  carries the live contract (`builder.rs`, step 2c): `gatedBackfill`. The
  full rewrites attach with the canary off, as before: they replace the space
  rather than write into it.
* `repinWarnsFixed` is the output line after such a re-pin; it is printed
  whenever the replaced pin was not confirmed, which replaces the old
  `prior == live` test that a successful rewrite never met.
* The override WARN is logged after the relabel succeeds:
  `attachWarnsFixed`.

## Results

* `reembedCmdFixed_repairs_drift`: a canary drift is now repairable in
  place: on a session under the live contract that holds a vector,
  `--rewrite-same-contract --repin-canary` succeeds whatever the canary says,
  and `reembedCmdFixed_full_spec` says every successful full run leaves every
  stored vector in the live space under the live contract.
* `reembedCmdFixed_plain_refuses_same_contract`: without the new flag the
  verb still refuses a session under the live contract, as before.
  `reembedCmdFixed_migration_agrees`: on a migration whose re-pin is safe the
  fixed verb is the old one.
* `missingOnly_fixed_refuses_drift`, `missingOnly_fixed_verify_drift_writes_nothing`:
  under `verify` with a drift the backfill refuses, and writes nothing in any
  case; `missingOnly_fixed_agrees`: otherwise it is the old backfill.
* `fixed_repin_unsafe_needs_optin`: a run that re-pins over a pin it could
  not confirm was given `--repin-canary`, and then warns.
* `reembedSame_keeps_contract`, `runAll2_keeps`, `runAll2_ordinary_keeps`,
  `contract_change_explicit2`, `runAll2_oneSpace`: the gated backfill and the
  in-place rewrite never move a stamped contract, add no new way to move it,
  and keep the session in one space.
* `attachWarnsFixed_relabelled`: the fixed WARN fires only on a relabel that
  happened.
* `gatePinAfter_only_creates`, `pinAfterFixed_changes_to_live`,
  `pinAfterFixed_no_silent_replace`: the pins the verb writes. The
  `--missing-only` gate only creates a missing pin, every pin written is the
  live embedder's, and an unconfirmed pin changes only through a full run
  given `--repin-canary`, which warns.
* `--repin-canary` under `canary = "off"` is a usage error (there is no pin
  to replace), so `reembedCmdFixed_repairs_drift` passes it only when the
  canary is on, and `reembedCmdFixed_migration_agrees` assumes it is not that
  error.

Modelling the fix forced one choice: `drifted` in the first half is one
Boolean, but the gate and the re-pin guard read the pin differently. The
attach gate admits a pin it could not check (`Unverified` is never a
refusal), while the re-pin guard must not replace a pin it could not check,
so the fix needs the four states of `PinState`.
-/

/-- What the store's canary pin for the live contract says about the live
embedder, read before a re-embed writes. -/
inductive PinState where
  /-- No usable pin: a re-pin replaces nothing. -/
  | absent
  /-- The live embedder reproduces the pin (`Verified`). -/
  | agrees
  /-- It does not (`Drifted`). -/
  | drifted
  /-- The canaries could not be embedded (`Unverified`). -/
  | unchecked
  deriving DecidableEq, Repr

/-- The verdict the attach gate reads: only a measured drift. -/
def PinState.isDrifted : PinState → Bool
  | .drifted => true
  | _ => false

/-- Replacing the pin takes nothing from the store's other sessions: there is
none, or the live embedder reproduces it. -/
def PinState.safeToReplace : PinState → Bool
  | .absent => true
  | .agrees => true
  | _ => false

/-- The in-place rewrite's checks: the session already carries the live
contract and holds a vector, then `reembed`'s image, duplicate, coverage and
membership checks. -/
def reembedSameOk (s : Session K M) (live : Contract K M) (ids : List Nat) (drop : Bool) :
    Bool :=
  decide (s.contract = some live) && hasVectors s && ((imageVectorIds s).isEmpty || drop) &&
    distinct ids && decide (ids.length = (textIds s).length) &&
      ids.all (fun i => decide (i ∈ textIds s))

/-- The in-place rewrite's batch: the upserts only. The contract does not
change, so no `SetEmbedding` (the guard `relabel` shares). -/
def reembedSameBatch (s : Session K M) (live : Contract K M) (ids : List Nat) (drop : Bool) :
    List (Mut K M) :=
  ids.map (fun i => .upsert i (some live)) ++
    (if drop then (imageVectorIds s).map (fun i => .upsert i none) else [])

/-- `Graph::reembed_in_place` (`drop`: `..._dropping_image_vectors`). -/
def reembedSame (s : Session K M) (live : Contract K M) (ids : List Nat) (drop : Bool) :
    Option (Session K M × List (Mut K M)) :=
  if reembedSameOk s live ids drop then
    some (⟨some live, rewrite s.concepts ids live drop⟩, reembedSameBatch s live ids drop)
  else none

/-- `rewrite` covering every text concept, with no image vector left behind,
puts every stored vector in the rewritten space. -/
theorem rewrite_one_space (s : Session K M) (live : Contract K M) (ids : List Nat) (drop : Bool)
    (himg : ((imageVectorIds s).isEmpty || drop) = true) (hdist : distinct ids = true)
    (hlen : ids.length = (textIds s).length) (hsub : ∀ i ∈ ids, i ∈ textIds s) :
    oneSpace (⟨some live, rewrite s.concepts ids live drop⟩ : Session K M) = true := by
  rw [oneSpace_iff]
  intro c' hc'
  simp only [rewrite, List.mem_map] at hc'
  obtain ⟨c, hc, rfl⟩ := hc'
  cases himgc : c.image with
  | false =>
    have hct : c.id ∈ textIds s := by
      simp only [textIds, List.mem_map, List.mem_filter]
      exact ⟨c, ⟨hc, by simp [himgc]⟩, rfl⟩
    have hcov := cover_of_distinct ids (textIds s) hdist hsub hlen c.id hct
    simp [hcov]
  | true =>
    cases drop with
    | true => simp
    | false =>
      simp only [Bool.or_false] at himg
      have hnone : c.vec = none := by
        cases hv : c.vec with
        | none => rfl
        | some v =>
          have hmem : c.id ∈ imageVectorIds s := by
            simp only [imageVectorIds, List.mem_map, List.mem_filter]
            exact ⟨c, ⟨hc, by simp [himgc, hv]⟩, rfl⟩
          rw [List.isEmpty_iff] at himg
          rw [himg] at hmem; cases hmem
      simp [hnone]

/-- Everything a successful in-place rewrite guarantees: the session was
already under the live contract and stays under it, every stored vector is in
the live space, and the batch is upserts only. -/
theorem reembedSame_spec {s s' : Session K M} {live : Contract K M} {ids : List Nat}
    {drop : Bool} {m : List (Mut K M)} (h : reembedSame s live ids drop = some (s', m)) :
    s.contract = some live ∧ hasVectors s = true ∧ s'.contract = some live ∧
      oneSpace s' = true ∧ ∀ x ∈ m, x.isUpsert = true := by
  unfold reembedSame at h
  split at h
  · rename_i hg
    unfold reembedSameOk at hg
    simp only [Bool.and_eq_true, decide_eq_true_eq, List.all_eq_true] at hg
    obtain ⟨⟨⟨⟨⟨hc, hv⟩, himg⟩, hdist⟩, hlen⟩, hsub⟩ := hg
    simp only [Option.some.injEq, Prod.mk.injEq] at h
    obtain ⟨rfl, rfl⟩ := h
    refine ⟨hc, hv, rfl, rewrite_one_space s live ids drop himg hdist hlen hsub, ?_⟩
    intro x hx
    simp only [reembedSameBatch, List.mem_append, List.mem_map] at hx
    rcases hx with ⟨i, _, rfl⟩ | hx
    · rfl
    · cases drop with
      | true =>
        simp only [ite_true, List.mem_map] at hx
        obtain ⟨i, _, rfl⟩ := hx
        rfl
      | false => simp at hx
  · cases h

/-- The in-place rewrite never moves the contract. -/
theorem reembedSame_keeps_contract (s s' : Session K M) (live : Contract K M) (ids : List Nat)
    (drop : Bool) (m : List (Mut K M)) (h : reembedSame s live ids drop = some (s', m)) :
    s'.contract = s.contract := by
  obtain ⟨hc, _, hc', _⟩ := reembedSame_spec h
  rw [hc, hc']

/-- The fixed `lambo re-embed`. `same` is `--rewrite-same-contract`, `repin`
is `--repin-canary`; `mode` is `[embedder] canary` and `pin` what the store's
pin for the live contract says about the live embedder. In the Rust's order:
the usage checks, the attach (whose canary gate runs only for the backfill of
a session under the live contract), the image and empty checks, the
backfill, then the full rewrite's contract precondition and re-pin guard. -/
def reembedCmdFixed (s : Session K M) (live : Contract K M)
    (allow missingOnly drop same repin : Bool) (mode : CanaryMode) (pin : PinState) :
    Option (Session K M × List (Mut K M)) :=
  if allow then none
  else if missingOnly && (drop || same || repin) then none
  else if repin && decide (mode = .off) then none
  else if missingOnly && decide (s.contract = some live) &&
      !canaryAdmits mode pin.isDrifted false then none
  else if !missingOnly && !(imageVectorIds s).isEmpty && !drop then none
  else if s.concepts.isEmpty then some (s, [])
  else if missingOnly then
    let ids := (s.concepts.filter (fun c => !c.image && c.vec.isNone)).map (·.id)
    if ids.isEmpty then some (s, []) else backfill s live ids
  else if same != decide (s.contract = some live) then none
  else if !(decide (mode = .off) || repin || pin.safeToReplace) then none
  else if same then reembedSame s live (textIds s) drop
  else reembed s live (textIds s) drop

/-- **Every successful full run ends in one space.** A full run of the fixed
verb that succeeds on a non-empty session, in-place or a migration, leaves
every stored vector in the live space under the live contract. -/
theorem reembedCmdFixed_full_spec (s s' : Session K M) (live : Contract K M)
    (drop same repin : Bool) (mode : CanaryMode) (pin : PinState) (m : List (Mut K M))
    (hne : s.concepts ≠ [])
    (h : reembedCmdFixed s live false false drop same repin mode pin = some (s', m)) :
    oneSpace s' = true ∧ s'.contract = some live := by
  have hne' : s.concepts.isEmpty = false := by
    cases hcs : s.concepts with
    | nil => exact absurd hcs hne
    | cons _ _ => rfl
  unfold reembedCmdFixed at h
  simp only [Bool.false_eq_true, ite_false, Bool.false_and, hne', Bool.not_false,
    Bool.true_and] at h
  split at h
  · cases h
  · split at h
    · cases h
    · split at h
      · cases h
      · split at h
        · cases h
        · split at h
          · obtain ⟨_, _, hc', hs', _⟩ := reembedSame_spec h
            exact ⟨hs', hc'⟩
          · exact ⟨reembed_one_space s s' live _ drop m h, reembed_contract s s' live _ drop m h⟩

/-- **The drift is repairable in place** (finding 1 fixed). On a non-empty
session under the live contract that holds a vector, with no image vector
left behind and distinct node ids, `re-embed --rewrite-same-contract
--repin-canary` succeeds whatever the canary mode and pin say (under
`canary = "off"` there is no pin to replace and `--repin-canary` is a usage
error, so there the repair is the first flag alone), and (by
`reembedCmdFixed_full_spec`) leaves every vector in the live space. -/
theorem reembedCmdFixed_repairs_drift (s : Session K M) (live : Contract K M) (drop : Bool)
    (mode : CanaryMode) (pin : PinState) (hc : s.contract = some live)
    (hv : hasVectors s = true) (himg : ((imageVectorIds s).isEmpty || drop) = true)
    (hdist : distinct (textIds s) = true) (hne : s.concepts ≠ []) :
    ∃ r, reembedCmdFixed s live false false drop true (!decide (mode = .off)) mode pin = some r ∧
      oneSpace r.1 = true ∧ r.1.contract = some live := by
  have hne' : s.concepts.isEmpty = false := by
    cases hcs : s.concepts with
    | nil => exact absurd hcs hne
    | cons _ _ => rfl
  have hok : reembedSameOk s live (textIds s) drop = true := by
    simp [reembedSameOk, hc, hv, himg, hdist]
  have himg' : (!(imageVectorIds s).isEmpty && !drop) = false := by
    cases drop <;> simp_all
  refine ⟨(⟨some live, rewrite s.concepts (textIds s) live drop⟩,
    reembedSameBatch s live (textIds s) drop), ?_, ?_, rfl⟩
  · cases hm : decide (mode = .off) <;>
      simp_all [reembedCmdFixed, reembedSame, PinState.safeToReplace]
  · exact rewrite_one_space s live (textIds s) drop himg hdist rfl (fun _ h => h)

/-- **Agreement: the plain verb still refuses a same-contract session.**
Without `--rewrite-same-contract`, a full run on a non-empty session under
the live contract is refused, as `reembedCmd_refuses_same_contract` says of
the old verb. -/
theorem reembedCmdFixed_plain_refuses_same_contract (s : Session K M) (live : Contract K M)
    (drop repin : Bool) (mode : CanaryMode) (pin : PinState) (hc : s.contract = some live)
    (hne : s.concepts ≠ []) :
    reembedCmdFixed s live false false drop false repin mode pin = none := by
  have hne' : s.concepts.isEmpty = false := by
    cases hcs : s.concepts with
    | nil => exact absurd hcs hne
    | cons _ _ => rfl
  cases drop <;> simp [reembedCmdFixed, hne', hc]

/-- **Agreement: a safe migration is unchanged.** On a session not under the
live contract, when the re-pin replaces nothing other sessions rely on (or
the canary is off, or `--repin-canary` is given), the fixed full verb is the
old one. -/
theorem reembedCmdFixed_migration_agrees (s : Session K M) (live : Contract K M)
    (allow drop repin : Bool) (mode : CanaryMode) (pin : PinState)
    (hc : s.contract ≠ some live)
    (hsafe : (decide (mode = .off) || repin || pin.safeToReplace) = true)
    (husage : (repin && decide (mode = .off)) = false) :
    reembedCmdFixed s live allow false drop false repin mode pin = reembedCmd s live allow false drop := by
  unfold reembedCmdFixed reembedCmd
  simp only [hc, decide_false, bne_self_eq_false, hsafe, husage, Bool.not_true,
    Bool.false_eq_true, ite_false, Bool.false_and, Bool.not_false, Bool.true_and]

/-- **`--missing-only` runs the canary** (finding 3 fixed): under `verify`
with a drift, the backfill of a session under the live contract is refused,
like every ordinary writer attach (`writerAttach_verify_drift`). -/
theorem missingOnly_fixed_refuses_drift (s : Session K M) (live : Contract K M)
    (drop same repin : Bool) (hc : s.contract = some live) :
    reembedCmdFixed s live false true drop same repin .verify .drifted = none := by
  cases drop <;> cases same <;> cases repin <;>
    simp [reembedCmdFixed, hc, canaryAdmits, PinState.isDrifted]

/-- Under `verify` with a drift the backfill writes nothing, whatever the
session's contract: it is refused, or it is a no-op. -/
theorem missingOnly_fixed_verify_drift_writes_nothing (s : Session K M) (live : Contract K M)
    (r : Session K M × List (Mut K M))
    (h : reembedCmdFixed s live false true false false false .verify .drifted = some r) :
    r = (s, []) := by
  by_cases hc : s.contract = some live
  · rw [missingOnly_fixed_refuses_drift s live false false false hc] at h; cases h
  · have hbf := backfill_needs_same_contract s live
      ((s.concepts.filter (fun c => !c.image && c.vec.isNone)).map (·.id)) hc
    unfold reembedCmdFixed at h
    by_cases he : s.concepts.isEmpty = true
    · simp [hc, he] at h; exact h.symm
    · by_cases hi : ((s.concepts.filter (fun c => !c.image && c.vec.isNone)).map (·.id)).isEmpty = true
      · simp [hc, he, hi] at h; exact h.symm
      · simp [hc, he, hi, hbf] at h

/-- **Agreement: an admitted backfill is unchanged.** When the canary admits
(any mode but `verify`, or no drift), `--missing-only` is the old backfill. -/
theorem missingOnly_fixed_agrees (s : Session K M) (live : Contract K M) (allow : Bool)
    (mode : CanaryMode) (pin : PinState) (hadm : canaryAdmits mode pin.isDrifted false = true) :
    reembedCmdFixed s live allow true false false false mode pin =
      reembedCmd s live allow true false := by
  unfold reembedCmdFixed reembedCmd
  simp [hadm]

/-- Whether a fixed run pins the canary afterwards: a full run that
succeeded on a non-empty session, with the canary not `off`. -/
def repinsFixed (s : Session K M) (live : Contract K M) (missingOnly drop same repin : Bool)
    (mode : CanaryMode) (pin : PinState) : Bool :=
  (reembedCmdFixed s live false missingOnly drop same repin mode pin).isSome && !missingOnly &&
    !s.concepts.isEmpty && !decide (mode = .off)

/-- Whether the fixed run prints the line that other sessions under the
contract are no longer flagged: whenever it re-pinned over a pin it could
not confirm (which, by `fixed_repin_unsafe_needs_optin`, took
`--repin-canary`). -/
def repinWarnsFixed (s : Session K M) (live : Contract K M) (missingOnly drop same repin : Bool)
    (mode : CanaryMode) (pin : PinState) : Bool :=
  repinsFixed s live missingOnly drop same repin mode pin && !pin.safeToReplace

/-- **No silent re-pin** (finding 2 fixed): a run that re-pins over a pin it
could not confirm (drifted, or unchecked) was given `--repin-canary`, and
prints the warning. -/
theorem fixed_repin_unsafe_needs_optin (s : Session K M) (live : Contract K M)
    (missingOnly drop same repin : Bool) (mode : CanaryMode) (pin : PinState)
    (h : repinsFixed s live missingOnly drop same repin mode pin = true)
    (hunsafe : pin.safeToReplace = false) :
    repin = true ∧ repinWarnsFixed s live missingOnly drop same repin mode pin = true := by
  have hr : repin = true := by
    unfold repinsFixed at h
    simp only [Bool.and_eq_true, Bool.not_eq_true', decide_eq_false_iff_not,
      Option.isSome_iff_exists] at h
    obtain ⟨⟨⟨⟨r, hr⟩, hmo⟩, hne⟩, hoff⟩ := h
    subst hmo
    cases repin with
    | true => rfl
    | false =>
      exfalso
      have hoff' : decide (mode = .off) = false := by simpa using hoff
      unfold reembedCmdFixed at hr
      cases same <;> cases drop <;>
        simp [hne, hunsafe, hoff'] at hr <;> split at hr <;> simp_all
  subst hr
  exact ⟨rfl, by simp [repinWarnsFixed, h, hunsafe]⟩

/-! ### The fixed operations keep the contract and one space -/

/-- The backfill behind the builder's canary gate. -/
def gatedBackfill (s : Session K M) (live : Contract K M) (ids : List Nat) (mode : CanaryMode)
    (pin : PinState) : Option (Session K M × List (Mut K M)) :=
  if decide (s.contract = some live) && !canaryAdmits mode pin.isDrifted false then none
  else backfill s live ids

/-- The session-level operations after the fix. -/
inductive Op2 (K M : Type) where
  | base (op : Op K M)
  | backfillGated (live : Contract K M) (ids : List Nat) (mode : CanaryMode) (pin : PinState)
  | reembedSame (live : Contract K M) (ids : List Nat) (drop : Bool)

/-- Ordinary operations: the old ordinary ones without the override, and the
gated backfill. -/
def Op2.ordinary : Op2 K M → Bool
  | .base op => op.ordinary && !op.overrides
  | .backfillGated .. => true
  | .reembedSame .. => false

/-- The operations that never move a stamped contract: the ordinary ones and
the in-place rewrite. -/
def Op2.keepsContract : Op2 K M → Bool
  | .reembedSame .. => true
  | op => op.ordinary

/-- Whether an operation carries the operator override. -/
def Op2.overrides : Op2 K M → Bool
  | .base op => op.overrides
  | _ => false

/-- One operation after the fix. -/
def step2 (s : Session K M) : Op2 K M → Option (Session K M × List (Mut K M))
  | .base op => step s op
  | .backfillGated live ids mode pin => gatedBackfill s live ids mode pin
  | .reembedSame live ids drop => reembedSame s live ids drop

/-- A sequence of operations after the fix; a refused one leaves the session
as it was. -/
def runAll2 (s : Session K M) : List (Op2 K M) → Session K M
  | [] => s
  | op :: rest =>
    match step2 s op with
    | some (s', _) => runAll2 s' rest
    | none => runAll2 s rest

/-- A gated backfill that succeeds is the plain backfill. -/
theorem gatedBackfill_eq {s : Session K M} {live : Contract K M} {ids : List Nat}
    {mode : CanaryMode} {pin : PinState} {r : Session K M × List (Mut K M)}
    (h : gatedBackfill s live ids mode pin = some r) : backfill s live ids = some r := by
  unfold gatedBackfill at h
  split at h
  · cases h
  · exact h

/-- One contract-keeping operation keeps a stamped contract. -/
theorem step2_keeps (s s' : Session K M) (c : Contract K M) (op : Op2 K M) (m : List (Mut K M))
    (hc : s.contract = some c) (hop : op.keepsContract = true) (h : step2 s op = some (s', m)) :
    s'.contract = some c := by
  cases op with
  | base op =>
    have hord : op.ordinary = true ∧ op.overrides = false := by
      simp only [Op2.keepsContract, Op2.ordinary, Bool.and_eq_true, Bool.not_eq_true'] at hop
      exact hop
    have := runAll_ordinary_keeps c [op] s hc (fun o ho => by simp at ho; subst ho; exact hord)
    simp only [runAll, step2] at this h
    rw [h] at this
    exact this
  | backfillGated live ids mode pin =>
    rw [backfill_keeps_contract s s' live ids m (gatedBackfill_eq h), hc]
  | reembedSame live ids drop =>
    rw [reembedSame_keeps_contract s s' live ids drop m h, hc]

/-- **Ordinary operations keep a stamped contract, after the fix**
(`runAll_ordinary_keeps` for the fixed operations): no sequence of ordinary
operations without the override, gated backfills and in-place rewrites
changes it. -/
theorem runAll2_keeps (c : Contract K M) :
    ∀ (ops : List (Op2 K M)) (s : Session K M), s.contract = some c →
      (∀ op ∈ ops, op.keepsContract = true) → (runAll2 s ops).contract = some c := by
  intro ops
  induction ops with
  | nil => intro s hc _; exact hc
  | cons op rest ih =>
    intro s hc hops
    have hrest : ∀ op ∈ rest, op.keepsContract = true := fun o ho => hops o (by simp [ho])
    unfold runAll2
    split
    · rename_i s' m hstep
      exact ih s' (step2_keeps s s' c op m hc (hops op (by simp)) hstep) hrest
    · exact ih s hc hrest

/-- The ordinary operations alone. -/
theorem runAll2_ordinary_keeps (c : Contract K M) (ops : List (Op2 K M)) (s : Session K M)
    (hc : s.contract = some c) (hops : ∀ op ∈ ops, op.ordinary = true) :
    (runAll2 s ops).contract = some c :=
  runAll2_keeps c ops s hc (fun op ho => by
    have := hops op ho
    cases op <;> simp_all [Op2.keepsContract])

/-- **The fix adds no way to move the contract.** If one fixed operation
changes a stamped contract, it is an old operation, so
`contract_change_explicit` applies to it. -/
theorem contract_change_explicit2 (s s' : Session K M) (c : Contract K M) (op : Op2 K M)
    (m : List (Mut K M)) (hc : s.contract = some c) (h : step2 s op = some (s', m))
    (hch : s'.contract ≠ some c) : ∃ op', op = .base op' ∧ step s op' = some (s', m) := by
  cases op with
  | base op' => exact ⟨op', rfl, h⟩
  | backfillGated live ids mode pin =>
    exact absurd (step2_keeps s s' c _ m hc rfl h) hch
  | reembedSame live ids drop =>
    exact absurd (step2_keeps s s' c _ m hc rfl h) hch

/-- Each fixed operation without the override keeps the session in one
space. -/
theorem step2_oneSpace (s s' : Session K M) (op : Op2 K M) (m : List (Mut K M))
    (hop : op.overrides = false) (hs : oneSpace s = true) (h : step2 s op = some (s', m)) :
    oneSpace s' = true := by
  cases op with
  | base op => exact step_oneSpace s s' op m hop hs h
  | backfillGated live ids mode pin =>
    exact step_oneSpace s s' (.backfill live ids) m rfl hs (gatedBackfill_eq h)
  | reembedSame live ids drop => exact (reembedSame_spec h).2.2.2.1

/-- **No mixing without the override, after the fix.** -/
theorem runAll2_oneSpace :
    ∀ (ops : List (Op2 K M)) (s : Session K M), oneSpace s = true →
      (∀ op ∈ ops, op.overrides = false) → oneSpace (runAll2 s ops) = true := by
  intro ops
  induction ops with
  | nil => intro s hs _; exact hs
  | cons op rest ih =>
    intro s hs hops
    have hrest : ∀ op ∈ rest, op.overrides = false := fun o ho => hops o (by simp [ho])
    unfold runAll2
    split
    · rename_i s' m hstep
      exact ih s' (step2_oneSpace s s' op m (hops op (by simp)) hs hstep) hrest
    · exact ih s hs hrest

/-! ### The pins the fixed verb writes

`PinState` above is what the store's pin says about the live embedder when
the verb starts. The verb also writes pins: the `--missing-only` attach gate
is `canary_gate`, whose `canary::check` pins a contract that has no usable
pin (`write_fingerprint_if_absent`), and a full run that succeeded pins again
from the live embedder once its rewrite is durable. Either new pin is the
live embedder's, so afterwards it `agrees`. (When the canaries cannot be
embedded the real gate writes nothing and the pin stays absent; the model's
gate always pins, which only adds changes, so the safety results below hold
of the code a fortiori.) -/

/-- The store's pin after the `--missing-only` attach gate (`gatedBackfill`):
the gate runs on a session under the live contract with the canary on, and
pins an absent pin. A drifted or unchecked pin is left as it is: `check`
keeps the old fingerprint on a drift and writes none when it cannot embed. -/
def gatePinAfter (s : Session K M) (live : Contract K M) (mode : CanaryMode) (pin : PinState) :
    PinState :=
  if !decide (mode = .off) && decide (s.contract = some live) && pin == .absent then .agrees
  else pin

/-- **The gate only creates a pin.** It never replaces one: when the pin
changes, there was none and the gate's is the live embedder's. -/
theorem gatePinAfter_only_creates (s : Session K M) (live : Contract K M) (mode : CanaryMode)
    (pin : PinState) (h : gatePinAfter s live mode pin ≠ pin) :
    pin = .absent ∧ gatePinAfter s live mode pin = .agrees := by
  unfold gatePinAfter at h ⊢
  by_cases hg : (!decide (mode = .off) && decide (s.contract = some live) && pin == .absent) = true
  · rw [ite_eq_left hg]
    simp only [Bool.and_eq_true, beq_iff_eq] at hg
    exact ⟨hg.2, rfl⟩
  · rw [ite_eq_right hg] at h; exact absurd rfl h

/-- The store's pin after a fixed run. A usage refusal (before the attach)
and `canary = "off"` write nothing; `--missing-only` writes what its gate
writes; a full run that succeeded on a non-empty session pins again. -/
def pinAfterFixed (s : Session K M) (live : Contract K M) (allow missingOnly drop same repin : Bool)
    (mode : CanaryMode) (pin : PinState) : PinState :=
  if allow || (missingOnly && (drop || same || repin)) || decide (mode = .off) then pin
  else if missingOnly then gatePinAfter s live mode pin
  else if repinsFixed s live false drop same repin mode pin then .agrees
  else pin

/-- A fixed run only ever leaves the live embedder's pin behind: when the
pin changes, it now agrees. -/
theorem pinAfterFixed_changes_to_live (s : Session K M) (live : Contract K M)
    (allow missingOnly drop same repin : Bool) (mode : CanaryMode) (pin : PinState)
    (h : pinAfterFixed s live allow missingOnly drop same repin mode pin ≠ pin) :
    pinAfterFixed s live allow missingOnly drop same repin mode pin = .agrees := by
  unfold pinAfterFixed at h ⊢
  by_cases hu : (allow || (missingOnly && (drop || same || repin)) || decide (mode = .off)) = true
  · rw [ite_eq_left hu] at h; exact absurd rfl h
  · rw [ite_eq_right hu] at h ⊢
    cases missingOnly with
    | true => exact (gatePinAfter_only_creates s live mode pin h).2
    | false =>
      simp only [Bool.false_eq_true, ite_false] at h ⊢
      by_cases hr : repinsFixed s live false drop same repin mode pin = true
      · rw [ite_eq_left hr]
      · rw [ite_eq_right hr] at h; exact absurd rfl h

/-- **No pin is replaced silently, the gate included.** If a fixed run
changes a pin it could not confirm (drifted, or unchecked), it was a full run
given `--repin-canary`, and it printed the warning (`repinWarnsFixed`). The
`--missing-only` gate can only create a missing pin (`gatePinAfter_only_creates`). -/
theorem pinAfterFixed_no_silent_replace (s : Session K M) (live : Contract K M)
    (allow missingOnly drop same repin : Bool) (mode : CanaryMode) (pin : PinState)
    (hunsafe : pin.safeToReplace = false)
    (h : pinAfterFixed s live allow missingOnly drop same repin mode pin ≠ pin) :
    missingOnly = false ∧ repin = true ∧
      repinWarnsFixed s live false drop same repin mode pin = true := by
  unfold pinAfterFixed at h
  by_cases hu : (allow || (missingOnly && (drop || same || repin)) || decide (mode = .off)) = true
  · rw [ite_eq_left hu] at h; exact absurd rfl h
  · rw [ite_eq_right hu] at h
    cases missingOnly with
    | true =>
      have := (gatePinAfter_only_creates s live mode pin h).1
      subst this; simp [PinState.safeToReplace] at hunsafe
    | false =>
      simp only [Bool.false_eq_true, ite_false] at h
      by_cases hr : repinsFixed s live false drop same repin mode pin = true
      · exact ⟨rfl, fixed_repin_unsafe_needs_optin s live false drop same repin mode pin hr hunsafe⟩
      · rw [ite_eq_right hr] at h; exact absurd rfl h

/-- The pins of the concrete cases: the `--missing-only` gate pins an absent
pin (not under `off`) and leaves a drifted one under `warn`; the drift repair
replaces the drifted pin; without `--repin-canary` the refused migration
leaves it. -/
theorem example_pins_written :
    pinAfterFixed sEmpty cA false true false false false .warn .absent = .agrees ∧
    pinAfterFixed sEmpty cA false true false false false .off .absent = .absent ∧
    pinAfterFixed sEmpty cA false true false false false .warn .drifted = .drifted ∧
    pinAfterFixed sA cA false false false true true .verify .drifted = .agrees ∧
    pinAfterFixed ⟨some cA', [⟨1, false, some cA'⟩]⟩ cA false false false false false .warn
      .drifted = .drifted := by decide

/-! ### The override WARN after the fix -/

/-- The fixed WARN: logged only once the relabel succeeded. -/
def attachWarnsFixed (s : Session K M) (live : Contract K M) (allow : Bool) : Bool :=
  attachWarns s live allow && (attach s live allow).isSome

/-- The fixed WARN fires only on a relabel that happened, which emitted the
one `SetEmbedding`. -/
theorem attachWarnsFixed_relabelled (s : Session K M) (live : Contract K M) (allow : Bool)
    (h : attachWarnsFixed s live allow = true) :
    ∃ e s' m, s.contract = some e ∧ e ≠ live ∧ attach s live allow = some (s', m) ∧
      m = [.setEmbedding (some live)] := by
  unfold attachWarnsFixed at h
  simp only [Bool.and_eq_true, Option.isSome_iff_exists] at h
  obtain ⟨hw, ⟨⟨s', m⟩, ha⟩⟩ := h
  obtain ⟨_, _, e, hc, _⟩ := (attachWarns_iff s live allow).mp hw
  have hne : e ≠ live := by
    intro he
    unfold attachWarns at hw
    rw [hc] at hw
    simp [he] at hw
  exact ⟨e, s', m, hc, hne, ha, ((attach_spec ha).2.2 e hc hne).2.2.2⟩

/-! ### Concrete cases after the fix -/

/-- **The drift repaired in place.** On `sA` under `cA` with a drifted pin:
the plain verb still refuses; `--rewrite-same-contract` without
`--repin-canary` is refused by the re-pin guard; with both it rewrites the
vector under `cA` with no `SetEmbedding`; and with a pin that agrees the
opt-in is not needed. -/
theorem example_drift_repaired_in_place :
    reembedCmdFixed sA cA false false false false true .verify .drifted = none ∧
    reembedCmdFixed sA cA false false false true false .verify .drifted = none ∧
    reembedCmdFixed sA cA false false false true true .verify .drifted =
      some (⟨some cA, [⟨1, false, some cA⟩]⟩, [.upsert 1 (some cA)]) ∧
    (reembedCmdFixed sA cA false false false true false .warn .agrees).isSome = true := by
  decide

/-- `--rewrite-same-contract` is refused on a session not under the live
contract (that is a migration), and on one with no vector (that is a
backfill). -/
theorem example_same_contract_preconditions :
    reembedCmdFixed sA cC false false false true false .off .absent = none ∧
    reembedCmdFixed sEmpty cA false false false true false .off .absent = none := by
  decide

/-- **The drifted backfill, fixed.** Under `verify` with a drift,
`--missing-only` on `sEmpty` is refused like the ordinary writer attach;
under `warn` it backfills as before. -/
theorem example_drifted_backfill_fixed :
    writerAttach sEmpty cA false .verify true = none ∧
    reembedCmdFixed sEmpty cA false true false false false .verify .drifted = none ∧
    reembedCmdFixed sEmpty cA false true false false false .warn .drifted =
      some (⟨some cA, [⟨1, false, some cA⟩]⟩, [.upsert 1 (some cA)]) := by decide

/-- **The re-pin guard on the round trip's second hop** (prior `cA'`, live
`cA`). Over a drifted or unchecked pin it is refused without
`--repin-canary`; with it, it re-pins and warns; over a pin that agrees, or
none, it re-pins without the opt-in (and without the warning, opt-in or
not), replacing nothing other sessions rely on. -/
theorem example_repin_guard :
    reembedCmdFixed ⟨some cA', [⟨1, false, some cA'⟩]⟩ cA false false false false false
      .verify .drifted = none ∧
    reembedCmdFixed ⟨some cA', [⟨1, false, some cA'⟩]⟩ cA false false false false false
      .warn .unchecked = none ∧
    repinWarnsFixed ⟨some cA', [⟨1, false, some cA'⟩]⟩ cA false false false true .verify
      .drifted = true ∧
    repinsFixed ⟨some cA', [⟨1, false, some cA'⟩]⟩ cA false false false false .warn
      .agrees = true ∧
    repinWarnsFixed ⟨some cA', [⟨1, false, some cA'⟩]⟩ cA false false false true .warn
      .agrees = false ∧
    repinsFixed (⟨none, [⟨1, false, none⟩]⟩ : Session Nat Nat) cA false false false false .warn
      .absent = true ∧
    (reembedCmdFixed ⟨some cA', [⟨1, false, some cA'⟩]⟩ cA false false false false false
      .off .drifted).isSome = true := by decide

/-- The override WARN no longer fires on the cross-kind attempt the relabel
refuses; it still fires on the rename that happens. -/
theorem example_warn_only_after_relabel :
    attachWarnsFixed sA cC true = false ∧ attachWarnsFixed sA cB true = true := by decide

/-! # The durable re-embed of a contract-less session (#322)

Everything above reads a re-embed's result off the session it returns, which
is the in-memory graph. The store applies the batch, and the store's rules
are not the graph's. This part models the store applying a flushed batch.

Rust items modelled (at the fix commit):

* Applying one flushed batch, in its own order (`MemoryStore::flush`,
  `src/store/memory.rs`; SQLite and Postgres through
  `crate::store::batch::plan_flush`, which makes every `SetEmbedding` a
  barrier, `src/store/sqlite/write_rows.rs` and `src/store/pg/write_rows.rs`):
  `applyBatch`. An upsert replaces the concept's vector (`applyMut`). A
  `SetEmbedding` of a contract on a store with none is also the **first-stamp
  legacy quarantine**: every vector in the session is nulled, then the
  contract is stamped (`memory.rs` `SetEmbedding` arm; SQLite `set_embedding`;
  Postgres `QUARANTINE_LEGACY_EMBEDDINGS_SQL`), in the same transaction.
  SQLite also nulls on a width change; a re-embed never changes width
  (`reembed_refuses_width`), so that rule is left out.
* `quarantine` (above) is the load: the graph of a session with no durable
  contract holds no vector, while the store's rows keep theirs.
* `Graph::reembed` after the fix (`src/graph/graph/embeddings.rs`):
  `reembedOrdered`. On a session with no contract it emits the
  `SetEmbedding` **first**, so the store's quarantine runs before the new
  vectors land; on a stamped session the batch is unchanged (vectors first,
  contract last).
* `lambo re-embed` after the fix: `reembedCmd322`, `reembedCmdFixed` with
  `reembedOrdered` in place of `reembed`.

## The bug (`reembed_contractless_durable_loses_vectors`)

On a contract-less session the old batch is the upserts, then
`SetEmbedding`. The store writes the new vectors, then the first stamp nulls
every vector, the new ones included. The graph says every text concept
carries a vector under the live contract (the coverage report reads the
graph: N/N); the store holds the live contract and no vector at all
(`example_contractless_bug`).

## Results

* `reembedOrdered_contractless_durable`, `reembedCmd322_contractless_durable`:
  after a successful full re-embed of a non-empty contract-less session, the
  store is under the live contract, holds one space, and every text concept
  holds a vector of the live contract.
* `reembedOrdered_contractless_matches_graph`: with distinct node ids (the
  graph's node map makes them distinct), the store after the batch is exactly
  the graph the re-embed returns, so the coverage the verb reports is the
  store's.
* `reembedOrdered_stamped_eq`, `reembedCmd322_stamped_agrees`: on a session
  that already has a contract nothing changes, the batch included.
  `reembedOrdered_graph_eq`: in every case the graph is the old one; only the
  batch order moved (`reembedBatchOrdered_perm`), with the `SetEmbedding`
  first on a contract-less session (`reembedOrdered_contract_first`).
* `missingOnly_contractless_writes_nothing`: `--missing-only` has no such
  hazard. On a contract-less session the backfill is refused (it never
  stamps a contract), so it writes nothing.

Modelling the store forced one observation: the old theorems are all true of
the graph, and `reembed_contract_last` ("vectors first, contract last") was
read as a durability argument. It is one only for a session that already has
a contract. For the first stamp the order has to be the other way round, the
same order an ordinary first write already uses (`write` emits its
`SetEmbedding` before its upsert).

Not modelled: the tier's index (`Tiered.lean`). With the `SetEmbedding`
first, its projection meets the contract before the upserts and indexes them
under it, as for any first write.
-/

/-- One mutation applied by the store. A contract stamped on a store with no
contract nulls every vector first (the first-stamp legacy quarantine). -/
def applyMut (d : Session K M) : Mut K M → Session K M
  | .upsert id v =>
    { d with concepts := d.concepts.map (fun c => if c.id = id then { c with vec := v } else c) }
  | .setEmbedding c =>
    { contract := c,
      concepts := if d.contract.isNone && c.isSome
        then d.concepts.map (fun x => { x with vec := none }) else d.concepts }

/-- One flushed batch, applied in order in one transaction. -/
def applyBatch (d : Session K M) (m : List (Mut K M)) : Session K M := m.foldl applyMut d

/-- The fixed batch: on a session with no contract, the contract first. -/
def reembedBatchOrdered (s : Session K M) (live : Contract K M) (ids : List Nat) (drop : Bool) :
    List (Mut K M) :=
  if s.contract = none then
    .setEmbedding (some live) :: (ids.map (fun i => .upsert i (some live)) ++
      (if drop then (imageVectorIds s).map (fun i => .upsert i none) else []))
  else reembedBatch s live ids drop

/-- `Graph::reembed` after the fix: the same checks and graph, the fixed batch. -/
def reembedOrdered (s : Session K M) (live : Contract K M) (ids : List Nat) (drop : Bool) :
    Option (Session K M × List (Mut K M)) :=
  if reembedOk s live ids drop then
    some (⟨some live, rewrite s.concepts ids live drop⟩, reembedBatchOrdered s live ids drop)
  else none

/-- `lambo re-embed` after the fix for #322: `reembedCmdFixed` with
`reembedOrdered`. -/
def reembedCmd322 (s : Session K M) (live : Contract K M)
    (allow missingOnly drop same repin : Bool) (mode : CanaryMode) (pin : PinState) :
    Option (Session K M × List (Mut K M)) :=
  if allow then none
  else if missingOnly && (drop || same || repin) then none
  else if repin && decide (mode = .off) then none
  else if missingOnly && decide (s.contract = some live) &&
      !canaryAdmits mode pin.isDrifted false then none
  else if !missingOnly && !(imageVectorIds s).isEmpty && !drop then none
  else if s.concepts.isEmpty then some (s, [])
  else if missingOnly then
    let ids := (s.concepts.filter (fun c => !c.image && c.vec.isNone)).map (·.id)
    if ids.isEmpty then some (s, []) else backfill s live ids
  else if same != decide (s.contract = some live) then none
  else if !(decide (mode = .off) || repin || pin.safeToReplace) then none
  else if same then reembedSame s live (textIds s) drop
  else reembedOrdered s live (textIds s) drop

/-! ### The store applying a batch -/

omit [DecidableEq K] [DecidableEq M] in
theorem applyBatch_append (d : Session K M) (a b : List (Mut K M)) :
    applyBatch d (a ++ b) = applyBatch (applyBatch d a) b := by
  simp [applyBatch, List.foldl_append]

omit [DecidableEq K] [DecidableEq M] in
/-- Upserts of one vector `v` set it on every listed concept. -/
theorem applyBatch_upserts (v : Option (Contract K M)) :
    ∀ (ids : List Nat) (d : Session K M),
      applyBatch d (ids.map (fun i => .upsert i v)) =
        ({ d with concepts :=
            d.concepts.map (fun c => if c.id ∈ ids then { c with vec := v } else c) } :
          Session K M) := by
  intro ids
  induction ids with
  | nil => intro d; simp [applyBatch]
  | cons i rest ih =>
    intro d
    show applyBatch (applyMut d (.upsert i v)) (rest.map (fun i => .upsert i v)) = _
    rw [ih]
    simp only [applyMut, List.map_map]
    congr 1
    apply List.map_congr_left
    intro c _
    by_cases hi : c.id = i
    · by_cases hr : c.id ∈ rest <;> simp [hi]
    · by_cases hr : c.id ∈ rest <;> simp [hi, hr]

omit [DecidableEq K] [DecidableEq M] in
/-- Upserts never move the store's contract. -/
theorem applyBatch_upserts_contract :
    ∀ (l : List (Mut K M)) (d : Session K M), (∀ x ∈ l, x.isUpsert = true) →
      (applyBatch d l).contract = d.contract := by
  intro l
  induction l with
  | nil => intro d _; rfl
  | cons x rest ih =>
    intro d hl
    show (applyBatch (applyMut d x) rest).contract = _
    rw [ih _ (fun y hy => hl y (by simp [hy]))]
    have := hl x (by simp)
    cases x with
    | upsert _ _ => rfl
    | setEmbedding _ => simp [Mut.isUpsert] at this

omit [DecidableEq K] [DecidableEq M] in
/-- The image-drop upserts of the re-embed batch are upserts. -/
theorem dropUpserts_isUpsert (s : Session K M) (drop : Bool) :
    ∀ x ∈ (if drop then (imageVectorIds s).map (fun i => (.upsert i none : Mut K M)) else []),
      x.isUpsert = true := by
  intro x hx
  cases drop with
  | true =>
    simp only [ite_true, List.mem_map] at hx
    obtain ⟨i, _, rfl⟩ := hx
    rfl
  | false => simp at hx

omit [DecidableEq K] [DecidableEq M] in
/-- The graph of a loaded contract-less session holds no image vector. -/
theorem imageVectorIds_quarantine (d : Session K M) (hd : d.contract = none) :
    imageVectorIds (quarantine d) = [] := by
  simp [imageVectorIds, quarantine, hd, List.filter_map, Function.comp_def]

/-- What a successful re-embed's checks give: every text concept is covered
once. -/
theorem reembedOk_cover {s : Session K M} {live : Contract K M} {ids : List Nat} {drop : Bool}
    (hg : reembedOk s live ids drop = true) : ∀ i ∈ textIds s, i ∈ ids := by
  unfold reembedOk at hg
  simp only [Bool.and_eq_true, decide_eq_true_eq, List.all_eq_true] at hg
  obtain ⟨⟨⟨⟨_, _⟩, hdist⟩, hlen⟩, hsub⟩ := hg
  exact cover_of_distinct ids (textIds s) hdist hsub hlen

/-- The graph after a successful re-embed: every text concept carries a
vector of the live contract (the coverage the verb reports). -/
theorem reembed_text_full (s s' : Session K M) (live : Contract K M) (ids : List Nat)
    (drop : Bool) (m : List (Mut K M)) (h : reembed s live ids drop = some (s', m)) :
    ∀ c ∈ s'.concepts, c.image = false → c.vec = some live := by
  unfold reembed at h
  split at h
  · rename_i hg
    have hcov := reembedOk_cover hg
    simp only [Option.some.injEq, Prod.mk.injEq] at h
    obtain ⟨rfl, rfl⟩ := h
    intro c' hc' himg'
    simp only [rewrite, List.mem_map] at hc'
    obtain ⟨c, hc, rfl⟩ := hc'
    cases himgc : c.image with
    | false =>
      have hct : c.id ∈ textIds s := by
        simp only [textIds, List.mem_map, List.mem_filter]
        exact ⟨c, ⟨hc, by simp [himgc]⟩, rfl⟩
      simp [hcov c.id hct]
    | true =>
      cases drop <;> simp [himgc] at himg'
  · cases h

/-! ### The bug -/

/-- **The bug (#322).** On a non-empty contract-less session (its graph
loaded through the quarantine), a successful old `re-embed` reports full
coverage from the graph, every text concept carrying a live vector, while the
store ends under the live contract with no vector at all. -/
theorem reembed_contractless_durable_loses_vectors (d s' : Session K M) (live : Contract K M)
    (drop : Bool) (m : List (Mut K M)) (hd : d.contract = none) (hne : d.concepts ≠ [])
    (h : reembedCmd (quarantine d) live false false drop = some (s', m)) :
    (applyBatch d m).contract = some live ∧ hasVectors (applyBatch d m) = false ∧
      ∀ c ∈ s'.concepts, c.image = false → c.vec = some live := by
  have hne' : (quarantine d).concepts ≠ [] := by
    simp [quarantine, hd, hne]
  rw [reembedCmd_full _ live drop hne'] at h
  by_cases hi : (!(imageVectorIds (quarantine d)).isEmpty && !drop) = true
  · rw [ite_eq_left hi] at h; cases h
  rw [ite_eq_right hi] at h
  have hfull := reembed_text_full _ s' live _ drop m h
  unfold reembed at h
  split at h
  · simp only [Option.some.injEq, Prod.mk.injEq] at h
    obtain ⟨rfl, rfl⟩ := h
    unfold reembedBatch
    rw [applyBatch_append]
    have hc : (applyBatch d ((textIds (quarantine d)).map (fun i => .upsert i (some live)) ++
        (if drop then (imageVectorIds (quarantine d)).map (fun i => .upsert i none) else []))).contract
        = none := by
      rw [applyBatch_upserts_contract _ d, hd]
      intro x hx
      rw [List.mem_append] at hx
      rcases hx with hx | hx
      · simp only [List.mem_map] at hx
        obtain ⟨i, _, rfl⟩ := hx
        rfl
      · exact dropUpserts_isUpsert _ drop x hx
    refine ⟨rfl, ?_, hfull⟩
    show hasVectors (applyMut (applyBatch d _) (.setEmbedding (some live))) = false
    unfold applyMut
    rw [hc]
    simp [hasVectors]
  · cases h

/-! ### The fix -/

/-- **Nothing changes for a stamped session**: the fixed `Graph::reembed` is
the old one, batch included. -/
theorem reembedOrdered_stamped_eq (s : Session K M) (live : Contract K M) (ids : List Nat)
    (drop : Bool) (hc : s.contract ≠ none) : reembedOrdered s live ids drop = reembed s live ids drop := by
  simp [reembedOrdered, reembed, reembedBatchOrdered, hc]

/-- The fix moves no graph state: the session `re-embed` leaves in memory is
the old one in every case. -/
theorem reembedOrdered_graph_eq (s : Session K M) (live : Contract K M) (ids : List Nat)
    (drop : Bool) :
    (reembedOrdered s live ids drop).map Prod.fst = (reembed s live ids drop).map Prod.fst := by
  unfold reembedOrdered reembed
  split <;> rfl

omit [DecidableEq K] [DecidableEq M] in
/-- The fixed batch holds the old batch's mutations, reordered. -/
theorem reembedBatchOrdered_perm (s : Session K M) (live : Contract K M) (ids : List Nat)
    (drop : Bool) : (reembedBatchOrdered s live ids drop).Perm (reembedBatch s live ids drop) := by
  unfold reembedBatchOrdered reembedBatch
  split
  · exact (List.perm_append_singleton _ _).symm
  · exact List.Perm.refl _

/-- On a contract-less session the fixed batch stamps the contract first, then
writes only upserts. -/
theorem reembedOrdered_contract_first (s s' : Session K M) (live : Contract K M) (ids : List Nat)
    (drop : Bool) (m : List (Mut K M)) (hc : s.contract = none)
    (h : reembedOrdered s live ids drop = some (s', m)) :
    ∃ rest, m = .setEmbedding (some live) :: rest ∧ ∀ x ∈ rest, x.isUpsert = true := by
  unfold reembedOrdered at h
  split at h
  · simp only [Option.some.injEq, Prod.mk.injEq] at h
    obtain ⟨rfl, rfl⟩ := h
    refine ⟨ids.map (fun i => .upsert i (some live)) ++
      (if drop then (imageVectorIds s).map (fun i => .upsert i none) else []),
      by simp [reembedBatchOrdered, hc], ?_⟩
    intro x hx
    rw [List.mem_append] at hx
    rcases hx with hx | hx
    · simp only [List.mem_map] at hx
      obtain ⟨i, _, rfl⟩ := hx
      rfl
    · exact dropUpserts_isUpsert _ drop x hx
  · cases h

/-- The store after the fixed batch on a contract-less session: the live
contract, every vector nulled by the first stamp, then the listed ones set. -/
theorem reembedOrdered_contractless_store (d s' : Session K M) (live : Contract K M)
    (ids : List Nat) (drop : Bool) (m : List (Mut K M)) (hd : d.contract = none)
    (h : reembedOrdered (quarantine d) live ids drop = some (s', m)) :
    reembedOk (quarantine d) live ids drop = true ∧
      s' = ⟨some live, rewrite (quarantine d).concepts ids live drop⟩ ∧
      applyBatch d m = ⟨some live, d.concepts.map (fun c =>
        if c.id ∈ ids then { c with vec := some live } else { c with vec := none })⟩ := by
  have hq : (quarantine d).contract = none := by simp [quarantine, hd]
  unfold reembedOrdered at h
  split at h
  · rename_i hg
    simp only [Option.some.injEq, Prod.mk.injEq] at h
    obtain ⟨rfl, rfl⟩ := h
    refine ⟨hg, rfl, ?_⟩
    simp only [reembedBatchOrdered, hq, ite_true, imageVectorIds_quarantine d hd, List.map_nil,
      ite_self, List.append_nil]
    show applyBatch (applyMut d (.setEmbedding (some live))) _ = _
    rw [applyBatch_upserts]
    simp only [applyMut, hd, Option.isNone_none, Option.isSome_some, Bool.and_self, ite_true,
      List.map_map]
    rfl
  · cases h

/-- **The fix (#322).** After a successful fixed re-embed of a contract-less
session (its graph loaded through the quarantine), the store is under the
live contract, in one space, and every text concept holds a vector of the
live contract. -/
theorem reembedOrdered_contractless_durable (d s' : Session K M) (live : Contract K M)
    (ids : List Nat) (drop : Bool) (m : List (Mut K M)) (hd : d.contract = none)
    (h : reembedOrdered (quarantine d) live ids drop = some (s', m)) :
    (applyBatch d m).contract = some live ∧ oneSpace (applyBatch d m) = true ∧
      ∀ c ∈ (applyBatch d m).concepts, c.image = false → c.vec = some live := by
  obtain ⟨hg, _, hs⟩ := reembedOrdered_contractless_store d s' live ids drop m hd h
  have hcov := reembedOk_cover hg
  rw [hs]
  refine ⟨rfl, ?_, ?_⟩
  · rw [oneSpace_iff]
    intro c hc
    simp only [List.mem_map] at hc
    obtain ⟨c0, _, rfl⟩ := hc
    by_cases hi : c0.id ∈ ids <;> simp [hi]
  · intro c hc himg
    simp only [List.mem_map] at hc
    obtain ⟨c0, hc0, rfl⟩ := hc
    have himg0 : c0.image = false := by
      by_cases hi : c0.id ∈ ids <;> simp_all
    have hct : c0.id ∈ textIds (quarantine d) := by
      simp only [textIds, quarantine, hd, List.mem_map, List.mem_filter]
      exact ⟨{ c0 with vec := none }, ⟨⟨c0, hc0, rfl⟩, by simp [himg0]⟩, rfl⟩
    simp [hcov c0.id hct]

omit [DecidableEq K] [DecidableEq M] in
/-- Distinct ids name distinct concepts. -/
theorem distinct_ids_inj :
    ∀ (l : List (Concept K M)), distinct (l.map (·.id)) = true →
      ∀ a ∈ l, ∀ b ∈ l, a.id = b.id → a = b := by
  intro l
  induction l with
  | nil => intro _ a ha; cases ha
  | cons x t ih =>
    intro hd a ha b hb hab
    simp only [List.map_cons, distinct, Bool.and_eq_true, decide_eq_true_eq] at hd
    obtain ⟨hx, ht⟩ := hd
    rcases List.mem_cons.mp ha with hax | ha'
    · rcases List.mem_cons.mp hb with hbx | hb'
      · rw [hax, hbx]
      · subst hax; exact (hx (List.mem_map.mpr ⟨b, hb', hab.symm⟩)).elim
    · rcases List.mem_cons.mp hb with hbx | hb'
      · subst hbx; exact (hx (List.mem_map.mpr ⟨a, ha', hab⟩)).elim
      · exact ih ht a ha' b hb' hab

/-- **The coverage report is the store's.** With distinct node ids, the store
after a successful fixed re-embed of a contract-less session is exactly the
graph the re-embed returns. -/
theorem reembedOrdered_contractless_matches_graph (d s' : Session K M) (live : Contract K M)
    (ids : List Nat) (drop : Bool) (m : List (Mut K M)) (hd : d.contract = none)
    (hid : distinct (d.concepts.map (·.id)) = true)
    (h : reembedOrdered (quarantine d) live ids drop = some (s', m)) :
    applyBatch d m = s' := by
  obtain ⟨hg, hs', hs⟩ := reembedOrdered_contractless_store d s' live ids drop m hd h
  have hsub : ∀ i ∈ ids, i ∈ textIds (quarantine d) := by
    unfold reembedOk at hg
    simp only [Bool.and_eq_true, decide_eq_true_eq, List.all_eq_true] at hg
    exact hg.2
  rw [hs, hs']
  simp only [quarantine, hd, rewrite, List.map_map]
  congr 1
  apply List.map_congr_left
  intro c hc
  cases himg : c.image with
  | false => by_cases hi : c.id ∈ ids <;> simp [himg, hi]
  | true =>
    have hni : c.id ∉ ids := by
      intro hi
      have ht := hsub c.id hi
      simp only [textIds, quarantine, hd, List.mem_map, List.mem_filter] at ht
      obtain ⟨_, ⟨⟨t, ht, rfl⟩, htimg⟩, hteq⟩ := ht
      have := distinct_ids_inj d.concepts hid t ht c hc hteq
      subst this
      simp [himg] at htimg
    cases drop <;> simp [himg, hni]

/-- The fixed verb on a stamped session is the #296 verb, batch included. -/
theorem reembedCmd322_stamped_agrees (s : Session K M) (live : Contract K M)
    (allow missingOnly drop same repin : Bool) (mode : CanaryMode) (pin : PinState)
    (hc : s.contract ≠ none) :
    reembedCmd322 s live allow missingOnly drop same repin mode pin =
      reembedCmdFixed s live allow missingOnly drop same repin mode pin := by
  unfold reembedCmd322 reembedCmdFixed
  rw [reembedOrdered_stamped_eq s live _ drop hc]

/-- **The fixed verb on a contract-less session.** A successful full run on a
non-empty contract-less session leaves the store under the live contract, in
one space, with every text concept holding a live vector. -/
theorem reembedCmd322_contractless_durable (d s' : Session K M) (live : Contract K M)
    (drop same repin : Bool) (mode : CanaryMode) (pin : PinState) (m : List (Mut K M))
    (hd : d.contract = none) (hne : d.concepts ≠ [])
    (h : reembedCmd322 (quarantine d) live false false drop same repin mode pin = some (s', m)) :
    (applyBatch d m).contract = some live ∧ oneSpace (applyBatch d m) = true ∧
      ∀ c ∈ (applyBatch d m).concepts, c.image = false → c.vec = some live := by
  have hq : (quarantine d).contract = none := by simp [quarantine, hd]
  have hne' : (quarantine d).concepts.isEmpty = false := by
    cases hcs : d.concepts with
    | nil => exact absurd hcs hne
    | cons _ _ => simp [quarantine, hd, hcs]
  unfold reembedCmd322 at h
  cases same <;>
    simp only [Bool.false_eq_true, ite_false, Bool.false_and, hne', Bool.not_false,
      Bool.true_and, hq, reduceCtorEq, decide_false, bne_self_eq_false, ite_true] at h
  · split at h
    · cases h
    · split at h
      · cases h
      · split at h
        · cases h
        · exact reembedOrdered_contractless_durable d s' live _ drop m hd h
  · simp at h

/-- **`--missing-only` has no such hazard**: on a contract-less session the
backfill is refused or finds nothing to do, and writes nothing. -/
theorem missingOnly_contractless_writes_nothing (s : Session K M) (live : Contract K M)
    (mode : CanaryMode) (pin : PinState) (r : Session K M × List (Mut K M))
    (hc : s.contract = none)
    (h : reembedCmd322 s live false true false false false mode pin = some r) : r.2 = [] := by
  have hbf := fun ids => backfill_needs_same_contract s live ids (by rw [hc]; simp)
  unfold reembedCmd322 at h
  by_cases he : s.concepts.isEmpty = true
  · simp [he] at h; rw [← h.2]
  · by_cases hi : ((s.concepts.filter (fun c => !c.image && c.vec.isNone)).map (·.id)).isEmpty = true
    · simp [he, hi] at h; rw [← h.2]
    · simp [he, hi, hc, hbf] at h

/-- **The #322 scenario.** A contract-less store with a legacy vector on
concept 1 and none on concept 2; its graph is loaded through the quarantine.
The old re-embed to `cA` reports both concepts embedded, and the store ends
under `cA` with no vector. The fixed one leaves both vectors in the store,
which then equals the graph; a stamped session's batch is unchanged. -/
theorem example_contractless_bug :
    let d : Session Nat Nat := ⟨none, [⟨1, false, some cC⟩, ⟨2, false, none⟩]⟩
    let g : Session Nat Nat := ⟨some cA, [⟨1, false, some cA⟩, ⟨2, false, some cA⟩]⟩
    (reembedCmd (quarantine d) cA false false false).map Prod.fst = some g ∧
    (reembedCmd (quarantine d) cA false false false).map (fun r => applyBatch d r.2) =
      some ⟨some cA, [⟨1, false, none⟩, ⟨2, false, none⟩]⟩ ∧
    (reembedCmd322 (quarantine d) cA false false false false false .warn .absent).map Prod.fst =
      some g ∧
    (reembedCmd322 (quarantine d) cA false false false false false .warn .absent).map
      (fun r => applyBatch d r.2) = some g ∧
    reembedCmd322 sA cC false false false false false .warn .absent =
      reembedCmdFixed sA cC false false false false false .warn .absent := by
  decide


end Lambo.Contract
