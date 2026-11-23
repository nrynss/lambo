/-
# Write-queue drain and durable-intent replay: the base model

Model of how the asynchronous write queue's admitted writes end: drained by
the lane workers, set aside and settled at `close()`, or replayed from their
durable intents by a later serve. The model is in six files, all in the
namespace `Lambo.Drain`, so a theorem's name does not say which file holds
it:

* `Drain/Base.lean` (this file): writes, run results and the mutation log;
  the lane worker's batches (`workerStep`) and the receipt store;
  `settle_deferred`'s set-aside (`CloseFlush`, `deferEntry`,
  `settleDeferred`); durable intents, the intent table and the load order;
  the replay's step, streaks and constants (`replayStep`, `sickThreshold`,
  `lookupTimeoutLimit`).
* `Drain/Replay.lean`: the replay fixes (#294, #295), the order invariant
  across passes, the in-session hold (`workH`) and the replay's retry pass.
* `Drain/Worker.lean`: the current lane worker (`stepW`, `runW`), with
  admission, the caps, retries and the fence.
* `Drain/Close.lean`: the close, crash and shutdown over the current worker
  (`closeW`), dead-lettered batches at close (#345), worker panics and the
  dead-lettered set, and where the lookup-timeout streak resets (#346).
* `Drain/Examples.lean`: worked examples and the findings about the Rust.
* `Drain/History.lean`: the lane worker before #295 (`work`, `close`) and the
  replay before #294 (`replay`'s theorems), kept as the evidence for the
  findings they show. It describes no current Rust: it is not checked against
  the Rust for drift, and the docs do not cite it.

Rust items modelled across the files (paths as at `16e9aafa`):

* `src/writeq/execution.rs`: the lane worker `WritePipeline::spawn_worker`
  (pop, the lease-lost fence, `WriteCtx::run`, which failures consume the
  intent, `settle_one`; since #295 the held writes, `Lanes::held`), and `WriteCtx::run` as far as its `ConsumeStamp`
  goes: the consume is appended under the same lock hold as the commit.
* `src/writeq/drain.rs`: `quiesce`, `abort_workers` (every unsettled receipt
  set aside), `settle_deferred` with `CloseFlush` (including the
  `close_unflushed → intent_durable` upgrade on a retried close), and
  `abort_all_sync`, which only `Memory`'s `Drop` calls; and which
  `CloseFlush` `Memory::close` and `Memory::close_fenced`
  (`src/memory/shutdown.rs`) pass to `settle_deferred`.
* `src/writeq/replay.rs`: `spawn_replay` (seeding the restart map, the
  unconsumed filter, the liveness gate, the loop's match arm by arm, both
  streaks, `EMBEDDER_SICK_THRESHOLD`, `REPLAY_LOOKUP_TIMEOUT_LIMIT`; since
  #295 the deferred agents and `replay_order`).
* `Graph::record_write_intent` / `consume_write_intent`
  (`src/graph/graph/mutation_log.rs`), the adapters' consume (an `UPDATE` by
  receipt) and their load order `ORDER BY issued_ms, lane_seq`
  (`src/store/{sqlite,pg}/session_load.rs`).
* `StoreError::is_cycle_refusal`, `StoreError::is_input_refusal` (#294) and
  `LamboError::is_lookup_timeout` (`src/types/mod.rs`) as error classes
  (`Err`).

It imports `Receipts.lean` and reuses its `Answer`, `Entry`, `settle`,
`settleAll_fixed`, `Lane`, `run` and `submitted`.

Writes are abstract: a run's result is an outcome function `Job → RunResult`
(and `Job → RR` at replay); the model says what each result leads to, not why
a run fails. There is no `f64` here; the drain budget and the probe budget
are abstract (`done`, `live`).

**Atomicity assumption.** Concurrency is out of scope. Each worker step (run,
commit with its consume, settle) and each replay step is one atomic
operation, as the Rust's locks and its "no `.await` between a completed graph
write and its settle" rule make them; an aborted worker is a job that was not
run. A flush carries whole batches (one lock hold each), so a crash leaves a
prefix of the batch log durable.

Rows and consumes are keyed by the receipt id, modelled as its (epoch,
sequence) pair (`Job.key`), as the adapters key them by the full receipt
string: a backlog may hold several crashed processes' intents, and every
process's sequence starts at 1.

## Results

* `workerStep_atomic`, `workerStep_keys`: a write in a worker batch has its
  consume beside it, and a batch names only its own job. They are stated
  over `workerStep`, the worker step before #295, and carry over to the
  current worker, whose batches are `workerStep`'s (`runW_log_refines`).
* `retry_upgrades`: the one settled-to-settled change, a retried close
  upgrading `close_unflushed` (about `deferEntry`, so it holds of the current
  close; `closeW_retry_upgradesW`).
* `load_seq_order`, `load_epoch_seq_order`: the load order applies one
  process's rows in sequence (admission) order as long as no later admission
  carries an earlier issue time. Ties across epochs (equal `issued_ms` and
  `lane_seq`) are unspecified in SQL, while `loadOrder` keeps input order for
  them; no theorem depends on how they break. Since #295 the load is
  `fixedLoad` (`Replay.lean`), which needs no clock assumption.

The other theorems here (the receipt store, the intent table, the load
order's insertion sort) are the lemmas the other files' proofs rest on.

Not modelled: concurrency and the cancellation-safety custody of handles
(`WorkerCustody`, `ReplayCustody`); `stop_replay` running before the quiesce;
admission and its caps (`Receipts.lean`); receipt retention, eviction, the
undelivered queue, delivery destinations and the live entry a replay inserts;
the stale consumed-row skip at seeding; the adapters' lazy purge of consumed
rows older than the retention window (sqlite `write_rows.rs`, inside the
consume; it deletes only consumed rows, so it never touches a pending one);
counters, the ledger and logging; the
lease fence at the flush (a fenced flush is refused at the store, so a
consume appended after a lost lease never lands); `calibration.rs` (drain
does not depend on it).
-/

import LamboProofs.Receipts

namespace Lambo.Drain

open Lambo.Receipts (Answer Entry settle Lane)

/-! ## Writes, run results, the mutation log -/

/-- A queued write (`Job`, `admission.rs`), reduced to what drain and replay
read: the receipt's per-process sequence number (`ReceiptId::seq`, stored as
`WriteIntent::lane_seq`), the agent whose lane it is on, the receipt's issue
time in Unix milliseconds (`issued_ms`, from the pipeline's wall clock), and
whether it is an image derive (`JobPayload::DeriveImage`), and the random
epoch of the process that issued it (`ReceiptId::epoch`). -/
structure Job where
  rid : Nat
  agent : Nat
  ms : Int
  image : Bool
  epoch : Nat
  deriving DecidableEq, Repr

/-- What identifies a write across processes: the receipt id
`lwr1.{epoch}.{ms}.{seq}` (`ReceiptId`'s `Display`), here its (epoch, issue
time, sequence) triple. The adapters key intent rows by that string (`WHERE
session_id = ? AND receipt = ?`), and every process's sequence starts at 1, so
the sequence alone is not a key once a backlog holds two crashed processes'
intents. -/
abbrev Key := Nat × Int × Nat

def Job.key (j : Job) : Key := (j.epoch, j.ms, j.rid)

/-- The classes of `LamboError` that `WriteCtx::run` can fail with, as the
worker and the replay tell them apart. `cycle` is a `StoreError::Invariant`
for which `is_cycle_refusal` holds. `inputRefusal` is a non-cycle `Invariant`
refusing the write's input against the graph it met (#294): a repeated
`Observation`, a second `Hierarchical` parent, a `parent_of` pair whose ends
resolve to one canonical key or one concept. Before #294 the Rust raised it as
a plain `Invariant` the replay could not tell from `store`, and `replayStep`
treats it so. `store` is every other `Store` error (an `Invariant` that can
mean an inconsistent graph or store, `NotFound` such as a vanished
interaction, a backend error). `lookupTimeout` is an `EmbedUnavailable` for
which `is_lookup_timeout` holds; `unavailable` is every other
`EmbedUnavailable`. -/
inductive Err where
  | embed
  | unavailable
  | lookupTimeout
  | imageIdTaken
  | reference
  | cycle
  | inputRefusal
  | store
  | config
  | conflict
  | softLock
  | other
  deriving DecidableEq, Repr

/-- `LamboError::EmbedUnavailable`: "says nothing about the input". -/
def Err.transient : Err → Bool
  | .unavailable => true
  | .lookupTimeout => true
  | _ => false

/-- A fact about the write's own input against the graph it met, which the
replay settles `failed` and goes on past: `Embed`, `ImageIdTaken`,
`Reference`, and a cycle refusal (`StoreError::is_cycle_refusal`). -/
def Err.inputFact : Err → Bool
  | .embed => true
  | .imageIdTaken => true
  | .reference => true
  | .cycle => true
  | _ => false

/-- What one `WriteCtx::run` of a job returned. -/
inductive RunResult where
  | ok
  | err (e : Err)
  deriving DecidableEq, Repr

/-- `WriteIntentOutcome::tag`. -/
inductive Tag where
  | applied
  | failed
  | appliedAfterRestart
  deriving DecidableEq, Repr

/-- One entry of the write-behind mutation log, as far as drain and replay
touch it: `PutWriteIntent` (recorded at admission), the graph mutations of an
applied write (one `write` stands for all of them), and
`ConsumeWriteIntent`. -/
inductive Mut where
  | put (j : Job)
  | write (k : Key)
  | consume (k : Key) (t : Tag)
  deriving DecidableEq, Repr

/-- What is appended under one hold of the graph write lock. The flush drain
takes that same lock, so a flush carries whole batches, and a batch is one
store transaction (`Graph::consume_write_intent`'s doc). -/
abbrev Batch := List Mut

/-! ## The lane worker's step and the receipt store (`spawn_worker`, `execution.rs`)

`workerStep` is one job through the lane worker **before #295**: an
in-session `EmbedUnavailable` (an outage or a lookup timeout) answered
`failed`, left the intent owed, and let the agent's next write run. The
current worker holds such a write instead (`pending_replay`, retried with
backoff), settles a single lookup timeout `failed` with its intent consumed,
and holds a first-attempt lookup timeout once a run of them has reached the
limit (the third, and every later one until a write applies). It is
modelled by `stepW`/`runW` (`Worker.lean`), and its close, crash and
shutdown accounting in `Close.lean`. `workerStep` is here because the
current worker's batches are still its batches (`runW_log_refines`); the
worker loop before #295 (`work`, `close`) and the theorems over it are in
`History.lean`. The receipt store (`RStore`, `settleOne`) and what admission
leaves behind are shared by both layers. -/

/-- One job through the lane worker **before #295**: its receipt answer and
the batch it appends. `fenced` is the lease-lost fence, checked before the
run.

* lease lost: `failed`, nothing written, the intent deliberately not consumed;
* `Ok`: `applied`; the write and `ConsumeWriteIntent(applied)` under one lock
  hold (`WriteCtx::run`, "Same lock hold as the commit");
* `EmbedUnavailable` (outage or lookup timeout): `failed`, intent left
  unconsumed for the next serve;
* any other error: `failed`, intent consumed `failed` on its own hold.

The current worker (`finishW`, `startW`) differs on the third arm: an outage,
or a first-attempt lookup timeout once a run of them has reached the limit,
holds the write (`pending_replay`, nothing appended), and any other lookup
timeout settles `failed` with its intent consumed. Its batches are still `workerStep`'s
(`runW_log_refines`), so the batch lemmas below (`workerStep_keys`,
`workerStep_atomic`) carry over to it. -/
def workerStep (fenced : Bool) (j : Job) (r : RunResult) : Answer × Batch :=
  if fenced then (.failed, [])
  else
    match r with
    | .ok => (.applied, [.write j.key, .consume j.key .applied])
    | .err e => if e.transient then (.failed, []) else (.failed, [.consume j.key .failed])

/-- The receipt store's entries, keyed by receipt id (`Job.key`). -/
abbrev RStore := List (Key × Entry)

/-- The first entry for `r`. -/
def lookup : RStore → Key → Option Entry
  | [], _ => none
  | p :: s, r => if p.1 = r then some p.2 else lookup s r

/-- Apply `f` to the entries for `r`. -/
def update (s : RStore) (r : Key) (f : Entry → Entry) : RStore :=
  s.map (fun p => if p.1 = r then (p.1, f p.2) else p)

/-- `settle_one` (`receipts.rs`): `Entry::settle` on the job's entry; a
missing entry is skipped. The undelivered queue and the retention sweep are
not modelled. -/
def settleOne (s : RStore) (r : Key) (a : Answer) (now : Int) : RStore :=
  update s r (fun e => (settle e a now).1)

/-- What admission leaves behind for the jobs it accepted: a `Pending` entry
each, and a `PutWriteIntent` each, on its own hold. -/
def admitStore (js : List Job) : RStore := js.map (fun j => (j.key, ⟨.pending, none⟩))

def admitLog (js : List Job) : List Batch := js.map (fun j => [.put j])

/-! ## Close (`drain.rs`)

`CloseFlush`, `deferEntry`, `pendingIds` and `settleDeferred` are
`settle_deferred` and `abort_workers`' set-aside as they are today, shared by
both worker layers. The current close is `closeW`, over `runW`
(`Close.lean`); `close`, which composes them with the worker **before #295**
(`work`), is in `History.lean`. -/

/-- `CloseFlush`: how `close()`'s final flush ended. -/
inductive CloseFlush where
  | durable
  | incomplete
  deriving DecidableEq, Repr

/-- `settle_deferred` on one entry. `Durable`: `intent_durable`, upgrading a
`close_unflushed` left by an earlier failed attempt of the close (the one
settled-to-settled change, bypassing `Entry::settle`). `Incomplete`:
`close_unflushed`, through `Entry::settle`. -/
def deferEntry (flush : CloseFlush) (now : Int) (e : Entry) : Entry :=
  match flush with
  | .durable =>
    if e.answer = .closeUnflushed then { answer := .intentRecorded, settledAt := some now }
    else (settle e .intentRecorded now).1
  | .incomplete => (settle e .closeUnflushed now).1

/-- The ids `abort_workers` sets aside: every receipt still unsettled once
the workers are aborted and joined. -/
def pendingIds (s : RStore) : List Key :=
  (s.filter (fun p => !p.2.answer.isSettled)).map (·.1)

/-- `settle_deferred` over the set-aside ids. -/
def settleDeferred (flush : CloseFlush) (now : Int) (ids : List Key) (s : RStore) : RStore :=
  ids.foldl (fun s r => update s r (deferEntry flush now)) s

/-! ## Durable intents and their load order -/

/-- A durable `WriteIntent` row: the job, and its outcome once consumed. -/
structure Intent where
  job : Job
  outcome : Option Tag
  deriving DecidableEq, Repr

/-- One mutation applied to the `write_intents` table: a put inserts an
unconsumed row; a consume updates every row with that receipt (the adapters'
`UPDATE ... WHERE receipt = ?`, so a later consume overwrites an earlier one);
graph writes do not touch the table. The Rust's put is an upsert on
(`session_id`, `receipt`), so the table never holds two rows with one key,
where the model's put appends; the replay theorems take arbitrary rows, so
they cover the Rust's case as a special one. -/
def applyMut (acc : List Intent) : Mut → List Intent
  | .put j => acc ++ [⟨j, none⟩]
  | .write _ => acc
  | .consume r t => acc.map (fun i => if i.job.key = r then { i with outcome := some t } else i)

/-- The rows a flushed mutation sequence leaves. -/
def intentsOf (ms : List Mut) : List Intent := ms.foldl applyMut []

/-- `ORDER BY issued_ms ASC, lane_seq ASC` (both adapters' `load_session`). -/
def keyLe (a b : Intent) : Bool :=
  decide (a.job.ms < b.job.ms) || (decide (a.job.ms = b.job.ms) && decide (a.job.rid ≤ b.job.rid))

def insertK (x : Intent) : List Intent → List Intent
  | [] => [x]
  | y :: ys => if keyLe x y then x :: y :: ys else y :: insertK x ys

/-- The load order, as an insertion sort by `keyLe`. -/
def loadOrder : List Intent → List Intent
  | [] => []
  | x :: xs => insertK x (loadOrder xs)

/-! ## Replay (`spawn_replay`, `replay.rs`) -/

/-- What the replay meets at one intent. `halt`: the lease is lost or the
pipeline sealed before it (the loop's `break` at its head). `badId`: its
receipt id does not parse (`continue`); since #295's review, any id that does
not parse other than `unknownFormat`. `unknownFormat`: an id that starts
`lwr<digits>.` with a version other than 1, as a newer build would write; the
code before that review did not tell it from `badId`. `decodeRefused`: the
stored payload does not decode into a job (#129). `run r`: `WriteCtx::run`
returned `r`. -/
inductive RR where
  | halt
  | badId
  | unknownFormat
  | decodeRefused
  | run (r : RunResult)
  deriving DecidableEq, Repr

/-- `ReplayBlockReason` as the loop sets it when it stops; `unset` is a stop
that sets none (lease lost or sealed). -/
inductive Block where
  | unset
  | embedder
  | imageConfig
  | other
  deriving DecidableEq, Repr

/-- What the loop does with one intent. -/
inductive Step where
  | applied
  | failed
  | skip
  | stop (b : Block)
  deriving DecidableEq, Repr

/-- `transient_streak` and `lookup_streak`. -/
structure Streaks where
  transient : Nat
  lookup : Nat
  deriving DecidableEq, Repr

/-- `EMBEDDER_SICK_THRESHOLD`.
Pinned by `formal_constants_tests::drain`. -/
def sickThreshold : Nat := 3

/-- `REPLAY_LOOKUP_TIMEOUT_LIMIT = EMBEDDER_SICK_THRESHOLD`.
Pinned by `formal_constants_tests::drain`. -/
def lookupTimeoutLimit : Nat := sickThreshold

/-- The match on the run's result in `spawn_replay`, arm by arm in source
order. -/
def replayStep (image : Bool) (rr : RR) (s : Streaks) : Step × Streaks :=
  match rr with
  | .halt => (.stop .unset, s)
  | .badId => (.skip, s)
  | .unknownFormat => (.skip, s)
  -- `decode_refused`: the content arm, no streak reset.
  | .decodeRefused => (.failed, s)
  | .run .ok => (.applied, ⟨0, 0⟩)
  | .run (.err e) =>
    match e with
    | .lookupTimeout =>
      if lookupTimeoutLimit ≤ s.lookup + 1 then (.stop .other, { s with lookup := s.lookup + 1 })
      else (.failed, { s with lookup := s.lookup + 1 })
    | .unavailable =>
      if sickThreshold ≤ s.transient + 1 then
        (.stop .embedder, { s with transient := s.transient + 1 })
      else (.skip, { s with transient := s.transient + 1 })
    -- The content arm. Only `Embed` and `ImageIdTaken` prove the embedder
    -- answered, so only they reset the transient streak.
    | .embed => (.failed, { s with transient := 0 })
    | .imageIdTaken => (.failed, { s with transient := 0 })
    | .reference => (.failed, s)
    | .cycle => (.failed, s)
    -- Before #294: a plain `Invariant`, so the "anything else" arm below.
    | .inputRefusal => (.stop .other, s)
    -- #22: an image derive this configuration cannot apply.
    | .config => (.stop (if image then .imageConfig else .other), s)
    -- "Anything else (store/lease/config)".
    | .store => (.stop .other, s)
    | .conflict => (.stop .other, s)
    | .softLock => (.stop .other, s)
    | .other => (.stop .other, s)

/-- What a replay did: restart-map inserts in order, the batches it appended,
the jobs it applied and settled failed, and the block reason if it stopped
before the end of the backlog. -/
structure ROut where
  answers : List (Key × Answer)
  log : List Batch
  applied : List Job
  failed : List Job
  block : Option Block
  deriving DecidableEq, Repr

def ROut.empty : ROut := ⟨[], [], [], [], none⟩

/-- The replay loop over the pending intents, strictly one at a time. An
applied intent is consumed `applied_after_restart` in the same commit
(`WriteCtx::run` with a `ConsumeStamp`); a failed one is consumed `failed` on
its own hold; a skipped one is left unconsumed; a stop leaves it and every
later one unconsumed. -/
def loop (rout : Job → RR) : Streaks → List Intent → ROut
  | _, [] => ROut.empty
  | s, i :: rest =>
    match replayStep i.job.image (rout i.job) s with
    | (.applied, s') =>
      let o := loop rout s' rest
      { o with
        answers := (i.job.key, .appliedAfterRestart) :: o.answers
        log := [.write i.job.key, .consume i.job.key .appliedAfterRestart] :: o.log
        applied := i.job :: o.applied }
    | (.failed, s') =>
      let o := loop rout s' rest
      { o with
        answers := (i.job.key, .failed) :: o.answers
        log := [.consume i.job.key .failed] :: o.log
        failed := i.job :: o.failed }
    | (.skip, s') => loop rout s' rest
    | (.stop b, _) => { ROut.empty with block := some b }

/-- The unconsumed rows: `intents.filter(|i| i.outcome.is_none())`. -/
def pendingOf (intents : List Intent) : List Intent := intents.filter (fun i => i.outcome.isNone)

/-- `spawn_replay` after seeding: nothing owed, nothing to do; otherwise the
liveness embed gates the loop, and a failed one consumes nothing. -/
def replay (live : Bool) (rout : Job → RR) (intents : List Intent) : ROut :=
  match pendingOf intents with
  | [] => ROut.empty
  | p => if live then loop rout ⟨0, 0⟩ p else { ROut.empty with block := some .embedder }

/-- The rows after the replay's own consumes have landed. -/
def applyLog (intents : List Intent) (log : List Batch) : List Intent :=
  log.flatten.foldl applyMut intents

/-! ## The receipt store -/

theorem update_cons (k : Key) (e : Entry) (s : RStore) (r : Key) (f : Entry → Entry) :
    update ((k, e) :: s) r f = (if k = r then (k, f e) else (k, e)) :: update s r f := by
  simp only [update, List.map_cons]

theorem lookup_update (s : RStore) (r q : Key) (f : Entry → Entry) :
    lookup (update s r f) q = if q = r then (lookup s q).map f else lookup s q := by
  induction s with
  | nil => simp [lookup, update]
  | cons p s ih =>
    obtain ⟨k, e⟩ := p
    rw [update_cons]
    by_cases hkr : k = r
    · by_cases hkq : k = q
      · subst hkr; subst hkq; simp [lookup]
      · have hqk : q ≠ k := fun h => hkq h.symm
        subst hkr
        simp [lookup, hkq, hqk, ih]
    · by_cases hkq : k = q
      · subst hkq; simp [lookup, hkr]
      · simp [lookup, hkr, hkq, ih]

theorem lookup_update_ne (s : RStore) {r q : Key} (f : Entry → Entry) (h : q ≠ r) :
    lookup (update s r f) q = lookup s q := by
  rw [lookup_update, ite_eq_right h]

theorem mem_update {s : RStore} {r : Key} {f : Entry → Entry} {p : Key × Entry}
    (hp : p ∈ update s r f) :
    ∃ q ∈ s, p = (if q.1 = r then (q.1, f q.2) else q) := by
  unfold update at hp
  obtain ⟨q, hq, he⟩ := List.mem_map.mp hp
  exact ⟨q, hq, he.symm⟩

/-- Admission's entries are all `Pending`. -/
theorem lookup_admit {js : List Job} {j : Job} (hj : j ∈ js) :
    ∃ e, lookup (admitStore js) j.key = some e ∧ e.answer = .pending := by
  induction js with
  | nil => simp at hj
  | cons k js ih =>
    show ∃ e, lookup ((k.key, ⟨.pending, none⟩) :: admitStore js) j.key = some e ∧ _
    by_cases hk : k.key = j.key
    · exact ⟨⟨.pending, none⟩, by simp [lookup, hk], rfl⟩
    · have hj' : j ∈ js := by
        cases List.mem_cons.mp hj with
        | inl h => subst h; exact absurd rfl hk
        | inr h => exact h
      obtain ⟨e, he, ha⟩ := ih hj'
      exact ⟨e, by simp [lookup, hk, he], ha⟩

/-- A worker batch mentions only its own job. Stated over `workerStep` (the
worker before #295); it holds of the current worker's batches through
`runW_log_refines`. -/
theorem workerStep_keys {f : Bool} {j : Job} {r : RunResult} {m : Mut}
    (hm : m ∈ (workerStep f j r).2) : m = .write j.key ∨ ∃ t, m = .consume j.key t := by
  unfold workerStep at hm
  cases f with
  | true => simp at hm
  | false =>
    cases r with
    | ok =>
      simp at hm
      cases hm with
      | inl h => exact Or.inl h
      | inr h => exact Or.inr ⟨_, h⟩
    | err e => cases e <;> simp [Err.transient] at hm <;> exact Or.inr ⟨_, hm⟩

/-- **The commit carries its consume.** A worker batch that writes also
consumes the intent as `applied`, in the same batch, so the same flush. Stated
over `workerStep` (the worker before #295); it holds of the current worker's
batches through `runW_log_refines`, which `crash_never_reappliesW` uses. -/
theorem workerStep_atomic {f : Bool} {j : Job} {r : RunResult} {q : Key}
    (hm : Mut.write q ∈ (workerStep f j r).2) : Mut.consume q .applied ∈ (workerStep f j r).2 := by
  unfold workerStep at hm ⊢
  cases f with
  | true => simp at hm
  | false =>
    cases r with
    | ok =>
      simp at hm
      subst hm
      simp
    | err e => cases e <;> simp [Err.transient] at hm

/-! ## `settle_deferred`: deferred, or answered `close_unflushed`

The `deferEntry` and `settleDeferred` lemmas here are about `settle_deferred`
as it is today and serve the current close (`closeW`, `Close.lean`). The
theorems about `close` (`close_settles_all`, `close_final`, `close_answer`)
are over the worker **before #295**, in `History.lean`; their current
counterparts are `shutdown_no_silent_lossW`, `held_at_closeW` and
`closeW_retry_upgradesW`. -/

theorem deferEntry_durable (now : Int) (e : Entry) :
    deferEntry .durable now e =
      if e.answer = .closeUnflushed then { answer := .intentRecorded, settledAt := some now }
      else (settle e .intentRecorded now).1 := rfl

theorem deferEntry_incomplete (now : Int) (e : Entry) :
    deferEntry .incomplete now e = (settle e .closeUnflushed now).1 := rfl

theorem lookup_mem : ∀ (s : RStore) (q : Key) (x : Entry), lookup s q = some x → (q, x) ∈ s := by
  intro s
  induction s with
  | nil => intro q x h; simp [lookup] at h
  | cons p s ih =>
    intro q x h
    obtain ⟨k, y⟩ := p
    by_cases hk : k = q
    · subst hk; simp [lookup] at h; subst h; exact List.mem_cons_self
    · simp [lookup, hk] at h; exact List.mem_cons_of_mem _ (ih q x h)

theorem deferEntry_settled (flush : CloseFlush) (now : Int) (e : Entry) :
    (deferEntry flush now e).answer.isSettled = true := by
  cases flush with
  | durable =>
    unfold deferEntry
    by_cases h : e.answer = .closeUnflushed
    · rw [ite_eq_left h]; rfl
    · rw [ite_eq_right h]; exact Lambo.Receipts.settle_isSettled e _ now rfl
  | incomplete => exact Lambo.Receipts.settle_isSettled e _ now rfl

/-- On a settled answer other than `close_unflushed`, `settle_deferred` does
nothing: a write the drain applied or failed keeps its answer. -/
theorem deferEntry_of_settled (flush : CloseFlush) (now : Int) (e : Entry)
    (hs : e.answer.isSettled = true) (hc : e.answer ≠ .closeUnflushed) :
    deferEntry flush now e = e := by
  cases flush with
  | durable =>
    unfold deferEntry
    rw [ite_eq_right hc]
    exact congrArg Prod.fst (Lambo.Receipts.settle_settled e _ now hs)
  | incomplete => exact congrArg Prod.fst (Lambo.Receipts.settle_settled e _ now hs)

theorem deferEntry_ne_closeUnflushed (now : Int) (e : Entry) :
    (deferEntry .durable now e).answer ≠ .closeUnflushed := by
  show (if e.answer = .closeUnflushed then { answer := .intentRecorded, settledAt := some now }
    else (settle e .intentRecorded now).1 : Entry).answer ≠ _
  by_cases h : e.answer = .closeUnflushed
  · rw [ite_eq_left h]
    show Answer.intentRecorded ≠ Answer.closeUnflushed
    decide
  · rw [ite_eq_right h]
    by_cases hs : e.answer.isSettled = true
    · rw [Lambo.Receipts.settle_settled e _ now hs]; exact h
    · rw [Lambo.Receipts.settle_unsettled e _ now hs]
      show Answer.intentRecorded ≠ Answer.closeUnflushed
      decide

theorem deferEntry_idem (flush : CloseFlush) (now : Int) (e : Entry) :
    deferEntry flush now (deferEntry flush now e) = deferEntry flush now e := by
  cases flush with
  | durable =>
    exact deferEntry_of_settled _ now _ (deferEntry_settled _ now e)
      (deferEntry_ne_closeUnflushed now e)
  | incomplete =>
    show (settle (deferEntry .incomplete now e) .closeUnflushed now).1 = _
    rw [Lambo.Receipts.settle_settled _ _ now (deferEntry_settled .incomplete now e)]

theorem lookup_settleDeferred (flush : CloseFlush) (now : Int) :
    ∀ (ids : List Key) (s : RStore) (q : Key),
      lookup (settleDeferred flush now ids s) q =
        if q ∈ ids then (lookup s q).map (deferEntry flush now) else lookup s q := by
  intro ids
  induction ids with
  | nil => intro s q; simp [settleDeferred]
  | cons r ids ih =>
    intro s q
    show lookup (settleDeferred flush now ids (update s r (deferEntry flush now))) q = _
    rw [ih, lookup_update]
    by_cases hr : q = r
    · subst hr
      by_cases hq : q ∈ ids
      · cases h : lookup s q with
        | none => simp
        | some e => simp [hq, deferEntry_idem]
      · simp [hq]
    · by_cases hq : q ∈ ids
      · simp [hq, hr]
      · simp [hq, hr]

theorem mem_pendingIds {s : RStore} {p : Key × Entry} (hp : p ∈ s)
    (hu : p.2.answer.isSettled = false) : p.1 ∈ pendingIds s := by
  unfold pendingIds
  exact List.mem_map_of_mem (List.mem_filter.mpr ⟨hp, by simp [hu]⟩)

/-- Two jobs with the same receipt id in a list whose ids are
distinct are the same job. -/
theorem eq_of_key {js : List Job} (hnd : (js.map (·.key)).Nodup) {a b : Job} (ha : a ∈ js)
    (hb : b ∈ js) (h : a.key = b.key) : a = b := by
  induction js with
  | nil => simp at ha
  | cons k js ih =>
    rw [List.map_cons, List.nodup_cons] at hnd
    cases List.mem_cons.mp ha with
    | inl ha' =>
      cases List.mem_cons.mp hb with
      | inl hb' => rw [ha', hb']
      | inr hb' =>
        subst ha'
        exact absurd (h ▸ List.mem_map_of_mem hb') hnd.1
    | inr ha' =>
      cases List.mem_cons.mp hb with
      | inl hb' =>
        subst hb'
        exact absurd (h.symm ▸ List.mem_map_of_mem ha') hnd.1
      | inr hb' => exact ih hnd.2 ha' hb'

/-- A retried close upgrades `close_unflushed` to `intent_durable` once its
flush lands: the one settled-to-settled change, made by `settle_deferred`
outside `Entry::settle` (#117). About one entry and `deferEntry`, which is
`settle_deferred` as it is today, so it describes the current close as well;
`closeW_retry_upgradesW` states it over `closeW` and a session of the current
worker. -/
theorem retry_upgrades (now now' : Int) (e : Entry) (h : e.answer.isSettled = false) :
    (deferEntry .durable now' (deferEntry .incomplete now e)).answer = .intentRecorded := by
  have h' : ¬ e.answer.isSettled = true := by rw [h]; decide
  rw [deferEntry_incomplete, Lambo.Receipts.settle_unsettled e _ now h', deferEntry_durable,
    ite_eq_left rfl]

/-! ## The intent table -/

/-- No `PutWriteIntent` in a mutation sequence. -/
def NoPut (ms : List Mut) : Prop := ∀ m ∈ ms, ∀ j, m ≠ .put j

theorem admitLog_flatten : ∀ (js : List Job), (admitLog js).flatten = js.map Mut.put := by
  intro js
  induction js with
  | nil => rfl
  | cons j js ih =>
    show [Mut.put j] ++ (admitLog js).flatten = _
    rw [ih]
    rfl

theorem foldl_put : ∀ (js : List Job) (acc : List Intent),
    (js.map Mut.put).foldl applyMut acc = acc ++ js.map (fun j => ⟨j, none⟩) := by
  intro js
  induction js with
  | nil => intro acc; simp
  | cons j js ih =>
    intro acc
    show (js.map Mut.put).foldl applyMut (acc ++ [⟨j, none⟩]) = _
    rw [ih]
    simp

/-- Admission's puts leave one unconsumed row per admitted job. -/
theorem intentsOf_admit (js : List Job) :
    intentsOf (admitLog js).flatten = js.map (fun j => ⟨j, none⟩) := by
  unfold intentsOf
  rw [admitLog_flatten, foldl_put]
  rfl

theorem applyMut_keeps_owed {acc : List Intent} {j : Job} {m : Mut} (h : ⟨j, none⟩ ∈ acc)
    (hm : ∀ t, m ≠ .consume j.key t) : ⟨j, none⟩ ∈ applyMut acc m := by
  cases m with
  | put k => exact List.mem_append_left _ h
  | write _ => exact h
  | consume r t =>
    have hr : j.key ≠ r := fun e => hm t (e ▸ rfl)
    exact List.mem_map.mpr ⟨⟨j, none⟩, h, by simp [hr]⟩

theorem foldl_keeps_owed {j : Job} : ∀ (ms : List Mut) (acc : List Intent),
    ⟨j, none⟩ ∈ acc → (∀ t, Mut.consume j.key t ∉ ms) → ⟨j, none⟩ ∈ ms.foldl applyMut acc := by
  intro ms
  induction ms with
  | nil => intro acc h _; exact h
  | cons m ms ih =>
    intro acc h hc
    apply ih
    · exact applyMut_keeps_owed h (fun t e => hc t (e ▸ List.mem_cons_self))
    · exact fun t ht => hc t (List.mem_cons_of_mem _ ht)

theorem applyMut_consumed {acc : List Intent} {r : Key} {m : Mut} (hm : ∀ j, m ≠ .put j)
    (h : ∀ i ∈ acc, i.job.key = r → i.outcome ≠ none) :
    ∀ i ∈ applyMut acc m, i.job.key = r → i.outcome ≠ none := by
  cases m with
  | put k => exact absurd rfl (hm k)
  | write _ => exact h
  | consume q t =>
    intro i hi hr
    obtain ⟨i0, h0, rfl⟩ := List.mem_map.mp hi
    by_cases hq : i0.job.key = q
    · simp [hq]
    · simp only [hq, ite_false] at hr ⊢
      exact h i0 h0 hr

theorem applyMut_consume_self (acc : List Intent) (r : Key) (t : Tag) :
    ∀ i ∈ applyMut acc (.consume r t), i.job.key = r → i.outcome ≠ none := by
  intro i hi hr
  obtain ⟨i0, _, rfl⟩ := List.mem_map.mp hi
  by_cases hq : i0.job.key = r
  · simp [hq]
  · simp only [hq, ite_false] at hr

/-- **A consume is never undone.** Once a mutation sequence with no puts
carries a consume of `r`, or every row of `r` is already consumed, every row
of `r` is consumed afterwards. -/
theorem foldl_consumed {r : Key} : ∀ (ms : List Mut) (acc : List Intent), NoPut ms →
    ((∃ t, Mut.consume r t ∈ ms) ∨ ∀ i ∈ acc, i.job.key = r → i.outcome ≠ none) →
    ∀ i ∈ ms.foldl applyMut acc, i.job.key = r → i.outcome ≠ none := by
  intro ms
  induction ms with
  | nil =>
    intro acc _ h
    cases h with
    | inl h => obtain ⟨t, ht⟩ := h; simp at ht
    | inr h => exact h
  | cons m ms ih =>
    intro acc hnp h
    have hnp' : NoPut ms := fun x hx => hnp x (List.mem_cons_of_mem _ hx)
    have hm : ∀ j, m ≠ .put j := hnp m List.mem_cons_self
    apply ih (applyMut acc m) hnp'
    cases h with
    | inl h =>
      obtain ⟨t, ht⟩ := h
      cases List.mem_cons.mp ht with
      | inl he => subst he; exact Or.inr (applyMut_consume_self acc r t)
      | inr ht' => exact Or.inl ⟨t, ht'⟩
    | inr h => exact Or.inr (applyMut_consumed hm h)

theorem foldl_rows {i' : Intent} : ∀ (ms : List Mut) (acc : List Intent), NoPut ms →
    i' ∈ ms.foldl applyMut acc → ∃ i ∈ acc, i'.job = i.job := by
  intro ms
  induction ms with
  | nil => intro acc _ h; exact ⟨i', h, rfl⟩
  | cons m ms ih =>
    intro acc hnp h
    obtain ⟨i1, h1, he⟩ := ih (applyMut acc m) (fun x hx => hnp x (List.mem_cons_of_mem _ hx)) h
    cases m with
    | put k => exact absurd rfl (hnp _ List.mem_cons_self k)
    | write _ => exact ⟨i1, h1, he⟩
    | consume q t =>
      obtain ⟨i0, h0, rfl⟩ := List.mem_map.mp h1
      refine ⟨i0, h0, ?_⟩
      rw [he]
      by_cases hq : i0.job.key = q <;> simp [hq]

/-! ## Load order -/

theorem keyLe_iff (a b : Intent) :
    keyLe a b = true ↔ a.job.ms < b.job.ms ∨ (a.job.ms = b.job.ms ∧ a.job.rid ≤ b.job.rid) := by
  simp [keyLe]

theorem keyLe_total (a b : Intent) : keyLe a b = true ∨ keyLe b a = true := by
  rw [keyLe_iff, keyLe_iff]
  omega

theorem keyLe_trans {a b c : Intent} (h1 : keyLe a b = true) (h2 : keyLe b c = true) :
    keyLe a c = true := by
  rw [keyLe_iff] at h1 h2 ⊢
  omega

theorem mem_insertK {x y : Intent} : ∀ {l : List Intent}, y ∈ insertK x l ↔ y = x ∨ y ∈ l := by
  intro l
  induction l with
  | nil => simp [insertK]
  | cons z zs ih =>
    unfold insertK
    by_cases h : keyLe x z = true
    · simp [h]
    · simp only [h]
      simp only [Bool.false_eq_true, ite_false, List.mem_cons, ih]
      constructor
      · intro h'
        rcases h' with h' | h' | h'
        · exact Or.inr (Or.inl h')
        · exact Or.inl h'
        · exact Or.inr (Or.inr h')
      · intro h'
        rcases h' with h' | h' | h'
        · exact Or.inr (Or.inl h')
        · exact Or.inl h'
        · exact Or.inr (Or.inr h')

theorem mem_loadOrder {y : Intent} : ∀ {l : List Intent}, y ∈ loadOrder l ↔ y ∈ l := by
  intro l
  induction l with
  | nil => simp [loadOrder]
  | cons x xs ih =>
    show y ∈ insertK x (loadOrder xs) ↔ _
    rw [mem_insertK, ih, List.mem_cons]

theorem pairwise_insertK (x : Intent) : ∀ (l : List Intent),
    l.Pairwise (fun a b => keyLe a b = true) →
      (insertK x l).Pairwise (fun a b => keyLe a b = true) := by
  intro l
  induction l with
  | nil => intro _; simp [insertK]
  | cons z zs ih =>
    intro h
    rw [List.pairwise_cons] at h
    unfold insertK
    by_cases hxz : keyLe x z = true
    · simp only [hxz, ite_true]
      refine List.pairwise_cons.mpr ⟨?_, List.pairwise_cons.mpr h⟩
      intro w hw
      cases List.mem_cons.mp hw with
      | inl e => rw [e]; exact hxz
      | inr hw' => exact keyLe_trans hxz (h.1 w hw')
    · simp only [hxz, Bool.false_eq_true, ite_false]
      refine List.pairwise_cons.mpr ⟨?_, ih h.2⟩
      intro w hw
      cases mem_insertK.mp hw with
      | inl e =>
        rw [e]
        cases keyLe_total x z with
        | inl h' => exact absurd h' hxz
        | inr h' => exact h'
      | inr hw' => exact h.1 w hw'

/-- The load is sorted by (`issued_ms`, `lane_seq`). -/
theorem loadOrder_sorted : ∀ (l : List Intent),
    (loadOrder l).Pairwise (fun a b => keyLe a b = true) := by
  intro l
  induction l with
  | nil => exact List.Pairwise.nil
  | cons x xs ih => exact pairwise_insertK x _ ih

/-- **With issue times in sequence order, the load is sequence order.** If,
among the rows, a higher sequence number never has an earlier issue time, the
(`issued_ms`, `lane_seq`) order is `lane_seq` order, the admission order the
`WriteIntent::lane_seq` doc promises. A monotone clock alone does not give the
hypothesis: `next_receipt` takes the sequence number before it reads the clock
and before admission takes its lock, so two concurrent admissions can read
the clock in the other order. It holds for sequential admissions under a
clock that does not step back. `clock_step_reorders` is the other side. -/
theorem load_seq_order (l : List Intent)
    (hmono : ∀ a ∈ l, ∀ b ∈ l, a.job.rid < b.job.rid → a.job.ms ≤ b.job.ms) :
    (loadOrder l).Pairwise (fun a b => a.job.rid ≤ b.job.rid) := by
  apply List.Pairwise.imp_of_mem _ (loadOrder_sorted l)
  intro a b ha hb hab
  rw [keyLe_iff] at hab
  have hm := hmono b (mem_loadOrder.mp hb) a (mem_loadOrder.mp ha)
  omega

/-- `load_seq_order` for a backlog of several processes: the hypothesis and
the conclusion compare only rows of the same epoch. Across epochs the load
order is issue-time order; sequence numbers of different processes are not
comparable. -/
theorem load_epoch_seq_order (l : List Intent)
    (hmono : ∀ a ∈ l, ∀ b ∈ l, a.job.epoch = b.job.epoch → a.job.rid < b.job.rid →
      a.job.ms ≤ b.job.ms) :
    (loadOrder l).Pairwise (fun a b => a.job.epoch = b.job.epoch → a.job.rid ≤ b.job.rid) := by
  apply List.Pairwise.imp_of_mem _ (loadOrder_sorted l)
  intro a b ha hb hab he
  rw [keyLe_iff] at hab
  have hm := hmono b (mem_loadOrder.mp hb) a (mem_loadOrder.mp ha) he.symm
  omega

/-! ## Admission's batches

Admission's batches hold only puts. Used by the crash theorems:
`crash_never_reappliesW` (`Close.lean`) and, before #295,
`crash_never_reapplies` (`History.lean`). -/

theorem mem_admit_flatten {all : List Job} {n : Nat} {m : Mut}
    (hm : m ∈ ((admitLog all).take n).flatten) : ∃ j, m = .put j := by
  obtain ⟨b, hb, hmb⟩ := List.mem_flatten.mp hm
  obtain ⟨j, _, rfl⟩ := List.mem_map.mp ((List.take_sublist n _).subset hb)
  exact ⟨j, List.mem_singleton.mp hmb⟩

end Lambo.Drain
