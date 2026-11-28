/-
# Write-queue drain: shared definitions

How the write queue's admitted writes end: drained by the lane workers, set
aside and settled at `close()`, or replayed from their durable intents by a
later serve. This file holds what the other `Drain/Spec/` files share: jobs,
run results and the mutation log, the receipt store, `settle_deferred`'s
set-aside, durable intents and their load order, and the replay's step.

Rust: `WriteCtx::run`, `ConsumeStamp` and the batches of
`WritePipeline::spawn_worker` (`src/writeq/execution.rs`); `settle_one`,
`ReceiptId`, `next_receipt` (`src/writeq/receipts.rs`); `ReplayBlockReason`
(`src/writeq/counters.rs`); `WriteIntent`, `WriteIntentOutcome`,
`is_cycle_refusal`, `is_input_refusal`, `is_lookup_timeout`
(`src/types/mod.rs`); `CloseFlush`, `abort_workers`, `settle_deferred`,
`abort_all_sync` (`src/writeq/drain.rs`) and `Memory`'s `Drop`; the step and
constants of `spawn_replay` (`src/writeq/replay.rs`); `record_write_intent`,
`consume_write_intent` (`src/graph/graph/mutation_log.rs`); the adapters'
load order (`src/store/{sqlite,pg}/session_load.rs`).

Results:
* `replayStep_inputFact`: a refusal that is a fact about the input settles `failed`.
* `unavailable_skips`, `unavailable_stops`: a transient answer skips its
  intent, and the third in a row stops the replay.

Boundary: concurrency is out of scope. Each worker step (run, commit with its
consume, settle) and each replay step is atomic, and an aborted worker is a
job not run; a replayed write's receipt waits for the store to confirm its
span. A crash leaves a prefix of the batch log durable, except on a
deterministic rejection (`Flush.replayRecoveryLog`). Why a run fails is not
modelled: its result is an input per write, and the drain and probe budgets
are abstract. The Rust's put is an upsert on (`session_id`, `receipt`) where
the model's appends; results over arbitrary rows cover the Rust's case. The
`Drop` path (`abort_all_sync`) aborts the workers without joining them and
settles nothing: a receipt pending when a `Memory` is dropped without
`close()` stays pending. Not modelled: the custody of handles
(`WorkerCustody`, `ReplayCustody`); `stop_replay` running before the quiesce;
receipt retention, eviction, the undelivered queue, delivery destinations and
the live entry a replay inserts; the stale consumed-row skip at seeding; the
adapters' purge of old consumed rows (never a pending one); counters, the
ledger and logging; the lease fence at the flush (a fenced flush is refused
at the store); `calibration.rs`.
-/

import LamboProofs.Receipts

namespace Lambo.Drain

open Lambo.Receipts (Answer Entry settle Lane)

/-! ## Writes, run results, the mutation log -/

/-- A queued write (`Job`, `admission.rs`) as drain and replay read it: its
receipt's sequence number (`ReceiptId::seq`, `WriteIntent::lane_seq`), agent,
issue time in ms, whether it is an image derive, and epoch (`ReceiptId::epoch`). -/
structure Job where
  rid : Nat
  agent : Nat
  ms : Int
  image : Bool
  epoch : Nat
  deriving DecidableEq, Repr

/-- The receipt id `lwr1.{epoch}.{ms}.{seq}`, by which the adapters key intent
rows; the sequence alone is not a key, as every process's starts at 1. -/
abbrev Key := Nat × Int × Nat

def Job.key (j : Job) : Key := (j.epoch, j.ms, j.rid)

/-- The classes of `LamboError` a run fails with: `cycle` and `inputRefusal`
are `Invariant`s for which `is_cycle_refusal` or `is_input_refusal` holds,
`lookupTimeout` is `is_lookup_timeout`, `store` and `unavailable` the rest. -/
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

/-- A write-behind log entry: `PutWriteIntent`, an applied write's graph
mutations (one `write` for all), `ConsumeWriteIntent`. -/
inductive Mut where
  | put (j : Job)
  | write (k : Key)
  | consume (k : Key) (t : Tag)
  deriving DecidableEq, Repr

/-- What one hold of the graph write lock appends: a flush carries whole
batches, each one store transaction (`Graph::consume_write_intent`). -/
abbrev Batch := List Mut

/-! ## The receipt store and admission -/

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
missing entry is skipped. -/
def settleOne (s : RStore) (r : Key) (a : Answer) (now : Int) : RStore :=
  update s r (fun e => (settle e a now).1)

/-- What admission (`WritePipeline`, `admission.rs`) leaves for the jobs it accepted: a
`Pending` entry each, and a `PutWriteIntent` each, on its own hold. -/
def admitStore (js : List Job) : RStore := js.map (fun j => (j.key, ⟨.pending, none⟩))

def admitLog (js : List Job) : List Batch := js.map (fun j => [.put j])

/-! ## `settle_deferred`'s set-aside (`drain.rs`) -/

/-- `CloseFlush`: how `close()`'s final flush ended. -/
inductive CloseFlush where
  | durable
  | incomplete
  deriving DecidableEq, Repr

/-- `settle_deferred` on one entry: `Durable` answers `intent_durable`, also over
a `close_unflushed` an earlier attempt left (bypassing `Entry::settle`);
`Incomplete` answers `close_unflushed`. -/
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

/-- One mutation on the `write_intents` table: a put inserts an unconsumed row,
a consume updates every row of its receipt (`UPDATE ... WHERE receipt = ?`). -/
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

/-! ## The replay's step (`spawn_replay`, `replay.rs`) -/

/-- What the replay meets at one intent: a lost lease or seal (`halt`), an id
that does not parse (`badId`) or is of another version (`unknownFormat`,
`lwr<n>.`, n ≠ 1), a payload that does not decode, or `WriteCtx::run`'s result. -/
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
    -- The "anything else" arm: the current step (`stepInput`) answers it first.
    | .inputRefusal => (.stop .other, s)
    -- An image derive this configuration cannot apply.
    | .config => (.stop (if image then .imageConfig else .other), s)
    -- "Anything else (store/lease/config)".
    | .store => (.stop .other, s)
    | .conflict => (.stop .other, s)
    | .softLock => (.stop .other, s)
    | .other => (.stop .other, s)

/-- What a replay did: restart-map inserts, batches appended, jobs applied and
failed, and the block reason if it stopped before the end of the backlog. -/
structure ROut where
  answers : List (Key × Answer)
  log : List Batch
  applied : List Job
  failed : List Job
  block : Option Block
  deriving DecidableEq, Repr

def ROut.empty : ROut := ⟨[], [], [], [], none⟩

/-- The unconsumed rows: `intents.filter(|i| i.outcome.is_none())`. -/
def pendingOf (intents : List Intent) : List Intent := intents.filter (fun i => i.outcome.isNone)

/-- The rows after the replay's own consumes have landed. -/
def applyLog (intents : List Intent) (log : List Batch) : List Intent :=
  log.flatten.foldl applyMut intents

/-! ## Results -/

/-- A refusal that is a fact about the intent's own input settles `failed`. -/
abbrev Spec.replayStep_inputFact : Prop :=
  ∀ (img : Bool) (e : Err) (s : Streaks),
    e.inputFact = true →
    (replayStep img (.run (.err e)) s).1 = .failed

/-- A transient embed answer below the threshold is a `skip`: the intent is
left unconsumed (and `loopD` defers its agent). -/
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
