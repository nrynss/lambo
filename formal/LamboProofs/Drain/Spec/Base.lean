/-
# Write-queue drain and durable-intent replay: the base model (specification)

Model of how the asynchronous write queue's admitted writes end: drained by
the lane workers, set aside and settled at `close()`, or replayed from their
durable intents by a later serve.

The model is in two layers. `Drain/Spec/` is the specification: the
definitions, which are the algorithm as the Rust runs it, and each result
stated as a proposition `Spec.<name>`, with no proof. `Drain/Proofs/` proves
them: `theorem <name> : Spec.<name>`, beside the lemmas the proofs need,
which state nothing about the Rust that a `Spec` proposition does not. Read
this layer to learn what the model says; a proposition here holds because
the theorem of its name in `Drain/Proofs/` is checked by `lake build`, and
`scripts/formal/audit-theorems.py --check` fails on one that has no such
theorem. Each layer has one file per subsystem, with the same names, and
everything is in the namespace `Lambo.Drain`, so a name does not say which
file holds it. Below and in the other files, `Base.lean`, `Replay.lean`,
`Worker.lean` and `Close.lean` name the subsystem: its `Spec/` file for a
definition or a statement, its `Proofs/` file for a proof.

* `Base.lean` (this file): writes, run results and the mutation log;
  the lane worker's batches (`workerStep`), the receipt store and the lane
  drain (`drain`); `settle_deferred`'s set-aside (`CloseFlush`, `deferEntry`,
  `settleDeferred`) and the `Drop` path (`abortAllSync`); durable intents,
  the intent table and the load order; the replay's step, its arms,
  streaks and constants (`replayStep`, `sickThreshold`,
  `lookupTimeoutLimit`) and the restart map's seed (`seed`).
