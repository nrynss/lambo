

/-
# The tiered store's pure pieces (#18)

Models the functions of `TieredStore` (`src/store/tiered/`) that the state
machine calls and that hold no state: `project::mirror_version` and
`Tier::next_version` (`mirrorVersion`, `nextVersion`); `knn_fetch` and
`KNN_OVERFETCH` (`knnFetch`); `project::project` and, since #319,
`Projection::switched` (`project`, `switches`), with the durable batch they
read (`applyBatch`); the sync marker (`check_marker`, `Marker::sync`:
`checkMarker`); the read breaker (`breaker_admits`, `breaker_success`,
`breaker_failure`, with `BREAKER_THRESHOLD`, `BREAKER_COOLDOWN` and
`REPAIR_BACKOFF` in `Params`); and the lease bookkeeping (`note_lease`,
`note_refresh` as changed by #293's review, `ensure_fenced`'s test:
`noteLease`, `noteRefresh`, `fenced`). Versions and epochs are naturals.

Results: a strictly larger token gives a strictly larger version whatever the
counters, the counter orders one holder's writes, and `next_version` never
repeats or goes back under one token or a later one (`version_holder_first`,
`version_counter`, `nextVersion_gt`); for every accepted limit the over-fetch
is at least the limit, never above the store-wide candidate limit, and
`limit + 16` below the cap (`knnFetch_bounds`); only an `InSync` marker is
trusted, and a marker is in sync exactly at the snapshot's epoch or, absent,
for a snapshot with nothing to index (`checkMarker_inSync_iff`); `threshold`
consecutive failures open the breaker for a full cool-down, fewer do not,
while open a read is the durable store's, one probe is admitted after the
cool-down, a successful probe closes it and a failed one re-opens it, and a
read admitted before it opened that succeeds after closes it at once
(`breaker_trips`, `breaker_counts`, `breaker_open_falls_back`,
`breaker_one_probe`, `breaker_probe_outcome`, `late_success_closes`); a
refresh never adopts a re-taken token, and the fence passes only for the
durable row's current token (`noteRefresh_never_adopts`, `fenced_iff`).

