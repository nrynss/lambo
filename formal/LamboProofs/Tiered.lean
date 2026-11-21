/-
# The tiered store: a SQL store of record beside an Elastic recall index (#18)

Model of `TieredStore` in `src/store/tiered/` (lines as at `ab9b57dc`): a
durable store (SQLite, Postgres, Cockroach) that stays the source of truth,
and an Elasticsearch index mirrored from it that serves the vector leg.

* `mod.rs`: the mirror (`mirror` l. 1025, `mirror_unnamed` l. 1047,
  `mirror_session` l. 1083, `mark_unleased` l. 1134, `mirror_ops` l. 1155,
  `next_version` l. 547, `mark_stale` l. 567); the repair (`request_repair`
  l. 797, `run_repairs` l. 819, `repair_once` l. 894, `reconcile` l. 939,
  `repair_if_due` l. 1015, `sweep` l. 641, `ensure_fenced` l. 610,
  `check_marker` l. 670, `recheck_ahead` l. 714, `settle_after_load` l. 742,
  `stop_repair_for_erase` l. 1840); reads (`sync_for_read` l. 1284,
  `checked_vector_candidates` l. 1329, `fallback` l. 1246, `breaker_admits`,
  `breaker_success`, `breaker_failure` l. 1205-1243, `knn_fetch` l. 1408,
  `rerank` l. 1421); lease bookkeeping (`note_lease` l. 1867, `note_refresh`
  l. 1882 as changed by #293's review, `release_lease` l. 1771,
  `erase_session` l. 1648, `flush` l. 1507, `load_session` l. 1514).
* `project.rs`: `mirror_version` (l. 72), `project` (l. 105), `index_doc`
  (l. 172), `sole_session` (l. 197).
* `index.rs` and `fake.rs`: the engine's rules the tier relies on (external
  versions, a conflict counted as success, delete-by-query leaving a
  tombstone one version up), as the fake models them; `rank_by_cosine` in
  `src/store/vector_source.rs` (l. 246).

## The state machine