* `Replay.lean`: the replay fixes (#294, #295), the order invariant
  across passes, the in-session hold (`workH`) and the replay's retry pass.
* `Worker.lean`: the current lane worker (`stepW`, `runW`), with
  admission, the caps, retries and the fence.
* `Close.lean`: the close, crash and shutdown over the current worker
  (`closeW`), dead-lettered batches at close (#345), worker panics and the
  dead-lettered set, and where the lookup-timeout streak resets (#346).

Two files are in neither layer and are not specification. `Drain/Examples.lean`
holds worked examples and the findings about the Rust before a fix.
`Drain/History.lean` holds the lane worker loop before #295 (`work`, `close`)
and the replay loop before #294 (`replay`'s theorems), kept as the evidence
for the findings they show. It describes no current Rust: it is not checked
against the Rust for drift, and the docs do not cite it. Both state their
theorems in full, with the proofs.

Rust items modelled across the files:

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
operation for the graph and mutation log; an aborted worker is a job that
was not run. The replay steps model proposed answers. A successful replay's
live receipt waits for the store to confirm the span. A crash leaves a
prefix of the batch log durable, except on a deterministic rejection, where
`Flush.replayRecoveryLog` describes the confirmed prefix and failed consume.

Rows and consumes are keyed by the receipt id, modelled as its (epoch,
sequence) pair (`Job.key`), as the adapters key them by the full receipt
string: a backlog may hold several crashed processes' intents, and every
process's sequence starts at 1.

## Results

* `drain_fifo`, `drain_extends_taken`: a lane drained to completion has taken
  exactly the submitted jobs in submission order; what it had taken before is
  a prefix (agrees with `Receipts.lane_fifo`).
* `settleDeferred_all_settled`: `settle_deferred` leaves every set-aside
  entry settled. `drop_settles_nothing`: the `Drop` path (`abort_all_sync`),
  the documented exception, settles nothing; it is definitional
  (`abortAllSync` is the identity on receipts) and is there to name the gap.
* `replayStep_inputFact`, `replayStep_decode`, `replayStep_cycle`: a refusal
  that is a fact about the intent's input settles `failed`;
  `unavailable_skips`, `unavailable_stops`: the transient arm. The current
  step (`stepQ` over `stepFixed` over `stepInput`) hands these answers to
  `replayStep`. It does not hand it a graph-dependent input refusal, which
  `replayStep` stops on (`replayStep_sessionWide`, `History.lean`); the
  current rule for the other non-transient errors is `stepInput_sessionWide`
  and `fixed_sessionWide_stops` (`Replay.lean`).
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

Boundary: not modelled are concurrency and the cancellation-safety custody of handles
(`WorkerCustody`, `ReplayCustody`); `stop_replay` running before the quiesce;
admission and its caps (`Receipts.lean`); receipt retention, eviction, the
undelivered queue, delivery destinations and the live entry a replay inserts;
the stale consumed-row skip at seeding; the adapters' lazy purge of consumed
rows older than the retention window (sqlite `write_rows.rs`, inside the
consume; it deletes only consumed rows, so it never touches a pending one);
counters, the ledger and logging; the
lease fence at the flush (a fenced flush is refused at the store, so a
consume appended after a lost lease never lands); `calibration.rs` (drain
does not depend on it). Why a run fails is not modelled: a run's result is an
input per write.
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

/-- A worker batch mentions only its own job. Stated over `workerStep` (the
worker before #295); it holds of the current worker's batches through
`runW_log_refines`. -/
abbrev Spec.workerStep_keys : Prop :=
  ∀ {f : Bool} {j : Job} {r : RunResult} {m : Mut},
    m ∈ (workerStep f j r).2 →
    m = .write j.key ∨ ∃ t, m = .consume j.key t

/-- **The commit carries its consume.** A worker batch that writes also
consumes the intent as `applied`, in the same batch, so the same flush. Stated
over `workerStep` (the worker before #295); it holds of the current worker's
batches through `runW_log_refines`, which `crash_never_reappliesW` uses. -/
abbrev Spec.workerStep_atomic : Prop :=
  ∀ {f : Bool} {j : Job} {r : RunResult} {q : Key},
    Mut.write q ∈ (workerStep f j r).2 →
    Mut.consume q .applied ∈ (workerStep f j r).2

/-! ## `settle_deferred`: deferred, or answered `close_unflushed`

The `deferEntry` and `settleDeferred` lemmas here are about `settle_deferred`
as it is today and serve the current close (`closeW`, `Close.lean`). The
theorems about `close` (`close_settles_all`, `close_final`, `close_answer`)
are over the worker **before #295**, in `History.lean`; their current
counterparts are `shutdown_no_silent_lossW`, `held_at_closeW` and
`closeW_retry_upgradesW`. -/

/-- A retried close upgrades `close_unflushed` to `intent_durable` once its
flush lands: the one settled-to-settled change, made by `settle_deferred`
outside `Entry::settle` (#117). About one entry and `deferEntry`, which is
`settle_deferred` as it is today, so it describes the current close as well;
`closeW_retry_upgradesW` states it over `closeW` and a session of the current
worker. -/
abbrev Spec.retry_upgrades : Prop :=
  ∀ (now now' : Int) (e : Entry),
    e.answer.isSettled = false →
    (deferEntry .durable now' (deferEntry .incomplete now e)).answer = .intentRecorded

/-! ## The intent table -/

/-- No `PutWriteIntent` in a mutation sequence. -/
def NoPut (ms : List Mut) : Prop := ∀ m ∈ ms, ∀ j, m ≠ .put j

/-- **A consume is never undone.** Once a mutation sequence with no puts
carries a consume of `r`, or every row of `r` is already consumed, every row
of `r` is consumed afterwards. -/
abbrev Spec.foldl_consumed : Prop :=
  ∀ {r : Key},
    ∀ (ms : List Mut) (acc : List Intent), NoPut ms →
      ((∃ t, Mut.consume r t ∈ ms) ∨ ∀ i ∈ acc, i.job.key = r → i.outcome ≠ none) →
      ∀ i ∈ ms.foldl applyMut acc, i.job.key = r → i.outcome ≠ none

/-! ## Load order -/

/-- **With issue times in sequence order, the load is sequence order.** If,
among the rows, a higher sequence number never has an earlier issue time, the
(`issued_ms`, `lane_seq`) order is `lane_seq` order, the admission order the
`WriteIntent::lane_seq` doc promises. A monotone clock alone does not give the
hypothesis: `next_receipt` takes the sequence number before it reads the clock
and before admission takes its lock, so two concurrent admissions can read
the clock in the other order. It holds for sequential admissions under a
clock that does not step back. `clock_step_reorders` is the other side. -/
abbrev Spec.load_seq_order : Prop :=
  ∀ (l : List Intent),
    (∀ a ∈ l, ∀ b ∈ l, a.job.rid < b.job.rid → a.job.ms ≤ b.job.ms) →
    (loadOrder l).Pairwise (fun a b => a.job.rid ≤ b.job.rid)

/-- `load_seq_order` for a backlog of several processes: the hypothesis and
the conclusion compare only rows of the same epoch. Across epochs the load
order is issue-time order; sequence numbers of different processes are not
comparable. -/
abbrev Spec.load_epoch_seq_order : Prop :=
  ∀ (l : List Intent),
    (∀ a ∈ l, ∀ b ∈ l, a.job.epoch = b.job.epoch → a.job.rid < b.job.rid →
      a.job.ms ≤ b.job.ms) →
    (loadOrder l).Pairwise (fun a b => a.job.epoch = b.job.epoch → a.job.rid ≤ b.job.rid)

/-! ## Draining a lane to completion

`drainLane` pops a lane until its queue is empty. What it takes is the lane's order, which the lane worker, before #295 and since, takes its writes in (`Receipts.lane_fifo`). -/

/-- Drain a lane to completion: pop until the queue is empty. -/
def drainLane {α : Type} : Nat → Lane α → Lane α
  | 0, l => l
  | n + 1, l => drainLane n l.pop

def drain {α : Type} (l : Lane α) : Lane α := drainLane l.queue.length l

/-- **Drain applies each lane's writes in submission order, all of them.**
However admissions and worker steps interleaved (`Receipts.run`), a lane
drained to completion has taken exactly the submitted jobs, in submission
order: the completion of `Receipts.lane_fifo`, whose prefix is what the
worker had taken when the drain began. -/
abbrev Spec.drain_fifo : Prop :=
  ∀ (ops : List (Lambo.Receipts.Op Job)),
    (drain (Lambo.Receipts.run ⟨[], []⟩ ops)).queue = [] ∧
      (drain (Lambo.Receipts.run ⟨[], []⟩ ops)).taken = Lambo.Receipts.submitted ops

/-! ## `settle_deferred` settles every set-aside entry -/

/-- `settle_deferred` leaves no receipt pending: when every receipt outside the
set-aside ids is already settled, every receipt is settled once it has run
over those ids, whichever way the final flush ended. -/
abbrev Spec.settleDeferred_all_settled : Prop :=
  ∀ (flush : CloseFlush) (now : Int),
    ∀ (ids : List Key) (s : RStore), (∀ p ∈ s, p.1 ∉ ids → p.2.answer.isSettled = true) →
      ∀ p ∈ settleDeferred flush now ids s, p.2.answer.isSettled = true

/-! ## The restart map's seed (`spawn_replay`)

Before a replay runs, `spawn_replay` seeds the restart map with one answer per stored row: `pending_replay` for an unconsumed one, `failed` or `applied_after_restart` for a consumed one, by its tag. -/

/-- The seeded cross-restart answer of one row (the stale-row skip is not
modelled). -/
def seedAnswer (i : Intent) : Answer :=
  match i.outcome with
  | none => .pendingReplay
  | some .failed => .failed
  | some _ => .appliedAfterRestart

def seed (intents : List Intent) : List (Key × Answer) :=
  intents.map (fun i => (i.job.key, seedAnswer i))

/-- The restart map after a list of `insert`s: the last insert wins. -/
def lookupLast (m : List (Key × Answer)) (r : Key) : Option Answer :=
  (m.reverse.find? (fun p => p.1 = r)).map (·.2)

/-! ## The replay step's arms

`replayStep` is the match on a run's result in `spawn_replay`. The current step (`stepQ` over `stepFixed` over `stepInput`, `Replay.lean`) overrides it for an unknown receipt format, an unparseable id and a graph-dependent input refusal and hands every other answer to it, so these arms are the current replay's. -/

/-- A refusal that is a fact about the intent's own input settles `failed`. -/
abbrev Spec.replayStep_inputFact : Prop :=
  ∀ (img : Bool) (e : Err) (s : Streaks),
    e.inputFact = true →
    (replayStep img (.run (.err e)) s).1 = .failed

/-- A transient embed answer below the threshold is a `skip`: the intent is
left unconsumed (the loop since #295, `loopD`, then defers its agent). -/
abbrev Spec.unavailable_skips : Prop :=
  ∀ (img : Bool) (s : Streaks),
    s.transient + 1 < sickThreshold →
    replayStep img (.run (.err .unavailable)) s =
      (.skip, { s with transient := s.transient + 1 })

/-- The third transient in a row with nothing answered between stops the
replay as `embedder` (`EMBEDDER_SICK_THRESHOLD`). -/
abbrev Spec.unavailable_stops : Prop :=
  ∀ (img : Bool) (s : Streaks),
    sickThreshold ≤ s.transient + 1 →
    (replayStep img (.run (.err .unavailable)) s).1 = .stop .embedder

end Lambo.Drain
