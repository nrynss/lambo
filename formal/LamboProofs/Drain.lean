/-
# Write-queue drain and durable-intent replay

Model of how the asynchronous write queue's admitted writes end: drained by
the lane workers, set aside and settled at `close()`, or replayed from their
durable intents by a later serve.

Rust items modelled (paths as at `16e9aafa`):

* `src/writeq/execution.rs`: the lane worker `WritePipeline::spawn_worker`
  (pop, the lease-lost fence, `WriteCtx::run`, which failures consume the
  intent, `settle_one`), and `WriteCtx::run` as far as its `ConsumeStamp`
  goes: the consume is appended under the same lock hold as the commit.
* `src/writeq/drain.rs`: `quiesce`, `abort_workers` (every unsettled receipt
  set aside), `settle_deferred` with `CloseFlush` (including the
  `close_unflushed → intent_durable` upgrade on a retried close), and
  `abort_all_sync`.
* `src/writeq/replay.rs`: `spawn_replay` (seeding the restart map, the
  unconsumed filter, the liveness gate, the loop's match arm by arm, both
  streaks, `EMBEDDER_SICK_THRESHOLD`, `REPLAY_LOOKUP_TIMEOUT_LIMIT`).
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

* `drain_fifo`, `drain_extends_taken`: a lane drained to completion has taken
  exactly the submitted jobs in submission order; what it had taken before is
  a prefix (agrees with `Receipts.lane_fifo`).
* `drain_settles_each`, `work_log`, `drained_final`: every admitted write is
  settled once with the worker's answer (`applied` or `failed`), one batch per
  job, and no later settle changes it.
* `workerStep_atomic`, `crash_never_reapplies`: a write in the durable log has
  its consume beside it, so no replay after a crash applies it again
  (`Graph::consume_write_intent`: "the double-apply this design excludes").
  Proved for logs shaped "all puts, then worker batches": the Rust relies on
  the admission lock order (graph write, then lanes) to put each job's put
  before its consume, and the model takes that shape as given. A crash in the
  middle of a replay is not proved (the replay's own batches have the same
  write-with-consume shape, `loop_facts`).
  Like `shutdown_no_silent_loss`, it is about one process's log over an
  empty intent table: an earlier epoch's backlog is not in the statement.
* `close_settles_all`, `close_answer`, `shutdown_no_silent_loss`: after
  `close()` every receipt is settled, and each admitted write is drained,
  `intent_durable` and owed by the next replay, or `close_unflushed`
  ("deferred, not lost", `quiesce`). `retry_upgrades`: the one
  settled-to-settled change. `drop_settles_nothing`: the `Drop` path, the
  documented exception; it is definitional (`abortAllSync` is the identity on
  receipts) and is there to name the gap, not to prove anything about it.
* `replay_applied_pending`, `replay_drained`, `replay_idempotent`,
  `replay_complete_idem`: replay applies only unconsumed rows; replaying a
  fully drained queue changes nothing; a replayed write is never applied by a
  later replay.
* `replay_order`, `load_seq_order`, `replay_seq_order`,
  `load_epoch_seq_order`, `replay_epoch_seq_order`: one serve applies in load
  order, which within one process is sequence (admission) order as long as no
  later admission carries an earlier issue time. Ties across epochs (equal
  `issued_ms` and `lane_seq`) are unspecified in SQL, while `loadOrder` keeps
  input order for them; no theorem depends on how they break.
* `inputFact_goes_on`, `replayStep_cycle`, `replayStep_decode`: a refusal
  that is a fact about the intent's input (`Embed`, `ImageIdTaken`,
  `Reference`, a cycle refusal, an undecodable payload) settles `failed` and
  the replay goes on (`StoreError::is_cycle_refusal` doc; test
  `a_cycle_refused_at_replay_settles_failed_and_the_replay_goes_on`).
  `sessionWide_stops`: any other non-transient error stops it, the intent and
  the rest left unconsumed. `unavailable_skips`, `unavailable_stops`: the
  transient arm. These describe the replay before #294
  (`replayStep_inputRefusal_stops`).
* The fix for #294 (`stepInput`, run through `loopWith`, the loop with its
  step as a parameter; `loopWith_replayStep` shows the generalisation is the
  old loop): `fixed_input_goes_on`, `stepInput_refusesInput`: the replay
  settles every input refusal `failed` and goes on, now including the
  graph-dependent refusals the Rust marks with `StoreError::input_refusal`
  (`Err.inputRefusal`); `fixed_sessionWide_stops`, `stepInput_sessionWide`:
  it still stops on every other non-transient error; `stepInput_agrees`:
  nothing else changes; `fixed_agrees_with_worker`: replay and the in-session
  worker now both consume an input refusal `failed`.
* `outage_failed_but_owed`: an in-session `EmbedUnavailable` answers
  `failed` and leaves the write owed (test
  `a_write_reached_during_an_in_session_embedder_outage_is_not_consumed`).