`St` is one session: the durable side `d` (vectors by node, contract, epoch,
erased flag, lease row and token), the writer's captured token `wtok`, the
index (`rt`: contract index × node → version and document; `mark`: the sync
marker's version and epoch), this store's `Tier` (`SessionTier`: sync, held
token, version token and counter, cached contract, committed epoch, failures,
backoffs, `repairing`, `again`), the repair task's position `rep`, the
in-flight mirror `mir`, requests orphaned by an abort or a deadline, a
half-done `load_session`, the read breaker, and a log of every index write
that landed and who made it. `step` takes one `Ev`: this writer's flush
(leased or not), another holder's flush (mirrored or not), one step of the
mirror or of the repair task, their deadlines, an orphaned request landing,
the two halves of a load, a read, acquire, the heartbeat's refresh, release
(waiting for the repair or abandoning it), a takeover, the operator override,
an erase by another process or through this store (stop, then sweep or
abandon), index and kNN outages, and time. Any interleaving is a list.

Atomicity: each repair step is one request or one lock hold (a fence check,
one bulk chunk, one sweep pass, the marker); the mirror's bulk lands item by
item, as the engine applies it, and a read sees the index at any point
between (a periodic refresh can expose any prefix). Chunks are one document
(the tests' `with_repair_chunk(1)`; with chunk `k` the one in-flight request
below is `k` documents). The read path's load and marker check are one step;
`load_session`'s are two (`loadBegin`, `loadEnd`), which is what #18 review M4
races.

## Results

* SQL authority. `indexSide_keeps_durable`: no mirror, repair, orphaned
  request, marker check, read, breaker change or outage touches the durable
  session. `write_commits`: an accepted flush is exactly the batch applied,
  whatever the index state; `mirror_ends`, `mirTimeout_ends`: the one mirror a
  flush waits for always ends. `lease_mono`, `run_lease_mono`: the token never
  goes back. `fenced_write_refused`.
* Convergence ("stale, never meaningless"). `repair_converges`: a pass that
  starts under the current lease with the marker behind and no further writes
  re-indexes every vector, sweeps the rest, writes the marker at the durable
  epoch and marks the session in sync, with the index matching the durable
  store (`Matches`). Version dominance (every document in the index below the
  pass's version, `hdom`) is an **assumption**, not an invariant:
  `nextVersion_gt` gives it only when the store's recorded token is at most
  the pass's, and a stale pass breaks that (`stale_pass_rewinds_version`).
  `putAll_eq` (first write at a version wins),
  `reconcile_matches`. `finish_overtaken`, `finish_inSync`: #18 review F4.
* Breaker (#18 review M2). `breaker_trips`, `breaker_counts`,
  `breaker_open_falls_back`, `breaker_one_probe`, `breaker_probe_outcome`; in the machine,
  `read_breaker_open` and `read_not_inSync_falls_back`: while open, or while
  not in sync, a read is the durable store's.
* Fencing (#18 review F2, #293). `repair_silent_after_loss`: once the lease moved
  past a token and the repair task is off its in-flight request, no repair write under
  that token ever lands, in any run; `erase_silences_repair` applies it to an
  erase or takeover. `mirror_starts_held`: a mirror starts only from an
  accepted flush with this store holding exactly the writer's token.
  `noteRefresh_never_adopts`, `fenced_iff`.
* Marker state. `checkMarker_inSync_iff`, `settle_consistent`,
  `settle_trusted`: after a load the session is trusted exactly when the
  final (re-checked) marker is in sync, unless a holder finds it behind and
  asks for a repair.
* Search (#18 review M3 and F7). `rerank_ignores_engine`: with every stored
  vector returned, the engine's scores and order do not matter, and the answer
  is `rank_by_cosine` over the candidates (`rerank_exact`, close to the
  definition: the durable store's exact scan run on the same candidates);
  `knnFetch_bounds`.
* Versions. `version_holder_first`, `version_counter`, `nextVersion_gt`.

Tests mirrored by `decide` runs: the mirror and served leg, the contract
refusal, the mirror failure and repair, #18 review F4, F2 and F6, the #293
re-take, the #18 review M4 ahead marker, #18 review L4, the breaker, the
overfetch, the exact re-rank and its boundary, and the projection table.

## What the model shows about the Rust

* `reembed_migration_misindexed`, `project_reembed_into_old_index`: a re-embed
  **migration** (`lambo re-embed`, `Graph::reembed_all`) drains every
  `UpsertNode` before the trailing `SetEmbedding`, but `project` switches the
  contract only for what follows a `SetEmbedding`. Through the tier the new
  vectors are written into the **old** contract's index, the new contract's
  index gets nothing, the marker advances and the session stays in sync: a
  read under the new contract is served from the index and finds no vector,
  no load repairs it (the marker is in sync), and a reader still holding the
  old contract ranks old-contract probes against new-model vectors under the
  old contract's name. The projection's own test and the tier test put
  `SetEmbedding` first, which is the order that works.
* `inplace_reembed_mixes_builds` (the #296 review's case): an in-place
  re-embed (`--rewrite-same-contract`) is one durable transaction, but the
  index takes the bulk item by item into the same contract's index while the
  session stays in sync, so a read in between ranks two builds together. It is
  brief for the holder (once the bulk and marker land the answer is one build;
  a bulk that fails part way makes the holder stale and fall back). The
  durable store never holds the mix.
* `reader_never_rechecks`, `reader_stays_inSync`: `sync_for_read` re-checks
  only `Unknown` and `Stale`; a reader that once found the marker in sync
  trusts the index until it reloads or evicts the session, whatever other
  processes do. A later failed mirror by the writer (or a partial in-place
  re-embed) is served from such a store until that reload; a node deleted
  durably stays in its vector leg meanwhile (recall's assembly drops it, per
  `a_stale_index_hit_never_reaches_the_assembled_recall`). Real readers bound
  the window: serve-web reloads on its snapshot TTL (`load_reader_graph` →
  `load_session` → `settle_after_load`), `lambo recall` is one-shot, and MCP
  serve layers are pinned holders. The theorem is about the store alone.
* `mirror_lands_after_erase`: `mirror_session` checks only this store's record
  of the lease, never the durable row; `ensure_fenced` guards repairs, not
  mirrors. A writer that stalls past its lease between the commit and the
  mirror, while another process erases the session (sweep, marker deleted,
  erase reported done), then writes the erased session's vector and marker
  back into the index. The repair in the same position writes nothing.
  The model's `eraseOther` has no lease-liveness gate; the real `erase_gate`
  answers `Held` while another holder's lease is live, so the erase needs the
  writer's lease to have lapsed (the stall past its TTL) first.
* `repair_from_pre_recheck_snapshot`: when `repair_once` finds the marker
  ahead and `recheck_ahead`'s fresh load finds it behind (another flush in
  between), it reconciles from the **first**, older snapshot, contrary to
  #18 review M4's "never repair from the older snapshot": it re-indexes a
  node deleted since and rewinds the marker. The #18 review F4 guard sends it
  round again and the rerun repairs it; meanwhile the holder falls back, but an in-sync reader elsewhere
  serves the deleted node.
* `edge_only_batch_leaves_marker_behind`: a batch of only `DeleteEdge`s names
  no session and deletes no node, so `mirror_unnamed` returns before
  attributing it and the marker stays behind the durable epoch: the holder's
  next load re-indexes the whole session and a reader falls back until the
  next mirrored flush.
* `stale_pass_rewinds_version`: `next_version` (`mod.rs:547-551`) restarts
  the counter whenever the token differs, an older token included, and
  `reconcile` calls it before any fence. A repair pass that captured token
  `T` and reaches `reconcile` after this store re-acquired at `T' > T` and
  mirrored under `T'` sets the recorded token back to `T`; its next fence
  stops it. The next `T'` write then restarts at counter 1, repeats a version
  already in the index, and its bulk and marker are refused as conflicts that
  count as success. The session ends in sync with the index serving the old
  vector, and since later mirrors advance the marker, no load repairs it:
  permanent silent staleness. This is also why `repair_converges` takes
  version dominance as a hypothesis.
* `rerank_partial_not_exact`: a hit returned without its vector keeps the
  engine's score and can outrank a nearer one (documented in `rerank`).
* `late_success_closes`: an index read admitted before the breaker opened
  and answering after it closes the breaker (`breaker_success` resets).

## The #273 seams (Track 3)

#273 proposes `tiered/repair.rs`, `tiered/mirror.rs` and `tiered/search.rs`,
keeping `TieredStore`/`Tier` state and the `GraphStore` delegation in
`tiered/mod.rs`:

* `tiered/repair.rs` (`request_repair`, `run_repairs`, `repair_once`,
  `reconcile`, `repair_if_due`, `stop_repair_for_erase`, `check_marker`,
  `settle_after_load`): `repStep` and `repair_converges`, `putAll_eq`,
  `reconcile_matches`, `bulk_loop`, `finish_overtaken`, `finish_inSync`,
  `repair_silent_after_loss`, `step_quiet`, `repStep_writing_imp`,
  `erase_silences_repair`, `checkMarker_inSync_iff`, `settle_consistent`,
  `settle_trusted`, and the pinned `repair_from_pre_recheck_snapshot` and
  `stale_pass_rewinds_version`. `recheck_ahead` and `load_durable` are not in
  #273's list and belong with it.
* `tiered/mirror.rs` (`mirror`, `mirror_unnamed`, `mirror_session`,
  `mirror_ops`, `breaker_*`, `fallback`, `sync_for_read`): `mirStep`,
  `mirror_starts_held`, `mirror_ends`, `write_commits`,
  `indexSide_keeps_durable`, the breaker theorems, `read_breaker_open`,
  `read_not_inSync_falls_back`, `syncForRead_inSync`, `reader_stays_inSync`,
  and the pinned `reembed_migration_misindexed`, `inplace_reembed_mixes_builds`,
  `mirror_lands_after_erase`, `edge_only_batch_leaves_marker_behind`,
  `reader_never_rechecks`. `project.rs` already holds `project` and
  `mirror_version` (`project_reembed_into_old_index`, the version theorems).
  The breaker and `sync_for_read` are read-path policy; #273 groups them with
  the mirror, and `read` here spans `mirror.rs` and `search.rs`.
* `tiered/search.rs` (`VectorCandidateSource`, `knn_fetch`, `rerank`):
  `rerank_exact`, `rerank_ignores_engine`, `rerank_partial_not_exact`,
  `knnFetch_bounds`.
* `tiered/mod.rs` keeps `note_lease`/`note_refresh` (`noteRefresh_never_adopts`),
  the lease delegation and erase (`lease_mono`, `fenced_write_refused`), and
  the shared primitives: `sweep` (used by `reconcile`, `mirror_unnamed` and
  `erase_session`), `ensure_fenced` (the repair's fence, and the mirror's once
  `mirror_lands_after_erase` is fixed), `next_version` (`nextVersion_gt`,
  `stale_pass_rewinds_version`) and `mark_stale`.

## What is left out

Exact arithmetic: versions and epochs are naturals (`u64` overflow is the
`mirror_version` refusal, modelled); scores are exact fixed-point cosines, so
`f32`/`f64` rounding in `rank_by_cosine` and the engine is outside the model.
Not modelled: the refresh interval as such (the realtime view stands for
every view a refresh can expose; delete-by-query is atomic with its refresh,
so the #18 review H1 refresh-first rule and version-conflict retries in
`sweep` and `erase_session` are assumed, not proved), `ensure_index`, index naming and
`validate_index_prefix`, widths (`index_doc`'s width check), interactions and
graph structure, multiple sessions (`mirror` over several sessions is reduced
to the one-session attribution rule), the eviction bound (#18 review L5) and
`repair_run` (#18 review L1), log messages and `last_error`, timeouts as
durations (a deadline is an event), the primary's own errors, `backfill_recall_index`,
the unchecked `vector_candidates` surface, and holder derive (#18 review M6).
-/

namespace Lambo.Tiered

/-! ## Versions (`project::mirror_version`, `Tier::next_version`) -/

/-- `1 << 32`: the counter's place in an external version. -/
def shift : Nat := 4294967296

/-- `u32::MAX`. -/
def u32Max : Nat := 4294967295

/-- `MAX_VERSIONED_TOKEN = (i64::MAX as u64) >> 32`. -/
def maxTok : Nat := 2147483647

/-- `mirror_version(token, counter)`: `(token << 32) | counter`, or `None` when
the token does not fit a positive signed 64-bit version. For a counter below
`1 << 32` (a `u32`) the `|` is `+`. -/
def mirrorVersion (t c : Nat) : Option Nat :=
  if t ≤ maxTok then some (t * shift + c) else none

/-- `next_version`: restart the counter for a new token, refuse to wrap it
(`checked_add`) or to overflow the version. Returns the new `(token, counter)`
and the version, or `none` (the Rust's error string). -/
def nextVersion (tok cnt t : Nat) : Option (Nat × Nat × Nat) :=
  let c := if tok = t then cnt else 0
  if c + 1 ≤ u32Max then
    match mirrorVersion t (c + 1) with
    | some v => some (t, c + 1, v)
    | none => none
  else none

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

/-- `MAX_VECTOR_CANDIDATE_LIMIT`. -/
def maxLimit : Nat := 2048

/-- `KNN_OVERFETCH`. -/
def overfetch : Nat := 16

/-- `knn_fetch(limit)`: `limit + 16`, capped at `max(2048, limit)`. -/
def knnFetch (limit : Nat) : Nat := min (limit + overfetch) (max maxLimit limit)

/-- **The over-fetch bound (#18 review F7).** For every limit the store accepts
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
batch in one transaction). -/
def applyMut (d : Durable) : Mut → Durable
  | .upsert n (some v) => { d with vecs := (n, v) :: d.vecs.filter (fun p => p.1 != n) }
  | .upsert n none => { d with vecs := d.vecs.filter (fun p => p.1 != n) }
  | .delete n => { d with vecs := d.vecs.filter (fun p => p.1 != n) }
  | .setContract c => { d with contract := c }
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
Times are whole seconds. -/
structure Params where
  threshold : Nat := 3
  cooldown : Nat := 30
  backoff : Nat := 60
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

/-- **The breaker trips** (#18 review M2): `threshold` consecutive failures from a closed
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

/-! ## The state machine -/

/-- One document slot of the index: its external version and its document
(`none`: absent or a delete tombstone, which keeps its version). -/
structure Entry where
  ver : Nat := 0
  doc : Option Vec := none
  deriving DecidableEq, Repr

/-- The index's realtime view: contract index, node id, entry. Every data
index `{prefix}-v-{hash}` is the first argument. One session throughout. -/
abbrev Idx := Nat → Nat → Entry

def Idx.empty : Idx := fun _ _ => {}

/-- A write at an external version: it lands only above the version the index
holds (a conflict otherwise, which the tier counts as success). -/
def put (rt : Idx) (c n v : Nat) (doc : Option Vec) : Idx :=
  if (rt c n).ver < v then fun c' n' => if c' = c ∧ n' = n then ⟨v, doc⟩ else rt c' n' else rt

def landed (rt : Idx) (c n v : Nat) : Bool := (rt c n).ver < v

/-- The marker index: `(version, synced_epoch)`, external versions as above. -/
def putMarker (mk : Option (Nat × Nat)) (v e : Nat) : Option (Nat × Nat) :=
  match mk with
  | some (w, _) => if w < v then some (v, e) else mk
  | none => some (v, e)

/-- `delete_session_docs(session, below)` after a refresh: every document of
the session in every data index, or those below a version, becomes a
tombstone (delete-by-query bumps the version). -/
def sweepBelow (rt : Idx) (below : Option Nat) : Idx := fun c n =>
  let e := rt c n
  if e.doc.isSome && (match below with | some v => decide (e.ver < v) | none => true)
  then ⟨e.ver + 1, none⟩ else e

/-- `delete_ids(ids)` after a refresh: those nodes in every data index. -/
def deleteIds (rt : Idx) (ids : List Nat) : Idx := fun c n =>
  let e := rt c n
  if e.doc.isSome && ids.contains n then ⟨e.ver + 1, none⟩ else e

/-- The per-session tier state of this store (`SessionTier`). Left out: the
contract-switch warnings, `ahead_warned`, `last_error`, `repair_run` (the
guard's run id: an aborted run's late cleanup is not modelled), `last_used`. -/
structure Tier where
  sync : Sync := .unknown
  held : Option Nat := none
  tok : Nat := 0
  cnt : Nat := 0
  contract : Option (Option Nat) := none
  committed : Nat := 0
  failures : Nat := 0
  nextRepair : Option Nat := none
  nextCheck : Option Nat := none
  repairing : Bool := false
  again : Bool := false
  deriving DecidableEq, Repr

/-- Where this store's repair task is (`run_repairs`, `repair_once`,
`reconcile`). `t` is the lease token the pass runs under, `v` its version,
`snap` the durable snapshot it loaded. Chunks are one document (the tests'
`with_repair_chunk(1)`). -/
inductive RPc where
  | idle
  | head
  | load (t : Nat)
  | check (t : Nat) (snap : Snap)
  | version (t : Nat) (snap : Snap)
  | bulkFence (t v : Nat) (ops : List (Nat × Vec)) (snap : Snap)
  | bulkPut (t v : Nat) (ops : List (Nat × Vec)) (snap : Snap)
  | sweepFence (t v : Nat) (snap : Snap)
  | sweep (t v : Nat) (snap : Snap)
  | markerFence (t v : Nat) (snap : Snap)
  | marker (t v : Nat) (snap : Snap)
  | finish (snap : Snap)
  deriving DecidableEq, Repr

/-- The mirror of the last committed batch (`mirror_ops`): the bulk's items
still to land, then the marker. A bulk is applied item by item by the engine. -/
inductive MPc where
  | idle
  | puts (t v e : Nat) (ops : List Op)
  | marker (t v e : Nat)
  deriving DecidableEq, Repr

/-- A request already sent when its task was aborted or timed out: it may
still land. -/
inductive Item where
  | doc (c n v : Nat) (d : Option Vec)
  | marker (v e : Nat)
  deriving DecidableEq, Repr

/-- Who made an index write that landed. -/
inductive Src where
  | mirror (t : Nat)
  | repair (t : Nat)
  | other
  | erase
  | unattributed
  deriving DecidableEq, Repr

/-- What a checked vector read returned: the durable store's own read, an
empty leg, the contract refusal, or the index's documents in contract `c`. -/
inductive Answer where
  | fallback
  | empty
  | refused
  | index (c : Nat) (view : Nat → Option Vec)

structure St where
  /-- The durable store, the authority. -/
  d : Durable := {}
  /-- The token the session's writer captured at its acquire. -/
  wtok : Option Nat := none
  rt : Idx := Idx.empty
  mark : Option (Nat × Nat) := none
  /-- The index is unreachable. -/
  down : Bool := false
  /-- Only kNN fails (a query-side outage, or a read past `READ_DEADLINE`). -/
  knnDown : Bool := false
  now : Nat := 0
  tier : Tier := {}
  rep : RPc := .idle
  mir : MPc := .idle
  orphans : List (Src × Item) := []
  /-- A `load_session` that has read the durable snapshot and not yet the
  marker. -/
  loading : Option Snap := none
  br : Breaker := {}
  /-- The other holder's flush counter. -/
  otherCnt : Nat := 0
  answer : Option Answer := none
  /-- An erase through this store has stopped the repair and not yet swept. -/
  erasing : Bool := false
  log : List Src := []

/-- What a durable load returns: `SessionNotFound` once erased. -/
def snapOf (d : Durable) : Snap := if d.erased then none else some d

def snapContract (snap : Snap) : Option Nat := snap.bind (·.contract)

def snapVecs : Snap → List (Nat × Vec)
  | some d => d.vecs
  | none => []

def due (now : Nat) : Option Nat → Bool
  | none => true
  | some a => decide (a ≤ now)

/-- The durable lease row as `read_lease` reports it. -/
def durableLease (d : Durable) : Option Nat := if d.row = .none then none else some d.lease

section Machine

variable (P : Params)

def setTier (s : St) (t : Tier) : St := { s with tier := t }

/-- `mark_stale`: stale, one more failure. -/
def markStale (s : St) : St :=
  { s with tier := { s.tier with sync := .stale, failures := s.tier.failures + 1 } }

/-- `request_repair`: single flight; a request while one runs asks for one
rerun. -/
def requestRepair (s : St) : St :=
  if s.tier.repairing then { s with tier := { s.tier with again := true } }
  else { s with tier := { s.tier with repairing := true, again := false }, rep := .head }

/-- `repair_if_due`. -/
def repairIfDue (s : St) : St := if due s.now s.tier.nextRepair then requestRepair s else s

/-- `settle_after_load(session, snap)`, with `recheck_ahead`'s fresh load. -/
def settle (s : St) (snap : Snap) : St :=
  let held := s.tier.held
  let s := { s with tier := { s.tier with contract := some (snapContract snap) } }
  let m₀ := checkMarker s.down s.mark snap
  let m := if m₀ = .ahead then checkMarker s.down s.mark (snapOf s.d) else m₀
  let s := if m₀ = .ahead then
    { s with tier := { s.tier with contract := some (snapContract (snapOf s.d)) } } else s
  match m, held with
  | .behind, some _ => requestRepair s
  | m, h =>
    if s.tier.repairing then s else
    { s with tier := { s.tier with
        sync := m.sync
        nextCheck := (if m = .ahead ∨ (m = .behind ∧ h = none) then some (s.now + P.backoff)
          else s.tier.nextCheck)
        nextRepair := (match m with
          | .ahead => some (s.now + P.backoff)
          | .inSync => none
          | _ => s.tier.nextRepair) } }

/-- `sync_for_read`'s re-check: a durable load, then `settle_after_load`. -/
def recheck (s : St) : St :=
  let s := settle P s (snapOf s.d)
  if s.tier.sync = .inSync then s
  else { s with tier := { s.tier with nextCheck := some (s.now + P.backoff) } }

/-- `sync_for_read`. -/
def syncForRead (s : St) : St :=
  let dueC := due s.now s.tier.nextCheck
  match s.tier.sync with
  | .inSync => s
  | .unknown => if dueC then recheck P s else s
  | .stale =>
    match s.tier.held with
    | some _ => if s.tier.repairing then s else repairIfDue s
    | none => if dueC then recheck P s else s

/-- `checked_vector_candidates` (`VectorCandidateSource`), from the tier's
side; the answer's ranking is `rerank`'s, modelled below. -/
def read (exp : Nat) (s : St) : St :=
  let s := syncForRead P s
  if s.tier.sync ≠ .inSync then { s with answer := some .fallback } else
  match s.tier.contract.bind id with
  | none => { s with answer := some .empty }
  | some c =>
    if c ≠ exp then { s with answer := some .refused } else
    match admits P s.now s.br with
    | (false, b) => { s with br := b, answer := some .fallback }
    | (true, b) =>
      if s.down || s.knnDown then { s with br := onFailure P s.now b, answer := some .fallback }
      else { s with br := onSuccess b, answer := some (.index c (fun n => (s.rt c n).doc)) }

/-- `mark_unleased`. -/
def markUnleased (s : St) : St :=
  { s with tier := { s.tier with sync := .stale, contract := none } }

/-- `mirror_session` after the primary committed `ms` as `d'` under token `t`. -/
def mirrorSession (s : St) (ms : List Mut) (t : Option Nat) (d' : Durable) : St :=
  let s := { s with tier := { s.tier with committed := max s.tier.committed d'.epoch } }
  match t with
  | none => markUnleased s
  | some t =>
    if s.tier.sync ≠ .inSync then repairIfDue s
    else if s.tier.held ≠ some t then markStale s
    else match nextVersion s.tier.tok s.tier.cnt t with
      | none => markStale s
      | some (tok, cnt, v) =>
        let before := match s.tier.contract with
          | some c => c
          | none => d'.contract
        let pr := project ms before
        { s with tier := { s.tier with tok := tok, cnt := cnt, contract := some pr.2 },
                 mir := .puts t v d'.epoch pr.1 }

/-- Whether a batch names its session (`sole_session`). -/
def namesSession (ms : List Mut) : Bool :=
  ms.any fun m => match m with
    | .upsert .. | .setContract _ | .other => true
    | .delete _ | .deleteEdge => false

def deletedIds (ms : List Mut) : List Nat :=
  ms.filterMap fun m => match m with
    | .delete n => some n
    | _ => none

/-- `mirror` (`mirror_unnamed` for a batch that names no session). -/
def mirror (s : St) (ms : List Mut) (t : Option Nat) (d' : Durable) : St :=
  if namesSession ms then mirrorSession s ms t d' else
  let del := deletedIds ms
  if del = [] then s
  else if t.isSome ∧ s.tier.held = t then mirrorSession s ms t d'
  else if s.down then markStale s
  else { s with rt := deleteIds s.rt del, log := .unattributed :: s.log }

/-- A pass ended without error: take a queued rerun or end the run. -/
def passOk (s : St) : St :=
  if s.tier.again then { s with tier := { s.tier with again := false }, rep := .head }
  else { s with tier := { s.tier with repairing := false }, rep := .idle }

/-- A pass failed (index error, deadline): stale, backoff, run ended. -/
def passFail (s : St) : St :=
  let s := markStale s
  { s with
    tier := { s.tier with
      nextRepair := some (s.now + P.backoff)
      again := false
      repairing := false }
    rep := .idle }

/-- The lease was lost (`StaleWrite` and the fence still fails): stop without
another write and stop calling it held. Not an index failure. -/
def passLost (s : St) (t : Nat) : St :=
  { s with
    tier := { s.tier with
      held := (if s.tier.held = some t then none else s.tier.held)
      sync := .stale
      again := false
      repairing := false }
    rep := .idle }

def isFenced (s : St) (t : Nat) : Bool := fenced s.tier.held (durableLease s.d) t

/-- One step of the repair task. -/
def repStep (s : St) : St :=
  match s.rep with
  | .idle => s
  | .head =>
    match s.tier.held with
    | none => { s with tier := { s.tier with repairing := false, again := false }, rep := .idle }
    | some t => { s with rep := .load t }
  | .load t =>
    let snap := snapOf s.d
    { s with tier := { s.tier with contract := some (snapContract snap) }, rep := .check t snap }
  | .check t snap =>
    let m₀ := checkMarker s.down s.mark snap
    let m := if m₀ = .ahead then checkMarker s.down s.mark (snapOf s.d) else m₀
    let s := if m₀ = .ahead then
      { s with tier := { s.tier with contract := some (snapContract (snapOf s.d)) } } else s
    match m with
    -- Reconciles from `snap`, the snapshot loaded before any re-check.
    | .behind => { s with rep := .version t snap }
    | .unreachable => passFail P s
    | m =>
      passOk { s with
        tier := { s.tier with
          sync := m.sync
          nextRepair := (if m = .ahead then some (s.now + P.backoff) else none)
          nextCheck := (if m = .ahead then some (s.now + P.backoff) else s.tier.nextCheck) } }
  | .version t snap =>
    match nextVersion s.tier.tok s.tier.cnt t with
    | none => passFail P s
    | some (tok, cnt, v) =>
      let tr := { s.tier with
        tok := tok
        cnt := cnt
        sync := (if s.tier.sync = .inSync then .unknown else s.tier.sync) }
      match snapContract snap with
      | some _ => { s with tier := tr, rep := .bulkFence t v (snapVecs snap) snap }
      | none => { s with tier := tr, rep := .sweepFence t v snap }
  | .bulkFence t v ops snap =>
    match ops with
    | [] => { s with rep := .sweepFence t v snap }
    | _ => if isFenced s t then { s with rep := .bulkPut t v ops snap } else passLost s t
  | .bulkPut t v ops snap =>
    match ops, snapContract snap with
    | (n, x) :: ops', some c =>
      if s.down then passFail P s
      else { s with rt := put s.rt c n v (some x), log := .repair t :: s.log,
                    rep := .bulkFence t v ops' snap }
    | _, _ => { s with rep := .sweepFence t v snap }
  | .sweepFence t v snap =>
    if isFenced s t then { s with rep := .sweep t v snap } else passLost s t
  | .sweep t v snap =>
    if s.down then passFail P s
    else { s with rt := sweepBelow s.rt (some v), log := .repair t :: s.log,
                  rep := .markerFence t v snap }
  | .markerFence t v snap =>
    if isFenced s t then { s with rep := .marker t v snap } else passLost s t
  | .marker t v snap =>
    if s.down then passFail P s
    else { s with mark := putMarker s.mark v (snapEpoch snap), log := .repair t :: s.log,
                  rep := .finish snap }
  | .finish snap =>
    if s.tier.committed > snapEpoch snap then
      passOk { s with tier := { s.tier with again := true } }
    else passOk { s with tier := { s.tier with sync := .inSync, nextRepair := none } }

/-- The request the repair task has on the wire, if it is at a write. -/
def repInflight : RPc → List (Src × Item)
  | .bulkPut t v ((n, x) :: _) snap =>
    match snapContract snap with
    | some c => [(.repair t, .doc c n v (some x))]
    | none => []
  | .marker t v snap => [(.repair t, .marker v (snapEpoch snap))]
  | _ => []

/-- The mirror's requests on the wire: the rest of the bulk, or the marker. -/
def mirInflight : MPc → List (Src × Item)
  | .puts t v _ ops => ops.map fun o => (.mirror t, .doc o.1 o.2.1 v o.2.2)
  | .marker t v e => [(.mirror t, .marker v e)]
  | .idle => []

/-- One step of the mirror. -/
def mirStep (s : St) : St :=
  match s.mir with
  | .idle => s
  | .puts t v e [] => { s with mir := .marker t v e }
  | .puts t v e ((c, n, x) :: ops) =>
    if s.down then { markStale s with mir := .idle }
    else { s with rt := put s.rt c n v x, log := .mirror t :: s.log, mir := .puts t v e ops }
  | .marker t v e =>
    if s.down then { markStale s with mir := .idle }
    else { s with mark := putMarker s.mark v e, log := .mirror t :: s.log, mir := .idle }

/-- An orphaned request lands, logged under the task that sent it. -/
def landItem (s : St) (src : Src) : Item → St
  | .doc c n v x => { s with rt := put s.rt c n v x, log := src :: s.log }
  | .marker v e => { s with mark := putMarker s.mark v e, log := src :: s.log }

/-- The events. -/
inductive Ev where
  /-- The writer's flush (`TieredStore::flush`) under its captured token. -/
  | write (ms : List Mut)
  /-- An unleased flush (the seed and fixture path). -/
  | writeUnleased (ms : List Mut)
  /-- Another holder's flush, its mirror landing whole or not at all. -/
  | otherWrite (ms : List Mut) (mirrored : Bool)
  | mirStep
  /-- `MIRROR_DEADLINE` passes. -/
  | mirTimeout
  | repStep
  /-- `REPAIR_DEADLINE` passes. -/
  | repTimeout
  | orphanLand
  | orphanDrop
  /-- `load_session`: the durable read, then (`loadEnd`) the marker check. -/
  | loadBegin
  | loadEnd
  /-- A checked vector read expecting contract `exp`. -/
  | read (exp : Nat)
  /-- This process's writer acquires (`acquire_lease`); another holder's
  lapsed lease is taken over. -/
  | acquire
  /-- The heartbeat's refresh. -/
  | refresh
  /-- `release_lease`: `wait` if the in-flight repair finished within
  `RELEASE_GRACE`, otherwise it is aborted. -/
  | release (wait : Bool)
  /-- Another process takes over the lapsed lease. -/
  | takeover
  /-- The operator override: the row released by hand. -/
  | override
  /-- `erase_session` run by another process (another `TieredStore`). -/
  | eraseOther
  /-- `erase_session` through this store: the durable erase and
  `stop_repair_for_erase`'s fence; then the sweep (`eraseFinish`, once the
  repair stopped) or the abandonment (`eraseAbort`, past the grace). -/
  | eraseStop
  | eraseFinish
  | eraseAbort
  | setDown (b : Bool)
  | setKnnDown (b : Bool)
  | tick (dt : Nat)
  deriving Repr

/-- Erase the durable session: tombstone the lease above every token. -/
def eraseDurable (d : Durable) : Durable :=
  { d with erased := true, vecs := [], contract := none, lease := d.lease + 1, row := .erased }

def step (s : St) : Ev → St
  | .write ms =>
    if s.mir = .idle ∧ s.d.erased = false ∧ s.wtok = some s.d.lease then
      let d' := applyBatch s.d ms
      mirror { s with d := d' } ms s.wtok d'
    else s
  | .writeUnleased ms =>
    if s.mir = .idle ∧ s.d.erased = false ∧ s.d.row = .none then
      let d' := applyBatch s.d ms
      mirror { s with d := d' } ms none d'
    else s
  | .otherWrite ms mirrored =>
    if s.d.erased = false ∧ s.d.row = .other then
      let d' := applyBatch s.d ms
      let s' := { s with d := d' }
      if mirrored ∧ s.down = false then
        match mirrorVersion s.d.lease (s.otherCnt + 1) with
        | some v =>
          let pr := project ms s.d.contract
          { s' with
            rt := pr.1.foldl (fun rt o => put rt o.1 o.2.1 v o.2.2) s.rt,
            mark := putMarker s.mark v d'.epoch, otherCnt := s.otherCnt + 1, log := .other :: s.log }
        | none => s'
      else s'
    else s
  | .mirStep => mirStep s
  | .mirTimeout =>
    if s.mir = .idle then s
    else { markStale s with mir := .idle, orphans := s.orphans ++ mirInflight s.mir }
  | .repStep => repStep P s
  | .repTimeout =>
    match s.rep with
    | .idle | .head => s
    | r => passFail P { s with orphans := s.orphans ++ repInflight r }
  | .orphanLand =>
    match s.orphans with
    | (src, it) :: rest => landItem { s with orphans := rest } src it
    | [] => s
  | .orphanDrop => { s with orphans := [] }
  | .loadBegin => { s with loading := some (snapOf s.d) }
  | .loadEnd =>
    match s.loading with
    | some snap => settle P { s with loading := none } snap
    | none => s
  | .read exp => read P exp s
  | .acquire =>
    if s.d.erased then { s with tier := { s.tier with held := none } }
    else if s.d.row = .ours then
      { s with tier := { s.tier with held := some s.d.lease }, wtok := some s.d.lease }
    else
      let t := s.d.lease + 1
      { s with d := { s.d with lease := t, row := .ours },
               tier := { s.tier with held := some t }, wtok := some t }
  | .refresh =>
    match s.d.row with
    | .ours => { s with tier := { s.tier with held := noteRefresh s.tier.held (.acquired s.d.lease) } }
    | .released | .none =>
      let t := s.d.lease + 1
      { s with d := { s.d with lease := t, row := .ours },
               tier := { s.tier with held := noteRefresh s.tier.held (.acquired t) } }
    | .other | .erased => { s with tier := { s.tier with held := none } }
  | .release wait =>
    if wait ∧ s.rep ≠ .idle then s else
    let s := if s.rep = .idle then s
      else { s with orphans := s.orphans ++ repInflight s.rep, rep := .idle,
                    tier := { s.tier with repairing := false, again := false } }
    let d := if s.d.row = .ours then { s.d with row := .released } else s.d
    let tr := { s.tier with held := none }
    { s with d := d, wtok := none, tier := if tr.repairing then tr else {} }
  | .takeover =>
    if s.d.erased then s
    else { s with d := { s.d with lease := s.d.lease + 1, row := .other }, otherCnt := 0 }
  | .override => if s.d.erased then s else { s with d := { s.d with row := .released } }
  | .eraseOther =>
    if s.d.erased then s
    else { s with d := eraseDurable s.d, rt := sweepBelow s.rt none, mark := none,
                  log := .erase :: s.log }
  | .eraseStop =>
    if s.d.erased then s
    else { s with d := eraseDurable s.d, tier := { s.tier with held := none, again := false },
                  erasing := true }
  | .eraseFinish =>
    if s.erasing ∧ s.rep = .idle then
      if s.down then { s with erasing := false }
      else { s with rt := sweepBelow s.rt none, mark := none, tier := {}, erasing := false,
                    log := .erase :: s.log }
    else s
  | .eraseAbort =>
    if s.erasing ∧ s.rep ≠ .idle then
      { s with orphans := s.orphans ++ repInflight s.rep, rep := .idle, erasing := false,
               tier := { s.tier with repairing := false } }
    else s
  | .setDown b => { s with down := b }
  | .setKnnDown b => { s with knnDown := b }
  | .tick dt => { s with now := s.now + dt }

def run (s : St) : List Ev → St
  | [] => s
  | e :: es => run (step P s e) es

/-- `n` of the same event. -/
def steps (e : Ev) (n : Nat) : List Ev := List.replicate n e

end Machine

/-! ## The durable store stays authoritative -/

section Authority

variable (P : Params)

theorem markStale_d (s : St) : (markStale s).d = s.d := rfl
theorem requestRepair_d (s : St) : (requestRepair s).d = s.d := by
  unfold requestRepair; split <;> rfl
theorem repairIfDue_d (s : St) : (repairIfDue s).d = s.d := by
  unfold repairIfDue; split
  · exact requestRepair_d s
  · rfl
theorem passOk_d (s : St) : (passOk s).d = s.d := by unfold passOk; split <;> rfl
theorem passFail_d (s : St) : (passFail P s).d = s.d := rfl
theorem passLost_d (s : St) (t : Nat) : (passLost s t).d = s.d := rfl

theorem settle_d (s : St) (snap : Snap) : (settle P s snap).d = s.d := by
  unfold settle
  simp only
  split
  · rw [requestRepair_d]; split <;> rfl
  · split
    · split <;> rfl
    · split <;> rfl

theorem recheck_d (s : St) : (recheck P s).d = s.d := by
  unfold recheck; simp only; split <;> simp [settle_d]

theorem syncForRead_d (s : St) : (syncForRead P s).d = s.d := by
  unfold syncForRead
  simp only
  split
  · rfl
  · split
    · exact recheck_d P s
    · rfl
  · split
    · split
      · rfl
      · exact repairIfDue_d s
    · split
      · exact recheck_d P s
      · rfl

theorem read_d (exp : Nat) (s : St) : (read P exp s).d = s.d := by
  unfold read
  simp only
  split
  · simp [syncForRead_d]
  · split
    · simp [syncForRead_d]
    · split
      · simp [syncForRead_d]
      · split <;> (try split) <;> simp [syncForRead_d]

theorem repStep_d (s : St) : (repStep P s).d = s.d := by
  unfold repStep
  split
  · rfl
  · split <;> rfl
  · rfl
  · simp only
    split
    · split <;> rfl
    · rw [passFail_d]; split <;> rfl
    · rw [passOk_d]; split <;> rfl
  · split
    · rfl
    · simp only; split <;> rfl
  · split
    · rfl
    · split
      · rfl
      · exact passLost_d s _
  · split
    · split
      · rfl
      · rfl
    · rfl
  · split
    · rfl
    · exact passLost_d s _
  · split <;> rfl
  · split
    · rfl
    · exact passLost_d s _
  · split <;> rfl
  · split <;> simp [passOk_d]

theorem mirStep_d (s : St) : (mirStep s).d = s.d := by
  unfold mirStep
  split <;> (try split) <;> rfl

theorem mirrorSession_d (s : St) (ms : List Mut) (t : Option Nat) (d' : Durable) :
    (mirrorSession s ms t d').d = s.d := by
  unfold mirrorSession
  simp only
  split
  · rfl
  · split
    · exact repairIfDue_d _
    · split
      · rfl
      · split <;> rfl

theorem mirror_d (s : St) (ms : List Mut) (t : Option Nat) (d' : Durable) :
    (mirror s ms t d').d = s.d := by
  unfold mirror
  split
  · exact mirrorSession_d s ms t d'
  · simp only
    split
    · rfl
    · split
      · exact mirrorSession_d s ms t d'
      · split <;> rfl

theorem landItem_d (s : St) (src : Src) (it : Item) : (landItem s src it).d = s.d := by
  cases it <;> rfl

/-- The events that act on the index or on this store's tier state only. -/
def Ev.indexSide : Ev → Bool
  | .mirStep | .mirTimeout | .repStep | .repTimeout | .orphanLand | .orphanDrop
  | .loadBegin | .loadEnd | .read _ | .eraseFinish | .eraseAbort
  | .setDown _ | .setKnnDown _ | .tick _ => true
  | _ => false

/-- **The index never touches the durable store.** Every mirror, repair,
orphaned request, marker check, read, breaker change and index outage leaves
the durable session exactly as it was. -/
theorem indexSide_keeps_durable (s : St) (e : Ev) (h : e.indexSide = true) :
    (step P s e).d = s.d := by
  cases e <;> simp only [Ev.indexSide, Bool.false_eq_true] at h <;> simp only [step]
  all_goals first
    | rfl
    | exact mirStep_d s
    | exact repStep_d P s
    | exact read_d P _ s
    | (split <;> first
        | rfl
        | exact landItem_d _ _ _
        | exact settle_d P _ _
        | (split <;> rfl))

/-- **A committed flush is exactly the batch, whatever the index does.** With
the flush loop free (the previous flush's mirror finished, which
`MIRROR_DEADLINE` bounds) and the writer's token current, the durable session
after the flush is the batch applied to it: index down, stale, in sync or
mid-repair alike (`a_mirror_failure_keeps_the_flush_...`). -/
theorem write_commits (s : St) (ms : List Mut) (hm : s.mir = .idle) (he : s.d.erased = false)
    (ht : s.wtok = some s.d.lease) : (step P s (.write ms)).d = applyBatch s.d ms := by
  simp only [step, hm, he, ht, and_self, ite_true]
  exact mirror_d _ _ _ _

/-- A fenced writer's flush (its token no longer current) is refused by the
primary and mirrors nothing. -/
theorem fenced_write_refused (s : St) (ms : List Mut) (ht : s.wtok ≠ some s.d.lease) :
    step P s (.write ms) = s := by
  simp [step, ht]

theorem applyMut_lease (d : Durable) (m : Mut) : (applyMut d m).lease = d.lease := by
  cases m with
  | upsert n v => cases v <;> rfl
  | _ => rfl

theorem foldl_applyMut_lease (ms : List Mut) (d : Durable) :
    (ms.foldl applyMut d).lease = d.lease ∧ (ms.foldl applyMut d).row = d.row ∧
      (ms.foldl applyMut d).erased = d.erased := by
  induction ms generalizing d with
  | nil => exact ⟨rfl, rfl, rfl⟩
  | cons m ms ih =>
    obtain ⟨h1, h2, h3⟩ := ih (applyMut d m)
    refine ⟨h1.trans (applyMut_lease d m), h2.trans ?_, h3.trans ?_⟩ <;>
      (cases m with
       | upsert n v => cases v <;> rfl
       | _ => rfl)

theorem applyBatch_lease (d : Durable) (ms : List Mut) :
    (applyBatch d ms).lease = d.lease ∧ (applyBatch d ms).row = d.row ∧
      (applyBatch d ms).erased = d.erased :=
  foldl_applyMut_lease ms d

/-- **The fencing token never goes back**, over every event (#1). -/
theorem lease_mono (s : St) (e : Ev) : s.d.lease ≤ (step P s e).d.lease := by
  by_cases hi : e.indexSide = true
  · rw [indexSide_keeps_durable P s e hi]; exact Nat.le_refl _
  · cases e <;> simp only [Ev.indexSide, Bool.false_eq_true, not_true_eq_false] at hi <;>
      simp only [step]
    all_goals (repeat' split) <;>
      simp [mirror_d, (applyBatch_lease _ _).1, eraseDurable]

theorem run_lease_mono (s : St) (evs : List Ev) : s.d.lease ≤ (run P s evs).d.lease := by
  induction evs generalizing s with
  | nil => exact Nat.le_refl _
  | cons e es ih => exact Nat.le_trans (lease_mono P s e) (ih _)

end Authority

/-! ## A fenced handle never mirrors or repairs -/

section Fence

variable (P : Params)

/-- The repair task is at a write under token `t` (a bulk chunk, a sweep pass
or the marker): its fence passed and the request is on the wire. -/
def RPc.writing (t : Nat) : RPc → Bool
  | .bulkPut t' _ _ _ | .sweep t' _ _ | .marker t' _ _ => t' == t
  | _ => false

/-- What has to hold, once the lease moved past `t`, for no repair write under
`t` to land again: the token is in the past, a lease row exists, the repair
task is not mid-request under `t`, and no aborted repair request under `t` is
still on the wire. -/
structure Quiet (t : Nat) (s : St) : Prop where
  past : t < s.d.lease
  row : s.d.row ≠ .none
  notWriting : s.rep.writing t = false
  noOrphan : ∀ x ∈ s.orphans, x.1 ≠ .repair t

theorem isFenced_past {s : St} {t : Nat} (hq : Quiet t s) : isFenced s t = false := by
  have hd : durableLease s.d = some s.d.lease := by simp [durableLease, hq.row]
  unfold isFenced
  rw [hd]
  cases h : fenced s.tier.held (some s.d.lease) t
  · rfl
  · have := ((fenced_iff _ _ _).mp h).2
    have := hq.past
    omega

@[simp] theorem passOk_orphans (s : St) : (passOk s).orphans = s.orphans := by
  unfold passOk; split <;> rfl
@[simp] theorem passOk_log (s : St) : (passOk s).log = s.log := by
  unfold passOk; split <;> rfl
@[simp] theorem passOk_mir (s : St) : (passOk s).mir = s.mir := by
  unfold passOk; split <;> rfl

theorem passOk_rep (s : St) : (passOk s).rep = .idle ∨ (passOk s).rep = .head := by
  unfold passOk; split <;> simp

/-- A repair write under `t` starts only from a passing fence under `t`. -/
theorem repStep_writing_imp (s : St) (t : Nat) (h : (repStep P s).rep.writing t = true) :
    s.rep.writing t = true ∨ isFenced s t = true := by
  cases hr : s.rep <;> simp only [repStep, hr] at h
  all_goals (repeat' split at h) <;>
    (try simp only [passFail, passLost, markStale, RPc.writing, Bool.false_eq_true] at h) <;>
    (try (rcases passOk_rep _ with h' | h' <;> rw [h'] at h <;> simp at h)) <;>
    simp_all [RPc.writing]

theorem repStep_orphans (s : St) : (repStep P s).orphans = s.orphans := by
  cases hr : s.rep <;> simp only [repStep, hr]
  all_goals (repeat' split) <;> simp [passFail, passLost, markStale]

theorem repStep_log (s : St) (t : Nat) (hw : s.rep.writing t = false) :
    ∃ new, (repStep P s).log = new ++ s.log ∧ Src.repair t ∉ new := by
  cases hr : s.rep <;> simp only [repStep, hr] <;> simp only [hr, RPc.writing] at hw
  all_goals (repeat' split)
  all_goals first
    | (refine ⟨[], ?_, List.not_mem_nil⟩
       simp [passFail, passLost, markStale]; done)
    | (refine ⟨[_], rfl, ?_⟩
       simp only [List.mem_singleton, Src.repair.injEq]
       intro hm; subst hm; simp at hw)

theorem repInflight_src (r : RPc) (t : Nat) (h : ∃ it, (Src.repair t, it) ∈ repInflight r) :
    r.writing t = true := by
  obtain ⟨it, h⟩ := h
  unfold repInflight at h
  split at h
  · split at h
    · simp only [List.mem_singleton, Prod.mk.injEq, Src.repair.injEq] at h
      simp [RPc.writing, h.1]
    · simp at h
  · simp only [List.mem_singleton, Prod.mk.injEq, Src.repair.injEq] at h
    simp [RPc.writing, h.1]
  · simp at h

theorem mirInflight_src (m : MPc) (t : Nat) (it : Item) : (Src.repair t, it) ∉ mirInflight m := by
  unfold mirInflight
  split <;> simp

theorem mirror_log (s : St) (ms : List Mut) (t : Option Nat) (d' : Durable) :
    (mirror s ms t d').log = s.log ∨ (mirror s ms t d').log = .unattributed :: s.log := by
  have hs : ∀ s', (mirrorSession s' ms t d').log = s'.log := by
    intro s'
    unfold mirrorSession
    simp only
    split
    · rfl
    · split
      · unfold repairIfDue requestRepair; split <;> (try split) <;> rfl
      · split
        · rfl
        · split <;> rfl
  unfold mirror
  split
  · exact Or.inl (hs _)
  · simp only
    split
    · exact Or.inl rfl
    · split
      · exact Or.inl (hs _)
      · split
        · exact Or.inl rfl
        · exact Or.inr rfl

theorem mirror_tier_rep (s : St) (ms : List Mut) (t : Option Nat) (d' : Durable) :
    (mirror s ms t d').rep = s.rep ∨ (mirror s ms t d').rep = .head := by
  have hr : ∀ s', (requestRepair s').rep = s'.rep ∨ (requestRepair s').rep = .head := by
    intro s'; unfold requestRepair; split
    · exact Or.inl rfl
    · exact Or.inr rfl
  have hs : ∀ s', (mirrorSession s' ms t d').rep = s'.rep ∨ (mirrorSession s' ms t d').rep = .head := by
    intro s'
    unfold mirrorSession
    simp only
    split
    · exact Or.inl rfl
    · split
      · unfold repairIfDue; split
        · exact hr _
        · exact Or.inl rfl
      · split
        · exact Or.inl rfl
        · split
          · exact Or.inl rfl
          · exact Or.inl rfl
  unfold mirror
  split
  · exact hs _
  · simp only
    split
    · exact Or.inl rfl
    · split
      · exact hs _
      · split <;> exact Or.inl rfl

theorem mirror_orphans (s : St) (ms : List Mut) (t : Option Nat) (d' : Durable) :
    (mirror s ms t d').orphans = s.orphans := by
  have hs : ∀ s', (mirrorSession s' ms t d').orphans = s'.orphans := by
    intro s'
    unfold mirrorSession
    simp only
    split
    · rfl
    · split
      · unfold repairIfDue requestRepair; split <;> (try split) <;> rfl
      · split
        · rfl
        · split <;> rfl
  unfold mirror
  split
  · exact hs _
  · simp only
    split
    · rfl
    · split
      · exact hs _
      · split <;> rfl

theorem settle_aux (s : St) (snap : Snap) :
    ((settle P s snap).rep = s.rep ∨ (settle P s snap).rep = .head) ∧
      (settle P s snap).orphans = s.orphans ∧ (settle P s snap).log = s.log := by
  unfold settle requestRepair
  by_cases h1 : checkMarker s.down s.mark snap = .ahead <;>
    simp only [h1, ite_true, ite_false] <;> split <;> (try split) <;> simp

theorem settle_rep (s : St) (snap : Snap) :
    (settle P s snap).rep = s.rep ∨ (settle P s snap).rep = .head := (settle_aux P s snap).1

theorem read_aux (exp : Nat) (s : St) :
    ((read P exp s).rep = s.rep ∨ (read P exp s).rep = .head) ∧
      (read P exp s).orphans = s.orphans ∧ (read P exp s).log = s.log := by
  have hr : ∀ s', ((recheck P s').rep = s'.rep ∨ (recheck P s').rep = .head) ∧
      (recheck P s').orphans = s'.orphans ∧ (recheck P s').log = s'.log := by
    intro s'
    unfold recheck
    simp only
    have h := settle_aux P s' (snapOf s'.d)
    split
    · exact h
    · exact h
  have hd : ∀ s', ((repairIfDue s').rep = s'.rep ∨ (repairIfDue s').rep = .head) ∧
      (repairIfDue s').orphans = s'.orphans ∧ (repairIfDue s').log = s'.log := by
    intro s'
    unfold repairIfDue requestRepair
    split
    · split
      · exact ⟨Or.inl rfl, rfl, rfl⟩
      · exact ⟨Or.inr rfl, rfl, rfl⟩
    · exact ⟨Or.inl rfl, rfl, rfl⟩
  have hsync : ((syncForRead P s).rep = s.rep ∨ (syncForRead P s).rep = .head) ∧
      (syncForRead P s).orphans = s.orphans ∧ (syncForRead P s).log = s.log := by
    unfold syncForRead
    simp only
    split
    · exact ⟨Or.inl rfl, rfl, rfl⟩
    · split
      · exact hr s
      · exact ⟨Or.inl rfl, rfl, rfl⟩
    · split
      · split
        · exact ⟨Or.inl rfl, rfl, rfl⟩
        · exact hd s
      · split
        · exact hr s
        · exact ⟨Or.inl rfl, rfl, rfl⟩
  unfold read
  simp only
  split
  · exact hsync
  · split
    · exact hsync
    · split
      · exact hsync
      · split <;> (try split) <;> exact hsync

/-- A head or idle repair task is not writing. -/
theorem writing_head (t : Nat) : RPc.head.writing t = false := rfl
theorem writing_idle (t : Nat) : RPc.idle.writing t = false := rfl

theorem step_row (s : St) (e : Ev) (h : s.d.row ≠ .none) : (step P s e).d.row ≠ .none := by
  by_cases hi : e.indexSide = true
  · rw [indexSide_keeps_durable P s e hi]; exact h
  · cases e <;> simp only [Ev.indexSide, Bool.false_eq_true, not_true_eq_false] at hi <;>
      simp only [step]
    all_goals (repeat' split) <;>
      simp_all [mirror_d, (applyBatch_lease _ _).2.1, eraseDurable]

theorem landItem_aux (s : St) (src : Src) (it : Item) :
    (landItem s src it).rep = s.rep ∧ (landItem s src it).orphans = s.orphans ∧
      (landItem s src it).log = src :: s.log := by
  cases it <;> exact ⟨rfl, rfl, rfl⟩

theorem mirStep_aux (s : St) :
    (mirStep s).rep = s.rep ∧ (mirStep s).orphans = s.orphans ∧
      ((mirStep s).log = s.log ∨ ∃ t, (mirStep s).log = .mirror t :: s.log) := by
  unfold mirStep
  split
  · exact ⟨rfl, rfl, Or.inl rfl⟩
  · exact ⟨rfl, rfl, Or.inl rfl⟩
  · split
    · exact ⟨rfl, rfl, Or.inl rfl⟩
    · exact ⟨rfl, rfl, Or.inr ⟨_, rfl⟩⟩
  · split
    · exact ⟨rfl, rfl, Or.inl rfl⟩
    · exact ⟨rfl, rfl, Or.inr ⟨_, rfl⟩⟩

/-- What one event does to the repair task, the orphaned requests and the
write log, for every event but the repair task's own step. -/
structure Frame (s s' : St) : Prop where
  rep : s'.rep = s.rep ∨ s'.rep = .head ∨ s'.rep = .idle
  orphans : s'.orphans = s.orphans ∨ s'.orphans = s.orphans ++ repInflight s.rep ∨
    s'.orphans = s.orphans ++ mirInflight s.mir ∨ s'.orphans = [] ∨
    ∃ src it, s.orphans = (src, it) :: s'.orphans ∧ s'.log = src :: s.log
  log : s'.log = s.log ∨ s'.log = .unattributed :: s.log ∨ s'.log = .other :: s.log ∨
    s'.log = .erase :: s.log ∨ (∃ t, s'.log = .mirror t :: s.log) ∨
    ∃ src it, s.orphans = (src, it) :: s'.orphans ∧ s'.log = src :: s.log

theorem frame_same {s s' : St} (h1 : s'.rep = s.rep) (h2 : s'.orphans = s.orphans)
    (h3 : s'.log = s.log) : Frame s s' := ⟨Or.inl h1, Or.inl h2, Or.inl h3⟩

theorem frame_refl (s : St) : Frame s s := frame_same rfl rfl rfl

theorem frame_mirror (s : St) (d : Durable) (ms : List Mut) (t : Option Nat) (d' : Durable) :
    Frame s (mirror { s with d := d } ms t d') := by
  refine ⟨?_, Or.inl (mirror_orphans _ _ _ _), ?_⟩
  · rcases mirror_tier_rep { s with d := d } ms t d' with h | h
    · exact Or.inl h
    · exact Or.inr (Or.inl h)
  · rcases mirror_log { s with d := d } ms t d' with h | h
    · exact Or.inl h
    · exact Or.inr (Or.inl h)

theorem step_frame (s : St) (e : Ev) (he : e ≠ .repStep) : Frame s (step P s e) := by
  cases e
  case write ms =>
    simp only [step]; split
    · exact frame_mirror s _ ms _ _
    · exact frame_same rfl rfl rfl
  case writeUnleased ms =>
    simp only [step]; split
    · exact frame_mirror s _ ms _ _
    · exact frame_same rfl rfl rfl
  case otherWrite ms mirrored =>
    simp only [step]; split
    · split
      · split
        · exact ⟨Or.inl rfl, Or.inl rfl, Or.inr (Or.inr (Or.inl rfl))⟩
        · exact frame_same rfl rfl rfl
      · exact frame_same rfl rfl rfl
    · exact frame_same rfl rfl rfl
  case mirStep =>
    obtain ⟨h1, h2, h3⟩ := mirStep_aux s
    refine ⟨Or.inl h1, Or.inl h2, ?_⟩
    rcases h3 with h | h
    · exact Or.inl h
    · exact Or.inr (Or.inr (Or.inr (Or.inr (Or.inl h))))
  case mirTimeout =>
    simp only [step]; split
    · exact frame_same rfl rfl rfl
    · exact ⟨Or.inl rfl, Or.inr (Or.inr (Or.inl rfl)), Or.inl rfl⟩
  case repStep => exact absurd rfl he
  case repTimeout =>
    simp only [step]; split
    · exact frame_same rfl rfl rfl
    · exact frame_same rfl rfl rfl
    · refine ⟨Or.inr (Or.inr rfl), Or.inr (Or.inl ?_), Or.inl rfl⟩
      simp_all [passFail, markStale]
  case orphanLand =>
    simp only [step]; split
    · rename_i src it rest hr
      obtain ⟨h1, h2, h3⟩ := landItem_aux { s with orphans := rest } src it
      exact ⟨Or.inl h1, Or.inr (Or.inr (Or.inr (Or.inr ⟨src, it, by rw [hr, h2], h3⟩))),
        Or.inr (Or.inr (Or.inr (Or.inr (Or.inr ⟨src, it, by rw [hr, h2], h3⟩))))⟩
    · exact frame_same rfl rfl rfl
  case orphanDrop => exact ⟨Or.inl rfl, Or.inr (Or.inr (Or.inr (Or.inl rfl))), Or.inl rfl⟩
  case loadBegin => exact frame_same rfl rfl rfl
  case loadEnd =>
    simp only [step]; split
    · obtain ⟨h1, h2, h3⟩ := settle_aux P { s with loading := none } _
      refine ⟨?_, Or.inl h2, Or.inl h3⟩
      rcases h1 with h | h
      · exact Or.inl h
      · exact Or.inr (Or.inl h)
    · exact frame_same rfl rfl rfl
  case read exp =>
    obtain ⟨h1, h2, h3⟩ := read_aux P exp s
    refine ⟨?_, Or.inl h2, Or.inl h3⟩
    rcases h1 with h | h
    · exact Or.inl h
    · exact Or.inr (Or.inl h)
  case acquire =>
    simp only [step]; split
    · exact frame_same rfl rfl rfl
    · split
      · exact frame_same rfl rfl rfl
      · exact frame_same rfl rfl rfl
  case refresh =>
    simp only [step]; split <;> exact frame_same rfl rfl rfl
  case release wait =>
    simp only [step]; split
    · exact frame_same rfl rfl rfl
    · split
      · exact frame_same rfl rfl rfl
      · exact ⟨Or.inr (Or.inr rfl), Or.inr (Or.inl rfl), Or.inl rfl⟩
  case takeover => simp only [step]; split <;> exact frame_same rfl rfl rfl
  case override => simp only [step]; split <;> exact frame_same rfl rfl rfl
  case eraseOther =>
    simp only [step]; split
    · exact frame_same rfl rfl rfl
    · exact ⟨Or.inl rfl, Or.inl rfl, Or.inr (Or.inr (Or.inr (Or.inl rfl)))⟩
  case eraseStop => simp only [step]; split <;> exact frame_same rfl rfl rfl
  case eraseFinish =>
    simp only [step]; split
    · split
      · exact frame_same rfl rfl rfl
      · exact ⟨Or.inl rfl, Or.inl rfl, Or.inr (Or.inr (Or.inr (Or.inl rfl)))⟩
    · exact frame_same rfl rfl rfl
  case eraseAbort =>
    simp only [step]; split
    · exact ⟨Or.inr (Or.inr rfl), Or.inr (Or.inl rfl), Or.inl rfl⟩
    · exact frame_same rfl rfl rfl
  case setDown => exact frame_same rfl rfl rfl
  case setKnnDown => exact frame_same rfl rfl rfl
  case tick => exact frame_same rfl rfl rfl

/-- **Once the lease moved past `t`, no repair write under `t` ever lands
again** (#18 review F2, `ensure_fenced`). From a state where the repair task is not
mid-request under `t` and no aborted request under `t` is on the wire, every
event keeps it so. -/
theorem step_quiet (s : St) (t : Nat) (e : Ev) (hq : Quiet t s) : Quiet t (step P s e) := by
  have hpast : t < (step P s e).d.lease := Nat.lt_of_lt_of_le hq.past (lease_mono P s e)
  have hw := hq.notWriting
  have ho := hq.noOrphan
  refine ⟨hpast, step_row P s e hq.row, ?_, ?_⟩
  · by_cases he : e = .repStep
    · subst he
      show (repStep P s).rep.writing t = false
      cases h : (repStep P s).rep.writing t
      · rfl
      · rcases repStep_writing_imp P s t h with h' | h'
        · rw [hw] at h'; cases h'
        · rw [isFenced_past hq] at h'; cases h'
    · rcases (step_frame P s e he).rep with h | h | h <;> rw [h]
      · exact hw
      · rfl
      · rfl
  · by_cases he : e = .repStep
    · subst he
      show ∀ x ∈ (repStep P s).orphans, x.1 ≠ .repair t
      rw [repStep_orphans]; exact ho
    · intro x hx hxe
      obtain ⟨src, it⟩ := x
      simp only at hxe
      subst hxe
      rcases (step_frame P s e he).orphans with h | h | h | h | ⟨y, z, h, _⟩
      · rw [h] at hx; exact ho _ hx rfl
      · rw [h] at hx
        rcases List.mem_append.mp hx with hx | hx
        · exact ho _ hx rfl
        · have := repInflight_src s.rep t ⟨it, hx⟩
          rw [hw] at this; cases this
      · rw [h] at hx
        rcases List.mem_append.mp hx with hx | hx
        · exact ho _ hx rfl
        · exact mirInflight_src s.mir t it hx
      · rw [h] at hx; simp at hx
      · exact ho _ (h ▸ List.mem_cons_of_mem _ hx) rfl

theorem step_log (s : St) (t : Nat) (e : Ev) (hq : Quiet t s) :
    ∃ new, (step P s e).log = new ++ s.log ∧ Src.repair t ∉ new := by
  by_cases he : e = .repStep
  · subst he; exact repStep_log P s t hq.notWriting
  rcases (step_frame P s e he).log with h | h | h | h | ⟨t', h⟩ | ⟨src, it, ho, h⟩
  · exact ⟨[], h, List.not_mem_nil⟩
  · exact ⟨[_], h, by simp⟩
  · exact ⟨[_], h, by simp⟩
  · exact ⟨[_], h, by simp⟩
  · exact ⟨[_], h, by simp⟩
  · refine ⟨[src], h, ?_⟩
    simp only [List.mem_singleton]
    intro hs
    exact hq.noOrphan (src, it) (by rw [ho]; exact List.mem_cons_self) hs.symm

/-- **Once the lease moved past `t`, no repair write under `t` ever lands
again** (#18 review F2, `ensure_fenced`), over every run.
`a_repair_that_lost_its_lease_stops_writing`
is the one write already on the wire; from then on the stalled holder writes
nothing however the events interleave: takeovers, re-takes, erases, its own
timeouts and reruns. -/
theorem repair_silent_after_loss (s : St) (t : Nat) (hq : Quiet t s) (evs : List Ev) :
    ∃ new, (run P s evs).log = new ++ s.log ∧ Src.repair t ∉ new := by
  induction evs generalizing s with
  | nil => exact ⟨[], rfl, List.not_mem_nil⟩
  | cons e es ih =>
    obtain ⟨n₁, h₁, hn₁⟩ := step_log P s t e hq
    obtain ⟨n₂, h₂, hn₂⟩ := ih (step P s e) (step_quiet P s t e hq)
    refine ⟨n₂ ++ n₁, ?_, ?_⟩
    · show (run P (step P s e) es).log = _
      rw [h₂, h₁, List.append_assoc]
    · intro hm
      rcases List.mem_append.mp hm with hm | hm
      · exact hn₂ hm
      · exact hn₁ hm

/-- **Repair and erase never race into rebuilding an erased session.** An
erase (through this store or another) or a takeover moves the lease past
every token handed out before it; from then on, once the repair task is off
its in-flight request (and no aborted request is on the wire), no repair
write under any earlier token lands, in any run. The one in-flight request
is the documented window (`ensure_fenced`); the mirror has no such fence
(`mirror_lands_after_erase`). -/
theorem erase_silences_repair (s : St) (t : Nat) (e : Ev)
    (he : e = .eraseStop ∨ e = .eraseOther ∨ e = .takeover) (hne : s.d.erased = false)
    (ht : t ≤ s.d.lease) (hw : s.rep.writing t = false)
    (ho : ∀ x ∈ s.orphans, x.1 ≠ .repair t) (evs : List Ev) :
    ∃ new, (run P (step P s e) evs).log = new ++ (step P s e).log ∧ Src.repair t ∉ new := by
  apply repair_silent_after_loss
  rcases he with rfl | rfl | rfl <;>
    exact ⟨by simp [step, hne, eraseDurable]; omega, by simp [step, hne, eraseDurable],
      by simpa [step, hne] using hw, by simpa [step, hne] using ho⟩

/-- The quiet state is reached: after the fence refuses (`passLost`) the
repair task is idle; with the lease past the token and nothing orphaned, the
stalled holder is `Quiet`. -/
theorem quiet_of_idle (s : St) (t : Nat) (hp : t < s.d.lease) (hr : s.d.row ≠ .none)
    (hi : s.rep = .idle ∨ s.rep = .head) (ho : ∀ x ∈ s.orphans, x.1 ≠ .repair t) : Quiet t s :=
  ⟨hp, hr, by rcases hi with h | h <;> rw [h] <;> rfl, ho⟩

theorem repStep_mir (s : St) : (repStep P s).mir = s.mir := by
  cases hr : s.rep <;> simp only [repStep, hr]
  all_goals (repeat' split) <;> first | rfl | simp

theorem landItem_mir (s : St) (src : Src) (it : Item) : (landItem s src it).mir = s.mir := by
  cases it <;> rfl

theorem settle_mir (s : St) (snap : Snap) : (settle P s snap).mir = s.mir := by
  unfold settle requestRepair
  by_cases h1 : checkMarker s.down s.mark snap = .ahead <;>
    simp only [h1, ite_true, ite_false] <;> split <;> (try split) <;> simp

theorem read_mir (exp : Nat) (s : St) : (read P exp s).mir = s.mir := by
  have hr : ∀ s', (recheck P s').mir = s'.mir := by
    intro s'; unfold recheck; simp only; split <;> simp [settle_mir]
  have hd : ∀ s', (repairIfDue s').mir = s'.mir := by
    intro s'; unfold repairIfDue requestRepair; split
    · split <;> rfl
    · rfl
  have hs : (syncForRead P s).mir = s.mir := by
    unfold syncForRead; simp only
    split
    · rfl
    · split
      · exact hr s
      · rfl
    · split
      · split
        · rfl
        · exact hd s
      · split
        · exact hr s
        · rfl
  unfold read; simp only
  split
  · exact hs
  · split
    · exact hs
    · split
      · exact hs
      · split <;> (try split) <;> exact hs

theorem mirror_mir (s : St) (ms : List Mut) (tok : Option Nat) (d' : Durable) (t v ep : Nat)
    (ops : List Op) (h : (mirror s ms tok d').mir = .puts t v ep ops) :
    s.mir = .puts t v ep ops ∨ (tok = some t ∧ s.tier.held = some t) := by
  have hs : ∀ s', (mirrorSession s' ms tok d').mir = .puts t v ep ops →
      s'.mir = .puts t v ep ops ∨ (tok = some t ∧ s'.tier.held = some t) := by
    intro s' h
    unfold mirrorSession at h
    simp only at h
    split at h
    · exact Or.inl h
    · rename_i tk
      split at h
      · left; unfold repairIfDue requestRepair at h; split at h
        · split at h <;> exact h
        · exact h
      · split at h
        · exact Or.inl h
        · split at h
          · exact Or.inl h
          · rename_i hne _ _ _ _ _
            simp only [MPc.puts.injEq] at h
            obtain ⟨rfl, -, -, -⟩ := h
            exact Or.inr ⟨rfl, Classical.not_not.mp hne⟩
  unfold mirror at h
  split at h
  · exact hs s h
  · simp only at h
    split at h
    · exact Or.inl h
    · split at h
      · exact hs s h
      · split at h <;> exact Or.inl h

/-- **A fenced handle never mirrors.** A mirror starts only from this writer's
flush, with the primary having accepted the writer's token (it is the lease's
current one) and this store holding exactly that token
(`a_flush_is_not_mirrored_after_the_lease_was_lost`). -/
theorem mirror_starts_held (s : St) (e : Ev) (t v ep : Nat) (ops : List Op)
    (hi : s.mir = .idle) (h : (step P s e).mir = .puts t v ep ops) :
    ∃ ms, e = .write ms ∧ s.wtok = some t ∧ s.d.lease = t ∧ s.tier.held = some t := by
  cases e
  case write ms =>
    simp only [step] at h
    split at h
    · rename_i hc
      rcases mirror_mir _ ms _ _ t v ep ops h with h' | ⟨h1, h2⟩
      · simp [hi] at h'
      · obtain ⟨-, -, h3⟩ := hc
        rw [h3] at h1
        cases h1
        exact ⟨ms, rfl, h3, rfl, h2⟩
    · rw [hi] at h; cases h
  case writeUnleased ms =>
    simp only [step] at h
    split at h
    · rcases mirror_mir _ ms _ _ t v ep ops h with h' | ⟨h1, -⟩
      · simp [hi] at h'
      · cases h1
    · rw [hi] at h; cases h
  case repStep => rw [show step P s .repStep = repStep P s from rfl, repStep_mir, hi] at h; cases h
  case loadEnd =>
    simp only [step] at h; split at h
    · rw [settle_mir, hi] at h; cases h
    · rw [hi] at h; cases h
  case read exp => rw [show step P s (.read exp) = read P exp s from rfl, read_mir, hi] at h; cases h
  case mirStep => simp [step, mirStep, hi] at h
  case orphanLand =>
    simp only [step] at h; split at h
    · rw [landItem_mir] at h; simp only at h; rw [hi] at h; cases h
    · rw [hi] at h; cases h
  all_goals (simp only [step] at h; (repeat' split at h) <;> simp_all [passFail, markStale])

end Fence

/-! ## Reads, the breaker and the marker state -/

section Reads

variable (P : Params)

/-- An in-sync session is never re-checked by a read (`sync_for_read`). -/
theorem syncForRead_inSync (s : St) (h : s.tier.sync = .inSync) : syncForRead P s = s := by
  unfold syncForRead; simp [h]

/-- **Reads fall back unless the session is in sync** (#18: "Only `InSync`
serves vector candidates from the index"). -/
theorem read_not_inSync_falls_back (exp : Nat) (s : St)
    (h : (syncForRead P s).tier.sync ≠ .inSync) : (read P exp s).answer = some .fallback := by
  unfold read; simp [h]

/-- **While the breaker is open, an in-sync read goes to the durable store**
and the index is not asked (the breaker is left as it was). -/
theorem read_breaker_open (exp : Nat) (s : St) (u : Nat) (h : s.tier.sync = .inSync)
    (hc : s.tier.contract = some (some exp)) (hu : s.br.openUntil = some u) (hlt : s.now < u) :
    (read P exp s).answer = some .fallback ∧ (read P exp s).br = s.br := by
  unfold read
  simp [syncForRead_inSync P s h, h, hc, admits, hu, hlt]

/-- An in-sync read leaves the tier state alone: whatever it returns, the next
read starts from the same trust. -/
theorem read_inSync_tier (exp : Nat) (s : St) (h : s.tier.sync = .inSync) :
    (read P exp s).tier = s.tier := by
  unfold read
  rw [syncForRead_inSync P s h]
  simp only [h, ne_eq, not_true_eq_false, ite_false]
  split
  · rfl
  · split
    · rfl
    · split <;> (try split) <;> rfl

/-- What another process does, and the passage of time: none of it reaches
this store's tier state. -/
def Ev.foreign : Ev → Bool
  | .otherWrite _ _ | .takeover | .override | .eraseOther | .tick _ | .setKnnDown _ | .setDown _ => true
  | _ => false

/-- **A reader in sync stays in sync whatever happens elsewhere**
(`reader_never_rechecks`, in general): through any run of its own reads and
of other processes' flushes (mirrored or not), takeovers, erases and outages,
its tier state never changes, so it never re-checks the marker and serves the
index for as long as the process lives (or until it reloads the session, or
evicts it past `MAX_TRACKED_SESSIONS`). -/
theorem reader_stays_inSync (s : St) (h : s.tier.sync = .inSync) (evs : List Ev)
    (he : ∀ e ∈ evs, (∃ exp, e = .read exp) ∨ e.foreign = true) :
    (run P s evs).tier = s.tier := by
  induction evs generalizing s with
  | nil => rfl
  | cons e es ih =>
    have hstep : (step P s e).tier = s.tier := by
      rcases he e List.mem_cons_self with ⟨exp, rfl⟩ | hf
      · exact read_inSync_tier P exp s h
      · cases e <;> simp only [Ev.foreign, Bool.false_eq_true] at hf <;> simp only [step] <;>
          (repeat' split) <;> rfl
    show (run P (step P s e) es).tier = s.tier
    rw [ih (step P s e) (hstep ▸ h) (fun e' he' => he e' (List.mem_cons_of_mem _ he')), hstep]

/-- **#18 review F4: a repair overtaken by a flush is not trusted.** At the end of a
pass whose snapshot is older than a flush this store committed, the session
is not marked in sync and the run goes round again. -/
theorem finish_overtaken (s : St) (snap : Snap) (hr : s.rep = .finish snap)
    (hc : s.tier.committed > snapEpoch snap) :
    (repStep P s).tier.sync = s.tier.sync ∧ (repStep P s).rep = .head := by
  simp [repStep, hr, hc, passOk]

/-- A pass that is not overtaken marks the session in sync (and the run ends
unless a rerun was asked for). -/
theorem finish_inSync (s : St) (snap : Snap) (hr : s.rep = .finish snap)
    (hc : s.tier.committed ≤ snapEpoch snap) : (repStep P s).tier.sync = .inSync := by
  have : ¬ s.tier.committed > snapEpoch snap := Nat.not_lt.mpr hc
  simp only [repStep, hr, this, ite_false]
  unfold passOk; split <;> rfl

/-- The marker a load finally compares (after `recheck_ahead`'s fresh load). -/
def settleMarker (s : St) (snap : Snap) : Marker :=
  if checkMarker s.down s.mark snap = .ahead then checkMarker s.down s.mark (snapOf s.d)
  else checkMarker s.down s.mark snap

/-- **A load ends in a consistent marker state** (`settle_after_load`): unless
a repair owns the state, the session is trusted exactly when the final marker
check says in sync, and a holder that finds the marker behind asks for a
repair instead of trusting or distrusting it. -/
theorem settle_consistent (s : St) (snap : Snap) (hrep : s.tier.repairing = false) :
    (settleMarker s snap = .behind ∧ s.tier.held ≠ none →
        (settle P s snap).rep = .head ∧ (settle P s snap).tier.sync = s.tier.sync) ∧
      (¬ (settleMarker s snap = .behind ∧ s.tier.held ≠ none) →
        (settle P s snap).tier.sync = (settleMarker s snap).sync) := by
  unfold settle settleMarker
  cases hh : s.tier.held <;>
  by_cases h1 : checkMarker s.down s.mark snap = .ahead <;>
    simp only [h1, ite_true, ite_false] <;> constructor <;> intro hb <;>
    split <;> (try split) <;> simp_all [requestRepair]

/-- Only an in-sync marker is trusted, and a trusted marker equals the epoch
loaded (or nothing was ever indexable). -/
theorem settle_trusted (s : St) (snap : Snap) (hrep : s.tier.repairing = false)
    (hnot : ¬ (settleMarker s snap = .behind ∧ s.tier.held ≠ none))
    (h : (settle P s snap).tier.sync = .inSync) : settleMarker s snap = .inSync := by
  rw [(settle_consistent P s snap hrep).2 hnot] at h
  cases hm : settleMarker s snap <;> simp_all [Marker.sync]

end Reads

/-! ## The mirror always ends -/

section Liveness

variable (P : Params)

def MPc.measure : MPc → Nat
  | .idle => 0
  | .puts _ _ _ ops => ops.length + 2
  | .marker _ _ _ => 1

theorem mirStep_measure (s : St) (h : s.mir ≠ .idle) :
    (mirStep s).mir.measure < s.mir.measure := by
  cases hm : s.mir with
  | idle => exact absurd hm h
  | puts t v e ops =>
    cases ops with
    | nil => simp [mirStep, hm, MPc.measure]
    | cons o ops =>
      obtain ⟨c, n, x⟩ := o
      simp only [mirStep, hm]
      split <;> simp [MPc.measure]
  | marker t v e =>
    simp only [mirStep, hm]
    split <;> simp [MPc.measure]

/-- **A flush is held up by at most one mirror, and that mirror ends**: from
any state, `|ops| + 2` mirror steps finish it, landed or failed (and
`MIRROR_DEADLINE` ends it in one event). -/
theorem mirror_ends (s : St) : (run P s (steps .mirStep s.mir.measure)).mir = .idle := by
  suffices ∀ n (s : St), s.mir.measure ≤ n → (run P s (steps .mirStep n)).mir = .idle from
    this _ s (Nat.le_refl _)
  intro n
  induction n with
  | zero =>
    intro s h
    have : s.mir.measure = 0 := Nat.le_zero.mp h
    show s.mir = .idle
    cases hm : s.mir <;> simp_all [MPc.measure]
  | succ n ih =>
    intro s h
    show (run P (mirStep s) (steps .mirStep n)).mir = .idle
    by_cases hi : s.mir = .idle
    · have : mirStep s = s := by unfold mirStep; rw [hi]
      rw [this]
      exact ih s (by rw [hi]; exact Nat.zero_le _)
    · exact ih _ (Nat.le_of_lt_succ (Nat.lt_of_lt_of_le (mirStep_measure s hi) h))

theorem mirTimeout_ends (s : St) : (step P s .mirTimeout).mir = .idle := by
  simp only [step]; split
  · assumption
  · rfl

end Liveness

/-! ## A repair converges: stale, never meaningless -/

section Converge

variable (P : Params)

/-- The index matches the durable session: in the durable contract's index,
every node's document is exactly its stored vector (none for a node without
one or deleted). Other contracts' indices are not read under this contract. -/
def Matches (s : St) : Prop :=
  ∀ c, s.d.contract = some c → ∀ n, (s.rt c n).doc = lookup n s.d.vecs

/-- The bulk of a reconcile: every stored vector at version `v`. -/
def putAll (rt : Idx) (c v : Nat) (ops : List (Nat × Vec)) : Idx :=
  ops.foldl (fun rt o => put rt c o.1 v (some o.2)) rt

theorem lookup_cons (n m : Nat) (x : Vec) (l : List (Nat × Vec)) :
    lookup n ((m, x) :: l) = if m = n then some x else lookup n l := by
  unfold lookup
  by_cases h : m = n <;> simp [h]

/-- External versions make the first write of an id at `v` win and every later
one at `v` a conflict. -/
theorem putAll_eq (c v : Nat) (ops : List (Nat × Vec)) :
    ∀ (rt : Idx) c' n, putAll rt c v ops c' n =
      if c' = c ∧ (rt c n).ver < v ∧ (lookup n ops).isSome then ⟨v, lookup n ops⟩ else rt c' n := by
  induction ops with
  | nil => intro rt c' n; simp [putAll, lookup]
  | cons o ops ih =>
    intro rt c' n
    obtain ⟨m, x⟩ := o
    show putAll (put rt c m v (some x)) c v ops c' n = _
    rw [ih]
    rw [lookup_cons]
    unfold put
    by_cases hlt : (rt c m).ver < v
    · simp only [hlt, ite_true]
      by_cases hm : m = n
      · subst hm
        by_cases hc : c' = c
        · subst hc; simp [Nat.not_le.mpr hlt]
        · simp [hc]
      · simp [hm, Ne.symm hm]
    · simp only [hlt, ite_false]
      by_cases hm : m = n
      · subst hm; simp [hlt]
      · simp [hm]

/-- After the bulk at `v` over a dominated index, the sweep below `v` leaves
exactly the snapshot's vectors in its contract's index. -/
theorem reconcile_matches (rt : Idx) (c v : Nat) (ops : List (Nat × Vec))
    (hdom : ∀ c n, (rt c n).ver < v) (n : Nat) :
    (sweepBelow (putAll rt c v ops) (some v) c n).doc = lookup n ops := by
  unfold sweepBelow
  rw [putAll_eq]
  simp only [true_and, hdom c n]
  cases h : lookup n ops with
  | some x => simp
  | none =>
    simp only [Option.isSome_none, Bool.false_eq_true, ite_false]
    by_cases hd : (rt c n).doc.isSome
    · simp [hd, hdom c n]
    · simp only [hd, Bool.false_and, Bool.false_eq_true, ite_false]
      cases hdc : (rt c n).doc <;> simp_all

theorem run_append (s : St) (a b : List Ev) : run P s (a ++ b) = run P (run P s a) b := by
  induction a generalizing s with
  | nil => rfl
  | cons e es ih => exact ih _

theorem steps_succ (e : Ev) (n : Nat) : steps e (n + 1) = e :: steps e n := rfl

/-- The bulk loop: `2·|ops|` steps put every vector at `v`, fence by fence. -/
theorem bulk_loop (t v c : Nat) (snap : Snap) (hc : snapContract snap = some c) :
    ∀ (ops : List (Nat × Vec)) (s : St), s.rep = .bulkFence t v ops snap → isFenced s t = true →
      s.down = false →
      ∃ lg, run P s (steps .repStep (2 * ops.length)) =
        { s with rt := putAll s.rt c v ops, rep := .bulkFence t v [] snap, log := lg ++ s.log } := by
  intro ops
  induction ops with
  | nil => intro s hr _ _; exact ⟨[], by simp [steps, run, putAll, ← hr]⟩
  | cons o ops ih =>
    intro s hr hf hd
    obtain ⟨n, x⟩ := o
    have h1 : step P s .repStep = { s with rep := .bulkPut t v ((n, x) :: ops) snap } := by
      simp [step, repStep, hr, hf]
    let s2 : St := { s with
      rt := put s.rt c n v (some x)
      log := .repair t :: s.log
      rep := .bulkFence t v ops snap }
    have h2 : step P { s with rep := .bulkPut t v ((n, x) :: ops) snap } .repStep = s2 := by
      simp [step, repStep, hc, hd, s2]
    obtain ⟨lg, hlg⟩ := ih s2 rfl (by simpa [isFenced, s2] using hf) hd
    refine ⟨lg ++ [.repair t], ?_⟩
    rw [show 2 * ((n, x) :: ops).length = 2 * ops.length + 1 + 1 by simp; omega]
    rw [steps_succ, steps_succ]
    show run P (step P (step P s .repStep) .repStep) _ = _
    rw [h1, h2, hlg]
    simp [s2, putAll, List.append_assoc]

/-- **A repair with no further writes makes the index match the durable store**
("stale, never meaningless"; `a_mirror_failure_keeps_the_flush_and_the_next_flush_repairs`,
`a_marker_behind_the_durable_epoch_is_caught_at_the_next_load`). From a pass
that starts while this store holds the current lease, with the index up, the
marker behind the durable epoch, no flush committed through this store past
it, and every document of the index below the pass's version (`hdom`,
`hmk`), `2·|vectors| + 9` steps of the task re-index every vector, sweep everything else, write the
marker at the durable epoch and mark the session in sync; the durable store
is untouched.

Version dominance is an assumption, not an invariant of the code.
`nextVersion_gt` and `version_holder_first` give it while this store's
recorded token never runs ahead of the pass's, but `next_version` also
resets to an older token, and a stale pass makes the next write repeat a
version already in the index (`stale_pass_rewinds_version`). -/
theorem repair_converges (s : St) (t tok cnt v : Nat) (hr : s.rep = .load t)
    (hh : s.tier.held = some t) (hl : s.d.lease = t) (hrow : s.d.row ≠ .none)
    (he : s.d.erased = false) (hd : s.down = false)
    (hm : checkMarker false s.mark (some s.d) = .behind)
    (hv : nextVersion s.tier.tok s.tier.cnt t = some (tok, cnt, v))
    (hdom : ∀ c n, (s.rt c n).ver < v) (hmk : ∀ w e, s.mark = some (w, e) → w < v)
    (hcomm : s.tier.committed ≤ s.d.epoch) (hag : s.tier.again = false) :
    let s' := run P s (steps .repStep (2 * s.d.vecs.length + 9))
    Matches s' ∧ s'.d = s.d ∧ s'.tier.sync = .inSync ∧ s'.mark = some (v, s.d.epoch) ∧
      s'.rep = .idle := by
  intro s'
  have hsnap : snapOf s.d = some s.d := by simp [snapOf, he]
  have hfen : ∀ (x : St), x.tier.held = some t → x.d = s.d → isFenced x t = true := by
    intro x hx hxd
    simp [isFenced, fenced, hx, durableLease, hxd, hrow, hl]
  -- load, check, version
  let s1 : St := { s with
    tier := { s.tier with contract := some s.d.contract }
    rep := .check t (some s.d) }
  have e1 : step P s .repStep = s1 := by simp [step, repStep, hr, hsnap, snapContract, s1]
  have hm' : checkMarker s1.down s1.mark (some s.d) = .behind := by simpa [s1, hd] using hm
  have e2 : step P s1 .repStep = { s1 with rep := .version t (some s.d) } := by
    simp [step, repStep, s1, hm']
  have hidle : ∀ (x : St) n, x.rep = .idle → run P x (steps .repStep n) = x := by
    intro x n hx
    induction n with
    | zero => rfl
    | succ n ih =>
      rw [steps_succ]
      show run P (repStep P x) _ = x
      have : repStep P x = x := by simp [repStep, hx]
      rw [this, ih]
  let tr : Tier := { s1.tier with
    tok := tok
    cnt := cnt
    sync := (if s.tier.sync = .inSync then .unknown else s.tier.sync) }
  have hsplit : steps Ev.repStep (2 * s.d.vecs.length + 9) =
      steps .repStep 3 ++ (steps .repStep (2 * s.d.vecs.length) ++ steps .repStep 6) := by
    simp only [steps, List.replicate_append_replicate]; congr 1; omega
  have hpre : run P s (steps .repStep 3) =
      { s1 with tier := tr, rep := match s.d.contract with
        | some _ => .bulkFence t v s.d.vecs (some s.d)
        | none => .sweepFence t v (some s.d) } := by
    show run P (step P (step P (step P s .repStep) .repStep) .repStep) [] = _
    rw [e1, e2]
    simp only [run, step, repStep, hv, snapContract, Option.bind, s1, tr]
    cases s.d.contract <;> rfl
  cases hc : s.d.contract with
  | none =>
    let s3 : St := { s1 with tier := tr, rep := .sweepFence t v (some s.d) }
    have hf : ∀ x : St, x.tier.held = some t → x.d = s.d → isFenced x t = true := hfen
    have hheld : s3.tier.held = some t := by simp [s3, tr, s1, hh]
    have n1 : repStep P s3 = { s3 with rep := .sweep t v (some s.d) } := by
      simp only [repStep, s3, hf s3 hheld rfl, ite_true]
    have n2 : repStep P { s3 with rep := .sweep t v (some s.d) } =
        { s3 with
          rt := sweepBelow s3.rt (some v)
          log := .repair t :: s3.log
          rep := .markerFence t v (some s.d) } := by
      simp only [repStep, s3, s1, hd, Bool.false_eq_true, ite_false]
    let y2 : St := { s3 with
      rt := sweepBelow s3.rt (some v)
      log := .repair t :: s3.log
      rep := .markerFence t v (some s.d) }
    have n3 : repStep P y2 = { y2 with rep := .marker t v (some s.d) } := by
      simp only [repStep, y2, hf y2 hheld rfl, ite_true]
    have n4 : repStep P { y2 with rep := .marker t v (some s.d) } =
        { y2 with
          mark := putMarker y2.mark v s.d.epoch
          log := .repair t :: y2.log
          rep := .finish (some s.d) } := by
      simp only [repStep, y2, s3, s1, hd, Bool.false_eq_true, ite_false, snapEpoch]
    let y4 : St := { y2 with
      mark := putMarker y2.mark v s.d.epoch
      log := .repair t :: y2.log
      rep := .finish (some s.d) }
    have hc5 : ¬ y4.tier.committed > snapEpoch (some s.d) := by
      simp only [y4, y2, s3, tr, s1, snapEpoch]; omega
    have x6 : (repStep P y4).rep = .idle ∧ (repStep P y4).tier.sync = .inSync ∧
        (repStep P y4).mark = y4.mark ∧ (repStep P y4).d = y4.d := by
      simp only [repStep, y4, hc5, ite_false]
      simp [passOk, y2, s3, tr, s1, hag]
    have hmk' : putMarker s.mark v s.d.epoch = some (v, s.d.epoch) := by
      unfold putMarker
      cases hk : s.mark with
      | none => rfl
      | some p =>
        obtain ⟨w, e⟩ := p
        simp [hmk w e hk]
    have hrun : run P s3 (steps .repStep 5) = repStep P y4 := by
      show repStep P (repStep P (repStep P (repStep P (repStep P s3)))) = _
      rw [n1, n2]
      show repStep P (repStep P (repStep P y2)) = _
      rw [n3, n4]
    show run P s (steps .repStep (2 * s.d.vecs.length + 9)) |> fun s' =>
      Matches s' ∧ s'.d = s.d ∧ s'.tier.sync = .inSync ∧ s'.mark = some (v, s.d.epoch) ∧
        s'.rep = .idle
    rw [hsplit, run_append, hpre]
    simp only [hc]
    rw [show steps Ev.repStep (2 * s.d.vecs.length) ++ steps .repStep 6 =
        steps .repStep 5 ++ steps .repStep (2 * s.d.vecs.length + 1) by
      simp only [steps, List.replicate_append_replicate]; congr 1; omega]
    rw [run_append]
    change (run P (run P s3 (steps .repStep 5)) (steps .repStep (2 * s.d.vecs.length + 1)) |>
      fun s' => Matches s' ∧ s'.d = s.d ∧ s'.tier.sync = .inSync ∧
        s'.mark = some (v, s.d.epoch) ∧ s'.rep = .idle)
    rw [hrun]
    obtain ⟨h1, h2, h4, h5⟩ := x6
    rw [hidle _ _ h1]
    refine ⟨?_, ?_, h2, ?_, h1⟩
    · intro c' hc'
      rw [h5] at hc'
      simp only [y4, y2, s3, s1] at hc'
      rw [hc] at hc'; cases hc'
    · rw [h5]
    · rw [h4]; exact hmk'
  | some c =>
    have hsc : snapContract (some s.d) = some c := by simp [snapContract, hc]
    let s3 : St := { s1 with tier := tr, rep := .bulkFence t v s.d.vecs (some s.d) }
    obtain ⟨lg, hbulk⟩ := bulk_loop P t v c (some s.d) hsc s.d.vecs s3 rfl
      (hfen s3 (by simp [s3, tr, s1, hh]) rfl) hd
    have hmk' : putMarker s.mark v s.d.epoch = some (v, s.d.epoch) := by
      unfold putMarker
      cases hk : s.mark with
      | none => rfl
      | some p =>
        obtain ⟨w, e⟩ := p
        simp [hmk w e hk]
    let s4 : St := { s3 with
      rt := putAll s3.rt c v s.d.vecs
      rep := .bulkFence t v [] (some s.d)
      log := lg ++ s3.log }
    have hd4 : s4.d = s.d := rfl
    have hf : ∀ x : St, x.tier.held = some t → x.d = s.d → isFenced x t = true := hfen
    have x1 : repStep P s4 = { s4 with rep := .sweepFence t v (some s.d) } := rfl
    have x2 : repStep P { s4 with rep := .sweepFence t v (some s.d) } =
        { s4 with rep := .sweep t v (some s.d) } := by
      have := hf { s4 with rep := .sweepFence t v (some s.d) } (by simp [s4, s3, tr, s1, hh]) rfl
      simp only [repStep, this, ite_true]
    have x3 : repStep P { s4 with rep := .sweep t v (some s.d) } =
        { s4 with
          rt := sweepBelow s4.rt (some v)
          log := .repair t :: s4.log
          rep := .markerFence t v (some s.d) } := by
      simp only [repStep, s4, s3, s1, hd, Bool.false_eq_true, ite_false]
    let y3 : St := { s4 with
      rt := sweepBelow s4.rt (some v)
      log := .repair t :: s4.log
      rep := .markerFence t v (some s.d) }
    have x4 : repStep P y3 = { y3 with rep := .marker t v (some s.d) } := by
      have := hf y3 (by simp [y3, s4, s3, tr, s1, hh]) rfl
      simp only [repStep, y3, this, ite_true]
    have x5 : repStep P { y3 with rep := .marker t v (some s.d) } =
        { y3 with
          mark := putMarker y3.mark v s.d.epoch
          log := .repair t :: y3.log
          rep := .finish (some s.d) } := by
      simp only [repStep, y3, s4, s3, s1, hd, Bool.false_eq_true, ite_false, snapEpoch]
    let y5 : St := { y3 with
      mark := putMarker y3.mark v s.d.epoch
      log := .repair t :: y3.log
      rep := .finish (some s.d) }
    have hc5 : ¬ y5.tier.committed > snapEpoch (some s.d) := by
      simp only [y5, y3, s4, s3, tr, s1, snapEpoch]; omega
    have hag5 : y5.tier.again = false := by simp [y5, y3, s4, s3, tr, s1, hag]
    have x6 : (repStep P y5).rep = .idle ∧ (repStep P y5).tier.sync = .inSync ∧
        (repStep P y5).rt = y5.rt ∧ (repStep P y5).mark = y5.mark ∧ (repStep P y5).d = y5.d := by
      simp only [repStep, y5, hc5, ite_false]
      simp [passOk, y3, s4, s3, tr, s1, hag]
    have hrun : run P s4 (steps .repStep 6) = repStep P y5 := by
      show repStep P (repStep P (repStep P (repStep P (repStep P (repStep P s4))))) = _
      rw [x1, x2, x3]
      show repStep P (repStep P (repStep P y3)) = _
      rw [x4, x5]
    show run P s (steps .repStep (2 * s.d.vecs.length + 9)) |> fun s' =>
      Matches s' ∧ s'.d = s.d ∧ s'.tier.sync = .inSync ∧ s'.mark = some (v, s.d.epoch) ∧
        s'.rep = .idle
    rw [hsplit, run_append, run_append, hpre]
    simp only [hc]
    rw [hbulk]
    change Matches (run P s4 _) ∧ _
    rw [hrun]
    obtain ⟨h1, h2, h3, h4, h5⟩ := x6
    refine ⟨?_, ?_, h2, ?_, h1⟩
    · intro c' hc' n
      rw [h5] at hc'
      simp only [y5, y3, s4, s3, s1] at hc'
      rw [hc] at hc'; cases hc'
      rw [h3, h5]
      exact reconcile_matches s.rt c v s.d.vecs hdom n
    · rw [h5]
    · rw [h4]; exact hmk'

end Converge

/-! ## The exact re-rank (`rerank`, `rank_by_cosine`) -/

section Search

/-- One kNN hit: its id, its tie-break key, its stored vector if it came back
at the probe's width, and the engine's (approximate) score. Scores are exact
fixed-point cosines shifted to be non-negative (higher is nearer); `cos`
scores a stored vector against the probe. -/
structure Hit where
  id : Nat
  key : Nat
  vec : Option Nat
  engine : Nat
  deriving DecidableEq, Repr

/-- `score desc, then tie_break_by_key` (canonical key, then id): `a` ranks
before `b`, or equal. -/
def ranksBefore (a b : Nat × Nat × Nat) : Bool :=
  a.2.1 > b.2.1 || (a.2.1 == b.2.1 && (a.2.2 < b.2.2 || (a.2.2 == b.2.2 && a.1 ≤ b.1)))

/-- Insertion by `ranksBefore` (a sort the kernel can run; any correct sort
by this total order gives the same list, `Order.sorted_unique`). -/
def insertRank (a : Nat × Nat × Nat) : List (Nat × Nat × Nat) → List (Nat × Nat × Nat)
  | [] => [a]
  | b :: l => if ranksBefore a b then a :: b :: l else b :: insertRank a l

/-- `rank_by_cosine(probe, candidates, limit)`: the exact scorer every exact
source uses (SQLite's scan, the pg family's re-rank, the holder's graph), on
`(id, score, key)` triples. -/
def rankExact (l : List (Nat × Nat × Nat)) (limit : Nat) : List Nat :=
  ((l.foldr insertRank []).take limit).map (·.1)

/-- `rerank(probe, hits, limit)`. -/
def rerank (cos : Nat → Nat) (hits : List Hit) (limit : Nat) : List Nat :=
  if hits.all (·.vec.isSome) then
    rankExact (hits.map fun h => (h.id, cos (h.vec.getD 0), h.key)) limit
  else
    rankExact (hits.map fun h => (h.id, (h.vec.map cos).getD h.engine, h.key)) limit

/-- The exact read over a set of stored vectors, as the durable store ranks it. -/
def exactScan (cos : Nat → Nat) (vs : List (Nat × Nat × Nat)) (limit : Nat) : List Nat :=
  rankExact (vs.map fun v => (v.1, cos v.2.1, v.2.2)) limit

/-- The re-rank is the durable store's ranking on the candidate set (#18 review M3). Close
to the definition of `rerank`; the content is `rerank_ignores_engine`:
when every hit carries its stored vector, the engine's scores and its order
are ignored and the answer is `rank_by_cosine` over exactly those vectors,
which is what the exact scan returns when run on the same candidates. -/
theorem rerank_exact (cos : Nat → Nat) (hits : List Hit) (limit : Nat)
    (h : ∀ x ∈ hits, x.vec.isSome) :
    rerank cos hits limit =
      exactScan cos (hits.map fun x => (x.id, x.vec.getD 0, x.key)) limit := by
  have ha : hits.all (·.vec.isSome) = true := List.all_eq_true.mpr h
  simp [rerank, ha, exactScan, Function.comp_def]

/-- **The engine's scores never matter** (#18 review M3): when every hit carries
its stored vector, two answers that differ only in the engine's (approximate,
quantized) scores re-rank the same. -/
theorem rerank_ignores_engine (cos : Nat → Nat) (hits : List Hit) (f : Hit → Nat) (limit : Nat)
    (h : ∀ x ∈ hits, x.vec.isSome) :
    rerank cos (hits.map fun x => { x with engine := f x }) limit = rerank cos hits limit := by
  rw [rerank_exact cos _ limit (by simpa using h), rerank_exact cos _ limit h]
  simp [Function.comp_def]

/-- `index_scores_are_exact_against_an_approximate_engine` and
`the_overfetch_lets_the_exact_rank_decide_the_boundary`: the engine (int8
scores) puts node 2 first; the exact scores put node 1 first, so a fetch of
two (`limit + KNN_OVERFETCH`) re-ranked to one gives node 1, and the order is
the exact scan's on the same vectors. -/
example :
    let cos : Nat → Nat := fun v => v
    let hits := [⟨2, 0, some 70, 90⟩, ⟨1, 0, some 80, 85⟩]
    rerank cos hits 1 = [1] ∧ rerank cos hits 2 = exactScan cos [(1, 80, 0), (2, 70, 0)] 2 := by
  decide

/-- Ties resolve by the issue-2 tie-break, key then id. -/
example : rankExact [(5, 10, 2), (4, 10, 2), (3, 10, 1)] 3 = [3, 4, 5] := by decide

/-- **A hit that came back without its vector keeps the engine's score**
(`rerank`'s documented exception), so the order is no longer the exact scan's:
node 2's stored vector scores 70, its engine score 90 puts it ahead of node 1
(exact 80). -/
theorem rerank_partial_not_exact :
    let cos : Nat → Nat := fun v => v
    rerank cos [⟨1, 0, some 80, 85⟩, ⟨2, 0, none, 90⟩] 2 = [2, 1] ∧
      exactScan cos [(1, 80, 0), (2, 70, 0)] 2 = [1, 2] := by decide

end Search

/-! ## Runs from the tests -/

section Runs

/-- The tests' tier: repairs without waiting (`with_repair_backoff(ZERO)`). -/
def Pt : Params := { backoff := 0 }

/-- What a read returned, at some nodes: `none` when it was not the index. -/
def Answer.at (a : Answer) (ns : List Nat) : Option (List (Option Vec)) :=
  match a with
  | .index _ view => some (ns.map view)
  | _ => none

def Answer.isFallback : Answer → Bool
  | .fallback => true
  | _ => false

def answerAt (s : St) (ns : List Nat) : Option (List (Option Vec)) :=
  s.answer.bind (·.at ns)

def fellBack (s : St) : Bool := s.answer.any (·.isFallback)

/-- The index document of node `n` in contract `c`. -/
def docAt (s : St) (c n : Nat) : Option Vec := (s.rt c n).doc

def markerEpoch (s : St) : Option Nat := s.mark.map (·.2)

/-- Acquire, then load (`attach`, the order `Memory` attaches in). -/
def attachEvs : List Ev := [.acquire, .loadBegin, .loadEnd]

/-- `seed_batch`: contract 1, two vectors of build 0, one concept without. -/
def seed : List Mut := [.setContract (some 1), .upsert 1 (some (0, 10)), .upsert 2 (some (0, 20)),
  .upsert 3 none]

/-- Attached, seeded and mirrored: in sync, the index serves nodes 1 and 2. -/
def seeded : St := run Pt {} (attachEvs ++ [.write seed] ++ steps .mirStep 5)

/-- `a_committed_flush_is_mirrored_and_the_index_serves_the_vector_leg`. -/
example : seeded.tier.sync = .inSync ∧ markerEpoch seeded = some 1 ∧
    answerAt (run Pt seeded [.read 1]) [1, 2, 3] = some [some (0, 10), some (0, 20), none] := by
  decide

/-- `a_contract_mismatch_is_refused_...`: a read expecting another contract is
refused. -/
example : (match (run Pt seeded [.read 2]).answer with | some .refused => true | _ => false) = true := by
  decide

/-- `a_mirror_failure_keeps_the_flush_and_the_next_flush_repairs`: the flush
is durable while the index is down, reads fall back, and the next flush after
the index returns repairs the session in full. -/
def outage : St := run Pt {} (attachEvs ++ [.setDown true, .write seed, .mirStep, .setDown false])

example : outage.d.vecs.length = 2 ∧ outage.tier.sync = .stale ∧ outage.tier.failures = 1 ∧
    fellBack (run Pt outage [.read 1]) = true := by decide

def outageRepaired : St :=
  run Pt outage ([.write [.upsert 4 (some (0, 40))]] ++ steps .repStep 20 ++ [.read 1])

example : outageRepaired.tier.sync = .inSync ∧ markerEpoch outageRepaired = some 2 ∧
    answerAt outageRepaired [1, 2, 3, 4] = some [some (0, 10), some (0, 20), none, some (0, 40)] ∧
    outageRepaired.rep = .idle := by decide

/-- `repair_converges` applies to the outage test: after the index came back
and the next flush asked for a repair, the task is at its load with every
hypothesis met, and the theorem gives the match. -/
def atLoad : St := run Pt outage [.write [.upsert 4 (some (0, 40))], .repStep]

example : atLoad.rep = .load 1 ∧ atLoad.tier.held = some 1 ∧ atLoad.d.lease = 1 ∧
    atLoad.d.row = .ours ∧ atLoad.d.erased = false ∧ atLoad.down = false ∧
    checkMarker false atLoad.mark (some atLoad.d) = .behind ∧
    nextVersion atLoad.tier.tok atLoad.tier.cnt 1 = some (1, 2, shift + 2) ∧
    atLoad.mark = none ∧ atLoad.tier.committed ≤ atLoad.d.epoch ∧ atLoad.tier.again = false ∧
    atLoad.d.vecs.length = 3 := by decide

theorem atLoad_rt : atLoad.rt = Idx.empty := rfl

example : Matches (run Pt atLoad (steps .repStep (2 * 3 + 9))) :=
  (repair_converges Pt atLoad 1 1 2 (shift + 2) (by decide) (by decide) (by decide)
    (by decide) (by decide) (by decide) (by decide) (by decide)
    (by intro c n; rw [atLoad_rt]; simp [Idx.empty, shift])
    (by intro w e h; simp [show atLoad.mark = none by decide] at h)
    (by decide) (by decide)).1

/-- `repairs_run_one_at_a_time_per_session` and #18 review F4
(`a_repair_overtaken_by_a_flush_is_not_trusted_until_the_rerun`): a flush that
commits while a repair runs is not in its snapshot; the pass writes its own
epoch but does not mark the session in sync, a read falls back, and the rerun
settles it. -/
def overtaken : St :=
  run Pt outage ([.write [.upsert 4 (some (0, 40))]] ++ steps .repStep 12 ++
    [.write [.upsert 5 (some (0, 50))]] ++ steps .repStep 4)

example : overtaken.rep = .head ∧ markerEpoch overtaken = some 2 ∧ overtaken.tier.sync ≠ .inSync ∧
    overtaken.tier.repairing = true ∧ fellBack (run Pt overtaken [.read 1]) = true := by decide

example : (run Pt overtaken (steps .repStep 30)).tier.sync = .inSync ∧
    markerEpoch (run Pt overtaken (steps .repStep 30)) = some 3 ∧
    docAt (run Pt overtaken (steps .repStep 30)) 1 5 = some (0, 50) := by decide

/-- `a_repair_that_lost_its_lease_stops_writing` (#18 review F2): the holder's repair
lands its first chunk, the lease lapses and another process erases the
session; the repair's next fence check stops it. One write landed, no marker,
stale, and the lost lease is not counted as an index failure. -/
def lostLease : St :=
  run Pt outage ([.write [.upsert 4 (some (0, 40))]] ++ steps .repStep 6 ++ [.eraseOther] ++
    steps .repStep 10)

example : lostLease.log.filter (· = .repair 1) = [.repair 1] ∧ lostLease.mark = none ∧
    lostLease.tier.sync = .stale ∧ lostLease.tier.failures = 1 ∧ lostLease.tier.held = none ∧
    lostLease.rep = .idle := by decide

/-- `erase_stops_this_stores_repair_before_it_sweeps` (#18 review F6): an erase through
this store fences the repair, waits for it, then sweeps: nothing of the
session is left and the state is gone. -/
def erasedHere : St :=
  run Pt outage ([.write [.upsert 4 (some (0, 40))]] ++ steps .repStep 6 ++ [.eraseStop] ++
    steps .repStep 10 ++ [.eraseFinish])

example : erasedHere.mark = none ∧ docAt erasedHere 1 1 = none ∧ docAt erasedHere 1 4 = none ∧
    erasedHere.tier = {} ∧ erasedHere.erasing = false := by decide

/-- `a_flush_is_not_mirrored_after_the_lease_was_lost` and the #293 review
(`a_refresh_that_retakes_the_lease_does_not_adopt_its_token`): after the
operator override the heartbeat's refresh re-takes the row at a new token, the
store stops calling the lease held, the writer's next flush is refused by the
primary, and no repair runs. -/
def retaken : St := run Pt seeded [.override, .refresh]

example : retaken.d.lease = 2 ∧ retaken.tier.held = none ∧ retaken.wtok = some 1 ∧
    (run Pt retaken [.write [.upsert 4 (some (0, 40))]]).d = retaken.d := by decide

/-- `a_marker_ahead_of_the_loaded_snapshot_is_rechecked_not_repaired` (#18 review M4): a
load that read e1 while the holder's own flush committed and mirrored e2 sees
the marker ahead, re-checks against a fresh load, and is in sync without a
repair. -/
example :
    let s := run Pt seeded ([.loadBegin, .write [.upsert 4 (some (0, 40))]] ++ steps .mirStep 3 ++
      [.loadEnd])
    s.tier.sync = .inSync ∧ s.rep = .idle ∧ s.tier.repairing = false := by decide

/-- `an_unleased_flush_is_not_mirrored_and_the_next_holder_repairs` (#18 review L4). -/
example :
    let s := run Pt {} [.writeUnleased seed]
    s.tier.sync = .stale ∧ s.log = [] ∧
      (run Pt s (attachEvs ++ steps .repStep 20)).tier.sync = .inSync := by decide

/-! ### What the model shows about the Rust -/

/-- **A re-embed migration through the tier writes its new vectors into the
old contract's index.** `Graph::reembed_all` drains every `UpsertNode`
**before** the trailing `SetEmbedding` (so the durable flush is one
old-to-new transaction), but `project` switches the contract only for what
follows a `SetEmbedding`: the rewrite is projected into contract 1's index
under contract 1's hash, the new contract 2's index gets nothing, the marker
advances and the session stays in sync. A read under the new contract is
served from the index and finds no vector at all, and since the marker is in
sync no load ever repairs it (only `lambo recall-index backfill` would);
contract 1's index now holds build-1 vectors under contract 1's name, which a
reader still holding contract 1 ranks a contract-1 probe against. -/
def reembedBatch : List Mut := [.upsert 1 (some (1, 11)), .upsert 2 (some (1, 21)), .setContract (some 2)]

theorem project_reembed_into_old_index :
    project reembedBatch (some 1) = ([(1, 1, some (1, 11)), (1, 2, some (1, 21))], some 2) := by
  decide

def migrated : St := run Pt seeded ([.write reembedBatch] ++ steps .mirStep 4 ++ [.read 2])

theorem reembed_migration_misindexed :
    migrated.d.contract = some 2 ∧ lookup 1 migrated.d.vecs = some (1, 11) ∧
      migrated.tier.sync = .inSync ∧ markerEpoch migrated = some 2 ∧
      answerAt migrated [1, 2] = some [none, none] ∧
      docAt migrated 1 1 = some (1, 11) ∧ docAt migrated 1 2 = some (1, 21) ∧
      (run Pt migrated ([.loadBegin, .loadEnd] ++ steps .repStep 20 ++ [.read 2])).rep = .idle ∧
      answerAt (run Pt migrated ([.loadBegin, .loadEnd] ++ steps .repStep 20 ++ [.read 2])) [1, 2]
        = some [none, none] := by decide

/-- The same switch with `SetEmbedding` first (the order `project`'s table and
`a_contract_mismatch_is_refused_and_a_switch_queries_only_the_new_index`
use) lands in the new index. -/
example :
    answerAt (run Pt seeded ([.write [.setContract (some 2), .upsert 1 (some (1, 11)),
      .upsert 2 (some (1, 21))]] ++ steps .mirStep 4 ++ [.read 2])) [1, 2]
      = some [some (1, 11), some (1, 21)] := by decide

/-- **The #296 review's case: an in-place re-embed briefly mixes two builds.**
`--rewrite-same-contract` drains `UpsertNode`s only; the durable store takes
them in one transaction, but the index takes the bulk item by item (and a
periodic refresh can expose any prefix of it) into the same contract's index,
while the session stays in sync: a read between two items ranks build-1 and
build-0 vectors together. The durable store never holds the mix. -/
def inPlace : List Mut := [.upsert 1 (some (1, 11)), .upsert 2 (some (1, 21))]

theorem inplace_reembed_mixes_builds :
    let s := run Pt seeded [.write inPlace, .mirStep, .read 1]
    s.tier.sync = .inSync ∧ answerAt s [1, 2] = some [some (1, 11), some (0, 20)] ∧
      s.d.vecs = [(2, 1, 21), (1, 1, 11)] := by decide

/-- Once the bulk and the marker land the answer is one build again. If the
bulk fails part way the holder goes stale and falls back; a reader in another
process does not (`reader_never_rechecks`). -/
example :
    answerAt (run Pt seeded ([.write inPlace] ++ steps .mirStep 4 ++ [.read 1])) [1, 2]
      = some [some (1, 11), some (1, 21)] ∧
    fellBack (run Pt seeded [.write inPlace, .mirStep, .setDown true, .mirStep, .setDown false,
      .read 1]) = true := by decide

/-- **A reader that once found the session in sync never re-checks it.**
`sync_for_read` re-checks only `Unknown` and `Stale` sessions; an `InSync`
one is trusted until this store reloads it or evicts it. Another holder's
mirror fails (its batch deleted node 1), the marker stays behind the durable
epoch, and the reader keeps serving node 1 from the index however much time
passes. -/
def readerSeeded : St := run Pt {} [.takeover, .otherWrite seed true, .read 1]

theorem reader_never_rechecks :
    let s := run Pt readerSeeded [.otherWrite [.delete 1] false, .tick 1000000, .read 1]
    readerSeeded.tier.sync = .inSync ∧ lookup 1 s.d.vecs = none ∧ markerEpoch s = some 1 ∧
      s.d.epoch = 2 ∧ answerAt s [1] = some [some (0, 10)] := by decide

/-- A reload does see it (`a_reader_that_saw_the_marker_behind_...`). -/
example : fellBack (run Pt readerSeeded [.otherWrite [.delete 1] false, .loadBegin, .loadEnd,
    .read 1]) = true := by decide

/-- **A mirror in flight lands after another process's erase.** The mirror
checks only this store's record of the lease, never the durable row
(`ensure_fenced` guards repairs, not mirrors): a flush that committed before
the writer stalled past its lease, then an erase by another process that
swept the index and deleted the marker and reported done, then the stalled
mirror resumes, and the erased session's vector and marker are back in the
index. -/
theorem mirror_lands_after_erase :
    let s := run Pt seeded ([.write [.upsert 4 (some (0, 40))], .eraseOther] ++ steps .mirStep 3)
    s.d.erased = true ∧ docAt s 1 4 = some (0, 40) ∧ markerEpoch s = some 2 ∧
      s.log.take 2 = [.mirror 1, .mirror 1] := by decide

/-- The repair in the same position writes nothing (#18 review F2): its fence reads the
durable row. -/
example :
    let s := run Pt outage ([.write [.upsert 4 (some (0, 40))]] ++ steps .repStep 4 ++
      [.eraseOther] ++ steps .repStep 20)
    s.log.filter (· matches .repair _) = [] ∧ s.mark = none := by decide

/-- **A repair can reconcile from the snapshot it loaded before re-checking an
ahead marker** (`repair_once`). #18 review M4 says a marker ahead of a load is re-checked
against a fresh load and never repaired from; but when the fresh re-check
finds the marker *behind* (another flush committed in between), `repair_once`
reconciles from the first, older snapshot. The holder loads while its own e2
mirror is in flight (marker behind: a repair is queued while the session is
still in sync), e2's mirror lands, the repair loads e2, e3 (deleting node 2)
commits and mirrors, e4 commits; the repair's check finds the marker (e3)
ahead of e2, the re-check finds it behind e4, and the pass re-indexes node 2
and rewinds the marker to e2. The #18 review F4 guard sends it round again, which
repairs it: for the holder, reads fall back meanwhile; another process's
reader in sync (`reader_never_rechecks`) serves the deleted node. -/
def rewound : St :=
  run Pt seeded ([.write [.upsert 4 (some (0, 40))], .loadBegin, .loadEnd] ++ steps .mirStep 3 ++
    [.repStep, .repStep, .write [.delete 2]] ++ steps .mirStep 3 ++
    [.write [.upsert 5 (some (0, 50))]] ++ steps .repStep 14)

theorem repair_from_pre_recheck_snapshot :
    lookup 2 rewound.d.vecs = none ∧ rewound.d.epoch = 4 ∧
      docAt rewound 1 2 = some (0, 20) ∧ markerEpoch rewound = some 2 ∧
      rewound.tier.sync ≠ .inSync ∧ rewound.rep = .head := by decide

set_option maxRecDepth 8000 in
example :
    let s := run Pt rewound (steps .mirStep 3 ++ steps .repStep 30)
    s.tier.sync = .inSync ∧ docAt s 1 2 = none ∧ docAt s 1 5 = some (0, 50) ∧
      markerEpoch s = some 4 := by decide

/-- **A stale repair pass rewinds the version counter.** `next_version`
restarts the counter whenever the token differs, an older token included,
and `reconcile` calls it before its first fence. A pass that captured token 1
reaches `reconcile` after this store was overridden and re-acquired at token 3
and mirrored node 5 under 3 at counter 1: the pass sets the recorded token
back to 1 and its fence stops it. The next token-3 write starts again at
counter 1, the version node 5's document already has, so its bulk and marker
are refused as conflicts that count as success; the repair it triggers does
the same. The session ends in sync with node 5's old vector served, and the
next mirrored flush advances the marker, so a reload finds it in sync too:
permanent, silent staleness. -/
def stalePass : St :=
  run Pt seeded ([.write [.upsert 4 (some (0, 40))], .loadBegin, .loadEnd, .repStep, .repStep,
    .repStep] ++ steps .mirStep 3 ++ [.takeover, .override, .acquire, .loadBegin, .loadEnd,
    .write [.upsert 5 (some (0, 50))]] ++ steps .mirStep 3)

def stalePassAfter : St :=
  run Pt stalePass ([.repStep, .repStep, .write [.upsert 5 (some (0, 55))]] ++ steps .mirStep 3 ++
    steps .repStep 30 ++ [.read 1])

def stalePassLater : St :=
  run Pt stalePassAfter ([.write [.upsert 6 (some (0, 60))]] ++ steps .mirStep 3 ++
    [.loadBegin, .loadEnd] ++ steps .repStep 30 ++ [.read 1])

set_option maxRecDepth 20000 in
/-- `stalePass`'s run, pinned: the stale pass rewinds the recorded token, the
next write's version repeats one already in the index, and the old vector is
served while the session is in sync, through a later flush and a reload. -/
theorem stale_pass_rewinds_version :
    stalePass.tier.tok = 3 ∧ stalePass.tier.cnt = 1 ∧ (stalePass.rt 1 5).ver = 3 * shift + 1 ∧
      (run Pt stalePass [.repStep]).tier.tok = 1 ∧ (run Pt stalePass [.repStep]).tier.cnt = 1 ∧
      stalePassAfter.tier.sync = .inSync ∧ lookup 5 stalePassAfter.d.vecs = some (0, 55) ∧
      answerAt stalePassAfter [5] = some [some (0, 50)] ∧
      stalePassLater.tier.sync = .inSync ∧ markerEpoch stalePassLater = some 5 ∧
      stalePassLater.d.epoch = 5 ∧ lookup 5 stalePassLater.d.vecs = some (0, 55) ∧
      answerAt stalePassLater [5, 6] = some [some (0, 50), some (0, 60)] ∧
      stalePassLater.rep = .idle := by decide

/-- **A batch that deletes only edges leaves the marker behind.** It names no
session and deletes no node, so `mirror_unnamed` returns before attributing
it: the durable epoch moves and the marker does not. The holder stays in sync
(the index content is right), but its next load finds the marker behind and
re-indexes the whole session, and a reader falls back until the holder's next
mirrored flush. -/
theorem edge_only_batch_leaves_marker_behind :
    let s := run Pt seeded [.write [.deleteEdge]]
    s.d.epoch = 2 ∧ markerEpoch s = some 1 ∧ s.tier.sync = .inSync ∧
      (run Pt s [.loadBegin, .loadEnd]).rep = .head := by decide

end Runs

end Lambo.Tiered
