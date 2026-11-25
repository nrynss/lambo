import LamboProofs.Tiered.Pure

/-
# The tiered store: a SQL store of record beside an Elastic recall index (#18)

Models `TieredStore` in `src/store/tiered/mod.rs` as one state machine: a
durable store (SQLite, Postgres, Cockroach) that stays the source of truth,
and an Elasticsearch index mirrored from it that serves the vector leg. The
mirror (`mirror`, `mirror_unnamed`, `mirror_session`, `mark_unleased`,
`mirror_ops`, `mark_stale`), the repair (`request_repair`, `run_repairs`,
`repair_once`, `reconcile`, `repair_if_due`, `sweep`, `ensure_fenced`,
`recheck_ahead`, `settle_after_load`, `stop_repair_for_erase`), reads
(`sync_for_read`, `checked_vector_candidates`, `fallback`, `rerank`) and the
lease (`release_lease`, `erase_session`, `flush`, `load_session`), over
`Pure.lean`'s pieces; the engine's rules the tier relies on (external
versions, a conflict counted as success, delete-by-query leaving a tombstone
one version up) as `index.rs` and `fake.rs` model them; and `rank_by_cosine`
in `src/store/vector_source.rs`. `St` is one session: the durable side, the
writer's captured token, the index (documents by contract and node, the sync
marker), this store's `Tier`, the repair task's position, the in-flight
mirror, orphaned requests, a half-done load, the read breaker and a log of
every index write that landed. `step` takes one `Ev`: flushes by this writer
or another holder, one step of the mirror or the repair, their deadlines, an
orphaned request landing, the two halves of a load, a read, acquire, refresh,
release, takeover, operator override, erase, outages and time. Each repair
step is one request or one lock hold; the mirror's bulk lands item by item and
a read sees any prefix; chunks are one document (the tests'
`with_repair_chunk(1)`). `Params.switchRepairs` selects the mirror since #319
(`true`) or before it; every theorem here holds for both.