What the model shows about the Rust (theorems in the last section):
`transient_skip_reorders` and `outage_then_replay_reorders` (a transient
embed answer breaks the per-agent order across serves),
`clock_step_reorders` (the load order trusts the wall clock),
`outage_failed_then_applied` (a doc fix: `ReceiptAnswer::Failed` says the
write did not happen, and after an outage it later does),
`store_error_blocks_backlog` (a graph-dependent refusal raised as a plain
`Invariant` stops the replay on every serve; fixed by #294, see above),
`bad_id_never_settled`.

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

/-! ## The lane worker (`spawn_worker`, `execution.rs`) -/

/-- One job through the lane worker: its receipt answer and the batch it
appends. `fenced` is the lease-lost fence, checked before the run.

* lease lost: `failed`, nothing written, the intent deliberately not consumed;
* `Ok`: `applied`; the write and `ConsumeWriteIntent(applied)` under one lock
  hold (`WriteCtx::run`, "Same lock hold as the commit");
* `EmbedUnavailable` (outage or lookup timeout): `failed`, intent left
  unconsumed for the next serve;
* any other error: `failed`, intent consumed `failed` on its own hold. -/
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

/-- The worker loop over the jobs it takes, in the order it takes them: run,
settle, next. `fence` and `out` are the lease state and the run result at that
job (an outcome function: the model does not say why a run fails). -/
def work (fence : Job → Bool) (out : Job → RunResult) (now : Int) :
    RStore → List Job → RStore × List Batch
  | s, [] => (s, [])
  | s, j :: js =>
    let r := workerStep (fence j) j (out j)
    let rest := work fence out now (settleOne s j.key r.1 now) js
    (rest.1, r.2 :: rest.2)

/-- Drain a lane to completion: pop until the queue is empty. -/
def drainLane {α : Type} : Nat → Lane α → Lane α
  | 0, l => l
  | n + 1, l => drainLane n l.pop

def drain {α : Type} (l : Lane α) : Lane α := drainLane l.queue.length l

/-! ## Close (`drain.rs`) -/

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

/-- `quiesce` then the final flush then `settle_deferred`. `done` is what the
workers finished before the drain budget ran out, in the order they finished
them; everything else is either still queued (an orphan) or was in flight and
aborted at an `.await` before its commit, which (no `.await` between a commit
and its settle) means not written. The store's every unsettled receipt is then
set aside and settled by how the flush ended. -/
def close (done : List Job) (fence : Job → Bool) (out : Job → RunResult) (now : Int)
    (flush : CloseFlush) (s : RStore) : RStore × List Batch :=
  let w := work fence out now s done
  (settleDeferred flush now (pendingIds w.1) w.1, w.2)

/-- `abort_all_sync`, the `Drop` path: aborts without joining and settles
nothing. -/
def abortAllSync (s : RStore) : RStore := s

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
receipt id does not parse (`continue`). `decodeRefused`: the stored payload
does not decode into a job (#129). `run r`: `WriteCtx::run` returned `r`. -/
inductive RR where
  | halt
  | badId
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

/-- `EMBEDDER_SICK_THRESHOLD`. -/
def sickThreshold : Nat := 3

/-- `REPLAY_LOOKUP_TIMEOUT_LIMIT = EMBEDDER_SICK_THRESHOLD`. -/
def lookupTimeoutLimit : Nat := sickThreshold

/-- The match on the run's result in `spawn_replay`, arm by arm in source
order. -/
def replayStep (image : Bool) (rr : RR) (s : Streaks) : Step × Streaks :=
  match rr with
  | .halt => (.stop .unset, s)
  | .badId => (.skip, s)
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

/-! ## The worker -/

/-- The worker only ever settles with a settled answer. -/
theorem workerStep_settled (f : Bool) (j : Job) (r : RunResult) :
    (workerStep f j r).1.isSettled = true := by
  unfold workerStep
  cases f with
  | true => rfl
  | false =>
    cases r with
    | ok => rfl
    | err e => cases e <;> rfl

/-- The worker answers `applied` or `failed`, nothing else. -/
theorem workerStep_answer (f : Bool) (j : Job) (r : RunResult) :
    (workerStep f j r).1 = .applied ∨ (workerStep f j r).1 = .failed := by
  unfold workerStep
  cases f with
  | true => exact Or.inr rfl
  | false =>
    cases r with
    | ok => exact Or.inl rfl
    | err e => cases e <;> exact Or.inr rfl

/-- A worker batch mentions only its own job. -/
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
consumes the intent as `applied`, in the same batch, so the same flush. -/
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

/-- One batch per job, in the order the worker took them. -/
theorem work_log (fence : Job → Bool) (out : Job → RunResult) (now : Int) :
    ∀ (js : List Job) (s : RStore),
      (work fence out now s js).2 = js.map (fun j => (workerStep (fence j) j (out j)).2) := by
  intro js
  induction js with
  | nil => intro s; rfl
  | cons j js ih =>
    intro s
    show (workerStep (fence j) j (out j)).2 :: (work fence out now _ js).2 = _
    rw [ih]
    rfl

/-- The worker leaves the entries of other jobs alone. -/
theorem work_lookup_other (fence : Job → Bool) (out : Job → RunResult) (now : Int) :
    ∀ (js : List Job) (s : RStore) (q : Key), q ∉ js.map (·.key) →
      lookup (work fence out now s js).1 q = lookup s q := by
  intro js
  induction js with
  | nil => intro s q _; rfl
  | cons j js ih =>
    intro s q hq
    have hne : q ≠ j.key := fun h => hq (by simp [h])
    have hq' : q ∉ js.map (·.key) := fun h => hq (List.mem_cons_of_mem _ h)
    show lookup (work fence out now (settleOne s j.key _ now) js).1 q = _
    rw [ih _ q hq']
    exact lookup_update_ne s _ hne

/-- Each job's receipt ends with the worker's answer for it, settled once. -/
theorem work_lookup (fence : Job → Bool) (out : Job → RunResult) (now : Int) :
    ∀ (js : List Job) (s : RStore) (j : Job), (js.map (·.key)).Nodup → j ∈ js →
      (∃ e, lookup s j.key = some e ∧ e.answer.isSettled = false) →
      (lookup (work fence out now s js).1 j.key).map (·.answer) =
        some (workerStep (fence j) j (out j)).1 := by
  intro js
  induction js with
  | nil => intro s j _ hj; simp at hj
  | cons k js ih =>
    intro s j hnd hj he
    rw [List.map_cons, List.nodup_cons] at hnd
    show (lookup (work fence out now (settleOne s k.key _ now) js).1 j.key).map (·.answer) = _
    cases List.mem_cons.mp hj with
    | inl h =>
      subst h
      rw [work_lookup_other fence out now js _ j.key hnd.1]
      obtain ⟨e, hl, hu⟩ := he
      unfold settleOne
      rw [lookup_update, ite_eq_left rfl, hl]
      have hs : ¬ e.answer.isSettled = true := by rw [hu]; decide
      simp [Lambo.Receipts.settle_unsettled e _ now hs]
    | inr h =>
      have hne : j.key ≠ k.key := fun hr => hnd.1 (hr ▸ List.mem_map_of_mem h)
      apply ih _ j hnd.2 h
      unfold settleOne
      rw [lookup_update_ne s _ hne]
      exact he

/-! ## Draining a lane to completion -/

theorem drainLane_spec {α : Type} :
    ∀ (n : Nat) (l : Lane α), l.queue.length = n →
      (drainLane n l).queue = [] ∧ (drainLane n l).taken = l.history := by
  intro n
  induction n with
  | zero =>
    intro l h
    refine ⟨List.eq_nil_of_length_eq_zero h, ?_⟩
    show l.taken = l.taken ++ l.queue
    rw [List.eq_nil_of_length_eq_zero h, List.append_nil]
  | succ n ih =>
    intro l h
    show (drainLane n l.pop).queue = [] ∧ (drainLane n l.pop).taken = l.history
    obtain ⟨queue, taken⟩ := l
    cases queue with
    | nil => simp at h
    | cons x q =>
      have hl : (Lane.pop ⟨x :: q, taken⟩).queue.length = n := by
        simp [Lane.pop] at h ⊢
        exact h
      obtain ⟨h1, h2⟩ := ih _ hl
      exact ⟨h1, h2.trans (Lane.pop_history _)⟩

/-- **Drain applies each lane's writes in submission order, all of them.**
However admissions and worker steps interleaved (`Receipts.run`), a lane
drained to completion has taken exactly the submitted jobs, in submission
order: the completion of `Receipts.lane_fifo`, whose prefix is what the
worker had taken when the drain began. -/
theorem drain_fifo (ops : List (Lambo.Receipts.Op Job)) :
    (drain (Lambo.Receipts.run ⟨[], []⟩ ops)).queue = [] ∧
      (drain (Lambo.Receipts.run ⟨[], []⟩ ops)).taken = Lambo.Receipts.submitted ops := by
  obtain ⟨h1, h2⟩ := drainLane_spec _ (Lambo.Receipts.run ⟨[], []⟩ ops) rfl
  refine ⟨h1, ?_⟩
  unfold drain
  rw [h2, Lambo.Receipts.run_history]
  rfl

/-- What the worker had taken when the drain began is a prefix of what the
drain takes (`Receipts.lane_fifo` and `drain_fifo` agree). -/
theorem drain_extends_taken {α : Type} (l : Lane α) : ∃ rest, l.taken ++ rest = (drain l).taken := by
  refine ⟨l.queue, ?_⟩
  unfold drain
  rw [(drainLane_spec _ l rfl).2]
  rfl

/-- **Every admitted write is settled, once, with the worker's answer.** A
lane drained to completion (no write outstanding) settles every job it was
submitted with `workerStep`'s answer (`applied` or `failed`), and appends one
batch per job. -/
theorem drain_settles_each (ops : List (Lambo.Receipts.Op Job)) (fence : Job → Bool)
    (out : Job → RunResult) (now : Int)
    (hnd : ((Lambo.Receipts.submitted ops).map (·.key)).Nodup)
    (j : Job) (hj : j ∈ Lambo.Receipts.submitted ops) :
    let js := (drain (Lambo.Receipts.run ⟨[], []⟩ ops)).taken
    (lookup (work fence out now (admitStore js) js).1 j.key).map (·.answer) =
        some (workerStep (fence j) j (out j)).1 ∧
      (work fence out now (admitStore js) js).2 =
        js.map (fun j => (workerStep (fence j) j (out j)).2) := by
  intro js
  have hjs : js = Lambo.Receipts.submitted ops := (drain_fifo ops).2
  rw [← hjs] at hnd hj
  refine ⟨work_lookup fence out now js _ j hnd hj ?_, work_log fence out now js _⟩
  obtain ⟨e, he, ha⟩ := lookup_admit hj
  exact ⟨e, he, by rw [ha]; rfl⟩

/-- Settled once means settled for good (`Receipts.settleAll_fixed`): no later
settle attempt changes a drained write's receipt. -/
theorem drained_final (fence : Bool) (j : Job) (r : RunResult) (e : Entry)
    (h : e.answer = (workerStep fence j r).1) (later : List (Answer × Int)) :
    Lambo.Receipts.settleAll e later = e :=
  Lambo.Receipts.settleAll_fixed later e (h ▸ workerStep_settled fence j r)

/-! ## Close: drained, deferred, or answered `close_unflushed` -/

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

theorem settleDeferred_all_settled (flush : CloseFlush) (now : Int) :
    ∀ (ids : List Key) (s : RStore), (∀ p ∈ s, p.1 ∉ ids → p.2.answer.isSettled = true) →
      ∀ p ∈ settleDeferred flush now ids s, p.2.answer.isSettled = true := by
  intro ids
  induction ids with
  | nil =>
    intro s h p hp
    exact h p hp (by simp)
  | cons r ids ih =>
    intro s h
    apply ih
    intro p hp hpi
    obtain ⟨q, hq, rfl⟩ := mem_update hp
    by_cases hqr : q.1 = r
    · rw [ite_eq_left hqr]; exact deferEntry_settled flush now q.2
    · rw [ite_eq_right hqr] at hpi ⊢
      apply h q hq
      intro hm
      cases List.mem_cons.mp hm with
      | inl h' => exact hqr h'
      | inr h' => exact hpi h'

theorem mem_pendingIds {s : RStore} {p : Key × Entry} (hp : p ∈ s)
    (hu : p.2.answer.isSettled = false) : p.1 ∈ pendingIds s := by
  unfold pendingIds
  exact List.mem_map_of_mem (List.mem_filter.mpr ⟨hp, by simp [hu]⟩)

/-- **No receipt is left pending by a close.** Whatever the drain finished
before its budget, every receipt in the store is settled afterwards: the drain
settled some, and `abort_workers` set aside every other one for
`settle_deferred` ("deferred, not lost"). -/
theorem close_settles_all (done : List Job) (fence : Job → Bool) (out : Job → RunResult)
    (now : Int) (flush : CloseFlush) (s : RStore) :
    ∀ p ∈ (close done fence out now flush s).1, p.2.answer.isSettled = true := by
  apply settleDeferred_all_settled
  intro p hp hpi
  cases h : p.2.answer.isSettled with
  | true => rfl
  | false => exact absurd (mem_pendingIds hp h) hpi

/-- And settled for good: no later settle attempt changes any receipt after a
close (`Receipts.settleAll_fixed`). -/
theorem close_final (done : List Job) (fence : Job → Bool) (out : Job → RunResult) (now : Int)
    (flush : CloseFlush) (s : RStore) (p : Key × Entry)
    (hp : p ∈ (close done fence out now flush s).1) (later : List (Answer × Int)) :
    Lambo.Receipts.settleAll p.2 later = p.2 :=
  Lambo.Receipts.settleAll_fixed later p.2 (close_settles_all done fence out now flush s p hp)

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

/-- **What every admitted write is after a close.** For admitted jobs `all`
(distinct receipt ids) of which the drain finished `done`: a finished job
carries the worker's answer (`applied` or `failed`); every other one is
`intent_durable` when the final flush landed and `close_unflushed` when it did
not. No job is in neither state. -/
theorem close_answer (all done : List Job) (fence : Job → Bool) (out : Job → RunResult)
    (now : Int) (flush : CloseFlush) (hnd : (all.map (·.key)).Nodup)
    (hdone : ∀ d ∈ done, d ∈ all) (hdnd : (done.map (·.key)).Nodup) (j : Job) (hj : j ∈ all) :
    (lookup (close done fence out now flush (admitStore all)).1 j.key).map (·.answer) =
      if j ∈ done then some (workerStep (fence j) j (out j)).1
      else some (match flush with
        | .durable => Answer.intentRecorded
        | .incomplete => Answer.closeUnflushed) := by
  obtain ⟨e, he, hpend⟩ := lookup_admit hj
  have hu : e.answer.isSettled = false := by rw [hpend]; rfl
  show (lookup (settleDeferred flush now _ (work fence out now (admitStore all) done).1)
    j.key).map (·.answer) = _
  rw [lookup_settleDeferred]
  by_cases hd : j ∈ done
  · rw [ite_eq_left hd]
    have hw := work_lookup fence out now done (admitStore all) j hdnd hd ⟨e, he, hu⟩
    cases hl : lookup (work fence out now (admitStore all) done).1 j.key with
    | none => rw [hl] at hw; simp at hw
    | some e' =>
      rw [hl] at hw
      have ha : e'.answer = (workerStep (fence j) j (out j)).1 := by simpa using hw
      have hs : e'.answer.isSettled = true := ha ▸ workerStep_settled _ j _
      have hc : e'.answer ≠ .closeUnflushed := by
        rw [ha]
        rcases workerStep_answer (fence j) j (out j) with h | h <;> rw [h] <;> decide
      by_cases hm : j.key ∈ pendingIds (work fence out now (admitStore all) done).1
      · simp [hm, deferEntry_of_settled flush now e' hs hc, ha]
      · simp [hm, ha]
  · rw [ite_eq_right hd]
    have hnot : j.key ∉ done.map (·.key) := by
      intro hm
      obtain ⟨d, hdm, hdr⟩ := List.mem_map.mp hm
      exact hd (eq_of_key hnd (hdone d hdm) hj hdr ▸ hdm)
    have hw := work_lookup_other fence out now done (admitStore all) j.key hnot
    rw [hw, he]
    have hm : j.key ∈ pendingIds (work fence out now (admitStore all) done).1 := by
      exact mem_pendingIds (lookup_mem _ _ _ (hw.trans he)) hu
    have hu' : ¬ e.answer.isSettled = true := by rw [hu]; decide
    cases flush with
    | durable =>
      simp only [hm, ite_true, Option.map_some]
      rw [deferEntry_durable, ite_eq_right (by rw [hpend]; decide),
        Lambo.Receipts.settle_unsettled e _ now hu']
    | incomplete =>
      simp only [hm, ite_true, Option.map_some]
      rw [deferEntry_incomplete, Lambo.Receipts.settle_unsettled e _ now hu']

/-- A retried close upgrades `close_unflushed` to `intent_durable` once its
flush lands: the one settled-to-settled change, made by `settle_deferred`
outside `Entry::settle` (#117). -/
theorem retry_upgrades (now now' : Int) (e : Entry) (h : e.answer.isSettled = false) :
    (deferEntry .durable now' (deferEntry .incomplete now e)).answer = .intentRecorded := by
  have h' : ¬ e.answer.isSettled = true := by rw [h]; decide
  rw [deferEntry_incomplete, Lambo.Receipts.settle_unsettled e _ now h', deferEntry_durable,
    ite_eq_left rfl]

/-- `abort_all_sync` (the `Drop` path) settles nothing: a receipt pending when
a `Memory` is dropped without `close()` stays pending in a process that is
going away. This is the documented exception to "no write is lost silently";
the write survives only if an earlier flush took its intent. -/
theorem drop_settles_nothing (s : RStore) (q : Key) : lookup (abortAllSync s) q = lookup s q := rfl

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

/-! ## Replay -/

theorem loop_nil (rout : Job → RR) (s : Streaks) : loop rout s [] = ROut.empty := rfl

theorem loop_applied {rout : Job → RR} {s s' : Streaks} {i : Intent} {rest : List Intent}
    (h : replayStep i.job.image (rout i.job) s = (.applied, s')) :
    loop rout s (i :: rest) =
      { loop rout s' rest with
        answers := (i.job.key, .appliedAfterRestart) :: (loop rout s' rest).answers
        log := [.write i.job.key, .consume i.job.key .appliedAfterRestart] :: (loop rout s' rest).log
        applied := i.job :: (loop rout s' rest).applied } := by
  simp only [loop, h]

theorem loop_failed {rout : Job → RR} {s s' : Streaks} {i : Intent} {rest : List Intent}
    (h : replayStep i.job.image (rout i.job) s = (.failed, s')) :
    loop rout s (i :: rest) =
      { loop rout s' rest with
        answers := (i.job.key, .failed) :: (loop rout s' rest).answers
        log := [.consume i.job.key .failed] :: (loop rout s' rest).log
        failed := i.job :: (loop rout s' rest).failed } := by
  simp only [loop, h]

theorem loop_skip {rout : Job → RR} {s s' : Streaks} {i : Intent} {rest : List Intent}
    (h : replayStep i.job.image (rout i.job) s = (.skip, s')) :
    loop rout s (i :: rest) = loop rout s' rest := by
  simp only [loop, h]

/-- **A stop ends the replay where it is.** The intent that stopped it and every
later one stay unconsumed (the loop appends nothing) for the next serve. -/
theorem loop_stop {rout : Job → RR} {s s' : Streaks} {i : Intent} {rest : List Intent} {b : Block}
    (h : replayStep i.job.image (rout i.job) s = (.stop b, s')) :
    loop rout s (i :: rest) = { ROut.empty with block := some b } := by
  simp only [loop, h]

/-- The facts about one loop run every theorem below rests on: what it applies
and fails is a sublist of its input, in input order; its log has no puts; and
every job it applied or failed is consumed in its log. -/
theorem loop_facts (rout : Job → RR) : ∀ (l : List Intent) (s : Streaks),
    (loop rout s l).applied.Sublist (l.map (·.job)) ∧
    (loop rout s l).failed.Sublist (l.map (·.job)) ∧
    NoPut (loop rout s l).log.flatten ∧
    (∀ x, x ∈ (loop rout s l).applied ∨ x ∈ (loop rout s l).failed →
      ∃ t, Mut.consume x.key t ∈ (loop rout s l).log.flatten) := by
  intro l
  induction l with
  | nil =>
    intro s
    refine ⟨List.Sublist.slnil, List.Sublist.slnil, ?_, ?_⟩
    · intro m hm; simp [loop_nil, ROut.empty] at hm
    · intro x hx; simp [loop_nil, ROut.empty] at hx
  | cons i rest ih =>
    intro s
    cases h : replayStep i.job.image (rout i.job) s with
    | mk st s' =>
      obtain ⟨h1, h2, h3, h4⟩ := ih s'
      cases st with
      | applied =>
        rw [loop_applied h]
        refine ⟨List.Sublist.cons_cons _ h1, List.Sublist.cons _ h2, ?_, ?_⟩
        · intro m hm j
          simp only [List.flatten_cons, List.mem_append, List.mem_cons] at hm
          rcases hm with ((hm | hm | hm) | hm)
          · rw [hm]; exact Mut.noConfusion
          · rw [hm]; exact Mut.noConfusion
          · simp at hm
          · exact h3 m hm j
        · intro x hx
          simp only [List.mem_cons] at hx
          rcases hx with (hx | hx) | hx
          · exact ⟨.appliedAfterRestart, by rw [hx]; simp⟩
          · obtain ⟨t, ht⟩ := h4 x (Or.inl hx)
            exact ⟨t, by simp only [List.flatten_cons]; exact List.mem_append_right _ ht⟩
          · obtain ⟨t, ht⟩ := h4 x (Or.inr hx)
            exact ⟨t, by simp only [List.flatten_cons]; exact List.mem_append_right _ ht⟩
      | failed =>
        rw [loop_failed h]
        refine ⟨List.Sublist.cons _ h1, List.Sublist.cons_cons _ h2, ?_, ?_⟩
        · intro m hm j
          simp only [List.flatten_cons, List.mem_append, List.mem_cons] at hm
          rcases hm with ((hm | hm) | hm)
          · rw [hm]; exact Mut.noConfusion
          · simp at hm
          · exact h3 m hm j
        · intro x hx
          simp only [List.mem_cons] at hx
          rcases hx with hx | hx | hx
          · obtain ⟨t, ht⟩ := h4 x (Or.inl hx)
            exact ⟨t, by simp only [List.flatten_cons]; exact List.mem_append_right _ ht⟩
          · exact ⟨.failed, by rw [hx]; simp⟩
          · obtain ⟨t, ht⟩ := h4 x (Or.inr hx)
            exact ⟨t, by simp only [List.flatten_cons]; exact List.mem_append_right _ ht⟩
      | skip =>
        rw [loop_skip h]
        exact ⟨List.Sublist.cons _ h1, List.Sublist.cons _ h2, h3, h4⟩
      | stop b =>
        rw [loop_stop h]
        refine ⟨List.nil_sublist _, List.nil_sublist _, ?_, ?_⟩
        · intro m hm; simp [ROut.empty] at hm
        · intro x hx; simp [ROut.empty] at hx

theorem replay_cases (live : Bool) (rout : Job → RR) (intents : List Intent) :
    replay live rout intents = ROut.empty ∨
      replay live rout intents = { ROut.empty with block := some .embedder } ∨
      replay live rout intents = loop rout ⟨0, 0⟩ (pendingOf intents) := by
  unfold replay
  cases hp : pendingOf intents with
  | nil => exact Or.inl rfl
  | cons p ps =>
    cases live with
    | true => exact Or.inr (Or.inr rfl)
    | false => exact Or.inr (Or.inl rfl)

/-- The loop facts lifted to `replay`, over the pending rows. -/
theorem replay_facts (live : Bool) (rout : Job → RR) (intents : List Intent) :
    (replay live rout intents).applied.Sublist ((pendingOf intents).map (·.job)) ∧
    (replay live rout intents).failed.Sublist ((pendingOf intents).map (·.job)) ∧
    NoPut (replay live rout intents).log.flatten ∧
    (∀ x, x ∈ (replay live rout intents).applied ∨ x ∈ (replay live rout intents).failed →
      ∃ t, Mut.consume x.key t ∈ (replay live rout intents).log.flatten) := by
  rcases replay_cases live rout intents with h | h | h <;> rw [h]
  · refine ⟨List.nil_sublist _, List.nil_sublist _, ?_, ?_⟩
    · intro m hm; simp [ROut.empty] at hm
    · intro x hx; simp [ROut.empty] at hx
  · refine ⟨List.nil_sublist _, List.nil_sublist _, ?_, ?_⟩
    · intro m hm; simp [ROut.empty] at hm
    · intro x hx; simp [ROut.empty] at hx
  · exact loop_facts rout _ _

/-- **Replay only ever applies an unconsumed row.** -/
theorem replay_applied_pending {live : Bool} {rout : Job → RR} {intents : List Intent} {x : Job}
    (hx : x ∈ (replay live rout intents).applied) :
    ∃ i ∈ intents, i.job = x ∧ i.outcome = none := by
  have hs := (replay_facts live rout intents).1.subset hx
  obtain ⟨i, hi, rfl⟩ := List.mem_map.mp hs
  obtain ⟨hi1, hi2⟩ := List.mem_filter.mp hi
  exact ⟨i, hi1, rfl, Option.isNone_iff_eq_none.mp hi2⟩

/-- **Replaying a fully drained queue changes nothing.** With every row
consumed (applied or failed, by the drain or an earlier replay), the replay
applies nothing, fails nothing, appends nothing and sets no block reason. -/
theorem replay_drained (live : Bool) (rout : Job → RR) (intents : List Intent)
    (h : ∀ i ∈ intents, i.outcome ≠ none) : replay live rout intents = ROut.empty := by
  have hp : pendingOf intents = [] := by
    unfold pendingOf
    apply List.filter_eq_nil_iff.mpr
    intro i hi hn
    exact h i hi (Option.isNone_iff_eq_none.mp hn)
  unfold replay
  rw [hp]

/-- **A settled intent is never re-applied.** No job a replay applied is
applied again by any later replay over the rows its log left. -/
theorem replay_idempotent (live live' : Bool) (rout rout' : Job → RR) (intents : List Intent)
    (x : Job) (hx : x ∈ (replay live rout intents).applied) (y : Job)
    (hy : y ∈ (replay live' rout' (applyLog intents (replay live rout intents).log)).applied) :
    y.key ≠ x.key := by
  obtain ⟨_, _, hnp, hcons⟩ := replay_facts live rout intents
  obtain ⟨i, hi, hij, hio⟩ := replay_applied_pending hy
  intro hr
  have hc := foldl_consumed _ intents hnp (Or.inl (hcons x (Or.inl hx))) i hi (by rw [hij, hr])
  exact hc hio

/-- A replay that settled every pending row (no skip, no stop) leaves nothing
for the next one: the next replay is a no-op. -/
theorem replay_complete_idem (live live' : Bool) (rout rout' : Job → RR) (intents : List Intent)
    (hall : ∀ i ∈ pendingOf intents,
      i.job ∈ (replay live rout intents).applied ∨ i.job ∈ (replay live rout intents).failed) :
    replay live' rout' (applyLog intents (replay live rout intents).log) = ROut.empty := by
  obtain ⟨_, _, hnp, hcons⟩ := replay_facts live rout intents
  apply replay_drained
  intro i' hi'
  obtain ⟨i, hi, hji⟩ := foldl_rows _ intents hnp hi'
  apply foldl_consumed _ intents hnp _ i' hi' rfl
  by_cases hp : ∃ k ∈ intents, k.job.key = i'.job.key ∧ k.outcome = none
  · obtain ⟨k, hk, hkr, hko⟩ := hp
    have hkp : k ∈ pendingOf intents := List.mem_filter.mpr ⟨hk, by simp [hko]⟩
    obtain ⟨t, ht⟩ := hcons k.job (hall k hkp)
    exact Or.inl ⟨t, hkr ▸ ht⟩
  · refine Or.inr ?_
    intro k hk hkr hko
    exact hp ⟨k, hk, hkr, hko⟩

/-- **Replay applies durable intents in their original order** (one serve):
what it applies is a sublist of the loaded rows, in load order. -/
theorem replay_order (live : Bool) (rout : Job → RR) (intents : List Intent) :
    (replay live rout intents).applied.Sublist (intents.map (·.job)) :=
  (replay_facts live rout intents).1.trans (List.filter_sublist.map _)

/-- With issue times in sequence order (sequential admissions, a clock that
does not step back), one serve's replay applies in sequence-number order,
which is admission order (`load_seq_order`). -/
theorem replay_seq_order (live : Bool) (rout : Job → RR) (l : List Intent)
    (hmono : ∀ a ∈ l, ∀ b ∈ l, a.job.rid < b.job.rid → a.job.ms ≤ b.job.ms) :
    (replay live rout (loadOrder l)).applied.Pairwise (fun a b => a.rid ≤ b.rid) := by
  have hs := replay_order live rout (loadOrder l)
  apply List.Pairwise.sublist hs
  exact List.pairwise_map.mpr (load_seq_order l hmono)

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

/-- One serve's replay over several processes' rows applies each process's
writes in sequence order (`load_epoch_seq_order`). -/
theorem replay_epoch_seq_order (live : Bool) (rout : Job → RR) (l : List Intent)
    (hmono : ∀ a ∈ l, ∀ b ∈ l, a.job.epoch = b.job.epoch → a.job.rid < b.job.rid →
      a.job.ms ≤ b.job.ms) :
    (replay live rout (loadOrder l)).applied.Pairwise
      (fun a b => a.epoch = b.epoch → a.rid ≤ b.rid) := by
  have hs := replay_order live rout (loadOrder l)
  apply List.Pairwise.sublist hs
  exact List.pairwise_map.mpr (load_epoch_seq_order l hmono)

/-! ## How the replay settles a refusal -/

/-- A refusal that is a fact about the intent's own input settles `failed`. -/
theorem replayStep_inputFact (img : Bool) (e : Err) (s : Streaks) (h : e.inputFact = true) :
    (replayStep img (.run (.err e)) s).1 = .failed := by
  cases e <;> simp_all [Err.inputFact, replayStep]

/-- So does a stored payload this build refuses to decode (#129). -/
theorem replayStep_decode (img : Bool) (s : Streaks) :
    replayStep img .decodeRefused s = (.failed, s) := rfl

/-- A cycle refusal settles `failed` and leaves both streaks alone: it is
decided before any embed, so it says nothing about the embedder
(`StoreError::is_cycle_refusal`, #116). -/
theorem replayStep_cycle (img : Bool) (s : Streaks) :
    replayStep img (.run (.err .cycle)) s = (.failed, s) := rfl

/-- Any other error that is neither transient nor a fact about the input
stops the replay: a non-cycle store error (another `Invariant`, a
`NotFound`, a backend error), `Config`, a lost lease (`Conflict`), anything
else. -/
theorem replayStep_sessionWide (img : Bool) (e : Err) (s : Streaks) (h1 : e.inputFact = false)
    (h2 : e.transient = false) : ∃ b, (replayStep img (.run (.err e)) s).1 = .stop b := by
  cases e <;> simp_all [Err.inputFact, Err.transient, replayStep]

/-- **An input refusal settles `failed` and the replay continues.** For a
refusal that is a fact about the intent's input (`Embed`, `ImageIdTaken`,
`Reference`, a cycle refusal) the intent is consumed `failed` and the loop goes
on to the next one (the `StoreError::is_cycle_refusal` doc; test
`a_cycle_refused_at_replay_settles_failed_and_the_replay_goes_on`). -/
theorem inputFact_goes_on (rout : Job → RR) (s : Streaks) (i : Intent) (rest : List Intent)
    (e : Err) (he : rout i.job = .run (.err e)) (hf : e.inputFact = true) :
    ∃ s', loop rout s (i :: rest) =
      { loop rout s' rest with
        answers := (i.job.key, .failed) :: (loop rout s' rest).answers
        log := [.consume i.job.key .failed] :: (loop rout s' rest).log
        failed := i.job :: (loop rout s' rest).failed } := by
  have h := replayStep_inputFact i.job.image e s hf
  refine ⟨(replayStep i.job.image (rout i.job) s).2, loop_failed ?_⟩
  rw [he]
  exact Prod.ext h rfl

/-- **Any other invariant error stops the replay**, the intent and every later
one left unconsumed. -/
theorem sessionWide_stops (rout : Job → RR) (s : Streaks) (i : Intent) (rest : List Intent)
    (e : Err) (he : rout i.job = .run (.err e)) (h1 : e.inputFact = false)
    (h2 : e.transient = false) :
    ∃ b, loop rout s (i :: rest) = { ROut.empty with block := some b } := by
  obtain ⟨b, hb⟩ := replayStep_sessionWide i.job.image e s h1 h2
  refine ⟨b, loop_stop (s' := (replayStep i.job.image (rout i.job) s).2) ?_⟩
  rw [he]
  exact Prod.ext hb rfl

/-- A transient embed answer below the threshold leaves the intent unconsumed
and moves on to the next one. -/
theorem unavailable_skips (img : Bool) (s : Streaks) (h : s.transient + 1 < sickThreshold) :
    replayStep img (.run (.err .unavailable)) s =
      (.skip, { s with transient := s.transient + 1 }) := by
  simp only [replayStep]
  rw [ite_eq_right (by omega)]

/-- The third transient in a row with nothing answered between stops the
replay as `embedder` (`EMBEDDER_SICK_THRESHOLD`). -/
theorem unavailable_stops (img : Bool) (s : Streaks) (h : sickThreshold ≤ s.transient + 1) :
    (replayStep img (.run (.err .unavailable)) s).1 = .stop .embedder := by
  simp only [replayStep]
  rw [ite_eq_left h]

/-- The old replay sends an input refusal to the stop arm (pins the code
before #294; `store_error_blocks_backlog` is the backlog it blocks). -/
theorem replayStep_inputRefusal_stops (img : Bool) (s : Streaks) :
    replayStep img (.run (.err .inputRefusal)) s = (.stop .other, s) := rfl

/-! ## The fix for #294: graph-dependent input refusals settle `failed`

The Rust marks the refusals `Err.inputRefusal` stands for by a message
prefix (`StoreError::input_refusal`, recognised by
`StoreError::is_input_refusal`, which a cycle refusal also satisfies), and
the replay's content arm settles every such refusal `failed`, as the in-session
worker already does. Every other non-transient error still stops the
replay. -/

/-- The fixed classification: the errors the replay settles `failed` and goes
on past. -/
def Err.refusesInput (e : Err) : Bool := e.inputFact || e == .inputRefusal

/-- The replay's match after #294: an input refusal settles `failed` and
leaves both streaks alone (decided before any embed, like a cycle refusal);
every other result is handled as before. -/
def stepInput (image : Bool) (rr : RR) (s : Streaks) : Step × Streaks :=
  match rr with
  | .run (.err .inputRefusal) => (.failed, s)
  | rr => replayStep image rr s

/-- `loop` with its step as a parameter, so a fixed step can be run through
the same loop. -/
def loopWith (step : Bool → RR → Streaks → Step × Streaks) (rout : Job → RR) :
    Streaks → List Intent → ROut
  | _, [] => ROut.empty
  | s, i :: rest =>
    match step i.job.image (rout i.job) s with
    | (.applied, s') =>
      let o := loopWith step rout s' rest
      { o with
        answers := (i.job.key, .appliedAfterRestart) :: o.answers
        log := [.write i.job.key, .consume i.job.key .appliedAfterRestart] :: o.log
        applied := i.job :: o.applied }
    | (.failed, s') =>
      let o := loopWith step rout s' rest
      { o with
        answers := (i.job.key, .failed) :: o.answers
        log := [.consume i.job.key .failed] :: o.log
        failed := i.job :: o.failed }
    | (.skip, s') => loopWith step rout s' rest
    | (.stop b, _) => { ROut.empty with block := some b }

/-- `replay` with its step as a parameter. -/
def replayWith (step : Bool → RR → Streaks → Step × Streaks) (live : Bool) (rout : Job → RR)
    (intents : List Intent) : ROut :=
  match pendingOf intents with
  | [] => ROut.empty
  | p => if live then loopWith step rout ⟨0, 0⟩ p else { ROut.empty with block := some .embedder }

/-- The generalisation agrees with the loop as it was. -/
theorem loopWith_replayStep (rout : Job → RR) : ∀ (l : List Intent) (s : Streaks),
    loopWith replayStep rout s l = loop rout s l := by
  intro l
  induction l with
  | nil => intro s; rfl
  | cons i rest ih =>
    intro s
    simp only [loopWith, loop]
    cases replayStep i.job.image (rout i.job) s with
    | mk st s' => cases st <;> simp [ih]

theorem replayWith_replayStep (live : Bool) (rout : Job → RR) (intents : List Intent) :
    replayWith replayStep live rout intents = replay live rout intents := by
  unfold replayWith replay
  cases pendingOf intents with
  | nil => rfl
  | cons p ps => cases live <;> simp [loopWith_replayStep]

/-- **Nothing else changes.** On every result but an input refusal the fixed
step is the old one. -/
theorem stepInput_agrees (img : Bool) (rr : RR) (s : Streaks)
    (h : rr ≠ .run (.err .inputRefusal)) : stepInput img rr s = replayStep img rr s := by
  unfold stepInput
  split
  · exact absurd rfl h
  · rfl

/-- An input refusal settles `failed` with both streaks unchanged. -/
theorem stepInput_inputRefusal (img : Bool) (s : Streaks) :
    stepInput img (.run (.err .inputRefusal)) s = (.failed, s) := rfl

/-- **The fixed replay settles every input refusal `failed`**: `Embed`,
`ImageIdTaken`, `Reference`, a cycle refusal, and the graph-dependent
refusals #294 names. -/
theorem stepInput_refusesInput (img : Bool) (e : Err) (s : Streaks) (h : e.refusesInput = true) :
    (stepInput img (.run (.err e)) s).1 = .failed := by
  cases e <;> simp_all [Err.refusesInput, Err.inputFact, stepInput, replayStep]

/-- **And still stops on everything else that is not transient**: a store
error that can mean an inconsistent graph or store, `Config`, a lost lease,
anything else. -/
theorem stepInput_sessionWide (img : Bool) (e : Err) (s : Streaks) (h1 : e.refusesInput = false)
    (h2 : e.transient = false) : ∃ b, (stepInput img (.run (.err e)) s).1 = .stop b := by
  cases e <;> simp_all [Err.refusesInput, Err.inputFact, Err.transient, stepInput, replayStep]

/-- **The fixed replay never stops on an input refusal**: the intent is
consumed `failed` and the loop goes on to the next one. -/
theorem fixed_input_goes_on (rout : Job → RR) (s : Streaks) (i : Intent) (rest : List Intent)
    (e : Err) (he : rout i.job = .run (.err e)) (hf : e.refusesInput = true) :
    ∃ s', loopWith stepInput rout s (i :: rest) =
      { loopWith stepInput rout s' rest with
        answers := (i.job.key, .failed) :: (loopWith stepInput rout s' rest).answers
        log := [.consume i.job.key .failed] :: (loopWith stepInput rout s' rest).log
        failed := i.job :: (loopWith stepInput rout s' rest).failed } := by
  have h := stepInput_refusesInput i.job.image e s hf
  refine ⟨(stepInput i.job.image (rout i.job) s).2, ?_⟩
  simp only [loopWith, he]
  cases hs : stepInput i.job.image (.run (.err e)) s with
  | mk st s' =>
    rw [hs] at h
    simp only at h
    subst h
    rfl

/-- **The fixed replay still stops on every other non-transient error**, the
intent and every later one left unconsumed. -/
theorem fixed_sessionWide_stops (rout : Job → RR) (s : Streaks) (i : Intent) (rest : List Intent)
    (e : Err) (he : rout i.job = .run (.err e)) (h1 : e.refusesInput = false)
    (h2 : e.transient = false) :
    ∃ b, loopWith stepInput rout s (i :: rest) = { ROut.empty with block := some b } := by
  obtain ⟨b, hb⟩ := stepInput_sessionWide i.job.image e s h1 h2
  refine ⟨b, ?_⟩
  simp only [loopWith, he]
  cases hs : stepInput i.job.image (.run (.err e)) s with
  | mk st s' =>
    rw [hs] at hb
    simp only at hb
    subst hb
    rfl

/-- The fixed replay and the in-session worker now agree on an input
refusal: both consume the intent `failed`. -/
theorem fixed_agrees_with_worker (img : Bool) (j : Job) (s : Streaks) :
    (stepInput img (.run (.err .inputRefusal)) s).1 = .failed ∧
      workerStep false j (.err .inputRefusal) = (.failed, [.consume j.key .failed]) :=
  ⟨rfl, rfl⟩

/-! ## Crash: a durable write is never replayed -/

theorem mem_admit_flatten {all : List Job} {n : Nat} {m : Mut}
    (hm : m ∈ ((admitLog all).take n).flatten) : ∃ j, m = .put j := by
  obtain ⟨b, hb, hmb⟩ := List.mem_flatten.mp hm
  obtain ⟨j, _, rfl⟩ := List.mem_map.mp ((List.take_sublist n _).subset hb)
  exact ⟨j, List.mem_singleton.mp hmb⟩

theorem mem_work_flatten {fence : Job → Bool} {out : Job → RunResult} {now : Int} {s : RStore}
    {done : List Job} {k : Nat} {m : Mut}
    (hm : m ∈ ((work fence out now s done).2.take k).flatten) :
    ∃ d ∈ done, m ∈ (workerStep (fence d) d (out d)).2 := by
  obtain ⟨b, hb, hmb⟩ := List.mem_flatten.mp hm
  have hb' := (List.take_sublist k _).subset hb
  rw [work_log] at hb'
  obtain ⟨d, hd, rfl⟩ := List.mem_map.mp hb'
  exact ⟨d, hd, hmb⟩

theorem work_noPut (fence : Job → Bool) (out : Job → RunResult) (now : Int) (s : RStore)
    (done : List Job) (k : Nat) : NoPut ((work fence out now s done).2.take k).flatten := by
  intro m hm j he
  obtain ⟨d, _, hmd⟩ := mem_work_flatten hm
  rcases workerStep_keys hmd with h | ⟨t, h⟩ <;> rw [h] at he <;> cases he

/-- **No write is applied twice across a crash.** Admission's puts are
logged first, then the workers' batches; a flush (background or final) makes
some prefix of the log durable, and the process may die after any flush. If a
write is in the durable prefix, its intent is consumed there too (the commit
carries its consume), so the next serve's replay never applies it again
(`Graph::consume_write_intent`'s doc, `WriteCtx::run`'s doc). -/
theorem crash_never_reapplies (all done : List Job) (fence : Job → Bool)
    (out : Job → RunResult) (now : Int) (s : RStore) (n : Nat) (r : Key) (live : Bool)
    (rout : Job → RR)
    (hw : Mut.write r ∈ ((admitLog all ++ (work fence out now s done).2).take n).flatten) :
    ∀ x ∈ (replay live rout (loadOrder (intentsOf
      ((admitLog all ++ (work fence out now s done).2).take n).flatten))).applied, x.key ≠ r := by
  intro x hx hxr
  obtain ⟨i, hi, hix, hio⟩ := replay_applied_pending hx
  rw [mem_loadOrder] at hi
  rw [List.take_append, List.flatten_append] at hw hi
  have hwB : Mut.write r ∈
      ((work fence out now s done).2.take (n - (admitLog all).length)).flatten := by
    cases List.mem_append.mp hw with
    | inl h =>
      obtain ⟨j, hj⟩ := mem_admit_flatten h
      cases hj
    | inr h => exact h
  have hcB : Mut.consume r .applied ∈
      ((work fence out now s done).2.take (n - (admitLog all).length)).flatten := by
    obtain ⟨b, hb, hwb⟩ := List.mem_flatten.mp hwB
    have hb' := (List.take_sublist _ _).subset hb
    rw [work_log] at hb'
    obtain ⟨d, _, rfl⟩ := List.mem_map.mp hb'
    exact List.mem_flatten.mpr ⟨_, hb, workerStep_atomic hwb⟩
  unfold intentsOf at hi
  rw [List.foldl_append] at hi
  exact foldl_consumed _ _ (work_noPut fence out now s done _) (Or.inl ⟨_, hcB⟩) i hi
    (by rw [hix]; exact hxr) hio

/-! ## Shutdown: drained or persisted, never silently lost -/

/-- A job whose own batch consumes nothing, and whose number no other
finished job shares, is still owed in the log: its row is unconsumed. -/
theorem owed_in_log (all done : List Job) (fence : Job → Bool) (out : Job → RunResult)
    (now : Int) (s : RStore) (j : Job) (hj : j ∈ all)
    (hno : ∀ d ∈ done, ∀ t, Mut.consume j.key t ∉ (workerStep (fence d) d (out d)).2) :
    ⟨j, none⟩ ∈ pendingOf (loadOrder (intentsOf
      (admitLog all ++ (work fence out now s done).2).flatten)) := by
  apply List.mem_filter.mpr
  refine ⟨mem_loadOrder.mpr ?_, rfl⟩
  unfold intentsOf
  rw [List.flatten_append, List.foldl_append, ← intentsOf, intentsOf_admit]
  apply foldl_keeps_owed
  · exact List.mem_map.mpr ⟨j, hj, rfl⟩
  · intro t ht
    have ht' : Mut.consume j.key t ∈
        ((work fence out now s done).2.take (work fence out now s done).2.length).flatten := by
      rw [List.take_length]; exact ht
    obtain ⟨d, hd, hmd⟩ := mem_work_flatten ht'
    exact hno d hd t hmd

/-- **Shutdown drains or persists every admitted write.** After `close()`,
each admitted write is exactly one of: finished by the drain, with the
worker's answer; deferred with the flush landed, answered `intent_durable`
and owed by the next serve's replay (an unconsumed row in the flushed log);
or deferred with the flush incomplete, answered `close_unflushed`, which says
the write may not have been saved. None is pending and none is unaccounted
for ("Acked ⇒ (applied ∨ durable intent) at a clean close", `quiesce`). -/
theorem shutdown_no_silent_loss (all done : List Job) (fence : Job → Bool)
    (out : Job → RunResult) (now : Int) (flush : CloseFlush) (hnd : (all.map (·.key)).Nodup)
    (hdone : ∀ d ∈ done, d ∈ all) (hdnd : (done.map (·.key)).Nodup) (j : Job) (hj : j ∈ all) :
    (j ∈ done ∧
      (lookup (close done fence out now flush (admitStore all)).1 j.key).map (·.answer) =
        some (workerStep (fence j) j (out j)).1) ∨
    (j ∉ done ∧ flush = .durable ∧
      (lookup (close done fence out now flush (admitStore all)).1 j.key).map (·.answer) =
        some .intentRecorded ∧
      ⟨j, none⟩ ∈ pendingOf (loadOrder (intentsOf
        (admitLog all ++ (close done fence out now flush (admitStore all)).2).flatten))) ∨
    (j ∉ done ∧ flush = .incomplete ∧
      (lookup (close done fence out now flush (admitStore all)).1 j.key).map (·.answer) =
        some .closeUnflushed) := by
  have ha := close_answer all done fence out now flush hnd hdone hdnd j hj
  by_cases hd : j ∈ done
  · rw [ite_eq_left hd] at ha
    exact Or.inl ⟨hd, ha⟩
  · rw [ite_eq_right hd] at ha
    cases flush with
    | durable =>
      refine Or.inr (Or.inl ⟨hd, rfl, ha, ?_⟩)
      apply owed_in_log all done fence out now _ j hj
      intro d hdm t hm
      rcases workerStep_keys hm with h | ⟨t', h⟩
      · cases h
      · have hr : j.key = d.key := (Mut.consume.inj h).1
        exact hd (eq_of_key hnd (hdone d hdm) hj hr.symm ▸ hdm)
    | incomplete => exact Or.inr (Or.inr ⟨hd, rfl, ha⟩)

/-- **An in-session outage answers `failed` and leaves the write owed.** A
job the worker ran into `EmbedUnavailable` (an outage or a lookup timeout) is
answered `failed`, but its intent is left unconsumed (by design, so that an
outage does not destroy it), so the next serve's replay owes it and, if the embedder answers then, applies it.
See `outage_failed_then_applied` for the receipt this produces. -/
theorem outage_failed_but_owed (all done : List Job) (fence : Job → Bool)
    (out : Job → RunResult) (now : Int) (flush : CloseFlush) (hnd : (all.map (·.key)).Nodup)
    (hdone : ∀ d ∈ done, d ∈ all) (hdnd : (done.map (·.key)).Nodup) (j : Job) (hjd : j ∈ done)
    (hf : fence j = false) (e : Err) (he : out j = .err e) (ht : e.transient = true) :
    (lookup (close done fence out now flush (admitStore all)).1 j.key).map (·.answer) =
        some .failed ∧
      ⟨j, none⟩ ∈ pendingOf (loadOrder (intentsOf
        (admitLog all ++ (close done fence out now flush (admitStore all)).2).flatten)) := by
  have hj := hdone j hjd
  have ha := close_answer all done fence out now flush hnd hdone hdnd j hj
  rw [ite_eq_left hjd] at ha
  have hws : workerStep (fence j) j (out j) = (.failed, []) := by
    rw [hf, he]; simp [workerStep, ht]
  refine ⟨by rw [ha, hws], ?_⟩
  apply owed_in_log all done fence out now _ j hj
  intro d hdm t hm
  rcases workerStep_keys hm with h | ⟨t', h⟩
  · cases h
  · have hr : j.key = d.key := (Mut.consume.inj h).1
    have hdj : d = j := eq_of_key hnd (hdone d hdm) hj hr.symm
    subst hdj
    rw [hws] at hm
    simp at hm

/-! ## What the model shows about the Rust: concrete cases -/

section Examples

private def j1 : Job := ⟨1, 0, 1000, false, 0⟩
private def j2 : Job := ⟨2, 0, 1001, false, 0⟩
private def j3 : Job := ⟨3, 0, 1002, false, 0⟩

private def okAll : Job → RR := fun _ => .run .ok

/-- Drain to completion takes a lane's jobs in submission order, whatever the
interleaving (`drain_fifo`). -/
example : (drain (Lambo.Receipts.run ⟨[], []⟩ [.push j1, .pop, .push j2, .push j3])).taken =
    [j1, j2, j3] := by decide

/-- `drain_settles_each`: one job applied, one refused for its content, one
hit by an outage; all three settled. -/
example :
    let w := work (fun _ => false)
      (fun j => if j.rid = 1 then .ok else if j.rid = 2 then .err .embed else .err .unavailable)
      0 (admitStore [j1, j2, j3]) [j1, j2, j3]
    ([j1.key, j2.key, j3.key].map (fun r => (lookup w.1 r).map (·.answer)) =
      [some .applied, some .failed, some .failed]) ∧
    w.2 = [[.write j1.key, .consume j1.key .applied], [.consume j2.key .failed], []] := by decide

/-- `shutdown_no_silent_loss`: the drain finished `j1`; `j2` and `j3` are
deferred and owed by the next replay. -/
example :
    let c := close [j1] (fun _ => false) (fun _ => .ok) 0 .durable (admitStore [j1, j2, j3])
    [j1.key, j2.key, j3.key].map (fun r => (lookup c.1 r).map (·.answer)) =
        [some .applied, some .intentRecorded, some .intentRecorded] ∧
      (pendingOf (loadOrder (intentsOf (admitLog [j1, j2, j3] ++ c.2).flatten))).map (·.job) =
        [j2, j3] := by decide

/-- `close_answer`, incomplete flush: the deferred write says
`close_unflushed`, and a retried close whose flush lands upgrades it
(`retry_upgrades`). -/
example :
    let c := close [] (fun _ => false) (fun _ => .ok) 0 .incomplete (admitStore [j1])
    (lookup c.1 j1.key).map (·.answer) = some .closeUnflushed ∧
      (lookup (settleDeferred .durable 1 [j1.key] c.1) j1.key).map (·.answer) = some .intentRecorded := by
  decide

/-- `drop_settles_nothing`: the `Drop` path leaves a pending receipt pending. -/
example : (lookup (abortAllSync (admitStore [j1])) j1.key).map (·.answer) = some .pending := by decide

/-- `crash_never_reapplies`: the process dies after flushing `j1`'s commit;
the next replay applies only `j2`. -/
example :
    let log := admitLog [j1, j2] ++
      (work (fun _ => false) (fun _ => .ok) 0 (admitStore [j1, j2]) [j1, j2]).2
    (replay true okAll (loadOrder (intentsOf (log.take 3).flatten))).applied = [j2] := by decide

/-- `replay_drained`, `replay_idempotent`: a second replay over what the
first left does nothing. -/
example :
    let ints : List Intent := [⟨j1, none⟩, ⟨j2, some .applied⟩, ⟨j3, none⟩]
    let r1 := replay true okAll ints
    r1.applied = [j1, j3] ∧ replay true okAll (applyLog ints r1.log) = ROut.empty := by decide

/-- Two crashed processes' backlogs with colliding sequence numbers: the
rows are distinct receipts (different epochs and issue times). `a` applies
while `b` meets a transient embed answer and is skipped; `a`'s consume does
not consume `b`'s row, so the next replay applies `b`. Keyed by sequence
alone, `a`'s consume would mark `b`'s row too and the next replay would apply
nothing: the dropped write this keying rules out. -/
example :
    let a : Job := ⟨1, 0, 1000, false, 7⟩
    let b : Job := ⟨1, 0, 2000, false, 9⟩
    let ints := loadOrder [⟨b, none⟩, ⟨a, none⟩]
    let rout : Job → RR := fun j => if j.epoch = 9 then .run (.err .unavailable) else .run .ok
    let r := replay true rout ints
    r.applied = [a] ∧ (replay true okAll (applyLog ints r.log)).applied = [b] := by decide

/-- Both processes' rows applied in one serve, in issue-time order. -/
example :
    let a : Job := ⟨1, 0, 1000, false, 7⟩
    let b : Job := ⟨1, 0, 2000, false, 9⟩
    let ints := loadOrder [⟨b, none⟩, ⟨a, none⟩]
    let r := replay true okAll ints
    r.applied = [a, b] ∧ replay true okAll (applyLog ints r.log) = ROut.empty := by decide

/-- `inputFact_goes_on`: a cycle refusal settles `failed` and the replay
applies the next intent. -/
example :
    let r := replay true (fun j => if j.rid = 1 then .run (.err .cycle) else .run .ok)
      [⟨j1, none⟩, ⟨j2, none⟩]
    r.failed = [j1] ∧ r.applied = [j2] ∧ r.block = none := by decide

/-- The liveness gate: a dead embedder at attach consumes nothing. -/
example : replay false okAll [⟨j1, none⟩] = { ROut.empty with block := some .embedder } := by
  decide

/-- `REPLAY_LOOKUP_TIMEOUT_LIMIT`: three lookup timeouts in a row settle two
`failed` and stop at the third, which stays owed. -/
example :
    let r := replay true (fun _ => .run (.err .lookupTimeout))
      [⟨j1, none⟩, ⟨j2, none⟩, ⟨j3, none⟩]
    r.failed = [j1, j2] ∧ r.block = some .other := by decide

/-- `EMBEDDER_SICK_THRESHOLD`: three transients in a row stop the replay as
`embedder` and consume nothing. -/
example :
    let r := replay true (fun _ => .run (.err .unavailable))
      [⟨j1, none⟩, ⟨j2, none⟩, ⟨j3, none⟩]
    r.log = [] ∧ r.block = some .embedder := by decide

/-! ### Finding 1: a transient skip breaks the per-agent replay order

On an `EmbedUnavailable` below the threshold the loop `continue`s
(`replay.rs`, the `Err(LamboError::EmbedUnavailable(e))` arm), leaving the
intent unconsumed and applying the next one, which can be the same agent's
later write. The skipped intent is applied by a later serve, after it. The
`WriteIntent::agent` doc says "Replay preserves order **within** an agent's
intents (the lane promise)"; `docs/reference/mcp.mdx` (the `intent_durable`
row) says the next server re-attempts the write "in the order you sent it";
and the image-`Config` arm stops rather than skip because "skipping it would
break the lane order".

The same reordering happens across a restart through the in-session outage
path: `j1` meets `EmbedUnavailable` in session (answered `failed`, intent
left unconsumed), `j2` applies in session, and `j1` applies at the next serve
(`outage_then_replay_reorders`). That contradicts `docs/reference/mcp.mdx`
line 91: "sequential writes from one agent to the same session apply in
submission order". -/

/-- Two writes of agent 0; at the first serve `j1` meets one transient embed
answer and `j2` applies; the next serve applies `j1`. Applied order across
the two serves: `j2`, then `j1`. -/
theorem transient_skip_reorders :
    let ints : List Intent := [⟨j1, none⟩, ⟨j2, none⟩]
    let r1 := replay true (fun j => if j.rid = 1 then .run (.err .unavailable) else .run .ok) ints
    let r2 := replay true okAll (applyLog ints r1.log)
    j1.agent = j2.agent ∧ j1.rid < j2.rid ∧ r1.applied ++ r2.applied = [j2, j1] := by
  decide

/-- Two sequential writes of one agent (`j1` before `j2`). In session the
worker meets an outage at `j1` and applies `j2`; the close lands; the next
serve applies `j1`. Writes in the order they reached the graph: `j2`, then
`j1`. -/
theorem outage_then_replay_reorders :
    let c := close [j1, j2] (fun _ => false)
      (fun j => if j.rid = 1 then .err .unavailable else .ok) 0 .durable (admitStore [j1, j2])
    let rows := loadOrder (intentsOf (admitLog [j1, j2] ++ c.2).flatten)
    j1.agent = j2.agent ∧ j1.rid < j2.rid ∧
    c.2 = [[], [.write j2.key, .consume j2.key .applied]] ∧
      (replay true okAll rows).applied = [j1] := by
  decide

/-! ### Finding 2: the load order trusts the wall clock

Rows load `ORDER BY issued_ms, lane_seq`, and `issued_ms` is the pipeline's
clock (`Utc::now` in production) read in `next_receipt`. The
`WriteIntent::lane_seq` doc says sorting by (`issued_ms`, `lane_seq`)
"replays one process's intents in exact admission order"; that holds only
while the clock does not step back between admissions (`load_seq_order`). -/

/-- One process, one agent: `j1` admitted first (sequence 1) at 1000 ms, then
the clock steps back one millisecond and `j2` (sequence 2) is stamped 999 ms.
The load puts `j2` first, and the replay applies it first. -/
theorem clock_step_reorders :
    let a : Job := ⟨1, 0, 1000, false, 0⟩
    let b : Job := ⟨2, 0, 999, false, 0⟩
    (replay true okAll (loadOrder [⟨a, none⟩, ⟨b, none⟩])).applied = [b, a] := by
  decide

/-! ### Finding 3 (doc fix): a `failed` receipt after an outage is not final

For an in-session `EmbedUnavailable` the worker answers `failed` but leaves
the intent unconsumed (`outage_failed_but_owed`), so the next serve applies
it and the same receipt id then answers `applied_after_restart`. This is the
designed behaviour, and the user docs describe it: `docs/reference/mcp.mdx`
and `docs/reference/config.mdx` tell callers to recall, or read the receipt
after the restart, before deriving the same concept again, "or it may be
written twice", and the `failed` row of mcp.mdx's receipt table says the
intent is kept and replayed. What is stale is the rustdoc on
`ReceiptAnswer::Failed` (`src/writeq/receipts.rs`), "Attempted and rejected.
The write did not happen.", with no qualifier, and the first sentence of
mcp.mdx's `failed` row, "Nothing was written", which needs "by this server"
or "yet". -/

/-- The restart map's answer for a receipt after a replay: seeded from the
rows, then overwritten by the replay's own inserts. -/
def restartAnswer (live : Bool) (rout : Job → RR) (intents : List Intent) (r : Key) :
    Option Answer :=
  lookupLast (seed intents ++ (replay live rout intents).answers) r

theorem outage_failed_then_applied :
    let c := close [j1] (fun _ => false) (fun _ => .err .unavailable) 0 .durable (admitStore [j1])
    let rows := loadOrder (intentsOf (admitLog [j1] ++ c.2).flatten)
    (lookup c.1 j1.key).map (·.answer) = some .failed ∧
      restartAnswer true okAll rows j1.key = some .appliedAfterRestart ∧
      (replay true okAll rows).applied = [j1] := by
  decide

/-! ### Finding 4: a store error other than a cycle blocks the backlog

Only a cycle refusal among `StoreError::Invariant`s settles `failed` at
replay; every other one reaches the stop arm, which leaves the intent durable
and ends the replay, on every serve, so every later intent stays
`pending_replay` (the shape #116 fixed for cycles). Some of those are facts
about the write's input against the graph, raised as plain `Invariant`s:
`reject_repeated_observation` and `reject_second_hierarchical_parent`
(`src/graph/derive.rs`) and the same-canonical-key check in hybrid
`validate_pairs` (`src/graph/hybrid.rs`). Scenario: a `Canonical` session
acks two derives of the same Observation, both puts flush, the process dies;
the next serve applies the first, the second is refused with a non-cycle
`Invariant`, and the replay stops (block reason `Other`) there on every later
serve, leaving the whole backlog behind it `pending_replay`. The in-session
worker, meeting the same error, consumes it `failed`. (`spawn_replay`'s doc
also lists "a vanished interaction" among the failures consumed `failed`; that
is a `Store(NotFound)` and stops the replay too, but it is effectively
unreachable: the interaction is logged before the put and never collected.) -/

theorem store_error_blocks_backlog :
    let ints : List Intent := [⟨j1, none⟩, ⟨j2, none⟩]
    let rout : Job → RR := fun j => if j.rid = 1 then .run (.err .store) else .run .ok
    let r := replay true rout ints
    r.applied = [] ∧ r.block = some .other ∧ applyLog ints r.log = ints ∧
      workerStep false j1 (.err .store) = (.failed, [.consume j1.key .failed]) := by
  decide

/-- #294 fixed, on the issue's scenario: a `Canonical` session's second
derive of one Observation (`j2`) is refused at replay as an input refusal. The
old replay stops there, `j3` unapplied, on every serve; the fixed one settles
`j2` `failed`, applies `j3`, and leaves nothing for the next serve. -/
example :
    let ints : List Intent := [⟨j1, none⟩, ⟨j2, none⟩, ⟨j3, none⟩]
    let rout : Job → RR := fun j => if j.rid = 2 then .run (.err .inputRefusal) else .run .ok
    let old := replay true rout ints
    let r := replayWith stepInput true rout ints
    old.applied = [j1] ∧ old.block = some .other ∧
      replay true rout (applyLog ints old.log) = { ROut.empty with block := some .other } ∧
      r.applied = [j1, j3] ∧ r.failed = [j2] ∧ r.block = none ∧
      replayWith stepInput true rout (applyLog ints r.log) = ROut.empty := by
  decide

/-- The fixed replay still stops on a store error that is not an input
refusal (the scenario of `store_error_blocks_backlog`). -/
example :
    let ints : List Intent := [⟨j1, none⟩, ⟨j2, none⟩]
    let rout : Job → RR := fun j => if j.rid = 1 then .run (.err .store) else .run .ok
    replayWith stepInput true rout ints = replay true rout ints ∧
      (replayWith stepInput true rout ints).block = some .other := by
  decide

/-! ### Finding 5: an intent with an unparseable receipt id is never settled

The loop `continue`s past it (`let Ok(receipt) = ... else { continue; }`),
so it is never applied and never consumed, and every serve skips it again.
Seeding's warning says "it will replay but cannot be looked up", which is
false: it never replays. And the row stays counted in `replay_owed` (set from
the backlog's length, decremented only when a row is applied or failed)
forever. -/

theorem bad_id_never_settled :
    let ints : List Intent := [⟨j1, none⟩, ⟨j2, none⟩]
    let rout : Job → RR := fun j => if j.rid = 1 then .badId else .run .ok
    let r := replay true rout ints
    r.applied = [j2] ∧ ⟨j1, none⟩ ∈ pendingOf (applyLog ints r.log) := by
  decide

end Examples

end Lambo.Drain
