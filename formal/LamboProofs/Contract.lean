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

end Lambo.Contract