Boundary: `u64` overflow is modelled only as `mirror_version`'s refusal;
widths (`index_doc`'s check), index naming and `validate_index_prefix` are
not modelled. How these functions combine is `Machine.lean`.
-/

namespace Lambo.Tiered

/-! ## Versions (`project::mirror_version`, `Tier::next_version`) -/

/-- `1 << 32`: the counter's place in an external version.
Pinned by `formal_constants_tests::tiered` (its Rust side with feature `recall-elastic`). -/
def shift : Nat := 4294967296

/-- `u32::MAX`.
Pinned by `formal_constants_tests::tiered` (its Rust side with feature `recall-elastic`). -/
def u32Max : Nat := 4294967295

/-- `MAX_VERSIONED_TOKEN = (i64::MAX as u64) >> 32`.
Pinned by `formal_constants_tests::tiered` (its Rust side with feature `recall-elastic`). -/
def maxTok : Nat := 2147483647

/-- `mirror_version(token, counter)`: `(token << 32) | counter`, or `None` when
the token does not fit a positive signed 64-bit version. For a counter below
`1 << 32` (a `u32`) the `|` is `+`. -/
def mirrorVersion (t c : Nat) : Option Nat :=
  if t ≤ maxTok then some (t * shift + c) else none

/-- `next_version`: refuse an older token, restart the counter for a newer token, refuse to wrap it
(`checked_add`) or to overflow the version. Returns the new `(token, counter)`
and the version, or `none` (abstracting the Rust `StoreError`). -/
def nextVersion (tok cnt t : Nat) : Option (Nat × Nat × Nat) :=
  if t < tok then none else
    let c := if tok = t then cnt else 0
    if c + 1 ≤ u32Max then
      match mirrorVersion t (c + 1) with
      | some v => some (t, c + 1, v)
      | none => none
    else none

/-- A stale pass cannot reserve a version with a token older than the recorded token. -/
theorem nextVersion_refuses_older {tok cnt t : Nat} (h : t < tok) :
    nextVersion tok cnt t = none := by
  simp [nextVersion, h]

theorem mirrorVersion_lt_tok {t t' c c' : Nat} (hc : c < shift) (ht : t < t')
    (h' : t' ≤ maxTok) : ∃ v v', mirrorVersion t c = some v ∧ mirrorVersion t' c' = some v' ∧
      v < v' := by
  refine ⟨t * shift + c, t' * shift + c', ?_, ?_, ?_⟩
  · simp [mirrorVersion]; omega
  · simp [mirrorVersion]; omega
  · have : (t + 1) * shift ≤ t' * shift := Nat.mul_le_mul_right _ ht
    rw [Nat.add_mul] at this
    omega

/-- **A new holder outranks every write of an older one** (the doc comment of
`mirror_version`): whatever the counters, a strictly larger token gives a
strictly larger version. -/
theorem version_holder_first {t t' c c' v v' : Nat} (hc : c < shift) (ht : t < t')
    (hv : mirrorVersion t c = some v) (hv' : mirrorVersion t' c' = some v') : v < v' := by
  have h' : t' ≤ maxTok := by
    unfold mirrorVersion at hv'; split at hv' <;> simp_all
  obtain ⟨w, w', hw, hw', hlt⟩ := mirrorVersion_lt_tok (c' := c') hc ht h'
  rw [hv] at hw; rw [hv'] at hw'
  cases hw; cases hw'; exact hlt

/-- Within one holder the counter orders its writes. -/
theorem version_counter {t c c' v v' : Nat} (hc : c < c')
    (hv : mirrorVersion t c = some v) (hv' : mirrorVersion t c' = some v') : v < v' := by
  unfold mirrorVersion at hv hv'
  split at hv <;> simp_all
  omega

/-- **`next_version` never repeats or goes back** under one token or a later
one: the version it hands out is above every version the previous
`(token, counter)` could have produced, so a holder's writes land in issue
order and a takeover's writes outrank the old holder's. -/
theorem nextVersion_gt {tok cnt t t' c' v w : Nat} (hcnt : cnt ≤ u32Max) (ht : tok ≤ t)
    (hold : mirrorVersion tok cnt = some w) (h : nextVersion tok cnt t = some (t', c', v)) :
    w < v ∧ t' = t ∧ c' ≤ u32Max := by
  unfold nextVersion at h
  simp only [Nat.not_lt_of_ge ht, ite_false] at h
  by_cases he : tok = t
  · subst he
    simp only [ite_true] at h
    split at h
    · rename_i hle
      split at h
      · rename_i v₀ hv
        simp only [Option.some.injEq, Prod.mk.injEq] at h
        obtain ⟨rfl, rfl, rfl⟩ := h
        exact ⟨version_counter (Nat.lt_succ_self _) hold hv, rfl, hle⟩
      · simp at h
    · simp at h
  · simp only [he, ite_false] at h
    split at h
    · rename_i hle
      split at h
      · rename_i v₀ hv
        simp only [Option.some.injEq, Prod.mk.injEq] at h
        obtain ⟨rfl, rfl, rfl⟩ := h
        have hlt : tok < t := Nat.lt_of_le_of_ne ht he
        exact ⟨version_holder_first (by unfold u32Max at hcnt; unfold shift; omega) hlt hold hv,
          rfl, hle⟩
      · simp at h
    · simp at h

/-- The counter refuses to wrap (`an_exhausted_flush_counter_refuses_the_mirror`). -/
example : nextVersion 7 u32Max 7 = none := by decide

/-- A new token restarts the counter at 1 (`flush_counters_are_kept_per_session`). -/
example : nextVersion 7 41 8 = some (8, 1, 8 * shift + 1) := by decide

/-- `the_version_orders_holders_before_counters_and_refuses_overflow`. -/
example : mirrorVersion 1 u32Max = some (shift + u32Max) ∧ shift + u32Max < 2 * shift + 1 ∧
    mirrorVersion 2 1 = some (2 * shift + 1) ∧ mirrorVersion (maxTok + 1) 1 = none := by decide

/-! ## The over-fetch (`knn_fetch`, `KNN_OVERFETCH`) -/

/-- `MAX_VECTOR_CANDIDATE_LIMIT`.
Pinned by `formal_constants_tests::tiered`. -/
def maxLimit : Nat := 2048

/-- `KNN_OVERFETCH`.
Pinned by `formal_constants_tests::tiered` (its Rust side with feature `recall-elastic`). -/
def overfetch : Nat := 16

/-- `knn_fetch(limit)`: `limit + 16`, capped at `max(2048, limit)`. -/
def knnFetch (limit : Nat) : Nat := min (limit + overfetch) (max maxLimit limit)

/-- **The over-fetch bound (#18).** For every limit the store accepts
(`validate_vector_candidate_limit`: at most 2048), the fetch is at least the
limit and never above the store-wide candidate limit; below the cap it is
exactly `limit + 16`. -/
theorem knnFetch_bounds (limit : Nat) (h : limit ≤ maxLimit) :
    limit ≤ knnFetch limit ∧ knnFetch limit ≤ maxLimit ∧
      (limit + overfetch ≤ maxLimit → knnFetch limit = limit + overfetch) := by
  unfold knnFetch maxLimit overfetch at *
  refine ⟨?_, ?_, ?_⟩ <;> omega

/-- `the_overfetch_never_exceeds_the_candidate_limit`. -/
example : knnFetch 5 = 21 ∧ knnFetch 2048 = 2048 ∧ knnFetch 2044 = 2048 := by decide

/-! ## The durable session and the projection (`project::project`) -/

/-- A stored vector: the embedder build that produced it and its value. Only
the build matters to the theorems (two builds behind one contract name is a
canary drift, #296); widths are taken as right (`index_doc`'s width check and
`check_supplied_values` are left out). -/
abbrev Vec := Nat × Nat

/-- The mutations the projection reads. `upsert n v` is an `UpsertNode` of a
concept (`v = none`: it carries no vector); `setContract` is `SetEmbedding`;
`deleteEdge` is `DeleteEdge`; `other` is every other mutation (interactions,
edges, canonization, names), which the index never sees but which names the
session (`batch_session_ids`). Contracts are named by a number. One session throughout. -/
inductive Mut where
  | upsert (n : Nat) (v : Option Vec)
  | delete (n : Nat)
  | setContract (c : Option Nat)
  | deleteEdge
  | other
  deriving DecidableEq, Repr

/-- One index write of a mirror: `(contract, node, document)`, `none` a delete
(`DocOp::Index` / `DocOp::Delete`), all at the batch's one version. -/
abbrev Op := Nat × Nat × Option Vec

/-- `project`'s `push`: drop any earlier write for the id, append this one (the
whole batch is written at one version, so a second write for an id would be
refused as a conflict). -/
def pushOp (ops : List Op) (o : Op) : List Op :=
  ops.filter (fun p => p.2.1 != o.2.1) ++ [o]

/-- `project(session, mutations, contract_before, version)`: the writes and the
contract after the batch. With no contract in force nothing is written. -/
def projectGo : List Mut → Option Nat → List Op → List Op × Option Nat
  | [], cur, ops => (ops, cur)
  | .setContract c :: ms, _, ops => projectGo ms c ops
  | .upsert n v :: ms, cur, ops =>
      match cur with
      | some c => projectGo ms cur (pushOp ops (c, n, v))
      | none => projectGo ms cur ops
  | .delete n :: ms, cur, ops =>
      match cur with
      | some c => projectGo ms cur (pushOp ops (c, n, none))
      | none => projectGo ms cur ops
  | .deleteEdge :: ms, cur, ops => projectGo ms cur ops
  | .other :: ms, cur, ops => projectGo ms cur ops

def project (ms : List Mut) (before : Option Nat) : List Op × Option Nat :=
  projectGo ms before []

/-- `the_projection_follows_the_mutation_table`, on its shape: no contract yet
writes nothing, the last write for an id wins, a switch moves what follows. -/
example :
    project [.upsert 1 (some (0, 10)), .setContract (some 1), .upsert 1 (some (0, 10)),
      .upsert 2 none, .other, .setContract (some 2), .upsert 1 (some (0, 11)), .delete 2] none
      = ([(2, 1, some (0, 11)), (2, 2, none)], some 2) := by decide

/-- `a_cleared_contract_stops_writes`. -/
example : project [.setContract none, .upsert 1 (some (0, 1)), .delete 2] (some 1) = ([], none) := by
  decide

/-- `Projection::switched` (#319): the batch moves the contract where its
projection cannot say what the new contract's index must hold. A
`SetEmbedding` that changes the contract in force counts when a contract was
already in force (the session's other vectors are now the new contract's:
`--allow-embedding-mismatch`'s relabel, or a migration) or when a concept
upsert carrying a vector came earlier in the batch (its vector is the new
contract's, and a first stamp quarantines it durably; `Graph::reembed_all`
wrote that order from no contract before #322, and a raw writer still can).
A first stamp with nothing before it, or a `SetEmbedding` that
keeps the contract, does not count. -/
def switchesGo : List Mut → Option Nat → Bool → Bool
  | [], _, _ => false
  | .setContract c :: ms, cur, up => (decide (c ≠ cur) && (cur.isSome || up)) || switchesGo ms c up
  | .upsert _ (some _) :: ms, cur, _ => switchesGo ms cur true
  | _ :: ms, cur, up => switchesGo ms cur up

def switches (ms : List Mut) (before : Option Nat) : Bool := switchesGo ms before false

/-- The durable side of one session: its concepts' vectors (most recent first,
one entry per id), its contract, its mutation epoch, whether it was erased,
and its lease row (`lease` is `current_token`, `row` who holds it). -/
inductive Row where
  | none | ours | other | released | erased
  deriving DecidableEq, Repr

structure Durable where
  vecs : List (Nat × Vec) := []
  contract : Option Nat := none
  epoch : Nat := 0
  erased : Bool := false
  lease : Nat := 0
  row : Row := .none
  deriving DecidableEq, Repr

/-- The stored vector of concept `n`. -/
def lookup (n : Nat) (l : List (Nat × Vec)) : Option Vec :=
  (l.find? (fun p => p.1 == n)).map (·.2)

/-- One mutation applied to the durable rows (the SQL adapters apply a whole
batch in one transaction). A session's first contract stamp quarantines every
vector it holds, those written earlier in the same batch included: the legacy
upgrade of `MemoryStore`'s `SetEmbedding`, the pg family's
`QUARANTINE_LEGACY_EMBEDDINGS_SQL` and SQLite's `set_embedding` (whose
width predicate is true for an unstamped session). -/
def applyMut (d : Durable) : Mut → Durable
  | .upsert n (some v) => { d with vecs := (n, v) :: d.vecs.filter (fun p => p.1 != n) }
  | .upsert n none => { d with vecs := d.vecs.filter (fun p => p.1 != n) }
  | .delete n => { d with vecs := d.vecs.filter (fun p => p.1 != n) }
  | .setContract c =>
      { d with contract := c, vecs := if d.contract.isNone && c.isSome then [] else d.vecs }
  | .deleteEdge | .other => d

/-- A committed batch: every mutation, then the next epoch. -/
def applyBatch (d : Durable) (ms : List Mut) : Durable :=
  { ms.foldl applyMut d with epoch := d.epoch + 1 }

/-! ## The sync marker (`check_marker`, `Marker::sync`) -/

inductive Sync where
  | unknown | inSync | stale
  deriving DecidableEq, Repr

inductive Marker where
  | inSync | behind | ahead | unreachable
  deriving DecidableEq, Repr

def Marker.sync : Marker → Sync
  | .inSync => .inSync
  | .behind => .stale
  | .ahead | .unreachable => .unknown

/-- What a durable load returns: `none` for `SessionNotFound`. -/
abbrev Snap := Option Durable

def snapEpoch : Snap → Nat
  | some d => d.epoch
  | none => 0

/-- `check_marker`'s `indexable`: a contract and at least one concept with a
vector. -/
def indexable : Snap → Bool
  | some d => d.contract.isSome && !d.vecs.isEmpty
  | none => false

/-- `check_marker(session, snap)`. `mk` is the marker the index holds (its
version and epoch); `down` an unreachable index. -/
def checkMarker (down : Bool) (mk : Option (Nat × Nat)) (snap : Snap) : Marker :=
  if down then .unreachable else
  match mk with
  | some (_, e) =>
      if e = snapEpoch snap then .inSync
      else if e > snapEpoch snap then .ahead else .behind
  | none => if indexable snap then .behind else .inSync

/-- A marker read classifies every snapshot one way, and only an `InSync`
marker is trusted. -/
theorem checkMarker_inSync_iff (mk : Option (Nat × Nat)) (snap : Snap) :
    checkMarker false mk snap = .inSync ↔
      (∃ v, mk = some (v, snapEpoch snap)) ∨ (mk = none ∧ indexable snap = false) := by
  unfold checkMarker
  cases mk with
  | none => cases indexable snap <;> simp
  | some p =>
    obtain ⟨v, e⟩ := p
    simp only [Bool.false_eq_true, ite_false, Option.some.injEq, Prod.mk.injEq, exists_eq_left',
      reduceCtorEq, false_and, or_false]
    by_cases h : e = snapEpoch snap
    · simp [h]
    · by_cases h' : e > snapEpoch snap <;> simp [h, h']

/-! ## The read breaker (`breaker_admits`, `breaker_success`, `breaker_failure`) -/

/-- `Breaker`: consecutive failures and, once open, until when. -/
structure Breaker where
  fails : Nat := 0
  openUntil : Option Nat := none
  deriving DecidableEq, Repr

/-- The breaker's two settings (`BREAKER_THRESHOLD = 3`,
`BREAKER_COOLDOWN = 30s`) and the repair backoff (`REPAIR_BACKOFF = 60s`).
Times are whole seconds.
Pinned by `formal_constants_tests::tiered` (its Rust side with feature `recall-elastic`). -/
structure Params where
  threshold : Nat := 3
  cooldown : Nat := 30
  backoff : Nat := 60
  /-- The #319 fix: a batch that moves the contract (`switches`) is repaired
  from the durable store, not mirrored. `false` is the previous mirror, kept
  for the theorems that pin it. -/
  switchRepairs : Bool := true
  deriving DecidableEq, Repr

/-- `breaker_admits`: whether this read may ask the index, and the breaker
after it. A read past the cool-down is the probe and pushes it forward. -/
def admits (P : Params) (now : Nat) (b : Breaker) : Bool × Breaker :=
  match b.openUntil with
  | some u => if now < u then (false, b) else (true, { b with openUntil := some (now + P.cooldown) })
  | none => (true, b)

def onSuccess (_b : Breaker) : Breaker := {}

def onFailure (P : Params) (now : Nat) (b : Breaker) : Breaker :=
  let f := b.fails + 1
  if f ≥ P.threshold then { fails := f, openUntil := some (now + P.cooldown) }
  else { b with fails := f }

/-- A read through the breaker: `ok` says whether the index would answer. The
result is `true` when the index answered, `false` when the read went to the
durable store (refused by an open breaker, or failed). -/
def breakerRead (P : Params) (now : Nat) (ok : Bool) (b : Breaker) : Bool × Breaker :=
  match admits P now b with
  | (false, b') => (false, b')
  | (true, b') => if ok then (true, onSuccess b') else (false, onFailure P now b')

/-- Reads at a time, each with whether the index would answer. -/
def breakerRun (P : Params) : List (Nat × Bool) → Breaker → List Bool × Breaker
  | [], b => ([], b)
  | (t, ok) :: rs, b =>
      let (r, b') := breakerRead P t ok b
      let (rs', b'') := breakerRun P rs b'
      (r :: rs', b'')

/-- **The breaker trips** (#18): `threshold` consecutive failures from a closed
breaker open it for a full cool-down after the last one. -/
theorem breaker_trips (P : Params) (b : Breaker) (now : Nat) (h : P.threshold ≤ b.fails + 1)
    (hc : b.openUntil = none) :
    (breakerRead P now false b).2.openUntil = some (now + P.cooldown) := by
  simp [breakerRead, admits, hc, onFailure, h]

/-- Below the threshold a failure does not open a closed breaker. -/
theorem breaker_counts (P : Params) (b : Breaker) (now : Nat) (h : b.fails + 1 < P.threshold)
    (hc : b.openUntil = none) : (breakerRead P now false b).2.openUntil = none := by
  simp only [breakerRead, admits, hc, onFailure]
  have : ¬ (b.fails + 1 ≥ P.threshold) := by omega
  simp [this]

/-- **While open, reads go to the durable store** and the index is not asked:
the answer is the fallback whatever the index would say, and the breaker is
unchanged. -/
theorem breaker_open_falls_back (P : Params) (b : Breaker) (now u : Nat) (ok : Bool)
    (hu : b.openUntil = some u) (hlt : now < u) : breakerRead P now ok b = (false, b) := by
  simp [breakerRead, admits, hu, hlt]

/-- **One probe after the cool-down**: the first read at or past `openUntil`
asks the index and pushes the cool-down forward, so a concurrent read before
the probe's outcome still falls back. -/
theorem breaker_one_probe (P : Params) (b : Breaker) (now u : Nat) (hu : b.openUntil = some u)
    (hge : u ≤ now) (hP : 0 < P.cooldown) :
    (admits P now b).1 = true ∧ ∀ ok', (breakerRead P now ok' (admits P now b).2).1 = false := by
  have hn : ¬ now < u := Nat.not_lt.mpr hge
  refine ⟨by simp [admits, hu, hn], ?_⟩
  intro ok'
  have hlt : now < now + P.cooldown := by omega
  simp [admits, hu, hn, breakerRead, hlt]

/-- **A successful probe closes the breaker**; a failed one re-opens it for
another cool-down. -/
theorem breaker_probe_outcome (P : Params) (b : Breaker) (now u : Nat) (hu : b.openUntil = some u)
    (hge : u ≤ now) :
    breakerRead P now true b = (true, {}) ∧
      (breakerRead P now false b).2.openUntil = some (now + P.cooldown) := by
  have hn : ¬ now < u := Nat.not_lt.mpr hge
  simp [breakerRead, admits, hu, hn, onSuccess, onFailure]

/-- `failing_index_reads_open_a_breaker_that_a_probe_closes` (threshold 3,
cool-down 100 ms as 100 ticks): three failed reads ask the index, the next five
do not, the probe after the cool-down does and succeeds, and the read after it
asks again. -/
example :
    (breakerRun { threshold := 3, cooldown := 100 }
      [(0, false), (0, false), (0, false), (1, false), (2, true), (3, true), (4, true), (5, true),
       (150, true), (151, true)] {}).1
      = [false, false, false, false, false, false, false, false, true, true] := by decide

/-- `a_hanging_index_read_is_cut_at_the_deadline_and_counts` (threshold 2): two
timeouts open it, the next two reads are not sent. -/
example :
    (breakerRun { threshold := 2, cooldown := 60 } [(0, false), (0, false), (0, false), (0, false)]
      {}).2.openUntil = some 60 := by decide

/-- A read admitted before the breaker opened and answering after it closes
the breaker at once: `breaker_success` resets whatever opened it in between. -/
theorem late_success_closes (b : Breaker) :
    onSuccess b = {} := rfl

/-! ## Lease bookkeeping (`note_lease`, `note_refresh`, `ensure_fenced`) -/

/-- What an acquire or refresh returned: `Acquired(token)` or `Held`. -/
inductive LeaseOutcome where
  | acquired (t : Nat)
  | held
  deriving DecidableEq, Repr

/-- `note_lease`: an acquire adopts the token it was given. -/
def noteLease (_held : Option Nat) : LeaseOutcome → Option Nat
  | .acquired t => some t
  | .held => none

/-- `note_refresh` (#293 review): a refresh keeps the token already held and
never adopts another. -/
def noteRefresh (held : Option Nat) : LeaseOutcome → Option Nat
  | .acquired t => if held = some t then some t else none
  | .held => none

/-- `ensure_fenced(session, token)`: this store's record, then the durable row
(absent: no durable leases, the first check only). -/
def fenced (held : Option Nat) (durable : Option Nat) (t : Nat) : Bool :=
  held == some t && (match durable with | some d => d == t | none => true)

/-- **A refresh never adopts a re-taken token** (#293 review): after
`note_refresh` the store holds a token only if it already held exactly it. -/
theorem noteRefresh_never_adopts (held : Option Nat) (o : LeaseOutcome) (t : Nat)
    (h : noteRefresh held o = some t) : held = some t := by
  cases o with
  | acquired t' =>
    simp only [noteRefresh] at h
    split at h
    · rename_i he; cases h; exact he
    · simp at h
  | held => simp [noteRefresh] at h

/-- Before #293's review the refresh was noted like an acquire and adopted the
re-taken token: `a_refresh_that_retakes_the_lease_does_not_adopt_its_token`. -/
example : noteLease (some 4) (.acquired 5) = some 5 ∧ noteRefresh (some 4) (.acquired 5) = none ∧
    noteRefresh (some 4) (.acquired 4) = some 4 := by decide

/-- **The fence passes only for the current generation**: under any token
other than the durable row's it refuses, whatever this store believes. -/
theorem fenced_iff (held : Option Nat) (d t : Nat) :
    fenced held (some d) t = true ↔ held = some t ∧ d = t := by
  simp [fenced]

end Lambo.Tiered