Results: SQL authority: no index-side event touches the durable session
(`indexSide_keeps_durable`), an accepted flush is exactly its batch
(`write_commits`), the one mirror a flush waits for always ends
(`mirror_ends`, `mirTimeout_ends`), the token never goes back (`lease_mono`,
`run_lease_mono`) and a fenced write is refused (`fenced_write_refused`).
Convergence, "stale, never meaningless": a pass that starts under the current
lease with the marker behind and no further writes leaves the index matching
the durable store, the marker at the durable epoch and the session in sync
(`repair_converges`), given version dominance, which is an assumption, not an
invariant (`nextVersion_gt` gives it only when the store's recorded token is
at most the pass's); `putAll_eq`, `reconcile_matches`; an overtaken repair is
not trusted (`finish_overtaken`, `finish_inSync`). While the breaker is open,
or while not in sync, a read is the durable store's (`read_breaker_open`,
`read_not_inSync_falls_back`). Fencing (#18, #293): once the lease moved past
a token and the repair is off its in-flight request, no repair write under
that token ever lands (`repair_silent_after_loss`, `erase_silences_repair`),
and a mirror starts only from an accepted flush with this store holding
exactly the writer's token (`mirror_starts_held`). After a load the session is
trusted exactly when the re-checked marker is in sync, unless a holder finds
it behind and asks for a repair (`settle_consistent`, `settle_trusted`). With
every stored vector returned the engine's scores and order do not matter and
the answer is `rank_by_cosine` over the candidates (`rerank_ignores_engine`,
`rerank_exact`); a hit returned without its vector keeps the engine's score
and can outrank a nearer one (`rerank_partial_not_exact`). Since #319 a batch
that switches the contract is durable but writes no document or marker into
any index and leaves the session out of sync, so it falls back and is
repaired (`mirror_switch_writes_nothing`, `write_switch_writes_nothing`), and
every batch that does not switch mirrors exactly as before (`write_agrees`,
`write_agrees_noSet`, `write_agrees_stampFirst`).

Boundary: scores are exact fixed-point cosines, so `f32`/`f64` rounding in
`rank_by_cosine` and the engine is outside the model. The refresh interval is
not modelled as such (the realtime view stands for every view a refresh can
expose; delete-by-query is atomic with its refresh, so the #18 refresh-first
rule and the version-conflict retries in `sweep` and `erase_session` are
assumed, not proved); nor are `ensure_index`, interactions and graph
structure, multiple sessions (`mirror` over several sessions is reduced to
the one-session attribution rule), the eviction bound and `repair_run`, log
messages and `last_error`, timeouts as durations (a deadline is an event), the
primary's own errors, `backfill_recall_index`, the unchecked
`vector_candidates` surface and holder derive. `eraseOther` has no
lease-liveness gate; the real `erase_gate` answers `Held` while another
holder's lease is live. #273 Track 3 proposes `tiered/repair.rs`,
`tiered/mirror.rs` and `tiered/search.rs`; the Rust is not split yet, and the
invariants here span all three.
-/

namespace Lambo.Tiered
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

/-- The contract `mirror_ops` projects from (`durable_contract`): the cached
one, else the durable row, which already holds the batch. -/
def mirrorBefore (s : St) (d' : Durable) : Option Nat :=
  match s.tier.contract with
  | some c => c
  | none => d'.contract

/-- `mirror_ops` once the version `v` is taken: start the bulk and the marker
at epoch `e`. With the #319 fix a batch that `switches` the contract writes
nothing: the session goes stale under the contract after the batch and a
repair is asked for, which re-indexes from the durable snapshot. Not counted
as a mirror failure. -/
def startMirror (s : St) (ms : List Mut) (t tok cnt v : Nat) (before : Option Nat) (e : Nat) :
    St :=
  let pr := project ms before
  if P.switchRepairs && switches ms before then
    repairIfDue { s with tier := { s.tier with tok := tok, cnt := cnt, contract := some pr.2,
                                               sync := .stale } }
  else
    { s with tier := { s.tier with tok := tok, cnt := cnt, contract := some pr.2 },
             mir := .puts t v e pr.1 }

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
      | some (tok, cnt, v) => startMirror P s ms t tok cnt v (mirrorBefore s d') d'.epoch

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
  if namesSession ms then mirrorSession P s ms t d' else
  let del := deletedIds ms
  if del = [] then s
  else if t.isSome ∧ s.tier.held = t then mirrorSession P s ms t d'
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
      mirror P { s with d := d' } ms s.wtok d'
    else s
  | .writeUnleased ms =>
    if s.mir = .idle ∧ s.d.erased = false ∧ s.d.row = .none then
      let d' := applyBatch s.d ms
      mirror P { s with d := d' } ms none d'
    else s
  | .otherWrite ms mirrored =>
    if s.d.erased = false ∧ s.d.row = .other then
      let d' := applyBatch s.d ms
      let s' := { s with d := d' }
      -- With the #319 fix the other holder repairs a switching batch rather
      -- than mirror it (its repair is not modelled).
      if mirrored ∧ s.down = false ∧ (P.switchRepairs && switches ms s.d.contract) = false then
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

/-- What `startMirror` leaves alone: the durable store, the write log, the
orphans and the index; the repair task stays or is asked for. -/
theorem startMirror_aux (s : St) (ms : List Mut) (t tok cnt v : Nat) (b : Option Nat) (e : Nat) :
    (startMirror P s ms t tok cnt v b e).d = s.d ∧ (startMirror P s ms t tok cnt v b e).log = s.log ∧
      (startMirror P s ms t tok cnt v b e).orphans = s.orphans ∧
      (startMirror P s ms t tok cnt v b e).rt = s.rt ∧
      (startMirror P s ms t tok cnt v b e).mark = s.mark ∧
      ((startMirror P s ms t tok cnt v b e).rep = s.rep ∨
        (startMirror P s ms t tok cnt v b e).rep = .head) := by
  unfold startMirror repairIfDue requestRepair
  dsimp only
  split <;> (try split) <;> (try split) <;> simp

theorem startMirror_d (s : St) (ms : List Mut) (t tok cnt v : Nat) (b : Option Nat) (e : Nat) :
    (startMirror P s ms t tok cnt v b e).d = s.d := (startMirror_aux P s ms t tok cnt v b e).1

theorem mirrorSession_d (s : St) (ms : List Mut) (t : Option Nat) (d' : Durable) :
    (mirrorSession P s ms t d').d = s.d := by
  unfold mirrorSession
  simp only
  split
  · rfl
  · split
    · exact repairIfDue_d _
    · split
      · rfl
      · split
        · rfl
        · exact startMirror_d P _ _ _ _ _ _ _ _

theorem mirror_d (s : St) (ms : List Mut) (t : Option Nat) (d' : Durable) :
    (mirror P s ms t d').d = s.d := by
  unfold mirror
  split
  · exact mirrorSession_d P s ms t d'
  · simp only
    split
    · rfl
    · split
      · exact mirrorSession_d P s ms t d'
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
  exact mirror_d P _ _ _ _

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

/-- The tiered store's durable lease token never decreases, over any run of
writes, mirrors, repairs and takeovers. -/
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
    (mirror P s ms t d').log = s.log ∨ (mirror P s ms t d').log = .unattributed :: s.log := by
  have hs : ∀ s', (mirrorSession P s' ms t d').log = s'.log := by
    intro s'
    unfold mirrorSession
    simp only
    split
    · rfl
    · split
      · unfold repairIfDue requestRepair; split <;> (try split) <;> rfl
      · split
        · rfl
        · split
          · rfl
          · exact (startMirror_aux P _ _ _ _ _ _ _ _).2.1
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
    (mirror P s ms t d').rep = s.rep ∨ (mirror P s ms t d').rep = .head := by
  have hr : ∀ s', (requestRepair s').rep = s'.rep ∨ (requestRepair s').rep = .head := by
    intro s'; unfold requestRepair; split
    · exact Or.inl rfl
    · exact Or.inr rfl
  have hs : ∀ s', (mirrorSession P s' ms t d').rep = s'.rep ∨
      (mirrorSession P s' ms t d').rep = .head := by
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
          · exact (startMirror_aux P _ _ _ _ _ _ _ _).2.2.2.2.2
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
    (mirror P s ms t d').orphans = s.orphans := by
  have hs : ∀ s', (mirrorSession P s' ms t d').orphans = s'.orphans := by
    intro s'
    unfold mirrorSession
    simp only
    split
    · rfl
    · split
      · unfold repairIfDue requestRepair; split <;> (try split) <;> rfl
      · split
        · rfl
        · split
          · rfl
          · exact (startMirror_aux P _ _ _ _ _ _ _ _).2.2.1
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

/-- A repair task at the head of `repair_once` (before it loads a snapshot)
has no repair write under any lease token on the wire, so a lost lease has
nothing of its to fence. -/
theorem writing_head (t : Nat) : RPc.head.writing t = false := rfl

/-- An idle repair task (`run_repairs` waiting for a request) has no repair
write under any lease token on the wire, so a lost lease has nothing of its
to fence. -/
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

theorem frame_mirror (s : St) (d : Durable) (ms : List Mut) (t : Option Nat) (d' : Durable) :
    Frame s (mirror P { s with d := d } ms t d') := by
  refine ⟨?_, Or.inl (mirror_orphans P _ _ _ _), ?_⟩
  · rcases mirror_tier_rep P { s with d := d } ms t d' with h | h
    · exact Or.inl h
    · exact Or.inr (Or.inl h)
  · rcases mirror_log P { s with d := d } ms t d' with h | h
    · exact Or.inl h
    · exact Or.inr (Or.inl h)

theorem step_frame (s : St) (e : Ev) (he : e ≠ .repStep) : Frame s (step P s e) := by
  cases e
  case write ms =>
    simp only [step]; split
    · exact frame_mirror P s _ ms _ _
    · exact frame_same rfl rfl rfl
  case writeUnleased ms =>
    simp only [step]; split
    · exact frame_mirror P s _ ms _ _
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
again** (#18, `ensure_fenced`). From a state where the repair task is not
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
again** (#18, `ensure_fenced`), over every run.
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
is the documented window (`ensure_fenced`). The legacy mirror step in this
machine lacks the #320 checks, which the runs model with `fencedMirrorTick`. -/
theorem erase_silences_repair (s : St) (t : Nat) (e : Ev)
    (he : e = .eraseStop ∨ e = .eraseOther ∨ e = .takeover) (hne : s.d.erased = false)
    (ht : t ≤ s.d.lease) (hw : s.rep.writing t = false)
    (ho : ∀ x ∈ s.orphans, x.1 ≠ .repair t) (evs : List Ev) :
    ∃ new, (run P (step P s e) evs).log = new ++ (step P s e).log ∧ Src.repair t ∉ new := by
  apply repair_silent_after_loss
  rcases he with rfl | rfl | rfl <;>
    exact ⟨by simp [step, hne, eraseDurable]; omega, by simp [step, hne, eraseDurable],
      by simpa [step, hne] using hw, by simpa [step, hne] using ho⟩

/-- Once the lease is past token `t`, a lease row exists, the repair task is
idle or at the head of `repair_once`, and no request under `t` is orphaned,
the old holder can land no repair write (`Quiet t`). -/
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

theorem startMirror_mir (s : St) (ms : List Mut) (t₀ tok cnt v₀ : Nat) (b : Option Nat) (e : Nat)
    {t v ep : Nat} {ops : List Op} (h : (startMirror P s ms t₀ tok cnt v₀ b e).mir = .puts t v ep ops) :
    s.mir = .puts t v ep ops ∨ t₀ = t := by
  unfold startMirror repairIfDue requestRepair at h
  dsimp only at h
  split at h
  · split at h
    · split at h <;> exact Or.inl h
    · exact Or.inl h
  · simp only [MPc.puts.injEq] at h
    exact Or.inr h.1

theorem mirror_mir (s : St) (ms : List Mut) (tok : Option Nat) (d' : Durable) (t v ep : Nat)
    (ops : List Op) (h : (mirror P s ms tok d').mir = .puts t v ep ops) :
    s.mir = .puts t v ep ops ∨ (tok = some t ∧ s.tier.held = some t) := by
  have hs : ∀ s', (mirrorSession P s' ms tok d').mir = .puts t v ep ops →
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
        · rename_i hne
          split at h
          · exact Or.inl h
          · rcases startMirror_mir P _ _ _ _ _ _ _ _ h with h | rfl
            · exact Or.inl h
            · exact Or.inr ⟨rfl, Classical.not_not.mp hne⟩
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
      rcases mirror_mir P _ ms _ _ t v ep ops h with h' | ⟨h1, h2⟩
      · simp [hi] at h'
      · obtain ⟨-, -, h3⟩ := hc
        rw [h3] at h1
        cases h1
        exact ⟨ms, rfl, h3, rfl, h2⟩
    · rw [hi] at h; cases h
  case writeUnleased ms =>
    simp only [step] at h
    split at h
    · rcases mirror_mir P _ ms _ _ t v ep ops h with h' | ⟨h1, -⟩
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

/-- **A repair overtaken by a flush is not trusted (#18).** At the end of a
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

/-- A mirror timeout ends the mirror task (`mirror_ops` is abandoned): nothing
more of that batch is mirrored. -/
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

/-- The re-rank is the durable store's ranking on the candidate set (#18). Close
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

/-- **The engine's scores never matter** (#18): when every hit carries
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

/-! ## The fix for #319: a contract switch is repaired, not mirrored -/

section Switch

variable (P : Params)

theorem switchesGo_noSet (ms : List Mut) (hm : ∀ c, Mut.setContract c ∉ ms) (cur : Option Nat)
    (up : Bool) : switchesGo ms cur up = false := by
  induction ms generalizing cur up with
  | nil => rfl
  | cons m ms ih =>
    have hm' : ∀ c, Mut.setContract c ∉ ms := fun c h => hm c (List.mem_cons_of_mem _ h)
    cases m with
    | setContract c => exact absurd List.mem_cons_self (hm c)
    | upsert n v => cases v <;> exact ih hm' _ _
    | delete n => exact ih hm' _ _
    | deleteEdge => exact ih hm' _ _
    | other => exact ih hm' _ _

/-- A batch with no `SetEmbedding` never switches. -/
theorem switches_noSet (ms : List Mut) (b : Option Nat) (hm : ∀ c, Mut.setContract c ∉ ms) :
    switches ms b = false := switchesGo_noSet ms hm b false

/-- A `SetEmbedding` first that stamps the session's first contract (every
`hybrid::derive` flush that stamps) or keeps the one in force, with no other
`SetEmbedding` after it, never switches. -/
theorem switches_stampFirst (c b : Option Nat) (ms : List Mut) (hm : ∀ c', Mut.setContract c' ∉ ms)
    (hb : b = none ∨ b = c) : switches (.setContract c :: ms) b = false := by
  show ((decide (c ≠ b) && (b.isSome || false)) || switchesGo ms c false) = false
  rw [switchesGo_noSet ms hm]
  rcases hb with rfl | rfl <;> simp

theorem switchesGo_names (ms : List Mut) (cur : Option Nat) (up : Bool)
    (h : switchesGo ms cur up = true) : namesSession ms = true := by
  induction ms generalizing cur up with
  | nil => cases h
  | cons m ms ih =>
    cases m with
    | setContract c => simp [namesSession]
    | upsert n v => simp [namesSession]
    | other => simp [namesSession]
    | delete n =>
      have := ih cur up h
      simp only [namesSession, List.any_cons] at this ⊢
      simpa using this
    | deleteEdge =>
      have := ih cur up h
      simp only [namesSession, List.any_cons] at this ⊢
      simpa using this

/-- A switching batch names its session, so it reaches `mirror_session`. -/
theorem switches_names (ms : List Mut) (b : Option Nat) (h : switches ms b = true) :
    namesSession ms = true := switchesGo_names ms b false h

theorem startMirror_agrees (P' : Params) (s : St) (ms : List Mut) (t tok cnt v : Nat)
    (b : Option Nat) (e : Nat) (h : switches ms b = false) :
    startMirror P s ms t tok cnt v b e = startMirror P' s ms t tok cnt v b e := by
  simp [startMirror, h]

theorem mirrorSession_agrees (P' : Params) (s : St) (ms : List Mut) (t : Option Nat) (d' : Durable)
    (h : switches ms (mirrorBefore s d') = false) :
    mirrorSession P s ms t d' = mirrorSession P' s ms t d' := by
  unfold mirrorSession
  cases t with
  | none => rfl
  | some t =>
    dsimp only
    split
    · rfl
    · split
      · rfl
      · split
        · rfl
        · exact startMirror_agrees P P' _ _ _ _ _ _ _ _ h

theorem mirror_agrees (P' : Params) (s : St) (ms : List Mut) (t : Option Nat) (d' : Durable)
    (h : switches ms (mirrorBefore s d') = false) : mirror P s ms t d' = mirror P' s ms t d' := by
  unfold mirror
  rw [mirrorSession_agrees P P' s ms t d' h]

/-- **The fix changes nothing for a batch that does not switch**: the writer's
flush is the previous version's (`P'` any parameters that differ, the
previous version among them), index writes, marker and tier state alike. -/
theorem write_agrees (P' : Params) (s : St) (ms : List Mut)
    (h : switches ms (mirrorBefore s (applyBatch s.d ms)) = false) :
    step P s (.write ms) = step P' s (.write ms) := by
  have hm := mirror_agrees P P' { s with d := applyBatch s.d ms } ms s.wtok (applyBatch s.d ms) h
  simp only [step]
  split
  · exact hm
  · rfl

/-- In particular on every batch without a `SetEmbedding` (`inPlace` and every
ordinary flush)... -/
theorem write_agrees_noSet (P' : Params) (s : St) (ms : List Mut)
    (hm : ∀ c, Mut.setContract c ∉ ms) : step P s (.write ms) = step P' s (.write ms) :=
  write_agrees P P' s ms (switches_noSet ms _ hm)

/-- ... and on every batch whose `SetEmbedding` comes first and stamps the
session's first contract or keeps the one in force. -/
theorem write_agrees_stampFirst (P' : Params) (s : St) (c : Option Nat) (ms : List Mut)
    (hm : ∀ c', Mut.setContract c' ∉ ms)
    (hc : mirrorBefore s (applyBatch s.d (.setContract c :: ms)) = none ∨
      mirrorBefore s (applyBatch s.d (.setContract c :: ms)) = c) :
    step P s (.write (.setContract c :: ms)) = step P' s (.write (.setContract c :: ms)) :=
  write_agrees P P' s _ (switches_stampFirst c _ ms hm hc)

theorem repairIfDue_index (s : St) :
    (repairIfDue s).rt = s.rt ∧ (repairIfDue s).mark = s.mark ∧ (repairIfDue s).mir = s.mir ∧
      (repairIfDue s).tier.sync = s.tier.sync := by
  unfold repairIfDue requestRepair
  split <;> (try split) <;> exact ⟨rfl, rfl, rfl, rfl⟩

theorem startMirror_switch (s : St) (ms : List Mut) (t tok cnt v : Nat) (b : Option Nat) (e : Nat)
    (hp : P.switchRepairs = true) (h : switches ms b = true) :
    (startMirror P s ms t tok cnt v b e).rt = s.rt ∧ (startMirror P s ms t tok cnt v b e).mark = s.mark ∧
      (startMirror P s ms t tok cnt v b e).mir = s.mir ∧
      (startMirror P s ms t tok cnt v b e).tier.sync ≠ .inSync := by
  unfold startMirror
  simp only [hp, h, Bool.and_self, ↓reduceIte]
  obtain ⟨h1, h2, h3, h4⟩ := repairIfDue_index
    { s with tier := { s.tier with tok := tok, cnt := cnt, contract := some (project ms b).2,
                                   sync := .stale } }
  exact ⟨h1, h2, h3, by rw [h4]; exact fun h => Sync.noConfusion h⟩

/-- **A switching batch is never mirrored (#319).** Whatever the tier's state,
the mirror of a batch that moves the contract writes no document and no
marker into any contract's index (the old contract's included) and leaves
the session out of sync, so the holder's reads fall back to the durable
store (`read_not_inSync_falls_back`) until a repair re-indexes it from the
durable snapshot (`repair_converges`). -/
theorem mirror_switch_writes_nothing (s : St) (ms : List Mut) (t : Option Nat) (d' : Durable)
    (hp : P.switchRepairs = true) (h : switches ms (mirrorBefore s d') = true) :
    (mirror P s ms t d').rt = s.rt ∧ (mirror P s ms t d').mark = s.mark ∧
      (mirror P s ms t d').mir = s.mir ∧ (mirror P s ms t d').tier.sync ≠ .inSync := by
  unfold mirror
  simp only [switches_names ms _ h, ↓reduceIte]
  unfold mirrorSession
  cases t with
  | none => simp [markUnleased]
  | some t =>
    dsimp only
    split
    · rename_i hs
      obtain ⟨h1, h2, h3, h4⟩ := repairIfDue_index
        { s with tier := { s.tier with committed := max s.tier.committed d'.epoch } }
      exact ⟨h1, h2, h3, by rw [h4]; exact hs⟩
    · split
      · simp [markStale]
      · split
        · simp [markStale]
        · exact startMirror_switch P _ ms _ _ _ _ _ _ hp h

/-- The writer's flush of a switching batch: the batch is durable
(`write_commits`), the index (every contract's) and the marker are as they
were, no mirror is in flight and the session is out of sync. -/
theorem write_switch_writes_nothing (s : St) (ms : List Mut) (hp : P.switchRepairs = true)
    (hg : s.mir = .idle ∧ s.d.erased = false ∧ s.wtok = some s.d.lease)
    (h : switches ms (mirrorBefore s (applyBatch s.d ms)) = true) :
    (step P s (.write ms)).d = applyBatch s.d ms ∧ (step P s (.write ms)).rt = s.rt ∧
      (step P s (.write ms)).mark = s.mark ∧ (step P s (.write ms)).mir = .idle ∧
      (step P s (.write ms)).tier.sync ≠ .inSync := by
  obtain ⟨hm, he, ht⟩ := hg
  have e : step P s (.write ms) =
      mirror P { s with d := applyBatch s.d ms } ms s.wtok (applyBatch s.d ms) := by
    simp only [step, hm, he, ht, and_self, ite_true]
  obtain ⟨h1, h2, h3, h4⟩ :=
    mirror_switch_writes_nothing P { s with d := applyBatch s.d ms } ms s.wtok (applyBatch s.d ms) hp h
  rw [e]
  exact ⟨mirror_d P _ _ _ _, h1, h2, h3.trans hm, h4⟩

end Switch

end Lambo.Tiered
